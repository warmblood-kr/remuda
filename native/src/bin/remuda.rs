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

#[path = "remuda/codex_tui.rs"]
mod codex_tui;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (server, rest) = split_server_flag(&args);
    let argv: Vec<&str> = rest.iter().map(String::as_str).collect();
    let path = daemon::socket_path(server);

    announce_update(&argv);
    // Asked once, before anything dispatches. A daemon that is already up was
    // started by some other binary, and this is the cheapest moment to ask which.
    let skew = version_skew(&argv, &path);
    if let Some(notice) = &skew {
        eprintln!("remuda: {notice}");
    }

    match argv.as_slice() {
        // The whole ask: typing the program's name opens the herd. Only when
        // both ends are a terminal — this also runs in CI and in pipes, where
        // the usage text is the useful answer and a TUI is a hang.
        [] if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() => {
            with_daemon(server, &path, |path| {
                // The line above went to stderr, which the alternate screen is
                // about to hide. The footer is where a TUI user can read it.
                match remuda_native::tui::run(path, server, skew.clone()) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => fail(format!("tui: {e}")),
                }
            })
        }

        [] | ["help"] | ["-h"] | ["--help"] => help_command(),

        ["--version"] | ["-V"] | ["version"] => {
            println!("remuda {}", dist::BUILD_VERSION);
            ExitCode::SUCCESS
        }

        ["_codex_tui", rest @ ..] => codex_tui::run(rest),

        // No daemon involved: this replaces the binary, it does not talk to one.
        ["upgrade", rest @ ..] => run_upgrade(rest),

        // Not part of the user-facing set: this is what the auto-start spawns.
        ["daemon"] => match daemon::serve(&path) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(format!("daemon: {e}")),
        },

        // Deliberately NOT behind `with_daemon`: the daemon this stops is often
        // exactly the one that cannot be talked to, and starting one to stop it
        // is not a thing to do.
        ["stop", rest @ ..] => stop(server, &path, rest),

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

        ["exec", name] => with_daemon(server, &path, |path| exec_command(path, name)),

        [command, rest @ ..] if remuda_native::packages::has_subcommand(command) => {
            extension_command(server, &path, command, rest)
        }

        // `emacsclient -e` for this runtime: the code runs in the daemon's
        // long-lived image, so what it defines is still there next time.
        ["-e", code] => with_daemon(server, &path, |path| eval_once(path, code)),

        ["mod", "install", rest @ ..] => mod_install_command(rest),
        ["mod", "list", rest @ ..] => mod_list_command(rest),
        ["mod", "info", rest @ ..] => mod_info_command(rest),
        ["mod", "test", rest @ ..] => mod_test_command(rest),
        ["mod", "update", rest @ ..] => mod_update_command(rest),
        ["mod", "remove", rest @ ..] => mod_remove_command(rest),

        ["doc", rest @ ..] => with_daemon(server, &path, |path| doc_command(path, rest)),

        ["repl"] => with_daemon(server, &path, repl),

        // Speaks MCP on stdin/stdout, so the thing running *inside* a session
        // can reach the manager. Not meant to be typed by hand — a client
        // spawns it and owns both pipes.
        ["mcp"] => with_daemon(server, &path, |path| {
            // An extension may put an unforgeable per-session capability in
            // this child process's environment. MCP JSON never supplies caller identity.
            let capability = std::env::var("REMUDA_SESSION_CAPABILITY")
                .ok()
                .filter(|token| !token.is_empty());
            match remuda_native::mcp::serve_with_capability(path, capability.as_deref()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => fail(format!("mcp: {e}")),
            }
        }),

        // Test-only plumbing for `remuda.process`, deliberately absent from
        // `USAGE`: no daemon involved, just a cross-platform, dependency-free
        // way to print N lines with a controllable pace.
        ["_print_lines", n, delay_ms] => print_lines(n, delay_ms),

        _ => {
            eprint!("{}", USAGE);
            ExitCode::FAILURE
        }
    }
}

