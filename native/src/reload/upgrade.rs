//! What `remuda upgrade` does with the running daemon once the new build is
//! installed (#713): reload it into that build, so its host-backed sessions
//! keep running. A daemon that cannot reload keeps serving, unchanged.
use remuda_core::protocol::{Request, Response};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, PartialEq, Eq)]
pub enum AfterUpgrade {
    /// Nothing listens; the next command starts the installed build.
    NoDaemon,
    /// The daemon already runs the installed build.
    Current(String),
    /// The daemon now runs the installed build: the reload's report.
    Reloaded(String),
    /// The daemon kept the old build, and its sessions keep running: why.
    Kept(String),
    /// The reload failed and was rolled back, or the daemon did not answer.
    /// Every other refusal is `Kept`: it changed nothing.
    Failed(String),
}

/// Reload the daemon at `socket` into `installed`, unless it already runs it.
pub fn after_upgrade(socket: &Path, installed: &Path) -> AfterUpgrade {
    if crate::ipc::connect(socket).is_err() {
        return AfterUpgrade::NoDaemon;
    }
    let running = match crate::client::request(socket, &Request::Version) {
        Ok(Response::Value(build)) => Some(build),
        _ => None,
    };
    if let (Some(running), Some(build)) = (running, build_of(installed)) {
        if running == build {
            return AfterUpgrade::Current(running);
        }
    }
    let request = Request::Reload {
        binary: installed.to_string_lossy().into_owned(),
        check: false,
    };
    match crate::client::request(socket, &request) {
        Ok(Response::Value(report)) if report.starts_with("reloaded:") => {
            match crate::client::request(socket, &Request::Version) {
                Ok(Response::Value(build)) => {
                    AfterUpgrade::Reloaded(format!("{report}\ndaemon now {build}"))
                }
                other => {
                    AfterUpgrade::Failed(format!("after the reload the daemon answered {other:?}"))
                }
            }
        }
        // A check found blockers; nothing changed.
        Ok(Response::Value(report)) => AfterUpgrade::Kept(report),
        Ok(Response::Error(reason)) if reason.contains("unknown variant `Reload`") => {
            AfterUpgrade::Kept(
                "the running daemon predates `remuda reload`, so it cannot hand its \
                 sessions over"
                    .into(),
            )
        }
        // A build older than reload does not know the candidate verb.
        Ok(Response::Error(reason)) if reason.contains("candidate exited before its hello") => {
            AfterUpgrade::Kept(format!(
                "the installed build did not start as a reload candidate, as a build \
                 older than `remuda reload` cannot: {reason}"
            ))
        }
        // Only a rollback got as far as changing anything; every other
        // refusal (an older daemon, a handoff mismatch) left it as it was.
        Ok(Response::Error(reason)) if reason.contains("rolled back") => {
            AfterUpgrade::Failed(reason)
        }
        Ok(Response::Error(reason)) => AfterUpgrade::Kept(reason),
        Ok(other) => AfterUpgrade::Failed(format!("unexpected reply {other:?}")),
        Err(error) => AfterUpgrade::Failed(error.to_string()),
    }
}

/// The binary the installer just wrote: this one's path. On Linux a file
/// replaced by rename reads as `PATH (deleted)`, and PATH is the new file.
pub fn installed_binary() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let text = exe.to_string_lossy();
    let path = match text.strip_suffix(" (deleted)") {
        Some(path) => PathBuf::from(path),
        None => exe,
    };
    path.canonicalize()
}

/// The build `binary --version` names, or `None` if it does not say.
fn build_of(binary: &Path) -> Option<String> {
    let output = Command::new(binary)
        .arg("--version")
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    let build = text.lines().next()?.strip_prefix("remuda ")?;
    Some(build.trim().to_string())
}
