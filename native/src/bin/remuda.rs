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
//! state-creating commands start the daemon on first use; read-only commands
//! require it to be running already. `-s` names one. See `USAGE`.

use remuda_core::protocol::{Request, Response};
use remuda_native::client::Left;
use remuda_native::cluster::listener_config::{ListenerBind, ListenerConfig};
use remuda_native::net::advertise_addr::CLUSTER_DEFAULT_PORT;
use remuda_native::{daemon, dist, terminal_size};
use std::fs;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
#[cfg(unix)]
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
static JOIN_INTERRUPTED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
static JOIN_INTERRUPT_SIGNAL: AtomicI32 = AtomicI32::new(0);

#[cfg(unix)]
extern "C" fn record_join_interrupt(signal: libc::c_int) {
    let _ = JOIN_INTERRUPT_SIGNAL.compare_exchange(0, signal, Ordering::Relaxed, Ordering::Relaxed);
    JOIN_INTERRUPTED.store(true, Ordering::Release);
}

#[path = "remuda/codex_tui.rs"]
mod codex_tui;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (server, rest) = split_server_flag(&args);
    let (stdin_enabled, rest) = match split_stdin_flag(rest) {
        Ok(flags) => flags,
        Err(error) => return fail(error),
    };
    let argv: Vec<&str> = rest.iter().map(String::as_str).collect();

    if let Some(exit) = run_internal_command(&argv) {
        return exit;
    }

    let path = daemon::socket_path(server);

    let skew = match prepare_command(&argv, &path) {
        Ok(skew) => skew,
        Err(error) => return fail(error),
    };

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

        ["_codex_watch", parent_pid, process_group] => run_codex_watch(parent_pid, process_group),

        // No daemon involved: this replaces the binary, it does not talk to one.
        ["upgrade", rest @ ..] => run_upgrade(rest),

        // Not part of the user-facing set: this is what the auto-start spawns.
        ["daemon"] => match daemon::serve_with_runtime(&path, &daemon::runtime_dir()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(format!("daemon: {e}")),
        },

        // Deliberately NOT behind `with_daemon`: the daemon this stops is often
        // exactly the one that cannot be talked to, and starting one to stop it
        // is not a thing to do.
        ["stop", rest @ ..] => stop(server, &path, rest),

        ["ls"] => with_existing_daemon(server, &path, list_sessions),
        ["resize", rest @ ..] => resize_command(server, &path, rest),

        ["run", rest @ ..] => run_session(server, &path, rest),

        ["send", name, text @ ..] => send_command(server, &path, name, text),

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

        ["cluster", rest @ ..] => cluster_command(server, &path, rest),

        [command, rest @ ..]
            if remuda_native::packages::valid_component(command)
                && remuda_native::packages::has_subcommand(command) =>
        {
            extension_command(server, &path, command, rest, stdin_enabled)
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

        ["doc", rest @ ..] => with_existing_daemon(server, &path, |path| doc_command(path, rest)),

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
        _ => {
            let word = argv.first().copied().unwrap_or("");
            if !remuda_native::packages::valid_component(word) {
                unknown_command(word)
            } else {
                match remuda_native::packages::half_installed(word) {
                    Some(message) => fail(message),
                    None => unknown_command(word),
                }
            }
        }
    }
}

fn run_codex_watch(parent_pid: &str, process_group: &str) -> ExitCode {
    match (parent_pid.parse::<i32>(), process_group.parse::<i32>()) {
        (Ok(parent_pid), Ok(process_group)) => codex_tui::watch_parent(parent_pid, process_group),
        _ => fail("invalid Codex app-server watch parameters"),
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
  remuda --stdin MOD [ARGS…]      opt in to passing up to 1 MiB of stdin to the mod
  remuda MOD ... -                a literal '-' argument also opts in to stdin
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
  remuda cluster init --new-identity [--yes]
                                  leave the cluster and create a new identity
  remuda cluster invite           invite another node
  remuda cluster join             join another node's cluster
  remuda cluster nodes            list local cluster membership
  remuda cluster revoke NODE [--yes] revoke a member locally
  remuda cluster remote [node/session] open the read-only cluster tree
  remuda cluster listen [--bind ADDR] [--allow-public] configure the daemon listener
                                  --foreground holds the listener in this terminal
                                  [::] may accept IPv4 too on dual-stack systems
  remuda doc [--format F]        print live Lua documentation (rst by default)
  remuda -e <code>              evaluate one chunk in that same image
  remuda repl                   the same image, a line at a time
  remuda mcp                    serve the image as an MCP tool on stdin/stdout
  remuda upgrade [--channel C]  re-run the installer on stable or nightly
  remuda stop [-f] [--yes] [--i-am-inside]  stop the daemon; the next command
                                  starts a fresh one. A hosted session needs the
                                  explicit --i-am-inside override.
                                  Its sessions and Lua image die with it,
                                  so a live herd is named and confirmed first.
  remuda --version              the version this binary was built with

Four verbs, not ten. A verb is here only if it needs a terminal, must survive
the shell's own quoting, or renders for a human in a way the image cannot.
`new`, `close`, `capture`, `insert`, `key` and `click` are all still there —
in Lua, where they cost no front page:

  remuda -e 'remuda.session.close(\"build\")'

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

  remuda.session.new(\"build\", {\"make\", \"-j4\"})
  local function close_when_ready()
    if remuda.capture(\"build\"):find(\"%$ %s*$\") then
      remuda.session.close(\"build\")
    else
      remuda.after(0.2, close_when_ready)
    end
  end
  remuda.after(0.2, close_when_ready)

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
  remuda resize NAME COLS ROWS   resize a session (cols 20..1000, rows 24..500)
  remuda upgrade [--channel stable|nightly]  replace the CLI binary
  remuda stop [-f] [--yes] [--i-am-inside]  stop the daemon (sessions are lost)

  remuda mod install OWNER/REPO  install a mod from GitHub
  remuda mod list | info NAME    inspect installed mods
  remuda mod update NAME|--all   update a mod
  remuda mod remove NAME         remove a mod
  remuda cluster                 show cluster status
  remuda cluster init            create this node's cluster identity
  remuda cluster init --new-identity [--yes]
                                 leave the cluster and create a new identity
  remuda cluster invite           invite another node
  remuda cluster join             join another node's cluster
  remuda cluster nodes           list local cluster membership
  remuda cluster revoke NODE [--yes] revoke a member locally
  remuda cluster remote [node/session] open the read-only cluster tree
  remuda cluster listen [--bind ADDR] [--allow-public] configure the daemon listener
                                  --foreground holds the listener in this terminal
                                  [::] may accept IPv4 too on dual-stack systems

  remuda doc | repl | -e CODE    use the persistent Lua runtime
  remuda --stdin MOD [ARGS…]     opt in to passing up to 1 MiB of stdin to the mod
  remuda MOD ... -                a literal '-' argument also opts in to stdin
  remuda --version

Run `remuda mod list` for installed mods and `remuda doc` for the live Lua API.
Next: run `remuda run -n NAME COMMAND` to start a session, or `remuda ls` to inspect sessions.
";

const UPGRADE_HELP: &str = "\
Usage: remuda upgrade [--channel stable|nightly]

Re-runs the installer to replace the CLI with the latest stable or nightly
release. The running daemon and its sessions keep using the old version until
you run `remuda stop`.

Next: run `remuda upgrade` to install the latest version from your channel.
";

fn help_command() -> ExitCode {
    print!("{USAGE}");
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

fn unknown_command(word: &str) -> ExitCode {
    if word.starts_with("remuda-join-v1") {
        eprintln!(
            "remuda: that looks like a join line; run: remuda cluster join [FINGERPRINT] 'remuda-join-v1 …' (FINGERPRINT required when not on a terminal)"
        );
    } else {
        let command_word = is_command_word(word);
        if command_word {
            eprintln!("remuda: no command or mod named {word}.");
        } else {
            eprintln!("remuda: no command or mod with that name.");
        }

        let command_suggestion = if command_word {
            suggest_command(word).map(|name| format!("remuda {name}"))
        } else {
            None
        };
        let suggestion = command_suggestion.clone().or_else(|| {
            if command_word {
                let installed_mod_names = remuda_native::packages::manifests()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|manifest| manifest.name)
                    .collect::<Vec<_>>();
                let installed_mod_name_refs = installed_mod_names
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>();
                closest_word(word, &installed_mod_name_refs).map(|name| format!("remuda {name}"))
            } else {
                None
            }
        });
        if let Some(suggestion) = suggestion {
            eprintln!("Did you mean: {suggestion}?");
        }

        let next = if let Some(command) = command_suggestion {
            format!("Next: run {command}.")
        } else if command_word {
            format!("Next: if {word} is a mod, install it with remuda mod install OWNER/REPO; installed mods: remuda mod list.")
        } else {
            "Next: if this is a mod, install it with remuda mod install OWNER/REPO; installed mods: remuda mod list.".to_string()
        };
        eprintln!("{next}");
    }
    ExitCode::FAILURE
}

fn is_command_word(word: &str) -> bool {
    !word.is_empty()
        && word.len() <= 32
        && word
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn suggest_command(word: &str) -> Option<String> {
    const CLUSTER_VERBS: &[&str] = &[
        "nodes", "init", "invite", "join", "revoke", "remote", "listen", "control", "call",
    ];
    if CLUSTER_VERBS.contains(&word) {
        return Some(format!("cluster {word}"));
    }

    const TOP_LEVEL_VERBS: &[&str] = &[
        "run", "attach", "ls", "send", "resize", "stop", "mod", "doc", "repl", "lua", "exec",
        "mcp", "upgrade", "cluster",
    ];
    closest_word(word, TOP_LEVEL_VERBS).map(str::to_owned)
}

fn closest_word<'a>(word: &str, candidates: &[&'a str]) -> Option<&'a str> {
    let word_length = word.chars().count();
    candidates
        .iter()
        .copied()
        .map(|candidate| (levenshtein(word, candidate), candidate))
        .filter(|(distance, _)| *distance <= 2 && *distance < word_length)
        .min_by_key(|(distance, candidate)| (*distance, *candidate))
        .map(|(_, candidate)| candidate)
}

fn levenshtein(left: &str, right: &str) -> usize {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    let mut row: Vec<usize> = (0..=right.len()).collect();
    for (i, left_char) in left.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, right_char) in right.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (row[j] + 1)
                .min(above + 1)
                .min(diagonal + usize::from(left_char != right_char));
            diagonal = above;
        }
    }
    row[right.len()]
}

#[derive(Debug, PartialEq, Eq)]
enum ClusterCommand {
    Status,
    Init {
        no_listen: bool,
    },
    InitNewIdentity {
        yes: bool,
    },
    Invite {
        bind_addr: Option<std::net::SocketAddr>,
        advertised_addr: Option<std::net::SocketAddr>,
    },
    Join {
        fingerprint: Option<String>,
        invitation: remuda_native::cluster::join_line::JoinLine,
        bind_addr: Option<std::net::SocketAddr>,
    },
    Nodes,
    Control(bool),
    Revoke {
        target: String,
        yes: bool,
    },
    Remote(Option<String>),
    ListenOff,
    Listen {
        bind_addr: Option<std::net::SocketAddr>,
        allow_public: bool,
        foreground: bool,
    },
    Call {
        target: String,
        address: std::net::SocketAddr,
        action: CallAction,
        json: bool,
    },
    Help(Option<String>),
    UnknownVerb(String),
    Invalid {
        verb: String,
        reason: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
enum CallAction {
    List,
    Capture(String),
}

fn parse_cluster_command(args: &[&str]) -> ClusterCommand {
    if args.is_empty() {
        return ClusterCommand::Status;
    }
    if matches!(args, ["help"] | ["-h"] | ["--help"]) {
        return ClusterCommand::Help(None);
    }
    if let [verb, "--help"] = args {
        if cluster_verb_known(verb) {
            return ClusterCommand::Help(Some((*verb).to_owned()));
        }
    }
    let (verb, rest) = args.split_first().expect("non-empty cluster args");
    if !cluster_verb_known(verb) {
        return ClusterCommand::UnknownVerb((*verb).to_owned());
    }
    let parsed = match *verb {
        "init" => parse_cluster_init(rest),
        "invite" => parse_cluster_invite(rest),
        "join" => parse_cluster_join(rest),
        "nodes" => parse_cluster_nodes(rest),
        "control" => parse_cluster_control(rest),
        "revoke" => parse_cluster_revoke(rest),
        "remote" => parse_cluster_remote(rest),
        "listen" => parse_cluster_listen(rest),
        "call" => parse_cluster_call_args(rest),
        _ => unreachable!("cluster_verb_known and parser dispatch disagree"),
    };
    parsed.unwrap_or_else(|reason| invalid_cluster(verb, reason))
}

fn cluster_verb_known(verb: &str) -> bool {
    matches!(
        verb,
        "init" | "invite" | "join" | "nodes" | "control" | "revoke" | "remote" | "listen" | "call"
    )
}

fn invalid_cluster(verb: &str, reason: impl Into<String>) -> ClusterCommand {
    ClusterCommand::Invalid {
        verb: verb.to_owned(),
        reason: reason.into(),
    }
}

fn parse_cluster_init(args: &[&str]) -> Result<ClusterCommand, String> {
    match args {
        [] => Ok(ClusterCommand::Init { no_listen: false }),
        ["--no-listen"] => Ok(ClusterCommand::Init { no_listen: true }),
        ["--new-identity"] => Ok(ClusterCommand::InitNewIdentity { yes: false }),
        ["--new-identity", "--yes"] => Ok(ClusterCommand::InitNewIdentity { yes: true }),
        _ => Err("expected [--no-listen | --new-identity [--yes]]".into()),
    }
}

fn parse_cluster_invite(args: &[&str]) -> Result<ClusterCommand, String> {
    let mut bind_addr = None;
    let mut advertised_addr = None;
    let mut index = 0;
    while index < args.len() {
        let (option, value, consumed) = match args[index] {
            "--bind" | "--addr" => {
                let option = args[index];
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("missing address after {option}"))?;
                (option, *value, 2)
            }
            flag if flag.starts_with("--bind=") => ("--bind", &flag[7..], 1),
            flag if flag.starts_with("--addr=") => ("--addr", &flag[7..], 1),
            argument => return Err(format!("unexpected argument {argument}")),
        };
        let address = parse_addr_default_port(value)?;
        match option {
            "--bind" => {
                if bind_addr.replace(address).is_some() {
                    return Err("--bind may only be provided once".into());
                }
            }
            "--addr" => {
                remuda_native::cluster::join_line::validate_endpoint(address)
                    .map_err(|_| "invalid advertised address")?;
                if advertised_addr.replace(address).is_some() {
                    return Err("--addr may only be provided once".into());
                }
            }
            _ => unreachable!("invite option parser returned an unknown option"),
        }
        index += consumed;
    }
    Ok(ClusterCommand::Invite {
        bind_addr,
        advertised_addr,
    })
}

fn parse_cluster_join(args: &[&str]) -> Result<ClusterCommand, String> {
    if args.len() > 2 {
        if let Some(index) = args
            .iter()
            .position(|arg| arg.starts_with("remuda-join-v1"))
        {
            if remuda_native::cluster::join_line::JoinLine::decode(args[index]).is_err() {
                let reason = "quote the whole join line (it contains spaces); FINGERPRINT is optional on a terminal: remuda cluster join [FINGERPRINT] 'remuda-join-v1 …'";
                return Err(reason.into());
            }
        }
    }
    let (fingerprint, line_index) = match args {
        [] => return Err("missing join line".into()),
        [_] => (None, 0),
        [line, ..] if remuda_native::cluster::join_line::JoinLine::decode(line).is_ok() => {
            (None, 0)
        }
        [fingerprint, ..] => (Some(*fingerprint), 1),
    };
    let line = args.get(line_index).ok_or("missing join line")?;
    let invitation = match remuda_native::cluster::join_line::JoinLine::decode(line) {
        Ok(invitation) => invitation,
        Err(_) => return Err("invalid join line".into()),
    };
    let bind_addr = match &args[line_index + 1..] {
        [] => None,
        ["--bind", address] => Some(parse_join_bind(address)?),
        [flag] if flag.starts_with("--bind=") => Some(parse_join_bind(&flag[7..])?),
        _ => return Err("unexpected arguments".into()),
    };
    Ok(ClusterCommand::Join {
        fingerprint: fingerprint.map(str::to_owned),
        invitation,
        bind_addr,
    })
}

fn parse_join_bind(value: &str) -> Result<std::net::SocketAddr, String> {
    let address = parse_addr_default_port(value)?;
    if address.port() == 0 {
        remuda_native::net::listener::validate_bind_address(address, false)
            .map_err(|_| "invalid client bind address")?;
    } else {
        remuda_native::cluster::join_line::validate_endpoint(address)
            .map_err(|_| "invalid client bind address")?;
    }
    Ok(address)
}

fn parse_cluster_nodes(args: &[&str]) -> Result<ClusterCommand, String> {
    if args.is_empty() {
        Ok(ClusterCommand::Nodes)
    } else {
        Err("unexpected arguments".into())
    }
}

fn parse_cluster_control(args: &[&str]) -> Result<ClusterCommand, String> {
    match args {
        ["on"] => Ok(ClusterCommand::Control(true)),
        ["off"] => Ok(ClusterCommand::Control(false)),
        _ => Err("expected on or off".into()),
    }
}

fn parse_cluster_revoke(args: &[&str]) -> Result<ClusterCommand, String> {
    match args {
        [target] => Ok(ClusterCommand::Revoke {
            target: (*target).to_owned(),
            yes: false,
        }),
        [target, "--yes"] | ["--yes", target] => Ok(ClusterCommand::Revoke {
            target: (*target).to_owned(),
            yes: true,
        }),
        _ => Err("expected a node or fingerprint".into()),
    }
}

fn parse_cluster_remote(args: &[&str]) -> Result<ClusterCommand, String> {
    match args {
        [] => Ok(ClusterCommand::Remote(None)),
        [target] if target.contains('/') => Ok(ClusterCommand::Remote(Some((*target).to_owned()))),
        _ => Err("expected NODE/SESSION".into()),
    }
}

fn parse_cluster_listen(args: &[&str]) -> Result<ClusterCommand, String> {
    if args == ["--off"] {
        return Ok(ClusterCommand::ListenOff);
    }
    let mut address = None;
    let mut allow_public = false;
    let mut foreground = false;
    let mut index = 0;
    while index < args.len() {
        match args[index] {
            "--bind" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err("missing --bind ADDR".into());
                };
                if address.replace(*value).is_some() {
                    return Err("--bind may only be provided once".into());
                }
            }
            flag if flag.starts_with("--bind=") => {
                if address.replace(&flag[7..]).is_some() {
                    return Err("--bind may only be provided once".into());
                }
            }
            "--allow-public" => allow_public = true,
            "--foreground" => foreground = true,
            _ => return Err("expected --bind ADDR [--allow-public] [--foreground]".into()),
        }
        index += 1;
    }
    if address.is_none() && (allow_public || foreground) {
        return Err("--allow-public and --foreground require --bind ADDR".into());
    }
    let bind_addr = address.map(parse_addr_default_port).transpose()?;
    Ok(ClusterCommand::Listen {
        bind_addr,
        allow_public,
        foreground,
    })
}

fn parse_cluster_call_args(args: &[&str]) -> Result<ClusterCommand, String> {
    let [target, operation, rest @ ..] = args else {
        return Err("expected NODE and list or capture SESSION".into());
    };
    parse_cluster_call(target, operation, rest)
}

fn parse_cluster_call(
    target: &str,
    operation: &str,
    args: &[&str],
) -> Result<ClusterCommand, String> {
    let action = match operation {
        "list" => CallAction::List,
        "capture" => match args.first() {
            Some(session) if !session.starts_with('-') => CallAction::Capture((*session).into()),
            _ => return Err("missing SESSION for capture".into()),
        },
        _ => return Err("expected list or capture SESSION".into()),
    };
    let option_start = usize::from(matches!(action, CallAction::Capture(_)));
    let mut address = None;
    let mut json = false;
    let mut index = option_start;
    while index < args.len() {
        match args[index] {
            "--json" if !json => {
                json = true;
                index += 1;
            }
            "--addr" if address.is_none() => {
                let value = args.get(index + 1).ok_or("missing address after --addr")?;
                address = Some(parse_addr_default_port(value)?);
                index += 2;
            }
            flag if address.is_none() && flag.starts_with("--addr=") => {
                address = Some(parse_addr_default_port(&flag[7..])?);
                index += 1;
            }
            _ => return Err(format!("unexpected argument {}", args[index])),
        }
    }
    let address = address.ok_or("missing --addr HOST:PORT")?;
    Ok(ClusterCommand::Call {
        target: target.into(),
        address,
        action,
        json,
    })
}

