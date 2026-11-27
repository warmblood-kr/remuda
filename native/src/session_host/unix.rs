//! Private Unix endpoint preparation; never reuse/remove a live or stale path
//! automatically. Same-UID code is explicitly outside this isolation boundary.
use super::channel::{Channel, Key, LocalIo};
use std::ffi::{CString, OsStr};
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(crate) struct Endpoint {
    listener: UnixListener,
    key: Key,
    directory: File,
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
pub(super) fn directory(path: &Path) -> io::Result<File> {
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
        Self::bind_with_socket(path, None)
    }

    pub fn bind_with_socket(path: &Path, socket: Option<&Path>) -> io::Result<Self> {
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
        let socket = match socket {
            Some(socket) => {
                let parent = socket.parent().ok_or_else(denied)?;
                if socket.file_name() != Some(std::ffi::OsStr::new("socket")) || parent == path {
                    return Err(denied());
                }
                socket_path_bytes(socket)?;
                trusted_ancestors(parent)?;
                DirBuilder::new().mode(0o700).create(parent)?;
                self::directory(parent)?;
                crate::fs_atomic::write_atomic_lua_private(
                    &path.join("socket-path"),
                    socket.as_os_str().as_bytes(),
                )?;
                socket.to_path_buf()
            }
            None => path.join("socket"),
        };
        let listener = UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        directory.sync_all()?;
        Ok(Self {
            listener,
            key,
            directory,
            path: path.to_owned(),
        })
    }
    /// Runtime limits concurrent accepted sockets BEFORE spawning workers.
    pub fn accept(&self) -> io::Result<Channel<UnixStream>> {
        self.authenticate(self.accept_stream()?)
    }
    pub fn nonblocking(&self) -> io::Result<()> {
        self.listener.set_nonblocking(true)
    }
    pub fn capability_missing(&self) -> io::Result<bool> {
        let fd = self.directory.as_raw_fd();
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: the descriptor is held by the endpoint and stat is writable.
        if unsafe {
            libc::fstatat(
                fd,
                c"capability".as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
        {
            return Ok(false);
        }
        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::NotFound => Ok(true),
            _ => Err(error),
        }
    }
    pub fn restore_missing_capability(&self) -> io::Result<()> {
        if !self.capability_missing()? {
            return Ok(());
        }
        let original = self.directory.metadata()?;
        let current = directory(&self.path)?.metadata()?;
        if (original.dev(), original.ino()) != (current.dev(), current.ino()) {
            return Err(denied());
        }
        publish_capability(&self.directory, &self.key)
    }
    pub fn wait_connection(&self, timeout: Duration) {
        let mut descriptor = libc::pollfd {
            fd: self.listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // Poll is only an availability wait; accept performs the actual check.
        let _ = unsafe {
            libc::poll(
                &mut descriptor,
                1,
                timeout.as_millis().min(i32::MAX as u128) as i32,
            )
        };
    }
    pub fn accept_stream(&self) -> io::Result<UnixStream> {
        let (stream, _) = self.listener.accept()?;
        verify_peer(&stream, uid())?;
        Ok(stream)
    }
    pub fn authenticate(&self, stream: UnixStream) -> io::Result<Channel<UnixStream>> {
        Channel::authenticate(stream, &self.key, false)
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn publish_capability(directory: &File, key: &Key) -> io::Result<()> {
    let mut random = [0; 8];
    getrandom::fill(&mut random).map_err(|_| io::Error::other("host entropy unavailable"))?;
    let name = format!(".capability-{:016x}.tmp", u64::from_ne_bytes(random));
    let temp = CString::new(name.as_bytes()).map_err(io::Error::other)?;
    let fd = directory.as_raw_fd();
    // SAFETY: the exclusive create is relative to the held directory.
    let key_fd = unsafe {
        libc::openat(
            fd,
            temp.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if key_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new descriptor owned by this call.
    let mut file = unsafe { File::from_raw_fd(key_fd) };
    let result = (|| {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(&key.0)?;
        file.sync_all()?;
        crate::staged_socket::rename_noreplace(
            directory,
            OsStr::new(&name),
            OsStr::new("capability"),
        )
    })();
    match result {
        Ok(()) => directory.sync_all(),
        Err(error) => {
            // SAFETY: remove only the unpublished private name we created.
            unsafe { libc::unlinkat(fd, temp.as_ptr(), 0) };
            if error.kind() == io::ErrorKind::AlreadyExists {
                Ok(())
            } else {
                Err(error)
            }
        }
    }
}

fn socket_path_bytes(socket: &Path) -> io::Result<()> {
    let bytes = socket.as_os_str().as_bytes();
    if !socket.is_absolute()
        || bytes.len() > crate::staged_socket::SOCKET_PATH_LIMIT
        || bytes.contains(&0)
    {
        return Err(denied());
    }
    Ok(())
}
fn socket_path(path: &Path) -> io::Result<PathBuf> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path.join("socket-path"))
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(path.join("socket")),
        Err(error) => return Err(error),
    };
    let info = file.metadata()?;
    let limit = crate::staged_socket::SOCKET_PATH_LIMIT;
    if !info.is_file()
        || info.uid() != uid()
        || info.mode() & 0o7777 != 0o600
        || info.nlink() != 1
        || info.len() > limit as u64
    {
        return Err(denied());
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    let socket = PathBuf::from(std::ffi::OsString::from_vec(bytes));
    socket_path_bytes(&socket)?;
    directory(socket.parent().ok_or_else(denied)?)?;
    Ok(socket)
}

/// PID from the same socket that passed the host identity handshake. Older
/// live hosts do not have an owner record, but can still be stopped safely.
pub(super) fn peer_pid(socket: &UnixStream) -> Option<libc::pid_t> {
    #[cfg(target_os = "macos")]
    {
        let mut pid: libc::pid_t = 0;
        let mut size = std::mem::size_of_val(&pid) as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut size,
            )
        };
        (result == 0 && pid > 0).then_some(pid)
    }
    #[cfg(target_os = "linux")]
    {
        let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
        let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                credentials.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if result != 0 || size as usize != std::mem::size_of::<libc::ucred>() {
            return None;
        }
        let pid = unsafe { credentials.assume_init() }.pid;
        (pid > 0).then_some(pid)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = socket;
        None
    }
}

/// The reserved host publishes its PID after claiming the one-use endpoint
/// and before creating any child. Keep this record through process exit.
pub(super) const OWNER_RECORD: &str = "owner.json";

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OwnerProcess {
    identity: super::api::Identity,
    namespace: Option<String>,
    pub host: libc::pid_t,
    pub child: Option<u32>,
}

pub(super) fn record_owner(
    path: &Path,
    identity: super::api::Identity,
    child: Option<u32>,
) -> io::Result<()> {
    let process = OwnerProcess {
        identity,
        namespace: super::hosted::owner_namespace().ok(),
        host: std::process::id() as libc::pid_t,
        child,
    };
    crate::fs_atomic::write_atomic_lua_private(
        &path.join(OWNER_RECORD),
        &serde_json::to_vec(&process)?,
    )
}

pub(super) fn recorded_owner(
    path: &Path,
    identity: &super::api::Identity,
) -> io::Result<OwnerProcess> {
    directory(path)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path.join(OWNER_RECORD))?;
    let info = file.metadata()?;
    if !info.is_file() || info.uid() != uid() || info.mode() & 0o7777 != 0o600 || info.nlink() != 1
    {
        return Err(denied());
    }
    const MAX_RECORD: u64 = 4096;
    let mut bytes = Vec::new();
    file.take(MAX_RECORD + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RECORD {
        return Err(denied());
    }
    let process: OwnerProcess = serde_json::from_slice(&bytes)?;
    if process.identity != *identity
        || process.namespace.as_deref() != Some(super::hosted::owner_namespace()?.as_str())
        || process.host <= 0
        || process.child == Some(0)
    {
        return Err(denied());
    }
    Ok(process)
}

// Only the fenced retirement path calls this, after confirming host exit.
pub(super) fn remove_owner_record(path: &Path, identity: &super::api::Identity) {
    if recorded_owner(path, identity).is_ok() {
        let _ = std::fs::remove_file(path.join(OWNER_RECORD));
        let _ = std::fs::remove_dir(path);
    }
}

/// A recycled PID or an unreadable process leaves ownership uncertain.
pub(super) fn owner_exited(pid: libc::pid_t) -> bool {
    if pid <= 0 {
        return false;
    }
    if unsafe { libc::kill(pid, 0) } == -1 {
        return io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    }
    // A zombie already exited. Its parent may not have collected it yet.
    child_exited(pid) || owner_is_zombie(pid)
}

fn child_exited(pid: libc::pid_t) -> bool {
    // WNOWAIT leaves collection to the thread or process that owns the child.
    let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut status,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    result == 0 && status.si_signo == libc::SIGCHLD
}

#[cfg(target_os = "macos")]
fn owner_is_zombie(pid: libc::pid_t) -> bool {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let result = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    result == size && unsafe { info.assume_init() }.pbi_status == libc::SZOMB
}

#[cfg(target_os = "linux")]
fn owner_is_zombie(pid: libc::pid_t) -> bool {
    // comm can contain whitespace and parentheses. The state follows its last ')'.
    std::fs::read(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            let end = stat.iter().rposition(|byte| *byte == b')')?;
            stat.get(end + 1..end + 4).map(|state| state == b" Z ")
        })
        .unwrap_or(false)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn owner_is_zombie(_: libc::pid_t) -> bool {
    false
}

pub(super) fn listener_missing(path: &Path) -> bool {
    let Ok(socket) = socket_path(path) else {
        return false;
    };
    connect_until(&socket, Instant::now() + Duration::from_millis(100)).is_err_and(|error| {
        matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        )
    })
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
    let socket = socket_path(path)?;
    let info = std::fs::symlink_metadata(&socket)?;
    if !info.file_type().is_socket() || info.uid() != uid() || info.mode() & 0o7777 != 0o600 {
        return Err(denied());
    }
    let stream = connect_until(&socket, until)?;
    verify_peer(&stream, uid())?;
    Channel::authenticate_until(stream, &key, true, until)
}
#[cfg(target_os = "macos")]
pub(super) fn connect_for_process_inspection(path: &Path) -> io::Result<UnixStream> {
    directory(path)?;
    let socket = socket_path(path)?;
    let info = std::fs::symlink_metadata(&socket)?;
    if !info.file_type().is_socket() || info.uid() != uid() || info.mode() & 0o7777 != 0o600 {
        return Err(denied());
    }
    let stream = connect_until(&socket, Instant::now() + Duration::from_secs(1))?;
    verify_peer(&stream, uid())?;
    Ok(stream)
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
    #[cfg(target_os = "linux")]
    let socket_flags = libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK;
    #[cfg(not(target_os = "linux"))]
    let socket_flags = libc::SOCK_STREAM;
    let fd = unsafe { libc::socket(libc::AF_UNIX, socket_flags, 0) };
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
    fn split_listener_probe_uses_the_runtime_socket_even_without_a_key() {
        let root = PathBuf::from("/tmp").join(format!(
            "rd-probe-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        DirBuilder::new().mode(0o700).create(&root).unwrap();
        let _cleanup = Cleanup(root.clone());
        let state = root.join("state");
        let socket = root.join("runtime/socket");
        let endpoint = Endpoint::bind_with_socket(&state, Some(&socket)).unwrap();

        std::fs::remove_file(state.join("capability")).unwrap();
        assert!(!listener_missing(&state));
        drop(endpoint);
        assert!(listener_missing(&state));
    }

    #[test]
    fn capability_restoration_refuses_a_replaced_directory() {
        let root = PathBuf::from("/tmp").join(format!(
            "rd-cap-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        DirBuilder::new().mode(0o700).create(&root).unwrap();
        let _cleanup = Cleanup(root.clone());
        let endpoint_path = root.join("state");
        let endpoint = Endpoint::bind(&endpoint_path).unwrap();
        let capability = endpoint_path.join("capability");
        std::fs::remove_file(&capability).unwrap();
        endpoint.restore_missing_capability().unwrap();
        assert!(std::fs::read(&capability).unwrap() == endpoint.key.0);
        assert_eq!(capability.metadata().unwrap().mode() & 0o7777, 0o600);
        std::fs::remove_file(&capability).unwrap();
        let moved = root.join("moved");
        std::fs::rename(&endpoint_path, &moved).unwrap();
        DirBuilder::new()
            .mode(0o700)
            .create(&endpoint_path)
            .unwrap();

        assert_eq!(
            endpoint.restore_missing_capability().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(!endpoint_path.join("capability").exists());
        assert!(!moved.join("capability").exists());
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
            endpoint.directory.as_raw_fd(),
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
        endpoint.restore_missing_capability().unwrap();
        assert!(std::fs::symlink_metadata(&capability)
            .unwrap()
            .file_type()
            .is_symlink());
        std::fs::remove_file(&capability).unwrap();
        std::fs::create_dir(&capability).unwrap();
        assert!(connect(&root).is_err());
        endpoint.restore_missing_capability().unwrap();
        assert!(capability.is_dir());
        std::fs::remove_dir(&capability).unwrap();
        std::fs::write(&capability, b"malformed").unwrap();
        endpoint.restore_missing_capability().unwrap();
        assert_eq!(std::fs::read(&capability).unwrap(), b"malformed");
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
