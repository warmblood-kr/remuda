//! A daemon asked to stop by a signal says so and goes in order, like the
//! `Shutdown` request: SIGTERM/SIGINT log a line to its stderr (the
//! `<server>.log`), reap, remove the socket and exit 0. SIGHUP is ignored, as
//! tmux's server does, when the daemon leads its own session (auto-started,
//! #106/#107); a hand-run `remuda daemon` whose terminal hangs up stops in
//! order instead. SFDPOT Charter 2 HIGH-1.
#![cfg(unix)]

use remuda_native::daemon;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Daemon {
    child: Child,
    socket: PathBuf,
    log: PathBuf,
}

impl Daemon {
    fn spawn(tag: &str) -> Self {
        Self::spawn_with(tag, false)
    }

    /// `leader`: its own session, as `start_daemon` spawns it (#107).
    fn spawn_with(tag: &str, leader: bool) -> Self {
        use std::os::unix::process::CommandExt;
        let dir = std::env::temp_dir().join(format!("rds-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("daemon.log");
        let mut command = Command::new(env!("CARGO_BIN_EXE_remuda"));
        if leader {
            // SAFETY: setsid is async-signal-safe.
            unsafe {
                command.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                })
            };
        }
        let child = command
            .args(["-s", "s", "daemon"])
            .env("REMUDA_RUNTIME_DIR", &dir)
            .env("HOME", &dir)
            .env("LOCALAPPDATA", &dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .expect("spawn daemon");
        let socket = daemon::socket_path_in(&dir, "s");
        let deadline = Instant::now() + Duration::from_secs(10);
        while remuda_native::ipc::connect(&socket).is_err() {
            assert!(Instant::now() < deadline, "daemon never bound {socket:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
        Self { child, socket, log }
    }

    fn signal(&self, signal: libc::c_int) {
        // SAFETY: kill only sends a signal to our own child's pid.
        assert_eq!(
            unsafe { libc::kill(self.child.id() as libc::pid_t, signal) },
            0
        );
    }

    fn wait(&mut self) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn stops_in_order(signal: libc::c_int, name: &str) {
    let mut daemon = Daemon::spawn(name);
    daemon.signal(signal);
    let status = daemon.wait().expect("daemon did not exit");
    let log = daemon.log();
    assert!(status.success(), "{name}: {status:?}, log: {log}");
    assert!(
        log.contains(&format!("remuda daemon: {name}, shutting down")),
        "{name}: nothing said why it went: {log:?}"
    );
    assert!(
        !Path::new(&daemon.socket).exists(),
        "{name}: stale socket left"
    );
}

#[test]
fn sigterm_stops_the_daemon_in_order() {
    stops_in_order(libc::SIGTERM, "SIGTERM");
}

#[test]
fn sigint_stops_the_daemon_in_order() {
    stops_in_order(libc::SIGINT, "SIGINT");
}

#[test]
fn sighup_stops_a_daemon_run_from_a_terminal() {
    stops_in_order(libc::SIGHUP, "SIGHUP");
}

#[test]
fn sighup_is_logged_and_ignored_by_a_detached_daemon() {
    let mut daemon = Daemon::spawn_with("SIGHUP-detached", true);
    daemon.signal(libc::SIGHUP);
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        daemon.child.try_wait().unwrap().is_none(),
        "SIGHUP killed the daemon"
    );
    assert!(
        remuda_native::ipc::connect(&daemon.socket).is_ok(),
        "stopped answering"
    );
    let log = daemon.log();
    assert!(log.contains("remuda daemon: SIGHUP ignored"), "{log:?}");
}

/// The handlers are caught, never SIG_IGN: a caught disposition resets to
/// SIG_DFL on exec, an ignored one is inherited — and a pty child ignoring
/// SIGHUP would outlive its daemon (the master-close hangup is what reaps it).
/// Both kinds of child report their own SIGHUP disposition: 0 = SIG_DFL.
/// Measured with SIG_IGN swapped in: the `remuda.process` child reports 1 and
/// fails this; the pty child still reports 0 (portable-pty's spawn appears to
/// reset dispositions itself), so its assertion pins that, not this change.
#[test]
fn children_of_a_detached_daemon_keep_the_default_sighup() {
    let daemon = Daemon::spawn_with("children", true);
    let dir = daemon.socket.parent().unwrap().to_path_buf();
    let probe = |file: &Path| {
        format!(
            "{{'python3', '-c', 'import signal, sys, time; open(sys.argv[1], \"w\").write(str(int(signal.getsignal(signal.SIGHUP)))); time.sleep(30)', '{}'}}",
            file.display()
        )
    };
    let (pty_file, process_file) = (dir.join("pty.disp"), dir.join("process.disp"));
    let code = format!(
        "remuda.new('probe', {}); remuda.process{{argv = {}}}",
        probe(&pty_file),
        probe(&process_file)
    );
    let answer = remuda_native::client::request(
        &daemon.socket,
        &remuda_core::protocol::Request::Eval { code, name: None },
    )
    .expect("eval");
    assert!(
        matches!(answer, remuda_core::protocol::Response::Value(_)),
        "{answer:?}"
    );

    let read = |file: &Path| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(text) = std::fs::read_to_string(file) {
                if !text.is_empty() {
                    return text;
                }
            }
            assert!(Instant::now() < deadline, "probe never reported {file:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    assert_eq!(read(&pty_file), "0", "pty child's SIGHUP is not SIG_DFL");
    assert_eq!(
        read(&process_file),
        "0",
        "process child's SIGHUP is not SIG_DFL"
    );
}