fn parse_addr_default_port(value: &str) -> Result<std::net::SocketAddr, String> {
    if let Ok(address) = value.parse() {
        return Ok(address);
    }
    if let Ok(address) = value.parse::<std::net::IpAddr>() {
        return Ok(std::net::SocketAddr::new(address, CLUSTER_DEFAULT_PORT));
    }
    if let Some(address) = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .and_then(|value| value.parse::<std::net::Ipv6Addr>().ok())
    {
        return Ok(std::net::SocketAddr::new(
            std::net::IpAddr::V6(address),
            CLUSTER_DEFAULT_PORT,
        ));
    }
    Err(format!("invalid address '{value}'"))
}

pub fn cluster_usage(verb: &str) -> String {
    match verb {
        "init" => "usage: remuda cluster init [--no-listen | --new-identity [--yes]]\nexample: remuda cluster init --no-listen\n".into(),
        "invite" => format!("usage: remuda cluster invite [--bind IP[:PORT]] [--addr IP[:PORT]] (default port {CLUSTER_DEFAULT_PORT})\nexample: remuda cluster invite\n"),
        "join" => format!("usage: remuda cluster join [FINGERPRINT] 'JOIN_LINE' (FINGERPRINT required when not on a terminal) [--bind IP[:PORT] (default port {CLUSTER_DEFAULT_PORT})]\nexample: remuda cluster join 'remuda-join-v1 …'\n"),
        "nodes" => "usage: remuda cluster nodes\nexample: remuda cluster nodes\n".into(),
        "revoke" => "usage: remuda cluster revoke NODE|FINGERPRINT [--yes]\nexample: remuda cluster revoke node-abcd1234\n".into(),
        "control" => "usage: remuda cluster control on|off\nexample: remuda cluster control off\n".into(),
        "remote" => "usage: remuda cluster remote [NODE/SESSION]\nexample: remuda cluster remote\n".into(),
        "listen" => format!("usage: remuda cluster listen [--bind IP[:PORT] (default port {CLUSTER_DEFAULT_PORT})] [--allow-public] [--foreground]\nusage: remuda cluster listen --off\nexample: remuda cluster listen\n"),
        "call" => format!("usage: remuda cluster call NODE (list|capture SESSION) --addr IP[:PORT] (default port {CLUSTER_DEFAULT_PORT}) [--json]\nexample: remuda cluster call node-abcd1234 list --addr 192.0.2.1\n"),
        _ => "usage: remuda cluster <command>\n  init\n  invite\n  join\n  nodes\n  revoke\n  control\n  remote\n  listen\n  call\n  help\n".into(),
    }
}

fn cluster_command(server: &str, path: &Path, args: &[&str]) -> ExitCode {
    match parse_cluster_command(args) {
        ClusterCommand::Status => cluster_status(server, path),
        ClusterCommand::Init { no_listen } => cluster_init(server, path, no_listen),
        ClusterCommand::InitNewIdentity { yes } => cluster_init_new_identity(yes),
        ClusterCommand::Invite {
            bind_addr,
            advertised_addr,
        } => cluster_invite(server, path, bind_addr, advertised_addr),
        ClusterCommand::Join {
            fingerprint,
            invitation,
            bind_addr,
        } => cluster_join_command(server, path, fingerprint.as_deref(), &invitation, bind_addr),
        ClusterCommand::Nodes => match remuda_native::cluster::nodes() {
            Ok(Some((identity, registry))) => {
                let mut stdout = std::io::stdout().lock();
                match write_nodes_table(&mut stdout, &identity, &registry) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(error) => fail(format!("cluster nodes: {error}")),
                }
            }
            Ok(None) => fail("cluster is not initialized; run `remuda cluster init`"),
            Err(error) => fail(format!("cluster nodes: {error}")),
        },
        ClusterCommand::Revoke { target, yes } => cluster_revoke(&target, yes),
        ClusterCommand::Control(enabled) => {
            match remuda_native::cluster::control::set_enabled(enabled) {
                Ok(()) => {
                    println!("Remote control {}.", remote_control_label(enabled));
                    ExitCode::SUCCESS
                }
                Err(error) => fail(format!("cluster control: {error}")),
            }
        }
        ClusterCommand::Remote(target) => {
            let node = std::env::var("HOSTNAME").unwrap_or_else(|_| "local".into());
            cluster_remote(server, path, &node, target.as_deref())
        }
        ClusterCommand::ListenOff => cluster_listen_off(server, path),
        ClusterCommand::Listen {
            bind_addr,
            allow_public,
            foreground,
        } => cluster_listen(server, path, bind_addr, allow_public, foreground),
        ClusterCommand::Call {
            target,
            address,
            action,
            json,
        } => cluster_call(&target, address, action, json),
        ClusterCommand::Help(verb) => {
            print!("{}", cluster_usage(verb.as_deref().unwrap_or("")));
            ExitCode::SUCCESS
        }
        ClusterCommand::UnknownVerb(verb) => {
            if verb.starts_with("remuda-join-v1") {
                eprintln!(
                    "remuda: that looks like a join line; run: remuda cluster join FINGERPRINT 'remuda-join-v1 …'"
                );
            } else if is_command_word(&verb) {
                eprintln!("remuda: unknown cluster command '{verb}'");
            } else {
                eprintln!("remuda: unknown cluster command");
            }
            eprint!("{}", cluster_usage(""));
            eprintln!("Next: use one of the listed cluster commands.");
            ExitCode::from(2)
        }
        ClusterCommand::Invalid { verb, reason } => {
            eprintln!("remuda: cluster {verb}: {reason}");
            eprint!("{}", cluster_usage(&verb));
            ExitCode::from(2)
        }
    }
}

const NEW_IDENTITY_WARNING: &str =
    "This creates a new identity; this machine leaves its current cluster and needs a new invite.";

fn cluster_init_new_identity(yes: bool) -> ExitCode {
    let prompt = match new_identity_confirmation(
        yes,
        std::io::stdin().is_terminal(),
        std::io::stderr().is_terminal(),
    ) {
        Ok(prompt) => prompt,
        Err(error) => return fail(format!("cluster init --new-identity: {error}")),
    };
    eprintln!("{NEW_IDENTITY_WARNING}");
    if prompt {
        match confirm_new_identity() {
            Ok(true) => {}
            Ok(false) => {
                println!("Cluster identity rotation cancelled.");
                return ExitCode::SUCCESS;
            }
            Err(error) => return fail(format!("cluster init --new-identity: {error}")),
        }
    }
    match remuda_native::cluster::init_new_identity() {
        Ok(identity) => {
            println!("Cluster identity rotated");
            println!("Node: {}", identity.node_name);
            println!("Fingerprint: {}", identity.node_fp);
            ExitCode::SUCCESS
        }
        Err(error) => fail(format!("cluster init --new-identity: {error}")),
    }
}

fn cluster_listen(
    server: &str,
    path: &Path,
    bind_addr: Option<std::net::SocketAddr>,
    allow_public: bool,
    foreground: bool,
) -> ExitCode {
    if foreground {
        if let Some(bind_addr) = bind_addr {
            return cluster_listen_foreground(server, path, bind_addr, allow_public);
        }
        return fail("cluster listen --foreground requires --bind ADDR\nNext: run `remuda cluster listen` for the detected private address.");
    }
    with_daemon(server, path, |daemon_path| {
        let bind = bind_addr
            .map(ListenerBind::Explicit)
            .unwrap_or(ListenerBind::Auto);
        let config = ListenerConfig {
            enabled: true,
            bind,
            allow_public: bind_addr.is_some() && allow_public,
        };
        match remuda_native::cluster::listener_control::start(daemon_path, Some(config)) {
            Ok(remuda_core::protocol::ListenerStatus::On { addr, .. }) => {
                println!("Cluster listener on {addr}.");
                println!(
                    "Next: run `remuda cluster invite` or `remuda cluster join` when the other machine is ready."
                );
                ExitCode::SUCCESS
            }
            Ok(remuda_core::protocol::ListenerStatus::Off) => fail(
                "cluster listen: listener stayed off after reload\nNext: run `remuda cluster` and retry `remuda cluster listen`.",
            ),
            Ok(remuda_core::protocol::ListenerStatus::WaitingForLan(reason)) => {
                println!("Cluster listener waiting for a private LAN address.");
                println!("{}", listener_failure_next_step(&reason));
                ExitCode::SUCCESS
            }
            Ok(remuda_core::protocol::ListenerStatus::Failed(reason)) => fail(format!(
                "cluster listen: listener failed: {reason}\n{}",
                listener_failure_next_step(&reason)
            )),
            Err(error) => fail(format!(
                "cluster listen: could not reload the daemon listener: {error}\nNext: check `remuda cluster` and retry `remuda cluster listen`."
            )),
        }
    })
}

fn cluster_listen_foreground(
    server: &str,
    path: &Path,
    bind_addr: std::net::SocketAddr,
    allow_public: bool,
) -> ExitCode {
    with_daemon(server, path, |daemon_path| {
        let _host_lock = match acquire_foreground_listener_host_lock() {
            Ok(lock) => lock,
            Err(reason) => return fail(reason),
        };
        let config = remuda_native::net::listener::ListenerConfig {
            bind_addr,
            allow_unspecified: allow_public,
        };
        match remuda_native::net::listener::bind(config, daemon_path) {
            Ok(listener) => {
                let address = listener.local_addr().unwrap_or(bind_addr);
                for line in render_listener_addresses(address, false, Some(address), &[address]) {
                    eprintln!("remuda: {line}");
                }
                eprintln!("{}", next_step_listen());
                match listener.serve() {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(error) => fail(describe_cluster_error(
                        "listen",
                        &error,
                        ClusterErrorContext::Address(bind_addr),
                    )),
                }
            }
            Err(error) => fail(describe_cluster_error(
                "listen",
                &error,
                ClusterErrorContext::Address(bind_addr),
            )),
        }
    })
}

fn cluster_listen_off(server: &str, path: &Path) -> ExitCode {
    match remuda_native::ipc::connect(path) {
        Ok(_) => with_existing_daemon(server, path, |daemon_path| {
            match remuda_native::cluster::listener_control::stop(daemon_path) {
                Ok(remuda_core::protocol::ListenerStatus::Off) => {
                    println!("Cluster listener off.");
                    println!("Next: run `remuda cluster invite` when you are ready to admit a peer.");
                    ExitCode::SUCCESS
                }
                Ok(remuda_core::protocol::ListenerStatus::WaitingForLan(reason)) => fail(format!(
                    "cluster listen --off: listener is still waiting for a private LAN address\n{}",
                    listener_failure_next_step(&reason)
                )),
                Ok(remuda_core::protocol::ListenerStatus::On { addr, .. }) => fail(format!(
                    "cluster listen --off: listener remains on at {addr}\nNext: check `remuda cluster` and try `remuda cluster listen --off` again."
                )),
                Ok(remuda_core::protocol::ListenerStatus::Failed(reason)) => fail(format!(
                    "cluster listen --off: listener reload failed: {reason}\nNext: check `remuda cluster` and try `remuda cluster listen --off` again."
                )),
                Err(error) => fail(format!(
                    "cluster listen --off: could not stop listener: {error}\nNext: check `remuda cluster` and try `remuda cluster listen --off` again."
                )),
            }
        }),
        Err(error) if remuda_native::ipc::may_start_daemon(path, &error) => {
            let config = match remuda_native::cluster::listener_control::config() {
                Ok(config) => config.unwrap_or(ListenerConfig {
                    enabled: false,
                    bind: ListenerBind::Auto,
                    allow_public: false,
                }),
                Err(error) => {
                    return fail(format!(
                        "cluster listen --off: cannot read listener config: {error}\nNext: start the remuda daemon and retry `remuda cluster listen --off`."
                    ));
                }
            };
            let mut config = config;
            config.enabled = false;
            config.allow_public = false;
            if let Err(error) = remuda_native::cluster::listener_config::write(&config) {
                return fail(format!(
                    "cluster listen --off: cannot save disabled listener config: {error}\nNext: start the remuda daemon and retry `remuda cluster listen --off`."
                ));
            }
            println!("Cluster listener stays off (daemon not running).");
            println!("Next: run `remuda cluster invite` when you are ready to admit a peer.");
            ExitCode::SUCCESS
        }
        Err(error) => fail(format!(
            "cannot connect to remuda daemon at {}: {error}; refusing to start a second daemon\nNext: check `remuda cluster` and try `remuda cluster listen --off` again.",
            path.display()
        )),
    }
}

fn cluster_status(server: &str, path: &Path) -> ExitCode {
    match remuda_native::cluster::status() {
        Ok(None) => {
            println!("This node is not in a cluster; run `remuda cluster init`.");
            print_cluster_listener_status(server, path)
        }
        Ok(Some((identity, members))) => {
            println!("Node: {}", identity.node_name);
            println!("Fingerprint: {}", identity.node_fp);
            println!("Members: {members}");
            println!("Authority: Any admitted member can admit new keys and revoke any member cluster-wide (see #282).");
            match remuda_native::cluster::control::revoked_notice() {
                Ok(Some(notice)) => {
                    let mut stdout = std::io::stdout().lock();
                    if let Err(error) = write_revocation_notice(&mut stdout, &notice) {
                        if error.kind() == std::io::ErrorKind::BrokenPipe {
                            return ExitCode::SUCCESS;
                        }
                        return fail(format!("cluster status: {error}"));
                    }
                }
                Ok(None) => {}
                Err(error) => return fail(format!("cluster status: {error}")),
            }
            match remuda_native::cluster::control::enabled() {
                Ok(enabled) => {
                    let (setting, trust) = remote_control_status_lines(enabled);
                    println!("{setting}");
                    println!("{trust}");
                    let listener_status = print_cluster_listener_status(server, path);
                    if let Some(next_step) = next_step_status(members) {
                        println!("{next_step}");
                    }
                    listener_status
                }
                Err(error) => fail(format!("cluster status: {error}")),
            }
        }
        Err(error) => fail(format!("cluster status: {error}")),
    }
}

fn print_cluster_listener_status(server: &str, path: &Path) -> ExitCode {
    match remuda_native::ipc::connect(path) {
        Ok(stream) => {
            drop(stream);
            with_existing_daemon(server, path, |daemon_path| {
                let status = remuda_native::cluster::listener_control::status(daemon_path);
                for line in cluster_listener_status_lines(Some(status)) {
                    println!("{line}");
                }
                ExitCode::SUCCESS
            })
        }
        Err(error) if remuda_native::ipc::may_start_daemon(path, &error) => {
            for line in cluster_listener_status_lines(None) {
                println!("{line}");
            }
            ExitCode::SUCCESS
        }
        Err(_) => with_existing_daemon(server, path, |_| ExitCode::SUCCESS),
    }
}

fn cluster_init_listener_config(existing: Option<ListenerConfig>, enabled: bool) -> ListenerConfig {
    let (bind, allow_public) = match existing {
        Some(ListenerConfig {
            bind: ListenerBind::Explicit(address),
            allow_public,
            ..
        }) => (ListenerBind::Explicit(address), allow_public),
        _ => (ListenerBind::Auto, false),
    };
    ListenerConfig {
        enabled,
        bind,
        allow_public,
    }
}

fn cluster_init(server: &str, path: &Path, no_listen: bool) -> ExitCode {
    with_daemon(server, path, |daemon_path| {
        let (identity, created) = match remuda_native::cluster::init() {
            Ok(initialized) => initialized,
            Err(error) => return fail(render_cluster_init_error(&error)),
        };
        let existing_config = match remuda_native::cluster::listener_control::config() {
            Ok(config) => config,
            Err(error) => return fail(render_init_listener_error(&error)),
        };
        let config = cluster_init_listener_config(existing_config, !no_listen);
        let listener_status = if no_listen {
            if let Err(error) = remuda_native::cluster::listener_config::write(&config) {
                return fail(render_init_listener_error(&error));
            }
            remuda_native::cluster::listener_control::stop(daemon_path)
        } else {
            remuda_native::cluster::listener_control::start(daemon_path, Some(config))
        };
        match listener_status {
            Ok(status) => {
                for line in render_cluster_init_lines(
                    created,
                    &identity.node_name,
                    &identity.node_fp,
                    &status,
                ) {
                    println!("{line}");
                }
                ExitCode::SUCCESS
            }
            Err(error) => fail(render_init_listener_error(&error)),
        }
    })
}

fn render_cluster_init_error(error: &std::io::Error) -> String {
    format!("cluster init: {error}")
}

fn render_cluster_init_lines(
    created: bool,
    node_name: &str,
    fingerprint: &str,
    status: &remuda_core::protocol::ListenerStatus,
) -> Vec<String> {
    let mut lines = vec![
        cluster_init_message(created).to_owned(),
        format!("Node: {node_name}"),
        format!("Fingerprint: {fingerprint}"),
    ];
    lines.extend(render_init_listener_lines(status));
    lines
}

fn listener_failure_next_step(reason: &str) -> &'static str {
    if reason.contains("no private LAN address found") {
        "Next: remuda cluster listen --bind IP"
    } else {
        "Next: remuda cluster listen"
    }
}

fn render_init_listener_lines(status: &remuda_core::protocol::ListenerStatus) -> Vec<String> {
    use remuda_core::protocol::ListenerStatus;

    match status {
        ListenerStatus::On {
            addr,
            auto,
            advertise_addr,
            listen_addrs,
        } => {
            let mut lines = render_listener_addresses(*addr, *auto, *advertise_addr, listen_addrs);
            lines.push(
                "Only admitted machines can connect; turn off: remuda cluster listen --off".into(),
            );
            lines.push(next_step_init().into());
            lines
        }
        ListenerStatus::Off => vec!["Listener off (--no-listen)".into(), next_step_init().into()],
        ListenerStatus::WaitingForLan(reason) => vec![
            "Listener waiting for a private LAN address".into(),
            listener_failure_next_step(reason).into(),
        ],
        ListenerStatus::Failed(reason) => vec![
            format!("Listener failed: {reason}"),
            listener_failure_next_step(reason).into(),
        ],
    }
}