#[allow(dead_code)]
const DETAILED_USAGE: &str = "\
remuda — a pty manager you can attach to

  remuda                        open the herd (a terminal is required)
  remuda run [-n name] <argv…>  start a program and ride it, in one act
  remuda attach <name>          hand this terminal over; Ctrl-\\ detaches
  remuda ls                     list sessions
  remuda send <name> <text>     deliver one instruction (body + Enter)

  remuda lua <script.lua>       run a Lua script in the daemon's living image
  remuda exec <name>            run an installed Lua mod
  remuda mod install OWNER/REPO [--ref REF] [--force]
                                  install a Lua mod from GitHub
  remuda mod list [--format F]    list installed mods
  remuda mod info NAME            show a mod manifest
  remuda mod test PATH            validate a local mod checkout
  remuda mod update NAME          update one installed mod
  remuda mod update --all         update all installed mods
  remuda mod remove NAME          remove one installed mod
  remuda doc [--format F]        print live Lua documentation (rst by default)
  remuda -e <code>              evaluate one chunk in that same image
  remuda repl                   the same image, a line at a time
  remuda mcp                    serve the image as an MCP tool on stdin/stdout
  remuda upgrade [--channel C]  re-run the installer on stable or nightly
  remuda stop [-f]              stop the daemon; the next command starts a fresh
                                  one. Its sessions and Lua image die with it,
                                  so a live herd is named and confirmed first.
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

A session whose program exits closes itself and leaves the list. Its last
screen is the evidence for why it died, so set REMUDA_KEEP_EXITED=1 in the
daemon's environment to keep it listed as `dead` until something calls `close`.
`close` refuses on an attached session, the same way `send` does, rather than
disconnecting a human.

In a script they live on one table, and a refusal is raised, not returned:

  remuda.new(\"build\", {\"make\", \"-j4\"})
  while not remuda.capture(\"build\"):find(\"$ \") do remuda.sleep(0.2) end
  remuda.close(\"build\")

`mcp` is for a program running inside a session to reach the manager holding
it — a client spawns it and owns both pipes, so there is nothing to type here.
";

const USAGE: &str = "\
remuda — terminal orchestration for coding agents

  remuda                         open the session screen
  remuda run [-n NAME] COMMAND   start and enter a session
  remuda attach NAME             enter a session; Ctrl-\\ detaches
  remuda ls | send NAME TEXT     inspect or message sessions
  remuda stop [-f]               stop the daemon (sessions are lost)

  remuda mod install OWNER/REPO  install a mod from GitHub
  remuda mod list | info NAME    inspect installed mods
  remuda mod update NAME|--all   update a mod
  remuda mod remove NAME         remove a mod

  remuda doc | repl | -e CODE    use the persistent Lua runtime
  remuda --version

Run `remuda mod list` for installed mods and `remuda doc` for the live Lua API.
";

fn help_command() -> ExitCode {
    eprint!("{USAGE}");
    match remuda_native::packages::manifests() {
        Ok(mods) => {
            let commands: Vec<_> = mods
                .into_iter()
                .filter_map(|mod_spec| mod_spec.command)
                .collect();
            if !commands.is_empty() {
                eprintln!("Installed mod commands:");
                for command in commands {
                    eprintln!("  remuda {command} [--headless]");
                }
            }
            ExitCode::SUCCESS
        }
        Err(error) => fail(error),
    }
}

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
            cwd: None,
            env: None,
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
            eprintln!("remuda: {name} exited — {}", fate(path, name));
            eprintln!("remuda: you are back in your own shell");
            ExitCode::SUCCESS
        }
        Err(e) => fail(format!("attach: {e}")),
    }
}

