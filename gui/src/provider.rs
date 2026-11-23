//! Bounded subprocess adapter, called only from background workers.
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const TIMEOUT: Duration = Duration::from_secs(8);
const STDOUT_LIMIT: usize = 1024 * 1024;
const STDERR_LIMIT: usize = 64 * 1024;

#[cfg(unix)]
pub fn output(
    program: impl AsRef<std::ffi::OsStr>,
    args: &[&str],
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    use std::io::{self, Read};
    use std::os::fd::AsRawFd;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let result = (|| {
        for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags == -1
                || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
            {
                return Err(io::Error::last_os_error().to_string());
            }
        }
        let deadline = Instant::now() + timeout;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut out_done = false;
        let mut err_done = false;
        let mut status = None;
        loop {
            fn read_available<R: Read>(
                reader: &mut R,
                into: &mut Vec<u8>,
                limit: usize,
                done: &mut bool,
            ) -> Result<(), String> {
                if *done {
                    return Ok(());
                }
                let mut buf = [0; 8192];
                // A bounded number of reads per turn keeps timeout and the other pipe responsive.
                for _ in 0..16 {
                    match reader.read(&mut buf) {
                        Ok(0) => {
                            *done = true;
                            break;
                        }
                        Ok(n) => {
                            if n > limit.saturating_sub(into.len()) {
                                return Err("Provider output limit exceeded".into());
                            }
                            into.extend_from_slice(&buf[..n]);
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(e.to_string()),
                    }
                }
                Ok(())
            }
            read_available(&mut stdout, &mut out, STDOUT_LIMIT, &mut out_done)?;
            read_available(&mut stderr, &mut err, STDERR_LIMIT, &mut err_done)?;
            if status.is_none() {
                status = child.try_wait().map_err(|e| e.to_string())?;
            }
            if let Some(status) = status {
                if out_done && err_done {
                    return if status.success() {
                        Ok(out)
                    } else {
                        Err(String::from_utf8_lossy(&err).into_owned())
                    };
                }
            }
            if Instant::now() >= deadline {
                return Err("Provider request timed out".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
    result
}

#[cfg(windows)]
pub fn output(
    program: impl AsRef<std::ffi::OsStr>,
    args: &[&str],
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    use std::io::Read;
    use std::sync::mpsc;

    fn drain(
        pipe: impl Read + Send + 'static,
        limit: usize,
    ) -> mpsc::Receiver<Result<Vec<u8>, String>> {
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = pipe
                .take(limit as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|error| error.to_string())
                .and_then(|_| {
                    if bytes.len() > limit {
                        Err("Provider output limit exceeded".into())
                    } else {
                        Ok(bytes)
                    }
                });
            let _ = sender.send(result);
        });
        receiver
    }

    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let stdout = drain(child.stdout.take().expect("piped stdout"), STDOUT_LIMIT);
    let stderr = drain(child.stderr.take().expect("piped stderr"), STDERR_LIMIT);
    let deadline = Instant::now() + timeout;
    let result = (|| {
        let receive = |pipe: mpsc::Receiver<Result<Vec<u8>, String>>| {
            pipe.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|error| format!("Provider response: {error}"))?
        };
        let out = receive(stdout)?;
        let err = receive(stderr)?;
        loop {
            if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
                return if status.success() {
                    Ok(out)
                } else {
                    Err(String::from_utf8_lossy(&err).into_owned())
                };
            }
            if Instant::now() >= deadline {
                return Err("Provider request timed out".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
    result
}

#[cfg(not(any(unix, windows)))]
pub fn output(
    _program: impl AsRef<std::ffi::OsStr>,
    _args: &[&str],
    _timeout: Duration,
) -> Result<Vec<u8>, String> {
    Err("Bounded GUI provider is unavailable on this platform".into())
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn windows_worker_captures_output_and_propagates_failure() {
        let out = output("cmd.exe", &["/d", "/c", "echo provider-ok"], TIMEOUT).unwrap();
        assert_eq!(String::from_utf8(out).unwrap().trim(), "provider-ok");
        assert!(output("cmd.exe", &["/d", "/c", "exit 9"], TIMEOUT).is_err());
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn captures_success_and_error() {
        assert_eq!(
            output("/bin/sh", &["-c", "printf ok"], TIMEOUT).unwrap(),
            b"ok"
        );
        assert_eq!(
            output("/bin/sh", &["-c", "printf bad >&2; exit 7"], TIMEOUT).unwrap_err(),
            "bad"
        );
    }
    #[test]
    fn timeout_kills_and_reaps_direct_child() {
        let start = Instant::now();
        let result = output(
            "/bin/sh",
            &["-c", "exec sleep 10"],
            Duration::from_millis(100),
        );
        assert!(result.unwrap_err().contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(3));
    }
    #[test]
    fn limits_stdout_and_stderr() {
        for cmd in ["exec yes", "exec yes >&2"] {
            let result = output("/bin/sh", &["-c", cmd], TIMEOUT);
            assert!(result.unwrap_err().contains("limit exceeded"));
        }
    }
}
