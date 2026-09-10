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
use crate::ipc::{self, Listener, Stream, TryClone};
use crate::pty::PtyAgent;
use interprocess::local_socket::traits::ListenerExt;
use remuda_core::protocol::{collapse_runs, Request, Response};
use remuda_core::{Registry, Session, Size};
use std::io::{BufRead, BufReader, Read, Write};
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
        .unwrap_or_else(default_runtime_dir);
    socket_path_in(&base, server)
}

/// The same derivation with the runtime directory supplied — what the daemon
/// tests use, so they exercise the shipped naming instead of a hand-built path
/// that only resembles it.
pub fn socket_path_in(base: &Path, server: &str) -> PathBuf {
    #[cfg(unix)]
    {
        base.join("remuda").join(format!("{server}.sock"))
    }
    // A named pipe has no directory to live in, so the runtime directory folds
    // into the pipe's NAME. That is what keeps `REMUDA_RUNTIME_DIR` isolating
    // one daemon from another on Windows the way a directory does on unix.
    #[cfg(windows)]
    {
        PathBuf::from(format!(
            r"\\.\pipe\remuda-{:016x}-{server}",
            fingerprint(base.as_os_str())
        ))
    }
}

fn default_runtime_dir() -> PathBuf {
    #[cfg(unix)]
    {
        let who = std::env::var("USER").unwrap_or_else(|_| "nobody".into());
        PathBuf::from(format!("/tmp/remuda-{who}"))
    }
    #[cfg(windows)]
    {
        let who = std::env::var("USERNAME").unwrap_or_else(|_| "nobody".into());
        PathBuf::from(format!(r"\\remuda\{who}"))
    }
}

/// FNV-1a over the runtime directory, so an arbitrarily long path still yields
/// a pipe name inside the 256-character limit. Only used on Windows.
#[cfg(windows)]
fn fingerprint(text: &std::ffi::OsStr) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The shell a bare `new` starts. `$SHELL` first on every host: a person's
/// shell is their own choice, and nothing here improves on it.
pub fn default_shell() -> String {
    shell_or_default(std::env::var("SHELL").ok())
}

/// `powershell.exe` and not `pwsh.exe`: 5.1 is in-box on every supported
/// Windows and 7 is a separate install, so `pwsh` risks prefilling the prompt
/// with a binary that is not on the machine — worse than the `cmd.exe` it replaces.
fn shell_or_default(configured: Option<String>) -> String {
    configured.unwrap_or_else(|| {
        if cfg!(windows) {
            "powershell.exe"
        } else {
            "sh"
        }
        .to_string()
    })
}

/// Whether a session that ended keeps its entry. Off by default: 정수님,
/// 2026-09-10, asked that a session go away by itself when its program exits.
/// Read in the DAEMON's environment, so changing it takes a `remuda restart`.
fn keep_exited() -> bool {
    std::env::var("REMUDA_KEEP_EXITED").is_ok_and(|v| v == "1")
}

