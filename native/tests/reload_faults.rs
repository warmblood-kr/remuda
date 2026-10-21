//! `remuda reload` under faults (#713): reloads that race, reloads in a
//! row, and a new daemon that dies before or after taking over. At every point exactly
//! one daemon serves, or none and the next one recovers the hosts.
#![cfg(unix)]

use remuda_core::protocol::{Request, Response};
use remuda_core::Size;
use remuda_native::{client, daemon};
use std::os::unix::fs::DirBuilderExt;
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
        let path = PathBuf::from(base).join(format!("rrf-{}-{tag}", std::process::id()));
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
        // By verified pid, never by pattern: hosts name the root in argv,
        // and the lock names whichever daemon serves last.
        let ps = Command::new("/bin/ps")
            .args(["-A", "-o", "pid=", "-o", "args="])
            .output()
            .unwrap();
        let needle = format!("{}/", self.0.display());
        let mut pids: Vec<i32> = String::from_utf8_lossy(&ps.stdout)
            .lines()
            .filter(|line| line.contains(&needle))
            .filter_map(|line| line.split_whitespace().next()?.parse().ok())
            .collect();
        pids.extend(serving_daemons(self).into_iter().map(|pid| pid as i32));
        for pid in pids {
            // SAFETY: a signal to a pid just listed under this root.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
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
            .env_remove("REMUDA_EXPERIMENTAL_SESSION_HOST")
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
            // No history file lands in the root after it is removed.
            env: Some([("HISTFILE".into(), "/dev/null".into())].into()),
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

/// Live processes running `remuda -s s daemon` (or a candidate) for this
/// root: the pid in its lock, plus the test's own daemons it names.
fn serving_daemons(root: &Root) -> Vec<u32> {
    let mut lock = root.socket().into_os_string();
    lock.push(".lock");
    std::fs::read_to_string(lock)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .filter(|&pid| alive(pid) && is_daemon(pid))
        .into_iter()
        .collect()
}
fn is_daemon(pid: u32) -> bool {
    let out = Command::new("/bin/ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let args = String::from_utf8_lossy(&out.stdout);
    let args = args.trim_end();
    args.ends_with(" daemon") || args.contains("_reload-candidate")
}

#[test]
fn two_reloads_at_once_leave_exactly_one_daemon_serving() {
    let root = Root::new("race");
    let path = root.socket();
    let mut old = Daemon::spawn(&root, true, &[("REMUDA_TEST_RELOAD_READY_MS", "15000")]);
    new_session(&path, "work");
    wait_for(&path, "work", "$");
    let racers: Vec<_> = (0..2)
        .map(|_| {
            let root_path = root.0.clone();
            std::thread::spawn(move || {
                Command::new(env!("CARGO_BIN_EXE_remuda"))
                    .args(["-s", "s", "reload"])
                    .env("REMUDA_RUNTIME_DIR", &root_path)
                    .env("HOME", &root_path)
                    .env("XDG_STATE_HOME", root_path.join("state"))
                    .env("REMUDA_NO_UPDATE_CHECK", "1")
                    .stdin(Stdio::null())
                    .output()
                    .unwrap()
            })
        })
        .collect();
    let outs: Vec<Output> = racers.into_iter().map(|r| r.join().unwrap()).collect();
    let texts: Vec<(String, String)> = outs.iter().map(text).collect();
    let reloaded = texts
        .iter()
        .filter(|(o, _)| o.starts_with("reloaded: "))
        .count();
    assert_eq!(reloaded, 1, "{texts:#?}");
    let refused = texts
        .iter()
        .find(|(o, _)| !o.starts_with("reloaded: "))
        .unwrap();
    assert!(
        refused.1.contains("another reload is already in progress")
            || refused
                .1
                .contains("already handed its socket to a new daemon"),
        "{texts:#?}"
    );
    assert!(old.exited(), "the old daemon did not exit");
    // Exactly one daemon serves: the one in the lock, and no other candidate.
    std::thread::sleep(Duration::from_millis(300));
    let serving = root.lock_pid();
    assert!(alive(serving) && is_daemon(serving));
    let ps = Command::new("/bin/ps")
        .args(["-A", "-o", "pid=", "-o", "args="])
        .output()
        .unwrap();
    let exe = env!("CARGO_BIN_EXE_remuda");
    let others: Vec<String> = String::from_utf8_lossy(&ps.stdout)
        .lines()
        .filter(|line| {
            line.contains(exe)
                && (line.trim_end().ends_with(" -s s daemon") || line.contains("_reload-candidate"))
        })
        .filter(|line| line.split_whitespace().next() != Some(serving.to_string().as_str()))
        .filter(|line| {
            let pid: u32 = line.split_whitespace().next().unwrap().parse().unwrap();
            // Only ours: a daemon whose lock or env is this root's.
            env_names_root(pid, &root.0)
        })
        .map(str::to_owned)
        .collect();
    assert!(others.is_empty(), "a second daemon serves: {others:?}");
    assert_eq!(send_line(&path, "work", "echo ONE-$((2+2))"), Response::Ok);
    wait_for(&path, "work", "ONE-4");
}

/// Whether `pid` runs with this root as its runtime dir (`ps eww`).
fn env_names_root(pid: u32, root: &Path) -> bool {
    let out = Command::new("/bin/ps")
        .args(["eww", "-o", "command=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).contains(&format!("REMUDA_RUNTIME_DIR={}", root.display()))
}

/// A request the old daemon accepted before it paused, but whose bytes come
/// only after the handover, must not start a second reload from it: that
/// would hand the same socket to a second new daemon.
#[test]
fn a_reload_asked_of_a_daemon_that_already_handed_over_is_refused() {
    use std::io::{BufRead, BufReader, Write};
    let root = Root::new("late");
    let path = root.socket();
    let mut old = Daemon::spawn(&root, true, &[("REMUDA_TEST_RELOAD_READY_MS", "15000")]);
    new_session(&path, "work");
    wait_for(&path, "work", "$");
    // Accepted by the old daemon; it says nothing yet.
    let mut early = std::os::unix::net::UnixStream::connect(&path).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let out = root.remuda(&["reload"]);
    let (stdout, stderr) = text(&out);
    assert!(stdout.starts_with("reloaded: "), "{stdout}{stderr}");
    let first = root.lock_pid();

    let binary = std::fs::canonicalize(env!("CARGO_BIN_EXE_remuda")).unwrap();
    let late = Request::Reload {
        binary: binary.to_string_lossy().into_owned(),
        check: false,
    };
    let mut frame = serde_json::to_vec(&late).unwrap();
    frame.push(b'\n');
    early.write_all(&frame).unwrap();
    early
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let mut answer = String::new();
    let _ = BufReader::new(&early).read_line(&mut answer);
    assert!(
        !answer.contains("reloaded: "),
        "the old daemon handed the socket over a second time: {answer}"
    );
    assert!(old.exited(), "the old daemon did not exit");
    assert_eq!(root.lock_pid(), first, "another daemon took the socket");
    assert_eq!(
        send_line(&path, "work", "echo STILL-$((3+4))"),
        Response::Ok
    );
    wait_for(&path, "work", "STILL-7");
}

/// A daemon that came from a reload reloads again like any other.
#[test]
fn two_reloads_in_a_row_each_hand_over() {
    let root = Root::new("twice");
    let path = root.socket();
    let mut old = Daemon::spawn(&root, true, &[("REMUDA_TEST_RELOAD_READY_MS", "15000")]);
    new_session(&path, "work");
    wait_for(&path, "work", "$");
    let mut pids = vec![root.lock_pid()];
    for round in 1..=2 {
        let out = root.remuda(&["reload"]);
        let (stdout, stderr) = text(&out);
        assert!(out.status.success(), "round {round}: {stdout}{stderr}");
        let pid = root.lock_pid();
        assert!(
            !pids.contains(&pid),
            "round {round}: the same daemon serves"
        );
        let previous = *pids.last().unwrap();
        let deadline = Instant::now() + PATIENCE;
        while alive(previous) && is_daemon(previous) {
            assert!(
                Instant::now() < deadline,
                "round {round}: {previous} never exited"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        pids.push(pid);
        let marker = format!("ROUND-{round}");
        assert_eq!(
            send_line(&path, "work", &format!("echo {marker}")),
            Response::Ok
        );
        wait_for(&path, "work", &marker);
    }
    assert!(old.exited());
}

/// No daemon at all is the one state a reload cannot prevent: the new one
/// dies after taking over. The hosts outlive it, and the next daemon to
/// start finds them and drives them.
#[test]
fn hosts_outlive_a_new_daemon_that_dies_and_the_next_one_drives_them() {
    let root = Root::new("dies");
    let path = root.socket();
    let _old = Daemon::spawn(&root, true, &[("REMUDA_TEST_RELOAD_READY_MS", "15000")]);
    new_session(&path, "work");
    wait_for(&path, "work", "$");
    assert_eq!(
        send_line(&path, "work", "echo BEFORE-$((4*4))"),
        Response::Ok
    );
    wait_for(&path, "work", "BEFORE-16");
    let out = root.remuda(&["reload"]);
    assert!(out.status.success(), "{:?}", text(&out));
    let new = root.lock_pid();
    assert!(is_daemon(new));
    // SAFETY: the pid is this root's serving daemon, checked just above.
    unsafe { libc::kill(new as i32, libc::SIGKILL) };
    let deadline = Instant::now() + PATIENCE;
    while alive(new) {
        assert!(Instant::now() < deadline, "the new daemon never died");
        std::thread::sleep(Duration::from_millis(10));
    }
    let _next = Daemon::spawn(&root, true, &[]);
    wait_for(&path, "work", "BEFORE-16");
    assert_eq!(send_line(&path, "work", "echo NEXT-$((5*5))"), Response::Ok);
    wait_for(&path, "work", "NEXT-25");
}

/// A new daemon that dies at any step before it takes over changes
/// nothing: the old daemon keeps the socket, the lock and its sessions, and
/// is left with no child but its session hosts.
#[test]
fn a_new_daemon_that_dies_before_taking_over_leaves_the_old_one_serving() {
    for fault in ["exit-before-hello", "exit-in-preflight", "exit-after-ready"] {
        let root = Root::new(&fault[5..]);
        let path = root.socket();
        let old = Daemon::spawn(&root, true, &[("REMUDA_TEST_RELOAD_FAULT", fault)]);
        new_session(&path, "work");
        wait_for(&path, "work", "$");
        let before = root.lock_pid();

        let out = root.remuda(&["reload"]);
        let (stdout, stderr) = text(&out);
        assert!(!out.status.success(), "{fault}: {stdout}{stderr}");
        assert!(!stdout.contains("reloaded: "), "{fault}: {stdout}{stderr}");
        assert_eq!(root.lock_pid(), before, "{fault}: another daemon serves");
        assert_eq!(
            send_line(&path, "work", "echo KEPT-$((8*8))"),
            Response::Ok,
            "{fault}"
        );
        wait_for(&path, "work", "KEPT-64");
        let deadline = Instant::now() + PATIENCE;
        while children(old.0.id())
            .iter()
            .any(|&pid| is_daemon_or_candidate(pid))
        {
            assert!(Instant::now() < deadline, "{fault}: a candidate is left");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            stderr.contains("candidate exited before"),
            "{fault}: {stderr}"
        );
    }
}

fn children(parent: u32) -> Vec<u32> {
    let ps = Command::new("/bin/ps")
        .args(["-A", "-o", "pid=", "-o", "ppid="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&ps.stdout)
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pid = words.next()?.parse().ok()?;
            (words.next()?.parse::<u32>().ok()? == parent).then_some(pid)
        })
        .collect()
}

/// Any remuda process that is not a session host.
fn is_daemon_or_candidate(pid: u32) -> bool {
    let ps = Command::new("/bin/ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let args = String::from_utf8_lossy(&ps.stdout);
    args.contains("remuda") && !args.contains("session-host")
}

/// The old daemon dies mid-handover. Before it sends the listener, the new
/// daemon has nothing and exits; the hosts outlive both and the next daemon
/// drives them. After it sends the listener, the new daemon serves anyway.
#[test]
fn an_old_daemon_that_dies_mid_handover_leaves_the_sessions_reachable() {
    for fault in ["old-exits-before-take", "old-exits-after-take"] {
        let root = Root::new(&fault[10..]);
        let path = root.socket();
        let mut old = Daemon::spawn(&root, true, &[("REMUDA_TEST_RELOAD_FAULT", fault)]);
        new_session(&path, "work");
        wait_for(&path, "work", "$");
        assert_eq!(
            send_line(&path, "work", "echo BEFORE-$((9*9))"),
            Response::Ok
        );
        wait_for(&path, "work", "BEFORE-81");
        let before = root.lock_pid();

        let out = root.remuda(&["reload"]);
        let (stdout, stderr) = text(&out);
        assert!(!stdout.contains("reloaded: "), "{fault}: {stdout}{stderr}");
        let deadline = Instant::now() + PATIENCE;
        let status = loop {
            if let Some(status) = old.0.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "{fault}: the old daemon lives");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(!status.success(), "{fault}: the fault did not fire");
        let _next = if fault == "old-exits-before-take" {
            let deadline = Instant::now() + PATIENCE;
            while children_of_dead_daemon_alive(&root, before) {
                assert!(Instant::now() < deadline, "{fault}: a candidate is left");
                std::thread::sleep(Duration::from_millis(20));
            }
            Some(Daemon::spawn(&root, true, &[]))
        } else {
            assert_ne!(root.lock_pid(), before, "{fault}: no new daemon serves");
            assert!(is_daemon(root.lock_pid()), "{fault}");
            None
        };
        wait_for(&path, "work", "BEFORE-81");
        assert_eq!(
            send_line(&path, "work", "echo AFTER-$((6*6))"),
            Response::Ok,
            "{fault}"
        );
        wait_for(&path, "work", "AFTER-36");
    }
}

/// Whether a candidate the dead daemon `parent` started is still running,
/// now reparented: a remuda process, not a host, whose env names `root`.
fn children_of_dead_daemon_alive(root: &Root, parent: u32) -> bool {
    let ps = Command::new("/bin/ps")
        .args(["-A", "-o", "pid=", "-o", "args="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&ps.stdout)
        .lines()
        .filter(|line| line.contains("_reload-candidate"))
        .filter_map(|line| line.split_whitespace().next()?.parse::<u32>().ok())
        .any(|pid| pid != parent && env_names_root(pid, &root.0))
}
