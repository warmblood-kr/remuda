//! Pins a mechanism this round did NOT build: a pty session's child already
//! dies when its daemon dies, by kernel accident (the daemon holds the pty
//! master fd; any daemon exit closes it; the kernel SIGHUPs the pty's
//! session-leader child; the default action for SIGHUP is termination). See
//! `child_guard.rs`'s `documented_pty_hangup_accident()`, called at
//! `pty.rs`'s spawn site so the decision not to add a guard there is a real,
//! grep-able marker rather than silence.
//!
//! This test exists so a future change to how `pty.rs` holds `_master` (or
//! to how the slave fd is dropped) breaks LOUDLY here instead of silently
//! reopening the orphan leak on the one path that currently closes itself.
//!
//! No red-then-green demonstration was attempted for this one (unlike the
//! `remuda.process` tests in `daemon.rs`) — defeating it artificially would
//! mean faking away the daemon's own fd-close-on-exit behaviour, which is
//! more machinery than this round's scope. A real green run today is the
//! deliverable.

use remuda_core::protocol::{Request, Response};
use remuda_core::Size;
use remuda_native::{client, daemon};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(10);

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("remuda-pty-death-{}-{tag}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// A daemon as its own PROCESS, with a real pid to SIGKILL — an in-thread
/// `daemon::serve` would die with the test harness itself, proving nothing.
struct Daemon(std::process::Child);

impl Daemon {
    fn spawn(dir: &Path) -> Self {
        let child = std::process::Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", "s", "daemon"])
            .env("REMUDA_RUNTIME_DIR", dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn daemon");
        let path = daemon::socket_path_in(dir, "s");
        let deadline = Instant::now() + PATIENCE;
        while remuda_native::ipc::connect(&path).is_err() {
            assert!(Instant::now() < deadline, "daemon never bound {path:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
        Self(child)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn new_session(path: &Path, name: &str) {
    let response = client::request(
        path,
        &Request::New {
            name: Some(name.to_string()),
            command: vec!["sh".into()],
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .expect("new");
    assert_eq!(
        response,
        Response::Value(name.to_string()),
        "New answers with the name it gave the session"
    );
}

fn capture(path: &Path, name: &str) -> String {
    match client::request(
        path,
        &Request::Capture {
            name: name.to_string(),
        },
    ) {
        Ok(Response::Screen(text)) => text,
        other => panic!("capture failed: {other:?}"),
    }
}

fn wait_for(path: &Path, name: &str, needle: &str) -> String {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let screen = capture(path, name);
        if screen.contains(needle) {
            return screen;
        }
        assert!(
            Instant::now() < deadline,
            "{needle:?} never appeared in {name}. screen:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `kill(pid, 0)` sends no signal, only asks whether one *could* be
/// delivered — ESRCH means the pid is gone. Anything else (success, or a
/// permission error) means it is still around.
#[cfg(target_os = "linux")]
fn pid_alive(pid: i32) -> bool {
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(target_os = "linux")]
#[test]
fn a_pty_sessions_child_dies_with_a_sigkilled_daemon() {
    let dir = scratch_dir("kill");
    let daemon = Daemon::spawn(&dir);
    let path = daemon::socket_path_in(&dir, "s");

    new_session(&path, "victim");
    // Arithmetic in the marker, per PRINCIPLES §4 — a pty echoes its own
    // input, so waiting for a literal in the command line would only prove
    // the echo happened, not that the shell evaluated anything.
    client::request(
        &path,
        &Request::SendLine {
            name: "victim".into(),
            text: "echo PID:$((6*7-42))$$".into(),
        },
    )
    .expect("send");
    let screen = wait_for(&path, "victim", "PID:0");

    let after_marker =
        &screen[screen.find("PID:0").expect("marker never appeared") + "PID:0".len()..];
    let digits: String = after_marker
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let pid: i32 = digits.parse().expect("a pid");
    assert!(
        pid_alive(pid),
        "sanity: the pty child must be alive before the kill"
    );

    let daemon_pid = daemon.0.id() as libc::pid_t;
    assert_eq!(
        unsafe { libc::kill(daemon_pid, libc::SIGKILL) },
        0,
        "SIGKILL of the real daemon pid must succeed"
    );

    let deadline = Instant::now() + PATIENCE;
    while pid_alive(pid) {
        assert!(
            Instant::now() < deadline,
            "the pty session's child ({pid}) outlived a SIGKILLed daemon — the fd-close/SIGHUP \
             accident this test pins no longer happens; see child_guard.rs and this file's \
             module doc"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
