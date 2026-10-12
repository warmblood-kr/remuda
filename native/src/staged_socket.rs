//! Inactive building blocks for staged Unix socket publication.
#![allow(dead_code)]

use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::sync::Mutex;

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
    #[allow(clippy::unnecessary_cast)] // libc's stat field types vary across Unix targets.
    fn capture(parent: &File, name: &OsStr) -> io::Result<Self> {
        let name = c_name(name)?;
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
        let kind = match stat.st_mode & libc::S_IFMT {
            libc::S_IFSOCK => NodeKind::Socket,
            libc::S_IFDIR => NodeKind::Directory,
            _ => NodeKind::Other,
        };
        Ok(Self {
            kind,
            dev: stat.st_dev as u64,
            ino: stat.st_ino,
        })
    }

    fn capture_socket(parent: &File, name: &OsStr) -> io::Result<Self> {
        let receipt = Self::capture(parent, name)?;
        if receipt.kind != NodeKind::Socket {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "staged socket path is not a socket",
            ));
        }
        Ok(receipt)
    }

    fn matches(self, parent: &File, name: &OsStr) -> bool {
        Self::capture(parent, name).is_ok_and(|current| current == self)
    }
}

fn c_name(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path component contains NUL"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ListenerIdentity {
    dev: u64,
    ino: u64,
}

impl ListenerIdentity {
    #[allow(clippy::unnecessary_cast)] // libc's stat field types vary across Unix targets.
    fn validate(fd: &OwnedFd, require_listening: bool) -> io::Result<Self> {
        let raw = fd.as_raw_fd();
        let descriptor_flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
        if descriptor_flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if descriptor_flags & libc::FD_CLOEXEC == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "listener fd is not close-on-exec",
            ));
        }
        let mut flags = 0;
        let mut opt_len = size_of::<libc::c_int>();
        if unsafe {
            libc::getsockopt(
                raw,
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                (&mut flags as *mut libc::c_int).cast(),
                &mut opt_len,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        if flags != libc::SOCK_STREAM {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "listener is not SOCK_STREAM",
            ));
        }
        let mut address = std::mem::MaybeUninit::<libc::sockaddr_storage>::zeroed();
        let mut address_len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        if unsafe { libc::getsockname(raw, address.as_mut_ptr().cast(), &mut address_len) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { address.assume_init().ss_family as i32 } != libc::AF_UNIX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "listener is not AF_UNIX",
            ));
        }
        if require_listening {
            let mut listening: libc::c_int = 0;
            let mut len = size_of::<libc::c_int>() as libc::socklen_t;
            if unsafe {
                libc::getsockopt(
                    raw,
                    libc::SOL_SOCKET,
                    libc::SO_ACCEPTCONN,
                    (&mut listening as *mut libc::c_int).cast(),
                    &mut len,
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            if listening == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "socket is not listening",
                ));
            }
        }
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(raw, stat.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let stat = unsafe { stat.assume_init() };
        Ok(Self {
            dev: stat.st_dev as u64,
            ino: stat.st_ino,
        })
    }
}

fn size_of<T>() -> libc::socklen_t {
    std::mem::size_of::<T>() as libc::socklen_t
}

fn create_stream_socket() -> io::Result<OwnedFd> {
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(owned)
}

fn chmod_socket_nofollow(
    parent: &File,
    name: &OsStr,
    expected: NodeReceipt,
    mode: u32,
) -> io::Result<()> {
    if expected.kind != NodeKind::Socket || !expected.matches(parent, name) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "socket receipt changed before chmod",
        ));
    }
    let name = c_name(name)?;
    #[cfg(all(
        any(target_os = "linux", target_os = "android"),
        any(
            target_arch = "x86",
            target_arch = "x86_64",
            target_arch = "arm",
            target_arch = "aarch64",
            target_arch = "riscv64"
        )
    ))]
    let result = unsafe {
        libc::syscall(
            452 as libc::c_long,
            parent.as_raw_fd(),
            name.as_ptr(),
            mode as libc::mode_t,
            libc::AT_SYMLINK_NOFOLLOW,
        ) as libc::c_int
    };
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::fchmodat(
            parent.as_raw_fd(),
            name.as_ptr(),
            mode as libc::mode_t,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    #[cfg(all(
        any(target_os = "linux", target_os = "android"),
        not(any(
            target_arch = "x86",
            target_arch = "x86_64",
            target_arch = "arm",
            target_arch = "aarch64",
            target_arch = "riscv64"
        ))
    ))]
    let result = return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "fchmodat2 syscall number is unsupported on this architecture",
    ));
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    let result = return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no-follow socket permissions are unsupported on this platform",
    ));
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    if !expected.matches(parent, OsStr::from_bytes(name.as_bytes())) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "socket receipt changed during chmod",
        ));
    }
    Ok(())
}

