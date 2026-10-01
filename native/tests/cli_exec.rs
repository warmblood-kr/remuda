//! `remuda exec NAME` must activate a lifecycle mod the same way
//! `remuda.exec(NAME)` does in the image — not evaluate its entry as a plain
//! chunk and discard the declaration. #98 item 3.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_STDIN_FIXTURE: AtomicU64 = AtomicU64::new(0);

#[test]
fn cli_exec_activates_a_lifecycle_mod() {
    let dir = std::env::temp_dir().join(format!("rcx-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let mod_dir = dir.join("data/remuda/mods/sample");
    fs::create_dir_all(mod_dir.join("packages/sample")).unwrap();
    fs::write(
        mod_dir.join("extension.toml"),
        "name = \"sample\"\nentry = \"packages/sample/init.lua\"\napi = \"remuda-lua-v1\"\nlifecycle = \"remuda-module-v1\"\n",
    )
    .unwrap();
    fs::write(
        mod_dir.join("packages/sample/init.lua"),
        r#"return {
          api = "remuda-module-v1", state_version = 1,
          initialize = function() return { starts = 0 } end,
          start = function(state) state.starts = state.starts + 1 end,
          tools = {{ name = "sample_start_count",
            about = "Read how often this module started.",
            run = function(state) return tostring(state.starts) end }},
        }"#,
    )
    .unwrap();
    let remuda = |args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", "s"])
            .args(args)
            .env("REMUDA_RUNTIME_DIR", &dir)
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("HOME", &dir)
            .output()
            .expect("run remuda")
    };

    let exec = remuda(&["exec", "sample"]);
    let starts = remuda(&["-e", "return remuda.tools.sample_start_count()"]);
    remuda(&["stop", "-f"]);
    let _ = fs::remove_dir_all(&dir);

    assert!(exec.status.success(), "exec failed: {exec:?}");
    assert_eq!(
        String::from_utf8_lossy(&starts.stdout).trim(),
        "1",
        "the mod was not activated (tool missing or start not run): {starts:?}"
    );
}

