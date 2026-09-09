//! `remuda` — a pty manager you can attach to, and script.
//!
//! ```text
//!   remuda ls                     list this node's sessions
//!   remuda new <name> [-- argv…]  start one (default: your $SHELL)
//!   remuda attach <name>          hand this terminal over; Ctrl-\ detaches
//!   remuda send <name> <text>     deliver one instruction, body and Enter
//!   remuda capture <name>         print the screen as text
//!   remuda run <script.lua>       run a script with those bound as functions
//!   remuda daemon                 run the daemon in the foreground
//! ```
//!
//! The daemon starts itself on first use, so none of the above needs a setup
//! step. That is deliberate: 정수님 asked for this to be seamless, and a tool
//! that makes you start a server before it works is not.

use remuda_core::protocol::{Request, Response};
use remuda_native::{daemon, terminal_size};
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let path = daemon::socket_path("default");

    match argv.as_slice() {
        [] | ["help"] | ["-h"] | ["--help"] => {
            eprint!("{}", USAGE);
            ExitCode::SUCCESS
        }

        // Not part of the user-facing set: this is what the auto-start spawns.
        ["daemon"] => match daemon::serve(&path) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(format!("daemon: {e}")),
        },

        ["ls"] => with_daemon(&path, |path| {
            match remuda_native::client::request(path, &Request::List) {
                Ok(Response::Sessions(sessions)) if sessions.is_empty() => {
                    println!("no sessions");
                    ExitCode::SUCCESS
                }
                Ok(Response::Sessions(sessions)) => {
                    for s in sessions {
                        let state = if s.alive { "live" } else { "dead" };
                        println!(
                            "{:<20} {:>4}x{:<4} {:<5} idle {}s",
                            s.name,
                            s.size.cols(),
                            s.size.rows(),
                            state,
                            s.idle.as_secs()
                        );
                    }
                    ExitCode::SUCCESS
                }
                other => fail(describe(other)),
            }
        }),

        ["new", name, rest @ ..] => {
            let command: Vec<String> = rest
                .iter()
                .skip_while(|a| **a == "--")
                .map(|s| s.to_string())
                .collect();
            with_daemon(&path, |path| {
                let request = Request::New {
                    name: name.to_string(),
                    command: command.clone(),
                    size: terminal_size(),
                };
                match remuda_native::client::request(path, &request) {
                    Ok(Response::Ok) => ExitCode::SUCCESS,
                    other => fail(describe(other)),
                }
            })
        }

        ["send", name, text @ ..] => {
            let text = text.join(" ");
            with_daemon(&path, |path| {
                let request = Request::SendLine {
                    name: name.to_string(),
                    text: text.clone(),
                };
                match remuda_native::client::request(path, &request) {
                    Ok(Response::Ok) => ExitCode::SUCCESS,
                    other => fail(describe(other)),
                }
            })
        }

        ["capture", name] => with_daemon(&path, |path| {
            let request = Request::Capture {
                name: name.to_string(),
            };
            match remuda_native::client::request(path, &request) {
                Ok(Response::Screen(text)) => {
                    println!("{text}");
                    ExitCode::SUCCESS
                }
                other => fail(describe(other)),
            }
        }),

        ["attach", name] => with_daemon(&path, |path| {
            match remuda_native::client::attach(path, name) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => fail(format!("attach: {e}")),
            }
        }),

        ["run", script] => with_daemon(&path, |path| {
            match remuda_native::script::run(path, Path::new(script)) {
                Ok(()) => ExitCode::SUCCESS,
                // Lua's own message, which already carries the file, the line
                // and a traceback. Reformatting it would only lose the line.
                Err(e) => fail(e),
            }
        }),

        _ => {
            eprint!("{}", USAGE);
            ExitCode::FAILURE
        }
    }
}

const USAGE: &str = "\
remuda — a pty manager you can attach to

  remuda ls                     list sessions
  remuda new <name> [-- argv…]  start one (default: $SHELL)
  remuda attach <name>          hand this terminal over; Ctrl-\\ detaches
  remuda send <name> <text>     deliver one instruction (body + Enter)
  remuda capture <name>         print the screen as text (no terminal needed)
  remuda run <script.lua>       run a script; the above are bound as functions

In a script they live on one table, and a refusal is raised, not returned:

  remuda.new(\"build\", {\"make\", \"-j4\"})
  while not remuda.capture(\"build\"):find(\"$ \") do remuda.sleep(0.2) end
";

/// Run `f`, starting the daemon first if nothing is listening yet.
fn with_daemon(path: &Path, f: impl Fn(&Path) -> ExitCode) -> ExitCode {
    if std::os::unix::net::UnixStream::connect(path).is_err() {
        if let Err(e) = start_daemon(path) {
            return fail(e);
        }
    }
    f(path)
}

/// Spawn ourselves as the daemon and wait for the socket to answer.
///
/// Waiting for a *successful connect* rather than for the file to exist: the
/// path can be there while the listener is not yet bound, and a client that
/// raced past it would fail on its first real request instead of here.
fn start_daemon(path: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find own binary: {e}"))?;
    // stderr is captured rather than discarded. The first version discarded it,
    // and when the daemon failed for a perfectly nameable reason ("path must be
    // shorter than SUN_LEN") the client reported a timeout instead — a wall
    // claim that named the wrong wall, and the single most expensive kind of
    // wrong message to debug.
    let mut child = std::process::Command::new(exe)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start daemon: {e}"))?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Ok(());
        }
        // If it has already exited, waiting out the deadline only delays the
        // real message.
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let _ = child.kill();
    let said = child
        .wait_with_output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stderr).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "it printed nothing".into());
    Err(format!(
        "daemon did not come up at {} — {said}",
        path.display()
    ))
}

fn describe(response: std::io::Result<Response>) -> String {
    match response {
        Ok(Response::Error(reason)) => reason,
        Ok(other) => format!("unexpected response: {other:?}"),
        Err(e) => e.to_string(),
    }
}

fn fail(message: impl std::fmt::Display) -> ExitCode {
    eprintln!("remuda: {message}");
    ExitCode::FAILURE
}
