//! A stop CLI launched by a short-lived command shell has an exited parent.
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

#[path = "daemon_support/spawn.rs"]
mod spawn;

/// A test-owned runtime that is removed after the daemon has been reaped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("remuda-ws{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch runtime");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn cmd_quote(path: &Path) -> String {
    format!("\"{}\"", path.to_string_lossy())
}

fn reap_launcher_after_kill(child: &mut Child) {
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return,
        }
    }
}

fn wait_for_launcher(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                reap_launcher_after_kill(child);
                panic!("cmd launcher did not exit within 10 seconds; it was killed");
            }
            Err(error) => {
                reap_launcher_after_kill(child);
                panic!("could not poll cmd launcher; it was killed: {error}");
            }
        }
    }
}

#[test]
fn stop_cli_succeeds_after_its_parent_exits() {
    let scratch = Scratch::new();
    let mut daemon = spawn::Daemon::spawn(&scratch.0);
    let exe = Path::new(env!("CARGO_BIN_EXE_remuda"));
    let output = scratch.0.join("stop-output.txt");
    let script = scratch.0.join("stop.cmd");
    let launcher_stderr = scratch.0.join("launcher-stderr.txt");
    let body = format!(
        "@echo off\r\nstart \"\" /b {} -s s stop -f --yes > {} 2>&1\r\n",
        cmd_quote(exe),
        cmd_quote(&output)
    );
    std::fs::write(&script, &body).expect("write stop launcher");

    let mut launcher = Command::new("cmd.exe")
        .arg("/c")
        .arg(&script)
        .env("REMUDA_RUNTIME_DIR", &scratch.0)
        .env("HOME", scratch.0.join("home"))
        .env("LOCALAPPDATA", scratch.0.join("home"))
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("REMUDA_DAEMON_ID")
        .env_remove("REMUDA_SESSION_ID")
        .env_remove("REMUDA_SESSION_NAME")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            std::fs::File::create(&launcher_stderr).expect("create launcher stderr file"),
        ))
        .spawn()
        .expect("start short-lived cmd launcher");
    let launcher_status = wait_for_launcher(&mut launcher);
    assert!(
        launcher_status.success(),
        "cmd launcher failed: {launcher_status}; stderr: {}; script:\n{body}",
        std::fs::read_to_string(&launcher_stderr).unwrap_or_default()
    );

    // `start` creates remuda as a child of this short-lived cmd process. The
    // launcher has exited before its result is read, so the CLI's parent is
    // gone by the time this test observes whether the daemon stopped.
    let deadline = Instant::now() + Duration::from_secs(25);
    let stop_output = loop {
        let output_so_far = std::fs::read_to_string(&output).unwrap_or_default();
        if output_so_far.contains("stopped the daemon") {
            break output_so_far;
        }
        if output_so_far.contains("cannot stop this daemon") {
            panic!("stop CLI refused the vanished-parent caller: {output_so_far}");
        }
        if Instant::now() >= deadline {
            let _ = daemon.0.kill();
            panic!("stop CLI produced no result within 25 seconds: {output_so_far}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    assert!(
        daemon.left_on_its_own(),
        "stop CLI reported success but daemon remained"
    );
}