/// Serve until the listener dies. Caution: connect before unlinking — an
/// unconditional unlink displaces a *live* peer, which then keeps running
/// unreachable and holds its pty children forever.
pub fn serve(path: &Path) -> std::io::Result<()> {
    if ipc::connect(path).is_ok() {
        return Err(std::io::Error::other(format!(
            "a daemon is already listening at {} — pick a different name (remuda -s <name>) \
             or stop it first",
            path.display()
        )));
    }
    let listener: Listener = ipc::listen(path)?;

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

fn handle(stream: Stream, registry: &Registry, image: &Image) -> std::io::Result<()> {
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
        // Where a session that ended stops being listed. Here rather than on a
        // timer because listing is the only moment the answer is looked at, and
        // a reaper thread would need a clock this layer is not given.
        Request::List => {
            if !keep_exited() {
                registry.reap();
            }
            reply(&stream, &Response::Sessions(registry.list()))
        }

        Request::Version => reply(&stream, &Response::Value(crate::dist::VERSION.into())),

        // Answer before going. A client left guessing from a hung-up socket
        // cannot tell "it stopped" from "it never heard me".
        Request::Shutdown => {
            reply(&stream, &Response::Ok)?;
            std::process::exit(0);
        }

        Request::New {
            name,
            command,
            size,
        } => {
            let name = match name {
                Some(given) => given,
                None => registry.unique_name(&remuda_core::registry::slug(
                    command
                        .first()
                        .map_or_else(default_shell, String::clone)
                        .as_str(),
                )),
            };
            match spawn(&name, &command, size) {
                Err(e) => reply(&stream, &Response::error(e)),
                Ok(session) => match registry.register(session) {
                    Ok(_) => reply(&stream, &Response::Value(name)),
                    Err(_) => reply(&stream, &Response::error(format!("name taken: {name}"))),
                },
            }
        }

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

        Request::CaptureStyled { name } => match registry.screen_cells(&name) {
            None => reply(
                &stream,
                &Response::error(format!("no such session: {name}")),
            ),
            Some(Err(e)) => reply(&stream, &Response::error(e)),
            Some(Ok(cells)) => {
                // Runs on the wire, not cells — see steps/022 for the 44x+
                // measured on a real screen.
                let runs = cells.iter().map(|row| collapse_runs(row)).collect();
                reply(&stream, &Response::StyledScreen(runs))
            }
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
        argv.push(default_shell());
    }
    let mut builder = CommandBuilder::new(&argv[0]);
    for arg in &argv[1..] {
        builder.arg(arg);
    }
    if let Ok(cwd) = std::env::current_dir() {
        builder.cwd(cwd);
    }
    // The daemon inherits its whole environment, and it is often auto-started
    // from something with no terminal — so `TERM` reaches the agent unset or
    // `dumb` and its TUI degrades for a reason nobody can see from inside.
    if std::env::var("TERM").map(|t| t == "dumb").unwrap_or(true) {
        builder.env("TERM", "xterm-256color");
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
    stream: Stream,
    mut reader: BufReader<Stream>,
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
        ipc::wake(&stream);
    });
    Ok(())
}

fn reply(mut stream: &Stream, response: &Response) -> std::io::Result<()> {
    let mut line = serde_json::to_string(response)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::shell_or_default;
    use remuda_core::agent::{Color, StyledCell};
    use remuda_core::protocol::collapse_runs;

    fn cell(text: &str, fg: Color) -> StyledCell {
        StyledCell {
            text: text.to_string(),
            fg,
            ..Default::default()
        }
    }

    /// [MEASURED] A real screen collapses to a fraction of the per-cell wire
    /// size — the fix for the 113x multiplier steps/020 introduced. See
    /// steps/022.
    #[test]
    fn collapsing_to_runs_shrinks_the_wire_size_a_real_screen_produces() {
        // 80x24: one coloured prompt-shaped run of text on row 0, everything
        // else default — the realistic case this fix targets, a handful of
        // style runs per row, not one independent style per cell.
        let mut row0: Vec<StyledCell> = Vec::new();
        for c in "user@host".chars() {
            row0.push(cell(&c.to_string(), Color::Idx(2)));
        }
        row0.push(cell(":", Color::Default));
        for c in "~/project".chars() {
            row0.push(cell(&c.to_string(), Color::Idx(4)));
        }
        while row0.len() < 80 {
            row0.push(cell(" ", Color::Default));
        }
        let mut screen: Vec<Vec<StyledCell>> = vec![row0];
        for _ in 1..24 {
            screen.push(vec![cell(" ", Color::Default); 80]);
        }

        let per_cell_bytes = serde_json::to_string(&screen).unwrap().len();
        let runs: Vec<_> = screen.iter().map(|r| collapse_runs(r)).collect();
        let run_bytes = serde_json::to_string(&runs).unwrap().len();

        assert!(
            run_bytes * 10 < per_cell_bytes,
            "expected at least a 10x reduction, got {per_cell_bytes} -> {run_bytes}"
        );
    }

    #[test]
    fn a_shell_the_person_already_chose_is_never_second_guessed() {
        assert_eq!(
            shell_or_default(Some("/usr/bin/fish".into())),
            "/usr/bin/fish"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_with_no_shell_set_falls_back_to_sh() {
        assert_eq!(shell_or_default(None), "sh");
    }

    /// Runs the prefill rather than asserting its spelling: the failure this
    /// guards is a prompt naming a binary the machine does not have.
    #[cfg(windows)]
    #[test]
    fn the_windows_prefill_is_powershell_and_it_is_really_there() {
        let shell = shell_or_default(None);
        assert_eq!(shell, "powershell.exe");
        // `42` cannot appear in an echo of the question (PRINCIPLES §4).
        let out = std::process::Command::new(&shell)
            .args(["-NoProfile", "-Command", "Write-Output (6*7)"])
            .output()
            .expect("the prefilled shell must be runnable, not merely plausible");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "42");
    }
}
