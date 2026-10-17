//! Staged startup transaction (no production caller yet) and test-only fault controls.

use super::{
    chmod_socket_nofollow, create_stream_socket, listen_and_validate, mkdir_exclusive,
    open_staging_dir, preflight_socket_path, remove_dir_if_matches, set_nonblocking,
    unlink_if_matches, NodeKind, NodeReceipt, Residue, StagedTransaction,
};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
#[cfg(test)]
use std::{
    path::PathBuf,
    sync::mpsc::{Receiver, Sender},
    time::Instant,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Boundary {
    Bound,
    Listening,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Failure {
    CaptureOnce,
    CapturePersistent,
    StageOpen,
    Bind,
    Clone,
    Rename,
    RenameUnsupported,
    RenameCrossDevice,
    AfterPublish,
    AfterPublishUnlink,
    PostCommitRmdir,
    Import,
    ImportCleanup,
}

#[cfg(test)]
pub(crate) struct Pause {
    pub boundary: Boundary,
    pub reached: Sender<PathBuf>,
    pub release: Receiver<()>,
    pub deadline: Instant,
}

/// Fault injection exists only in test builds; release builds get a unit type.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct Controls {
    pub pause: Option<Pause>,
    pub failure: Option<Failure>,
    pub directory_mode: Option<u32>,
}

#[cfg(not(test))]
#[derive(Default)]
pub(crate) struct Controls;

impl Controls {
    fn fails(&self, which: Failure) -> bool {
        #[cfg(test)]
        return self.failure == Some(which);
        #[cfg(not(test))]
        return {
            let _ = which;
            false
        };
    }

    fn fails_any(&self, which: &[Failure]) -> bool {
        which.iter().any(|f| self.fails(*f))
    }

    fn directory_mode(&self) -> Option<u32> {
        #[cfg(test)]
        return self.directory_mode;
        #[cfg(not(test))]
        return None;
    }

    #[cfg(test)]
    pub(crate) fn checkpoint(&self, boundary: Boundary, leaf: &Path) -> io::Result<()> {
        if let Some(pause) = &self.pause {
            if pause.boundary == boundary {
                pause
                    .reached
                    .send(leaf.to_path_buf())
                    .map_err(io::Error::other)?;
                pause
                    .release
                    .recv_timeout(pause.deadline.saturating_duration_since(Instant::now()))
                    .map_err(io::Error::other)?;
            }
        }
        Ok(())
    }

    #[cfg(not(test))]
    pub(crate) fn checkpoint(&self, _boundary: Boundary, _leaf: &Path) -> io::Result<()> {
        Ok(())
    }
}

/// A published listener, the receipt authorizing its public cleanup, and any
/// private residue left after commit (the generation stays active).
pub(crate) struct Opened {
    pub listener: OwnedFd,
    pub dev: u64,
    pub ino: u64,
    pub residue: Vec<String>,
    pub published: Published,
}

/// Authority to remove exactly the node this transaction published: the
/// parent descriptor, leaf name and receipt, never a re-resolved path.
#[derive(Debug)]
pub(crate) struct Published {
    parent: File,
    name: OsString,
    receipt: NodeReceipt,
}

impl Published {
    /// Remove the published socket if it is still the receipted node. A
    /// vanished node is silent; anything left behind is reported with `what`.
    pub(crate) fn abort(&self, what: &Path, residue: &mut Vec<String>) {
        let outcome = match NodeReceipt::capture(&self.parent, &self.name) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Residue::Removed),
            Err(error) => Err(error),
            Ok(_) => unlink_if_matches(&self.parent, &self.name, self.receipt).map(removed),
        };
        note(
            residue,
            &format!("published socket {}", what.display()),
            outcome,
        );
    }
}

/// The primary failure plus every entry left behind and why.
#[derive(Debug)]
pub(crate) struct OpenError {
    pub error: io::Error,
    pub residue: Vec<String>,
}