/// Whether the session survived its own exit — asked, not assumed. Only the
/// daemon knows whether it was started with REMUDA_KEEP_EXITED, and `List` is
/// where an exited session is dropped, so this reads the answer it just made.
fn fate(path: &Path, name: &str) -> String {
    match remuda_native::client::request(path, &Request::List) {
        Ok(Response::Sessions(sessions)) if sessions.iter().any(|s| s.name == name) => {
            "kept as dead, its last screen is in `remuda ls`".into()
        }
        _ => "the session is gone; REMUDA_KEEP_EXITED=1 in the daemon keeps it".into(),
    }
}

/// `stop [-f]`: stop this server's daemon. `remuda upgrade` replaces the
/// binary but cannot touch a daemon already running — this is the verb that
/// closes that gap.
fn stop(server: &str, path: &Path, args: &[&str]) -> ExitCode {
    let force = match args {
        [] => false,
        ["-f"] | ["--force"] => true,
        _ => return fail("usage: remuda stop [-f]"),
    };
    if remuda_native::ipc::connect(path).is_err() {
        eprintln!("remuda: no daemon running for {server:?} — the next command starts one");
        return ExitCode::SUCCESS;
    }
    if !force {
        if let Err(refusal) = confirm_losses(path) {
            return fail(refusal);
        }
    }
    match stop_daemon(path) {
        Ok(()) => {
            eprintln!(
                "remuda: stopped the daemon for {server:?} — the next command starts a fresh one"
            );
            ExitCode::SUCCESS
        }
        Err(e) => fail(e),
    }
}

/// Name what dies before it dies. Killing the daemon takes every session's
/// process and scrollback and the whole Lua image with it, so an empty herd is
/// the only case that goes without asking.
fn confirm_losses(path: &Path) -> Result<(), String> {
    let live = match remuda_native::client::request(path, &Request::List) {
        Ok(Response::Sessions(sessions)) => sessions,
        // It cannot even list. That is itself the reason someone typed this.
        _ => return Ok(()),
    };
    if live.is_empty() {
        return Ok(());
    }
    // A dead session counts too: its last screen is the evidence for why it
    // died, which is the whole reason `close` does not happen automatically.
    let names: Vec<String> = live
        .iter()
        .map(|s| format!("{} ({})", s.name, if s.alive { "live" } else { "dead" }))
        .collect();
    eprintln!(
        "remuda: {} session(s) go with it: {}",
        names.len(),
        names.join(", ")
    );
    eprintln!("remuda: their processes, their last screens and the Lua image are all lost.");
    if !std::io::stdin().is_terminal() {
        return Err("nothing to ask on — `remuda stop -f` if that is what you want".into());
    }
    eprint!("remuda: type y to go ahead: ");
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| e.to_string())?;
    match answer.trim() {
        "y" | "Y" => Ok(()),
        _ => Err("left it running".into()),
    }
}

/// `Shutdown` is the door. A daemon built before that variant existed refuses
/// it, and its Lua image is the only lever those already-deployed ones leave —
/// `os.exit` there ends the same process. Drop the fallback after one release.
fn stop_daemon(path: &Path) -> Result<(), String> {
    let asked = remuda_native::client::request(path, &Request::Shutdown);
    if !matches!(asked, Ok(Response::Ok)) {
        let _ = remuda_native::client::request(
            path,
            &Request::Eval {
                code: "os.exit(0)".into(),
                name: None,
            },
        );
    }
    // The socket file outlives the process on unix, so "gone" is a connect that
    // is refused, never a path that disappeared. `ipc::listen` clears the file.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if remuda_native::ipc::connect(path).is_err() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Err(format!(
        "the daemon at {} is still answering — {}",
        path.display(),
        describe(asked)
    ))
}

/// One line when the running daemon is not this build. A warning, not a refusal:
/// most skews are harmless, and stranding someone mid-work behind a version
/// string is its own incident. `None` when nothing is listening — it will be us.
fn version_skew(argv: &[&str], path: &Path) -> Option<String> {
    if matches!(argv, ["daemon"] | ["mcp"] | ["stop", ..]) {
        return None;
    }
    remuda_native::ipc::connect(path).ok()?;
    skew_notice(remuda_native::client::request(path, &Request::Version))
}

