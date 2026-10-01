#![cfg(unix)]

//! Regression coverage for text entry that a terminal UI recognizes as paste.

use remuda_core::protocol::{Request, Response};
use remuda_core::Size;
use remuda_native::{client, daemon, script};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[path = "daemon_support/spawn.rs"]
mod spawn;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("remuda-input-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn daemon_at(path: &Path, dir: &Path) -> spawn::Daemon {
    debug_assert_eq!(path, daemon::socket_path_in(dir, "s"));
    let mut command = spawn::base_command(dir);
    command.env("REMUDA_SUPPRESS_DEPRECATIONS", "1");
    spawn::spawn_and_wait(command, dir)
}

fn capture(path: &Path, name: &str) -> String {
    match client::request(path, &Request::Capture { name: name.into() }) {
        Ok(Response::Screen(text)) => text,
        other => panic!("capture failed: {other:?}"),
    }
}

fn wait_screen(path: &Path, name: &str, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let screen = capture(path, name);
        if screen.contains(needle) {
            return screen;
        }
        assert!(
            Instant::now() < deadline,
            "did not see {needle:?}:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn fake_agent(mode: &str) -> Vec<String> {
    vec![
        env!("CARGO_BIN_EXE_input_unit_fake_agent").into(),
        mode.into(),
    ]
}

#[test]
fn empty_type_text_sends_only_return_in_bracketed_paste_mode() {
    let dir = scratch("empty-submit");
    let socket = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&socket, &dir);

    client::request(
        &socket,
        &Request::New {
            name: Some("empty-agent".into()),
            command: fake_agent("empty"),
            size: Size::new(80, 24),
            cwd: Some(dir.display().to_string()),
            env: None,
        },
    )
    .expect("start fake paste agent");
    wait_screen(&socket, "empty-agent", "READY");

    script::run_source(
        &socket,
        "input-unit-empty-submit",
        "assert(remuda.type_text('empty-agent', '') == 'submitted')",
    )
    .expect("submit an empty body");
    let screen = wait_screen(&socket, "empty-agent", "BARE_RETURN");
    assert!(screen.contains("BARE_RETURN"), "unexpected bytes: {screen}");
}

#[test]
fn silent_submit_does_not_retry_when_the_raw_screen_is_unchanged() {
    let dir = scratch("silent-submit");
    let socket = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&socket, &dir);

    client::request(
        &socket,
        &Request::New {
            name: Some("silent-agent".into()),
            command: fake_agent("silent"),
            size: Size::new(80, 24),
            cwd: Some(dir.display().to_string()),
            env: None,
        },
    )
    .expect("start silent submit agent");
    wait_screen(&socket, "silent-agent", "READY");

    script::run_source(
        &socket,
        "input-unit-silent-submit",
        "assert(remuda.type_text('silent-agent', 'silent') == 'unverified')",
    )
    .expect("report the unverified silent submission");
    let screen = wait_screen(&socket, "silent-agent", "RETURNS:");
    assert!(
        screen.contains("RETURNS:1"),
        "a silent successful submit must send exactly one Return: {screen}"
    );
}

#[test]
fn type_text_submits_paste_once_and_preserves_embedded_newline() {
    let dir = scratch("submit");
    let socket = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&socket, &dir);

    client::request(
        &socket,
        &Request::New {
            name: Some("paste-agent".into()),
            command: fake_agent("paste"),
            size: Size::new(80, 24),
            cwd: Some(dir.display().to_string()),
            env: None,
        },
    )
    .expect("start fake paste agent");
    wait_screen(&socket, "paste-agent", "READY");

    let started = Instant::now();
    script::run_source(
        &socket,
        "input-unit-submit",
        "remuda.type_text('paste-agent', 'alpha\\nbeta')",
    )
    .expect("type_text");
    eprintln!("type_text elapsed_ms={}", started.elapsed().as_millis());

    let screen = wait_screen(&socket, "paste-agent", "SUBMITTED:");
    assert!(
        screen.contains("SUBMITTED:alpha\\nbeta"),
        "text changed: {screen}"
    );
    assert!(
        screen.contains("COUNT:1"),
        "expected exactly one submission: {screen}"
    );
    assert!(
        !screen.contains("COUNT:2"),
        "the visible transcript must not be mistaken for an active composer: {screen}"
    );
    assert!(
        screen.contains("RETURNS:2"),
        "the first Return should become a composer newline and one retry should submit: {screen}"
    );

    script::run_source(
        &socket,
        "input-unit-repeat",
        "remuda.type_text('paste-agent', 'alpha\\nbeta')",
    )
    .expect("repeat the same text after its transcript remains visible");
    let repeated = wait_screen(&socket, "paste-agent", "COUNT:2");
    assert!(
        repeated.contains("COUNT:2"),
        "repeat did not submit: {repeated}"
    );
    assert!(
        !repeated.contains("COUNT:3"),
        "one repeated notice must submit exactly once: {repeated}"
    );
    assert!(
        repeated.contains("RETURNS:3"),
        "the repeated tail must not trigger an early Return: {repeated}"
    );
}

