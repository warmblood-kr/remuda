//! Child environment handling for both public process APIs.

use remuda_core::protocol::{Request, Response};
use remuda_native::{client, daemon};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[path = "daemon_support/spawn.rs"]
mod spawn;

const PROBE_KEY: &str = "REMUDA_PROCESS_ENV_PROBE";
const PROBE_PREFIX: &str = "PROCESS_ENV_RESULT=";
static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(1);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let root = if cfg!(unix) {
            PathBuf::from("/tmp")
                .canonicalize()
                .expect("canonical /tmp")
        } else {
            std::env::temp_dir()
        };
        let path = root.join(format!(
            "remuda-pe{}-{}",
            std::process::id(),
            NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create process env scratch");
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Node {
    _daemon: spawn::Daemon,
    scratch: Scratch,
}

impl Node {
    fn start() -> Self {
        let scratch = Scratch::new();
        let mut command = spawn::base_command(&scratch.0);
        command.env("REMUDA_PROCESS_ENV_PARENT", "inherited");
        let daemon = spawn::spawn_and_wait(command, &scratch.0);
        Self {
            _daemon: daemon,
            scratch,
        }
    }

    fn eval(&self, code: &str) -> String {
        let socket = daemon::socket_path_in(&self.scratch.0, "s");
        let request = Request::Eval {
            code: code.to_string(),
            name: None,
        };
        match client::request(&socket, &request).expect("eval request") {
            Response::Value(value) => value,
            other => panic!("eval failed: {other:?}"),
        }
    }

    fn error_of(&self, code: &str) -> String {
        self.eval(&format!(
            "local ok, err = pcall(function() {code} end); return ok and 'no error' or tostring(err)"
        ))
    }
}

fn lua_string(value: &str) -> String {
    format!("{value:?}")
}

fn probe_argv() -> String {
    format!(
        "{{ {}, '--exact', 'process_env_child_probe', '--nocapture' }}",
        lua_string(
            &std::env::current_exe()
                .expect("test binary path")
                .to_string_lossy()
        )
    )
}

fn probe_environment(line: &str) -> BTreeMap<String, String> {
    let json = line
        .strip_prefix(PROBE_PREFIX)
        .unwrap_or_else(|| panic!("probe result prefix missing in {line:?}"));
    let mut environment: BTreeMap<String, String> =
        serde_json::from_str(json).expect("probe environment JSON");
    // LLVM's coverage runtime adds this marker when an instrumented child
    // starts, even if the parent's clear_env removed it before exec.
    environment.remove("__LLVM_PROFILE_RT_INIT_ONCE");
    environment
}

fn extract_probe(stdout: &str) -> BTreeMap<String, String> {
    probe_environment(
        stdout
            .lines()
            .find(|line| line.starts_with(PROBE_PREFIX))
            .unwrap_or_else(|| panic!("child did not report environment: {stdout:?}")),
    )
}

fn clear_env_entries() -> BTreeMap<String, String> {
    let env = BTreeMap::from([
        (PROBE_KEY.to_string(), "1".to_string()),
        (
            "REMUDA_PROCESS_ENV_EXTRA".to_string(),
            "child-only".to_string(),
        ),
    ]);
    #[cfg(windows)]
    let env = {
        let mut env = env;
        for name in ["SystemRoot", "PATH"] {
            if let Some(value) = std::env::var_os(name) {
                env.insert(name.to_string(), value.to_string_lossy().into_owned());
            }
        }
        env
    };
    env
}

fn expected_after_clear(requested: BTreeMap<String, String>) -> BTreeMap<String, String> {
    // macOS adds this per-user encoding marker when a Rust test executable
    // starts, even when the parent launches it with an otherwise empty env.
    #[cfg(target_os = "macos")]
    let requested = {
        let mut requested = requested;
        if let Ok(value) = std::env::var("__CF_USER_TEXT_ENCODING") {
            requested.insert("__CF_USER_TEXT_ENCODING".to_string(), value);
        }
        requested
    };
    requested
}

fn lua_env(entries: &BTreeMap<String, String>) -> String {
    let fields = entries
        .iter()
        .map(|(key, value)| format!("[{}] = {}", lua_string(key), lua_string(value)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{{ {fields} }}")
}

#[test]
fn process_env_child_probe() {
    if std::env::var_os(PROBE_KEY).is_none() {
        return;
    }
    let vars: BTreeMap<String, String> = std::env::vars().collect();
    // The leading newline keeps the result on its own line even when libtest
    // has printed "test <name> ... " first (one test thread).
    println!("\n{PROBE_PREFIX}{}", serde_json::to_string(&vars).unwrap());
}

#[test]
fn process_env_probe_ignores_llvm_coverage_startup_marker() {
    assert_eq!(
        probe_environment(
            "PROCESS_ENV_RESULT={\"KEEP\":\"value\",\"__LLVM_PROFILE_RT_INIT_ONCE\":\"__LLVM_PROFILE_RT_INIT_ONCE\"}"
        ),
        BTreeMap::from([("KEEP".to_string(), "value".to_string())])
    );
}

#[test]
fn process_run_adds_env_on_top_of_the_inherited_environment() {
    let node = Node::start();
    let output = node.eval(&format!(
        "local result = remuda.process.run({{ argv = {}, env = {{ {PROBE_KEY} = '1', REMUDA_PROCESS_ENV_EXTRA = 'child-only' }} }}); return tostring(result.code) .. '\\n' .. result.stdout",
        probe_argv()
    ));
    let (status, stdout) = output.split_once('\n').expect("run status and stdout");
    assert_eq!(status, "0", "{output}");
    let env = extract_probe(stdout);
    assert_eq!(
        env.get("REMUDA_PROCESS_ENV_PARENT").map(String::as_str),
        Some("inherited")
    );
    assert_eq!(
        env.get("REMUDA_PROCESS_ENV_EXTRA").map(String::as_str),
        Some("child-only")
    );
}

#[test]
fn process_run_clear_env_passes_exactly_the_requested_variables() {
    let node = Node::start();
    let expected = clear_env_entries();
    let output = node.eval(&format!(
        "local result = remuda.process.run({{ argv = {}, env = {}, clear_env = true }}); return tostring(result.code) .. '\\n' .. result.stdout",
        probe_argv(),
        lua_env(&expected)
    ));
    let (status, stdout) = output.split_once('\n').expect("run status and stdout");
    assert_eq!(status, "0", "{output}");
    assert_eq!(extract_probe(stdout), expected_after_clear(expected));
}

#[test]
fn process_async_applies_additive_and_cleared_environments() {
    let node = Node::start();
    let argv = probe_argv();
    for (clear, env) in [
        (
            false,
            BTreeMap::from([
                (PROBE_KEY.to_string(), "1".to_string()),
                (
                    "REMUDA_PROCESS_ENV_EXTRA".to_string(),
                    "child-only".to_string(),
                ),
            ]),
        ),
        (true, clear_env_entries()),
    ]
    .into_iter()
    {
        let lua = format!(
            "process_env_lines = {{}}; process_env_done = false; \
             remuda.on('process-env-line', function(line) table.insert(process_env_lines, line) end); \
             remuda.on('process-env-exit', function() process_env_done = true end); \
             remuda.process({{ argv = {argv}, env = {env}, clear_env = {clear}, on_line = 'process-env-line', on_exit = 'process-env-exit' }}); return 'started'",
            env = lua_env(&env),
            clear = if clear { "true" } else { "false" },
        );
        assert_eq!(node.eval(&lua), "started");
        let deadline = Instant::now() + Duration::from_secs(10);
        while node.eval("return tostring(process_env_done)") != "true" {
            assert!(Instant::now() < deadline, "process child did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
        let text = node.eval("return table.concat(process_env_lines, '\\n')");
        assert!(
            text.lines().any(|line| line.starts_with(PROBE_PREFIX)),
            "clear_env={clear}: child did not report environment: {text:?}"
        );
        let actual = extract_probe(&text);
        if clear {
            assert_eq!(actual, expected_after_clear(env));
        } else {
            assert_eq!(
                actual.get("REMUDA_PROCESS_ENV_PARENT").map(String::as_str),
                Some("inherited")
            );
            assert_eq!(
                actual.get("REMUDA_PROCESS_ENV_EXTRA").map(String::as_str),
                Some("child-only")
            );
        }
    }
}

#[test]
fn both_process_words_reject_invalid_environment_specs() {
    let node = Node::start();
    let argv = probe_argv();
    let invalid = [
        ("env = 7", "env must be a table"),
        ("env = { [1] = 'x' }", "env names must be strings"),
        ("env = { GOOD = 1 }", "env values must be strings"),
        ("env = { [''] = 'x' }", "env names must not be empty"),
        ("env = { ['BAD=NAME'] = 'x' }", "env names must not contain"),
        (
            "env = { ['BAD' .. string.char(0)] = 'x' }",
            "env names must not contain",
        ),
        (
            "env = { GOOD = 'x' .. string.char(0) }",
            "env values must not contain NUL",
        ),
        ("clear_env = 'yes'", "clear_env must be a boolean"),
    ];
    for word in ["process", "process.run"] {
        for (fields, expected) in invalid {
            let code = if word == "process" {
                format!("remuda.process({{ argv = {argv}, {fields} }})")
            } else {
                format!("remuda.process.run({{ argv = {argv}, {fields} }})")
            };
            let message = node.error_of(&code);
            assert!(message.contains(expected), "{word}: {message}");
        }
    }
}