fn render_listener_addresses(
    addr: std::net::SocketAddr,
    auto: bool,
    advertise_addr: Option<std::net::SocketAddr>,
    listen_addrs: &[std::net::SocketAddr],
) -> Vec<String> {
    let addresses = if listen_addrs.is_empty() {
        vec![addr]
    } else {
        listen_addrs.to_vec()
    };
    let mut lines = Vec::new();
    if auto && addr.ip().is_unspecified() {
        if let Some(advertise_addr) = advertise_addr {
            lines.push(format!(
                "Listening on all interfaces ({}); invites use {advertise_addr}",
                addresses
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        } else {
            lines.push(format!(
                "Listening on all interfaces ({})",
                addresses
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    } else {
        lines.push(format!(
            "Listening on {}",
            addresses
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
        if let Some(advertise_addr) =
            advertise_addr.filter(|advertised| !addresses.contains(advertised))
        {
            lines.push(format!("Invites use {advertise_addr}"));
        }
    }
    for address in &addresses {
        if is_public_listener_address(address.ip()) {
            lines.push(format!(
                "Warning: listening on public address {address}; use --allow-public only on a trusted network"
            ));
        }
    }
    lines
}

fn is_public_listener_address(address: std::net::IpAddr) -> bool {
    if address.is_unspecified() || address.is_loopback() || address.is_multicast() {
        return false;
    }
    if remuda_native::net::advertise_addr::is_private_lan(address) {
        return false;
    }
    match address {
        std::net::IpAddr::V4(address) => {
            !address.is_link_local() && !address.is_broadcast() && !address.is_documentation()
        }
        std::net::IpAddr::V6(address) => {
            !address.is_unicast_link_local() && !address.is_unique_local()
        }
    }
}

fn render_init_listener_error(error: &std::io::Error) -> String {
    format!(
        "cluster init listener: {error}\nNext: remuda cluster init --no-listen or remuda cluster listen"
    )
}

fn cluster_listener_status_lines(
    status: Option<remuda_core::protocol::ListenerStatus>,
) -> Vec<String> {
    use remuda_core::protocol::ListenerStatus;

    match status {
        None => vec!["Listener: off (daemon not running)".into()],
        Some(ListenerStatus::Off) => {
            vec!["Listener: off (Next: remuda cluster listen)".into()]
        }
        Some(ListenerStatus::WaitingForLan(reason)) => vec![
            "Listener: waiting for a private LAN address".into(),
            listener_failure_next_step(&reason).into(),
        ],
        Some(ListenerStatus::On {
            addr,
            auto,
            advertise_addr,
            listen_addrs,
        }) => {
            let addresses = if listen_addrs.is_empty() {
                vec![addr]
            } else {
                listen_addrs.clone()
            };
            let mut lines = vec![format!(
                "Listener: on {} ({})",
                addresses
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
                if auto { "auto" } else { "explicit" }
            )];
            lines.extend(render_listener_addresses(
                addr,
                auto,
                advertise_addr,
                &listen_addrs,
            ));
            lines
        }
        Some(ListenerStatus::Failed(reason)) => vec![
            format!("Listener: failed: {reason}"),
            "Next: remuda cluster listen".into(),
        ],
    }
}

fn cluster_invite(
    server: &str,
    path: &Path,
    bind_addr: Option<std::net::SocketAddr>,
    advertised_addr: Option<std::net::SocketAddr>,
) -> ExitCode {
    with_daemon(server, path, |daemon_path| {
        match remuda_native::cluster::status() {
            Ok(Some(_)) => {}
            Ok(None) => return fail(render_invite_not_initialized()),
            Err(error) => return fail(render_invite_state_error(&error)),
        }
        let listener_status = match bind_addr {
            Some(address) => remuda_native::cluster::listener_control::start(
                daemon_path,
                Some(ListenerConfig {
                    enabled: true,
                    bind: ListenerBind::Explicit(address),
                    allow_public: false,
                }),
            ),
            None => match remuda_native::cluster::listener_control::status(daemon_path) {
                remuda_core::protocol::ListenerStatus::Off => {
                    remuda_native::cluster::listener_control::start(daemon_path, None)
                }
                status => Ok(status),
            },
        };
        let listener_status = match listener_status {
            Ok(status) => status,
            Err(error) => return fail(render_invite_listener_start_error(&error)),
        };
        let bound_addr = match &listener_status {
            remuda_core::protocol::ListenerStatus::On {
                addr,
                advertise_addr,
                ..
            } => advertise_addr.unwrap_or(*addr),
            status => return fail(render_invite_listener_refusal(status)),
        };
        let advertised_addr = advertised_addr.unwrap_or(bound_addr);
        match remuda_native::cluster::mint_join_line(advertised_addr) {
            Ok(line) => match invite_message(&line) {
                Ok(message) => {
                    println!("{message}");
                    ExitCode::SUCCESS
                }
                Err(error) => fail(describe_cluster_error(
                    "invite",
                    &error,
                    ClusterErrorContext::Address(advertised_addr),
                )),
            },
            Err(error) => fail(describe_cluster_error(
                "invite",
                &error,
                ClusterErrorContext::Address(advertised_addr),
            )),
        }
    })
}

fn render_invite_not_initialized() -> &'static str {
    "cluster is not initialized; run `remuda cluster init`"
}

fn render_invite_state_error(error: &std::io::Error) -> String {
    format!("cluster invite: {error}\nNext: remuda cluster init")
}

fn render_invite_listener_start_error(error: &std::io::Error) -> String {
    format!(
        "cluster invite: could not start the listener: {error}\n{}",
        listener_failure_next_step(&error.to_string())
    )
}

fn render_invite_listener_refusal(status: &remuda_core::protocol::ListenerStatus) -> String {
    use remuda_core::protocol::ListenerStatus;

    match status {
        ListenerStatus::Off => {
            "cluster invite: listener is off\nNext: remuda cluster listen".into()
        }
        ListenerStatus::WaitingForLan(reason) => format!(
            "cluster invite: listener is waiting for a private LAN address\n{}",
            listener_failure_next_step(reason)
        ),
        ListenerStatus::Failed(reason) => format!(
            "cluster invite: listener failed: {reason}\n{}",
            listener_failure_next_step(reason)
        ),
        ListenerStatus::On { .. } => unreachable!("an active listener can accept invitations"),
    }
}

fn cluster_join_command(
    server: &str,
    path: &Path,
    fingerprint: Option<&str>,
    invitation: &remuda_native::cluster::join_line::JoinLine,
    bind_addr: Option<std::net::SocketAddr>,
) -> ExitCode {
    let fingerprint = match fingerprint {
        Some(fingerprint) => fingerprint,
        None => {
            if join_confirmation(
                std::io::stdin().is_terminal(),
                std::io::stderr().is_terminal(),
            )
            .is_err()
            {
                eprintln!("remuda: fingerprint confirmation requires stdin and stderr terminals");
                eprint!("{}", cluster_usage("join"));
                eprintln!("Next: use the two-argument join form with an independently supplied fingerprint.");
                return ExitCode::from(2);
            }
            match confirm_join(invitation) {
                Ok(true) => invitation.issuer_fingerprint.as_str(),
                Ok(false) => {
                    eprintln!(
                        "Join cancelled.\nNext: compare with `remuda cluster` on the inviting machine, then run the join command again."
                    );
                    return ExitCode::FAILURE;
                }
                Err(error) => return fail(format!("cluster join confirmation: {error}")),
            }
        }
    };
    if let Err(error) = invitation.verify_pin(fingerprint) {
        return fail(describe_cluster_error(
            "join",
            &error,
            ClusterErrorContext::Join(invitation),
        ));
    }
    with_daemon(server, path, |daemon_path| {
        cluster_join_with_listener(daemon_path, fingerprint, invitation, bind_addr)
    })
}

fn cluster_join_with_listener(
    daemon_path: &Path,
    fingerprint: &str,
    invitation: &remuda_native::cluster::join_line::JoinLine,
    bind_addr: Option<std::net::SocketAddr>,
) -> ExitCode {
    let listener_snapshot = match remuda_native::cluster::listener_config::read() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return fail(format!(
                "cluster join: could not read the saved listener config: {error}"
            ))
        }
    };
    let mut restore_guard = JoinListenerRestoreGuard::new(daemon_path, listener_snapshot.clone());
    // Install before listener start: finish_join_listener_start checks signals after reload
    // and restores the config, so moving this below start could leave an interrupted join unobserved.
    #[cfg(unix)]
    let signal_handler = match JoinInterruptHandler::install() {
        Ok(handler) => handler,
        Err(error) => {
            return fail(describe_cluster_error(
                "join",
                &error,
                ClusterErrorContext::Join(invitation),
            ))
        }
    };
    let listener_status = start_join_listener(
        daemon_path,
        &mut restore_guard,
        listener_snapshot.as_ref(),
        bind_addr,
    );
    let listener_status = match listener_status {
        JoinListenerStartOutcome::Started(listener_status) => listener_status,
        JoinListenerStartOutcome::Cancelled(exit_code) => return exit_code,
    };
    let listener_status = match listener_status {
        Ok(status) => status,
        Err(error) => {
            let rollback_error = restore_join_listener(&mut restore_guard);
            return fail(render_join_failure(
                render_join_listener_start_error(&error),
                rollback_error.as_deref(),
            ));
        }
    };
    let bound_addr = match listener_status {
        remuda_core::protocol::ListenerStatus::On {
            addr,
            advertise_addr,
            ..
        } => advertise_addr.unwrap_or(addr),
        status => {
            let message = render_join_listener_refusal(&status);
            let rollback_error = restore_join_listener(&mut restore_guard);
            return fail(render_join_failure(message, rollback_error.as_deref()));
        }
    };

    let join_fingerprint = fingerprint.to_owned();
    let join_invitation = invitation.clone();
    let join = move || cluster_join(&join_fingerprint, &join_invitation, Some(bound_addr));
    #[cfg(unix)]
    let join_session = run_join_interruptible(signal_handler, join);
    #[cfg(not(unix))]
    let join_session = run_join_interruptible(join);
    let (join_outcome, signal_session) = match join_session {
        Ok(mut session) => (session.take_outcome(), Some(session)),
        Err(error) => (JoinRun::Finished(Err(error)), None),
    };
    let exit_code = match join_outcome {
        #[cfg(unix)]
        JoinRun::Cancelled => cancel_join_with_restore(&mut restore_guard),
        #[cfg(not(unix))]
        JoinRun::Cancelled => ExitCode::from(130),
        JoinRun::Finished(Ok(())) => {
            restore_guard.disarm();
            println!("{}", join_success_message(invitation, fingerprint));
            report_cluster_pushes();
            println!("{}", next_step_join());
            ExitCode::SUCCESS
        }
        JoinRun::Finished(Err(error)) => {
            let message =
                describe_cluster_error("join", &error, ClusterErrorContext::Join(invitation));
            let rollback_error = restore_join_listener(&mut restore_guard);
            fail(render_join_failure(message, rollback_error.as_deref()))
        }
    };
    drop(signal_session);
    exit_code
}

struct JoinListenerRestoreGuard {
    daemon_path: PathBuf,
    snapshot: Option<ListenerConfig>,
    expected: Option<ListenerConfig>,
    expected_off: bool,
    armed: bool,
}

impl JoinListenerRestoreGuard {
    fn new(daemon_path: &Path, snapshot: Option<ListenerConfig>) -> Self {
        let expected_off = snapshot.as_ref().is_none_or(|config| !config.enabled);
        Self {
            daemon_path: daemon_path.to_path_buf(),
            expected: snapshot.clone(),
            snapshot,
            expected_off,
            armed: true,
        }
    }

    fn set_expected(&mut self, expected: Option<ListenerConfig>) {
        self.expected = expected;
    }

    fn restore(
        &mut self,
    ) -> std::io::Result<remuda_native::cluster::listener_control::RestoreOutcome> {
        self.armed = false;
        remuda_native::cluster::listener_control::restore(
            &self.daemon_path,
            self.expected.as_ref(),
            self.snapshot.take(),
        )
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for JoinListenerRestoreGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.restore();
        }
    }
}

fn restore_join_listener(guard: &mut JoinListenerRestoreGuard) -> Option<String> {
    use remuda_native::cluster::listener_control::RestoreOutcome;

    match guard.restore() {
        Ok(RestoreOutcome::Restored(remuda_core::protocol::ListenerStatus::On {
            addr, ..
        })) if guard.expected_off => Some(render_join_listener_unexpected_on(addr)),
        Ok(RestoreOutcome::Restored(_)) => None,
        Ok(RestoreOutcome::SkippedChanged) => Some(render_join_listener_changed_during_join()),
        Err(error) => Some(render_join_listener_restore_error(&error)),
    }
}

#[derive(Debug)]
enum JoinRun {
    Finished(std::io::Result<()>),
    Cancelled,
}

enum JoinListenerStartOutcome {
    Started(std::io::Result<remuda_core::protocol::ListenerStatus>),
    Cancelled(ExitCode),
}

fn start_join_listener(
    daemon_path: &Path,
    restore_guard: &mut JoinListenerRestoreGuard,
    listener_snapshot: Option<&ListenerConfig>,
    bind_addr: Option<std::net::SocketAddr>,
) -> JoinListenerStartOutcome {
    let initial_status = remuda_native::cluster::listener_control::status(daemon_path);
    match bind_addr {
        Some(address) => start_join_listener_with_config(
            daemon_path,
            restore_guard,
            ListenerConfig {
                enabled: true,
                bind: ListenerBind::Explicit(address),
                allow_public: false,
            },
        ),
        None => match initial_status {
            remuda_core::protocol::ListenerStatus::On { .. } => {
                JoinListenerStartOutcome::Started(Ok(initial_status))
            }
            _ => {
                let mut config = listener_snapshot.cloned().unwrap_or(ListenerConfig {
                    enabled: true,
                    bind: ListenerBind::Auto,
                    allow_public: false,
                });
                config.enabled = true;
                start_join_listener_with_config(daemon_path, restore_guard, config)
            }
        },
    }
}

fn start_join_listener_with_config(
    daemon_path: &Path,
    restore_guard: &mut JoinListenerRestoreGuard,
    config: ListenerConfig,
) -> JoinListenerStartOutcome {
    finish_join_listener_start(restore_guard, |restore_guard| {
        remuda_native::cluster::listener_control::start_with_config_written(
            daemon_path,
            Some(config.clone()),
            || restore_guard.set_expected(Some(config)),
        )
    })
}

fn finish_join_listener_start(
    restore_guard: &mut JoinListenerRestoreGuard,
    start: impl FnOnce(
        &mut JoinListenerRestoreGuard,
    ) -> std::io::Result<remuda_core::protocol::ListenerStatus>,
) -> JoinListenerStartOutcome {
    finish_join_listener_start_with_reporter(restore_guard, start, |message| {
        eprintln!("{message}");
    })
}

fn finish_join_listener_start_with_reporter(
    restore_guard: &mut JoinListenerRestoreGuard,
    start: impl FnOnce(
        &mut JoinListenerRestoreGuard,
    ) -> std::io::Result<remuda_core::protocol::ListenerStatus>,
    report: impl FnMut(&str),
) -> JoinListenerStartOutcome {
    let result = start(restore_guard);
    #[cfg(unix)]
    {
        let mut report = report;
        if JOIN_INTERRUPTED.load(Ordering::Acquire) {
            if let Err(error) = &result {
                report(&format!(
                    "\x1b[31m{}\x1b[0m",
                    format_failure(&render_join_listener_start_error(error))
                ));
            }
            return JoinListenerStartOutcome::Cancelled(cancel_join_with_restore_with_reporter(
                restore_guard,
                &mut report,
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = report;
    JoinListenerStartOutcome::Started(result)
}

#[cfg(unix)]
fn cancel_join_with_restore(restore_guard: &mut JoinListenerRestoreGuard) -> ExitCode {
    cancel_join_with_restore_with_reporter(restore_guard, |message| eprintln!("{message}"))
}

#[cfg(unix)]
fn cancel_join_with_restore_with_reporter(
    restore_guard: &mut JoinListenerRestoreGuard,
    mut report: impl FnMut(&str),
) -> ExitCode {
    let restore_warning = restore_join_listener(restore_guard);
    report("Join cancelled.\nNext: compare with `remuda cluster` on the inviting machine, then run the join command again.");
    if let Some(warning) = restore_warning {
        report(&warning);
    }
    ExitCode::from((128 + JOIN_INTERRUPT_SIGNAL.load(Ordering::Acquire)) as u8)
}

struct JoinRunSession {
    outcome: Option<JoinRun>,
    #[cfg(unix)]
    handler: JoinInterruptHandler,
}

impl JoinRunSession {
    fn take_outcome(&mut self) -> JoinRun {
        #[cfg(unix)]
        let _ = &self.handler;
        self.outcome.take().expect("join session has an outcome")
    }
}

#[cfg(unix)]
fn run_join_interruptible(
    handler: JoinInterruptHandler,
    join: impl FnOnce() -> std::io::Result<()> + Send + 'static,
) -> std::io::Result<JoinRunSession> {
    Ok(JoinRunSession {
        outcome: Some(wait_for_join(&JOIN_INTERRUPTED, join)),
        handler,
    })
}

#[cfg(not(unix))]
// ponytail: Windows has no console Ctrl-C handler here, so an interrupted join
// can exit without restoring its listener config.
fn run_join_interruptible(
    join: impl FnOnce() -> std::io::Result<()> + Send + 'static,
) -> std::io::Result<JoinRunSession> {
    Ok(JoinRunSession {
        outcome: Some(JoinRun::Finished(join())),
    })
}

#[cfg(unix)]
struct JoinInterruptHandler {
    previous: Vec<(libc::c_int, libc::sighandler_t)>,
}

#[cfg(unix)]
impl JoinInterruptHandler {
    fn install() -> std::io::Result<Self> {
        JOIN_INTERRUPT_SIGNAL.store(0, Ordering::Relaxed);
        JOIN_INTERRUPTED.store(false, Ordering::Relaxed);
        let mut previous = Vec::with_capacity(3);
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            // SAFETY: install the flag-only handler for this signal, retaining its prior action.
            let prior = unsafe {
                libc::signal(
                    signal,
                    record_join_interrupt as *const () as libc::sighandler_t,
                )
            };
            if prior == libc::SIG_ERR {
                let error = std::io::Error::last_os_error();
                for (installed_signal, old_handler) in previous.drain(..).rev() {
                    // SAFETY: restore the disposition returned by the earlier signal call.
                    unsafe {
                        libc::signal(installed_signal, old_handler);
                    }
                }
                return Err(error);
            }
            if prior == libc::SIG_IGN {
                // Preserve signals ignored by the caller (for example SIGHUP under nohup).
                if unsafe { libc::signal(signal, libc::SIG_IGN) } == libc::SIG_ERR {
                    let error = std::io::Error::last_os_error();
                    for (installed_signal, old_handler) in previous.drain(..).rev() {
                        // SAFETY: restore the disposition returned by the earlier signal call.
                        unsafe {
                            libc::signal(installed_signal, old_handler);
                        }
                    }
                    return Err(error);
                }
                continue;
            }
            previous.push((signal, prior));
        }
        Ok(Self { previous })
    }
}

#[cfg(unix)]
impl Drop for JoinInterruptHandler {
    fn drop(&mut self) {
        for (signal, previous) in self.previous.drain(..).rev() {
            // SAFETY: restore the disposition returned when this handler was installed.
            unsafe {
                libc::signal(signal, previous);
            }
        }
    }
}

#[cfg(unix)]
fn wait_for_join(
    cancelled: &AtomicBool,
    join: impl FnOnce() -> std::io::Result<()> + Send + 'static,
) -> JoinRun {
    if cancelled.load(Ordering::Acquire) {
        return JoinRun::Cancelled;
    }
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let _ = sender.send(join());
    });
    loop {
        if cancelled.load(Ordering::Acquire) {
            return JoinRun::Cancelled;
        }
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(result) => return JoinRun::Finished(result),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => match worker.join() {
                Err(panic) => std::panic::resume_unwind(panic),
                Ok(()) => {
                    return JoinRun::Finished(Err(std::io::Error::other(
                        "join worker ended without a result",
                    )))
                }
            },
        }
    }
}

fn render_join_failure(message: String, rollback_error: Option<&str>) -> String {
    match rollback_error {
        Some(rollback_error) => format!("{message}\n{rollback_error}"),
        None => message,
    }
}

fn render_join_listener_start_error(error: &std::io::Error) -> String {
    format!(
        "cluster join: could not start the listener: {error}\n{}",
        listener_failure_next_step(&error.to_string())
    )
}

fn render_join_listener_refusal(status: &remuda_core::protocol::ListenerStatus) -> String {
    use remuda_core::protocol::ListenerStatus;

    match status {
        ListenerStatus::Off => "cluster join: listener is off\nNext: remuda cluster listen".into(),
        ListenerStatus::WaitingForLan(reason) => format!(
            "cluster join: listener is waiting for a private LAN address\n{}",
            listener_failure_next_step(reason)
        ),
        ListenerStatus::Failed(reason) => format!(
            "cluster join: listener failed: {reason}\n{}",
            listener_failure_next_step(reason)
        ),
        ListenerStatus::On { .. } => unreachable!("an active listener can accept joins"),
    }
}

fn render_join_listener_restore_error(error: &std::io::Error) -> String {
    format!("cluster join: could not restore the saved listener config: {error}\nNext: check `remuda cluster` and run `remuda cluster listen --off` if the listener should be disabled.")
}

fn render_join_listener_unexpected_on(addr: std::net::SocketAddr) -> String {
    format!("cluster join: listener remains on at {addr}\nNext: run `remuda cluster listen --off` and check `remuda cluster`.")
}

fn render_join_listener_changed_during_join() -> String {
    "cluster join: listener config changed during join; leaving the current config unchanged. Next: check `remuda cluster`.".into()
}

#[derive(Clone, Copy)]
enum ClusterErrorContext<'a> {
    Address(std::net::SocketAddr),
    Join(&'a remuda_native::cluster::join_line::JoinLine),
}

impl ClusterErrorContext<'_> {
    fn address(self) -> std::net::SocketAddr {
        match self {
            Self::Address(address) => address,
            Self::Join(invitation) => invitation.issuer_addr,
        }
    }
}

fn describe_cluster_error(
    verb: &str,
    error: &std::io::Error,
    context: ClusterErrorContext<'_>,
) -> String {
    let kind = error.kind();
    let detail = error.to_string();
    let detail = match context {
        ClusterErrorContext::Join(invitation) => {
            detail.replace(invitation.token.as_str(), "[redacted]")
        }
        ClusterErrorContext::Address(_) => detail,
    };
    match verb {
        "join" => {
            let address = context.address();
            let reason = match kind {
                std::io::ErrorKind::ConnectionRefused => Some("connection refused"),
                std::io::ErrorKind::TimedOut => Some("connection timed out"),
                _ => None,
            };
            if let Some(reason) = reason {
                return format!(
                    "cluster join: cannot reach {address} ({reason}).\nNext: on the inviting machine, check `remuda cluster listen` is running on that address."
                );
            }
            if kind == std::io::ErrorKind::PermissionDenied && detail == "join was refused" {
                return "cluster join: the invitation was refused (join lines work once and expire after 10 minutes).\nNext: run `remuda cluster invite` on the inviting machine again.".into();
            }
            if kind == std::io::ErrorKind::PermissionDenied
                && detail.starts_with("issuer fingerprint mismatch:")
            {
                let ClusterErrorContext::Join(invitation) = context else {
                    return "cluster join: issuer fingerprint mismatch.\nNext: ask the inviting machine to run `remuda cluster` and read its Fingerprint line.".into();
                };
                let mismatch = describe_fingerprint_mismatch(&detail, invitation);
                return format!(
                    "cluster join: {mismatch}.\nNext: ask the inviting machine to run `remuda cluster` and read its Fingerprint line."
                );
            }
            if kind == std::io::ErrorKind::PermissionDenied && error.raw_os_error() == Some(1) {
                return format!(
                    "cluster join: cannot reach {address} (the OS blocked the connection).\nNext: the OS blocked the connection; allow remuda network access (macOS: System Settings > Privacy & Security > Local Network), then retry the same join command."
                );
            }
            format!("cluster join: {detail}.\nNext: check the invitation and try again.")
        }
        "listen" => {
            let address = context.address();
            if kind == std::io::ErrorKind::PermissionDenied
                && detail == "wildcard listener bind requires explicit public-bind opt-in"
                && address.ip().is_unspecified()
            {
                return "cluster listener: binding all interfaces needs --allow-public (or use this machine's LAN IP).\nNext: add --allow-public or bind this machine's LAN IP.".into();
            }
            if kind == std::io::ErrorKind::AddrNotAvailable {
                return format!(
                    "cluster listener: {} is not an address of this machine.\nNext: run `remuda cluster listen` to use this machine's detected private address.",
                    address.ip()
                );
            }
            format!("cluster listener: {detail}.\nNext: check the bind address and run `remuda cluster listen` again.")
        }
        "invite" => {
            let address = context.address();
            if kind == std::io::ErrorKind::InvalidData
                && detail == "invalid cluster join line"
                && address.ip().is_unspecified()
            {
                return "cluster invite: use an address of this machine that the other machine can reach, e.g. --bind 192.168.1.20:7441.\nNext: rerun `remuda cluster invite` with that address.".into();
            }
            format!("cluster invite: {detail}.\nNext: check the bind address and run `remuda cluster invite` again.")
        }
        _ => format!("cluster {verb}: operation failed.\nNext: run `remuda help` for commands."),
    }
}

fn describe_fingerprint_mismatch(
    detail: &str,
    invitation: &remuda_native::cluster::join_line::JoinLine,
) -> String {
    let expected = detail
        .strip_prefix("issuer fingerprint mismatch: expected ")
        .and_then(|rest| rest.split_once(", received ").map(|(expected, _)| expected))
        .unwrap_or("the supplied fingerprint");
    let token = invitation.token.as_str();
    let expected = safe_fingerprint_field(expected, token);
    let received = safe_fingerprint_field(
        &remuda_native::cluster::encoding::fingerprint(&invitation.issuer_static_pubkey),
        token,
    );
    let declared = safe_fingerprint_field(&invitation.issuer_fingerprint, token);
    format!(
        "issuer fingerprint mismatch: expected {expected}, received {received} (invitation declares {declared})"
    )
}

fn safe_fingerprint_field(value: &str, token: &str) -> String {
    if value.starts_with("SHA256:")
        && value.len() <= 64
        && value.is_ascii()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=' | b':'))
        && !value.contains(token)
    {
        value.to_owned()
    } else {
        "the supplied fingerprint".into()
    }
}

fn cluster_remote(server: &str, path: &Path, node: &str, target: Option<&str>) -> ExitCode {
    match remuda_native::cluster::nodes() {
        Ok(Some((identity, registry))) => {
            let poller = match remuda_native::cluster_remote::RemotePoller::from_registry(
                &registry,
                &identity.node_fp,
            ) {
                Ok(poller) => poller,
                Err(error) => return fail(format!("cluster remote: {error}")),
            };
            if let Err(error) = poller.start() {
                return fail(format!("cluster remote: {error}"));
            }
            let source = poller.source();
            let selection = poller.selection();
            with_daemon(server, path, |path| {
                match remuda_native::cluster_tui::run_with_remote_selection(
                    path,
                    node,
                    target,
                    source.as_ref(),
                    &selection,
                ) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(error) => fail(format!("cluster remote: {error}")),
                }
            })
        }
        Ok(None) => with_daemon(server, path, |path| {
            match remuda_native::cluster_tui::run(path, node, target) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => fail(format!("cluster remote: {error}")),
            }
        }),
        Err(error) => fail(format!("cluster remote: {error}")),
    }
}