fn bind_path(fd: &OwnedFd, path: &Path) -> io::Result<()> {
    let bytes = path.as_os_str().as_bytes();
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "path too long"));
    }
    addr.sun_family = libc::AF_UNIX as _;
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = *src as _;
    }
    let rc = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            (&addr as *const libc::sockaddr_un).cast(),
            std::mem::size_of::<libc::sockaddr_un>() as _,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn open_dir_nofollow(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

fn injected(what: &str) -> io::Error {
    io::Error::other(format!("injected {what} failure"))
}

fn note(residue: &mut Vec<String>, what: &str, outcome: io::Result<Residue>) {
    match outcome {
        Ok(Residue::Removed) => {}
        Ok(Residue::Preserved) => residue.push(format!("{what}: changed, preserved")),
        Err(error) => residue.push(format!("{what}: {error}")),
    }
}

/// Test-only staged transaction: private staging, receipts, exclusive publish.
/// Stale-socket takeover and lifecycle installation belong to slice 3.
pub(crate) fn open(public: &Path, controls: &Controls) -> Result<Opened, OpenError> {
    let mut residue = Vec::new();
    run(public, controls, &mut residue).map_err(|error| OpenError { error, residue })
}

// One linear transaction: each boundary's cleanup is read top to bottom.
#[allow(clippy::too_many_lines)]
fn run(public: &Path, controls: &Controls, residue: &mut Vec<String>) -> io::Result<Opened> {
    let parent_path = public
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket has no parent"))?;
    let leaf = public
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket has no name"))?;
    let stage_name = format!(".r660-{:08x}-{:016x}", std::process::id(), unique());
    let staged_path = parent_path.join(&stage_name).join("s");
    // Everything is validated before any mutation, leaf and NUL included.
    super::c_name(leaf)?;
    preflight_socket_path(public)?;
    preflight_socket_path(&staged_path)?;
    let parent = open_dir_nofollow(parent_path)?;
    let stage = OsStr::new(&stage_name);
    mkdir_exclusive(&parent, stage)?;
    // From here the directory exists; every exit reports what remains of it.
    let stage_path = parent_path.join(&stage_name);
    let mut captured = NodeReceipt::capture(&parent, stage);
    if captured.is_err() {
        captured = NodeReceipt::capture(&parent, stage);
    }
    let stage_receipt = match captured {
        Ok(receipt) if receipt.kind == NodeKind::Directory => receipt,
        other => {
            residue.push(format!(
                "{}: unreceipted staging directory",
                stage_path.display()
            ));
            return Err(match other {
                Err(error) => io::Error::new(
                    error.kind(),
                    format!("staging directory receipt unavailable: {error}"),
                ),
                Ok(_) => io::Error::other("staging path is not a directory"),
            });
        }
    };
    let drop_dir = |residue: &mut Vec<String>| {
        note(
            residue,
            &format!("staging directory {}", stage_path.display()),
            remove_dir_if_matches(&parent, stage, stage_receipt),
        );
    };
    if let Some(mode) = controls.directory_mode() {
        // Test seam: start the directory in an unusable mode to force repair.
        if let Err(error) = super::chmod_receipted(&parent, stage, stage_receipt, mode) {
            drop_dir(residue);
            return Err(error);
        }
    }
    let stage_dir = match if controls.fails(Failure::StageOpen) {
        Err(injected("staging open"))
    } else {
        open_staging_dir(&parent, stage, stage_receipt)
    } {
        Ok(dir) => dir,
        Err(error) => {
            drop_dir(residue);
            return Err(error);
        }
    };
    let fd = match create_stream_socket() {
        Ok(fd) => fd,
        Err(error) => {
            drop_dir(residue);
            return Err(error);
        }
    };
    // A failed bind created no leaf; only the empty directory can remain.
    let bound = if controls.fails(Failure::Bind) {
        Err(injected("bind"))
    } else {
        bind_path(&fd, &staged_path)
    };
    if let Err(error) = bound {
        drop_dir(residue);
        return Err(error);
    }
    let leaf_name = OsStr::new("s");
    let unreceipted = |residue: &mut Vec<String>| {
        residue.push(format!("{}: unreceipted socket", staged_path.display()));
        residue.push(format!(
            "staging directory {}: not empty",
            stage_path.display()
        ));
    };
    if let Err(error) = controls.checkpoint(Boundary::Bound, &staged_path) {
        // Bounded receipt recovery: a capturable leaf is ours to remove.
        let recovered =
            (0..2).find_map(|_| NodeReceipt::capture_socket(&stage_dir, leaf_name).ok());
        match recovered {
            Some(receipt) => {
                note(
                    residue,
                    &format!("staged socket {}", staged_path.display()),
                    unlink_if_matches(&stage_dir, leaf_name, receipt).map(removed),
                );
                drop_dir(residue);
            }
            None => unreceipted(residue),
        }
        return Err(error);
    }
    let mut attempts = 0;
    let receipt = loop {
        attempts += 1;
        let captured = if controls.fails(Failure::CapturePersistent)
            || (controls.fails(Failure::CaptureOnce) && attempts == 1)
        {
            Err(injected("capture"))
        } else {
            NodeReceipt::capture_socket(&stage_dir, leaf_name)
        };
        match captured {
            Ok(receipt) => break receipt,
            // One bounded recovery; persistent failure preserves the residue.
            Err(_) if attempts < 2 => continue,
            Err(error) => {
                unreceipted(residue);
                return Err(error);
            }
        }
    };
    let cloned = if controls.fails(Failure::Clone) {
        Err(injected("descriptor clone"))
    } else {
        stage_dir.try_clone()
    };
    let mut transaction =
        match cloned.and_then(|dir| StagedTransaction::new(dir, leaf_name.into(), receipt)) {
            Ok(transaction) => transaction,
            Err(error) => {
                note(
                    residue,
                    &format!("staged socket {}", staged_path.display()),
                    unlink_if_matches(&stage_dir, leaf_name, receipt).map(removed),
                );
                drop_dir(residue);
                return Err(error);
            }
        };
    let abort = |transaction: StagedTransaction, residue: &mut Vec<String>| {
        note(
            residue,
            &format!("staged socket {}", staged_path.display()),
            transaction.abort(),
        );
    };
    // The abort handle's descriptor is taken before publishing, so a failure
    // to obtain it is still a pre-commit failure.
    let prepared = parent.try_clone().and_then(|kept| {
        chmod_socket_nofollow(&stage_dir, leaf_name, receipt, 0o600)
            .and_then(|()| set_nonblocking(&fd))
            .and_then(|()| listen_and_validate(&fd).map(drop))
            .and_then(|()| controls.checkpoint(Boundary::Listening, &staged_path))
            .map(|()| kept)
    });
    let kept_parent = match prepared {
        Ok(kept) => kept,
        Err(error) => {
            abort(transaction, residue);
            drop_dir(residue);
            return Err(error);
        }
    };
    let published = if controls.fails(Failure::Rename) {
        Err(injected("rename"))
    } else if controls.fails(Failure::RenameUnsupported) {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "exclusive rename unsupported",
        ))
    } else if controls.fails(Failure::RenameCrossDevice) {
        Err(io::Error::new(
            io::ErrorKind::CrossesDevices,
            "staging is on another filesystem",
        ))
    } else {
        transaction.publish_into(&parent, leaf)
    };
    let receipt = match published {
        Ok(receipt) => receipt,
        Err(error) => {
            // Only the still-matching staged leaf goes; the destination stays.
            abort(transaction, residue);
            drop_dir(residue);
            return Err(error);
        }
    };
    if controls.fails_any(&[Failure::AfterPublish, Failure::AfterPublishUnlink]) {
        let outcome = if controls.fails(Failure::AfterPublishUnlink) {
            Err(injected("published unlink"))
        } else {
            unlink_if_matches(&parent, leaf, receipt).map(removed)
        };
        note(residue, &format!("published {}", public.display()), outcome);
        drop_dir(residue);
        return Err(injected("post-publish startup"));
    }
    // Commit is final: a staging-rmdir failure is residue, not a rollback.
    if controls.fails(Failure::PostCommitRmdir) {
        residue.push(format!(
            "staging directory {}: injected rmdir failure",
            stage_path.display()
        ));
    } else {
        drop_dir(residue);
    }
    Ok(Opened {
        listener: fd,
        dev: receipt.dev,
        ino: receipt.ino,
        residue: std::mem::take(residue),
        published: Published {
            parent: kept_parent,
            name: leaf.to_os_string(),
            receipt,
        },
    })
}

fn removed(done: bool) -> Residue {
    if done {
        Residue::Removed
    } else {
        Residue::Preserved
    }
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    nanos ^ NEXT.fetch_add(1, Ordering::Relaxed)
}
