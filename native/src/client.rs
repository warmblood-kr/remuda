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

/// Which way a ride ended. Both used to return `Ok(())`, and the terminal
/// looked identical either way — that is the incident this exists for: a second
/// `exit` went to the user's real login shell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Left {
    Detached,
    Exited,
}

/// Give this terminal to a session until the user presses [`DETACH`] or the
/// session ends. Leaving does not disturb the session: the process keeps
/// running and the pty keeps its size, since nothing here can resize it.
pub fn attach(path: &Path, name: &str) -> std::io::Result<Left> {
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

    // Only the key thread can tell the two exits apart: the reader below just
    // sees the stream end, which is true of a detach and of a death alike.
    let detached = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Keystrokes out, on their own thread; the screen pump runs here.
    let keys = std::thread::spawn({
        let mut stream = stream.try_clone()?;
        let detached = std::sync::Arc::clone(&detached);
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
                        detached.store(true, std::sync::atomic::Ordering::SeqCst);
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
    let left = if detached.load(std::sync::atomic::Ordering::SeqCst) {
        Left::Detached
    } else {
        // The session ended while the key thread sits in a tty read, and a tty
        // read cannot be interrupted portably — so `join` below returns only on
        // the next keystroke, which it then swallows. Say so instead of
        // freezing: a stated wait is not the same failure as a dead screen.
        let _ = write!(stdout, "\r\n[remuda] {name} ended — press any key\r\n");
        let _ = stdout.flush();
        Left::Exited
    };
    let _ = keys.join();
    Ok(left)
}

/// Raw mode plus the alternate screen, restored on the way out. Restoring must
/// stay in `Drop`: the exits that matter — an error, a panic, a `?` — are
/// exactly the ones that skip the end of `attach`.
pub struct RawMode;

impl RawMode {
    /// The alternate screen is what makes leaving *visible*: your scrollback
    /// and prompt come back, so a second `exit` cannot be aimed at the wrong
    /// shell by a screen that never changed.
    pub fn enable() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        if let Err(e) =
            crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen)
        {
            let _ = crossterm::terminal::disable_raw_mode();
            return Err(e);
        }
        Ok(Self)
    }
}

impl Drop for RawMode {
    /// Leave the alternate screen *before* termios goes back, so the last thing
    /// the terminal does in raw mode is the buffer switch.
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}
