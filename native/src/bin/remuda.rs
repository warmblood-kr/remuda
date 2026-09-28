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
use std::fs;
use std::io::{IsTerminal, Write};
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
        ["attach", name, "--mouse=false"] => {
            with_daemon(server, &path, |path| ride_with_mouse(path, name, false))
        }
        ["attach", name, "--mouse=true"] => {
            with_daemon(server, &path, |path| ride_with_mouse(path, name, true))
        }
        ["attach", "--mouse=false", name] => {
            with_daemon(server, &path, |path| ride_with_mouse(path, name, false))
        }
        ["attach", "--mouse=true", name] => {
            with_daemon(server, &path, |path| ride_with_mouse(path, name, true))
        }

        ["lua", script] => with_daemon(server, &path, |path| {
            match remuda_native::script::run(path, Path::new(script)) {
                Ok(()) => ExitCode::SUCCESS,
                // Lua's own message, which already carries the file, the line
                // and a traceback. Reformatting it would only lose the line.
                Err(e) => fail(e),
            }
        }),

        ["exec", name] => with_daemon(server, &path, |path| exec_command(path, name)),

        ["cluster", rest @ ..] => cluster_command(rest),

        [command, rest @ ..] if remuda_native::packages::has_subcommand(command) => {
            extension_command(server, &path, command, rest)
        }

        // `emacsclient -e` for this runtime: the code runs in the daemon's
        // long-lived image, so what it defines is still there next time.
        ["-e", code] => with_daemon(server, &path, |path| eval_once(path, code)),

        ["mod", "install", rest @ ..] => mod_install_command(server, &path, rest),
        ["mod", "list", rest @ ..] => mod_list_command(rest),
        ["mod", "info", rest @ ..] => mod_info_command(rest),
        ["mod", "test", rest @ ..] => mod_test_command(rest),
        ["mod", "update", rest @ ..] => mod_update_command(server, &path, rest),
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

        // Not a known verb or mod command. A mod directory with no readable
        // manifest is named rather than answered with usage (#134).
        _ => match argv
            .first()
            .and_then(|word| remuda_native::packages::half_installed(word))
        {
            Some(message) => fail(message),
            None => {
                eprint!("{}", USAGE);
                ExitCode::FAILURE
            }
        },
    }
}

#[allow(dead_code)]
const DETAILED_USAGE: &str = "\
remuda — a pty manager you can attach to

  remuda                        open the herd (a terminal is required)
  remuda run [-n name] <argv…>  start a program and ride it, in one act
  remuda attach <name> [--mouse=false] hand this terminal over; Ctrl-\\ detaches (2 = attached elsewhere)
  remuda ls                     list sessions
  remuda send <name> <text>     deliver one instruction (body + Enter)

  remuda lua <script.lua>       run a Lua script in the daemon's living image
  remuda exec <name>            run an installed Lua mod
  remuda butler                 run the installed Butler mod
  remuda butler help             show Butler coordination commands
  remuda butler sessions         list Butler-managed agent sessions
  remuda butler launch KIND [N]  launch a claude or codex session
  remuda butler send FROM TO MSG queue a message for an agent
  remuda butler inbox NAME       drain an agent's queued messages
  remuda mod install OWNER/REPO [--ref REF] [--force] [--reload]
                                  install a Lua mod from GitHub
  remuda mod list [--format F]    list installed mods
  remuda mod info NAME            show a mod manifest
  remuda mod test PATH            validate a local mod checkout
  remuda mod update NAME [--reload] update one installed mod
  remuda mod update --all         update all installed mods
  remuda mod remove NAME          remove one installed mod
  remuda cluster                  show cluster status
  remuda cluster init             create this node's cluster identity
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
  remuda attach NAME             enter a session; Ctrl-\\ detaches (exit 2 if attached elsewhere)
                                 Ctrl-] toggles mouse; wheel scrolls history
                                 --mouse=false disables mouse handling (before or after NAME)
  remuda ls | send NAME TEXT     inspect or message sessions
  remuda stop [-f]               stop the daemon (sessions are lost)

  remuda mod install OWNER/REPO  install a mod from GitHub
  remuda mod list | info NAME    inspect installed mods
  remuda mod update NAME|--all   update a mod
  remuda mod remove NAME         remove a mod
  remuda cluster                 show cluster status
  remuda cluster init            create this node's cluster identity

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
                    eprintln!("  remuda {command} [--agent AGENT] [--headless]");
                }
            }
            ExitCode::SUCCESS
        }
        Err(error) => fail(error),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ClusterCommand {
    Status,
    Init,
    Invalid,
}

