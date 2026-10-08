//! One local socket for both platforms: a unix socket, or a Windows named pipe.
//!
//! `interprocess` hides the transport, so `client` and `daemon` are one code
//! path rather than two cfg-forked ones. Exactly one thing does not hide, and it
//! is the reason this module exists rather than a type alias: **a named pipe has
//! no `shutdown`.** Unix wakes a thread blocked in `read` by half-closing the
//! socket under it; Windows has to cancel the pending read instead. Both mean
//! "stop waiting", neither closes the handle, and [`wake`] is where the two meet.
//!
//! The address is still a `Path` on both sides, because that is what
//! `daemon::socket_path` produces — a filesystem path on unix, and the literal
//! `\\.\pipe\…` name on Windows, which `GenericFilePath` passes through verbatim.

use interprocess::local_socket::traits::Stream as _;
use interprocess::local_socket::{GenericFilePath, ListenerOptions, Name, ToFsName};
use std::io;
use std::path::Path;

pub use interprocess::local_socket::{Listener, Stream};
pub use interprocess::TryClone;

#[derive(Clone, Copy)]
pub(crate) enum WakeHandle {
    #[cfg(unix)]
    Unix(std::os::fd::RawFd),
    #[cfg(windows)]
    Windows(usize),
}

/// Keep the stream alive for as long as the returned handle may be used.
pub(crate) fn wake_handle(stream: &Stream) -> WakeHandle {
    #[cfg(unix)]
    {
        use std::os::fd::{AsFd, AsRawFd};
        let Stream::UdSocket(socket) = stream;
        WakeHandle::Unix(socket.as_fd().as_raw_fd())
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsHandle, AsRawHandle};
        let Stream::NamedPipe(pipe) = stream;
        WakeHandle::Windows(pipe.as_handle().as_raw_handle() as usize)
    }
}

pub(crate) fn wake_captured(handle: WakeHandle) {
    #[cfg(unix)]
    {
        let WakeHandle::Unix(fd) = handle;
        let _ = nix::sys::socket::shutdown(fd, nix::sys::socket::Shutdown::Both);
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::RawHandle;
        let WakeHandle::Windows(handle) = handle;
        use windows_sys::Win32::System::IO::CancelIoEx;
        unsafe {
            CancelIoEx(handle as RawHandle, std::ptr::null());
        }
    }
}

fn name(path: &Path) -> io::Result<Name<'_>> {
    #[cfg(unix)]
    {
        // sun_path has room for a trailing NUL, so these are usable bytes.
        const LIMIT: usize = if cfg!(any(target_os = "linux", target_os = "android")) {
            107
        } else {
            103
        };
        check_socket_path_len(path.as_os_str().len(), LIMIT)?;
    }
    path.to_fs_name::<GenericFilePath>()
}

#[cfg(unix)]
fn check_socket_path_len(len: usize, limit: usize) -> io::Result<()> {
    if len > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "socket path too long ({len} bytes, limit {limit})\n\
                 Next: set a shorter REMUDA_RUNTIME_DIR"
            ),
        ));
    }
    Ok(())
}

/// Connect to a daemon, retaining the endpoint path in transport errors.
pub fn connect(path: &Path) -> io::Result<Stream> {
    Stream::connect(name(path)?).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot connect to daemon at {}: {error}", path.display()),
        )
    })
}

/// Whether a failed connection proves no daemon is running: a missing endpoint
/// or (Unix) a refused stale socket. Any other error must surface, or a second
/// daemon could split control state from the sessions the first still owns.
pub fn may_start_daemon(_path: &Path, error: &io::Error) -> bool {
    match error.kind() {
        io::ErrorKind::NotFound => true,
        #[cfg(unix)]
        io::ErrorKind::ConnectionRefused => _path.exists(),
        _ => false,
    }
}

