//! User-facing acceptance for default hosting and safe recovery.
#![cfg(unix)]
#[path = "daemon_support/parity.rs"]
mod parity;
use parity::{Mode, Parity};
use remuda_core::protocol::{Request, Response};
use std::time::{Duration, Instant};

fn instance(daemon: &Parity, name: &str) -> String {
    let Response::Sessions(rows) = daemon.request(&Request::List) else {
        panic!("no session list")
    };
    rows.into_iter()
        .find(|row| row.name == name)
        .and_then(|row| row.instance_id)
        .expect("live session must retain an instance id")
}

fn shell_pid(daemon: &Parity, marker: &str) -> u32 {
    daemon
        .eval(&format!(
            "remuda.send('sh', [[printf '{marker}=%s\\n' \"$$\"]])"
        ))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let screen = daemon.screen("sh");
        if let Some(pid) = screen.lines().find_map(|line| {
            let value = line.trim().strip_prefix(&format!("{marker}="))?;
            value.parse().ok()
        }) {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "missing PID marker {marker}: {screen}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn finish_daemon(daemon: &Parity) {
    assert_eq!(
        daemon.request(&Request::Close {
            name: "sh".into(),
            instance_id: None,
            confirm: None
        }),
        Response::Ok
    );
    let out = daemon.cli(&["stop"]);
    assert!(
        out.status.success(),
        "stop failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn default_reload_preserves_instance_shell_pid_and_input() {
    let daemon = Parity::start(Mode::Default);
    let before_instance = daemon.shell("sh");
    struct StopCandidate<'a>(&'a Parity);
    impl Drop for StopCandidate<'_> {
        fn drop(&mut self) {
            let _ = self.0.cli(&["stop", "-f", "--yes"]);
        }
    }
    let _cleanup = StopCandidate(&daemon);
    let before_pid = shell_pid(&daemon, "PID_BEFORE");
    let out = daemon.cli(&["reload"]);
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(instance(&daemon, "sh"), before_instance);
    assert_eq!(shell_pid(&daemon, "PID_AFTER"), before_pid);
    daemon
        .eval("remuda.send('sh', [[echo ACCEPT_RELOAD:$((3+4))]])")
        .unwrap();
    daemon.wait_for("sh", "ACCEPT_RELOAD:7");
    finish_daemon(&daemon);
}

#[test]
fn default_daemon_crash_does_not_restart_the_session() {
    let mut daemon = Parity::start(Mode::Default);
    let before_instance = daemon.shell("sh");
    let before_pid = shell_pid(&daemon, "PID_BEFORE_CRASH");
    daemon.kill_daemon();
    daemon.restart();
    assert_eq!(instance(&daemon, "sh"), before_instance);
    assert_eq!(shell_pid(&daemon, "PID_AFTER_CRASH"), before_pid);
    daemon
        .eval("remuda.send('sh', [[echo ACCEPT_CRASH:$((3+5))]])")
        .unwrap();
    daemon.wait_for("sh", "ACCEPT_CRASH:8");
    finish_daemon(&daemon);
}

#[test]
fn explicit_in_process_mode_refuses_reload_without_ending_session() {
    let daemon = Parity::start(Mode::InProcess);
    let before_instance = daemon.shell("sh");
    let before_pid = shell_pid(&daemon, "PID_BEFORE_BLOCK");
    let out = daemon.cli(&["reload"]);
    let message = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "in-process session must block reload: {message}"
    );
    assert!(
        message.contains("in-process") && message.contains("sh"),
        "{message}"
    );
    assert_eq!(instance(&daemon, "sh"), before_instance);
    assert_eq!(shell_pid(&daemon, "PID_AFTER_BLOCK"), before_pid);
    finish_daemon(&daemon);
}

#[test]
fn cli_run_default_session_survives_reload_and_accepts_input() {
    use std::io::Write;
    let daemon = Parity::start(Mode::Default);
    struct Cleanup<'a>(&'a Parity);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = self.0.cli(&["stop", "-f", "--yes"]);
        }
    }
    let _cleanup = Cleanup(&daemon);
    let pty = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = portable_pty::CommandBuilder::new(env!("CARGO_BIN_EXE_remuda"));
    command.args(["-s", "s", "run", "-n", "sh", "/bin/sh"]);
    command.env("REMUDA_RUNTIME_DIR", daemon.root());
    command.env("HOME", daemon.root());
    command.env("XDG_STATE_HOME", daemon.root().join("state"));
    let mut child = pty.slave.spawn_command(command).unwrap();
    drop(pty.slave);
    let mut writer = pty.master.take_writer().unwrap();
    let mut reader = pty.master.try_clone_reader().unwrap();
    let output = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let read_output = output.clone();
    let reader_thread = std::thread::spawn(move || {
        use std::io::Read;
        let mut bytes = [0; 4096];
        while let Ok(n) = reader.read(&mut bytes) {
            if n == 0 {
                break;
            }
            read_output.lock().unwrap().extend_from_slice(&bytes[..n]);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Response::Sessions(rows) = daemon.request(&Request::List) {
            if rows
                .iter()
                .any(|row| row.name == "sh" && row.alive && row.attached)
            {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "remuda run did not create session"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    daemon.wait_for("sh", "$");
    let deadline = Instant::now() + Duration::from_secs(15);
    while !String::from_utf8_lossy(&output.lock().unwrap()).contains("$") {
        assert!(Instant::now() < deadline, "CLI did not paint shell prompt");
        std::thread::sleep(Duration::from_millis(20));
    }
    let before_instance = instance(&daemon, "sh");
    writer.write_all(b"echo FROM_CLI:$((7+4))\r").unwrap();
    daemon.wait_for("sh", "FROM_CLI:11");
    writer.write_all(&[0x1c]).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "run client did not detach");
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(writer);
    drop(pty.master);
    let _ = reader_thread.join();
    let before_pid = shell_pid(&daemon, "PID_CLI_BEFORE");
    let reload = daemon.cli(&["reload"]);
    assert!(
        reload.status.success(),
        "{}{}",
        String::from_utf8_lossy(&reload.stdout),
        String::from_utf8_lossy(&reload.stderr)
    );
    assert_eq!(instance(&daemon, "sh"), before_instance);
    assert_eq!(shell_pid(&daemon, "PID_CLI_AFTER"), before_pid);
    let send = daemon.cli(&["send", "sh", "echo FROM_SEND:$((6+6))"]);
    assert!(
        send.status.success(),
        "{}",
        String::from_utf8_lossy(&send.stderr)
    );
    daemon.wait_for("sh", "FROM_SEND:12");
    finish_daemon(&daemon);
}
