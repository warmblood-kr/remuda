//! `remuda` — a pty manager you can attach to, and script.
//!
//! ```text
//!   remuda ls                     list this node's sessions
//!   remuda new <name> [-- argv…]  start one (default: your $SHELL)
//!   remuda attach <name>          hand this terminal over; Ctrl-\ detaches
//!   remuda send <name> <text>     deliver one instruction, body and Enter
//!   remuda capture <name>         print the screen as text
//!   remuda close <name>           end a session (live or already self-exited)
//!   remuda run <script.lua>       run a script with those bound as functions
//!   remuda mcp                    serve those as MCP tools on stdin/stdout
//!   remuda daemon                 run the daemon in the foreground
//!   remuda -s <server> …          talk to a *named* daemon instead of "default"
//! ```
//!
//! `mcp` is the one line here a person does not type. It exists so the program
//! running *inside* a session can reach the manager that holds it — point a
//! client's server config at `remuda mcp` and the four operations show up as
//! tools.
//!
//! The daemon starts itself on first use, so none of the above needs a setup
//! step. That is deliberate: 정수님 asked for this to be seamless, and a tool
//! that makes you start a server before it works is not.
//!
//! `-s` is the second axis of the same idea (step 006): several daemons can
//! coexist on one node the way several `claude` configs coexist under
//! different home directories — this picks *which* one by name, under the
//! same runtime directory. `REMUDA_RUNTIME_DIR` is the other axis (a whole
//! different runtime), and the two compose: `daemon::socket_path` already
//! took a server name from day one, only the CLI had it hardcoded to
//! `"default"`.

use remuda_core::protocol::{Request, Response};
use remuda_native::{daemon, terminal_size};
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (server, rest) = split_server_flag(&args);
    let argv: Vec<&str> = rest.iter().map(String::as_str).collect();
    let path = daemon::socket_path(server);

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

        ["ls"] => with_daemon(server, &path, list_sessions),

        ["new", name, rest @ ..] => {
            let command: Vec<String> = rest
                .iter()
                .skip_while(|a| **a == "--")
                .map(|s| s.to_string())
                .collect();
            with_daemon(server, &path, |path| {
                let request = Request::New {
                    name: name.to_string(),
                    command: command.clone(),
                    size: terminal_size(),
                };
                simple_request(path, request)
            })
        }

        ["send", name, text @ ..] => {
            let text = text.join(" ");
            with_daemon(server, &path, |path| {
                let request = Request::SendLine {
                    name: name.to_string(),
                    text: text.clone(),
                };
                simple_request(path, request)
            })
        }

        ["capture", name] => with_daemon(server, &path, |path| {
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

        ["attach", name] => {
            with_daemon(server, &path, |path| {
                match remuda_native::client::attach(path, name) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => fail(format!("attach: {e}")),
                }
            })
        }

        ["close", name] => with_daemon(server, &path, |path| {
            simple_request(
                path,
                Request::Close {
                    name: name.to_string(),
                },
            )
        }),

        ["run", script] => with_daemon(server, &path, |path| {
            match remuda_native::script::run(path, Path::new(script)) {
                Ok(()) => ExitCode::SUCCESS,
                // Lua's own message, which already carries the file, the line
                // and a traceback. Reformatting it would only lose the line.
                Err(e) => fail(e),
            }
        }),

        // Speaks MCP on stdin/stdout, so the thing running *inside* a session
        // can reach the manager. Not meant to be typed by hand — a client
        // spawns it and owns both pipes.
        ["mcp"] => with_daemon(server, &path, |path| {
            match remuda_native::mcp::serve(path) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => fail(format!("mcp: {e}")),
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
  remuda close <name>           end a session (live, or already self-exited)
  remuda run <script.lua>       run a script; the above are bound as functions
  remuda mcp                    serve them as MCP tools on stdin/stdout

  remuda -s <server> <command…>  talk to a named daemon instead of \"default\" —
                                  several can coexist on one node, each with its
                                  own sessions, the way several claude configs
                                  coexist under different home directories.

A session that dies on its own is not removed automatically — its last screen
is exactly the evidence for why it died, so `ls` keeps showing it as `dead`
until something calls `close` on it. `close` also refuses on an attached
session, the same way `send` does, rather than disconnecting a human.

In a script they live on one table, and a refusal is raised, not returned:

  remuda.new(\"build\", {\"make\", \"-j4\"})
  while not remuda.capture(\"build\"):find(\"$ \") do remuda.sleep(0.2) end
  remuda.close(\"build\")

`mcp` is for a program running inside a session to reach the manager holding
it — a client spawns it and owns both pipes, so there is nothing to type here.
";

/// Pull a leading `-s <server>` off argv, wherever `main` needs the daemon's
/// name before it can compute a socket path. Returns `"default"` when absent.
///
/// Only the *leading* position is recognised — `remuda new -s x` treats `-s`
/// as the session's own argv, not ours, because after `new <name>` everything
/// is already the spawned command's business (see the `--` convention there).
fn split_server_flag(args: &[String]) -> (&str, &[String]) {
    match args {
        [flag, server, rest @ ..] if flag == "-s" => (server.as_str(), rest),
        _ => ("default", args),
    }
}

/// Run `f`, starting the named daemon first if nothing is listening yet.
fn with_daemon(server: &str, path: &Path, f: impl Fn(&Path) -> ExitCode) -> ExitCode {
    if std::os::unix::net::UnixStream::connect(path).is_err() {
        if let Err(e) = start_daemon(server, path) {
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
///
/// `-s <server>` is passed through explicitly. Without it the spawned child
/// re-derives its own path from bare `remuda daemon`, which always means
/// `"default"` — so `remuda -s alt ls` would wait forever on `alt.sock` while
/// a daemon came up on `default.sock` instead. Measured, not foreseen
/// (`steps/006-lifetime.md` Actual).
fn start_daemon(server: &str, path: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find own binary: {e}"))?;
    // stderr is captured rather than discarded. The first version discarded it,
    // and when the daemon failed for a perfectly nameable reason ("path must be
    // shorter than SUN_LEN") the client reported a timeout instead — a wall
    // claim that named the wrong wall, and the single most expensive kind of
    // wrong message to debug.
    let mut child = std::process::Command::new(exe)
        .args(["-s", server, "daemon"])
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

/// The three-line list a person reads; also what `remuda ls` was inlining
/// before `main` grew past clippy's line budget with `close` added.
fn list_sessions(path: &Path) -> ExitCode {
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
}

/// The shape every request that answers with a bare `Ok` shares: `new`,
/// `send`, and `close`. Only `capture` (returns a screen) and `attach`
/// (takes the connection over) need their own arm.
fn simple_request(path: &Path, request: Request) -> ExitCode {
    match remuda_native::client::request(path, &request) {
        Ok(Response::Ok) => ExitCode::SUCCESS,
        other => fail(describe(other)),
    }
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