/// What to say about a `Request::Version` outcome — split out of
/// `version_skew` so this decision is testable without a socket. See
/// steps/024.
fn skew_notice(response: std::io::Result<Response>) -> Option<String> {
    let theirs = match response {
        Ok(Response::Value(theirs)) if theirs == dist::BUILD_VERSION => return None,
        Ok(Response::Value(theirs)) => theirs,
        // Cure first, same reason as the message below: this can reach a TUI
        // footer that crops the tail at the terminal's width.
        Err(_) => {
            return Some(format!(
                "the daemon could not confirm its version — `remuda stop` \
                 replaces it if something still seems off. The check itself \
                 failed partway through; this command is {}",
                dist::BUILD_VERSION
            ))
        }
        // Older than the handshake itself. Not knowing is itself the answer.
        Ok(_) => "from a build that predates this handshake".into(),
    };
    // Cure first, and no "remuda:" prefix — the caller adds one, and this also
    // goes to a TUI footer that crops the tail at the terminal's width.
    Some(format!(
        "the daemon is not this build — `remuda stop` replaces it, and its \
         sessions and Lua image go with it. It is {theirs}; this command is {}",
        dist::BUILD_VERSION
    ))
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
    match remuda_native::ipc::connect(path) {
        Ok(_) => {}
        Err(error) if remuda_native::ipc::may_start_daemon(path, &error) => {
            if let Err(e) = start_daemon(server, path) {
                return fail(e);
            }
        }
        Err(error) => {
            return fail(format!(
                "cannot connect to remuda daemon at {}: {error}; refusing to start a second daemon",
                path.display()
            ));
        }
    }
    f(path)
}

/// Delegates to the lib crate's installed package resolver — the
/// `remuda.exec()` Lua binding (script.rs) resolves through the same resolver,
/// so there is exactly one list, not two.
/// Run an installed mod's entry file in the daemon's image.
fn exec_command(path: &Path, name: &str) -> ExitCode {
    match remuda_native::packages::resolve(name) {
        Err(error) => fail(error),
        Ok(None) => fail(format!("no such package: {name}")),
        Ok(Some(package)) => {
            match remuda_native::script::run_source(path, &package.chunk_name, &package.source) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => fail(e),
            }
        }
    }
}

/// Dispatch a manifest-declared mod command. With no arguments it starts the
/// mod and opens the regular Remuda screen; `--headless` starts only the mod.
/// Other arguments belong entirely to the installed Lua mod.
fn extension_command(server: &str, path: &Path, command: &str, args: &[&str]) -> ExitCode {
    let package = match remuda_native::packages::subcommand(command) {
        Ok(Some(package)) => package,
        Ok(None) => return fail(format!("no installed mod provides command {command}")),
        Err(error) => return fail(error),
    };
    if args.is_empty() || args == ["--headless"] {
        let headless = args == ["--headless"];
        return with_daemon(server, path, |path| {
            let started = exec_command(path, &package);
            if started != ExitCode::SUCCESS
                || headless
                || !std::io::stdin().is_terminal()
                || !std::io::stdout().is_terminal()
            {
                return started;
            }
            match remuda_native::tui::run(path, server, None) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => fail(format!("tui: {error}")),
            }
        });
    }
    let arguments = args
        .iter()
        .map(|argument| serde_json::to_string(argument).expect("argument serializes"))
        .collect::<Vec<_>>()
        .join(", ");
    let code = format!(
        "return remuda._dispatch_extension_command({}, {{{arguments}}})",
        serde_json::to_string(command).expect("command serializes")
    );
    with_daemon(server, path, |path| eval_once(path, &code))
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