/// Bind, after clearing what a crashed daemon left behind. That cleanup is the
/// one genuinely platform-shaped step: a unix socket is a file in a directory
/// that may not exist yet, and a named pipe is neither.
pub fn listen(path: &Path) -> io::Result<Listener> {
    #[cfg(unix)]
    {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if path.exists() {
            match connect(path) {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "Remuda daemon is already listening",
                    ));
                }
                Err(error) if may_start_daemon(path, &error) => {
                    std::fs::remove_file(path)?;
                }
                Err(error) => return Err(error),
            }
        }
    }
    ListenerOptions::new().name(name(path)?).create_sync()
}

/// Wake a thread blocked reading `stream`, from another thread. Not a close:
/// the handle stays valid and the blocked `read` returns. Best-effort — a
/// caller that cannot proceed without it must not depend on the return.
pub fn wake(stream: &Stream) {
    #[cfg(unix)]
    {
        use std::os::fd::{AsFd, AsRawFd};
        let Stream::UdSocket(socket) = stream;
        let _ = nix::sys::socket::shutdown(
            socket.as_fd().as_raw_fd(),
            nix::sys::socket::Shutdown::Both,
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsHandle, AsRawHandle};
        let Stream::NamedPipe(pipe) = stream;
        // `interprocess` opens pipes FILE_FLAG_OVERLAPPED and drives them with
        // synchronous waits, so the blocked read *is* a pending overlapped
        // operation and this is the API that cancels one. A null OVERLAPPED
        // cancels every pending operation for this pipe handle, so callers use
        // wake only with the stream dedicated to the blocked reader.
        unsafe {
            windows_sys::Win32::System::IO::CancelIoEx(
                pipe.as_handle().as_raw_handle(),
                std::ptr::null(),
            )
        };
    }
}

/// Check whether the connected Windows named-pipe peer has closed without
/// changing the stream's read mode; `set_nonblocking` is unsupported there.
#[cfg(windows)]
pub fn peer_disconnected(stream: &Stream) -> io::Result<bool> {
    use std::os::windows::io::{AsHandle, AsRawHandle};
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;

    let Stream::NamedPipe(pipe) = stream;
    let mut available = 0u32;
    let result = unsafe {
        PeekNamedPipe(
            pipe.as_handle().as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    };
    if result != 0 {
        Ok(false)
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Stop a reader thread and wait for it to exit: `stop` (checked before
/// every read) covers "not reading yet"; retrying `wake` until `is_finished`
/// covers "reading, but the cancel arrived too early". See steps/029.
pub fn stop_reader(
    stream: &Stream,
    stop: &std::sync::atomic::AtomicBool,
    is_finished: impl Fn() -> bool,
) {
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    loop {
        wake(stream);
        if is_finished() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn socket_path_length_reports_both_unix_platform_limits() {
        for limit in [103, 107] {
            check_socket_path_len(limit, limit).expect("the limit itself fits");
            let error = check_socket_path_len(limit + 1, limit).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!(
                    "socket path too long ({} bytes, limit {limit})\nNext: set a shorter REMUDA_RUNTIME_DIR",
                    limit + 1
                )
            );
        }
    }

    #[test]
    fn only_absence_or_a_refused_existing_unix_socket_allows_autostart() {
        let path = std::env::temp_dir().join(format!("remuda-ipc-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);

        assert!(may_start_daemon(
            &path,
            &io::Error::from(io::ErrorKind::NotFound)
        ));
        assert!(!may_start_daemon(
            &path,
            &io::Error::from(io::ErrorKind::PermissionDenied)
        ));

        #[cfg(unix)]
        {
            assert!(!may_start_daemon(
                &path,
                &io::Error::from(io::ErrorKind::ConnectionRefused)
            ));
            std::fs::write(&path, "not a socket").expect("temporary endpoint marker");
            assert!(may_start_daemon(
                &path,
                &io::Error::from(io::ErrorKind::ConnectionRefused)
            ));
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn missing_daemon_connection_names_endpoint_path() {
        let path = std::env::temp_dir()
            .join(format!("remuda-ipc-missing-{}", std::process::id()))
            .join("daemon.sock");
        let error = connect(&path).unwrap_err();
        // Windows named pipes report Unsupported for a missing path.
        #[cfg(unix)]
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains(&path.display().to_string()));
        assert!(error.to_string().contains("cannot connect to daemon at"));
    }
}
