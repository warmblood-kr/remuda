//! Who a caller is on Windows, for processes that belong to a session: the
//! session's own child, a grandchild whose parent has exited, and a child that
//! asks to leave the session's job. All of them are `session`, never
//! `outside`. A command in an ordinary terminal still reads `unknown` here.
#![cfg(windows)]

use remuda_core::protocol::{Request, Response};
use remuda_native::{client, daemon};
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[path = "daemon_support/spawn.rs"]
mod spawn;

const ASK: &str = "return remuda.caller().kind";
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

/// A directory of our own, removed when the test ends, passing or not.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("remuda-j{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A batch file that asks the daemon who it is and writes the answer to
/// `out`, after `wait` seconds when its starter must be gone by then.
fn asking_script(dir: &Path, out: &Path, wait: u32) -> PathBuf {
    let pause = if wait > 0 {
        format!("ping -n {} 127.0.0.1 >nul\r\n", wait + 1)
    } else {
        String::new()
    };
    let body = format!(
        "@echo off\r\n{pause}\"{}\" -s s -e \"{ASK}\" >\"{}\" 2>&1\r\n",
        env!("CARGO_BIN_EXE_remuda"),
        out.display()
    );
    let script = dir.join("ask.cmd");
    std::fs::write(&script, body).expect("write ask.cmd");
    script
}

/// Runs `code` in the scratch daemon; the session must start.
fn start(dir: &Path, code: String) {
    let request = Request::Eval { code, name: None };
    let socket = daemon::socket_path_in(dir, "s");
    let started = client::request(&socket, &request).expect("eval request");
    assert!(matches!(started, Response::Value(_)), "{started:?}");
}

/// What appears in `out` within 40 seconds, trimmed; empty when nothing did.
fn answer(out: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        let text = std::fs::read_to_string(out).unwrap_or_default();
        if !text.trim().is_empty() || Instant::now() >= deadline {
            return text.trim().to_owned();
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn fact(line: std::fmt::Arguments) {
    // Straight to stderr, so the facts are in the log for a passing test too.
    let _ = writeln!(std::io::stderr(), "FACT job {line}");
}

fn lua(path: &Path) -> String {
    format!("{:?}", path.to_str().expect("utf-8 scratch path"))
}

#[test]
fn a_session_child_is_a_session_caller() {
    let scratch = Scratch::new("child");
    let _daemon = spawn::Daemon::spawn(&scratch.0);
    let out = scratch.0.join("kind.txt");
    let script = asking_script(&scratch.0, &out, 0);
    start(
        &scratch.0,
        format!(
            "return remuda.new('child', {{ 'cmd.exe', '/c', {} }})",
            lua(&script)
        ),
    );
    let kind = answer(&out);
    fact(format_args!("session child: kind {kind:?}"));
    assert_eq!(kind, "session");
}

/// `start` without /b gives the new process its own console, so it is not
/// closed with the session's pty when the session's child exits at once.
#[test]
fn a_session_grandchild_whose_parent_exited_is_a_session_caller() {
    let scratch = Scratch::new("grand");
    let _daemon = spawn::Daemon::spawn(&scratch.0);
    let out = scratch.0.join("kind.txt");
    let script = asking_script(&scratch.0, &out, 3);
    start(
        &scratch.0,
        format!(
            "return remuda.new('grand', {{ 'cmd.exe', '/c', 'start', '', '/min', 'cmd.exe', '/c', {} }})",
            lua(&script)
        ),
    );
    let kind = answer(&out);
    fact(format_args!(
        "grandchild after its parent exited: kind {kind:?}"
    ));
    assert_eq!(kind, "session");
}

/// Not a test of its own: the program of the breakaway session. It starts the
/// asking script in a new console and asks Windows to keep it OUT of the job,
/// then exits. Without the variables it does nothing.
#[test]
fn breakaway_helper() {
    let (Some(script), Some(out)) = (
        std::env::var_os("REMUDA_BREAKAWAY_SCRIPT"),
        std::env::var_os("REMUDA_BREAKAWAY_OUT"),
    ) else {
        return;
    };
    let spawned = Command::new("cmd.exe")
        .arg("/c")
        .arg(script)
        .creation_flags(CREATE_NEW_CONSOLE | CREATE_BREAKAWAY_FROM_JOB)
        .spawn();
    if let Err(error) = spawned {
        std::fs::write(out, format!("breakaway refused: {error}")).expect("record the refusal");
    }
}

/// A session's job allows no breakaway: the request is refused by Windows, or
/// the process is started and is still the session's.
#[test]
fn a_child_that_asks_to_leave_the_job_is_still_a_session_caller() {
    let scratch = Scratch::new("break");
    let _daemon = spawn::Daemon::spawn(&scratch.0);
    let out = scratch.0.join("kind.txt");
    let script = asking_script(&scratch.0, &out, 3);
    let helper = std::env::current_exe().expect("locate this test binary");
    start(
        &scratch.0,
        format!(
            "return remuda.new('break', {{ {}, '--exact', 'breakaway_helper', '--nocapture' }}, nil, \
             {{ REMUDA_BREAKAWAY_SCRIPT = {}, REMUDA_BREAKAWAY_OUT = {} }})",
            lua(&helper),
            lua(&script),
            lua(&out)
        ),
    );
    let kind = answer(&out);
    fact(format_args!("child that asked for breakaway: {kind:?}"));
    assert!(
        kind == "session" || kind.starts_with("breakaway refused"),
        "{kind:?}"
    );
}

/// The order of the two changes, pinned: until the chain end is decided in
/// its own change, a command outside every session is not yet `outside`.
#[test]
fn a_command_outside_every_session_still_reads_unknown() {
    let scratch = Scratch::new("plain");
    let _daemon = spawn::Daemon::spawn(&scratch.0);
    let output = Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(["-s", "s", "-e", ASK])
        .env("REMUDA_RUNTIME_DIR", &scratch.0)
        .env("HOME", scratch.0.join("home"))
        .env("LOCALAPPDATA", scratch.0.join("home"))
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        .output()
        .expect("run remuda");
    let kind = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    fact(format_args!("plain command: kind {kind:?}"));
    assert_eq!(kind, "unknown");
}