fn remote_control_label(enabled: bool) -> &'static str {
    if enabled {
        "enabled"
    } else {
        "disabled"
    }
}

fn remote_control_status_lines(enabled: bool) -> (&'static str, &'static str) {
    let setting = if enabled {
        "Remote control: enabled"
    } else {
        "Remote control: disabled"
    };
    (
        setting,
        "Trust if enabled: a compromised admitted node can type into and close every session.",
    )
}

fn cluster_call(
    target: &str,
    address: std::net::SocketAddr,
    action: CallAction,
    json: bool,
) -> ExitCode {
    use remuda_core::protocol::{expand_runs, Response};
    use remuda_native::net::cluster_client::{ClientError, ClusterClient};
    let resolved = match remuda_native::cluster::resolve_target(target, Some(address)) {
        Ok(resolved) => resolved,
        Err(error) => {
            let code = match error.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied => 4,
                std::io::ErrorKind::InvalidInput => 2,
                std::io::ErrorKind::InvalidData => 5,
                _ => 1,
            };
            eprintln!("cluster call: {error}");
            return ExitCode::from(code);
        }
    };
    let private_key = match remuda_native::cluster::identity::load_static_private_key() {
        Ok(key) => key,
        Err(error) => return fail(format!("cluster call: {error}")),
    };
    let request = match &action {
        CallAction::List => Request::List,
        CallAction::Capture(session) => Request::CaptureStyled {
            name: session.clone(),
            scrollback: 0,
        },
    };
    let response = match ClusterClient::system().request(
        resolved.address,
        &resolved.pinned_static_key,
        &private_key,
        &request,
    ) {
        Ok(response) => response,
        Err(error) => {
            let code = match error {
                ClientError::Unreachable
                | ClientError::PeerNotListening
                | ClientError::PeerClosedConnection
                | ClientError::OsBlockedConnection
                | ClientError::Timeout => 3,
                ClientError::Refused(_) => 4,
                ClientError::Crypto | ClientError::BadResponse => 5,
            };
            eprintln!("cluster call: {error}");
            return ExitCode::from(code);
        }
    };
    let response = sanitize_peer_response(response);
    if let Response::Error(message) = &response {
        eprintln!("cluster call: {message}");
        return ExitCode::from(4);
    }
    if json {
        return match serde_json::to_string_pretty(&response) {
            Ok(value) => {
                println!("{value}");
                ExitCode::SUCCESS
            }
            Err(error) => fail(format!("cluster call: {error}")),
        };
    }
    match (&action, response) {
        (CallAction::List, Response::Sessions(sessions)) => {
            for session in sessions {
                println!(
                    "{}\t{}",
                    session.name,
                    if session.alive { "live" } else { "ended" }
                );
            }
            ExitCode::SUCCESS
        }
        (CallAction::Capture(_), Response::StyledScreen { rows, .. }) => {
            for row in rows {
                let cells = expand_runs(&row);
                println!(
                    "{}",
                    cells
                        .iter()
                        .map(|cell| cell.text.as_str())
                        .collect::<String>()
                );
            }
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("cluster call: peer returned an unexpected response");
            ExitCode::from(5)
        }
    }
}

fn sanitize_peer_response(
    mut response: remuda_core::protocol::Response,
) -> remuda_core::protocol::Response {
    use remuda_core::protocol::Response;

    fn strip(text: &mut String) {
        *text = remuda_native::text::strip_terminal_controls(text).into_owned();
    }

    match &mut response {
        Response::Sessions(sessions) => {
            for session in sessions {
                strip(&mut session.id);
                strip(&mut session.name);
                if let Some(instance_id) = &mut session.instance_id {
                    strip(instance_id);
                }
            }
        }
        Response::Screen(text) | Response::Value(text) | Response::Error(text) => strip(text),
        Response::StyledScreen { rows, .. } => {
            for run in rows.iter_mut().flatten() {
                strip(&mut run.text);
            }
        }
        Response::CommandResult {
            stdout_base64,
            stderr_base64,
            ..
        } => {
            strip(stdout_base64);
            strip(stderr_base64);
        }
        Response::Entries(entries) => {
            for entry in entries {
                strip(entry);
            }
        }
        _ => {}
    }
    response
}

fn cluster_revoke(target: &str, yes: bool) -> ExitCode {
    let (identity, registry) = match remuda_native::cluster::nodes() {
        Ok(Some(nodes)) => nodes,
        Ok(None) => return fail("cluster is not initialized; run `remuda cluster init`"),
        Err(error) => return fail(format!("cluster nodes: {error}")),
    };
    let entry = match remuda_native::cluster::resolve_node(&registry.authorized_nodes, target) {
        Ok(entry) => entry,
        Err(error) => return fail(format!("cluster revoke: {error}")),
    };
    if entry.node_fp == identity.node_fp {
        return fail("cluster revoke: cannot revoke self");
    }
    if entry.state == remuda_native::cluster::NodeState::Revoked {
        println!(
            "Node {} is already revoked.",
            remuda_native::cluster::node_label(&entry.node_fp)
        );
        return ExitCode::SUCCESS;
    }
    let fingerprint = entry.node_fp.clone();
    let label = remuda_native::cluster::node_label(&entry.node_fp);
    let prompt = match revoke_confirmation(
        yes,
        std::io::stdin().is_terminal(),
        std::io::stderr().is_terminal(),
    ) {
        Ok(prompt) => prompt,
        Err(message) => return fail(message),
    };
    match confirm_revoke(&label, &fingerprint, prompt) {
        Ok(false) => {
            println!("Revocation cancelled.");
            return ExitCode::SUCCESS;
        }
        Ok(true) => {}
        Err(error) => return fail(format!("cluster revoke: {error}")),
    }
    match remuda_native::cluster::revoke_local(&fingerprint) {
        Ok(remuda_native::cluster::RevokeOutcome::Revoked) => {
            println!("Node {label} revoked locally.");
            report_revocation_pushes(&fingerprint);
            ExitCode::SUCCESS
        }
        Ok(remuda_native::cluster::RevokeOutcome::AlreadyRevoked) => {
            println!("Node {label} is already revoked.");
            ExitCode::SUCCESS
        }
        Err(error) => fail(format!("cluster revoke: {error}")),
    }
}

fn report_cluster_pushes() {
    report_cluster_push_results(remuda_native::cluster::push_now());
}

fn report_revocation_pushes(revoked_peer: &str) {
    report_cluster_push_results(remuda_native::cluster::push_now_with_revoked_target(
        revoked_peer,
    ));
}

fn report_cluster_push_results(peers: Vec<remuda_native::cluster::PeerPushResult>) {
    if peers.is_empty() {
        println!("Registry push: no configured peers.");
        return;
    }
    let needs_retry = peers.iter().any(|peer| !peer.reached);
    for peer in peers {
        if peer.reached {
            println!("Registry push reached peer {}.", peer.peer_fp);
        } else {
            eprintln!(
                "Registry push did not reach peer {}: {}.",
                peer.peer_fp, peer.detail
            );
        }
    }
    if needs_retry {
        // Requeue failed synchronous pushes so anti-entropy can retry sooner.
        remuda_native::cluster::registry_changed();
    }
}

fn cluster_join(
    shown_fingerprint: &str,
    invitation: &remuda_native::cluster::join_line::JoinLine,
    bind_addr: Option<std::net::SocketAddr>,
) -> std::io::Result<()> {
    cluster_join_with_private_loader(shown_fingerprint, invitation, bind_addr, || {
        remuda_native::cluster::identity::load_static_private_key()
    })
}

fn cluster_join_with_private_loader(
    shown_fingerprint: &str,
    invitation: &remuda_native::cluster::join_line::JoinLine,
    bind_addr: Option<std::net::SocketAddr>,
    load_private: impl FnOnce() -> std::io::Result<zeroize::Zeroizing<Vec<u8>>>,
) -> std::io::Result<()> {
    invitation.verify_pin(shown_fingerprint)?;
    let private = load_private()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(std::io::Error::other)?
        .as_secs() as i64;
    remuda_native::net::join::join(invitation, &private, now, bind_addr)
}

fn revoke_confirmation(yes: bool, stdin_tty: bool, stderr_tty: bool) -> Result<bool, &'static str> {
    if yes {
        Ok(false)
    } else if stdin_tty && stderr_tty {
        Ok(true)
    } else {
        Err("use --yes to confirm non-interactively")
    }
}

fn new_identity_confirmation(
    yes: bool,
    stdin_tty: bool,
    stderr_tty: bool,
) -> Result<bool, &'static str> {
    if yes {
        Ok(false)
    } else if stdin_tty && stderr_tty {
        Ok(true)
    } else {
        Err("use --yes to confirm non-interactively")
    }
}

fn confirm_new_identity() -> std::io::Result<bool> {
    use std::io::{self, Write};
    let mut stderr = io::stderr().lock();
    write!(stderr, "Create a new cluster identity? [y/N] ")?;
    stderr.flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(confirmation_answer_is_yes(&answer))
}

pub fn join_confirmation(stdin_tty: bool, stderr_tty: bool) -> Result<bool, &'static str> {
    if stdin_tty && stderr_tty {
        Ok(true)
    } else {
        Err("fingerprint confirmation requires stdin and stderr terminals")
    }
}

pub fn join_prompt(invitation: &remuda_native::cluster::join_line::JoinLine) -> String {
    let node = remuda_native::cluster::node_label(&invitation.issuer_fingerprint);
    format!(
        "Joining {node} at {}\nFingerprint: {}\nCheck the inviting machine shows this fingerprint (run remuda cluster there). Continue? [y/N]",
        invitation.issuer_addr, invitation.issuer_fingerprint
    )
}

pub fn join_success_message(
    invitation: &remuda_native::cluster::join_line::JoinLine,
    pinned_fingerprint: &str,
) -> String {
    let node = remuda_native::cluster::node_label(&invitation.issuer_fingerprint);
    format!("Joined {node} (fingerprint {pinned_fingerprint}).")
}

fn confirm_join(invitation: &remuda_native::cluster::join_line::JoinLine) -> std::io::Result<bool> {
    use std::io::{self, Write};
    let mut stderr = io::stderr().lock();
    write!(stderr, "{} ", join_prompt(invitation))?;
    stderr.flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(confirmation_answer_is_yes(&answer))
}

fn confirm_revoke(label: &str, fingerprint: &str, prompt: bool) -> std::io::Result<bool> {
    use std::io::{self, Write};
    if !prompt {
        return Ok(true);
    }
    let mut stderr = io::stderr().lock();
    write!(stderr, "Revoke node {label} ({fingerprint})? [y/N] ")?;
    stderr.flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(confirmation_answer_is_yes(&answer))
}

fn confirmation_answer_is_yes(answer: &str) -> bool {
    answer.trim().eq_ignore_ascii_case("y") || answer.trim().eq_ignore_ascii_case("yes")
}

fn write_nodes_table<W: Write>(
    writer: &mut W,
    identity: &remuda_native::cluster::NodeIdentity,
    registry: &remuda_native::cluster::Registry,
) -> std::io::Result<()> {
    let table = remuda_native::cluster::format_nodes_table(identity, registry);
    match writer.write_all(table.as_bytes()) {
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error),
        Ok(()) => match remuda_native::cluster::control::revoked_notice()? {
            Some(notice) => write_revocation_notice(writer, &notice),
            None => Ok(()),
        },
    }
}

fn write_revocation_notice<W: Write>(
    writer: &mut W,
    notice: &remuda_native::cluster::control::RevokedNotice,
) -> std::io::Result<()> {
    let text = format!(
        "This node was revoked by {} at {}.\nNext: run `remuda cluster init --new-identity`, then ask an admitted machine for a new invite.\n",
        remuda_native::cluster::node_label(&notice.by_fp),
        format_revocation_time(&notice.at)
    );
    match writer.write_all(text.as_bytes()) {
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}

#[cfg(unix)]
fn format_revocation_time(timestamp: &str) -> String {
    let Ok(seconds) = timestamp.parse::<libc::time_t>() else {
        return "unknown local time".into();
    };
    let format = b"%Y-%m-%d %H:%M %Z\0";
    let mut buffer = [0 as libc::c_char; 64];
    // SAFETY: `local` is a valid zero-initialized C tm output struct,
    // `seconds` points to a valid time_t, and `buffer` and `format` meet the
    // writable-buffer and NUL-terminated-format requirements of these calls.
    let written = unsafe {
        let mut local = std::mem::zeroed::<libc::tm>();
        if libc::localtime_r(&seconds, &mut local).is_null() {
            return "unknown local time".into();
        }
        libc::strftime(
            buffer.as_mut_ptr(),
            buffer.len(),
            format.as_ptr().cast(),
            &local,
        )
    };
    if written == 0 {
        return "unknown local time".into();
    }
    let bytes = buffer[..written]
        .iter()
        .map(|byte| *byte as u8)
        .collect::<Vec<_>>();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(windows)]
fn format_revocation_time(timestamp: &str) -> String {
    // Windows falls back to UTC because native local-time formatting is not
    // available here without adding a dependency.
    format_utc_revocation_time(timestamp)
}

#[cfg(any(windows, test))]
fn format_utc_revocation_time(timestamp: &str) -> String {
    let Ok(seconds) = timestamp.parse::<i64>() else {
        return "unknown UTC time".into();
    };
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        day_seconds / 3_600,
        (day_seconds % 3_600) / 60
    )
}

#[cfg(any(windows, test))]
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month, day)
}

fn cluster_init_message(created: bool) -> &'static str {
    if created {
        "Cluster initialized"
    } else {
        "Already initialized"
    }
}

fn invite_message(line: &remuda_native::cluster::join_line::JoinLine) -> std::io::Result<String> {
    let encoded = line.encode()?;
    Ok(format!(
        "Invitation for one machine, valid 10 minutes. Run this on the other machine:\n\n  remuda cluster join '{}' '{}'\n\nFingerprint of this machine: {} (the other machine must show the same one)\n\nNext: after it joins, run `remuda cluster nodes` here to see it.",
        line.issuer_fingerprint,
        encoded,
        line.issuer_fingerprint
    ))
}

fn next_step_init() -> &'static str {
    "Next: remuda cluster invite (on this machine), or join an existing cluster with the command another machine's invite prints."
}

fn next_step_join() -> &'static str {
    "Next: remuda cluster remote"
}

