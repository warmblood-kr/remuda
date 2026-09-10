//! The daemon: it owns the sessions, and it outlives every client.
//!
//! 정수님, 2026-09-10: *"일종의 tmux 같은 pty manager, which support daemon
//! mode. with session list so that user can select a session to attach."*
//!
//! That "outlives" is the whole reason a daemon exists rather than a library
//! call. A session is where an agent gets logged in by a human and then works
//! for hours; if it died when the viewer's terminal closed, attaching could
//! never be the credential path it has to be.
//!
//! One connection carries one request. After an accepted `Attach` the same
//! connection stops speaking the protocol and becomes a raw byte pipe, in both
//! directions, until either end hangs up.

use crate::image::Image;
use crate::pty::PtyAgent;
use remuda_core::protocol::{Request, Response};
use remuda_core::{Registry, Session, Size};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::SystemClock;
use portable_pty::CommandBuilder;

/// Where a node's socket lives. Under `$XDG_RUNTIME_DIR` when the system
/// provides one (it is cleaned up on logout and is not world-writable), else a
/// per-uid directory under `/tmp`.
pub fn socket_path(server: &str) -> PathBuf {
    let base = std::env::var_os("REMUDA_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from))
        .unwrap_or_else(|| {
            let who = std::env::var("USER").unwrap_or_else(|_| "nobody".into());
            PathBuf::from(format!("/tmp/remuda-{who}"))
        });
    base.join("remuda").join(format!("{server}.sock"))
}

/// Serve until the listener dies. Caution: connect before unlinking — an
/// unconditional unlink displaces a *live* peer, which then keeps running
/// unreachable and holds its pty children forever.
pub fn serve(path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if UnixStream::connect(path).is_ok() {
        return Err(std::io::Error::other(format!(
            "a daemon is already listening at {} — pick a different name (remuda -s <name>) \
             or stop it first",
            path.display()
        )));
    }
    // Nothing answered, so any file here is a stale socket from a crashed
    // daemon, not a live peer's. Removing it is what lets bind succeed instead
    // of failing forever on an address already in use.
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;

    let registry = Arc::new(Registry::new());
    // The image starts with the daemon and lives exactly as long (step 007).
    // It is started *after* the bind, so the `remuda` table it binds points at
    // a socket that is already accepting — the interpreter's first call cannot
    // race the listener it will talk to.
    let image = Image::spawn(path);
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let registry = Arc::clone(&registry);
        let image = image.clone();
        std::thread::spawn(move || {
            let _ = handle(stream, &registry, &image);
        });
    }
    Ok(())
}

fn handle(stream: UnixStream, registry: &Registry, image: &Image) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }

    let request: Request = match serde_json::from_str(&line) {
        Ok(request) => request,
        Err(e) => return reply(&stream, &Response::error(format!("bad request: {e}"))),
    };

    match request {
        Request::List => reply(&stream, &Response::Sessions(registry.list())),

        Request::New {
            name,
            command,
            size,
        } => match spawn(&name, &command, size) {
            Err(e) => reply(&stream, &Response::error(e)),
            Ok(session) => match registry.register(session) {
                Ok(_) => reply(&stream, &Response::Ok),
                Err(_) => reply(&stream, &Response::error(format!("name taken: {name}"))),
            },
        },

        Request::SendLine { name, text } => match registry.send_line(&name, &text) {
            None => reply(
                &stream,
                &Response::error(format!("no such session: {name}")),
            ),
            Some(Err(e)) => reply(&stream, &Response::error(e)),
            Some(Ok(())) => reply(&stream, &Response::Ok),
        },

        Request::Send { name, bytes } => match registry.send(&name, &bytes) {
            None => reply(
                &stream,
                &Response::error(format!("no such session: {name}")),
            ),
            Some(Err(e)) => reply(&stream, &Response::error(e)),
            Some(Ok(())) => reply(&stream, &Response::Ok),
        },

        Request::Capture { name } => match registry.screen_text(&name) {
            None => reply(
                &stream,
                &Response::error(format!("no such session: {name}")),
            ),
            Some(Err(e)) => reply(&stream, &Response::error(e)),
            Some(Ok(text)) => reply(&stream, &Response::Screen(text)),
        },

        Request::Attach { name } => attach(stream, reader, registry, &name),

        Request::Close { name } => match registry.close(&name) {
            None => reply(
                &stream,
                &Response::error(format!("no such session: {name}")),
            ),
            Some(Err(e)) => reply(&stream, &Response::error(e)),
            Some(Ok(())) => reply(&stream, &Response::Ok),
        },

        Request::Eval { code, name } => match image.eval(&code, name.as_deref()) {
            Ok(value) => reply(&stream, &Response::Value(value)),
            // Lua's own message, which already carries the line and a
            // traceback — the same treatment `remuda run` gives a script file.
            Err(e) => reply(&stream, &Response::error(e)),
        },
    }
}

