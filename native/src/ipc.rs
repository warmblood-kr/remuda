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

fn name(path: &Path) -> io::Result<Name<'_>> {
    path.to_fs_name::<GenericFilePath>()
}

/// Connect to a daemon. The error is the transport's own, so "nothing is
/// listening" still reads the way it did over a unix socket.
pub fn connect(path: &Path) -> io::Result<Stream> {
    Stream::connect(name(path)?)
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
        // Nothing answered at this address (the caller checked), so any file
        // here is stale. Removing it is what lets bind succeed instead of
        // failing forever on an address already in use.
        let _ = std::fs::remove_file(path);
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
        // operation and this is the API that cancels one.
        unsafe {
            windows_sys::Win32::System::IO::CancelIoEx(
                pipe.as_handle().as_raw_handle(),
                std::ptr::null(),
            )
        };
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