fn next_step_status(members: usize) -> Option<&'static str> {
    (members == 1).then_some("Next: remuda cluster invite")
}

fn next_step_listen() -> &'static str {
    "Keep this running; open another terminal for invite/join."
}

fn acquire_foreground_listener_host_lock() -> Result<std::fs::File, String> {
    match remuda_native::cluster::try_acquire_listener_host_lock() {
        Ok(lock) => Ok(lock),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            Err(foreground_listener_host_error().into())
        }
        Err(error) => Err(format!("cluster listener lock: {error}")),
    }
}

fn foreground_listener_host_error() -> &'static str {
    "the cluster listener already runs in the remuda daemon (Next: remuda cluster to see it)"
}

#[cfg(test)]
#[allow(clippy::disallowed_types)]
mod cluster_cli_tests {
    #[cfg(unix)]
    use super::acquire_foreground_listener_host_lock;
    #[cfg(unix)]
    use super::cluster_join_with_private_loader;
    use super::{
        cluster_init_message, cluster_listener_status_lines, cluster_usage,
        confirmation_answer_is_yes, invite_message, is_public_listener_address, join_confirmation,
        join_prompt, join_success_message, new_identity_confirmation, next_step_init,
        next_step_join, next_step_listen, next_step_status, parse_addr_default_port,
        parse_cluster_command, remote_control_status_lines, render_cluster_init_lines,
        render_init_listener_lines, render_invite_listener_refusal, render_join_failure,
        render_join_listener_restore_error, render_join_listener_unexpected_on,
        revoke_confirmation, write_nodes_table, write_revocation_notice, ClusterCommand,
        NEW_IDENTITY_WARNING,
    };
    #[cfg(unix)]
    use super::{
        finish_join_listener_start, finish_join_listener_start_with_reporter,
        restore_join_listener, start_join_listener_with_config, wait_for_join, ExitCode,
        JoinInterruptHandler, JoinListenerRestoreGuard, JoinListenerStartOutcome, JoinRun,
        JOIN_INTERRUPTED, JOIN_INTERRUPT_SIGNAL,
    };
    #[cfg(unix)]
    use remuda_native::cluster::join_line::JoinLine;
    #[cfg(unix)]
    use remuda_native::cluster::listener_config::{ListenerBind, ListenerConfig};
    #[cfg(unix)]
    use std::io::{Read, Write};
    #[cfg(unix)]
    use std::net::TcpListener;
    #[cfg(unix)]
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[cfg(unix)]
    use std::sync::Arc;
    #[cfg(unix)]
    use std::sync::Mutex;
    #[cfg(unix)]
    use std::sync::MutexGuard;
    #[cfg(unix)]
    use std::time::{Duration, Instant};
    #[cfg(unix)]
    use zeroize::Zeroizing;

    #[cfg(unix)]
    static FOREGROUND_LISTENER_LOCK_TEST: Mutex<()> = Mutex::new(());

    #[cfg(unix)]
    static JOIN_SIGNAL_TEST: Mutex<()> = Mutex::new(());

    #[cfg(unix)]
    struct ForegroundListenerLockEnvironment {
        _lock: MutexGuard<'static, ()>,
        root: std::path::PathBuf,
        old_home: Option<std::ffi::OsString>,
        old_state: Option<std::ffi::OsString>,
    }

