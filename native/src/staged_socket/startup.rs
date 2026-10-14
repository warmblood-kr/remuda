//! Test-only startup transaction and bounded fault controls.

use super::{
    chmod_socket_nofollow, create_stream_socket, listen_and_validate, mkdir_exclusive,
    open_staging_dir, preflight_socket_path, remove_dir_if_matches, set_nonblocking,
    unlink_if_matches, NodeKind, NodeReceipt, Residue, StagedTransaction,
};
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;

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

pub(crate) struct Pause {
    pub boundary: Boundary,
    pub reached: Sender<PathBuf>,
    pub release: Receiver<()>,
    pub deadline: Instant,
}

#[derive(Default)]
pub(crate) struct Controls {
    pub pause: Option<Pause>,
    pub failure: Option<Failure>,
    pub directory_mode: Option<u32>,
}

impl Controls {
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
}

/// A published listener, the receipt authorizing its public cleanup, and any
/// private residue left after commit (the generation stays active).
pub(crate) struct Opened {
    pub listener: OwnedFd,
    pub dev: u64,
    pub ino: u64,
    pub residue: Vec<String>,
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
    let mut stage_receipt = None;
    for _ in 0..2 {
        if let Ok(receipt) = NodeReceipt::capture(&parent, stage) {
            stage_receipt = Some(receipt);
            break;
        }
    }
    let Some(stage_receipt) = stage_receipt.filter(|r| r.kind == NodeKind::Directory) else {
        residue.push(format!("{}: unreceipted staging directory", stage_name));
        return Err(io::Error::other("staging directory receipt unavailable"));
    };
    let drop_dir = |residue: &mut Vec<String>| {
        note(
            residue,
            &format!("staging directory {stage_name}"),
            remove_dir_if_matches(&parent, stage, stage_receipt),
        );
    };
    if let Some(mode) = controls.directory_mode {
        // Test seam: start the directory in an unusable mode to force repair.
        let _ = super::chmod_receipted(&parent, stage, stage_receipt, mode);
    }
    let stage_dir = match if controls.failure == Some(Failure::StageOpen) {
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
    let bound = if controls.failure == Some(Failure::Bind) {
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
        residue.push(format!("staging directory {stage_name}: not empty"));
    };
    if let Err(error) = controls.checkpoint(Boundary::Bound, &staged_path) {
        unreceipted(residue);
        return Err(error);
    }
    let mut attempts = 0;
    let receipt = loop {
        attempts += 1;
        let captured = match controls.failure {
            Some(Failure::CapturePersistent) => Err(injected("capture")),
            Some(Failure::CaptureOnce) if attempts == 1 => Err(injected("capture")),
            _ => NodeReceipt::capture_socket(&stage_dir, leaf_name),
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
    let cloned = if controls.failure == Some(Failure::Clone) {
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
                    "staged socket",
                    unlink_if_matches(&stage_dir, leaf_name, receipt).map(removed),
                );
                drop_dir(residue);
                return Err(error);
            }
        };
    let abort = |transaction: StagedTransaction, residue: &mut Vec<String>| {
        note(residue, "staged socket", transaction.abort());
    };
    let prepared = chmod_socket_nofollow(&stage_dir, leaf_name, receipt, 0o600)
        .and_then(|()| set_nonblocking(&fd))
        .and_then(|()| listen_and_validate(&fd).map(drop))
        .and_then(|()| controls.checkpoint(Boundary::Listening, &staged_path));
    if let Err(error) = prepared {
        abort(transaction, residue);
        drop_dir(residue);
        return Err(error);
    }
    let published = match controls.failure {
        Some(Failure::Rename) => Err(injected("rename")),
        Some(Failure::RenameUnsupported) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "exclusive rename unsupported",
        )),
        Some(Failure::RenameCrossDevice) => Err(io::Error::new(
            io::ErrorKind::CrossesDevices,
            "staging is on another filesystem",
        )),
        _ => transaction.publish_into(&parent, leaf),
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
    if matches!(
        controls.failure,
        Some(Failure::AfterPublish | Failure::AfterPublishUnlink)
    ) {
        let outcome = if controls.failure == Some(Failure::AfterPublishUnlink) {
            Err(injected("published unlink"))
        } else {
            unlink_if_matches(&parent, leaf, receipt).map(removed)
        };
        note(residue, &format!("published {}", public.display()), outcome);
        drop_dir(residue);
        return Err(injected("post-publish startup"));
    }
    // Commit is final: a staging-rmdir failure is residue, not a rollback.
    if controls.failure == Some(Failure::PostCommitRmdir) {
        residue.push(format!(
            "staging directory {stage_name}: injected rmdir failure"
        ));
    } else {
        drop_dir(residue);
    }
    Ok(Opened {
        listener: fd,
        dev: receipt.dev,
        ino: receipt.ino,
        residue: std::mem::take(residue),
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
