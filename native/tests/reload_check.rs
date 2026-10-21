//! `remuda reload --check` through a real daemon (#713): the daemon starts a
//! candidate from the target binary, asks what it sees of every session
//! host, and stops it. Nothing changes. Fake candidates stand in for builds
//! that cannot hand over: one from before reload, a hung one, one speaking a
//! newer handoff version, and one that cannot drive a host.
#![cfg(unix)]

use remuda_core::protocol::{Request, Response};
use remuda_core::Size;
use remuda_native::{client, daemon};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(15);

/// A short private root: host socket paths must fit `sun_path`.
struct Root(PathBuf);
impl Root {
    fn new(tag: &str) -> Self {
        let base = if cfg!(target_os = "macos") {
            "/private/tmp"
        } else {
            "/tmp"
        };
        let path = PathBuf::from(base).join(format!("rrc-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        Self(path)
    }
    fn socket(&self) -> PathBuf {
        daemon::socket_path_in(&self.0, "s")
    }
    fn remuda(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", "s"])
            .args(args)
            .env("REMUDA_RUNTIME_DIR", &self.0)
            .env("HOME", &self.0)
            .env("XDG_STATE_HOME", self.0.join("state"))
            .env("REMUDA_NO_UPDATE_CHECK", "1")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
    /// A fake candidate: a script that writes these handoff messages to fd 3
    /// (4-byte big-endian length, then JSON), then waits.
    fn candidate(&self, name: &str, messages: &[&str], then: &str) -> PathBuf {
        let mut body = String::from("#!/bin/sh\n");
        for message in messages {
            let length = u32::try_from(message.len()).unwrap().to_be_bytes();
            let prefix: String = length.iter().map(|b| format!("\\{b:03o}")).collect();
            body.push_str(&format!("printf '{prefix}%s' '{message}' >&3\n"));
        }
        body.push_str(then);
        body.push('\n');
        let path = self.0.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/pkill")
            .args(["-KILL", "-f", &self.0.to_string_lossy()])
            .status();
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Daemon(Child);
impl Daemon {
    fn spawn(root: &Root, hosted: bool) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_remuda"));
        command
            .args(["-s", "s", "daemon"])
            .env("REMUDA_RUNTIME_DIR", &root.0)
            .env("HOME", &root.0)
            .env("XDG_STATE_HOME", root.0.join("state"))
            .env("REMUDA_TEST_RELOAD_READY_MS", "3000")
            .env_remove("REMUDA_EXPERIMENTAL_SESSION_HOST")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if hosted {
            command.env("REMUDA_EXPERIMENTAL_SESSION_HOST", "1");
        }
        let daemon = Self(command.spawn().unwrap());
        let deadline = Instant::now() + PATIENCE;
        while remuda_native::ipc::connect(&root.socket()).is_err() {
            assert!(Instant::now() < deadline, "daemon never bound");
            std::thread::sleep(Duration::from_millis(10));
        }
        daemon
    }
    fn pid(&self) -> u32 {
        self.0.id()
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
            name: Some(name.into()),
            command: vec!["/bin/sh".into()],
            size: Size::new(80, 24),
            cwd: Some("/".into()),
            env: None,
        },
    )
    .unwrap();
    assert_eq!(response, Response::Value(name.into()));
}
fn build(path: &Path) -> String {
    match client::request(path, &Request::Version).unwrap() {
        Response::Value(build) => build,
        other => panic!("{other:?}"),
    }
}
fn daemon_pid(root: &Root) -> String {
    std::fs::read_to_string(root.socket().with_extension("sock.lock"))
        .or_else(|_| {
            let mut lock = root.socket().into_os_string();
            lock.push(".lock");
            std::fs::read_to_string(lock)
        })
        .unwrap()
        .trim()
        .to_owned()
}
/// Processes still running a candidate under this root.
fn candidates(root: &Root) -> Vec<String> {
    let out = Command::new("/bin/ps")
        .args(["-A", "-o", "args="])
        .output()
        .unwrap();
    let root = root.0.to_string_lossy().into_owned();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| line.contains(&root) && line.contains("_reload-candidate"))
        .map(str::to_owned)
        .collect()
}
fn text(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn a_check_with_only_hosted_sessions_is_ready_and_changes_nothing() {
    let root = Root::new("ready");
    let path = root.socket();
    let daemon = Daemon::spawn(&root, true);
    new_session(&path, "one");
    new_session(&path, "two");
    let build = build(&path);
    let before = daemon_pid(&root);

    let out = root.remuda(&["reload", "--check"]);
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}{stderr}");
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        [
            format!("ready: a reload into {build} would keep every host"),
            format!("host one: now {build}; candidate sees {build}"),
            format!("host two: now {build}; candidate sees {build}"),
        ]
    );
    // The same daemon still serves both sessions, and no candidate is left.
    assert_eq!(daemon_pid(&root), before);
    assert_eq!(daemon.pid().to_string(), before);
    let sent = client::request(
        &path,
        &Request::SendLine {
            name: "one".into(),
            text: "echo still".into(),
        },
    )
    .unwrap();
    assert_eq!(sent, Response::Ok);
    assert!(candidates(&root).is_empty(), "{:?}", candidates(&root));
}

#[test]
fn in_process_sessions_block_a_reload() {
    let root = Root::new("legacy");
    let path = root.socket();
    let _daemon = Daemon::spawn(&root, false);
    new_session(&path, "shell");
    let out = root.remuda(&["reload", "--check"]);
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(1), "{stdout}{stderr}");
    assert!(stdout.starts_with("blocked: "), "{stdout}");
    assert!(
        stdout.contains("blocker: in-process sessions would end: shell"),
        "{stdout}"
    );
}

