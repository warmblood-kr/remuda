//! An auto-started daemon must lead its own session, as tmux's daemon(3)
//! does — otherwise it shares the starting CLI's terminal session and a
//! hangup (an SSH disconnect) SIGHUPs it and, through its closed pty masters,
//! every session it holds. Issue #106.
#![cfg(unix)]

use std::process::Command;

#[test]
fn auto_started_daemon_leads_its_own_session() {
    let dir = std::env::temp_dir().join(format!("rdd-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let remuda = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", "s"])
            .args(args)
            .env("REMUDA_RUNTIME_DIR", &dir)
            .env("HOME", &dir)
            .env("LOCALAPPDATA", &dir)
            .output()
            .expect("run remuda")
    };

    // popen's shell is the daemon's child, so its $PPID is the daemon.
    let out = remuda(&["-e", r#"return io.popen("echo $PPID"):read("*l")"#]);
    let pid: libc::pid_t = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("no daemon pid: {out:?}"));
    // SAFETY: getsid only reads another process's session id.
    let sid = unsafe { libc::getsid(pid) };
    remuda(&["stop", "-f"]);

    assert_eq!(sid, pid, "daemon {pid} is in session {sid}, not its own");
}
