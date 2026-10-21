//! `remuda reload` through real daemons (#713): the running daemon hands its
//! socket, lock and session hosts to a daemon started from the target build,
//! then exits. Clients never see the socket go away. A candidate that fails
//! before it serves is killed, and the old daemon takes everything back.
#![cfg(unix)]

use remuda_core::protocol::{Request, Response};
use remuda_core::Size;
use remuda_native::{client, daemon};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
        let path = PathBuf::from(base).join(format!("rrx-{}-{tag}", std::process::id()));
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
    fn lock_pid(&self) -> u32 {
        let mut lock = self.socket().into_os_string();
        lock.push(".lock");
        std::fs::read_to_string(lock)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
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
    /// `env` reaches the candidate too: it inherits the daemon's environment.
    fn spawn(root: &Root, hosted: bool, env: &[(&str, &str)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_remuda"));
        command
            .args(["-s", "s", "daemon"])
            .env("REMUDA_RUNTIME_DIR", &root.0)
            .env("HOME", &root.0)
            .env("XDG_STATE_HOME", root.0.join("state"))
            .env("REMUDA_EXPERIMENTAL_SESSION_HOST", "0")
            .env_remove("REMUDA_TEST_RELOAD_FAULT")
            .envs(env.iter().copied())
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
    fn exited(&mut self) -> bool {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status.success();
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn request(path: &Path, request: &Request) -> Response {
    client::request(path, request).expect("daemon request")
}
fn new_session(path: &Path, name: &str) {
    let made = request(
        path,
        &Request::New {
            name: Some(name.into()),
            command: vec!["/bin/sh".into()],
            size: Size::new(80, 24),
            cwd: Some("/".into()),
            env: None,
        },
    );
    assert_eq!(made, Response::Value(name.into()));
}
fn send_line(path: &Path, name: &str, text: &str) -> Response {
    request(
        path,
        &Request::SendLine {
            name: name.into(),
            text: text.into(),
        },
    )
}
fn wait_for(path: &Path, name: &str, needle: &str) {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Response::Screen(screen) = request(path, &Request::Capture { name: name.into() }) {
            if screen.contains(needle) {
                return;
            }
        }
        assert!(Instant::now() < deadline, "{needle:?} never appeared");
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn text(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}
fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[test]
fn a_reload_hands_over_every_host_and_no_client_sees_the_socket_go() {
    let root = Root::new("happy");
    let path = root.socket();
    let mut old = Daemon::spawn(&root, true, &[("REMUDA_TEST_RELOAD_READY_MS", "15000")]);
    new_session(&path, "one");
    new_session(&path, "two");
    wait_for(&path, "one", "$");
    assert_eq!(
        send_line(&path, "one", "echo BEFORE-$((3*4))"),
        Response::Ok
    );
    wait_for(&path, "one", "BEFORE-12");
    let old_pid = old.0.id();

    // A client that keeps asking all through the reload.
    let stop = Arc::new(AtomicBool::new(false));
    let asking = {
        let (path, stop) = (path.clone(), Arc::clone(&stop));
        std::thread::spawn(move || {
            let (mut answered, mut failed) = (0, Vec::new());
            while !stop.load(Ordering::SeqCst) {
                match client::request(&path, &Request::List) {
                    Ok(Response::Sessions(_)) => answered += 1,
                    other => failed.push(format!("{other:?}")),
                }
            }
            (answered, failed)
        })
    };
    std::thread::sleep(Duration::from_millis(100));

    let out = root.remuda(&["reload"]);
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(stdout.starts_with("reloaded: "), "{stdout}");
    assert!(stdout.contains("\ndaemon now "), "{stdout}");
    assert!(old.exited(), "the old daemon did not exit cleanly");

    std::thread::sleep(Duration::from_millis(200));
    stop.store(true, Ordering::SeqCst);
    let (answered, failed) = asking.join().unwrap();
    assert!(failed.is_empty(), "{} failed: {failed:?}", failed.len());
    assert!(answered > 0);

    // The candidate serves now: it holds the lock and leads its session.
    let new_pid = root.lock_pid();
    assert_ne!(new_pid, old_pid);
    assert!(alive(new_pid));
    // SAFETY: getsid only reads.
    assert_eq!(unsafe { libc::getsid(new_pid as i32) }, new_pid as i32);
    // Both hosts came along, with their screens.
    wait_for(&path, "one", "BEFORE-12");
    assert_eq!(send_line(&path, "one", "echo AFTER-$((5*5))"), Response::Ok);
    wait_for(&path, "one", "AFTER-25");
    assert_eq!(send_line(&path, "two", "echo TWO-$((1+1))"), Response::Ok);
    wait_for(&path, "two", "TWO-2");
    assert!(path.exists());
}

#[test]
fn a_request_the_old_daemon_accepted_is_still_answered_after_the_handover() {
    let root = Root::new("slow");
    let path = root.socket();
    let mut old = Daemon::spawn(&root, true, &[("REMUDA_TEST_RELOAD_READY_MS", "15000")]);
    new_session(&path, "one");
    // Accepted by the old daemon, answered by its Lua two seconds later.
    let slow = {
        let path = path.clone();
        std::thread::spawn(move || {
            client::request(
                &path,
                &Request::Eval {
                    code: "local reply = remuda.pending { timeout = 20 }\n\
                           local s\n\
                           s = remuda.schedule { every = 2, run = function()\n\
                             remuda.cancel(s)\n\
                             reply:resolve(0, 'late', '')\n\
                           end }\n\
                           return reply"
                        .into(),
                    name: None,
                },
            )
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    let out = root.remuda(&["reload"]);
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}{stderr}");
    let answer = slow.join().unwrap();
    assert!(
        matches!(&answer, Ok(Response::CommandResult { exit_code: 0, .. })),
        "{answer:?}"
    );
    assert!(old.exited());
}

#[test]
fn in_process_sessions_block_a_reload_and_nothing_changes() {
    let root = Root::new("legacy");
    let path = root.socket();
    let old = Daemon::spawn(&root, false, &[]);
    new_session(&path, "shell");
    let out = root.remuda(&["reload"]);
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(1), "{stdout}{stderr}");
    assert!(stdout.starts_with("blocked: "), "{stdout}");
    assert_eq!(root.lock_pid(), old.0.id());
    assert_eq!(send_line(&path, "shell", "echo STILL"), Response::Ok);
}

/// The old daemon after a failed handover: same process, holding the lock,
/// driving its hosts again and admitting new host work.
fn still_the_old_daemon(root: &Root, old: &mut Daemon) {
    let path = root.socket();
    assert!(old.0.try_wait().unwrap().is_none(), "the old daemon exited");
    assert_eq!(root.lock_pid(), old.0.id());
    assert_eq!(send_line(&path, "work", "echo BACK-$((6*7))"), Response::Ok);
    wait_for(&path, "work", "BACK-42");
    new_session(&path, "fresh");
    let still = Command::new("/usr/bin/pgrep")
        .args(["-f", "_reload-candidate"])
        .output()
        .unwrap();
    let root_name = root.0.to_string_lossy().into_owned();
    let left: Vec<_> = String::from_utf8_lossy(&still.stdout)
        .lines()
        .filter_map(|pid| pid.trim().parse::<u32>().ok())
        .filter(|pid| {
            let args = Command::new("/bin/ps")
                .args(["-o", "args=", "-p", &pid.to_string()])
                .output()
                .unwrap();
            String::from_utf8_lossy(&args.stdout).contains(&root_name)
        })
        .collect();
    assert!(left.is_empty(), "candidate left running: {left:?}");
}

#[test]
fn a_candidate_that_dies_after_claiming_the_hosts_is_rolled_back() {
    let root = Root::new("dies");
    let path = root.socket();
    let mut old = Daemon::spawn(
        &root,
        true,
        &[
            ("REMUDA_TEST_RELOAD_READY_MS", "15000"),
            ("REMUDA_TEST_RELOAD_FAULT", "exit-before-serving"),
        ],
    );
    new_session(&path, "work");
    wait_for(&path, "work", "$");
    let out = root.remuda(&["reload"]);
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(1), "{stdout}{stderr}");
    assert!(
        stderr.contains("reload failed and was rolled back; this daemon still serves"),
        "{stderr}"
    );
    assert!(
        stderr.contains("candidate exited before its serving"),
        "{stderr}"
    );
    still_the_old_daemon(&root, &mut old);
}

#[test]
fn a_candidate_that_hangs_after_take_is_killed_at_its_deadline_and_rolled_back() {
    let root = Root::new("hangs");
    let path = root.socket();
    let mut old = Daemon::spawn(
        &root,
        true,
        &[
            ("REMUDA_TEST_RELOAD_READY_MS", "5000"),
            ("REMUDA_TEST_RELOAD_FAULT", "hang-before-serving"),
        ],
    );
    new_session(&path, "work");
    wait_for(&path, "work", "$");
    let started = Instant::now();
    let out = root.remuda(&["reload"]);
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(1), "{stdout}{stderr}");
    assert!(
        stderr.contains("candidate sent no serving in time"),
        "{stderr}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    still_the_old_daemon(&root, &mut old);
}
