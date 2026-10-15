//! Private Unix endpoint preparation; never reuse/remove a live or stale path
//! automatically. Same-UID code is explicitly outside this isolation boundary.
use super::channel::{Channel, Key, LocalIo};
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(crate) struct Endpoint {
    listener: UnixListener,
    key: Key,
    _directory: File,
    path: PathBuf,
}
fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "host endpoint owner/permission/peer mismatch",
    )
}
fn uid() -> u32 {
    unsafe { libc::geteuid() }
}
fn directory(path: &Path) -> io::Result<File> {
    if !path.is_absolute() {
        return Err(denied());
    }
    trusted_ancestors(path)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    let info = file.metadata()?;
    if info.uid() != uid() || info.mode() & 0o7777 != 0o700 {
        return Err(denied());
    }
    Ok(file)
}
// Require trusted ancestor ownership and no group/other rename permission.
// Root-owned sticky temporary directories are allowed; same-UID renames are
// explicitly outside the threat boundary. Final leaf itself remains no-follow.
fn trusted_ancestors(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(denied)?;
    for ancestor in parent.ancestors() {
        let metadata = std::fs::symlink_metadata(ancestor)?;
        if metadata.uid() != 0 && metadata.uid() != uid() {
            return Err(denied());
        }
    }
    for ancestor in parent.canonicalize()?.ancestors() {
        let metadata = std::fs::metadata(ancestor)?;
        if metadata.uid() != 0 && metadata.uid() != uid() {
            return Err(denied());
        }
        if metadata.mode() & 0o022 != 0 && !(metadata.uid() == 0 && metadata.mode() & 0o1000 != 0) {
            return Err(denied());
        }
    }
    Ok(())
}
impl Endpoint {
    pub fn bind(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "absolute private host directory required",
            ));
        }
        DirBuilder::new().mode(0o700).create(path)?; // refuses all existing paths
        let directory = directory(path)?;
        let mut key = Key([0; 32]);
        getrandom::fill(&mut key.0)
            .map_err(|error| io::Error::other(format!("host entropy unavailable: {error}")))?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path.join("capability"))?;
        file.write_all(&key.0)?;
        file.sync_all()?;
        let socket = path.join("socket");
        let listener = UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        directory.sync_all()?;
        Ok(Self {
            listener,
            key,
            _directory: directory,
            path: path.to_owned(),
        })
    }
    /// Runtime limits concurrent accepted sockets BEFORE spawning workers.
    pub fn accept(&self) -> io::Result<Channel<UnixStream>> {
        let (stream, _) = self.listener.accept()?;
        verify_peer(&stream, uid())?;
        deadlines(&stream)?;
        Channel::authenticate(stream, &self.key, false)
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}
pub(crate) fn connect(path: &Path) -> io::Result<Channel<UnixStream>> {
    let until = Instant::now() + Duration::from_secs(2);
    let _directory = directory(path)?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path.join("capability"))?;
    let info = file.metadata()?;
    if !info.is_file()
        || info.uid() != uid()
        || info.mode() & 0o7777 != 0o600
        || info.len() != 32
        || info.nlink() != 1
    {
        return Err(denied());
    }
    let mut key = Key([0; 32]);
    file.read_exact(&mut key.0)?;
    let socket = path.join("socket");
    let info = std::fs::symlink_metadata(&socket)?;
    if !info.file_type().is_socket() || info.uid() != uid() || info.mode() & 0o7777 != 0o600 {
        return Err(denied());
    }
    let stream = connect_until(&socket, until)?;
    verify_peer(&stream, uid())?;
    deadlines(&stream)?;
    Channel::authenticate_until(stream, &key, true, until)
}
fn connect_until(path: &Path, until: Instant) -> io::Result<UnixStream> {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "host Unix endpoint path limit",
        ));
    }
    address.sun_family = libc::AF_UNIX as _;
    for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
        *target = *byte as _;
    }
    let len = std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    #[cfg(target_os = "macos")]
    {
        address.sun_len = len as u8;
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // Own immediately, so every later failure closes this descriptor.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = UnixStream::from(owned);
    stream.prepare()?;
    loop {
        let remaining = until
            .checked_duration_since(Instant::now())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "host connect deadline"))?;
        let result = unsafe {
            libc::connect(
                fd,
                (&address as *const libc::sockaddr_un).cast(),
                len as libc::socklen_t,
            )
        };
        if result == 0 {
            return Ok(stream);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EISCONN) => return Ok(stream),
            Some(libc::EINTR) => continue,
            Some(libc::EAGAIN) => {
                std::thread::sleep(remaining.min(Duration::from_millis(5)));
            }
            Some(libc::EINPROGRESS) | Some(libc::EALREADY) => {
                stream.wait(true, remaining)?;
                let mut status = 0_i32;
                let mut size = std::mem::size_of::<i32>() as libc::socklen_t;
                if unsafe {
                    libc::getsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_ERROR,
                        (&mut status as *mut i32).cast(),
                        &mut size,
                    )
                } != 0
                {
                    return Err(io::Error::last_os_error());
                }
                if size as usize != std::mem::size_of::<i32>() {
                    return Err(io::Error::other("host connect status size"));
                }
                if status != 0 {
                    return Err(io::Error::from_raw_os_error(status));
                }
                return Ok(stream);
            }
            _ => return Err(error),
        }
    }
}
fn deadlines(stream: &UnixStream) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))
}
fn verify_peer(stream: &UnixStream, expected: u32) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let (mut actual, mut group) = (0, 0);
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut actual, &mut group) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if actual != expected {
            return Err(denied());
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                credentials.as_mut_ptr().cast(),
                &mut len,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if len as usize != std::mem::size_of::<libc::ucred>()
            || unsafe { credentials.assume_init() }.uid != expected
        {
            return Err(denied());
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (stream, expected);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "host peer credentials unsupported on this Unix target",
        ))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn private_endpoint_mutually_authenticates_and_never_rebinds_existing_path() {
        let path = std::env::temp_dir().join(format!(
            "rm-host-auth-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path.clone());
        let endpoint = Endpoint::bind(&path).unwrap();
        assert_eq!(endpoint.path(), path);
        assert!(Endpoint::bind(&path).is_err());
        let task = std::thread::spawn(move || {
            let mut channel = endpoint.accept().unwrap();
            assert_eq!(channel.receive().unwrap(), b"inspect");
            channel.send(b"identity").unwrap();
        });
        let mut client = connect(&path).unwrap();
        client.send(b"inspect").unwrap();
        assert_eq!(client.receive().unwrap(), b"identity");
        task.join().unwrap();
        drop(client);
        drop(cleanup);
    }
    #[test]
    fn unsafe_ancestors_symlinks_hardlinks_and_special_capabilities_refuse() {
        let parent = std::env::temp_dir().join(format!(
            "rm-host-negative-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&parent).unwrap();
        let cleanup = Cleanup(parent.clone());
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(
            matches!(Endpoint::bind(&parent.join("unsafe")), Err(error) if error.kind() == io::ErrorKind::PermissionDenied)
        );
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = parent.join("valid");
        let endpoint = Endpoint::bind(&root).unwrap();
        for fd in [
            endpoint.listener.as_raw_fd(),
            endpoint._directory.as_raw_fd(),
        ] {
            assert_ne!(
                unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
        }
        std::os::unix::fs::symlink(&root, parent.join("alias")).unwrap();
        assert!(connect(&parent.join("alias")).is_err());
        let capability = root.join("capability");
        std::fs::hard_link(&capability, root.join("second-link")).unwrap();
        assert!(connect(&root).is_err());
        std::fs::remove_file(root.join("second-link")).unwrap();
        std::fs::rename(&capability, root.join("saved")).unwrap();
        std::os::unix::fs::symlink(root.join("saved"), &capability).unwrap();
        assert!(connect(&root).is_err());
        std::fs::remove_file(&capability).unwrap();
        std::fs::create_dir(&capability).unwrap();
        assert!(connect(&root).is_err());
        drop(endpoint);
        drop(cleanup);
    }
    #[test]
    fn peer_mismatch_and_nonprivate_capability_fail_closed() {
        let (first, _) = UnixStream::pair().unwrap();
        assert_eq!(
            verify_peer(&first, uid().wrapping_add(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        let path = std::env::temp_dir().join(format!(
            "rm-host-private-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path.clone());
        let _endpoint = Endpoint::bind(&path).unwrap();
        std::fs::set_permissions(
            path.join("capability"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(
            matches!(connect(&path), Err(error) if error.kind() == io::ErrorKind::PermissionDenied)
        );
        drop(cleanup);
    }
}
