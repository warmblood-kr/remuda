//! `remuda` — a pty manager you can attach to, and script.
//!
//! Bare `remuda` opens the herd. Four verbs survive being typed at a shell, and
//! a verb earns one only if it needs a terminal, must survive the shell's own
//! quoting, or renders for a human in a way the Lua image cannot:
//!
//! ```text
//!   remuda                        the TUI — the herd, and a session to ride
//!   remuda run [-n name] <argv…>  start a program and ride it, in one act
//!   remuda attach <name>          hand this terminal over; Ctrl-\ detaches
//!   remuda ls                     list this node's sessions
//!   remuda send <name> <text>     deliver one instruction, body and Enter
//! ```
//!
//! Everything else — `new`, `close`, `capture`, `insert`, `key`, `click` — lives
//! in the Lua image, reached by `-e`, `remuda lua <file>`, or `remuda mcp`. The
//! daemon starts itself on first use. `-s` names one. See `USAGE`.

use remuda_core::protocol::{Request, Response};
use remuda_native::client::Left;
use remuda_native::{daemon, dist, terminal_size};
use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (server, rest) = split_server_flag(&args);
    let argv: Vec<&str> = rest.iter().map(String::as_str).collect();
    let path = daemon::socket_path(server);

    announce_update(&argv);

    match argv.as_slice() {
        // The whole ask: typing the program's name opens the herd. Only when
        // both ends are a terminal — this also runs in CI and in pipes, where
        // the usage text is the useful answer and a TUI is a hang.
        [] if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() => {
            with_daemon(server, &path, |path| {
                match remuda_native::tui::run(path, server) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => fail(format!("tui: {e}")),
                }
            })
        }

        [] | ["help"] | ["-h"] | ["--help"] => {
            eprint!("{}", USAGE);
            ExitCode::SUCCESS
        }

        ["--version"] | ["-V"] | ["version"] => {
            println!("remuda {}", dist::VERSION);
            ExitCode::SUCCESS
        }

        // No daemon involved: this replaces the binary, it does not talk to one.
        ["upgrade", rest @ ..] => run_upgrade(rest),

        // Not part of the user-facing set: this is what the auto-start spawns.
        ["daemon"] => match daemon::serve(&path) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(format!("daemon: {e}")),
        },

        ["ls"] => with_daemon(server, &path, list_sessions),

        ["run", rest @ ..] => run_session(server, &path, rest),

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

        ["attach", name] => with_daemon(server, &path, |path| ride(path, name)),

        ["lua", script] => with_daemon(server, &path, |path| {
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

  remuda                        open the herd (a terminal is required)
  remuda run [-n name] <argv…>  start a program and ride it, in one act
  remuda attach <name>          hand this terminal over; Ctrl-\\ detaches
  remuda ls                     list sessions
  remuda send <name> <text>     deliver one instruction (body + Enter)

  remuda lua <script.lua>       run a Lua script in the daemon's living image
  remuda -e <code>              evaluate one chunk in that same image
  remuda repl                   the same image, a line at a time
  remuda mcp                    serve the image as an MCP tool on stdin/stdout
  remuda upgrade [--channel C]  re-run the installer on stable or nightly
  remuda --version              the version this binary was built with

Four verbs, not ten. A verb is here only if it needs a terminal, must survive
the shell's own quoting, or renders for a human in a way the image cannot.
`new`, `close`, `capture`, `insert`, `key` and `click` are all still there —
in Lua, where they cost no front page:

  remuda -e 'remuda.close(\"build\")'

Installs follow a channel — `stable` (release tags) or `nightly` (every commit
on main) — recorded at $XDG_DATA_HOME/remuda/channel by the install script.
Every command checks for a newer one at most once a day, in a detached child
that no command waits for. REMUDA_NO_UPDATE_CHECK=1 turns it off.

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

/// `run [-n name] <argv…>`: create a session and ride it, in one act. argv
/// comes first, so no leading positional can eat the program name — which is
/// exactly what made `new claude-code` start a bare shell called "claude-code".
fn run_session(server: &str, path: &Path, args: &[&str]) -> ExitCode {
    let (name, argv) = match args {
        ["-n", name, rest @ ..] => (Some((*name).to_string()), rest),
        rest => (None, rest),
    };
    if let Some(refusal) = lua_script_refusal(argv) {
        return fail(refusal);
    }
    let command: Vec<String> = argv.iter().map(|a| (*a).to_string()).collect();
    with_daemon(server, path, |path| {
        let request = Request::New {
            name: name.clone(),
            command: command.clone(),
            size: terminal_size(),
        };
        match remuda_native::client::request(path, &request) {
            Ok(Response::Value(made)) => ride(path, &made),
            other => fail(describe(other)),
        }
    })
}

/// `run` used to mean "run a Lua script". Refuse, do not fall back: executing a
/// `.lua` file as a program fails with a format error anyway, and this message
/// is strictly better than that. Drop the arm after one release.
fn lua_script_refusal(argv: &[&str]) -> Option<String> {
    let [only] = argv else { return None };
    (only.ends_with(".lua") && Path::new(only).is_file())
        .then(|| format!("`run` executes a program. For a Lua script: remuda lua {only}"))
}

/// Hand the terminal over, then say which way it came back. Both exits used to
/// be silent and identical, and a second `exit` then went to the real login
/// shell — the incident this whole change is named after.
fn ride(path: &Path, name: &str) -> ExitCode {
    match remuda_native::client::attach(path, name) {
        Ok(Left::Detached) => {
            eprintln!(
                "remuda: detached from {name} — still running, `remuda attach {name}` to go back"
            );
            ExitCode::SUCCESS
        }
        Ok(Left::Exited) => {
            eprintln!("remuda: {name} exited — kept as dead, its last screen is in `remuda ls`");
            eprintln!("remuda: you are back in your own shell");
            ExitCode::SUCCESS
        }
        Err(e) => fail(format!("attach: {e}")),
    }
}

/// One stderr line when a newer version is out. Silent on `daemon` — its stderr
/// is the client's only diagnostic when start-up fails — and on `mcp`, whose
/// streams belong to whatever spawned it.
fn announce_update(argv: &[&str]) {
    if !matches!(argv, ["daemon"] | ["mcp"]) {
        if let Some(notice) = dist::update_notice() {
            eprintln!("{notice}");
        }
    }
}

/// Split out of `main` for the same reason `list_sessions` was: clippy's line
/// budget. This one talks to no daemon — it replaces this very binary.
fn run_upgrade(args: &[&str]) -> ExitCode {
    match upgrade_channel(args).and_then(dist::upgrade) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(e),
    }
}

/// `--channel <name>` or nothing, in which case the installed channel file
/// decides. An unknown flag is refused rather than ignored.
fn upgrade_channel<'a>(args: &[&'a str]) -> Result<Option<&'a str>, String> {
    match args {
        [] => Ok(None),
        ["--channel", name] if dist::is_channel(name) => Ok(Some(name)),
        ["--channel", name] => Err(format!("unknown channel {name:?} — stable or nightly")),
        _ => Err("usage: remuda upgrade [--channel stable|nightly]".into()),
    }
}