/// Render the live registry in the requested documentation format.
fn doc_command(path: &Path, args: &[&str]) -> ExitCode {
    let format = match args {
        [] => "rst",
        ["--format", format @ ("rst" | "markdown" | "json")] => format,
        _ => return fail("usage: remuda doc [--format rst|markdown|json]"),
    };
    let encoded = serde_json::to_string(format).expect("format names are valid strings");
    match remuda_native::client::request(
        path,
        &Request::Eval {
            code: format!("return remuda._registry_dump({encoded})"),
            name: None,
        },
    ) {
        Ok(Response::Value(value)) => {
            println!("{value}");
            ExitCode::SUCCESS
        }
        other => fail(describe(other)),
    }
}

fn mod_install_command(args: &[&str]) -> ExitCode {
    let Some(repository) = args.first() else {
        return fail("usage: remuda mod install OWNER/REPO [--ref REF] [--force]");
    };
    let mut reference = None;
    let mut force = false;
    let mut index = 1;
    while index < args.len() {
        match args[index] {
            "--force" if !force => force = true,
            "--ref" if reference.is_none() && index + 1 < args.len() => {
                index += 1;
                reference = Some(args[index]);
            }
            _ => return fail("usage: remuda mod install OWNER/REPO [--ref REF] [--force]"),
        }
        index += 1;
    }
    match remuda_native::packages::install(repository, reference, force) {
        Ok(report) => {
            println!(
                "installed mod {} {} from {} at {}",
                report.manifest.name, report.manifest.version, report.repository, report.commit
            );
            println!(
                "reload with `remuda exec {}` or restart the daemon; installation does not mutate a live Lua image",
                report.manifest.name
            );
            ExitCode::SUCCESS
        }
        Err(error) => fail(error),
    }
}

fn mod_list_command(args: &[&str]) -> ExitCode {
    let format = match args {
        [] => "rst",
        ["--format", format @ ("rst" | "markdown" | "json")] => format,
        _ => return fail("usage: remuda mod list [--format rst|markdown|json]"),
    };
    let manifests = match remuda_native::packages::manifests() {
        Ok(manifests) => manifests,
        Err(error) => return fail(error),
    };
    match format {
        "json" => {
            let mods: Vec<_> = manifests
                .iter()
                .map(|entry| {
                    serde_json::json!({
                        "name": entry.name,
                        "version": entry.version,
                        "api": entry.api,
                        "entry": entry.entry,
                        "command": entry.command,
                        "source": entry.source,
                        "status": entry.status,
                    })
                })
                .collect();
            println!("{}", serde_json::json!({ "mods": mods }));
        }
        "markdown" => {
            println!("# Remuda mods\n");
            for entry in manifests {
                println!(
                    "## `{}`\n\n- version: `{}`\n- api: `{}`\n- entry: `{}`\n- source: `{}`\n- status: `{}`\n",
                    entry.name, entry.version, entry.api, entry.entry, entry.source, entry.status
                );
            }
        }
        "rst" => {
            println!("Remuda mods\n===========\n");
            for entry in manifests {
                println!(
                    "{}\n{}\n\n* version: ``{}``\n* api: ``{}``\n* entry: ``{}``\n* source: ``{}``\n* status: ``{}``\n",
                    entry.name,
                    "-".repeat(entry.name.len()),
                    entry.version,
                    entry.api,
                    entry.entry,
                    entry.source,
                    entry.status
                );
            }
        }
        _ => unreachable!(),
    }
    ExitCode::SUCCESS
}

