//! Verbs that must answer without touching any daemon, and a socket path too
//! long to bind that must say so instead of blaming a second daemon. #115.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn remuda(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(["-s", "s"])
        .args(args)
        .env("REMUDA_RUNTIME_DIR", dir)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("HOME", dir)
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        .output()
        .expect("run remuda")
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rnd{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct SessionAutoStartScratch {
    root: PathBuf,
    server: Option<String>,
}

impl SessionAutoStartScratch {
    fn new(tag: &str, server: Option<String>) -> Self {
        let base = if Path::new("/private/tmp").is_dir() {
            PathBuf::from("/private/tmp")
        } else {
            std::env::temp_dir().canonicalize().unwrap()
        };
        let root = base.join(format!("r453-session-{}-{tag}", std::process::id()));
        let mut builder = std::fs::DirBuilder::new();
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&root).unwrap();
        Self { root, server }
    }

    fn socket(&self, server: &str) -> PathBuf {
        remuda_native::daemon::socket_path_in(&self.root, server)
    }
}

fn daemon_pids_for_runtime(runtime: &Path) -> Vec<u32> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,comm="])
        .output()
        .expect("list processes for the private runtime");
    assert!(output.status.success(), "ps failed: {output:?}");
    let runtime_marker = format!("REMUDA_RUNTIME_DIR={}", runtime.display());
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let executable = fields.next()?;
            (Path::new(executable)
                .file_name()
                .and_then(|name| name.to_str())
                == Some("remuda"))
            .then_some(pid)
        })
        .filter(|pid| {
            let pid = pid.to_string();
            let output = Command::new("ps")
                .args(["eww", "-p", &pid, "-o", "command="])
                .output()
                .expect("inspect remuda process environment");
            String::from_utf8_lossy(&output.stdout).contains(&runtime_marker)
        })
        .collect()
}

