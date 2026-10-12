//! Inactive building blocks for staged Unix socket publication.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NodeKind {
    Socket,
    Directory,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NodeReceipt {
    kind: NodeKind,
    dev: u64,
    ino: u64,
}

impl NodeReceipt {
    fn capture(parent: &File, name: &std::ffi::OsStr) -> io::Result<Self> {
        let name = std::ffi::CString::new(name.as_bytes())?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let stat = unsafe { stat.assume_init() };
        let kind = if (stat.st_mode & libc::S_IFMT) == libc::S_IFSOCK {
            NodeKind::Socket
        } else if (stat.st_mode & libc::S_IFMT) == libc::S_IFDIR {
            NodeKind::Directory
        } else {
            NodeKind::Other
        };
        Ok(Self {
            kind,
            dev: stat.st_dev as u64,
            ino: stat.st_ino as u64,
        })
    }
}

fn create_stream_socket() -> io::Result<OwnedFd> {
    // RED stub: the implementation must request CLOEXEC atomically.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn chmod_nofollow(parent: &File, name: &std::ffi::OsStr, mode: u32) -> io::Result<()> {
    let name = std::ffi::CString::new(name.as_bytes())?;
    // RED stub: follows a symlink; production must use the no-follow adapter.
    let result =
        unsafe { libc::fchmodat(parent.as_raw_fd(), name.as_ptr(), mode as libc::mode_t, 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn rename_noreplace(parent: &File, from: &std::ffi::OsStr, to: &std::ffi::OsStr) -> io::Result<()> {
    let from = std::ffi::CString::new(from.as_bytes())?;
    let to = std::ffi::CString::new(to.as_bytes())?;
    // RED stub: ordinary rename replaces an occupied destination.
    let result = unsafe {
        libc::renameat(
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to.as_ptr(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn cleanup_if_matches(
    parent: &File,
    name: &std::ffi::OsStr,
    _receipt: NodeReceipt,
) -> io::Result<()> {
    let name = std::ffi::CString::new(name.as_bytes())?;
    // RED stub: cleanup ignores its receipt.
    let result = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Scratch(std::path::PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "remuda-staged-red-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn receipts_reject_non_socket_and_detect_replacement() {
        let scratch = Scratch::new();
        let parent = File::open(&scratch.0).unwrap();
        std::fs::write(scratch.0.join("node"), b"sentinel").unwrap();
        let receipt = NodeReceipt::capture(&parent, std::ffi::OsStr::new("node")).unwrap();
        assert_eq!(
            receipt.kind,
            NodeKind::Socket,
            "receipt accepted a regular file"
        );
        let path = scratch.0.join("node");
        std::fs::remove_file(&path).unwrap();
        UnixListener::bind(&path).unwrap();
        let current = NodeReceipt::capture(&parent, std::ffi::OsStr::new("node")).unwrap();
        assert_ne!((receipt.dev, receipt.ino), (current.dev, current.ino));
    }

    #[test]
    fn socket_creation_is_cloexec() {
        let fd = create_stream_socket().unwrap();
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(
            flags & libc::FD_CLOEXEC,
            0,
            "listener fd leaked across exec"
        );
    }

    #[test]
    fn nofollow_permission_adapter_preserves_symlink_target() {
        let scratch = Scratch::new();
        let parent = File::open(&scratch.0).unwrap();
        let target = scratch.0.join("target");
        std::fs::write(&target, b"keep").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink("target", scratch.0.join("leaf")).unwrap();
        let result = chmod_nofollow(&parent, std::ffi::OsStr::new("leaf"), 0o600);
        assert!(result.is_err(), "permission adapter followed a symlink");
        assert_eq!(
            std::fs::metadata(target).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn rename_adapter_never_clobbers_destination() {
        let scratch = Scratch::new();
        let parent = File::open(&scratch.0).unwrap();
        std::fs::write(scratch.0.join("source"), b"source").unwrap();
        std::fs::write(scratch.0.join("destination"), b"sentinel").unwrap();
        let result = rename_noreplace(&parent, "source".as_ref(), "destination".as_ref());
        assert!(result.is_err(), "rename adapter overwrote an occupied name");
        assert_eq!(
            std::fs::read(scratch.0.join("destination")).unwrap(),
            b"sentinel"
        );
        assert_eq!(std::fs::read(scratch.0.join("source")).unwrap(), b"source");
    }

    #[test]
    fn failed_receipt_never_authorizes_cleanup() {
        let scratch = Scratch::new();
        let parent = File::open(&scratch.0).unwrap();
        std::fs::write(scratch.0.join("unreceipted"), b"preserve").unwrap();
        let stale = NodeReceipt {
            kind: NodeKind::Socket,
            dev: 0,
            ino: 0,
        };
        let path = scratch.0.join("unreceipted");
        let _ = cleanup_if_matches(&parent, std::ffi::OsStr::new("unreceipted"), stale);
        assert!(
            path.exists(),
            "unknown entry was removed without a matching receipt"
        );
    }
}
