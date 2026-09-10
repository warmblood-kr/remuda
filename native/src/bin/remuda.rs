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
//!   remuda -e <code>              evaluate Lua in the daemon's living image
//!   remuda repl                   the same image, a line at a time
//!   remuda mcp                    serve those as MCP tools on stdin/stdout
//!   remuda daemon                 run the daemon in the foreground
//!   remuda -s <server> …          talk to a *named* daemon instead of "default"
//! ```
//!
//! `mcp` is the one line a person does not type: it lets the program running
//! *inside* a session reach the manager holding it. The daemon starts itself on
//! first use. `-s` names one of several daemons; `REMUDA_RUNTIME_DIR` the tree.

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

        // `emacsclient -e` for this runtime: the code runs in the daemon's
        // long-lived image, so what it defines is still there next time.
        ["-e", code] => with_daemon(server, &path, |path| eval_once(path, code)),

        ["repl"] => with_daemon(server, &path, repl),

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
  remuda -e <code>              evaluate Lua in the daemon's living image
  remuda repl                   the same image, a line at a time
  remuda mcp                    serve them as MCP tools on stdin/stdout

The daemon holds one Lua interpreter for its whole life, and `-e`, `repl` and
a script are three doors into it. State persists between them:

  $ remuda -e \"fleet = {}\"
  $ remuda -e \"#fleet\"
  0

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

/// Pull a leading `-s <server>` off argv; `"default"` when absent. Only the
/// *leading* position counts — in `remuda new -s x` the `-s` belongs to the
/// spawned command's argv, not to us.
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

/// Spawn ourselves as the daemon and wait for the socket to answer. Wait on a
/// successful *connect*, not on the file existing, and pass `-s <server>`
/// through — a bare `remuda daemon` re-derives `"default"` and never matches.
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

/// Evaluate one chunk in the daemon's image and print what it came to. Nothing
/// is printed when it returned nothing, so `-e "x = 1"` is silent.
fn eval_once(path: &Path, code: &str) -> ExitCode {
    match remuda_native::client::request(
        path,
        &Request::Eval {
            code: code.to_string(),
            name: None,
        },
    ) {
        Ok(Response::Value(value)) => {
            if !value.is_empty() {
                println!("{value}");
            }
            ExitCode::SUCCESS
        }
        other => fail(describe(other)),
    }
}

/// A line-at-a-time REPL against the image. Deliberately not a readline — no
/// history, completion or raw mode — so a piped heredoc works too. An error
/// prints and the loop continues; a typo must not discard accumulated state.
fn repl(path: &Path) -> ExitCode {
    use std::io::Write;
    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        print!("> ");
        let _ = std::io::stdout().flush();
        line.clear();
        match stdin.read_line(&mut line) {
            Ok(0) => return ExitCode::SUCCESS,
            Ok(_) => {}
            Err(e) => return fail(e),
        }
        let code = line.trim();
        if code.is_empty() {
            continue;
        }
        match remuda_native::client::request(
            path,
            &Request::Eval {
                code: code.to_string(),
                name: None,
            },
        ) {
            Ok(Response::Value(value)) => {
                if !value.is_empty() {
                    println!("{value}");
                }
            }
            other => eprintln!("remuda: {}", describe(other)),
        }
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