fn rename_noreplace(parent: &File, from: &OsStr, to: &OsStr) -> io::Result<()> {
    let from = c_name(from)?;
    let to = c_name(to)?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        ) as libc::c_int
    };
    #[cfg(target_os = "macos")]
    let result = unsafe {
        renameatx_np(
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to.as_ptr(),
            0x0000_0004,
        )
    };
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    let result = return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "exclusive rename is unsupported on this platform",
    ));
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn renameatx_np(
        from_fd: libc::c_int,
        from: *const libc::c_char,
        to_fd: libc::c_int,
        to: *const libc::c_char,
        flags: libc::c_uint,
    ) -> libc::c_int;
}

fn unlink_if_matches(parent: &File, name: &OsStr, receipt: NodeReceipt) -> io::Result<bool> {
    if !receipt.matches(parent, name) {
        return Ok(false);
    }
    let name = c_name(name)?;
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(true)
}

#[derive(Debug)]
struct Generation {
    number: u64,
    listener: OwnedFd,
    node: NodeReceipt,
    name: OsString,
}

#[derive(Debug)]
struct LifecycleState {
    terminal: bool,
    current: Option<Generation>,
}

/// Stable holder for the descriptor and its cleanup receipt.
struct Lifecycle {
    parent: File,
    state: Mutex<LifecycleState>,
}

impl Lifecycle {
    fn new(parent: File) -> Self {
        Self {
            parent,
            state: Mutex::new(LifecycleState {
                terminal: false,
                current: None,
            }),
        }
    }

    fn install(&self, expected: Option<u64>, next: Generation) -> io::Result<Option<Generation>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("lifecycle lock poisoned"))?;
        if state.terminal || state.current.as_ref().map(|generation| generation.number) != expected
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "lifecycle changed before publication",
            ));
        }
        Ok(state.current.replace(next))
    }

    fn begin_shutdown(&self) -> io::Result<Option<(u64, NodeReceipt, OsString)>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("lifecycle lock poisoned"))?;
        state.terminal = true;
        Ok(state
            .current
            .as_ref()
            .map(|generation| (generation.number, generation.node, generation.name.clone())))
    }
}

/// Owns only receipted entries until an exclusive rename transfers authority.
struct StagedTransaction {
    parent: File,
    source: OsString,
    receipt: NodeReceipt,
    committed: bool,
}

impl StagedTransaction {
    fn new(parent: File, source: OsString, receipt: NodeReceipt) -> io::Result<Self> {
        if receipt.kind != NodeKind::Socket || !receipt.matches(&parent, &source) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "staging receipt is not a matching socket",
            ));
        }
        Ok(Self {
            parent,
            source,
            receipt,
            committed: false,
        })
    }

    fn publish(mut self, destination: &OsStr) -> io::Result<NodeReceipt> {
        if self.receipt.kind != NodeKind::Socket
            || !self.receipt.matches(&self.parent, &self.source)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "staging receipt is not a matching socket before rename",
            ));
        }
        rename_noreplace(&self.parent, &self.source, destination)?;
        self.committed = true;
        Ok(self.receipt)
    }
}