fn parse_cluster_command(args: &[&str]) -> ClusterCommand {
    match args {
        [] => ClusterCommand::Status,
        ["init"] => ClusterCommand::Init,
        _ => ClusterCommand::Invalid,
    }
}

fn cluster_command(args: &[&str]) -> ExitCode {
    match parse_cluster_command(args) {
        ClusterCommand::Status => match remuda_native::cluster::status() {
            Ok(None) => {
                println!("This node is not in a cluster; run `remuda cluster init`.");
                ExitCode::SUCCESS
            }
            Ok(Some((identity, members))) => {
                println!("Node: {}", identity.node_name);
                println!("Fingerprint: {}", identity.node_fp);
                println!("Members: {members}");
                ExitCode::SUCCESS
            }
            Err(error) => fail(format!("cluster status: {error}")),
        },
        ClusterCommand::Init => match remuda_native::cluster::init() {
            Ok(identity) => {
                println!("Cluster initialized");
                println!("Node: {}", identity.node_name);
                println!("Fingerprint: {}", identity.node_fp);
                ExitCode::SUCCESS
            }
            Err(error) => fail(format!("cluster init: {error}")),
        },
        ClusterCommand::Invalid => fail("usage: remuda cluster [init]"),
    }
}

#[cfg(test)]
mod cluster_cli_tests {
    use super::{parse_cluster_command, ClusterCommand};

