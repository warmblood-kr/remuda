//! `remuda stop` ends every session, host-backed ones too; `--keep-sessions`
//! leaves host-backed ones running for the next daemon (#713, #843). Same
//! scenario on both backends; see `daemon_support/parity.rs`.
#![cfg(unix)]

use remuda_core::protocol::{Request, Response};
use std::time::{Duration, Instant};

#[path = "daemon_support/parity.rs"]
mod parity;
use parity::{Mode, Parity, MODES, PATIENCE};

fn names(daemon: &Parity) -> Vec<String> {
    match daemon.request(&Request::List) {
        Response::Sessions(sessions) => sessions.into_iter().map(|s| s.name).collect(),
        other => panic!("{:?}: ls: {other:?}", daemon.mode),
    }
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Start a session whose child writes its pid, and return that pid.
fn sleeper(daemon: &Parity) -> i32 {
    let file = daemon.root().join("child.pid");
    daemon.start_command(
        "keeper",
        vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("echo $$ > {}; exec sleep 600", file.display()),
        ],
    );
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(pid) = std::fs::read_to_string(&file)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "{:?}: no child pid", daemon.mode);
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn stop(daemon: &Parity, keep: bool) -> String {
    let mut args = vec!["stop", "-f"];
    if keep {
        args.push("--keep-sessions");
    }
    let out = daemon.cli(&args);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "{:?}: stop failed: {stderr}",
        daemon.mode
    );
    stderr
}

#[test]
fn stop_ends_every_session_in_both_backends() {
    for mode in MODES {
        let mut daemon = Parity::start(mode);
        let child = sleeper(&daemon);
        let stderr = stop(&daemon, false);
        assert!(
            stderr.contains("stopping ends 1 session(s)") && stderr.contains("--keep-sessions"),
            "{mode:?}: notice: {stderr}"
        );
        let deadline = Instant::now() + PATIENCE;
        while alive(child) {
            assert!(Instant::now() < deadline, "{mode:?}: child outlived stop");
            std::thread::sleep(Duration::from_millis(20));
        }
        let deadline = Instant::now() + PATIENCE;
        while daemon.hosts_running() > 0 {
            assert!(Instant::now() < deadline, "{mode:?}: a host outlived stop");
            std::thread::sleep(Duration::from_millis(20));
        }
        daemon.restart();
        assert!(names(&daemon).is_empty(), "{mode:?}: {:?}", names(&daemon));
    }
}

#[test]
fn stop_keep_sessions_keeps_only_host_backed_ones() {
    for mode in MODES {
        let mut daemon = Parity::start(mode);
        let child = sleeper(&daemon);
        let stderr = stop(&daemon, true);
        assert!(
            stderr.contains("host-backed ones keep running"),
            "{mode:?}: notice: {stderr}"
        );
        daemon.restart();
        match mode {
            Mode::Hosted => {
                assert!(alive(child), "hosted: child ended with --keep-sessions");
                assert_eq!(names(&daemon), ["keeper"]);
            }
            Mode::InProcess => {
                let deadline = Instant::now() + PATIENCE;
                while alive(child) {
                    assert!(Instant::now() < deadline, "in-process: child outlived stop");
                    std::thread::sleep(Duration::from_millis(20));
                }
                assert!(names(&daemon).is_empty(), "{:?}", names(&daemon));
            }
        }
    }
}

/// Hold a tracked attach open, as a human terminal does.
fn attach(daemon: &Parity, name: &str) -> remuda_native::ipc::Stream {
    use std::io::{BufRead, BufReader, Write};
    let mut stream = remuda_native::ipc::connect(&daemon.socket()).expect("connect");
    let mut line = serde_json::to_string(&Request::AttachTracked { name: name.into() }).unwrap();
    line.push('\n');
    stream.write_all(line.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut reader =
        BufReader::new(remuda_native::ipc::TryClone::try_clone(&stream).expect("clone"));
    let mut ack = String::new();
    reader.read_line(&mut ack).unwrap();
    assert!(
        matches!(
            serde_json::from_str::<Response>(&ack),
            Ok(Response::AttachStarted { .. })
        ),
        "{:?}: attach refused: {ack}",
        daemon.mode
    );
    stream
}

/// The host refuses Close while a human holds a grant; stop must still end
/// the session, not leave its host and child running for the next daemon.
#[test]
fn stop_ends_a_session_a_human_is_attached_to() {
    for mode in MODES {
        let mut daemon = Parity::start(mode);
        let child = sleeper(&daemon);
        let _attached = attach(&daemon, "keeper");
        stop(&daemon, false);
        let deadline = Instant::now() + PATIENCE;
        while alive(child) {
            assert!(Instant::now() < deadline, "{mode:?}: child outlived stop");
            std::thread::sleep(Duration::from_millis(20));
        }
        let deadline = Instant::now() + PATIENCE;
        while daemon.hosts_running() > 0 {
            assert!(Instant::now() < deadline, "{mode:?}: a host outlived stop");
            std::thread::sleep(Duration::from_millis(20));
        }
        daemon.restart();
        assert!(names(&daemon).is_empty(), "{mode:?}: {:?}", names(&daemon));
    }
}