#[test]
fn cli_exec_waits_for_lifecycle_readiness_and_reports_failures() {
    use std::time::{Duration, Instant};

    let dir = std::env::temp_dir().join(format!("rc-ready-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let write_mod = |name: &str, source: &str| {
        let mod_dir = dir.join(format!("data/remuda/mods/{name}"));
        fs::create_dir_all(mod_dir.join(format!("packages/{name}"))).unwrap();
        fs::write(
            mod_dir.join("extension.toml"),
            format!(
                "name = \"{name}\"\nentry = \"packages/{name}/init.lua\"\napi = \"remuda-lua-v1\"\nlifecycle = \"remuda-module-v1\"\n"
            ),
        )
        .unwrap();
        fs::write(mod_dir.join(format!("packages/{name}/init.lua")), source).unwrap();
    };
    let declaration = |fields: &str| {
        format!(
            "return {{ api = \"remuda-module-v1\", state_version = 1, initialize = function() return {{ polls = 0 }} end, {fields} }}"
        )
    };
    write_mod("no_ready", &declaration("start = function() end,"));
    write_mod(
        "ready_now",
        &declaration("ready = function() return true end, timeout_ms = 1000,"),
    );
    write_mod(
        "ready_later",
        &declaration(
            "ready = function(state) state.polls = state.polls + 1; return state.polls >= 4 and true or nil end, timeout_ms = 3000,",
        ),
    );
    write_mod(
        "ready_fail",
        &declaration("ready = function() return nil, 'connection refused' end, timeout_ms = 1000,"),
    );
    write_mod(
        "ready_error",
        &declaration("ready = function() error('readiness exploded') end, timeout_ms = 1000,"),
    );
    write_mod(
        "ready_timeout",
        &declaration("ready = function() return nil end, timeout_ms = 50,"),
    );
    let remuda = |args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", "s"])
            .args(args)
            .env("REMUDA_RUNTIME_DIR", &dir)
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("HOME", &dir)
            .output()
            .expect("run remuda")
    };
    let boot = remuda(&["-e", "return remuda.session.list()"]);
    assert!(boot.status.success(), "boot private daemon: {boot:?}");

    let no_ready_started = Instant::now();
    let no_ready = remuda(&["exec", "no_ready"]);
    assert!(
        no_ready.status.success(),
        "no ready declaration: {no_ready:?}"
    );
    assert!(
        no_ready_started.elapsed() < Duration::from_secs(3),
        "a mod without ready must keep immediate completion: {no_ready:?}"
    );

    let ready = remuda(&["exec", "ready_now"]);
    assert!(ready.status.success(), "immediately ready: {ready:?}");

    let wait_started = Instant::now();
    let later = remuda(&["exec", "ready_later"]);
    assert!(later.status.success(), "eventually ready: {later:?}");
    assert!(
        wait_started.elapsed() >= Duration::from_millis(500),
        "exec did not poll between Eval requests: {later:?}"
    );

    let failed = remuda(&["exec", "ready_fail"]);
    assert_eq!(failed.status.code(), Some(1), "{failed:?}");
    assert_eq!(
        failed.stderr,
        b"mod ready_fail failed to become ready: connection refused\n"
    );

    let errored = remuda(&["exec", "ready_error"]);
    assert_eq!(errored.status.code(), Some(1), "{errored:?}");
    assert!(
        String::from_utf8_lossy(&errored.stderr)
            .starts_with("mod ready_error failed to become ready: "),
        "thrown readiness error was not reported as startup failure: {errored:?}"
    );
    assert!(
        String::from_utf8_lossy(&errored.stderr).contains("readiness exploded"),
        "thrown readiness error message was lost: {errored:?}"
    );

    let timed_out = remuda(&["exec", "ready_timeout"]);
    assert_eq!(timed_out.status.code(), Some(124), "{timed_out:?}");
    assert_eq!(
        timed_out.stderr,
        b"mod ready_timeout did not become ready within 0.05s\n"
    );

    remuda(&["stop", "-f"]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cli_exec_reports_timeout_declaration_errors_cleanly() {
    let dir = std::env::temp_dir().join(format!("rc-ready-timeout-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    for (name, fields) in [
        (
            "invalid_timeout",
            "ready = function() return nil end, timeout_ms = 0,",
        ),
        ("timeout_without_ready", "timeout_ms = 1000,"),
    ] {
        let mod_dir = dir.join(format!("data/remuda/mods/{name}"));
        fs::create_dir_all(mod_dir.join(format!("packages/{name}"))).unwrap();
        fs::write(
            mod_dir.join("extension.toml"),
            format!(
                "name = \"{name}\"\nentry = \"packages/{name}/init.lua\"\napi = \"remuda-lua-v1\"\nlifecycle = \"remuda-module-v1\"\n"
            ),
        )
        .unwrap();
        fs::write(
            mod_dir.join(format!("packages/{name}/init.lua")),
            format!(
                "return {{ api = \"remuda-module-v1\", state_version = 1, initialize = function() return {{}} end, {fields} }}"
            ),
        )
        .unwrap();
    }
    let remuda = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", "s"])
            .args(args)
            .env("REMUDA_RUNTIME_DIR", &dir)
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("HOME", &dir)
            .output()
            .expect("run remuda")
    };

    let boot = remuda(&["-e", "return remuda.session.list()"]);
    assert!(boot.status.success(), "boot private daemon: {boot:?}");
    let invalid_timeout = remuda(&["exec", "invalid_timeout"]);
    assert_eq!(
        invalid_timeout.status.code(),
        Some(1),
        "{invalid_timeout:?}"
    );
    assert_eq!(
        invalid_timeout.stderr,
        b"module timeout_ms must be an integer from 1 through 240000\n"
    );
    let timeout_without_ready = remuda(&["exec", "timeout_without_ready"]);
    assert_eq!(
        timeout_without_ready.status.code(),
        Some(1),
        "{timeout_without_ready:?}"
    );
    assert_eq!(
        timeout_without_ready.stderr,
        b"module timeout_ms requires a ready function\n"
    );

    remuda(&["stop", "-f"]);
    let _ = fs::remove_dir_all(&dir);
}

/// The one-line `remuda.exec(...)` wrapper must not borrow the mod's chunk
/// name: a traceback then blamed `packages/<mod>/init.lua:1: in main chunk`
/// for a line the mod never had. The mod's own frames keep their names.
#[test]
fn cli_exec_error_names_the_wrapper_not_the_mod_entry() {
    let dir = std::env::temp_dir().join(format!("rcy-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let mod_dir = dir.join("data/remuda/mods/broken");
    fs::create_dir_all(mod_dir.join("packages/broken")).unwrap();
    fs::write(
        mod_dir.join("extension.toml"),
        "name = \"broken\"\nentry = \"packages/broken/init.lua\"\napi = \"remuda-lua-v1\"\nlifecycle = \"remuda-module-v1\"\n",
    )
    .unwrap();
    fs::write(
        mod_dir.join("packages/broken/init.lua"),
        r#"return {
          api = "remuda-module-v1", state_version = 1,
          initialize = function() return {} end,
          start = function() error("broken start") end,
        }"#,
    )
    .unwrap();
    let remuda = |args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", "s"])
            .args(args)
            .env("REMUDA_RUNTIME_DIR", &dir)
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("HOME", &dir)
            .output()
            .expect("run remuda")
    };

    let exec = remuda(&["exec", "broken"]);
    remuda(&["stop", "-f"]);
    let _ = fs::remove_dir_all(&dir);

    let stderr = String::from_utf8_lossy(&exec.stderr);
    assert!(!exec.status.success(), "{stderr}");
    assert!(stderr.contains("broken start"), "{stderr}");
    assert!(
        stderr.contains("init.lua\"]:4"),
        "the mod's frame keeps its name: {stderr}"
    );
    assert!(
        !stderr.contains("init.lua:1: in main chunk"),
        "the wrapper borrowed the mod's chunk name: {stderr}"
    );
    assert!(stderr.contains("remuda exec:1:"), "{stderr}");
}

/// `start` that errors *indirectly* — by emitting an event whose hook errors —
/// must fail `exec` the same as a `start` that errors directly. #162.
#[test]
fn cli_exec_fails_when_a_start_hook_errors() {
    let dir = std::env::temp_dir().join(format!("rcz-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let mod_dir = dir.join("data/remuda/mods/hookbroken");
    fs::create_dir_all(mod_dir.join("packages/hookbroken")).unwrap();
    fs::write(
        mod_dir.join("extension.toml"),
        "name = \"hookbroken\"\nentry = \"packages/hookbroken/init.lua\"\napi = \"remuda-lua-v1\"\nlifecycle = \"remuda-module-v1\"\n",
    )
    .unwrap();
    fs::write(
        mod_dir.join("packages/hookbroken/init.lua"),
        r#"return {
          api = "remuda-module-v1", state_version = 1,
          initialize = function() return {} end,
          start = function()
            remuda.on("some-event", function() error("hook exploded") end)
            remuda.emit("some-event")
          end,
        }"#,
    )
    .unwrap();
    let remuda = |args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", "s"])
            .args(args)
            .env("REMUDA_RUNTIME_DIR", &dir)
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("HOME", &dir)
            .output()
            .expect("run remuda")
    };

    let exec = remuda(&["exec", "hookbroken"]);
    remuda(&["stop", "-f"]);
    let _ = fs::remove_dir_all(&dir);

    let stderr = String::from_utf8_lossy(&exec.stderr);
    assert!(!exec.status.success(), "{stderr}");
    assert!(stderr.contains("hook exploded"), "{stderr}");
}

#[test]
fn typed_failures_print_only_the_message_and_use_the_requested_exit_code() {
    let dir = std::env::temp_dir().join(format!("rc-typed-failure-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let mod_dir = dir.join("data/remuda/mods/sample");
    fs::create_dir_all(mod_dir.join("packages/sample")).unwrap();
    fs::write(
        mod_dir.join("extension.toml"),
        "name = \"sample\"\nentry = \"packages/sample/init.lua\"\napi = \"remuda-lua-v1\"\ncommand = \"sample\"\n",
    )
    .unwrap();
    fs::write(
        mod_dir.join("packages/sample/init.lua"),
        "remuda.extension_command('sample', function() remuda.fail('extension rejected', 23) end)",
    )
    .unwrap();
    let remuda = |args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", "s"])
            .args(args)
            .env("REMUDA_RUNTIME_DIR", &dir)
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("HOME", &dir)
            .output()
            .expect("run remuda")
    };

    let boot = remuda(&["-e", "remuda.new('bootstrap')"]);
    assert!(
        boot.status.success(),
        "explicitly start private daemon: {boot:?}"
    );
    let eval = remuda(&["-e", "remuda.fail('eval rejected', 17)"]);
    let default_code = remuda(&["-e", "remuda.fail('default rejected')"]);
    let zero_code = remuda(&["-e", "remuda.fail('zero rejected', 0)"]);
    let invalid_code = remuda(&["-e", "remuda.fail('invalid rejected', 256)"]);
    let upper_code = remuda(&["-e", "remuda.fail('upper rejected', 255)"]);
    let _ = remuda(&["exec", "sample"]);
    let extension = remuda(&["sample", "run"]);
    let docs = remuda(&["doc", "--format", "markdown"]);
    remuda(&["stop", "-f"]);
    let _ = fs::remove_dir_all(&dir);

    for (output, expected_message, expected_code) in [
        (eval, "eval rejected\n", 17),
        (extension, "extension rejected\n", 23),
    ] {
        assert_eq!(output.status.code(), Some(expected_code), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert_eq!(output.stderr, expected_message.as_bytes(), "{output:?}");
    }
    assert_eq!(default_code.status.code(), Some(1), "{default_code:?}");
    assert_eq!(
        default_code.stderr, b"default rejected\n",
        "{default_code:?}"
    );
    assert_eq!(zero_code.status.code(), Some(1), "{zero_code:?}");
    assert_eq!(invalid_code.status.code(), Some(1), "{invalid_code:?}");
    assert!(
        String::from_utf8_lossy(&invalid_code.stderr)
            .contains("remuda.fail exit code must be an integer from 1 through 255"),
        "{invalid_code:?}"
    );
    assert_eq!(upper_code.status.code(), Some(255), "{upper_code:?}");
    assert_eq!(upper_code.stderr, b"upper rejected\n", "{upper_code:?}");
    assert!(docs.status.success(), "{docs:?}");
    assert!(
        String::from_utf8_lossy(&docs.stdout).contains("fail(message, code?)"),
        "remuda doc omitted remuda.fail: {docs:?}"
    );
}

fn stdin_cli(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_remuda"));
    command
        .args(["-s", "s"])
        .args(args)
        .env("REMUDA_RUNTIME_DIR", dir)
        .env("REMUDA_SUPPRESS_DEPRECATIONS", "1")
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("HOME", dir);
    command
}

fn setup_stdin_fixture() -> std::path::PathBuf {
    // These tests run in parallel in the same integration-test process. Give
    // each fixture its own runtime directory so one test cannot remove the
    // other's loaded mod or daemon data.
    let fixture_id = NEXT_STDIN_FIXTURE.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rc-stdin-{}-{fixture_id}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let mod_dir = dir.join("data/remuda/mods/sample");
    fs::create_dir_all(mod_dir.join("packages/sample")).unwrap();
    fs::write(
        mod_dir.join("extension.toml"),
        "name = \"sample\"\nentry = \"packages/sample/init.lua\"\napi = \"remuda-lua-v1\"\ncommand = \"sample\"\n",
    )
    .unwrap();
    fs::write(
        mod_dir.join("packages/sample/init.lua"),
        r#"remuda.extension_command("sample", function(args, caller)
          if args[1] == "-" then return caller.stdin or "<missing>" end
          if args[1] == "y" then
            return table.concat(args, "|") .. "|" .. (caller.stdin == nil and "no-stdin" or "stdin-set")
          end
          if args[1] == "bytes" then
            local bytes = {}
            for i = 1, #caller.stdin do bytes[#bytes + 1] = tostring(string.byte(caller.stdin, i)) end
            return table.concat(bytes, ",")
          end
          return caller.stdin == nil and "<nil>" or "unexpected stdin"
        end)"#,
    )
    .unwrap();

    let boot = stdin_cli(&dir, &["-e", "remuda.session.list()"])
        .output()
        .expect("start daemon");
    assert!(boot.status.success(), "daemon boot failed: {boot:?}");
    let loaded = stdin_cli(&dir, &["exec", "sample"])
        .output()
        .expect("load mod");
    assert!(loaded.status.success(), "mod load failed: {loaded:?}");
    dir
}

fn cleanup_stdin_fixture(dir: &Path) {
    let _ = stdin_cli(dir, &["stop", "-f"]).output();
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn extension_commands_receive_stdin_with_dash_and_enforce_limit() {
    use std::io::Write;

    const MAX_STDIN: usize = 1024 * 1024;
    let dir = setup_stdin_fixture();

    let payload = b"message from pipe\nwith \"quotes\" and backslash \\";
    let mut command = stdin_cli(&dir, &["sample", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run extension command with piped stdin");
    command
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(payload)
        .expect("write stdin payload");
    let output = command.wait_with_output().expect("extension result");
    assert!(output.status.success(), "stdin command failed: {output:?}");
    assert_eq!(output.stdout, [payload.as_slice(), b"\n"].concat());

    let oversized = vec![b'x'; MAX_STDIN + 1];
    let mut command = stdin_cli(&dir, &["sample", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run oversized extension command");
    command
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(&oversized)
        .expect("write oversized stdin");
    let output = command
        .wait_with_output()
        .expect("oversized extension result");
    assert!(
        !output.status.success(),
        "oversized stdin should be rejected"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("stdin exceeds 1 MiB limit"),
        "unexpected oversized stdin error: {output:?}"
    );

    cleanup_stdin_fixture(&dir);
}

#[test]
fn extension_commands_accept_binary_stdin_only_when_requested() {
    use std::io::Write;

    let dir = setup_stdin_fixture();
    let bytes = [0, 0xff, b'A'];
    let mut command = stdin_cli(&dir, &["--stdin", "sample", "bytes"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run extension command with binary stdin");
    command
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(&bytes)
        .expect("write binary stdin payload");
    let output = command.wait_with_output().expect("binary stdin result");
    assert!(
        output.status.success(),
        "binary stdin command failed: {output:?}"
    );
    assert_eq!(output.stdout, b"0,255,65\n");

    // Keep stdin open: an ordinary extension command must not wait for EOF.
    let mut command = stdin_cli(&dir, &["sample", "no-stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run extension command without stdin opt-in");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let status = loop {
        if let Some(status) = command.try_wait().expect("poll extension command") {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = command.kill();
            panic!("extension command blocked on an unrequested stdin pipe");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert!(status.success(), "no-stdin extension failed: {status:?}");
    let output = command.wait_with_output().expect("no-stdin command result");
    assert_eq!(output.stdout, b"<nil>\n");
    cleanup_stdin_fixture(&dir);
}

#[test]
fn extension_command_rejects_trailing_stdin_flag_with_usage_hint() {
    let dir = setup_stdin_fixture();
    let output = stdin_cli(&dir, &["sample", "--stdin"])
        .output()
        .expect("run extension command with misplaced stdin flag");
    assert!(
        !output.status.success(),
        "misplaced --stdin should be rejected"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("usage: remuda --stdin MOD [ARGS…]")
            && stderr.contains("put --stdin before the mod command"),
        "misplaced --stdin should show a usage hint: {output:?}"
    );
    cleanup_stdin_fixture(&dir);
}

#[test]
fn extension_command_separator_passes_stdin_flag_as_a_literal_argument() {
    let dir = setup_stdin_fixture();
    let output = stdin_cli(&dir, &["sample", "y", "--", "--stdin"])
        .output()
        .expect("run extension command with literal stdin flag");
    assert!(
        output.status.success(),
        "literal --stdin failed: {output:?}"
    );
    assert_eq!(output.stdout, b"y|--|--stdin|no-stdin\n");
    cleanup_stdin_fixture(&dir);
}

#[test]
fn extension_command_separator_passes_dash_as_a_literal_argument() {
    let dir = setup_stdin_fixture();
    let output = stdin_cli(&dir, &["sample", "y", "--", "-"])
        .output()
        .expect("run extension command with literal dash argument");
    assert!(output.status.success(), "literal dash failed: {output:?}");
    assert_eq!(output.stdout, b"y|--|-|no-stdin\n");
    cleanup_stdin_fixture(&dir);
}

/// #394: a fresh isolated home whose FIRST command is the mod subcommand under
/// test. Dropping it stops its private daemon, also when an assert panics.
struct FreshHome(std::path::PathBuf);

impl FreshHome {
    fn new(label: &str, manifest_extra: &str, entry: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("rc-first-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mod_dir = dir.join("data/remuda/mods/sample");
        fs::create_dir_all(mod_dir.join("packages/sample")).unwrap();
        fs::write(
            mod_dir.join("extension.toml"),
            format!(
                "name = \"sample\"\nentry = \"packages/sample/init.lua\"\napi = \"remuda-lua-v1\"\ncommand = \"sample\"\n{manifest_extra}"
            ),
        )
        .unwrap();
        fs::write(mod_dir.join("packages/sample/init.lua"), entry).unwrap();
        Self(dir)
    }

    fn remuda(&self, args: &[&str]) -> Output {
        stdin_cli(&self.0, args)
            .env("REMUDA_NO_UPDATE_CHECK", "1")
            .output()
            .expect("run remuda")
    }
}

impl Drop for FreshHome {
    fn drop(&mut self) {
        let _ = self.remuda(&["stop", "-f"]);
        let _ = fs::remove_dir_all(&self.0);
    }
}

const LIFECYCLE: &str = "lifecycle = \"remuda-module-v1\"\n";

#[test]
fn first_mod_subcommand_on_a_fresh_daemon_loads_the_mod() {
    for (label, args, expected) in [
        ("doctor", &["sample", "doctor"][..], "handled doctor\n"),
        (
            "matrix",
            &["sample", "matrix", "setup"][..],
            "handled matrix setup\n",
        ),
    ] {
        let home = FreshHome::new(
            label,
            LIFECYCLE,
            r#"return {
              api = "remuda-module-v1", state_version = 1,
              initialize = function() return {} end,
              start = function()
                remuda.extension_command("sample", function(args)
                  return "handled " .. table.concat(args, " ")
                end)
              end,
            }"#,
        );

        let first = home.remuda(args);

        let stderr = String::from_utf8_lossy(&first.stderr);
        assert!(!stderr.contains("stack traceback"), "{first:?}");
        assert!(first.status.success(), "{first:?}");
        assert_eq!(String::from_utf8_lossy(&first.stdout), expected);

        // The load is never silent, and it happens once.
        let second = home.remuda(args);
        let notice = "remuda: started mod sample\n";
        assert!(stderr.contains(notice), "{first:?}");
        assert!(
            !String::from_utf8_lossy(&second.stderr).contains(notice),
            "{second:?}"
        );
        assert_eq!(String::from_utf8_lossy(&second.stdout), expected);
    }
}

#[test]
fn mod_subcommand_that_cannot_load_prints_one_line_and_next() {
    let failing_start = r#"return {
      api = "remuda-module-v1", state_version = 1,
      initialize = function() return {} end,
      start = function() error("start exploded") end,
    }"#;
    // A legacy entry registers its command, then fails.
    let failing_entry =
        "remuda.extension_command('sample', function() return 'half' end)\nerror('entry exploded')";
    for (label, manifest_extra, entry) in [
        ("fail", LIFECYCLE, failing_start),
        ("half", "", failing_entry),
    ] {
        cannot_load_prints_one_line_and_next(&FreshHome::new(label, manifest_extra, entry));
    }
}

fn cannot_load_prints_one_line_and_next(home: &FreshHome) {
    let first = home.remuda(&["sample", "doctor"]);
    let registered = home.remuda(&["-e", "return remuda._extension_commands.sample == nil"]);

    let stderr = String::from_utf8_lossy(&first.stderr);
    // The autostart notice is the daemon's, not the failure's.
    let lines: Vec<&str> = stderr
        .lines()
        .filter(|line| !line.starts_with("remuda: started a daemon"))
        .collect();
    assert!(!first.status.success(), "{first:?}");
    assert!(!stderr.contains("stack traceback"), "{first:?}");
    assert_eq!(lines.len(), 2, "one line plus Next: {first:?}");
    assert!(lines[0].contains("sample"), "{first:?}");
    assert!(lines[1].starts_with("Next: "), "{first:?}");
    assert_eq!(
        String::from_utf8_lossy(&registered.stdout).trim(),
        "true",
        "a failed load left a half-registered command: {registered:?}"
    );
}

#[test]
fn loaded_legacy_mod_is_not_rerun_by_a_subcommand() {
    let home = FreshHome::new(
        "legacy",
        "",
        "remuda._sample_runs = (remuda._sample_runs or 0) + 1\n\
         remuda.extension_command('sample', function() return tostring(remuda._sample_runs) end)",
    );

    let first = home.remuda(&["sample", "count"]);
    let second = home.remuda(&["sample", "count"]);

    assert!(first.status.success(), "{first:?}");
    assert_eq!(String::from_utf8_lossy(&first.stdout), "1\n");
    assert_eq!(String::from_utf8_lossy(&second.stdout), "1\n", "{second:?}");
}