impl Drop for StagedTransaction {
    fn drop(&mut self) {
        if !self.committed && self.receipt.kind == NodeKind::Socket {
            let _ = unlink_if_matches(&self.parent, &self.source, self.receipt);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Scratch(std::path::PathBuf);
    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            loop {
                let path = std::env::temp_dir().join(format!(
                    "remuda-staged-{}-{}-{}",
                    std::process::id(),
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_nanos(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match std::fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("create scratch directory: {error}"),
                }
            }
        }
        fn parent(&self) -> File {
            File::open(&self.0).unwrap()
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
        let parent = scratch.parent();
        std::fs::write(scratch.0.join("node"), b"sentinel").unwrap();
        assert!(NodeReceipt::capture_socket(&parent, OsStr::new("node")).is_err());
        let path = scratch.0.join("node");
        std::fs::remove_file(&path).unwrap();
        let listener = UnixListener::bind(&path).unwrap();
        let receipt = NodeReceipt::capture_socket(&parent, OsStr::new("node")).unwrap();
        assert_eq!(receipt.kind, NodeKind::Socket);
        let original_path = scratch.0.join("original");
        std::fs::rename(&path, &original_path).unwrap();
        let replacement_listener = UnixListener::bind(&path).unwrap();
        assert!(!receipt.matches(&parent, OsStr::new("node")));
        drop((listener, replacement_listener));
    }

    #[test]
    fn socket_creation_is_cloexec_and_validates_listener_identity() {
        let fd = create_stream_socket().unwrap();
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        assert!(ListenerIdentity::validate(&fd, false).is_ok());
        assert!(ListenerIdentity::validate(&fd, true).is_err());
    }

    #[test]
    fn nofollow_permission_adapter_preserves_symlink_target() {
        let scratch = Scratch::new();
        let parent = scratch.parent();
        let target = scratch.0.join("target");
        std::fs::write(&target, b"keep").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink("target", scratch.0.join("leaf")).unwrap();
        let receipt = NodeReceipt::capture(&parent, OsStr::new("leaf")).unwrap();
        assert!(chmod_socket_nofollow(&parent, OsStr::new("leaf"), receipt, 0o600).is_err());
        assert_eq!(
            std::fs::metadata(target).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn rename_adapter_never_clobbers_destination() {
        let scratch = Scratch::new();
        let parent = scratch.parent();
        std::fs::write(scratch.0.join("source"), b"source").unwrap();
        std::fs::write(scratch.0.join("destination"), b"sentinel").unwrap();
        assert!(
            rename_noreplace(&parent, OsStr::new("source"), OsStr::new("destination")).is_err()
        );
        assert_eq!(
            std::fs::read(scratch.0.join("destination")).unwrap(),
            b"sentinel"
        );
        assert_eq!(std::fs::read(scratch.0.join("source")).unwrap(), b"source");
    }

    #[test]
    fn transaction_cleanup_requires_unchanged_receipt() {
        let scratch = Scratch::new();
        let parent = scratch.parent();
        let path = scratch.0.join("source");
        let listener = UnixListener::bind(&path).unwrap();
        let receipt = NodeReceipt::capture_socket(&parent, OsStr::new("source")).unwrap();
        let transaction =
            StagedTransaction::new(parent, OsString::from("source"), receipt).unwrap();
        drop(listener);
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        drop(transaction);
        assert_eq!(std::fs::read(path).unwrap(), b"replacement");
    }

    #[test]
    fn transaction_rejects_regular_file_receipt_in_new() {
        let scratch = Scratch::new();
        let parent = scratch.parent();
        let path = scratch.0.join("source");
        std::fs::write(&path, b"regular file").unwrap();
        let receipt = NodeReceipt::capture(&parent, OsStr::new("source")).unwrap();

        assert_eq!(receipt.kind, NodeKind::Other);
        assert!(
            StagedTransaction::new(parent, OsString::from("source"), receipt).is_err(),
            "transaction accepted a regular-file receipt"
        );
        assert!(path.exists(), "refusing the receipt must preserve the file");
    }

    #[test]
    fn transaction_rejects_regular_file_receipt_in_publish() {
        let scratch = Scratch::new();
        let parent = scratch.parent();
        let source_path = scratch.0.join("source");
        let destination_path = scratch.0.join("published");
        std::fs::write(&source_path, b"regular file").unwrap();
        let receipt = NodeReceipt::capture(&parent, OsStr::new("source")).unwrap();
        let transaction = StagedTransaction {
            parent,
            source: OsString::from("source"),
            receipt,
            committed: false,
        };

        assert_eq!(receipt.kind, NodeKind::Other);
        assert!(
            transaction.publish(OsStr::new("published")).is_err(),
            "publish accepted a regular-file receipt"
        );
        assert!(
            source_path.exists(),
            "refusing the receipt must preserve the file"
        );
        assert!(!destination_path.exists());
    }

    #[test]
    fn lifecycle_keeps_receipt_when_terminal_and_rejects_later_publish() {
        let scratch = Scratch::new();
        let parent = scratch.parent();
        let path = scratch.0.join("published");
        let listener = UnixListener::bind(&path).unwrap();
        let receipt = NodeReceipt::capture_socket(&parent, OsStr::new("published")).unwrap();
        let fd = create_stream_socket().unwrap();
        let lifecycle = Lifecycle::new(parent);
        lifecycle
            .install(
                None,
                Generation {
                    number: 1,
                    listener: fd,
                    node: receipt,
                    name: OsString::from("published"),
                },
            )
            .unwrap();
        let terminal = lifecycle.begin_shutdown().unwrap().unwrap();
        assert_eq!(terminal.0, 1);
        assert_eq!(terminal.1, receipt);
        assert!(lifecycle
            .install(
                Some(1),
                Generation {
                    number: 2,
                    listener: create_stream_socket().unwrap(),
                    node: receipt,
                    name: OsString::from("published")
                }
            )
            .is_err());
        drop(listener);
    }

    #[test]
    fn valid_receipt_allows_cleanup_and_absent_name_is_harmless() {
        let scratch = Scratch::new();
        let parent = scratch.parent();
        let path = scratch.0.join("socket");
        let listener = UnixListener::bind(&path).unwrap();
        let receipt = NodeReceipt::capture_socket(&parent, OsStr::new("socket")).unwrap();
        drop(listener);
        assert!(unlink_if_matches(&parent, OsStr::new("socket"), receipt).unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn receipt_carries_filesystem_identity() {
        let scratch = Scratch::new();
        let parent = scratch.parent();
        let path = scratch.0.join("node");
        let listener = UnixListener::bind(&path).unwrap();
        let receipt = NodeReceipt::capture_socket(&parent, OsStr::new("node")).unwrap();
        let metadata = std::fs::symlink_metadata(path).unwrap();
        assert_eq!(receipt.dev, metadata.dev());
        assert_eq!(receipt.ino, metadata.ino());
        drop(listener);
    }
}
