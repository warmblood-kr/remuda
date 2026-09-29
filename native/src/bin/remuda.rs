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
use remuda_native::{daemon, dist, terminal_size};
use std::fs;
use std::io::{IsTerminal, Read, Write};
use std::path::Path;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

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

        [command, rest @ ..] if remuda_native::packages::has_subcommand(command) => {
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
  remuda cluster nodes            list local cluster membership
  remuda cluster revoke NODE [--yes] revoke a member locally
  remuda cluster control on|off   allow or refuse remote control
  remuda cluster remote [node/session] open the cluster session tree
  remuda cluster listen --bind ADDR [--allow-public] start the cluster listener
  remuda cluster call NODE list --addr HOST:PORT [--json]  list a remote node's sessions
  remuda cluster call NODE capture SESSION --addr HOST:PORT [--json]  capture a remote screen
                                  exits: 0 success, 2 usage, 3 unreachable/timeout,
                                  4 refused/unknown/revoked, 5 crypto/bad response
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
  while not remuda.capture(\"build\"):find(\"$ \") do remuda.sleep(0.2) end
  remuda.session.close(\"build\")

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
  remuda stop [-f] [--yes] [--i-am-inside]  stop the daemon (sessions are lost)

  remuda mod install OWNER/REPO  install a mod from GitHub
  remuda mod list | info NAME    inspect installed mods
  remuda mod update NAME|--all   update a mod
  remuda mod remove NAME         remove a mod
  remuda cluster                 show cluster status
  remuda cluster init            create this node's cluster identity
  remuda cluster nodes           list local cluster membership
  remuda cluster revoke NODE [--yes] revoke a member locally
  remuda cluster control on|off   allow or refuse remote control
  remuda cluster remote [node/session] open the cluster session tree
  remuda cluster listen --bind ADDR [--allow-public] start the cluster listener
  remuda cluster call NODE list --addr HOST:PORT [--json]  list a remote node's sessions
  remuda cluster call NODE capture SESSION --addr HOST:PORT [--json]  capture a remote screen
                                  exits: 0 success, 2 usage, 3 unreachable/timeout,
                                  4 refused/unknown/revoked, 5 crypto/bad response
                                  [::] may accept IPv4 too on dual-stack systems

  remuda doc | repl | -e CODE    use the persistent Lua runtime
  remuda --stdin MOD [ARGS…]     opt in to passing up to 1 MiB of stdin to the mod
  remuda MOD ... -                a literal '-' argument also opts in to stdin
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
    Nodes,
    Control(bool),
    Revoke {
        target: String,
        yes: bool,
    },
    Remote(Option<String>),
    Listen {
        bind_addr: std::net::SocketAddr,
        allow_public: bool,
    },
    Call {
        target: String,
        address: std::net::SocketAddr,
        action: CallAction,
        json: bool,
    },
    Invalid,
}

#[derive(Debug, PartialEq, Eq)]
enum CallAction {
    List,
    Capture(String),
}

fn parse_cluster_command(args: &[&str]) -> ClusterCommand {
    match args {
        [] => ClusterCommand::Status,
        ["init"] => ClusterCommand::Init,
        ["nodes"] => ClusterCommand::Nodes,
        ["control", "on"] => ClusterCommand::Control(true),
        ["control", "off"] => ClusterCommand::Control(false),
        ["revoke", target] => ClusterCommand::Revoke {
            target: (*target).to_string(),
            yes: false,
        },
        ["revoke", target, "--yes"] => ClusterCommand::Revoke {
            target: (*target).to_string(),
            yes: true,
        },
        ["revoke", "--yes", target] => ClusterCommand::Revoke {
            target: (*target).to_string(),
            yes: true,
        },
        ["remote"] => ClusterCommand::Remote(None),
        ["remote", target] if target.contains('/') => {
            ClusterCommand::Remote(Some((*target).to_string()))
        }
        ["listen", "--bind", address] => address
            .parse()
            .ok()
            .map(|bind_addr| ClusterCommand::Listen {
                bind_addr,
                allow_public: false,
            })
            .unwrap_or(ClusterCommand::Invalid),
        ["listen", "--bind", address, "--allow-public"]
        | ["listen", "--allow-public", "--bind", address] => address
            .parse()
            .ok()
            .map(|bind_addr| ClusterCommand::Listen {
                bind_addr,
                allow_public: true,
            })
            .unwrap_or(ClusterCommand::Invalid),
        ["call", target, operation, rest @ ..] => parse_cluster_call(target, operation, rest),
        _ => ClusterCommand::Invalid,
    }
}

fn parse_cluster_call(target: &str, operation: &str, args: &[&str]) -> ClusterCommand {
    let action = match operation {
        "list" => CallAction::List,
        "capture" => match args.first() {
            Some(session) if !session.starts_with('-') => CallAction::Capture((*session).into()),
            _ => return ClusterCommand::Invalid,
        },
        _ => return ClusterCommand::Invalid,
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
            "--addr" if address.is_none() && index + 1 < args.len() => {
                address = args[index + 1].parse().ok();
                if address.is_none() {
                    return ClusterCommand::Invalid;
                }
                index += 2;
            }
            _ => return ClusterCommand::Invalid,
        }
    }
    address.map_or(ClusterCommand::Invalid, |address| ClusterCommand::Call {
        target: target.into(),
        address,
        action,
        json,
    })
}

fn cluster_command(server: &str, path: &Path, args: &[&str]) -> ExitCode {
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
                match remuda_native::cluster::control::enabled() {
                    Ok(enabled) => {
                        let (setting, trust) = remote_control_status_lines(enabled);
                        println!("{setting}");
                        println!("{trust}");
                        ExitCode::SUCCESS
                    }
                    Err(error) => fail(format!("cluster status: {error}")),
                }
            }
            Err(error) => fail(format!("cluster status: {error}")),
        },
        ClusterCommand::Init => match remuda_native::cluster::init() {
            Ok((identity, created)) => {
                println!("{}", cluster_init_message(created));
                println!("Node: {}", identity.node_name);
                println!("Fingerprint: {}", identity.node_fp);
                ExitCode::SUCCESS
            }
            Err(error) => fail(format!("cluster init: {error}")),
        },
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
        ClusterCommand::Listen {
            bind_addr,
            allow_public,
        } => with_daemon(server, path, |daemon_path| {
            let config = remuda_native::net::listener::ListenerConfig {
                bind_addr,
                allow_unspecified: allow_public,
            };
            match remuda_native::net::listener::bind(config, daemon_path) {
                Ok(listener) => {
                    eprintln!(
                        "remuda: cluster listener on {}",
                        listener.local_addr().unwrap_or(bind_addr)
                    );
                    match listener.serve() {
                        Ok(()) => ExitCode::SUCCESS,
                        Err(error) => fail(format!("cluster listener: {error}")),
                    }
                }
                Err(error) => fail(format!("cluster listener: {error}")),
            }
        }),
        ClusterCommand::Call {
            target,
            address,
            action,
            json,
        } => cluster_call(&target, address, action, json),
        ClusterCommand::Invalid => {
            eprintln!("usage: remuda cluster [init | nodes | revoke <node|fingerprint> [--yes] | control on|off | remote [node/session] | listen --bind ADDR [--allow-public] | call NODE (list | capture SESSION) --addr HOST:PORT [--json]]");
            ExitCode::from(2)
        }
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
                ClientError::Unreachable | ClientError::Timeout => 3,
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
    match remuda_native::cluster::revoke(&fingerprint) {
        Ok(remuda_native::cluster::RevokeOutcome::Revoked) => {
            println!(
                "Node {label} revoked locally; propagates when the cluster transport is enabled."
            );
            ExitCode::SUCCESS
        }
        Ok(remuda_native::cluster::RevokeOutcome::AlreadyRevoked) => {
            println!("Node {label} is already revoked.");
            ExitCode::SUCCESS
        }
        Err(error) => fail(format!("cluster revoke: {error}")),
    }
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
    match writer
        .write_all(remuda_native::cluster::format_nodes_table(identity, registry).as_bytes())
    {
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}