#[test]
fn a_build_from_before_reload_is_refused_cleanly() {
    let root = Root::new("old");
    let path = root.socket();
    let _daemon = Daemon::spawn(&root, true);
    new_session(&path, "one");
    // Today's remuda answers an unknown verb with a usage error.
    let old = root.candidate("old-remuda", &[], "echo 'unknown command' >&2; exit 2");
    let started = Instant::now();
    let out = root.remuda(&["reload", "--check", "--binary", &old.to_string_lossy()]);
    let (_, stderr) = text(&out);
    assert!(!out.status.success());
    assert!(
        stderr.contains("candidate exited before its hello"),
        "{stderr}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(matches!(
        client::request(&path, &Request::Capture { name: "one".into() }).unwrap(),
        Response::Screen(_)
    ));
}

#[test]
fn a_hung_candidate_is_stopped_at_its_deadline() {
    let root = Root::new("hung");
    let path = root.socket();
    let _daemon = Daemon::spawn(&root, true);
    new_session(&path, "one");
    let hung = root.candidate("hung-remuda", &[], "exec sleep 60");
    let started = Instant::now();
    let out = root.remuda(&["reload", "--check", "--binary", &hung.to_string_lossy()]);
    let (_, stderr) = text(&out);
    assert!(
        stderr.contains("candidate sent no hello in time"),
        "{stderr}"
    );
    // The daemon's test deadline is 3 s; then the candidate is killed.
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
    let still = Command::new("/usr/bin/pgrep")
        .args(["-f", &hung.to_string_lossy()])
        .output()
        .unwrap();
    assert!(
        still.stdout.is_empty(),
        "the hung candidate was left running"
    );
}

#[test]
fn a_candidate_speaking_a_newer_handoff_is_refused() {
    let root = Root::new("newer");
    let path = root.socket();
    let _daemon = Daemon::spawn(&root, true);
    let newer = root.candidate(
        "newer-remuda",
        &[r#"{"kind":"Hello","handoff":99,"build":"9.9.9+future"}"#],
        "exec sleep 60",
    );
    let out = root.remuda(&["reload", "--check", "--binary", &newer.to_string_lossy()]);
    let (_, stderr) = text(&out);
    assert!(
        stderr.contains("candidate 9.9.9+future speaks handoff version 99, this daemon 2"),
        "{stderr}"
    );
    let _ = path;
}

#[test]
fn a_candidate_that_cannot_drive_a_host_blocks_the_reload() {
    let root = Root::new("blind");
    let path = root.socket();
    let _daemon = Daemon::spawn(&root, true);
    new_session(&path, "one");
    let build = build(&path);
    let blind = root.candidate(
        "blind-remuda",
        &[
            r#"{"kind":"Hello","handoff":2,"build":"9.9.9+future"}"#,
            r#"{"kind":"Ready","hosts":[{"name":"one","build":null,"problem":"incompatible host protocol"}]}"#,
        ],
        "exec sleep 60",
    );
    let out = root.remuda(&["reload", "--check", "--binary", &blind.to_string_lossy()]);
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(1), "{stdout}{stderr}");
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        [
            "blocked: a reload into 9.9.9+future would not be safe".to_owned(),
            "blocker: host one: the candidate cannot drive it (incompatible host protocol)".into(),
            format!(
                "host one: now {build}; candidate sees unreachable (incompatible host protocol)"
            ),
        ]
    );
}
