//! Talking to a daemon, including handing your terminal over to one.

use crate::ipc::{self, Stream, TryClone};
use remuda_core::protocol::{Request, Response};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

/// Detach key: Ctrl-\ (0x1C). Chosen because almost nothing binds it, unlike
/// Ctrl-C/D/Z, which the attached program needs. Consumed, never forwarded.
pub const DETACH: u8 = 0x1C;

/// Send one request and read one response.
pub fn request(path: &Path, request: &Request) -> std::io::Result<Response> {
    let stream = ipc::connect(path)?;
    send(&stream, request)?;
    read_response(&stream)
}

fn send(mut stream: &Stream, request: &Request) -> std::io::Result<()> {
    let mut line = serde_json::to_string(request)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()
}

fn read_response(stream: &Stream) -> std::io::Result<Response> {
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    serde_json::from_str(&line).map_err(std::io::Error::other)
}

/// Give this terminal to a session until the user presses [`DETACH`], then
/// return. Leaving does not disturb the session: the process keeps running and
/// the pty keeps its size, since nothing here can resize it.
pub fn attach(path: &Path, name: &str) -> std::io::Result<()> {
    let stream = ipc::connect(path)?;
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
            // Ends the screen pump below, which then returns from `attach` and
            // drops every handle on this connection — that hang-up is what the
            // daemon reads as "the human left".
            ipc::wake(&stream);
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

    ipc::wake(&stream);
    let _ = keys.join();
    Ok(())
}

/// Puts the terminal in raw mode and puts it back on the way out. Restoring
/// must stay in `Drop`: the exits that matter — an error, a panic, a `?` —
/// are exactly the ones that skip the end of `attach`.
struct RawMode;

impl RawMode {
    fn enable() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}