/// Pull a leading `-s <server>` off argv; `"default"` when absent. Only the
/// *leading* position counts — in `remuda run -s x` the `-s` belongs to the
/// spawned command's argv, not to us.
fn split_server_flag(args: &[String]) -> (&str, &[String]) {
    match args {
        [flag, server, rest @ ..] if flag == "-s" => (server.as_str(), rest),
        _ => ("default", args),
    }
}

/// Run `f`, starting the named daemon first if nothing is listening yet.
fn with_daemon(server: &str, path: &Path, f: impl Fn(&Path) -> ExitCode) -> ExitCode {
    if remuda_native::ipc::connect(path).is_err() {
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
        if remuda_native::ipc::connect(path).is_ok() {
            // "remuda should always come up in daemon mode" — it already did.
            // What was missing was the line saying so.
            eprintln!("remuda: started a daemon for {server:?}");
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
                let held = if s.attached { "attached" } else { "" };
                println!(
                    "{:<20} {:>4}x{:<4} {:<5} idle {:<5} {held}",
                    s.name,
                    s.size.cols(),
                    s.size.rows(),
                    state,
                    format!("{}s", s.idle.as_secs()),
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

/// A line-at-a-time REPL against the image. `rustyline` because arrow keys
/// printing `^[[A` is what a person hits first; it also reads a pipe as lines,
/// so `printf … | remuda repl` keeps working. Ctrl-C cancels, Ctrl-D exits.
fn repl(path: &Path) -> ExitCode {
    let mut editor = match rustyline::DefaultEditor::new() {
        Ok(editor) => editor,
        Err(e) => return fail(format!("repl: {e}")),
    };
    let history = history_path();
    // A missing or unwritable state dir loses history, not the REPL.
    if let Some(file) = &history {
        let _ = editor.load_history(file);
    }
    loop {
        match editor.readline("> ") {
            Ok(line) => {
                let code = line.trim();
                if code.is_empty() {
                    continue;
                }
                let _ = editor.add_history_entry(code);
                eval_line(path, code);
            }
            // Ctrl-C abandons the half-typed line; the image keeps its state,
            // which is the whole reason not to exit here.
            Err(rustyline::error::ReadlineError::Interrupted) => continue,
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(e) => return fail(format!("repl: {e}")),
        }
    }
    if let Some(file) = &history {
        let _ = std::fs::create_dir_all(file.parent().unwrap_or(file));
        let _ = editor.save_history(file);
    }
    ExitCode::SUCCESS
}

/// One REPL line. An error prints and the loop continues — a typo must not
/// discard accumulated state.
fn eval_line(path: &Path, code: &str) {
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

/// Where REPL history lives, by the XDG state convention. `None` when `$HOME`
/// is unset too — nowhere to put it is not a reason to refuse to start.
fn history_path() -> Option<std::path::PathBuf> {
    let state = match std::env::var_os("XDG_STATE_HOME") {
        Some(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => std::path::PathBuf::from(std::env::var_os("HOME")?).join(".local/state"),
    };
    Some(state.join("remuda").join("repl-history"))
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