impl Drop for SessionAutoStartScratch {
    fn drop(&mut self) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_remuda"));
        if let Some(server) = &self.server {
            command.args(["-s", server]);
        }
        let _ = command
            .args(["stop", "-f", "--yes"])
            .env("REMUDA_RUNTIME_DIR", &self.root)
            .env("XDG_RUNTIME_DIR", &self.root)
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env_remove("REMUDA_SESSION_ID")
            .env_remove("REMUDA_SESSION_CAPABILITY")
            .output();
        for pid in daemon_pids_for_runtime(&self.root) {
            let _ = Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn session_client(root: &Path, args: &[&str], session_env: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(args)
        .env("REMUDA_RUNTIME_DIR", root)
        .env("XDG_RUNTIME_DIR", root)
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        .env(session_env, "present")
        .output()
        .expect("run session client")
}

#[test]
fn session_clients_block_default_autostart_but_explicit_other_still_starts() {
    for (index, session_env) in ["REMUDA_SESSION_ID", "REMUDA_SESSION_CAPABILITY"]
        .into_iter()
        .enumerate()
    {
        let scratch = SessionAutoStartScratch::new(&format!("default-{index}"), None);
        let socket = scratch.socket("default");
        let out = session_client(&scratch.root, &["-e", "return true"], session_env);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success(),
            "{session_env} must prevent implicit default autostart: {out:?}"
        );
        assert!(
            stderr.contains(&socket.display().to_string()),
            "error must name the socket tried: {stderr}"
        );
        assert!(
            stderr.contains("Next:")
                && stderr.contains("outside the session")
                && stderr.contains("pass -s NAME"),
            "error must say how to proceed: {stderr}"
        );
        assert!(!socket.exists(), "session client started a default daemon");
        assert!(
            daemon_pids_for_runtime(&scratch.root).is_empty(),
            "session client left a daemon process running"
        );
    }

    let server = format!("other-{}", std::process::id());
    let scratch = SessionAutoStartScratch::new("explicit-other", Some(server.clone()));
    let out = session_client(
        &scratch.root,
        &["-s", &server, "-e", "return true"],
        "REMUDA_SESSION_ID",
    );
    assert!(
        out.status.success(),
        "explicit other server must autostart: {out:?}"
    );
    assert!(
        scratch.socket(&server).exists(),
        "explicit other daemon did not start"
    );
}

#[test]
fn empty_or_whitespace_server_names_are_refused_before_daemon_access() {
    for (tag, server, verb) in [
        ("empty-autostart", "", &["-e", "return true"][..]),
        ("empty-read-only", "", &["ls"][..]),
        ("spaces-autostart", " ", &["-e", "return true"][..]),
        ("spaces-read-only", " ", &["ls"][..]),
    ] {
        let scratch = SessionAutoStartScratch::new(tag, None);
        let out = Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", server])
            .args(verb)
            .env("REMUDA_RUNTIME_DIR", &scratch.root)
            .env("XDG_RUNTIME_DIR", &scratch.root)
            .env("HOME", &scratch.root)
            .env("XDG_CONFIG_HOME", scratch.root.join("config"))
            .env("XDG_DATA_HOME", scratch.root.join("data"))
            .env("REMUDA_NO_UPDATE_CHECK", "1")
            .output()
            .expect("run remuda with an invalid server name");
        let stderr = String::from_utf8_lossy(&out.stderr);
        let socket = scratch.socket(server);

        assert!(!out.status.success(), "accepted server {server:?}: {out:?}");
        assert_eq!(
            out.status.code(),
            Some(2),
            "invalid server name should be a usage error: {out:?}"
        );
        assert_eq!(
            stderr,
            "remuda: session name cannot be empty or whitespace.\nNext: name the session with -s NAME, or omit -s.\n",
            "invalid server name should have one line plus Next:"
        );
        assert!(!socket.exists(), "started a daemon at {socket:?}: {stderr}");
        assert!(
            !scratch.root.join("remuda").exists(),
            "created a runtime socket directory: {stderr}"
        );
        assert!(
            daemon_pids_for_runtime(&scratch.root).is_empty(),
            "left a daemon running for server {server:?}: {stderr}"
        );
    }
}

/// Run `args` against a stand-in at the private server's socket that only
/// records connections, hanging up at once so a connecting CLI fails fast.
fn touches_daemon(tag: &str, args: &[&str]) -> (Output, bool) {
    let dir = scratch(tag);
    let socket = remuda_native::daemon::socket_path_in(&dir, "s");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&socket).expect("bind stand-in");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        while listener.accept().is_ok() {
            let _ = tx.send(());
        }
    });
    let out = remuda(&dir, args);
    let touched = rx.try_recv().is_ok();
    let _ = std::fs::remove_dir_all(&dir);
    (out, touched)
}

#[test]
fn help_and_version_never_connect_to_a_daemon() {
    // Bare `remuda` here has no tty (output() nulls stdin), so it prints help.
    for args in [
        &["--help"][..],
        &["-h"],
        &["help"],
        &["--version"],
        &["-V"],
        &["version"],
        &[],
    ] {
        let (out, touched) = touches_daemon("help", args);
        assert!(out.status.success(), "{args:?} failed: {out:?}");
        assert!(!touched, "{args:?} connected to the daemon socket");
    }
}

