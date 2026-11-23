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
        assert_eq!(daemon.hosts_running(), 0, "{mode:?}: a host outlived stop");
        daemon.restart();
        assert!(names(&daemon).is_empty(), "{mode:?}: {:?}", names(&daemon));
        daemon.shell("keeper");
        stop(&daemon, false);
    }
}

/// Locate the single reserved host belonging to this isolated daemon.
fn owner_record(daemon: &Parity) -> (std::path::PathBuf, i32) {
    let inventory_root = daemon.socket().with_file_name("s.sock.hosts");
    std::fs::read_dir(inventory_root)
        .unwrap()
        .filter_map(Result::ok)
        .find_map(|entry| {
            let path = entry.path().join("owner.json");
            let record: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
            Some((path, i32::try_from(record["host"].as_i64()?).ok()?))
        })
        .expect("test host recorded its PID")
}

fn kill_host(pid: i32) {
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    let until = Instant::now() + PATIENCE;
    while alive(pid) {
        assert!(Instant::now() < until, "test host did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn stop_retires_an_already_dead_hosts_reservation() {
    let mut daemon = Parity::start(Mode::Hosted);
    let previous = daemon.shell("keeper");
    let (_, pid) = owner_record(&daemon);
    kill_host(pid);

    stop(&daemon, false);
    daemon.restart();
    assert_ne!(daemon.shell("keeper"), previous);
    stop(&daemon, false);
}

#[test]
fn stop_uses_authenticated_process_identity_for_a_live_pre_upgrade_host() {
    let mut daemon = Parity::start(Mode::Hosted);
    let old = daemon.shell("keeper");
    let (record, _) = owner_record(&daemon);
    std::fs::remove_file(record).unwrap();

    stop(&daemon, false);
    assert_eq!(
        daemon.hosts_running(),
        0,
        "the pre-upgrade host outlived stop"
    );
    daemon.restart();
    assert_ne!(daemon.shell("keeper"), old);
    stop(&daemon, false);
}

#[test]
fn stop_reports_and_preserves_a_reservation_with_unverifiable_ownership() {
    let mut daemon = Parity::start(Mode::Hosted);
    daemon.shell("keeper");
    let (record, pid) = owner_record(&daemon);
    kill_host(pid);
    // Reproduce a pre-upgrade record. The inventory itself is untouched.
    std::fs::remove_file(record).unwrap();
    daemon.kill_daemon();
    let out = daemon.cli(&["-e", "return 1"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("session host status unverified; names remain reserved: keeper"),
        "{stderr}"
    );

    let out = daemon.cli(&["stop", "-f", "--i-am-inside"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains("unresolved session hosts: keeper"),
        "{stderr}"
    );
    assert!(stderr.contains("Restart in the same runtime"), "{stderr}");
    assert!(remuda_native::ipc::connect(&daemon.socket()).is_err());

    let out = daemon.cli(&["-e", "return 1"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("names remain reserved: keeper"), "{stderr}");
    let out = daemon.cli(&["stop", "-f", "--i-am-inside", "--keep-sessions"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
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
            Mode::Default => unreachable!("MODES contains only explicitly selected backends"),
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

/// `close` returns before the host has stopped; a SIGTERM right after must
/// not leave that host running until some later daemon starts.
#[test]
fn sigterm_right_after_close_leaves_no_host_running() {
    for mode in MODES {
        let mut daemon = Parity::start(mode);
        let child = sleeper(&daemon);
        daemon
            .eval("remuda.session.close('keeper')")
            .unwrap_or_else(|error| panic!("{mode:?}: close: {error}"));
        daemon.terminate_daemon();
        let deadline = Instant::now() + PATIENCE;
        while alive(child) || daemon.hosts_running() > 0 {
            assert!(
                Instant::now() < deadline,
                "{mode:?}: after close + SIGTERM: child alive {}, hosts {}",
                alive(child),
                daemon.hosts_running()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// After a crash, the command that starts the next daemon says what that
/// daemon found: host-backed sessions outlive it, in-process ones do not.
#[test]
fn autostart_after_a_crash_names_the_sessions_it_found() {
    for mode in MODES {
        let mut daemon = Parity::start(mode);
        let child = sleeper(&daemon);
        daemon.kill_daemon();
        let out = daemon.cli(&["-e", "return 1"]);
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let survived = alive(child);
        let _ = daemon.cli(&["stop", "-f"]);
        assert!(out.status.success(), "{mode:?}: {stderr}");
        match mode {
            Mode::Hosted => {
                assert!(survived, "hosted: child ended with the daemon");
                assert!(
                    stderr.contains(
                        "none was running; it found 1 host-backed session(s) still running: keeper"
                    ),
                    "hosted: notice: {stderr}"
                );
            }
            Mode::Default => unreachable!("MODES contains only explicitly selected backends"),
            Mode::InProcess => assert!(
                stderr
                    .contains("none was running (sessions of an earlier daemon, if any, are gone)"),
                "in-process: notice: {stderr}"
            ),
        }
    }
}

/// What an attach that just lost its stream reports: a session still
/// running, no daemon, or one that ended. Only a host-backed one survives.
#[test]
fn an_ended_attach_tells_a_lost_daemon_from_an_ended_session() {
    use remuda_native::client::{aftermath, Aftermath};
    for mode in MODES {
        let mut daemon = Parity::start(mode);
        sleeper(&daemon);
        assert_eq!(
            aftermath(&daemon.socket(), "keeper"),
            Aftermath::Running,
            "{mode:?}"
        );
        daemon.kill_daemon();
        assert_eq!(
            aftermath(&daemon.socket(), "keeper"),
            Aftermath::NoDaemon,
            "{mode:?}"
        );
        daemon.restart();
        let expected = match mode {
            Mode::Hosted => Aftermath::Running,
            Mode::Default => unreachable!("MODES contains only explicitly selected backends"),
            Mode::InProcess => Aftermath::Ended,
        };
        assert_eq!(aftermath(&daemon.socket(), "keeper"), expected, "{mode:?}");
        stop(&daemon, false);
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