#[test]
fn type_text_still_submits_plain_shell_commands_and_long_wrapped_text() {
    let dir = scratch("shell");
    let socket = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&socket, &dir);
    client::request(
        &socket,
        &Request::New {
            name: Some("plain-shell".into()),
            command: vec!["sh".into()],
            size: Size::new(32, 8),
            cwd: Some(dir.display().to_string()),
            env: None,
        },
    )
    .expect("start shell");

    script::run_source(
        &socket,
        "input-unit-shell",
        "remuda.type_text('plain-shell', 'printf SHELL_OK')",
    )
    .expect("type shell command");
    assert!(wait_screen(&socket, "plain-shell", "SHELL_OK").contains("SHELL_OK"));

    client::request(
        &socket,
        &Request::SendLine {
            name: "plain-shell".into(),
            text: "printf SEND_LINE_OK".into(),
        },
    )
    .expect("send line through the shared text/submit path");
    assert!(wait_screen(&socket, "plain-shell", "SEND_LINE_OK").contains("SEND_LINE_OK"));

    let wrapped = "printf WRAPPED_OK; # this text is deliberately longer than two screen lines in this narrow terminal";
    script::run_source(
        &socket,
        "input-unit-wrapped",
        &format!("remuda.type_text('plain-shell', {wrapped:?})"),
    )
    .expect("type wrapped command");
    assert!(wait_screen(&socket, "plain-shell", "WRAPPED_OK").contains("WRAPPED_OK"));
}

/// A child that is slow to drain its input (an agent still starting up) must
/// still receive the whole task and its Return. Today the 2 s bounded PTY
/// write times out, type_text raises, and the text lands later without Return.
#[test]
fn type_text_delivers_and_submits_when_the_child_drains_slowly() {
    let dir = scratch("slow-drain");
    let socket = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&socket, &dir);
    // Larger than any PTY input queue, so the write blocks until the child reads.
    let bytes = 8000;
    let child = format!(
        "stty raw -echo; printf READY; sleep 3; \
         printf 'GOT:%s ' $(head -c {bytes} | wc -c); \
         printf 'RET:%s' \"$(dd bs=1 count=1 2>/dev/null | od -An -tx1 | tr -d ' ')\"; sleep 5"
    );
    client::request(
        &socket,
        &Request::New {
            name: Some("slow-drain".into()),
            command: vec!["sh".into(), "-c".into(), child],
            size: Size::new(80, 24),
            cwd: Some(dir.display().to_string()),
            env: None,
        },
    )
    .expect("start slow-draining child");
    wait_screen(&socket, "slow-drain", "READY");

    script::run_source(
        &socket,
        "input-unit-slow-drain",
        &format!(
            "local ok, err = pcall(remuda.type_text, 'slow-drain', string.rep('x', {bytes}))\n\
             assert(ok, 'type_text raised: ' .. tostring(err))"
        ),
    )
    .expect("type_text must not fail while the child is only slow");

    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let screen = capture(&socket, "slow-drain");
        if screen.contains("RET:0d") {
            assert!(screen.contains(&format!("GOT:{bytes}")), "{screen}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no Return after the text:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
