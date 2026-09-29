#![cfg(unix)]

use remuda_native::daemon;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[path = "daemon_support/spawn.rs"]
mod spawn;
use spawn::Daemon;

fn run_bounded(dir: &Path, args: &[&str], limit: Duration) -> (std::process::Output, Duration) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(["-s", "s"])
        .args(args)
        .env("REMUDA_RUNTIME_DIR", dir)
        .env("HOME", dir.join("home"))
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start CLI command");
    let start = Instant::now();
    loop {
        if child.try_wait().expect("poll CLI command").is_some() {
            let elapsed = start.elapsed();
            return (
                child.wait_with_output().expect("collect CLI output"),
                elapsed,
            );
        }
        if start.elapsed() >= limit {
            let _ = child.kill();
            let _ = child.wait();
            panic!("CLI command {args:?} exceeded {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn cli_commands_fail_once_on_version_timeout_for_a_stopped_daemon() {
    let dir = std::env::temp_dir().join(format!("remuda-stopped-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create private runtime directory");
    let daemon = Daemon::spawn(&dir);
    let pid = daemon.0.id();
    let socket = daemon::socket_path_in(&dir, "s");
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGSTOP) }, 0);

    for args in [&["ls"][..], &["-e", "return 1"][..]] {
        let (output, elapsed) = run_bounded(&dir, args, Duration::from_secs(15));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?} unexpectedly succeeded");
        assert!(
            elapsed < Duration::from_secs(15),
            "{args:?} took {elapsed:?}"
        );
        assert!(stderr.contains("timed out after 10s"), "{args:?}: {stderr}");
        assert!(
            stderr.contains(&socket.display().to_string()),
            "{args:?}: {stderr}"
        );
        assert!(stderr.contains(&format!("pid {pid}")), "{args:?}: {stderr}");
        assert!(stderr.contains("kill -CONT"), "{args:?}: {stderr}");
        assert!(
            !stderr.contains("could not confirm its version"),
            "{args:?} printed the misleading version-skew warning: {stderr}"
        );
    }

    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGCONT) }, 0);
    drop(daemon);
    std::fs::remove_dir_all(&dir).expect("remove private runtime directory");
}
