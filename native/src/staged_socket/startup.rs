//! Test-only startup transaction and bounded fault controls.

use super::{
    chmod_socket_nofollow, create_staging_dir, create_stream_socket, listen_and_validate,
    preflight_socket_path, set_nonblocking, unlink_if_matches, NodeReceipt, StagedTransaction,
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
    Rename,
    AfterPublish,
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

/// A published listener and the receipt authorizing its public cleanup.
pub(crate) struct Opened {
    pub listener: OwnedFd,
    pub dev: u64,
    pub ino: u64,
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

fn remove_dir_if_matches(parent: &File, name: &OsStr, receipt: NodeReceipt) -> io::Result<()> {
    if !receipt.matches(parent, name) {
        return Ok(());
    }
    let c = super::c_name(name)?;
    if unsafe { libc::unlinkat(parent.as_raw_fd(), c.as_ptr(), libc::AT_REMOVEDIR) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn injected(what: &str) -> io::Error {
    io::Error::other(format!("injected {what} failure"))
}

/// Test-only staged transaction: private staging, receipts, exclusive publish.
/// Stale-socket takeover and lifecycle installation belong to slice 3.
pub(crate) fn open(public: &Path, controls: &Controls) -> io::Result<Opened> {
    let parent_path = public
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket has no parent"))?;
    let leaf = public
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket has no name"))?;
    let stage_name = format!(".r660-{}-{}", std::process::id(), unique());
    let staged_path = parent_path.join(&stage_name).join("s");
    // Everything is measured before any mutation.
    preflight_socket_path(public)?;
    preflight_socket_path(&staged_path)?;
    let parent = open_dir_nofollow(parent_path)?;
    let (stage_dir, stage_receipt) = create_staging_dir(&parent, OsStr::new(&stage_name))?;
    let finish_dir = |dir_ok: bool| {
        if dir_ok {
            let _ = remove_dir_if_matches(&parent, OsStr::new(&stage_name), stage_receipt);
        }
    };
    let fd = match create_stream_socket() {
        Ok(fd) => fd,
        Err(error) => {
            finish_dir(true);
            return Err(error);
        }
    };
    // A bind error never authorizes a blind unlink: the leaf is unreceipted.
    bind_path(&fd, &staged_path)?;
    controls.checkpoint(Boundary::Bound, &staged_path)?;
    let leaf_name = OsStr::new("s");
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
            Err(error) => return Err(error),
        }
    };
    let transaction = StagedTransaction::new(stage_dir.try_clone()?, leaf_name.into(), receipt)?;
    let prepared = chmod_socket_nofollow(&stage_dir, leaf_name, receipt, 0o600)
        .and_then(|()| set_nonblocking(&fd))
        .and_then(|()| listen_and_validate(&fd).map(drop));
    if let Err(error) = prepared {
        drop(transaction);
        finish_dir(true);
        return Err(error);
    }
    if let Err(error) = controls.checkpoint(Boundary::Listening, &staged_path) {
        drop(transaction);
        finish_dir(true);
        return Err(error);
    }
    let published = if controls.failure == Some(Failure::Rename) {
        drop(transaction);
        Err(injected("rename"))
    } else {
        transaction.publish_into(&parent, leaf)
    };
    let receipt = match published {
        Ok(receipt) => receipt,
        Err(error) => {
            // StagedTransaction's Drop removed only a still-matching leaf.
            finish_dir(true);
            return Err(error);
        }
    };
    if controls.failure == Some(Failure::AfterPublish) {
        let _ = unlink_if_matches(&parent, leaf, receipt);
        finish_dir(true);
        return Err(injected("post-publish startup"));
    }
    finish_dir(true);
    Ok(Opened {
        listener: fd,
        dev: receipt.dev,
        ino: receipt.ino,
    })
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    nanos ^ NEXT.fetch_add(1, Ordering::Relaxed)
}