fn spawn(name: &str, command: &[String], size: Size) -> Result<Session, String> {
    let mut argv = command.to_vec();
    if argv.is_empty() {
        argv.push(std::env::var("SHELL").unwrap_or_else(|_| "sh".into()));
    }
    let mut builder = CommandBuilder::new(&argv[0]);
    for arg in &argv[1..] {
        builder.arg(arg);
    }
    if let Ok(cwd) = std::env::current_dir() {
        builder.cwd(cwd);
    }

    let agent = PtyAgent::spawn(builder, size).map_err(|e| e.to_string())?;
    Ok(Session::new(
        name,
        Box::new(agent),
        Arc::new(SystemClock::new()),
    ))
}

/// Hand this connection over to a human. Exclusivity is enforced by
/// `Session::attach` returning `None`, not here, so a second viewer is refused
/// even when it arrives over some later transport.
fn attach(
    stream: UnixStream,
    mut reader: BufReader<UnixStream>,
    registry: &Registry,
    name: &str,
) -> std::io::Result<()> {
    let Some(session) = registry.get(name) else {
        return reply(
            &stream,
            &Response::error(format!("no such session: {name}")),
        );
    };
    let Some(held) = session.attach() else {
        return reply(
            &stream,
            &Response::error("already attached by someone else"),
        );
    };
    reply(&stream, &Response::Ok)?;

    // Paint what is already on screen before streaming anything new, or the
    // viewer sees a blank terminal until the program next redraws.
    let mut out = stream.try_clone()?;
    if let Ok(painted) = held.screen_bytes() {
        out.write_all(&painted)?;
        out.flush()?;
    }

    // Scoped threads, so the single exclusive guard can be shared with the
    // input pump rather than cloned or re-taken. There is still exactly one
    // `Attached` in existence, which is the invariant that makes raw writes
    // safe in the first place.
    // The two pumps block on *different* things — one on the socket, one on the
    // pty — so neither can be woken by the other's end-of-stream. A shared flag
    // plus a bounded wait is what lets either side end the attachment.
    //
    // Measured, not foreseen: without this, detaching left the output pump
    // parked on recv() from an idle shell, the scope never closed, the guard
    // was never dropped, and the session stayed locked to a viewer that had
    // already gone. The core got "a human is attached" forever.
    let done = std::sync::atomic::AtomicBool::new(false);
    let done = &done;

    std::thread::scope(|scope| {
        // Keystrokes in, on their own thread: reading a socket blocks, and the
        // output pump must not wait on the human to type.
        let held = &held;
        scope.spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || held.write_raw(&buf[..n]).is_err() {
                    break;
                }
            }
            done.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        if let Some(rx) = held.subscribe() {
            while !done.load(std::sync::atomic::Ordering::SeqCst) {
                match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                    Ok(chunk) => {
                        if out.write_all(&chunk).is_err() || out.flush().is_err() {
                            break;
                        }
                    }
                    // Timeout: nothing was printed, which is the normal state of
                    // an idle agent. Loop back and re-check whether we are done.
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    // The sender is gone: the process exited.
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        }
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        // Unblocks the key thread's read so the scope can close.
        let _ = stream.shutdown(std::net::Shutdown::Both);
    });
    Ok(())
}

fn reply(mut stream: &UnixStream, response: &Response) -> std::io::Result<()> {
    let mut line = serde_json::to_string(response)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()
}
