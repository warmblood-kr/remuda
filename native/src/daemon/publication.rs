//! Staged endpoint publication for `serve_inner`: the listener is bound and
//! receipted on a private name, then published by exclusive rename.

use super::*;
use crate::staged_socket::{
    owner::{self, Owner},
    probe::{self, Unsupported},
    startup::{self, Controls, Failure},
};

/// A published listener and the lifecycle owner holding its receipt.
pub(super) struct Imported {
    pub listener: Listener,
    pub owner: Arc<SocketOwnership>,
    pub residue: Vec<String>,
}

/// Primary failure plus every entry left behind.
#[derive(Debug)]
pub(super) struct StagedError {
    pub error: std::io::Error,
    pub residue: Vec<String>,
}

impl From<std::io::Error> for StagedError {
    fn from(error: std::io::Error) -> Self {
        Self {
            error,
            residue: Vec::new(),
        }
    }
}

pub(super) fn bind_staged(path: &Path, controls: &Controls) -> Result<Imported, StagedError> {
    let opened = startup::open(path, controls).map_err(|e| StagedError {
        error: e.error,
        residue: e.residue,
    })?;
    let published = opened.published;
    let mut residue = opened.residue;
    let listener = Listener::UdSocket(opened.listener.into());
    let imported = if controls.fails(Failure::Import) || controls.fails(Failure::ImportCleanup) {
        Err(std::io::Error::other("injected import failure"))
    } else {
        listener.set_nonblocking(ListenerNonblockingMode::Accept)
    };
    if let Err(error) = imported {
        drop(listener);
        if controls.fails(Failure::ImportCleanup) {
            residue.push(format!(
                "published socket {}: injected published unlink failure",
                path.display()
            ));
        } else {
            published.abort(path, &mut residue);
        }
        return Err(StagedError { error, residue });
    }
    // The receipt is the transaction's, never a re-read of the path.
    let owner = Owner::from_published(published).map_err(|error| StagedError {
        error,
        residue: residue.clone(),
    })?;
    Ok(Imported {
        listener,
        owner: Arc::new(SocketOwnership::from_owner(owner)),
        residue,
    })
}

/// Operator kill-switch: `REMUDA_SOCKET_PUBLICATION=legacy` keeps the old bind.
pub(super) const KILL_SWITCH: &str = "REMUDA_SOCKET_PUBLICATION";

/// How this daemon's endpoint was (or will be) published, for status output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Mode {
    Staged,
    Legacy(String),
}

impl Mode {
    pub(super) fn describe(&self) -> String {
        match self {
            Self::Staged => "staged".into(),
            Self::Legacy(reason) => format!("legacy: {reason}"),
        }
    }
}

fn why(refusal: Unsupported) -> &'static str {
    match refusal {
        Unsupported::Platform => "platform has no staged publication",
        Unsupported::Parent => "socket directory cannot be inspected",
        Unsupported::RenameFlags => "exclusive rename unsupported here",
        Unsupported::NoFollowChmod => "no-follow chmod unsupported here",
        Unsupported::SunPathBudget => "socket path too long for a staged name",
        Unsupported::Acl => "socket directory has an ACL",
    }
}

/// Staged unless the operator switched it off or the probe refuses.
pub(super) fn choose(path: &Path, switch: Option<&std::ffi::OsStr>) -> Mode {
    if switch.is_some_and(|value| value == "legacy") {
        return Mode::Legacy(format!("{KILL_SWITCH}=legacy (operator setting)"));
    }
    match probe::qualify(path) {
        Ok(()) => Mode::Staged,
        Err(refusal) => Mode::Legacy(why(refusal).into()),
    }
}

/// Staged publication when chosen, with a legacy fallback that says why.
pub(super) fn bind_endpoint(
    path: &Path,
) -> std::io::Result<(Listener, Arc<SocketOwnership>, Mode)> {
    let mode = choose(path, std::env::var_os(KILL_SWITCH).as_deref());
    if mode == Mode::Staged {
        // A live peer was refused by the caller; what remains is a dead daemon's.
        if let Err(error) = ipc::connect(path) {
            if path.exists() && !ipc::may_start_daemon(path, &error) {
                return Err(error);
            }
        }
        owner::remove_stale_socket(path)?;
        match bind_staged(path, &Controls::none()) {
            Ok(staged) => {
                for entry in &staged.residue {
                    eprintln!("remuda daemon: staged socket residue: {entry}");
                }
                return Ok((staged.listener, staged.owner, Mode::Staged));
            }
            Err(failed) => {
                for entry in &failed.residue {
                    eprintln!("remuda daemon: staged socket residue: {entry}");
                }
                use std::io::ErrorKind::{CrossesDevices, Unsupported};
                if !matches!(failed.error.kind(), Unsupported | CrossesDevices) {
                    return Err(failed.error);
                }
                return legacy(path, Mode::Legacy(failed.error.to_string()));
            }
        }
    }
    legacy(path, mode)
}

fn legacy(path: &Path, mode: Mode) -> std::io::Result<(Listener, Arc<SocketOwnership>, Mode)> {
    if let Mode::Legacy(reason) = &mode {
        eprintln!("remuda daemon: legacy socket bind ({reason})");
    }
    let listener = prepare_unix_listener(ipc::listen(path)?, path)?;
    // Right after the bind: startup below can take seconds, and a path replaced
    // meanwhile is not ours to delete on shutdown (#660).
    Ok((listener, Arc::new(SocketOwnership::capture(path)?), mode))
}
