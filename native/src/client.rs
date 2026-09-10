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
    Ok(interpret(&line))
}

/// What a daemon says when it cannot READ what we sent. Only the deserializer
/// produces it — every refusal about content ("no such session") is worded by a
/// handler — so this prefix means one thing and nothing else.
const CANNOT_READ: &str = "bad request: ";

/// One reply, with the two shapes of version skew turned into words a person
/// can act on. Everything else is passed through exactly as the daemon wrote it:
/// a malformed request that is NOT skew must still say what it was.
fn interpret(line: &str) -> Response {
    if line.trim().is_empty() {
        return Response::error("the daemon hung up without answering");
    }
    match serde_json::from_str::<Response>(line) {
        // We serialized that request from this binary's own `Request`, so a
        // daemon on this build CANNOT fail to read it. One that did is not.
        Ok(Response::Error(reason)) => match reason.strip_prefix(CANNOT_READ) {
            Some(detail) => {
                Response::error(skew(&format!("it could not read the request: {detail}")))
            }
            None => Response::Error(reason),
        },
        Ok(other) => other,
        // The mirror: a reply we cannot read, from a daemon newer than we are.
        Err(e) => Response::error(skew(&format!("its reply did not parse: {e}"))),
    }
}

/// Every daemon deployed before any handshake existed can never announce its own
/// version, so the client's reaction to the failure is the only diagnosis that
/// reaches those users. See [`tests::the_cure_survives_an_eighty_column_crop`].
fn skew(detail: &str) -> String {
    // One line, cure first. The TUI footer is one row of `cols` and crops the
    // tail: measured at 80 columns, a message that led with the diagnosis lost
    // `remuda restart` off the right edge — the only actionable half of it.
    format!(
        "the daemon is not this build — run `remuda restart`, then this again. \
         This command is {}; the daemon: {detail}",
        crate::dist::VERSION,
    )
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
    match interpret(&line) {
        Response::Ok => {}
        Response::Error(reason) => {
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

#[cfg(test)]
mod tests {
    use super::interpret;
    use remuda_core::protocol::Response;

    /// The line the daemon at 618cda4^ actually sent, byte for byte.
    const STALE: &str =
        r#"{"Error":"bad request: invalid type: null, expected a string at line 1 column 19"}"#;

    #[test]
    fn a_daemon_that_cannot_read_us_is_named_as_a_stale_build() {
        let Response::Error(said) = interpret(STALE) else {
            panic!("not an error")
        };
        assert!(said.starts_with("the daemon is not this build"), "{said}");
        // The serde detail is the real diagnostic; it must survive, just not lead.
        assert!(said.contains("invalid type: null"), "{said}");
    }

    #[test]
    fn an_unknown_variant_is_the_same_diagnosis() {
        // What an old daemon says to `Version` — the shape step 015 must not
        // turn into something more confusing than what it replaced.
        let line = r#"{"Error":"bad request: unknown variant `Version`, expected one of `List`, `New` at line 1 column 11"}"#;
        let Response::Error(said) = interpret(line) else {
            panic!("not an error")
        };
        assert!(said.starts_with("the daemon is not this build"), "{said}");
    }

    /// The TUI footer is one row wide and crops the tail, so a message that
    /// leads with the diagnosis loses the cure off the right edge. Measured at
    /// 80 columns with the 8-character "remuda: " prefix a caller adds.
    #[test]
    fn the_cure_survives_an_eighty_column_crop() {
        let Response::Error(said) = interpret(STALE) else {
            panic!("not an error")
        };
        let footer: String = format!("remuda: {said}").chars().take(80).collect();
        assert!(
            footer.contains("remuda restart"),
            "cropped away the cure:\n{footer}"
        );
    }

    /// The negative control: a refusal about CONTENT is not skew and must reach
    /// the user in the daemon's own words. Without this the fix above would
    /// swallow every error into one sentence and be indistinguishable from it.
    #[test]
    fn an_ordinary_refusal_is_passed_through_untouched() {
        let line = r#"{"Error":"no such session: build"}"#;
        assert_eq!(
            interpret(line),
            Response::Error("no such session: build".into())
        );
    }

    #[test]
    fn a_hangup_is_not_reported_as_a_parse_failure() {
        let Response::Error(said) = interpret("") else {
            panic!("not an error")
        };
        assert!(said.contains("hung up"), "{said}");
    }

    #[test]
    fn a_normal_reply_still_arrives_as_itself() {
        assert_eq!(interpret(r#""Ok""#), Response::Ok);
    }
}