#[test]
fn unknown_cluster_verbs_suggest_the_cluster_path() {
    let dir = scratch("unknown-cluster-verb");
    let out = remuda(&dir, &["nodes"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr.trim(),
        "remuda: no command or mod named nodes.\nDid you mean: remuda cluster nodes?\nNext: run remuda cluster nodes."
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn nearby_top_level_typos_get_did_you_mean() {
    let dir = scratch("unknown-nearby-verb");
    let out = remuda(&dir, &["atach"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr.trim(),
        "remuda: no command or mod named atach.\nDid you mean: remuda attach?\nNext: run remuda attach."
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn unrelated_unknown_verb_gets_concise_help() {
    let dir = scratch("unknown-unrelated-verb");
    let out = remuda(&dir, &["zzzz"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr.trim(),
        "remuda: no command or mod named zzzz.\nNext: if zzzz is a mod, install it with remuda mod install OWNER/REPO; installed mods: remuda mod list."
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn quoted_join_line_unknown_word_does_not_echo_its_token() {
    let dir = scratch("unknown-join-line");
    let token = "secret-join-token-331";
    let join_line = format!("remuda-join-v1 192.0.2.4:7441 SHA256:issuer PUBKEY {token}");
    let out = remuda(&dir, &[&join_line]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1));
    assert!(!stderr.contains(token), "diagnostic echoed secret material");
    assert!(
        stderr.contains("that looks like a join line; run: remuda cluster join [FINGERPRINT]")
            && stderr.contains("FINGERPRINT required when not on a terminal"),
        "missing join-line hint"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn cluster_unknown_join_line_does_not_echo_its_token() {
    let dir = scratch("cluster-unknown-join-line");
    let token = "secret-cluster-join-token-337";
    let join_line = format!("remuda-join-v1 192.0.2.4:7441 SHA256:issuer PUBKEY {token}");
    let out = remuda(&dir, &["cluster", &join_line]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2));
    assert!(!stderr.contains(token), "diagnostic echoed secret material");
    assert!(
        stderr.contains("that looks like a join line; run: remuda cluster join FINGERPRINT"),
        "missing cluster join-line hint: {stderr}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn long_whitespace_unknown_word_is_not_echoed() {
    let dir = scratch("unknown-long-word");
    let token = "secret-join-token-331";
    let word = format!("not a command with sensitive suffix {token}");
    let out = remuda(&dir, &[&word]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr.trim(),
        "remuda: no command or mod with that name.\nNext: if this is a mod, install it with remuda mod install OWNER/REPO; installed mods: remuda mod list."
    );
    assert!(!stderr.contains(token), "diagnostic echoed secret material");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn help_flags_write_usage_to_stdout_and_mod_commands_to_stderr() {
    let dir = scratch("help-output");
    let mod_dir = dir.join("data/remuda/mods/probe");
    std::fs::create_dir_all(&mod_dir).unwrap();
    std::fs::write(
        mod_dir.join("extension.toml"),
        "name = \"probe\"\nentry = \"packages/probe/init.lua\"\napi = \"remuda-lua-v1\"\ncommand = \"probe\"\n",
    )
    .unwrap();
    for args in [&["help"][..], &["--help"], &["-h"]] {
        let out = remuda(&dir, args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("terminal orchestration"));
        assert!(
            String::from_utf8_lossy(&out.stderr)
                .contains("Installed mod commands:\n  remuda probe"),
            "{args:?}: {out:?}"
        );
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn mod_verb_help_does_not_start_a_daemon_or_install_a_mod() {
    let dir = scratch("mod-verb-help");
    for verb in ["install", "list", "info", "test", "update", "remove"] {
        let out = remuda(&dir, &["mod", verb, "--help"]);
        assert!(out.status.success(), "{verb}: {out:?}");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains(&format!("Usage: remuda mod {verb}")),
            "{verb}: {out:?}"
        );
        assert!(out.stderr.is_empty(), "{verb}: {out:?}");
        assert!(
            !remuda_native::daemon::socket_path_in(&dir, "s").exists(),
            "{verb} --help started a daemon"
        );
    }
    assert!(!dir.join("data/remuda/mods").exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn mod_parse_errors_use_exit_two_and_include_help_and_next() {
    let dir = scratch("mod-parse-error");
    for args in [
        &["mod", "list", "--formta", "json"][..],
        &["mod", "install"][..],
    ] {
        let out = remuda(&dir, args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        if args[1] == "list" {
            assert!(stderr.contains("Did you mean '--format'?"), "{stderr}");
        }
        assert!(stderr.contains("Fix: run `remuda mod"), "{stderr}");
        assert!(stderr.contains("Next:"), "{stderr}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

fn assert_invalid_mod_command_has_no_side_effects(tag: &str, args: &[&str]) {
    let dir = scratch(tag);
    let out = remuda(&dir, args);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
    assert!(
        stderr.contains("Usage:") && stderr.contains("Fix:") && stderr.contains("Next:"),
        "{args:?}: {stderr}"
    );
    assert!(
        !dir.join("data/remuda/mods").exists(),
        "{args:?} created the mods directory: {out:?}"
    );
    assert!(
        !remuda_native::daemon::socket_path_in(&dir, "s").exists(),
        "{args:?} created a daemon socket: {out:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn invalid_mod_install_flags_do_not_touch_the_filesystem_or_daemon() {
    assert_invalid_mod_command_has_no_side_effects(
        "mod-install-bogus-flag",
        &["mod", "install", "owner/repo", "--bogus"],
    );
}

#[test]
fn update_all_with_a_name_does_not_touch_the_filesystem_or_daemon() {
    assert_invalid_mod_command_has_no_side_effects(
        "mod-update-all-name",
        &["mod", "update", "--all", "sample"],
    );
}

#[test]
fn remove_extra_argument_does_not_touch_the_filesystem_or_daemon() {
    assert_invalid_mod_command_has_no_side_effects(
        "mod-remove-extra-argument",
        &["mod", "remove", "sample", "extra"],
    );
}

#[test]
fn invalid_mod_list_format_does_not_touch_the_filesystem_or_daemon() {
    assert_invalid_mod_command_has_no_side_effects(
        "mod-list-bad-format",
        &["mod", "list", "--format", "bad"],
    );
}

#[test]
fn stop_parser_handles_valid_flags_help_and_errors_before_connecting() {
    for args in [
        &["stop"][..],
        &["stop", "-f"],
        &["stop", "--force"],
        &["stop", "--yes"],
        &["stop", "--i-am-inside"],
        &["stop", "-f", "--yes", "--i-am-inside"],
    ] {
        let dir = scratch("stop-valid");
        let out = remuda(&dir, args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        assert!(
            !remuda_native::daemon::socket_path_in(&dir, "s").exists(),
            "{args:?} created a daemon socket"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    for (tag, args, code) in [
        ("stop-bogus-flag", &["stop", "--bogus"][..], Some(2)),
        ("stop-extra-arg", &["stop", "stray"], Some(2)),
        ("stop-help", &["stop", "--help"], Some(0)),
    ] {
        let (out, touched) = touches_daemon(tag, args);
        assert!(!touched, "{args:?} connected to the daemon socket");
        assert_eq!(out.status.code(), code, "{args:?}: {out:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        if args[1] == "--help" {
            assert!(stdout.contains("Usage: remuda stop"), "{stdout}");
            assert!(stderr.is_empty(), "{stderr}");
        } else {
            assert!(stderr.contains("Usage: remuda stop"), "{stderr}");
            assert!(stderr.contains("Fix:"), "{stderr}");
            assert!(stderr.contains("Next:"), "{stderr}");
        }
    }
}

#[test]
fn upgrade_help_flags_explain_channels_and_daemon_lifecycle() {
    let dir = scratch("upgrade-help-flags");
    for args in [&["upgrade", "--help"][..], &["upgrade", "-h"]] {
        let out = remuda(&dir, args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("--channel stable|nightly"), "{stdout}");
        assert!(
            stdout.contains("daemon and its sessions") && stdout.contains("remuda stop"),
            "missing daemon/session behavior: {stdout}"
        );
        assert!(
            stdout
                .lines()
                .last()
                .unwrap_or_default()
                .starts_with("Next:"),
            "{stdout}"
        );
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn invalid_upgrade_channel_suggests_supported_channels_and_next_step() {
    let dir = scratch("upgrade-bad-channel");
    let out = remuda(&dir, &["upgrade", "--channel", "beta"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(
            "use --channel stable or --channel nightly, e.g. remuda upgrade --channel nightly"
        ),
        "missing supported channel suggestion: {stderr}"
    );
    assert!(
        stderr
            .lines()
            .last()
            .unwrap_or_default()
            .starts_with("Next:"),
        "{stderr}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn top_level_help_ends_with_one_concrete_next_step() {
    let dir = scratch("help-next-step");
    let out = remuda(&dir, &["help"]);
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let next = stdout
        .lines()
        .filter(|line| line.starts_with("Next:"))
        .collect::<Vec<_>>();
    assert_eq!(next.len(), 1, "expected one Next line: {stdout}");
    assert!(
        next[0].contains("remuda run") && next[0].contains("remuda ls"),
        "Next line should offer a session action: {}",
        next[0]
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn help_lists_the_upgrade_command() {
    let dir = scratch("upgrade-help");
    let out = remuda(&dir, &["help"]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("remuda upgrade"),
        "help omitted upgrade: {out:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn upgrade_explains_daemon_and_session_lifecycle() {
    let dir = scratch("upgrade-message");
    let fake_bin = dir.join("bin");
    std::fs::create_dir_all(&fake_bin).unwrap();
    let curl = fake_bin.join("curl");
    std::fs::write(
        &curl,
        "#!/bin/sh\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = \"-o\" ]; then\n    shift\n    printf '#!/bin/sh\\nexit 0\\n' > \"$1\"\n    exit 0\n  fi\n  shift\ndone\nexit 2\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&curl).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&curl, permissions).unwrap();
    let path = format!(
        "{}:{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let out = Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(["upgrade", "--channel", "nightly"])
        .env("PATH", path)
        .env("REMUDA_RUNTIME_DIR", &dir)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("HOME", &dir)
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        .output()
        .expect("run remuda upgrade");
    assert!(out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("daemon"),
        "missing daemon behavior: {stderr}"
    );
    assert!(
        stderr.contains("session"),
        "missing session behavior: {stderr}"
    );
    assert!(
        stderr.contains("until you run `remuda stop` (that ends those sessions)")
            && stderr.contains("the next remuda command starts the new version"),
        "missing daemon/session lifecycle: {stderr}"
    );
    let next = stderr.lines().last().unwrap_or_default();
    assert!(
        next.starts_with("Next:")
            && next.contains("remuda stop")
            && next.contains("remuda --version"),
        "missing concrete next step: {stderr}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// A bad channel fails before any download, after the handshake would have run.
#[test]
fn upgrade_never_connects_to_a_daemon() {
    let (out, touched) = touches_daemon("upgrade", &["upgrade", "--channel", "bogus"]);
    assert!(
        !out.status.success(),
        "a bogus channel was accepted: {out:?}"
    );
    assert!(!touched, "upgrade connected to the daemon socket");
}

#[test]
fn a_socket_path_over_sun_path_names_the_length_not_a_second_daemon() {
    let dir = scratch("long").join("x".repeat(120));
    std::fs::create_dir_all(&dir).unwrap();

    let out = remuda(&dir, &["ls"]);
    let said = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir_all(dir.parent().unwrap());

    assert!(
        !out.status.success(),
        "ls with an unbindable path succeeded"
    );
    assert!(
        !said.contains("second daemon"),
        "blamed a second daemon: {said}"
    );
    assert!(said.contains("REMUDA_RUNTIME_DIR"), "no cure named: {said}");
}

/// `mod install --reload` reaches the daemon through its own connect path; a
/// local repo stands in for GitHub via git's `insteadOf`.
#[test]
fn mod_reload_over_sun_path_names_the_length_not_a_daemon() {
    let root = scratch("reload");
    let repo = root.join("src/owner/sample.git");
    std::fs::create_dir_all(repo.join("packages/sample")).unwrap();
    std::fs::write(
        repo.join("extension.toml"),
        "name = \"sample\"\nentry = \"packages/sample/init.lua\"\napi = \"remuda-lua-v1\"\nlifecycle = \"remuda-module-v1\"\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("packages/sample/init.lua"),
        "return { api = \"remuda-module-v1\", state_version = 1 }",
    )
    .unwrap();
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .current_dir(&repo)
            .args(args)
            .status()
            .unwrap();
        assert!(ok.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "sample",
    ]);

    let long = root.join("x".repeat(120));
    std::fs::create_dir_all(&long).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(["-s", "s", "mod", "install", "owner/sample", "--reload"])
        .env("REMUDA_RUNTIME_DIR", &long)
        .env("XDG_DATA_HOME", root.join("data"))
        .env("HOME", &root)
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        .env("GIT_CONFIG_COUNT", "1")
        .env(
            "GIT_CONFIG_KEY_0",
            format!("url.file://{}/.insteadOf", root.join("src").display()),
        )
        .env("GIT_CONFIG_VALUE_0", "https://github.com/")
        .output()
        .expect("run remuda");
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        said.contains("installed mod sample"),
        "install itself failed: {said}"
    );
    assert!(
        !said.contains("cannot connect to remuda daemon"),
        "blamed a daemon: {said}"
    );
    assert!(said.contains("REMUDA_RUNTIME_DIR"), "no cure named: {said}");
}

/// #134: a mod directory without a readable manifest (half-swapped during an
/// update) is named, not answered with the generic usage text.
#[test]
fn a_half_installed_mod_is_named_instead_of_generic_usage() {
    for (tag, manifest) in [
        ("nomanifest", None),
        ("badmanifest", Some("this is not toml [")),
    ] {
        let dir = scratch(tag);
        let mod_dir = dir.join("data/remuda/mods/butler");
        std::fs::create_dir_all(mod_dir.join("packages/butler")).unwrap();
        if let Some(text) = manifest {
            std::fs::write(mod_dir.join("extension.toml"), text).unwrap();
        }
        let out = remuda(&dir, &["butler", "--headless"]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{tag}: {stderr}");
        assert!(stderr.contains("mod 'butler' at"), "{tag}: {stderr}");
        assert!(
            stderr.contains(&mod_dir.display().to_string()),
            "{tag}: {stderr}"
        );
        assert!(
            stderr.contains("partially installed or mid-update"),
            "{tag}: {stderr}"
        );
        assert!(
            !stderr.contains("terminal orchestration"),
            "{tag}: generic usage: {stderr}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A word that names no mod directory at all gets the concise diagnostic.
    let dir = scratch("unknown-word");
    let out = remuda(&dir, &["nosuchverb"]);
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "remuda: no command or mod named nosuchverb.\nNext: if nosuchverb is a mod, install it with remuda mod install OWNER/REPO; installed mods: remuda mod list."
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// PR #146 / butler-qa half134 rows 5-6: one half-installed mod must not take
/// down its siblings. The scan skips it with a warning naming it, a healthy
/// mod's command still dispatches, and the bad mod's own command still gets
/// the named error.
#[test]
fn a_half_installed_sibling_does_not_hide_healthy_mods() {
    let dir = scratch("sibling");
    let mods = dir.join("data/remuda/mods");
    std::fs::create_dir_all(mods.join("butler/packages/butler")).unwrap(); // no manifest
    std::fs::create_dir_all(mods.join("probe/packages/probe")).unwrap();
    std::fs::write(
        mods.join("probe/extension.toml"),
        "name = \"probe\"\nentry = \"packages/probe/init.lua\"\napi = \"remuda-lua-v1\"\ncommand = \"probe\"\n",
    )
    .unwrap();
    std::fs::write(
        mods.join("probe/packages/probe/init.lua"),
        "remuda.extension_command('probe', function(args) return 'pong ' .. (args[1] or '') end)",
    )
    .unwrap();

    let help = remuda(&dir, &["help"]);
    let help_err = String::from_utf8_lossy(&help.stderr);
    assert!(
        help_err.contains("probe"),
        "help lists the healthy mod: {help_err}"
    );
    assert!(
        help_err.contains(&mods.join("butler").display().to_string()),
        "and names the bad dir: {help_err}"
    );

    let launch = remuda(&dir, &["probe", "--headless"]);
    let ping = remuda(&dir, &["probe", "ping"]);
    let bad = remuda(&dir, &["butler"]);
    remuda(&dir, &["stop", "-f"]);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        launch.status.success(),
        "{}",
        String::from_utf8_lossy(&launch.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&ping.stdout).trim(),
        "pong ping",
        "{}",
        String::from_utf8_lossy(&ping.stderr)
    );
    let bad_err = String::from_utf8_lossy(&bad.stderr);
    assert!(
        !bad.status.success() && bad_err.contains("mod 'butler' at"),
        "{bad_err}"
    );
}

#[test]
fn underscore_mod_command_dispatches() {
    let dir = scratch("underscore-mod-command");
    let mods = dir.join("data/remuda/mods");
    let package = mods.join("my_mod");
    std::fs::create_dir_all(package.join("packages/my_mod")).unwrap();
    std::fs::write(
        package.join("extension.toml"),
        "name = \"my_mod\"\nentry = \"packages/my_mod/init.lua\"\napi = \"remuda-lua-v1\"\ncommand = \"my_mod\"\n",
    )
    .unwrap();
    std::fs::write(
        package.join("packages/my_mod/init.lua"),
        "remuda.extension_command('my_mod', function(args) return 'pong ' .. (args[1] or '') end)",
    )
    .unwrap();

    let launch = remuda(&dir, &["my_mod", "--headless"]);
    let ping = remuda(&dir, &["my_mod", "ping"]);
    let _ = remuda(&dir, &["stop", "-f"]);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        launch.status.success(),
        "{}",
        String::from_utf8_lossy(&launch.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&ping.stdout).trim(),
        "pong ping",
        "{}",
        String::from_utf8_lossy(&ping.stderr)
    );
}

/// #150: a half-installed dependency manifest names its owner, dependency,
/// and path instead of returning only the raw parse failure.
#[test]
fn a_corrupt_required_manifest_names_owner_dependency_and_path() {
    let dir = scratch("bad-requirement");
    let mods = dir.join("data/remuda/mods");
    let owner = mods.join("A");
    let dependency = mods.join("B");
    std::fs::create_dir_all(owner.join("packages/A")).unwrap();
    std::fs::write(
        owner.join("extension.toml"),
        "name = \"A\"\nversion = \"1.0.0\"\nentry = \"packages/A/init.lua\"\napi = \"remuda-lua-v1\"\nrequires = { B = \">=1.0\" }\n",
    )
    .unwrap();
    std::fs::write(
        owner.join("packages/A/init.lua"),
        "return { api = \"remuda-lua-v1\" }",
    )
    .unwrap();
    std::fs::create_dir_all(dependency.join("packages/B")).unwrap();
    let dependency_manifest = dependency.join("extension.toml");
    std::fs::write(&dependency_manifest, "version = \"1.0.0\"\n").unwrap();

    let out = remuda(&dir, &["exec", "A"]);
    let error = String::from_utf8_lossy(&out.stderr);
    let _ = remuda(&dir, &["stop", "-f"]);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        !out.status.success(),
        "executed despite a corrupt dependency manifest"
    );
    assert!(error.contains("A"), "owner is not named: {error}");
    assert!(error.contains("B"), "dependency is not named: {error}");
    assert!(
        error.contains(&dependency_manifest.display().to_string()),
        "manifest path is not named: {error}"
    );
}
