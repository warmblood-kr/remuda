//! Regression coverage for text entry that a terminal UI recognizes as paste.

use remuda_core::protocol::{Request, Response};
use remuda_core::Size;
use remuda_native::{client, daemon, script};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[path = "daemon_support/spawn.rs"]
mod spawn;

fn scratch(tag: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/private/tmp/remuda-input-{}-{tag}", std::process::id()));
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
        assert!(Instant::now() < deadline, "did not see {needle:?}:\n{screen}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn type_text_submits_paste_once_and_preserves_embedded_newline() {
    let dir = scratch("submit");
    let socket = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&socket, &dir);
    let fake = dir.join("paste_agent.py");
    std::fs::write(
        &fake,
        r#"import os, select, sys, termios, tty, time
fd = sys.stdin.fileno()
tty.setraw(fd)
buf = bytearray()
last = 0.0
paste = False
submitted = 0
while True:
    ready, _, _ = select.select([fd], [], [], 5)
    if not ready:
        continue
    data = os.read(fd, 4096)
    if not data:
        break
    i = 0
    while i < len(data):
        if data.startswith(b'\x1b[200~', i):
            paste = True
            i += 6
            continue
        if data.startswith(b'\x1b[201~', i):
            paste = False
            i += 6
            continue
        b = data[i]
        now = time.monotonic()
        if b == 13:
            if paste or (last and now - last < 0.15):
                buf.extend(b'\n')
            else:
                submitted += 1
                os.write(1, b'\r\nSUBMITTED:' + bytes(buf) + b'\r\nCOUNT:' + str(submitted).encode() + b'\r\n')
                buf.clear()
        else:
            buf.append(b)
        last = now
        i += 1
"#,
    )
    .unwrap();

    client::request(
        &socket,
        &Request::New {
            name: Some("paste-agent".into()),
            command: vec!["python3".into(), fake.display().to_string()],
            size: Size::new(80, 24),
            cwd: Some(dir.display().to_string()),
            env: None,
        },
    )
    .expect("start fake paste agent");

    script::run_source(
        &socket,
        "input-unit-submit",
        "remuda.type_text('paste-agent', 'alpha\\nbeta')",
    )
    .expect("type_text");

    let screen = wait_screen(&socket, "paste-agent", "SUBMITTED:");
    assert!(screen.contains("SUBMITTED:alpha\nbeta"), "text changed: {screen}");
    assert!(screen.contains("COUNT:1"), "expected exactly one submission: {screen}");
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

    script::run_source(&socket, "input-unit-shell", "remuda.type_text('plain-shell', 'printf SHELL_OK')")
        .expect("type shell command");
    assert!(wait_screen(&socket, "plain-shell", "SHELL_OK").contains("SHELL_OK"));

    let wrapped = "printf WRAPPED_OK; # this text is deliberately longer than two screen lines in this narrow terminal";
    script::run_source(
        &socket,
        "input-unit-wrapped",
        &format!("remuda.type_text('plain-shell', {wrapped:?})"),
    )
    .expect("type wrapped command");
    assert!(wait_screen(&socket, "plain-shell", "WRAPPED_OK").contains("WRAPPED_OK"));
}
