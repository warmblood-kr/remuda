//! Opt-in, worker-free local requests with one transport deadline.

use remuda_core::protocol::{Request, Response};
use std::{io, path::Path, time::Duration};

/// Connect, send and receive against one deadline, without extra workers.
/// Unix only; other platforms return Unsupported before attempting connection.
/// Existing request_with_timeout callers retain their current behavior.
pub fn request_with_deadline(
    path: &Path,
    request: &Request,
    timeout: Duration,
) -> io::Result<Response> {
    #[cfg(unix)]
    {
        unix::request(path, request, timeout)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, request, timeout);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Bounded document requests are currently supported on Unix only; Windows document browsing is not supported yet",
        ))
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use crate::ipc::Stream;
    use interprocess::{
        local_socket::{traits::Stream as _, ConnectOptions},
        ConnectWaitMode,
    };
    use std::{
        io::{Read, Write},
        time::Instant,
    };

    struct Deadline(Instant);

    impl Deadline {
        fn remaining(&self) -> io::Result<Duration> {
            self.0
                .checked_duration_since(Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "document request deadline exceeded",
                    )
                })
        }

        fn pause(&self) -> io::Result<()> {
            std::thread::sleep(self.remaining()?.min(Duration::from_millis(2)));
            self.remaining().map(|_| ())
        }
    }

    pub(super) fn request(
        path: &Path,
        request: &Request,
        timeout: Duration,
    ) -> io::Result<Response> {
        request_using(path, request, timeout, connect)
    }

    fn connect(path: &Path, deadline: &Deadline) -> io::Result<Stream> {
        use std::os::fd::{AsFd, AsRawFd};
        let stream = ConnectOptions::new()
            .name(crate::ipc::name(path)?)
            .wait_mode(ConnectWaitMode::Timeout(deadline.remaining()?))
            .nonblocking_stream(true)
            .connect_sync()?;
        // A saturated Linux AF_UNIX queue can report EAGAIN rather than an
        // in-progress connection. Readiness alone must not imply connection.
        let Stream::UdSocket(socket) = &stream;
        nix::sys::socket::getpeername::<nix::sys::socket::UnixAddr>(socket.as_fd().as_raw_fd())
            .map_err(io::Error::from)?;
        Ok(stream)
    }

    fn request_using(
        path: &Path,
        request: &Request,
        timeout: Duration,
        connect: impl FnOnce(&Path, &Deadline) -> io::Result<Stream>,
    ) -> io::Result<Response> {
        let deadline = Deadline(Instant::now().checked_add(timeout).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "document deadline out of range",
            )
        })?);
        deadline.remaining()?;
        let mut wire = serde_json::to_vec(request)?;
        wire.push(b'\n');
        deadline.remaining()?;
        let stream = connect(path, &deadline)?;
        // Even injected connectors must return before any request bytes are sent
        // if they consumed the budget. Production connect is itself bounded.
        deadline.remaining()?;
        stream.set_nonblocking(true)?;
        let mut stream = &stream;
        let mut sent = 0;
        while sent < wire.len() {
            deadline.remaining()?;
            match stream.write(&wire[sent..]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(count) => sent += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => deadline.pause()?,
                Err(error) => return Err(error),
            }
        }
        let limit = crate::reply_limit::max_reply_wire_bytes();
        let mut bytes = Vec::with_capacity(8192);
        let mut chunk = [0; 8192];
        loop {
            deadline.remaining()?;
            let room = (limit + 1).saturating_sub(bytes.len()).min(chunk.len());
            match stream.read(&mut chunk[..room]) {
                Ok(count) => {
                    let end = chunk[..count].iter().position(|&b| b == b'\n');
                    bytes.extend_from_slice(&chunk[..end.map_or(count, |i| i + 1)]);
                    if bytes.len() > limit {
                        return Err(super::super::reply_too_large());
                    }
                    if end.is_some() || count == 0 {
                        deadline.remaining()?;
                        let line = String::from_utf8(bytes)
                            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                        let response = super::super::interpret(&line);
                        deadline.remaining()?;
                        return Ok(response);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => deadline.pause()?,
                Err(error) => return Err(error),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use interprocess::local_socket::traits::Listener as _;
        use std::io::BufRead;
        use std::sync::atomic::{AtomicU64, Ordering};

        struct Endpoint(std::path::PathBuf);
        impl Endpoint {
            fn new() -> Self {
                static NEXT: AtomicU64 = AtomicU64::new(0);
                Self(std::env::temp_dir().join(format!(
                    "doc-budget-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                )))
            }
        }
        impl Drop for Endpoint {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }

        #[test]
        fn blocked_connector_expires_and_repeated_requests_can_recover() {
            let endpoint = Endpoint::new();
            for _ in 0..8 {
                let error = request_using(
                    &endpoint.0,
                    &Request::Version,
                    Duration::from_millis(10),
                    |_, deadline| loop {
                        deadline.pause()?;
                    },
                )
                .unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            }
            let listener = crate::ipc::listen(&endpoint.0).unwrap();
            let server = std::thread::spawn(move || {
                for _ in 0..8 {
                    let mut stream = listener.accept().unwrap();
                    let mut request = String::new();
                    std::io::BufReader::new(&stream)
                        .read_line(&mut request)
                        .unwrap();
                    stream.write_all(b"{\"Value\":\"recovered\"}\n").unwrap();
                }
            });
            for _ in 0..8 {
                assert_eq!(
                    request(&endpoint.0, &Request::Version, Duration::from_secs(1)).unwrap(),
                    Response::Value("recovered".into())
                );
            }
            server.join().unwrap();
        }

        #[test]
        fn saturated_unix_listener_fails_boundedly_then_recovers() {
            let endpoint = Endpoint::new();
            let listener = std::os::unix::net::UnixListener::bind(&endpoint.0).unwrap();
            let mut queued = Vec::new();
            let mut saturated = false;
            for _ in 0..512 {
                let start = Instant::now();
                let result = connect(&endpoint.0, &Deadline(start + Duration::from_millis(30)));
                assert!(start.elapsed() < Duration::from_secs(1));
                match result {
                    Ok(stream) => queued.push(stream),
                    Err(_) => {
                        saturated = true;
                        break;
                    }
                }
            }
            assert!(saturated, "test did not fill the kernel listener queue");
            let start = Instant::now();
            assert!(request(&endpoint.0, &Request::Version, Duration::from_millis(30)).is_err());
            assert!(start.elapsed() < Duration::from_secs(1));
            for _ in 0..queued.len() {
                drop(listener.accept().unwrap());
            }
            drop(queued);
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .write_all(b"{\"Value\":\"capacity restored\"}\n")
                    .unwrap();
            });
            assert_eq!(
                request(&endpoint.0, &Request::Version, Duration::from_secs(1)).unwrap(),
                Response::Value("capacity restored".into())
            );
            server.join().unwrap();
        }

        #[test]
        fn exhausted_connect_budget_sends_no_request_bytes() {
            let endpoint = Endpoint::new();
            let listener = crate::ipc::listen(&endpoint.0).unwrap();
            let server = std::thread::spawn(move || {
                let mut stream = listener.accept().unwrap();
                stream
                    .set_recv_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                assert_eq!(stream.read(&mut [0; 1]).unwrap(), 0);
            });
            let error = request_using(
                &endpoint.0,
                &Request::Version,
                Duration::from_millis(20),
                |path, deadline| {
                    let stream = connect(path, deadline)?;
                    std::thread::sleep(Duration::from_millis(40));
                    Ok(stream)
                },
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            server.join().unwrap();
        }

        #[test]
        fn connect_and_read_share_the_original_budget() {
            let endpoint = Endpoint::new();
            let listener = crate::ipc::listen(&endpoint.0).unwrap();
            let (release, wait) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                let _stream = listener.accept().unwrap();
                wait.recv_timeout(Duration::from_secs(2)).unwrap();
            });
            let start = Instant::now();
            let error = request_using(
                &endpoint.0,
                &Request::Version,
                Duration::from_millis(200),
                |path, deadline| {
                    let stream = connect(path, deadline)?;
                    std::thread::sleep(Duration::from_millis(120));
                    Ok(stream)
                },
            )
            .unwrap_err();
            let elapsed = start.elapsed();
            release.send(()).unwrap();
            server.join().unwrap();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            assert!(
                elapsed >= Duration::from_millis(200) && elapsed < Duration::from_millis(290),
                "{elapsed:?}"
            );
        }

        #[test]
        fn a_peer_that_never_reads_cannot_hold_a_large_write() {
            let endpoint = Endpoint::new();
            let listener = crate::ipc::listen(&endpoint.0).unwrap();
            let (release, wait) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                let _stream = listener.accept().unwrap();
                wait.recv_timeout(Duration::from_secs(2)).unwrap();
            });
            let start = Instant::now();
            let error = request(
                &endpoint.0,
                &Request::Vfs {
                    args: vec!["x".repeat(4 * 1024 * 1024)],
                    input_base64: None,
                },
                Duration::from_millis(250),
            )
            .unwrap_err();
            let elapsed = start.elapsed();
            release.send(()).unwrap();
            server.join().unwrap();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
        }

        #[test]
        fn zero_budget_does_not_call_the_connector() {
            let error = request_using(
                Path::new("unused"),
                &Request::Version,
                Duration::ZERO,
                |_, _| panic!("expired request connected"),
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        }
    }
}