    #[cfg(unix)]
    impl ForegroundListenerLockEnvironment {
        fn new() -> Self {
            let lock = FOREGROUND_LISTENER_LOCK_TEST
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let root = std::env::temp_dir().join(format!(
                "remuda-cli-listener-lock-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&root).unwrap();
            let home = root.join("home");
            let state = root.join("state");
            std::fs::create_dir(&home).unwrap();
            std::fs::create_dir(&state).unwrap();
            let environment = Self {
                _lock: lock,
                root,
                old_home: std::env::var_os("HOME"),
                old_state: std::env::var_os("XDG_STATE_HOME"),
            };
            std::env::set_var("HOME", &home);
            std::env::set_var("XDG_STATE_HOME", &state);
            remuda_native::cluster::init().unwrap();
            environment
        }
    }

    #[cfg(unix)]
    impl Drop for ForegroundListenerLockEnvironment {
        fn drop(&mut self) {
            match self.old_home.take() {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match self.old_state.take() {
                Some(value) => std::env::set_var("XDG_STATE_HOME", value),
                None => std::env::remove_var("XDG_STATE_HOME"),
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn invalid_reason(args: &[&str]) -> (String, String) {
        match parse_cluster_command(args) {
            ClusterCommand::Invalid { verb, reason } => (verb, reason),
            command => panic!("expected an invalid command, got {command:?}"),
        }
    }

    #[test]
    fn cluster_status_and_init_are_recognized() {
        assert_eq!(parse_cluster_command(&[]), ClusterCommand::Status);
        assert_eq!(
            parse_cluster_command(&["init"]),
            ClusterCommand::Init { no_listen: false }
        );
        assert_eq!(
            parse_cluster_command(&["init", "--no-listen"]),
            ClusterCommand::Init { no_listen: true }
        );
        assert_eq!(invalid_reason(&["join"]).0, "join");
        assert_eq!(
            parse_cluster_command(&["invite", "--bind", "192.0.2.4:9443"]),
            ClusterCommand::Invite {
                bind_addr: Some("192.0.2.4:9443".parse().unwrap()),
                advertised_addr: None,
            }
        );
    }

    #[test]
    fn cluster_invite_advertised_address_is_recognized() {
        assert_eq!(
            parse_cluster_command(&["invite", "--addr=203.0.113.9:9443"]),
            ClusterCommand::Invite {
                bind_addr: None,
                advertised_addr: Some("203.0.113.9:9443".parse().unwrap()),
            }
        );
        assert_eq!(
            parse_cluster_command(&[
                "invite",
                "--bind",
                "192.0.2.4:7441",
                "--addr",
                "203.0.113.9:9443"
            ]),
            ClusterCommand::Invite {
                bind_addr: Some("192.0.2.4:7441".parse().unwrap()),
                advertised_addr: Some("203.0.113.9:9443".parse().unwrap()),
            }
        );
    }

    #[test]
    fn init_and_invite_listener_rendering_is_actionable_without_join_tokens() {
        use remuda_core::protocol::ListenerStatus;

        let listening = render_cluster_init_lines(
            true,
            "node-a",
            "fingerprint-a",
            &ListenerStatus::On {
                addr: "192.0.2.4:7441".parse().unwrap(),
                auto: true,
                advertise_addr: Some("192.0.2.4:7441".parse().unwrap()),
                listen_addrs: vec!["192.0.2.4:7441".parse().unwrap()],
            },
        );
        assert!(listening
            .iter()
            .any(|line| { line == "Listening on 192.0.2.4:7441" }));
        assert!(listening.iter().any(|line| {
            line == "Only admitted machines can connect; turn off: remuda cluster listen --off"
        }));
        assert!(listening.iter().any(|line| line.starts_with("Next:")));

        let failed = render_init_listener_lines(&ListenerStatus::Failed("address busy".into()));
        assert!(failed
            .iter()
            .any(|line| line == "Listener failed: address busy"));
        assert!(failed
            .iter()
            .any(|line| line == "Next: remuda cluster listen"));

        let refusal =
            render_invite_listener_refusal(&ListenerStatus::Failed("address busy".into()));
        assert!(refusal.contains("address busy"));
        assert!(refusal.contains("remuda cluster listen"));
        assert!(!refusal.contains("remuda-join-v1"));
    }

    #[test]
    fn no_lan_listener_refusals_suggest_an_explicit_bind() {
        use remuda_core::protocol::ListenerStatus;

        let reason = "no private LAN address found. Next: remuda cluster listen --bind IP";
        let failed = ListenerStatus::Failed(reason.into());
        for message in [
            render_init_listener_lines(&failed).join("\n"),
            render_invite_listener_refusal(&failed),
            super::render_join_listener_refusal(&failed),
        ] {
            assert!(
                message.ends_with("Next: remuda cluster listen --bind IP"),
                "missing explicit bind recovery in: {message}"
            );
        }
    }

    #[test]
    fn join_rollback_rendering_reports_uncertain_listener_state() {
        let restore_failed =
            render_join_listener_restore_error(&std::io::Error::other("disk is full"));
        assert!(
            restore_failed.contains("could not restore the saved listener config: disk is full")
        );
        assert!(restore_failed.contains("remuda cluster listen --off"));

        assert_eq!(
            render_join_failure("join failed".into(), Some("rollback failed")),
            "join failed\nrollback failed"
        );
    }

    #[test]
    fn join_restore_warns_if_an_off_snapshot_is_still_on() {
        let warning = render_join_listener_unexpected_on("127.0.0.1:7441".parse().unwrap());
        assert!(warning.contains("listener remains on at 127.0.0.1:7441"));
        assert!(warning.contains("Next: run `remuda cluster listen --off`"));
    }

    #[cfg(unix)]
    #[test]
    fn join_listener_drop_guard_restores_during_unwind() {
        let environment = ForegroundListenerLockEnvironment::new();
        let join_config = ListenerConfig {
            enabled: true,
            bind: ListenerBind::Auto,
            allow_public: false,
        };
        remuda_native::cluster::listener_config::write(&join_config).unwrap();
        let daemon_path = environment.root.join("missing-daemon.sock");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut guard = JoinListenerRestoreGuard::new(&daemon_path, None);
            guard.set_expected(Some(join_config));
            panic!("simulate join panic");
        }));
        assert!(result.is_err());
        assert_eq!(
            remuda_native::cluster::listener_config::read().unwrap(),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn join_listener_write_failure_does_not_report_a_concurrent_change() {
        let _serial = JOIN_SIGNAL_TEST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        struct ResetInterruptState;
        impl Drop for ResetInterruptState {
            fn drop(&mut self) {
                JOIN_INTERRUPTED.store(false, Ordering::Relaxed);
                JOIN_INTERRUPT_SIGNAL.store(0, Ordering::Relaxed);
            }
        }
        let _reset = ResetInterruptState;
        JOIN_INTERRUPTED.store(false, Ordering::Relaxed);
        JOIN_INTERRUPT_SIGNAL.store(0, Ordering::Relaxed);

        let _environment = ForegroundListenerLockEnvironment::new();
        let cluster_dir = _environment.root.join("state/remuda/cluster");
        let snapshot = ListenerConfig {
            enabled: true,
            bind: ListenerBind::Explicit("127.0.0.1:7441".parse().unwrap()),
            allow_public: false,
        };
        let join_config = ListenerConfig {
            enabled: true,
            bind: ListenerBind::Explicit("127.0.0.2:7441".parse().unwrap()),
            allow_public: false,
        };
        remuda_native::cluster::listener_config::write(&snapshot).unwrap();
        let daemon_path = std::path::Path::new("missing.sock");
        let mut restore_guard = JoinListenerRestoreGuard::new(daemon_path, Some(snapshot.clone()));

        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cluster_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let start = start_join_listener_with_config(daemon_path, &mut restore_guard, join_config);
        std::fs::set_permissions(&cluster_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(matches!(start, JoinListenerStartOutcome::Started(Err(_))));
        assert_eq!(
            remuda_native::cluster::listener_config::read().unwrap(),
            Some(snapshot.clone())
        );
        let rollback_message = restore_join_listener(&mut restore_guard).unwrap_or_default();
        assert!(!rollback_message.contains("listener config changed during join"));
        assert_eq!(
            remuda_native::cluster::listener_config::read().unwrap(),
            Some(snapshot)
        );
    }

    #[cfg(unix)]
    #[test]
    fn join_signal_during_listener_start_restores_listener_config() {
        use remuda_core::protocol::ListenerStatus;

        let _serial = JOIN_SIGNAL_TEST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        struct ResetInterruptState;
        impl Drop for ResetInterruptState {
            fn drop(&mut self) {
                JOIN_INTERRUPTED.store(false, Ordering::Relaxed);
                JOIN_INTERRUPT_SIGNAL.store(0, Ordering::Relaxed);
            }
        }

        let _reset = ResetInterruptState;
        let _environment = ForegroundListenerLockEnvironment::new();
        let join_config = ListenerConfig {
            enabled: true,
            bind: ListenerBind::Explicit("127.0.0.1:7441".parse().unwrap()),
            allow_public: false,
        };
        let daemon_path =
            std::env::temp_dir().join(format!("remuda-s3-missing-{}.sock", std::process::id()));
        let mut restore_guard = JoinListenerRestoreGuard::new(&daemon_path, None);
        restore_guard.set_expected(Some(join_config.clone()));
        let _handler = JoinInterruptHandler::install().unwrap();

        let outcome = finish_join_listener_start(&mut restore_guard, |_| {
            remuda_native::cluster::listener_config::write(&join_config).unwrap();
            assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
            Ok(ListenerStatus::On {
                addr: "127.0.0.1:7441".parse().unwrap(),
                auto: false,
                advertise_addr: Some("127.0.0.1:7441".parse().unwrap()),
                listen_addrs: vec!["127.0.0.1:7441".parse().unwrap()],
            })
        });

        assert_eq!(
            remuda_native::cluster::listener_config::read().unwrap(),
            None,
            "listener config was not restored after cancellation during start"
        );
        assert!(matches!(
            outcome,
            JoinListenerStartOutcome::Cancelled(code) if code == ExitCode::from(143)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn join_signal_during_listener_start_reports_start_error_before_cancellation() {
        let _serial = JOIN_SIGNAL_TEST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        struct ResetInterruptState;
        impl Drop for ResetInterruptState {
            fn drop(&mut self) {
                JOIN_INTERRUPTED.store(false, Ordering::Relaxed);
                JOIN_INTERRUPT_SIGNAL.store(0, Ordering::Relaxed);
            }
        }

        let _reset = ResetInterruptState;
        let _environment = ForegroundListenerLockEnvironment::new();
        let daemon_path =
            std::env::temp_dir().join(format!("remuda-s3-missing-{}.sock", std::process::id()));
        let mut restore_guard = JoinListenerRestoreGuard::new(&daemon_path, None);
        JOIN_INTERRUPT_SIGNAL.store(libc::SIGINT, Ordering::Relaxed);
        JOIN_INTERRUPTED.store(true, Ordering::Release);

        let mut reported = Vec::new();
        let outcome = finish_join_listener_start_with_reporter(
            &mut restore_guard,
            |_| Err(std::io::Error::other("address already in use")),
            |message| reported.push(message.to_owned()),
        );

        assert!(matches!(
            outcome,
            JoinListenerStartOutcome::Cancelled(code) if code == ExitCode::from(130)
        ));
        assert!(reported.len() >= 2);
        assert!(reported[0].starts_with("\x1b[31m"));
        assert!(reported[0].contains("could not start the listener: address already in use"));
        let rendered = reported[0]
            .strip_prefix("\x1b[31m")
            .unwrap()
            .strip_suffix("\x1b[0m")
            .unwrap();
        let mut lines = rendered.lines();
        assert!(lines.next().unwrap().contains("address already in use"));
        assert_eq!(lines.next(), Some("Next: remuda cluster listen"));
        assert_eq!(lines.next(), None);
        assert!(reported[1].starts_with("Join cancelled."));
    }

    #[cfg(unix)]
    #[test]
    fn join_wait_observes_cancellation_flag() {
        use std::sync::atomic::AtomicBool;

        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_signal = Arc::clone(&cancelled);
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            cancel_signal.store(true, std::sync::atomic::Ordering::Release);
        });
        let join_result = wait_for_join(cancelled.as_ref(), || {
            std::thread::sleep(Duration::from_millis(60));
            Ok(())
        });
        sender.join().unwrap();
        assert!(matches!(join_result, JoinRun::Cancelled));
    }

    #[cfg(unix)]
    #[test]
    fn join_sigint_handler_sets_the_cancellation_flag() {
        let _serial = JOIN_SIGNAL_TEST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _handler = JoinInterruptHandler::install().unwrap();
        assert!(!JOIN_INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0);
        assert!(JOIN_INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(JOIN_INTERRUPT_SIGNAL.load(Ordering::Relaxed), libc::SIGINT);
    }

    #[cfg(unix)]
    #[test]
    fn join_interrupt_handler_preserves_ignored_sighup() {
        let _serial = JOIN_SIGNAL_TEST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        struct RestoreSignal(libc::c_int, libc::sighandler_t);
        impl Drop for RestoreSignal {
            fn drop(&mut self) {
                unsafe {
                    libc::signal(self.0, self.1);
                }
            }
        }

        let prior = unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };
        assert_ne!(prior, libc::SIG_ERR, "set SIGHUP to ignored");
        let _restore = RestoreSignal(libc::SIGHUP, prior);
        JOIN_INTERRUPTED.store(false, std::sync::atomic::Ordering::Relaxed);
        let _handler = JoinInterruptHandler::install().unwrap();

        assert_eq!(current_signal_handler(libc::SIGHUP), libc::SIG_IGN);
        assert_eq!(unsafe { libc::raise(libc::SIGHUP) }, 0);
        assert!(!JOIN_INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[cfg(unix)]
    #[test]
    fn join_interrupt_handler_tracks_signals_and_restores_prior_dispositions() {
        let _serial = JOIN_SIGNAL_TEST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let signals = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];
        let before: Vec<_> = signals
            .iter()
            .map(|signal| current_signal_handler(*signal))
            .collect();
        let handler = JoinInterruptHandler::install().unwrap();
        JOIN_INTERRUPT_SIGNAL.store(0, Ordering::Relaxed);
        JOIN_INTERRUPTED.store(false, Ordering::Relaxed);
        for signal in signals {
            assert_eq!(unsafe { libc::raise(signal) }, 0);
            assert!(JOIN_INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed));
            assert_eq!(JOIN_INTERRUPT_SIGNAL.load(Ordering::Relaxed), libc::SIGINT);
        }
        drop(handler);
        for (signal, expected) in signals.into_iter().zip(before) {
            assert_eq!(current_signal_handler(signal), expected);
        }
    }

    #[cfg(unix)]
    fn current_signal_handler(signal: libc::c_int) -> libc::sighandler_t {
        let mut action = std::mem::MaybeUninit::<libc::sigaction>::uninit();
        // SAFETY: a null action queries the current disposition into valid output storage.
        let status = unsafe { libc::sigaction(signal, std::ptr::null(), action.as_mut_ptr()) };
        assert_eq!(status, 0, "query signal disposition");
        // SAFETY: sigaction initialized the output when it returned success.
        unsafe { action.assume_init().sa_sigaction }
    }

    #[test]
    fn new_identity_init_is_recognized_with_optional_yes() {
        assert_eq!(
            parse_cluster_command(&["init", "--new-identity"]),
            ClusterCommand::InitNewIdentity { yes: false }
        );
        assert_eq!(
            parse_cluster_command(&["init", "--new-identity", "--yes"]),
            ClusterCommand::InitNewIdentity { yes: true }
        );
    }

    #[test]
    fn new_identity_init_requires_yes_when_non_tty() {
        assert_eq!(
            new_identity_confirmation(false, false, false),
            Err("use --yes to confirm non-interactively")
        );
        assert_eq!(new_identity_confirmation(true, false, false), Ok(false));
        assert_eq!(new_identity_confirmation(false, true, true), Ok(true));
        assert_eq!(
            NEW_IDENTITY_WARNING,
            "This creates a new identity; this machine leaves its current cluster and needs a new invite."
        );
    }

    #[test]
    fn cluster_listen_flags_requiring_an_address_are_actionable() {
        assert_eq!(
            invalid_reason(&["listen", "--allow-public"]),
            (
                "listen".into(),
                "--allow-public and --foreground require --bind ADDR".into()
            )
        );
    }

    #[test]
    fn cluster_listen_without_bind_uses_auto_address() {
        assert!(matches!(
            parse_cluster_command(&["listen"]),
            ClusterCommand::Listen {
                bind_addr: None,
                allow_public: false,
                foreground: false,
            }
        ));
    }

    #[test]
    fn cluster_listen_address_without_port_uses_default() {
        assert_eq!(
            parse_cluster_command(&["listen", "--bind", "192.168.100.0"]),
            ClusterCommand::Listen {
                bind_addr: Some("192.168.100.0:7441".parse().unwrap()),
                allow_public: false,
                foreground: false,
            }
        );
        assert_eq!(
            parse_cluster_command(&["listen", "--bind", "2001:db8::1"]),
            ClusterCommand::Listen {
                bind_addr: Some("[2001:db8::1]:7441".parse().unwrap()),
                allow_public: false,
                foreground: false,
            }
        );
    }

    #[test]
    fn cluster_cli_default_port_matches_advertise_address() {
        let address = parse_addr_default_port("192.168.1.20").unwrap();
        assert_eq!(
            address.port(),
            remuda_native::net::advertise_addr::CLUSTER_DEFAULT_PORT
        );
    }

    #[cfg(unix)]
    #[test]
    fn foreground_listener_conflict_has_an_actionable_next_step() {
        let _environment = ForegroundListenerLockEnvironment::new();
        let _daemon_host_lock = remuda_native::cluster::try_acquire_listener_host_lock().unwrap();
        assert_eq!(
            acquire_foreground_listener_host_lock().unwrap_err(),
            "the cluster listener already runs in the remuda daemon (Next: remuda cluster to see it)"
        );
    }

    #[test]
    fn cluster_invite_address_without_port_uses_default() {
        assert_eq!(
            parse_cluster_command(&["invite", "--bind", "192.0.2.4"]),
            ClusterCommand::Invite {
                bind_addr: Some("192.0.2.4:7441".parse().unwrap()),
                advertised_addr: None,
            }
        );
    }

    #[test]
    fn cluster_join_address_without_port_uses_default() {
        let key = [7; 32];
        let fingerprint = remuda_native::cluster::encoding::fingerprint(&key);
        let line = remuda_native::cluster::join_line::JoinLine {
            issuer_addr: "192.0.2.4:9443".parse().unwrap(),
            issuer_fingerprint: fingerprint.clone(),
            issuer_static_pubkey: key,
            token: zeroize::Zeroizing::new(remuda_native::cluster::encoding::encode_base64(
                &[9; 32],
            )),
        }
        .encode()
        .unwrap();
        assert!(matches!(
            parse_cluster_command(&["join", &fingerprint, &line, "--bind", "192.0.2.8"]),
            ClusterCommand::Join {
                bind_addr: Some(address),
                ..
            } if address == "192.0.2.8:7441".parse().unwrap()
        ));
    }

    #[test]
    fn cluster_call_address_without_port_uses_default() {
        assert!(matches!(
            parse_cluster_command(&["call", "node-abc", "list", "--addr", "192.0.2.4"]),
            ClusterCommand::Call { address, .. }
                if address == "192.0.2.4:7441".parse().unwrap()
        ));
    }

    #[test]
    fn cluster_unquoted_join_line_has_quote_hint() {
        let key = [7; 32];
        let fingerprint = remuda_native::cluster::encoding::fingerprint(&key);
        let bearer = "BEARER_TOKEN_MUST_NOT_APPEAR_71d9";
        let actual = invalid_reason(&[
            "join",
            &fingerprint,
            "remuda-join-v1",
            "192.0.2.4:9443",
            "SHA256:other",
            bearer,
            "proof",
        ]);
        assert_eq!(
            actual,
            (
                "join".into(),
                "quote the whole join line (it contains spaces); FINGERPRINT is optional on a terminal: remuda cluster join [FINGERPRINT] 'remuda-join-v1 …'".into()
            )
        );
        assert!(!actual.1.contains(bearer), "{actual:?}");
        assert!(
            !actual.1.contains("remuda-join-v1 192.0.2.4:9443"),
            "{actual:?}"
        );
    }

    #[test]
    fn cluster_join_without_fingerprint_gets_fingerprint_and_quote_hint() {
        let bearer = "BEARER_TOKEN_MUST_NOT_APPEAR_71d9";
        let actual = invalid_reason(&[
            "join",
            "remuda-join-v1",
            "192.0.2.4:9443",
            "SHA256:other",
            bearer,
            "proof",
        ]);
        assert_eq!(
            actual,
            (
                "join".into(),
                "quote the whole join line (it contains spaces); FINGERPRINT is optional on a terminal: remuda cluster join [FINGERPRINT] 'remuda-join-v1 …'".into()
            )
        );
        assert!(!actual.1.contains(bearer), "{actual:?}");
        assert!(
            !actual.1.contains("remuda-join-v1 192.0.2.4:9443"),
            "{actual:?}"
        );
    }

    #[test]
    fn cluster_help_and_unknown_verbs_are_distinguished() {
        for args in [&["help"][..], &["-h"][..], &["--help"][..]] {
            assert_eq!(parse_cluster_command(args), ClusterCommand::Help(None));
        }
        assert_eq!(
            parse_cluster_command(&["listen", "--help"]),
            ClusterCommand::Help(Some("listen".into()))
        );
        assert_eq!(
            parse_cluster_command(&["node"]),
            ClusterCommand::UnknownVerb("node".into())
        );
    }

    #[test]
    fn listen_off_is_a_cluster_command() {
        assert_eq!(
            parse_cluster_command(&["listen", "--off"]),
            ClusterCommand::ListenOff
        );
    }

    #[test]
    fn cluster_usage_has_one_verb_per_line_and_examples() {
        assert_eq!(
            cluster_usage("listen"),
            "usage: remuda cluster listen [--bind IP[:PORT] (default port 7441)] [--allow-public] [--foreground]\nusage: remuda cluster listen --off\nexample: remuda cluster listen\n"
        );
        for verb in ["invite", "join", "call"] {
            let usage = cluster_usage(verb);
            assert!(usage.contains("IP[:PORT]"), "{verb}: {usage}");
            assert!(usage.contains("default port 7441"), "{verb}: {usage}");
        }
        assert!(cluster_usage("call").contains("--addr 192.0.2.1"));
        assert_eq!(
            cluster_usage("join"),
            "usage: remuda cluster join [FINGERPRINT] 'JOIN_LINE' (FINGERPRINT required when not on a terminal) [--bind IP[:PORT] (default port 7441)]\nexample: remuda cluster join 'remuda-join-v1 …'\n"
        );
        let usage = cluster_usage("");
        for verb in [
            "init", "invite", "join", "nodes", "revoke", "control", "remote", "listen", "call",
        ] {
            assert!(
                usage.contains(&format!("  {verb}\n")),
                "missing {verb}: {usage}"
            );
        }
    }

    #[test]
    fn parse_addr_default_port_adds_7441_to_bare_ipv4_and_ipv6() {
        assert_eq!(
            parse_addr_default_port("192.0.2.4").unwrap(),
            "192.0.2.4:7441".parse().unwrap()
        );
        assert_eq!(
            parse_addr_default_port("2001:db8::4").unwrap(),
            "[2001:db8::4]:7441".parse().unwrap()
        );
        assert_eq!(
            parse_addr_default_port("[fd00::1]").unwrap(),
            "[fd00::1]:7441".parse().unwrap()
        );
    }

    #[test]
    fn parse_addr_default_port_preserves_explicit_ports() {
        assert_eq!(
            parse_addr_default_port("192.0.2.4:9443").unwrap(),
            "192.0.2.4:9443".parse().unwrap()
        );
        assert_eq!(
            parse_addr_default_port("[2001:db8::4]:9443").unwrap(),
            "[2001:db8::4]:9443".parse().unwrap()
        );
    }

    #[test]
    fn cluster_address_options_accept_equals_form() {
        assert_eq!(
            parse_cluster_command(&["invite", "--bind=192.0.2.4:9443"]),
            ClusterCommand::Invite {
                bind_addr: Some("192.0.2.4:9443".parse().unwrap()),
                advertised_addr: None,
            }
        );
        assert_eq!(
            parse_cluster_command(&["listen", "--bind=192.0.2.4:9443"]),
            ClusterCommand::Listen {
                bind_addr: Some("192.0.2.4:9443".parse().unwrap()),
                allow_public: false,
                foreground: false,
            }
        );
        let key = [7; 32];
        let fingerprint = remuda_native::cluster::encoding::fingerprint(&key);
        let line = remuda_native::cluster::join_line::JoinLine {
            issuer_addr: "192.0.2.4:9443".parse().unwrap(),
            issuer_fingerprint: fingerprint.clone(),
            issuer_static_pubkey: key,
            token: zeroize::Zeroizing::new(remuda_native::cluster::encoding::encode_base64(
                &[9; 32],
            )),
        }
        .encode()
        .unwrap();
        assert!(matches!(
            parse_cluster_command(&["join", &fingerprint, &line, "--bind=192.0.2.8:9443"]),
            ClusterCommand::Join {
                fingerprint: Some(_),
                bind_addr: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn cluster_join_accepts_optional_client_endpoint() {
        let key = [7; 32];
        let fingerprint = remuda_native::cluster::encoding::fingerprint(&key);
        let invitation = remuda_native::cluster::join_line::JoinLine {
            issuer_addr: "192.0.2.4:9443".parse().unwrap(),
            issuer_fingerprint: fingerprint.clone(),
            issuer_static_pubkey: key,
            token: zeroize::Zeroizing::new(remuda_native::cluster::encoding::encode_base64(
                &[9; 32],
            )),
        };
        let line = invitation.encode().unwrap();
        assert!(matches!(
            parse_cluster_command(&["join", &fingerprint, &line]),
            ClusterCommand::Join {
                fingerprint: Some(_),
                bind_addr: None,
                ..
            }
        ));
        assert!(matches!(
            parse_cluster_command(&["join", &fingerprint, &line, "--bind", "192.0.2.8:9443"]),
            ClusterCommand::Join {
                fingerprint: Some(_),
                bind_addr: Some(_),
                ..
            }
        ));
        assert!(matches!(
            parse_cluster_command(&["join", &fingerprint, &line, "--bind", "127.0.0.1:0"]),
            ClusterCommand::Join {
                fingerprint: Some(_),
                bind_addr: Some(address),
                ..
            } if address == "127.0.0.1:0".parse().unwrap()
        ));
        assert!(matches!(
            parse_cluster_command(&["join", &fingerprint, &line, "--bind", "0.0.0.0:9443"]),
            ClusterCommand::Invalid { .. }
        ));
    }

    #[test]
    fn cluster_join_accepts_line_without_fingerprint() {
        let key = [7; 32];
        let fingerprint = remuda_native::cluster::encoding::fingerprint(&key);
        let invitation = remuda_native::cluster::join_line::JoinLine {
            issuer_addr: "192.0.2.4:9443".parse().unwrap(),
            issuer_fingerprint: fingerprint.clone(),
            issuer_static_pubkey: key,
            token: zeroize::Zeroizing::new(remuda_native::cluster::encoding::encode_base64(
                &[9; 32],
            )),
        };
        let line = invitation.encode().unwrap();
        assert!(matches!(
            parse_cluster_command(&["join", &line]),
            ClusterCommand::Join {
                fingerprint: None,
                bind_addr: None,
                ..
            }
        ));
    }

    #[test]
    fn invite_message_prints_a_shell_parseable_join_command_and_next_step() {
        let key = [7; 32];
        let fingerprint = remuda_native::cluster::encoding::fingerprint(&key);
        let invitation = remuda_native::cluster::join_line::JoinLine {
            issuer_addr: "192.0.2.4:9443".parse().unwrap(),
            issuer_fingerprint: fingerprint.clone(),
            issuer_static_pubkey: key,
            token: zeroize::Zeroizing::new(remuda_native::cluster::encoding::encode_base64(
                &[9; 32],
            )),
        };
        let output = invite_message(&invitation).unwrap();
        let command = output
            .lines()
            .find(|line| line.starts_with("  remuda cluster join "))
            .unwrap()
            .trim();
        let encoded = invitation.encode().unwrap();
        assert!(!fingerprint.contains('\''));
        assert!(!encoded.contains('\''));
        assert_eq!(
            output,
            format!(
                "Invitation for one machine, valid 10 minutes. Run this on the other machine:\n\n  remuda cluster join '{fingerprint}' '{encoded}'\n\nFingerprint of this machine: {fingerprint} (the other machine must show the same one)\n\nNext: after it joins, run `remuda cluster nodes` here to see it."
            )
        );
        let args = shell_split_single_quotes(command);
        assert_eq!(&args[..2], &["remuda", "cluster"]);
        assert_eq!(
            parse_cluster_command(&args[2..].iter().map(String::as_str).collect::<Vec<_>>(),),
            ClusterCommand::Join {
                fingerprint: Some(fingerprint),
                invitation,
                bind_addr: None,
            }
        );
    }

    #[test]
    fn join_prompt_shows_fingerprint_and_address_without_token() {
        let key = [7; 32];
        let invitation = remuda_native::cluster::join_line::JoinLine {
            issuer_addr: "192.0.2.4:9443".parse().unwrap(),
            issuer_fingerprint: remuda_native::cluster::encoding::fingerprint(&key),
            issuer_static_pubkey: key,
            token: zeroize::Zeroizing::new(remuda_native::cluster::encoding::encode_base64(
                &[9; 32],
            )),
        };
        let prompt = join_prompt(&invitation);
        let node = remuda_native::cluster::node_label(&invitation.issuer_fingerprint);
        assert!(prompt.contains(&format!("Joining {node} at 192.0.2.4:9443")));
        assert!(prompt.contains(&invitation.issuer_fingerprint));
        assert!(prompt.contains(
            "Check the inviting machine shows this fingerprint (run remuda cluster there). Continue? [y/N]"
        ));
        assert!(!prompt.contains(invitation.token.as_str()));
    }

    #[test]
    fn join_success_message_identifies_the_pinned_issuer_without_the_token() {
        let key = [7; 32];
        let invitation = remuda_native::cluster::join_line::JoinLine {
            issuer_addr: "192.0.2.4:9443".parse().unwrap(),
            issuer_fingerprint: remuda_native::cluster::encoding::fingerprint(&key),
            issuer_static_pubkey: key,
            token: zeroize::Zeroizing::new(remuda_native::cluster::encoding::encode_base64(
                &[9; 32],
            )),
        };
        let node = remuda_native::cluster::node_label(&invitation.issuer_fingerprint);
        let message = join_success_message(&invitation, &invitation.issuer_fingerprint);
        assert_eq!(
            message,
            format!(
                "Joined {node} (fingerprint {}).",
                invitation.issuer_fingerprint
            )
        );
        assert!(!message.contains(invitation.token.as_str()));
    }

    #[test]
    fn join_confirmation_requires_both_ttys_and_only_yes_proceeds() {
        assert!(join_confirmation(false, false).is_err());
        assert!(join_confirmation(false, true).is_err());
        assert!(join_confirmation(true, false).is_err());
        assert_eq!(join_confirmation(true, true), Ok(true));
        assert!(confirmation_answer_is_yes("y\n"));
        assert!(confirmation_answer_is_yes("yes\n"));
        assert!(!confirmation_answer_is_yes("n\n"));
        assert!(!confirmation_answer_is_yes(""));
    }

    #[test]
    fn next_steps_cover_init_join_and_single_member_status() {
        assert_eq!(
            next_step_init(),
            "Next: remuda cluster invite (on this machine), or join an existing cluster with the command another machine's invite prints."
        );
        assert_eq!(next_step_join(), "Next: remuda cluster remote");
        assert_eq!(
            next_step_listen(),
            "Keep this running; open another terminal for invite/join."
        );
        assert_eq!(next_step_status(1), Some("Next: remuda cluster invite"));
        assert_eq!(next_step_status(2), None);
    }

    #[test]
    fn utc_revocation_formatter_handles_fixed_epochs() {
        assert_eq!(
            super::format_utc_revocation_time("0"),
            "1970-01-01 00:00 UTC"
        );
        assert_eq!(
            super::format_utc_revocation_time("1790743860"),
            "2026-09-30 04:51 UTC"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_revocation_time_has_local_date_time_and_zone_shape() {
        let actual = super::format_revocation_time("1790743860");
        let mut fields = actual.split(' ');
        let date = fields.next().unwrap_or_default().as_bytes();
        let time = fields.next().unwrap_or_default().as_bytes();
        let zone = fields.next().unwrap_or_default();

        assert_eq!(date.len(), 10, "{actual}");
        assert!(date[..4].iter().all(u8::is_ascii_digit), "{actual}");
        assert_eq!(date[4], b'-', "{actual}");
        assert!(date[5..7].iter().all(u8::is_ascii_digit), "{actual}");
        assert_eq!(date[7], b'-', "{actual}");
        assert!(date[8..10].iter().all(u8::is_ascii_digit), "{actual}");
        assert_eq!(time.len(), 5, "{actual}");
        assert!(time[..2].iter().all(u8::is_ascii_digit), "{actual}");
        assert_eq!(time[2], b':', "{actual}");
        assert!(time[3..5].iter().all(u8::is_ascii_digit), "{actual}");
        assert!(!zone.is_empty(), "{actual}");
        assert!(fields.next().is_none(), "expected one zone token: {actual}");
    }

    #[test]
    fn revoked_notice_keeps_local_time_and_recovery_next_step() {
        let notice = remuda_native::cluster::control::RevokedNotice {
            by_fp: "SHA256:issuer".into(),
            at: "1790743860".into(),
        };
        let mut output = Vec::new();
        write_revocation_notice(&mut output, &notice).unwrap();
        let output = String::from_utf8(output).unwrap();

        assert!(
            output.contains("This node was revoked by node-"),
            "{output}"
        );
        assert!(
            output.contains(" at ") && !output.contains("Unix time"),
            "{output}"
        );
        assert!(output.contains("Next: run `remuda cluster init --new-identity`"));
    }

    fn shell_split_single_quotes(command: &str) -> Vec<String> {
        let mut args = Vec::new();
        let mut word = String::new();
        let mut quoted = false;
        for character in command.chars() {
            match character {
                '\'' => quoted = !quoted,
                ' ' if !quoted => {
                    if !word.is_empty() {
                        args.push(std::mem::take(&mut word));
                    }
                }
                _ => word.push(character),
            }
        }
        assert!(!quoted);
        if !word.is_empty() {
            args.push(word);
        }
        args
    }

    #[cfg(unix)]
    #[test]
    #[allow(clippy::disallowed_types)]
    fn mismatched_join_pin_fails_before_connecting() {
        let keypair = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let expected = remuda_native::cluster::encoding::fingerprint(&keypair.public);
        let invitation = JoinLine {
            issuer_addr: "127.0.0.1:9".parse().unwrap(),
            issuer_fingerprint: expected.clone(),
            issuer_static_pubkey: keypair.public.as_slice().try_into().unwrap(),
            token: Zeroizing::new(remuda_native::cluster::encoding::encode_base64(&[9; 32])),
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut invitation = invitation;
        invitation.issuer_addr = address;

        let accepts = Arc::new(AtomicUsize::new(0));
        let count = accepts.clone();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(250);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        count.fetch_add(1, Ordering::SeqCst);
                        let mut request = [0; 1024];
                        let _ = stream.read(&mut request);
                        let _ = stream.write_all(
                            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("listener accept failed: {error}"),
                }
            }
        });

        let result = cluster_join_with_private_loader("SHA256:wrong", &invitation, None, || {
            Ok(Zeroizing::new(keypair.private))
        });
        let error = result.unwrap_err().to_string();
        server.join().unwrap();
        assert!(error.contains("expected SHA256:wrong"), "{error}");
        assert!(error.contains(&format!("received {expected}")), "{error}");
        assert_eq!(accepts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn cluster_remote_accepts_an_optional_target() {
        assert_eq!(
            parse_cluster_command(&["remote"]),
            ClusterCommand::Remote(None)
        );
        assert_eq!(
            parse_cluster_command(&["remote", "studio/dev"]),
            ClusterCommand::Remote(Some("studio/dev".into()))
        );
    }

    #[test]
    fn cluster_listener_supports_auto_and_explicit_bind_options() {
        assert_eq!(
            parse_cluster_command(&["listen", "--bind", "192.0.2.4:9443"]),
            ClusterCommand::Listen {
                bind_addr: Some("192.0.2.4:9443".parse().unwrap()),
                allow_public: false,
                foreground: false,
            }
        );
        assert_eq!(
            parse_cluster_command(&["listen", "--bind", "0.0.0.0:9443", "--allow-public"]),
            ClusterCommand::Listen {
                bind_addr: Some("0.0.0.0:9443".parse().unwrap()),
                allow_public: true,
                foreground: false,
            }
        );
        assert_eq!(
            parse_cluster_command(&[
                "listen",
                "--foreground",
                "--allow-public",
                "--bind=0.0.0.0:9443"
            ]),
            ClusterCommand::Listen {
                bind_addr: Some("0.0.0.0:9443".parse().unwrap()),
                allow_public: true,
                foreground: true,
            }
        );
        assert!(matches!(
            parse_cluster_command(&["listen", "--bind", "not-an-address"]),
            ClusterCommand::Invalid { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn cluster_init_keeps_explicit_listener_bind() {
        let existing = ListenerConfig {
            enabled: false,
            bind: ListenerBind::Explicit("192.168.1.20:7441".parse().unwrap()),
            allow_public: true,
        };
        for enabled in [true, false] {
            let updated = super::cluster_init_listener_config(Some(existing.clone()), enabled);
            assert_eq!(updated.enabled, enabled);
            assert_eq!(updated.bind, existing.bind);
            assert_eq!(updated.allow_public, existing.allow_public);
        }
    }

    #[test]
    fn repeated_init_uses_already_initialized_wording() {
        assert_eq!(cluster_init_message(false), "Already initialized");
    }

    #[test]
    fn cluster_nodes_is_recognized() {
        assert_eq!(parse_cluster_command(&["nodes"]), ClusterCommand::Nodes);
    }

    #[test]
    fn cluster_control_requires_an_explicit_on_or_off_value() {
        assert_eq!(
            parse_cluster_command(&["control", "on"]),
            ClusterCommand::Control(true)
        );
        assert_eq!(
            parse_cluster_command(&["control", "off"]),
            ClusterCommand::Control(false)
        );
        assert!(matches!(
            parse_cluster_command(&["control", "yes"]),
            ClusterCommand::Invalid { .. }
        ));
    }

    #[test]
    fn cluster_status_explains_remote_control_trust() {
        let (setting, trust) = remote_control_status_lines(true);
        assert_eq!(setting, "Remote control: enabled");
        assert!(trust.contains("compromised admitted node"));
        assert!(trust.contains("close every session"));
        assert_eq!(
            remote_control_status_lines(false).0,
            "Remote control: disabled"
        );
    }

    #[test]
    fn cluster_listener_status_formats_on_auto_and_explicit() {
        use remuda_core::protocol::ListenerStatus;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7441);
        assert_eq!(
            cluster_listener_status_lines(Some(ListenerStatus::On {
                addr,
                auto: true,
                advertise_addr: Some(addr),
                listen_addrs: vec![addr],
            })),
            [
                "Listener: on 127.0.0.1:7441 (auto)",
                "Listening on 127.0.0.1:7441"
            ]
        );
        assert_eq!(
            cluster_listener_status_lines(Some(ListenerStatus::On {
                addr,
                auto: false,
                advertise_addr: Some(addr),
                listen_addrs: vec![addr],
            })),
            [
                "Listener: on 127.0.0.1:7441 (explicit)",
                "Listening on 127.0.0.1:7441"
            ]
        );
    }

    #[test]
    fn auto_listener_init_reports_the_detected_bind_address() {
        use remuda_core::protocol::ListenerStatus;
        use std::net::SocketAddr;

        let status = ListenerStatus::On {
            addr: "192.168.1.20:7441".parse::<SocketAddr>().unwrap(),
            auto: true,
            advertise_addr: Some("192.168.1.20:7441".parse().unwrap()),
            listen_addrs: vec!["192.168.1.20:7441".parse().unwrap()],
        };
        let lines = render_init_listener_lines(&status).join("\n");
        assert!(
            lines.contains("Listening on 192.168.1.20:7441"),
            "init output must show the detected bind address: {lines}"
        );
    }

    #[test]
    fn auto_listener_status_reports_the_detected_bind_address() {
        use remuda_core::protocol::ListenerStatus;
        use std::net::SocketAddr;

        let status = ListenerStatus::On {
            addr: "192.168.1.20:7441".parse::<SocketAddr>().unwrap(),
            auto: true,
            advertise_addr: Some("192.168.1.20:7441".parse().unwrap()),
            listen_addrs: vec!["192.168.1.20:7441".parse().unwrap()],
        };
        let lines = cluster_listener_status_lines(Some(status)).join("\n");
        assert!(
            lines.contains("Listener: on 192.168.1.20:7441 (auto)")
                && lines.contains("Listening on 192.168.1.20:7441"),
            "status must show the detected listener address: {lines}"
        );
    }

    #[test]
    fn listener_output_warns_for_a_public_listened_address() {
        use remuda_core::protocol::ListenerStatus;

        assert!(!is_public_listener_address("169.254.1.2".parse().unwrap()));
        assert!(!is_public_listener_address("192.0.2.1".parse().unwrap()));
        assert!(!is_public_listener_address("fe80::1".parse().unwrap()));
        assert!(is_public_listener_address("8.8.8.8".parse().unwrap()));
        let address = "8.8.8.8:7441".parse().unwrap();
        let lines = cluster_listener_status_lines(Some(ListenerStatus::On {
            addr: address,
            auto: false,
            advertise_addr: Some(address),
            listen_addrs: vec![address],
        }))
        .join("\n");
        assert!(
            lines.contains("Warning: listening on public address 8.8.8.8:7441"),
            "status must warn when a listened address is public: {lines}"
        );
    }

    #[test]
    fn listener_status_accepts_a_pre_advertise_address_wire_value() {
        use remuda_core::protocol::ListenerStatus;

        let old_wire_value = r#"{"On":{"addr":"192.168.1.20:7441","auto":true}}"#;
        let status: ListenerStatus = serde_json::from_str(old_wire_value)
            .expect("older daemons omit the new listener address fields");
        assert!(matches!(
            status,
            ListenerStatus::On {
                advertise_addr: None,
                listen_addrs,
                ..
            } if listen_addrs.is_empty()
        ));
    }

    #[test]
    fn listener_status_preserves_separate_bind_and_advertise_addresses() {
        use remuda_core::protocol::ListenerStatus;

        let wire_value = r#"{"On":{"addr":"0.0.0.0:7441","auto":true,"advertise_addr":"192.168.1.20:7441","listen_addrs":["0.0.0.0:7441"]}}"#;
        let status: ListenerStatus =
            serde_json::from_str(wire_value).expect("listener address fields parse");
        let round_tripped = serde_json::to_value(status).expect("listener status serializes");
        assert_eq!(
            round_tripped["On"]["advertise_addr"], "192.168.1.20:7441",
            "the advertised invite address must survive the protocol round trip"
        );
        assert_eq!(
            round_tripped["On"]["listen_addrs"],
            serde_json::json!(["0.0.0.0:7441"]),
            "the listened addresses must survive the protocol round trip"
        );
    }

    #[test]
    fn cluster_listener_status_formats_off_and_failed_with_next_steps() {
        use remuda_core::protocol::ListenerStatus;

        assert_eq!(
            cluster_listener_status_lines(Some(ListenerStatus::Off)),
            ["Listener: off (Next: remuda cluster listen)"]
        );
        assert_eq!(
            cluster_listener_status_lines(Some(ListenerStatus::Failed("address in use".into()))),
            [
                "Listener: failed: address in use",
                "Next: remuda cluster listen"
            ]
        );
        assert_eq!(
            cluster_listener_status_lines(Some(ListenerStatus::WaitingForLan(
                "no private LAN address found. Next: remuda cluster listen --bind IP".into()
            ))),
            [
                "Listener: waiting for a private LAN address",
                "Next: remuda cluster listen --bind IP"
            ]
        );
        assert_eq!(
            cluster_listener_status_lines(None),
            ["Listener: off (daemon not running)"]
        );
    }

    #[test]
    fn cluster_revoke_accepts_target_and_yes_flag() {
        assert_eq!(
            parse_cluster_command(&["revoke", "node-abcd1234"]),
            ClusterCommand::Revoke {
                target: "node-abcd1234".into(),
                yes: false
            }
        );
        assert_eq!(
            parse_cluster_command(&["revoke", "SHA256:abc", "--yes"]),
            ClusterCommand::Revoke {
                target: "SHA256:abc".into(),
                yes: true
            }
        );
        assert_eq!(
            parse_cluster_command(&["revoke", "--yes", "SHA256:abc"]),
            ClusterCommand::Revoke {
                target: "SHA256:abc".into(),
                yes: true
            }
        );
    }

    #[test]
    fn revoke_confirmation_requires_yes_for_non_tty() {
        assert_eq!(
            revoke_confirmation(false, false, false),
            Err("use --yes to confirm non-interactively")
        );
        assert_eq!(
            revoke_confirmation(false, true, false),
            Err("use --yes to confirm non-interactively")
        );
        assert_eq!(revoke_confirmation(true, false, false), Ok(false));
        assert_eq!(revoke_confirmation(false, true, true), Ok(true));
    }

    #[test]
    fn confirmation_accepts_yes_case_insensitively_and_defaults_no() {
        assert!(confirmation_answer_is_yes("y\n"));
        assert!(confirmation_answer_is_yes("Y"));
        assert!(confirmation_answer_is_yes("yes\n"));
        assert!(confirmation_answer_is_yes("YeS"));
        assert!(!confirmation_answer_is_yes("n"));
        assert!(!confirmation_answer_is_yes("anything else"));
        assert!(!confirmation_answer_is_yes(""));
    }

    #[test]
    fn nodes_output_ignores_a_broken_pipe() {
        struct BrokenPipe;
        impl std::io::Write for BrokenPipe {
            fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let identity = remuda_native::cluster::NodeIdentity {
            node_name: "node-local".into(),
            node_fp: "SHA256:local".into(),
            static_pubkey: vec![],
        };
        assert!(write_nodes_table(
            &mut BrokenPipe,
            &identity,
            &remuda_native::cluster::Registry::default()
        )
        .is_ok());
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
    let mut force = false;
    let mut yes = false;
    let mut inside_override = false;
    for arg in args {
        match *arg {
            "-f" | "--force" if !force => force = true,
            "--yes" if !yes => yes = true,
            "--i-am-inside" if !inside_override => inside_override = true,
            _ => return fail("usage: remuda stop [-f] [--yes]"),
        }
    }
    if remuda_native::ipc::connect(path).is_err() {
        eprintln!("remuda: no daemon running for {server:?} — a state-creating command starts one");
        return ExitCode::SUCCESS;
    }
    if !yes && !force && has_sessions(path) {
        if let Err(refusal) = confirm_losses(path) {
            return fail(refusal);
        }
    }
    let shutdown = Request::Shutdown {
        requester_daemon_id: std::env::var("REMUDA_DAEMON_ID").ok(),
        requester_session_id: std::env::var("REMUDA_SESSION_ID").ok(),
        requester_session_name: std::env::var("REMUDA_SESSION_NAME").ok(),
        override_hosted: inside_override,
    };
    match stop_daemon(path, shutdown) {
        Ok(()) => {
            eprintln!(
                "remuda: stopped the daemon for {server:?} — the next command starts a fresh one"
            );
            ExitCode::SUCCESS
        }
        Err(e) => fail(e),
    }
}

fn has_sessions(path: &Path) -> bool {
    matches!(remuda_native::client::request(path, &Request::List),
        Ok(Response::Sessions(sessions)) if !sessions.is_empty())
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
        return Err("nothing to ask on — `remuda stop -f --yes` if that is what you want".into());
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

fn stop_daemon(path: &Path, shutdown: Request) -> Result<(), String> {
    let asked = remuda_native::client::request(path, &shutdown);
    if !matches!(&asked, Ok(Response::Ok)) {
        return Err(match asked {
            Ok(Response::Error(reason)) => reason,
            other => format!("daemon refused shutdown: {}", describe(other)),
        });
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
fn version_skew(argv: &[&str], path: &Path) -> Result<Option<String>, String> {
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
        return Ok(None);
    }
    if remuda_native::ipc::connect(path).is_err() {
        return Ok(None);
    }
    match remuda_native::client::request(path, &Request::Version) {
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => Err(error.to_string()),
        response => Ok(skew_notice(response)),
    }
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

fn prepare_command(argv: &[&str], path: &Path) -> Result<Option<String>, String> {
    announce_update(argv);
    // Ask once before dispatch. A running daemon belongs to another binary,
    // and this is the cheapest point to check for a version skew.
    let skew = version_skew(argv, path)?;
    if let Some(notice) = &skew {
        eprintln!("remuda: {notice}");
    }
    Ok(skew)
}

/// Split out of `main` for the same reason `list_sessions` was: clippy's line
/// budget. This one talks to no daemon — it replaces this very binary.
fn run_upgrade(args: &[&str]) -> ExitCode {
    if matches!(args, ["--help"] | ["-h"]) {
        print!("{UPGRADE_HELP}");
        return ExitCode::SUCCESS;
    }
    match upgrade_channel(args).and_then(dist::upgrade) {
        Ok(()) => {
            eprintln!(
                "The running daemon and its sessions keep using the old version until you run `remuda stop` (that ends those sessions); the next remuda command starts the new version."
            );
            eprintln!(
                "Next: run `remuda stop` when your sessions can end, then `remuda --version` to check the installed version."
            );
            ExitCode::SUCCESS
        }
        Err(e) => fail(e),
    }
}

fn send_command(server: &str, path: &Path, name: &str, text: &[&str]) -> ExitCode {
    let text = text.join(" ");
    with_daemon(server, path, |path| {
        let request = Request::SendLine {
            name: name.to_string(),
            text: text.clone(),
        };
        simple_request(path, request)
    })
}

/// Commands used by release automation must not start a daemon or run update checks.
fn run_internal_command(argv: &[&str]) -> Option<ExitCode> {
    match argv {
        ["_latest-index", args @ ..] => Some(run_latest_index(args)),
        _ => None,
    }
}

/// Internal release-workflow command. Reuse dist::is_newer so publication and
/// update notices order channel versions identically.
fn run_latest_index(args: &[&str]) -> ExitCode {
    let [file, channel, version, updated] = args else {
        return fail("usage: remuda _latest-index FILE CHANNEL VERSION UPDATED");
    };
    let (file, channel, version, updated) = (*file, *channel, *version, *updated);
    if !dist::is_channel(channel) {
        return fail(format!("unknown channel {channel:?} — stable or nightly"));
    }

    let result = (|| -> Result<&'static str, String> {
        let path = Path::new(file);
        let text = fs::read_to_string(path)
            .map_err(|error| format!("cannot read latest index {}: {error}", path.display()))?;
        let mut index: serde_json::Value = serde_json::from_str(&text)
            .map_err(|error| format!("cannot parse latest index {}: {error}", path.display()))?;
        let object = index
            .as_object_mut()
            .ok_or_else(|| "latest index must be a JSON object".to_string())?;
        let current = object
            .get(channel)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");

        if version == current {
            eprintln!("remuda: latest.json {channel} already points to {version}");
            return Ok("publish=already");
        }
        if !dist::is_newer(version, current) {
            eprintln!(
                "remuda: latest.json {channel} is already {current}; skipping stale candidate {version}"
            );
            return Ok("publish=false");
        }

        object.insert(
            channel.to_string(),
            serde_json::Value::String(version.to_string()),
        );
        object.insert(
            "updated".to_string(),
            serde_json::Value::String(updated.to_string()),
        );
        let mut output = serde_json::to_vec_pretty(&index)
            .map_err(|error| format!("cannot encode latest index: {error}"))?;
        output.push(b'\n');
        fs::write(path, output)
            .map_err(|error| format!("cannot write latest index {}: {error}", path.display()))?;
        Ok("publish=true")
    })();

    match result {
        Ok(status) => {
            println!("{status}");
            ExitCode::SUCCESS
        }
        Err(error) => fail(error),
    }
}

/// `--channel <name>` or nothing, in which case the installed channel file
/// decides. An unknown flag is refused rather than ignored.
fn upgrade_channel<'a>(args: &[&'a str]) -> Result<Option<&'a str>, String> {
    match args {
        [] => Ok(None),
        ["--channel", name] if dist::is_channel(name) => Ok(Some(name)),
        ["--channel", name] => Err(format!(
            "unknown channel {name:?}; use --channel stable or --channel nightly, e.g. remuda upgrade --channel nightly.\nNext: run `remuda upgrade --channel stable` or `remuda upgrade --channel nightly`."
        )),
        _ => Err(
            "usage: remuda upgrade [--channel stable|nightly]\nNext: run `remuda upgrade --channel stable` or `remuda upgrade --channel nightly`."
                .into(),
        ),
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

/// Pull a leading caller-stdin opt-in from argv after the optional server
/// selector. Without this flag, piped stdin remains untouched for mod commands.
fn split_stdin_flag(args: &[String]) -> Result<(bool, &[String]), &'static str> {
    match args {
        [flag, command, ..]
            if flag == "--stdin"
                && remuda_native::packages::valid_component(command)
                && remuda_native::packages::has_subcommand(command) =>
        {
            Ok((true, &args[1..]))
        }
        [flag, ..] if flag == "--stdin" => {
            Err("--stdin is only valid before an installed mod command")
        }
        [command, rest @ ..]
            if remuda_native::packages::valid_component(command)
                && remuda_native::packages::has_subcommand(command) =>
        {
            let options = rest.split(|arg| arg == "--").next().unwrap_or(rest);
            if options.iter().any(|arg| arg == "--stdin") {
                Err("usage: remuda --stdin MOD [ARGS…] (put --stdin before the mod command)")
            } else {
                Ok((false, args))
            }
        }
        _ => Ok((false, args)),
    }
}

/// Run `f`, starting the named daemon first if nothing is listening yet.
fn with_daemon(server: &str, path: &Path, f: impl Fn(&Path) -> ExitCode) -> ExitCode {
    match ensure_daemon(server, path) {
        Ok(()) => f(path),
        Err(e) => fail(e),
    }
}

/// Run a read-only command only against a daemon that already exists.
fn with_existing_daemon(server: &str, path: &Path, f: impl Fn(&Path) -> ExitCode) -> ExitCode {
    match remuda_native::ipc::connect(path) {
        Ok(_) => f(path),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {
            fail(format!("cannot use {}: {error}", path.display()))
        }
        Err(error) if remuda_native::ipc::may_start_daemon(path, &error) => fail(format!(
            "no daemon running for {server:?} (socket {}); start one with remuda run ... or remuda -e ...",
            path.display(),
        )),
        Err(error) => fail(format!(
            "cannot connect to remuda daemon at {}: {error}; refusing to start a second daemon",
            path.display()
        )),
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
            if let Err(error) = remuda_native::script::run_source(path, "=remuda exec", &code) {
                return fail_exec(error);
            }
            wait_for_module_ready(path, name)
        }
    }
}

/// Declaration mistakes are user-facing exec failures, not useful Lua
/// tracebacks. Keep their messages stable and single-line at the CLI boundary.
fn fail_exec(error: String) -> ExitCode {
    const CLEAN_DECLARATION_ERRORS: [&str; 2] = [
        "module timeout_ms must be an integer from 1 through 240000",
        "module timeout_ms requires a ready function",
    ];
    if let Some(message) = CLEAN_DECLARATION_ERRORS
        .iter()
        .find(|message| error.contains(**message))
    {
        eprintln!("{message}");
        ExitCode::FAILURE
    } else {
        fail(error)
    }
}

fn wait_for_module_ready(path: &Path, name: &str) -> ExitCode {
    let source = format!(
        "return remuda.json.encode(remuda._module_readiness({}))",
        remuda_native::mcp::lua_string(name)
    );
    let mut deadline = None;
    loop {
        let checked_at = Instant::now();
        let output =
            match remuda_native::script::eval_source(path, "=remuda exec readiness", &source) {
                Ok(output) => output,
                Err(error) => return fail(error),
            };
        let json = output
            .trim_end_matches('\n')
            .rsplit('\n')
            .next()
            .unwrap_or_default();
        let result: serde_json::Value = match serde_json::from_str(json) {
            Ok(result) => result,
            Err(error) => {
                return fail(format!(
                    "remuda exec readiness probe returned invalid data ({error}): {output}"
                ));
            }
        };
        match result.get("status").and_then(serde_json::Value::as_str) {
            Some("ready") => return ExitCode::SUCCESS,
            Some("failed") => {
                let message = result
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("readiness callback failed");
                eprintln!("mod {name} failed to become ready: {message}");
                return ExitCode::FAILURE;
            }
            Some("pending") => {
                let timeout_ms = match result.get("timeout_ms").and_then(serde_json::Value::as_u64)
                {
                    Some(timeout_ms @ 1..=240_000) => timeout_ms,
                    _ => return fail("module readiness returned an invalid timeout_ms"),
                };
                let wait_until =
                    *deadline.get_or_insert_with(|| checked_at + Duration::from_millis(timeout_ms));
                let now = Instant::now();
                if now >= wait_until {
                    eprintln!(
                        "mod {name} did not become ready within {}s",
                        timeout_ms as f64 / 1000.0
                    );
                    return ExitCode::from(124);
                }
                thread::sleep(Duration::from_millis(250).min(wait_until - now));
            }
            _ => return fail("module readiness returned an unknown status"),
        }
    }
}

/// Dispatch a manifest-declared mod command. The launch form may select an
/// agent and/or skip the screen; other arguments belong to the Lua mod.
fn extension_command(
    server: &str,
    path: &Path,
    command: &str,
    args: &[&str],
    stdin_enabled: bool,
) -> ExitCode {
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
    if stdin_enabled && launch.is_some() {
        return fail("--stdin requires a mod command handler, not a mod launch");
    }
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
    let stdin_opted_in = stdin_enabled
        || args
            .iter()
            .take_while(|argument| **argument != "--")
            .any(|argument| *argument == "-");
    let stdin = if stdin_opted_in {
        const MAX_CALLER_STDIN: usize = 1024 * 1024;
        let mut bytes = Vec::new();
        if let Err(error) = std::io::stdin()
            .take((MAX_CALLER_STDIN + 1) as u64)
            .read_to_end(&mut bytes)
        {
            return fail(format!("read extension command stdin: {error}"));
        }
        if bytes.len() > MAX_CALLER_STDIN {
            return fail("stdin exceeds 1 MiB limit");
        }
        Some(bytes)
    } else {
        None
    };
    let stdin_field = stdin.map_or_else(String::new, |value| {
        format!(", stdin = {}", lua_bytes_literal(&value))
    });
    let code = format!(
        "return remuda._dispatch_extension_command({}, {{{arguments}}}, {{env = {{{env}}}{stdin_field}}})",
        serde_json::to_string(command).expect("command serializes")
    );
    with_daemon(server, path, |path| {
        match load_extension_command(path, command, &package) {
            Ok(()) => eval_once(path, &code),
            Err(failed) => failed,
        }
    })
}

/// Load the mod behind a command on its first use, and say so: loading runs
/// the mod's `start` (#394). The policy is Lua's `load_extension_command`.
fn load_extension_command(path: &Path, command: &str, package: &str) -> Result<(), ExitCode> {
    let load = format!(
        "return remuda.load_extension_command({}, {})",
        serde_json::to_string(command).expect("command serializes"),
        serde_json::to_string(package).expect("mod name serializes")
    );
    match remuda_native::script::eval_source(path, "=remuda mod command", &load) {
        Ok(loaded) => {
            if loaded.trim_end() == "true" {
                eprintln!("remuda: started mod {package}");
            }
            Ok(())
        }
        Err(error) => Err(fail(error)),
    }
}

/// Encode arbitrary bytes as a quoted Lua string with fixed-width decimal
/// escapes, preserving NUL and non-UTF-8 input exactly.
fn lua_bytes_literal(bytes: &[u8]) -> String {
    let mut literal = String::with_capacity(bytes.len() * 4 + 2);
    literal.push('"');
    for byte in bytes {
        literal.push_str(&format!("\\{byte:03}"));
    }
    literal.push('"');
    literal
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

fn echo_socket_lock_wait(stderr_path: &Path, offset: &mut usize, announced: &mut bool) {
    if *announced {
        return;
    }
    let Ok(bytes) = fs::read(stderr_path) else {
        return;
    };
    let Some(tail) = bytes.get(*offset..) else {
        return;
    };
    *offset = bytes.len();
    let Some(start) = tail
        .windows(b"waiting for the socket lock".len())
        .position(|window| window == b"waiting for the socket lock")
    else {
        return;
    };
    let begin = tail[..start]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let end = tail[start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(tail.len(), |index| start + index);
    eprintln!("{}", String::from_utf8_lossy(&tail[begin..end]).trim());
    *announced = true;
}

fn daemon_start_error(path: &Path, stderr_path: &Path, offset: u64, separator: &str) -> String {
    let said = fs::read(stderr_path)
        .ok()
        .and_then(|bytes| bytes.get(offset as usize..).map(<[u8]>::to_vec))
        .map(|bytes| String::from_utf8_lossy(&bytes).replace(separator, ""))
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "it printed nothing".into());
    format!("daemon did not come up at {} — {said}", path.display())
}

/// Spawn ourselves as the daemon and wait for the socket to answer. Wait on a
/// successful *connect*, not on the file existing, and pass `-s <server>`
/// through — a bare `remuda daemon` re-derives `"default"` and never matches.
fn start_daemon(server: &str, path: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find own binary: {e}"))?;
    daemon::prepare_socket_path(path, Some(&daemon::runtime_dir()))
        .map_err(|e| format!("cannot prepare daemon socket directory: {e}"))?;
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
    let stderr_file = open_unix_daemon_log(&stderr_path);
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
    let mut observed_log_bytes = offset as usize;
    let mut announced_lock_wait = false;
    while std::time::Instant::now() < deadline {
        echo_socket_lock_wait(
            &stderr_path,
            &mut observed_log_bytes,
            &mut announced_lock_wait,
        );
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
    Err(daemon_start_error(
        path,
        &stderr_path,
        offset,
        &separator_line,
    ))
}

#[cfg(unix)]
fn open_unix_daemon_log(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if fs::metadata(path).is_ok_and(|m| m.len() > 1_048_576) {
        let _ = fs::rename(path, path.with_extension("log.1"));
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
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

fn resize_session(path: &Path, name: &str, cols: &str, rows: &str) -> ExitCode {
    use remuda_core::Size;
    let parsed = cols
        .parse::<u16>()
        .ok()
        .zip(rows.parse::<u16>().ok())
        .filter(|(cols, rows)| {
            (Size::MIN_RESIZE_COLS..=Size::MAX_RESIZE_COLS).contains(cols)
                && (Size::MIN_ROWS..=Size::MAX_RESIZE_ROWS).contains(rows)
        });
    let Some((cols, rows)) = parsed else {
        eprintln!(
            "resize dimensions must be integers: cols {}..{}, rows {}..{}",
            Size::MIN_RESIZE_COLS,
            Size::MAX_RESIZE_COLS,
            Size::MIN_ROWS,
            Size::MAX_RESIZE_ROWS
        );
        return ExitCode::from(2);
    };
    let size = if cols < Size::MIN_COLS {
        Size::for_pane(cols, rows)
    } else {
        Size::new(cols, rows)
    };
    let request = Request::Resize {
        name: name.to_string(),
        size,
    };
    match remuda_native::client::request(path, &request) {
        Ok(Response::Ok) => ExitCode::SUCCESS,
        Ok(Response::Error(error)) => fail(error),
        other => fail(format!("resize failed: {}", describe(other))),
    }
}

fn resize_command(server: &str, path: &Path, args: &[&str]) -> ExitCode {
    let [name, cols, rows] = args else {
        eprintln!("usage: remuda resize NAME COLS ROWS (cols 20..1000, rows 24..500)");
        return ExitCode::from(2);
    };
    with_existing_daemon(server, path, |path| resize_session(path, name, cols, rows))
}

/// Evaluate one chunk in the daemon's image and print what it came to. Nothing
/// is printed when it returned nothing, so `-e "x = 1"` is silent.
fn eval_once(path: &Path, code: &str) -> ExitCode {
    match remuda_native::client::request_with_secret_prompts(
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
        Ok(Response::CommandResult {
            exit_code,
            stdout_base64,
            stderr_base64,
        }) => {
            use std::io::Write;
            let (Ok(stdout), Ok(stderr)) = (
                remuda_native::cluster::encoding::decode_base64(&stdout_base64),
                remuda_native::cluster::encoding::decode_base64(&stderr_base64),
            ) else {
                return fail("invalid deferred command output encoding");
            };
            let mut out = std::io::stdout().lock();
            let mut err = std::io::stderr().lock();
            if out.write_all(&stdout).is_err() || err.write_all(&stderr).is_err() {
                return fail("could not write deferred command output");
            }
            ExitCode::from(exit_code)
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

#[cfg(test)]
mod cluster_call_tests {
    use super::{parse_cluster_command, sanitize_peer_response, CallAction, ClusterCommand};
    use remuda_core::agent::{Color, Cursor, Size};
    use remuda_core::protocol::{Response, StyledRun};
    use remuda_core::registry::SessionSummary;
    use std::time::Duration;

    #[test]
    fn cluster_call_accepts_only_list_and_capture_with_required_address() {
        assert_eq!(
            parse_cluster_command(&[
                "call",
                "node-abc",
                "list",
                "--addr",
                "127.0.0.1:9",
                "--json"
            ]),
            ClusterCommand::Call {
                target: "node-abc".into(),
                address: "127.0.0.1:9".parse().unwrap(),
                action: CallAction::List,
                json: true,
            }
        );
        assert_eq!(
            parse_cluster_command(&["call", "node-abc", "list", "--addr=127.0.0.1:9"]),
            ClusterCommand::Call {
                target: "node-abc".into(),
                address: "127.0.0.1:9".parse().unwrap(),
                action: CallAction::List,
                json: false,
            }
        );
        assert_eq!(
            parse_cluster_command(&["call", "node-abc", "capture", "build", "--addr", "[::1]:9"]),
            ClusterCommand::Call {
                target: "node-abc".into(),
                address: "[::1]:9".parse().unwrap(),
                action: CallAction::Capture("build".into()),
                json: false,
            }
        );
        for args in [
            vec!["call", "node-abc", "list"],
            vec![
                "call",
                "node-abc",
                "input",
                "build",
                "--addr",
                "127.0.0.1:9",
            ],
            vec!["call", "node-abc", "capture", "--addr", "127.0.0.1:9"],
            vec!["call", "node-abc", "list", "--addr", "invalid"],
            vec![
                "call",
                "node-abc",
                "list",
                "--addr",
                "127.0.0.1:9",
                "--json",
                "--json",
            ],
        ] {
            assert!(
                matches!(parse_cluster_command(&args), ClusterCommand::Invalid { .. }),
                "{args:?}"
            );
        }
    }

    #[test]
    fn cluster_call_strips_osc52_and_osc0_from_peer_text() {
        let sessions = sanitize_peer_response(Response::Sessions(vec![SessionSummary {
            id: "id\u{1b}]52;c;clipboard\u{7}".into(),
            name: "name\u{1b}]0;peer title\u{7}".into(),
            instance_id: None,
            output_version: None,
            alive: true,
            idle: Duration::ZERO,
            output_idle: None,
            size: Size::new(80, 24),
            attached: false,
            human_idle: None,
            mouse_tracking: false,
        }]));
        let Response::Sessions(sessions) = sessions else {
            panic!("expected sanitized sessions");
        };
        assert_eq!(sessions[0].id, "id]52;c;clipboard");
        assert_eq!(sessions[0].name, "name]0;peer title");

        let error = sanitize_peer_response(Response::Error("bad\u{1b}]52;c;clipboard\u{7}".into()));
        assert_eq!(error, Response::Error("bad]52;c;clipboard".into()));

        let capture = sanitize_peer_response(Response::StyledScreen {
            rows: vec![vec![StyledRun {
                text: "screen\u{1b}]0;peer title\u{7}".into(),
                fg: Color::default(),
                bg: Color::default(),
                bold: false,
                dim: false,
                italic: false,
                underline: false,
                inverse: false,
                wide: false,
            }]],
            instance_id: None,
            output_version: None,
            wrapped: vec![],
            scrollback_len: 0,
            scrollback_total: 0,
            cursor: Cursor {
                row: 0,
                col: 0,
                visible: true,
            },
        });
        let Response::StyledScreen { rows, .. } = capture else {
            panic!("expected sanitized capture");
        };
        assert_eq!(rows[0][0].text, "screen]0;peer title");
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
        Ok(Response::Busy) => "session input is busy".into(),
        Ok(Response::WriteTimeout) => {
            "session PTY write timed out; delivery may be partial or late".into()
        }
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
    eprintln!("{}", format_failure(&message));
    ExitCode::FAILURE
}

fn format_failure(message: &str) -> String {
    format!("remuda: {message}")
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

    #[derive(Debug)]
    struct TestErrorMessage(String);

    impl std::fmt::Display for TestErrorMessage {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.fmt(formatter)
        }
    }

    impl std::error::Error for TestErrorMessage {}

    #[test]
    fn cluster_error_mappings_are_actionable() {
        let issuer_addr = "192.0.2.8:7441".parse().unwrap();
        let wildcard_addr = "0.0.0.0:7441".parse().unwrap();
        let unavailable_addr = "192.0.2.99:7441".parse().unwrap();
        let cases = [
            (
                "join",
                std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"),
                ClusterErrorContext::Address(issuer_addr),
                "cannot reach 192.0.2.8:7441 (connection refused).",
            ),
            (
                "join",
                std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout"),
                ClusterErrorContext::Address(issuer_addr),
                "cannot reach 192.0.2.8:7441 (connection timed out).",
            ),
            (
                "join",
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, "join was refused"),
                ClusterErrorContext::Address(issuer_addr),
                "the invitation was refused (join lines work once and expire after 10 minutes).",
            ),
            (
                "listen",
                std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "wildcard listener bind requires explicit public-bind opt-in",
                ),
                ClusterErrorContext::Address(wildcard_addr),
                "binding all interfaces needs --allow-public",
            ),
            (
                "listen",
                std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "unavailable"),
                ClusterErrorContext::Address(unavailable_addr),
                "192.0.2.99 is not an address of this machine.",
            ),
            (
                "invite",
                std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid cluster join line"),
                ClusterErrorContext::Address(wildcard_addr),
                "use an address of this machine that the other machine can reach",
            ),
        ];

        for (verb, error, address, expected) in cases {
            let message = describe_cluster_error(verb, &error, address);
            assert!(message.contains(expected), "{verb}: {message}");
            assert!(message.contains("Next:"), "{verb}: {message}");
        }
    }

    #[test]
    fn generic_join_error_keeps_detail_and_has_one_cli_prefix() {
        let address = "192.0.2.8:7441".parse().unwrap();
        let error = std::io::Error::other("connection reset by peer");
        let description =
            describe_cluster_error("join", &error, ClusterErrorContext::Address(address));
        assert_eq!(
            description,
            "cluster join: connection reset by peer.\nNext: check the invitation and try again."
        );

        let printed = format_failure(&description);
        assert_eq!(printed.matches("remuda: ").count(), 1, "{printed}");
        assert_eq!(
            printed,
            "remuda: cluster join: connection reset by peer.\nNext: check the invitation and try again."
        );
    }

    #[cfg(unix)]
    #[test]
    fn join_permission_denied_explains_network_permission_without_leaking_invite() {
        let key = [7; 32];
        let invitation = remuda_native::cluster::join_line::JoinLine {
            issuer_addr: "192.0.2.8:7441".parse().unwrap(),
            issuer_fingerprint: remuda_native::cluster::encoding::fingerprint(&key),
            issuer_static_pubkey: key,
            token: zeroize::Zeroizing::new(remuda_native::cluster::encoding::encode_base64(
                &[9; 32],
            )),
        };
        let line = invitation.encode().unwrap();
        let error = std::io::Error::from_raw_os_error(1);

        let message =
            describe_cluster_error("join", &error, ClusterErrorContext::Join(&invitation));

        assert_eq!(
            message,
            "cluster join: cannot reach 192.0.2.8:7441 (the OS blocked the connection).\nNext: the OS blocked the connection; allow remuda network access (macOS: System Settings > Privacy & Security > Local Network), then retry the same join command."
        );
        assert!(!message.contains(&line));
        assert!(!message.contains(invitation.token.as_str()));
    }

    #[cfg(unix)]
    #[test]
    fn join_eacces_keeps_the_generic_permission_denied_message() {
        let address = "192.0.2.8:7441".parse().unwrap();
        let error = std::io::Error::from_raw_os_error(13);

        let message = describe_cluster_error("join", &error, ClusterErrorContext::Address(address));

        assert!(!message.contains("the OS blocked the connection"));
        assert!(message.contains("Next: check the invitation and try again."));
    }

    #[test]
    fn fingerprint_mismatch_explains_how_to_check_the_pin() {
        let key = [7; 32];
        let fingerprint = remuda_native::cluster::encoding::fingerprint(&key);
        let invitation = remuda_native::cluster::join_line::JoinLine {
            issuer_addr: "192.0.2.8:7441".parse().unwrap(),
            issuer_fingerprint: fingerprint,
            issuer_static_pubkey: key,
            token: zeroize::Zeroizing::new(remuda_native::cluster::encoding::encode_base64(
                &[9; 32],
            )),
        };
        let error = invitation.verify_pin("SHA256:wrong").unwrap_err();
        let message =
            describe_cluster_error("join", &error, ClusterErrorContext::Join(&invitation));
        assert!(message.contains("issuer fingerprint mismatch: expected SHA256:wrong"));
        assert!(message.contains(
            "Next: ask the inviting machine to run `remuda cluster` and read its Fingerprint line."
        ));
    }

    #[test]
    fn join_error_messages_never_include_invitation_line_or_token() {
        let key = [7; 32];
        let fingerprint = remuda_native::cluster::encoding::fingerprint(&key);
        let invitation = remuda_native::cluster::join_line::JoinLine {
            issuer_addr: "192.0.2.8:7441".parse().unwrap(),
            issuer_fingerprint: fingerprint,
            issuer_static_pubkey: key,
            token: zeroize::Zeroizing::new(remuda_native::cluster::encoding::encode_base64(
                &[9; 32],
            )),
        };
        let line = invitation.encode().unwrap();
        let error = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        let message =
            describe_cluster_error("join", &error, ClusterErrorContext::Join(&invitation));
        assert!(!message.contains(&line), "error echoed the invitation line");
        assert!(
            !message.contains(invitation.token.as_str()),
            "error echoed the invitation token"
        );

        let error = invitation
            .verify_pin(invitation.token.as_str())
            .expect_err("the bearer token is not the issuer fingerprint");
        let message =
            describe_cluster_error("join", &error, ClusterErrorContext::Join(&invitation));
        assert!(
            !message.contains(invitation.token.as_str()),
            "pin error echoed the invitation token"
        );
        assert!(
            !message.contains(&line),
            "pin error echoed the invitation line"
        );

        let error = std::io::Error::other(TestErrorMessage(line.clone()));
        let message =
            describe_cluster_error("join", &error, ClusterErrorContext::Join(&invitation));
        assert!(
            !message.contains(&line),
            "generic join error echoed the invitation line"
        );
        assert!(
            !message.contains(invitation.token.as_str()),
            "generic join error echoed the invitation token"
        );
    }

    #[test]
    fn suggest_command_qualifies_cluster_verbs() {
        for verb in [
            "nodes", "init", "invite", "join", "revoke", "remote", "listen", "control", "call",
        ] {
            assert_eq!(suggest_command(verb), Some(format!("cluster {verb}")));
        }
    }

    #[test]
    fn suggest_command_uses_distance_for_top_level_verbs() {
        assert_eq!(suggest_command("atach").as_deref(), Some("attach"));
        assert_eq!(suggest_command("zzzz"), None);
    }

    #[test]
    fn closest_word_uses_a_shorter_distance_and_breaks_ties_by_name() {
        assert_eq!(closest_word("x", &["ls"]), None);
        assert_eq!(closest_word("atach", &["attach"]), Some("attach"));
        assert_eq!(closest_word("cot", &["cut", "bot"]), Some("bot"));
        assert_eq!(suggest_command("nodes"), Some("cluster nodes".into()));
    }

    #[test]
    fn write_timeout_has_a_user_facing_diagnostic() {
        assert_eq!(
            describe(Ok(Response::WriteTimeout)),
            "session PTY write timed out; delivery may be partial or late"
        );
        assert_eq!(describe(Ok(Response::Busy)), "session input is busy");
    }

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