fn mod_info_command(args: &[&str]) -> ExitCode {
    let Some(name) = args.first() else {
        return fail("usage: remuda mod info NAME [--format rst|markdown|json]");
    };
    let format = match args.get(1..) {
        Some([]) => "rst",
        Some(["--format", format @ ("rst" | "markdown" | "json")]) => format,
        _ => return fail("usage: remuda mod info NAME [--format rst|markdown|json]"),
    };
    let manifest = match remuda_native::packages::manifest(name) {
        Ok(Some(manifest)) => manifest,
        Ok(None) => return fail(format!("no such mod: {name}")),
        Err(error) => return fail(error),
    };
    match format {
        "json" => println!(
            "{}",
            serde_json::json!({
                "name": manifest.name,
                "version": manifest.version,
                "api": manifest.api,
                "entry": manifest.entry,
                "command": manifest.command,
                "source": manifest.source,
                "status": manifest.status,
            })
        ),
        "markdown" => println!(
            "# `{}`\n\n- version: `{}`\n- api: `{}`\n- entry: `{}`\n- source: `{}`\n- status: `{}`",
            manifest.name, manifest.version, manifest.api, manifest.entry, manifest.source, manifest.status
        ),
        "rst" => println!(
            "{}\n{}\n\n* version: ``{}``\n* api: ``{}``\n* entry: ``{}``\n* source: ``{}``\n* status: ``{}``",
            manifest.name,
            "-".repeat(manifest.name.len()),
            manifest.version,
            manifest.api,
            manifest.entry,
            manifest.source,
            manifest.status
        ),
        _ => unreachable!(),
    }
    ExitCode::SUCCESS
}

fn mod_update_command(args: &[&str]) -> ExitCode {
    let reports = match args {
        ["--all"] => remuda_native::packages::update_all(),
        [name] => remuda_native::packages::update(name).map(|report| vec![report]),
        _ => return fail("usage: remuda mod update NAME|--all"),
    };
    match reports {
        Ok(reports) => {
            for report in reports {
                println!(
                    "updated mod {} {} from {} at {}",
                    report.manifest.name, report.manifest.version, report.repository, report.commit
                );
            }
            println!("reload with `remuda exec NAME` or restart the daemon");
            ExitCode::SUCCESS
        }
        Err(error) => fail(error),
    }
}

fn mod_remove_command(args: &[&str]) -> ExitCode {
    let [name] = args else {
        return fail("usage: remuda mod remove NAME");
    };
    match remuda_native::packages::remove(name) {
        Ok(report) => {
            println!(
                "removed mod {} from {}",
                report.manifest.name,
                report.path.display()
            );
            println!(
                "a running daemon keeps its loaded Lua definitions until restart; no session was stopped"
            );
            ExitCode::SUCCESS
        }
        Err(error) => fail(error),
    }
}

fn mod_test_command(args: &[&str]) -> ExitCode {
    let [path] = args else {
        return fail("usage: remuda mod test PATH");
    };
    match remuda_native::packages::test_path(Path::new(path)) {
        Ok(spec) => {
            println!(
                "valid mod {} {} ({})",
                spec.name,
                spec.version,
                remuda_native::packages::LUA_API_VERSION
            );
            ExitCode::SUCCESS
        }
        Err(error) => fail(error),
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

/// Prints `1..=n`, one per line, flushing after each and sleeping
/// `delay_ms` between them (0 = no delay).
// Cross-platform, dependency-free, so `remuda.process` tests can control
// pacing without a shell loop that behaves differently on Windows vs Unix.
// Test-only; deliberately absent from `USAGE`.
fn print_lines(n: &str, delay_ms: &str) -> ExitCode {
    let (Ok(n), Ok(delay)) = (n.parse::<u64>(), delay_ms.parse::<u64>()) else {
        return fail("usage: remuda _print_lines <n> <delay_ms>");
    };
    let mut out = std::io::stdout().lock();
    for i in 1..=n {
        use std::io::Write;
        if writeln!(out, "{i}").is_err() || out.flush().is_err() {
            break;
        }
        if delay > 0 {
            std::thread::sleep(std::time::Duration::from_millis(delay));
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [MEASURED] A request failure must not read as a confirmed match —
    /// that silently uses a possibly-mismatched daemon. See steps/024.
    #[test]
    fn a_failed_version_request_is_not_silently_no_skew() {
        let notice = skew_notice(Err(std::io::Error::other("boom")));
        assert!(
            notice.is_some(),
            "a transport failure on the version check must say something, \
             not silently read as a confirmed match"
        );
        assert!(notice.unwrap().contains("remuda stop"));
    }
}