    #[test]
    fn cluster_status_and_init_are_recognized() {
        assert_eq!(parse_cluster_command(&[]), ClusterCommand::Status);
        assert_eq!(parse_cluster_command(&["init"]), ClusterCommand::Init);
        assert_eq!(parse_cluster_command(&["join"]), ClusterCommand::Invalid);
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
    ride_with_mouse(path, name, true)
}

fn ride_with_mouse(path: &Path, name: &str, mouse: bool) -> ExitCode {
    match remuda_native::client::attach_with_mouse(path, name, mouse) {
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
        Ok(Left::TakenOver) => ExitCode::from(2),
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
    // Help, version and upgrade must answer with no daemon at all (#115).
    if matches!(
        argv,
        ["daemon"]
            | ["mcp"]
            | ["stop", ..]
            | [
                "help" | "-h" | "--help" | "version" | "-V" | "--version" | "upgrade",
                ..
            ]
    ) || (argv.is_empty()
        && !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()))
    {
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
    match ensure_daemon(server, path) {
        Ok(()) => f(path),
        Err(e) => fail(e),
    }
}

/// Connect, or start the daemon when nothing proves one is running. The one
/// place a connect error is worded, so no caller blames a daemon that isn't.
fn ensure_daemon(server: &str, path: &Path) -> Result<(), String> {
    match remuda_native::ipc::connect(path) {
        Ok(_) => Ok(()),
        Err(error) if remuda_native::ipc::may_start_daemon(path, &error) => {
            start_daemon(server, path)
        }
        // A path the transport cannot even name proves nothing about a daemon.
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {
            Err(format!("cannot use {}: {error}", path.display()))
        }
        Err(error) => Err(format!(
            "cannot connect to remuda daemon at {}: {error}; refusing to start a second daemon",
            path.display()
        )),
    }
}

/// Run an installed mod's entry file in the daemon's image. Resolves through
/// the same resolver as the `remuda.exec()` Lua binding, so there is one list.
fn exec_command(path: &Path, name: &str) -> ExitCode {
    match remuda_native::packages::resolve(name) {
        Err(error) => fail(error),
        Ok(None) => fail(format!("no such package: {name}")),
        // Through the image's loader, not the entry as a plain chunk: that
        // discarded a lifecycle mod's declaration, so it never activated (#98).
        // The wrapper gets its own chunk name; the mod's frames keep theirs.
        Ok(Some(_)) => {
            let code = format!("remuda.exec({})", remuda_native::mcp::lua_string(name));
            match remuda_native::script::run_source(path, "=remuda exec", &code) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => fail(e),
            }
        }
    }
}

/// Dispatch a manifest-declared mod command. The launch form may select an
/// agent and/or skip the screen; other arguments belong to the Lua mod.
fn extension_command(server: &str, path: &Path, command: &str, args: &[&str]) -> ExitCode {
    let package = match remuda_native::packages::subcommand(command) {
        Ok(Some(package)) => package,
        Ok(None) => return fail(format!("no installed mod provides command {command}")),
        Err(error) => return fail(error),
    };
    let launch = match args {
        [] => Some((false, None)),
        ["--headless"] => Some((true, None)),
        ["--agent", agent] => Some((false, Some(*agent))),
        ["--agent", agent, "--headless"] | ["--headless", "--agent", agent] => {
            Some((true, Some(*agent)))
        }
        _ => None,
    };
    if let Some((headless, agent)) = launch {
        return with_daemon(server, path, |path| {
            if let Some(agent) = agent {
                let code = format!(
                    "remuda._mod_launch_options = remuda._mod_launch_options or {{}}; remuda._mod_launch_options[{}] = {{agent = {}}}",
                    serde_json::to_string(command).expect("command serializes"),
                    serde_json::to_string(agent).expect("agent serializes")
                );
                if eval_once(path, &code) != ExitCode::SUCCESS {
                    return ExitCode::FAILURE;
                }
            }
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
    let env = caller_env(std::env::vars());
    let code = format!(
        "return remuda._dispatch_extension_command({}, {{{arguments}}}, {{env = {{{env}}}}})",
        serde_json::to_string(command).expect("command serializes")
    );
    with_daemon(server, path, |path| eval_once(path, &code))
}

/// The caller's `REMUDA_*` variables as Lua table fields. The handler runs in
/// the daemon, whose own environment is not the caller's (#95).
fn caller_env(vars: impl Iterator<Item = (String, String)>) -> String {
    vars.filter(|(name, _)| name.starts_with("REMUDA_"))
        .map(|(name, value)| {
            format!(
                "[{}] = {}",
                serde_json::to_string(&name).expect("name serializes"),
                serde_json::to_string(&value).expect("value serializes")
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Removes the Windows temp startup log once `start_daemon` returns.
#[cfg(windows)]
struct StartupLogCleanup(std::path::PathBuf);

#[cfg(windows)]
impl Drop for StartupLogCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Spawn ourselves as the daemon and wait for the socket to answer. Wait on a
/// successful *connect*, not on the file existing, and pass `-s <server>`
/// through — a bare `remuda daemon` re-derives `"default"` and never matches.
fn start_daemon(server: &str, path: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find own binary: {e}"))?;
    // Keep stderr in a file beside the socket. The daemon outlives this client,
    // so a pipe reader would be dropped on successful startup and a later
    // diagnostic could kill the daemon with SIGPIPE. The file also preserves
    // startup errors for the failure message below.
    #[cfg(unix)]
    let stderr_path = path.with_extension("log");
    #[cfg(windows)]
    let stderr_path =
        std::env::temp_dir().join(format!("remuda-daemon-startup-{}.log", std::process::id()));
    if let Some(parent) = stderr_path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            format!(
                "cannot create daemon log directory {}: {e}",
                parent.display()
            )
        })?;
    }
    // Append, so the previous daemon's post-mortem survives this start; rotate
    // once past 1 MiB so it cannot grow forever.
    #[cfg(unix)]
    let stderr_file = {
        if fs::metadata(&stderr_path).is_ok_and(|m| m.len() > 1_048_576) {
            let _ = fs::rename(&stderr_path, stderr_path.with_extension("log.1"));
        }
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&stderr_path)
    };
    #[cfg(windows)]
    let stderr_file = fs::File::create(&stderr_path);
    let stderr_file = stderr_file
        .map_err(|e| format!("cannot create daemon log {}: {e}", stderr_path.display()))?;
    // Where this run's output starts, for the failure message below.
    let offset = stderr_file.metadata().map_or(0, |m| m.len());
    let mut separator = stderr_file.try_clone().ok();
    #[cfg(windows)]
    let _log_cleanup = StartupLogCleanup(stderr_path.clone());
    // std spawns with bInheritHandles=TRUE, so the daemon would inherit every
    // inheritable handle we hold — including our own stdout/stderr pipe ends,
    // keeping a caller that captures our output waiting on the daemon's life
    // instead of ours. Our std handles don't need to be inheritable: std's
    // Stdio::inherit passes an inheritable duplicate, not the handle itself.
    // Best effort, so errors (e.g. no std handle) are ignored. Issue #111.
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT};
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: both calls take plain values; an invalid handle only
            // makes SetHandleInformation fail.
            unsafe { SetHandleInformation(GetStdHandle(id), HANDLE_FLAG_INHERIT, 0) };
        }
    }
    let mut command = std::process::Command::new(exe);
    command
        .args(["-s", server, "daemon"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(stderr_file));
    // Its own session, as tmux's daemon(3) does: otherwise the daemon stays in
    // this CLI's process group and terminal session, and a hangup (an SSH
    // disconnect) SIGHUPs it along with the shell. See issue #106.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid() is async-signal-safe; the child is a fresh fork, so
        // it is never already a process-group leader and cannot fail.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start daemon: {e}"))?;
    // Written after spawn so it names the daemon's pid; the failure read below
    // drops it, since it may land after the daemon's own first lines.
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let separator_line = format!(
        "--- remuda daemon start pid {} at unix time {since_epoch} (UTC) ---",
        child.id()
    );
    if let Some(log) = separator.as_mut() {
        let _ = writeln!(log, "{separator_line}");
    }

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
    let _ = child.wait();
    let said = fs::read(&stderr_path)
        .ok()
        .and_then(|bytes| bytes.get(offset as usize..).map(<[u8]>::to_vec))
        .map(|bytes| String::from_utf8_lossy(&bytes).replace(&separator_line, ""))
        .map(|text| text.trim().to_string())
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

fn reload_mod_in_daemon(server: &str, path: &Path, name: &str) -> Result<(), String> {
    ensure_daemon(server, path)?;
    match remuda_native::client::request(
        path,
        &Request::Eval {
            code: format!(
                "return remuda.reload({})",
                remuda_native::mcp::lua_string(name)
            ),
            name: None,
        },
    )
    .map_err(|error| error.to_string())?
    {
        Response::Value(_) => Ok(()),
        Response::Error(error) => Err(error),
        other => Err(format!("unexpected reload response: {other:?}")),
    }
}

fn mod_install_command(server: &str, path: &Path, args: &[&str]) -> ExitCode {
    let Some(repository) = args.first() else {
        return fail("usage: remuda mod install OWNER/REPO [--ref REF] [--force] [--reload]");
    };
    let mut reference = None;
    let mut force = false;
    let mut reload = false;
    let mut index = 1;
    while index < args.len() {
        match args[index] {
            "--force" if !force => force = true,
            "--reload" if !reload => reload = true,
            "--ref" if reference.is_none() && index + 1 < args.len() => {
                index += 1;
                reference = Some(args[index]);
            }
            _ => {
                return fail(
                    "usage: remuda mod install OWNER/REPO [--ref REF] [--force] [--reload]",
                )
            }
        }
        index += 1;
    }
    match remuda_native::packages::install(repository, reference, force) {
        Ok(report) => {
            println!(
                "installed mod {} {} from {} at {}",
                report.manifest.name, report.manifest.version, report.repository, report.commit
            );
            if reload {
                match reload_mod_in_daemon(server, path, &report.manifest.name) {
                    Ok(()) => println!(
                        "reloaded mod {} in the running daemon",
                        report.manifest.name
                    ),
                    Err(error) => {
                        return fail(format!(
                            "installed mod {}, but its running copy was not replaced: {error}",
                            report.manifest.name
                        ))
                    }
                }
            } else {
                if report.manifest.lifecycle.is_some() {
                    println!(
                        "use `remuda -e \"remuda.reload('{}')\"` to reload in-process, or restart the daemon",
                        report.manifest.name
                    );
                } else {
                    println!(
                        "legacy mod {} is installed; run it once with `remuda exec {}` or restart the daemon to load updated files",
                        report.manifest.name, report.manifest.name
                    );
                }
            }
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
                        "lifecycle": entry.lifecycle,
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
                    "## `{}`\n\n- version: `{}`\n- api: `{}`\n- entry: `{}`\n- lifecycle: `{}`\n- source: `{}`\n- status: `{}`\n",
                    entry.name, entry.version, entry.api, entry.entry,
                    entry.lifecycle.as_deref().unwrap_or("legacy"), entry.source, entry.status
                );
            }
        }
        "rst" => {
            println!("Remuda mods\n===========\n");
            for entry in manifests {
                println!(
                    "{}\n{}\n\n* version: ``{}``\n* api: ``{}``\n* entry: ``{}``\n* lifecycle: ``{}``\n* source: ``{}``\n* status: ``{}``\n",
                    entry.name,
                    "-".repeat(entry.name.len()),
                    entry.version,
                    entry.api,
                    entry.entry,
                    entry.lifecycle.as_deref().unwrap_or("legacy"),
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
                "lifecycle": manifest.lifecycle,
                "source": manifest.source,
                "status": manifest.status,
            })
        ),
        "markdown" => println!(
            "# `{}`\n\n- version: `{}`\n- api: `{}`\n- entry: `{}`\n- lifecycle: `{}`\n- source: `{}`\n- status: `{}`",
            manifest.name, manifest.version, manifest.api, manifest.entry,
            manifest.lifecycle.as_deref().unwrap_or("legacy"), manifest.source, manifest.status
        ),
        "rst" => println!(
            "{}\n{}\n\n* version: ``{}``\n* api: ``{}``\n* entry: ``{}``\n* lifecycle: ``{}``\n* source: ``{}``\n* status: ``{}``",
            manifest.name,
            "-".repeat(manifest.name.len()),
            manifest.version,
            manifest.api,
            manifest.entry,
            manifest.lifecycle.as_deref().unwrap_or("legacy"),
            manifest.source,
            manifest.status
        ),
        _ => unreachable!(),
    }
    ExitCode::SUCCESS
}

fn mod_update_all() -> ExitCode {
    match remuda_native::packages::update_all() {
        Ok(batch) => {
            for report in &batch.updated {
                println!(
                    "updated mod {} {} from {} at {}",
                    report.manifest.name, report.manifest.version, report.repository, report.commit
                );
            }
            if let Some(failed) = batch.failed {
                println!(
                    "update outcomes: updated [{}]; reloaded none; failed {}: {}; not attempted [{}]",
                    display_names(
                        &batch.updated.iter().map(|report| report.manifest.name.clone()).collect::<Vec<_>>()
                    ),
                    failed.name,
                    failed.error,
                    display_names(&batch.not_attempted)
                );
                fail(format!("update stopped after {} failed", failed.name))
            } else {
                println!(
                    "update outcomes: updated [{}]; reloaded none; failed none; not attempted none",
                    display_names(
                        &batch
                            .updated
                            .iter()
                            .map(|report| report.manifest.name.clone())
                            .collect::<Vec<_>>()
                    )
                );
                println!("use `remuda mod update NAME --reload` to reload in-process, or restart the daemon");
                ExitCode::SUCCESS
            }
        }
        Err(error) => fail(error),
    }
}

fn mod_update_command(server: &str, path: &Path, args: &[&str]) -> ExitCode {
    let (all, name, reload) = match args {
        ["--all"] => (true, None, false),
        ["--all", "--reload"] => (true, None, true),
        [name] => (false, Some(*name), false),
        [name, "--reload"] => (false, Some(*name), true),
        _ => return fail("usage: remuda mod update NAME [--reload]|--all"),
    };
    if all && reload {
        let manifests = match remuda_native::packages::manifests() {
            Ok(manifests) => manifests,
            Err(error) => return fail(error),
        };
        let not_attempted = all_reload_not_attempted_names(&manifests);
        println!(
            "batch reload preflight: updated none; reloaded none; failed none; not attempted [{}]",
            display_names(&not_attempted)
        );
        return fail(
            "`remuda mod update --all --reload` is disabled; update and reload mods individually",
        );
    }
    if all {
        return mod_update_all();
    }
    let name = name.expect("single mod name");
    let result = if reload {
        match remuda_native::packages::manifest(name) {
            Ok(Some(manifest)) if manifest.lifecycle.is_some() => {
                remuda_native::packages::update_lifecycle(name)
            }
            Ok(Some(_)) => {
                return fail(format!(
                    "update outcomes: updated none; reloaded none; failed none; not attempted [{}]: legacy mods cannot be reloaded in-process",
                    name
                ));
            }
            Ok(None) => return fail(format!("mod {name} is not installed")),
            Err(error) => return fail(error),
        }
    } else {
        remuda_native::packages::update(name)
    };
    match result {
        Ok(report) => {
            println!(
                "updated mod {} {} from {} at {}",
                report.manifest.name, report.manifest.version, report.repository, report.commit
            );
            if reload {
                match reload_mod_in_daemon(server, path, &report.manifest.name) {
                    Ok(()) => {
                        println!("update outcomes: updated [{}]; reloaded [{}]; failed none; not attempted none", report.manifest.name, report.manifest.name);
                        ExitCode::SUCCESS
                    }
                    Err(error) => fail(format!(
                        "update outcomes: updated [{}]; reloaded none; failed {}: {}; not attempted none",
                        report.manifest.name, report.manifest.name, error
                    )),
                }
            } else {
                println!(
                    "update outcomes: updated [{}]; reloaded none; failed none; not attempted none",
                    report.manifest.name
                );
                println!("use `remuda mod update NAME --reload` to reload in-process, or restart the daemon");
                ExitCode::SUCCESS
            }
        }
        Err(error) if reload => fail(format!(
            "update outcomes: updated none; reloaded none; failed {name}: {error}; not attempted none"
        )),
        Err(error) => fail(error),
    }
}

fn all_reload_not_attempted_names(manifests: &[remuda_native::packages::Manifest]) -> Vec<String> {
    manifests
        .iter()
        .map(|manifest| manifest.name.clone())
        .collect()
}

fn display_names(names: &[String]) -> String {
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
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

#[cfg(test)]
mod mod_update_tests {
    use super::all_reload_not_attempted_names;
    use remuda_native::packages::Manifest;

    #[test]
    fn batch_reload_policy_leaves_mixed_lifecycle_and_legacy_mods_unattempted() {
        let manifests = vec![
            Manifest {
                name: "managed".into(),
                version: "1".into(),
                api: "remuda-lua-v1".into(),
                entry: "init.lua".into(),
                command: None,
                lifecycle: Some("remuda-module-v1".into()),
                source: "disk".into(),
                status: "installed".into(),
            },
            Manifest {
                name: "legacy".into(),
                version: "1".into(),
                api: "remuda-lua-v1".into(),
                entry: "init.lua".into(),
                command: None,
                lifecycle: None,
                source: "disk".into(),
                status: "installed".into(),
            },
        ];
        assert_eq!(
            all_reload_not_attempted_names(&manifests),
            vec!["managed", "legacy"]
        );
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
    let message = message.to_string();
    if let Some((code, text)) = remuda_native::image::typed_failure_message(&message) {
        eprintln!("{text}");
        return ExitCode::from(code);
    }
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

    #[test]
    fn a_failed_daemon_start_includes_its_stderr() {
        let dir = std::env::temp_dir().join(format!(
            "remuda-startup-stderr-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        let path = daemon::socket_path_in(&dir, "s");
        #[cfg(unix)]
        let log = path.with_extension("log");
        #[cfg(unix)]
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        #[cfg(unix)]
        fs::write(&log, "stale output from a previous run\n").unwrap();

        // `current_exe()` is this test harness. It exits immediately with a
        // useful stderr diagnostic when asked to run as a daemon.
        let error = start_daemon("s", &path).expect_err("the test harness is not a daemon");
        assert!(error.contains("daemon did not come up"), "{error}");
        assert!(
            !error.contains("it printed nothing"),
            "the child diagnostic must survive startup failure: {error}"
        );
        assert!(
            !error.contains("stale output from a previous run"),
            "the failure must quote only this run's output: {error}"
        );
        #[cfg(unix)]
        assert!(
            fs::read_to_string(&log)
                .unwrap()
                .contains("stale output from a previous run"),
            "a new daemon run must keep its prior log"
        );
        let _ = fs::remove_dir_all(dir);
    }

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

    #[test]
    fn caller_env_keeps_only_remuda_vars_as_escaped_lua_fields() {
        let vars = [
            ("REMUDA_BUTLER_AGENT_ID", "dev \"lead\""),
            ("HOME", "/home/x"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()));
        assert_eq!(
            caller_env(vars.into_iter()),
            r#"["REMUDA_BUTLER_AGENT_ID"] = "dev \"lead\"""#
        );
    }
}
