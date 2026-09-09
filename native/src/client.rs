//! Talking to a daemon, including handing your terminal over to one.

use remuda_core::protocol::{Request, Response};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

/// Detach key: Ctrl-\ (0x1C).
///
/// In raw mode this is just a byte, and almost nothing binds it — unlike
/// Ctrl-C, Ctrl-D or Ctrl-Z, which the program you are attached to needs. It is
/// consumed by the client and never forwarded.
pub const DETACH: u8 = 0x1C;

/// Send one request and read one response.
pub fn request(path: &Path, request: &Request) -> std::io::Result<Response> {
    let stream = UnixStream::connect(path)?;
    send(&stream, request)?;
    read_response(&stream)
}

fn send(mut stream: &UnixStream, request: &Request) -> std::io::Result<()> {
    let mut line = serde_json::to_string(request)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()
}

fn read_response(stream: &UnixStream) -> std::io::Result<Response> {
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    serde_json::from_str(&line).map_err(std::io::Error::other)
}

/// Give this terminal to a session until the user presses [`DETACH`].
///
/// Returns once detached. The session is untouched by our leaving: the process
/// keeps running and the pty keeps its size, because nothing here can resize it
/// (`Session` exposes no such method).
pub fn attach(path: &Path, name: &str) -> std::io::Result<()> {
    let stream = UnixStream::connect(path)?;
    send(
        &stream,
        &Request::Attach {
            name: name.to_string(),
        },
    )?;

    // The acknowledgement is read before raw mode goes on. Failing here must
    // leave the terminal exactly as we found it, and a raw terminal printing an
    // error message is how a tool loses a user's trust in one keystroke.
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    match serde_json::from_str::<Response>(&line) {
        Ok(Response::Ok) => {}
        Ok(Response::Error(reason)) => {
            return Err(std::io::Error::other(reason));
        }
        _ => return Err(std::io::Error::other("daemon did not acknowledge attach")),
    }

    let _raw = RawMode::enable()?;

    // Keystrokes out, on their own thread; the screen pump runs here.
    let keys = std::thread::spawn({
        let mut stream = stream.try_clone()?;
        move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0u8; 1024];
            while let Ok(n) = stdin.read(&mut buf) {
                if n == 0 {
                    break;
                }
                match buf[..n].iter().position(|&b| b == DETACH) {
                    // Forward what was typed before the detach key, then stop.
                    // Dropping those bytes would silently swallow input the
                    // user believes they sent.
                    Some(at) => {
                        let _ = stream.write_all(&buf[..at]);
                        let _ = stream.flush();
                        break;
                    }
                    None => {
                        if stream.write_all(&buf[..n]).is_err() || stream.flush().is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    });

    let mut stdout = std::io::stdout();
    let mut buf = [0u8; 8192];
    while let Ok(n) = reader.read(&mut buf) {
        if n == 0 {
            break;
        }
        if stdout.write_all(&buf[..n]).is_err() || stdout.flush().is_err() {
            break;
        }
    }

    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = keys.join();
    Ok(())
}

/// Puts the terminal in raw mode and puts it back on the way out.
///
/// Restoring lives in `Drop` rather than at the end of `attach` because the
/// interesting exits are the ones that skip the end: an error, a panic, a `?`.
/// A tool that leaves a terminal raw is one the user has to blindly type
/// `reset` into.
struct RawMode {
    original: nix::sys::termios::Termios,
}

impl RawMode {
    fn enable() -> std::io::Result<Self> {
        use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg};
        let stdin = std::io::stdin();
        let original = tcgetattr(&stdin).map_err(std::io::Error::other)?;
        let mut raw = original.clone();
        cfmakeraw(&mut raw);
        tcsetattr(&stdin, SetArg::TCSANOW, &raw).map_err(std::io::Error::other)?;
        Ok(Self { original })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        use nix::sys::termios::{tcsetattr, SetArg};
        let _ = tcsetattr(std::io::stdin(), SetArg::TCSANOW, &self.original);
    }
}