fn cluster_init_message(created: bool) -> &'static str {
    if created {
        "Cluster initialized"
    } else {
        "Already initialized"
    }
}

#[cfg(test)]
mod cluster_cli_tests {
    use super::{
        cluster_init_message, confirmation_answer_is_yes, parse_cluster_command,
        remote_control_status_lines, revoke_confirmation, write_nodes_table, ClusterCommand,
    };

    #[test]
    fn cluster_status_and_init_are_recognized() {
        assert_eq!(parse_cluster_command(&[]), ClusterCommand::Status);
        assert_eq!(parse_cluster_command(&["init"]), ClusterCommand::Init);
        assert_eq!(parse_cluster_command(&["join"]), ClusterCommand::Invalid);
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
    fn cluster_listener_requires_explicit_bind_and_public_wildcard_opt_in() {
        assert_eq!(
            parse_cluster_command(&["listen", "--bind", "192.0.2.4:9443"]),
            ClusterCommand::Listen {
                bind_addr: "192.0.2.4:9443".parse().unwrap(),
                allow_public: false,
            }
        );
        assert_eq!(
            parse_cluster_command(&["listen", "--bind", "0.0.0.0:9443", "--allow-public"]),
            ClusterCommand::Listen {
                bind_addr: "0.0.0.0:9443".parse().unwrap(),
                allow_public: true,
            }
        );
        assert_eq!(
            parse_cluster_command(&["listen", "--bind", "not-an-address"]),
            ClusterCommand::Invalid
        );
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
        assert_eq!(
            parse_cluster_command(&["control", "yes"]),
            ClusterCommand::Invalid
        );
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
    match upgrade_channel(args).and_then(dist::upgrade) {
        Ok(()) => ExitCode::SUCCESS,
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

/// Pull a leading caller-stdin opt-in from argv after the optional server
/// selector. Without this flag, piped stdin remains untouched for mod commands.
fn split_stdin_flag(args: &[String]) -> Result<(bool, &[String]), &'static str> {
    match args {
        [flag, command, ..]
            if flag == "--stdin" && remuda_native::packages::has_subcommand(command) =>
        {
            Ok((true, &args[1..]))
        }
        [flag, ..] if flag == "--stdin" => {
            Err("--stdin is only valid before an installed mod command")
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
    let stdin_opted_in = stdin_enabled || args.contains(&"-");
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
    with_daemon(server, path, |path| eval_once(path, &code))
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
            assert_eq!(
                parse_cluster_command(&args),
                ClusterCommand::Invalid,
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
