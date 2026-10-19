//! Opt-in host-backed sessions through a real daemon process (#713).
//! The daemon is SIGKILLed and restarted, and it reattaches from its
//! private inventory. The host is a separate `remuda-session-host` process.
#![cfg(unix)]

use remuda_core::protocol::{Request, Response};
use remuda_core::Size;
use remuda_native::{client, daemon};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(15);
static NEXT: AtomicU64 = AtomicU64::new(0);

/// A short private root: host socket paths must fit `sun_path`.
struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let base = if cfg!(target_os = "macos") {
            "/private/tmp"
        } else {
            "/tmp"
        };
        let path = PathBuf::from(base).join(format!(
            "rhd-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // Private regardless of umask: host directories refuse shared ancestors.
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        Self(path)
    }
    fn socket(&self) -> PathBuf {
        daemon::socket_path_in(&self.0, "s")
    }
    fn hosts(&self) -> PathBuf {
        self.0.join("remuda").join("s.sock.hosts")
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        // Only processes whose argv names this unique root: our hosts.
        for pid in processes_naming(&self.0) {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Daemon(Child);
impl Daemon {
    fn spawn(root: &Root, opt_in: bool) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_remuda"));
        command
            .args(["-s", "s", "daemon"])
            .env("REMUDA_RUNTIME_DIR", &root.0)
            .env("HOME", &root.0)
            .env("XDG_STATE_HOME", root.0.join("state"))
            .env_remove("REMUDA_EXPERIMENTAL_SESSION_HOST")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if opt_in {
            command.env("REMUDA_EXPERIMENTAL_SESSION_HOST", "1");
        }
        let daemon = Self(command.spawn().expect("spawn daemon"));
        let deadline = Instant::now() + PATIENCE;
        while remuda_native::ipc::connect(&root.socket()).is_err() {
            assert!(Instant::now() < deadline, "daemon never bound");
            std::thread::sleep(Duration::from_millis(10));
        }
        daemon
    }
    fn sigkill(mut self) {
        let pid = self.0.id() as libc::pid_t;
        assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
        let _ = self.0.wait();
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
    let response = request(
        path,
        &Request::New {
            name: Some(name.into()),
            command: vec!["/bin/sh".into()],
            size: Size::new(80, 24),
            cwd: Some("/".into()),
            env: None,
        },
    );
    assert_eq!(response, Response::Value(name.into()));
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
fn capture(path: &Path, name: &str) -> Response {
    request(path, &Request::Capture { name: name.into() })
}
fn wait_for(path: &Path, name: &str, needle: &str) -> String {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Response::Screen(screen) = capture(path, name) {
            if screen.contains(needle) {
                return screen;
            }
            assert!(
                Instant::now() < deadline,
                "{needle:?} never appeared:\n{screen}"
            );
        } else {
            assert!(Instant::now() < deadline, "capture kept failing");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn names(path: &Path) -> Vec<String> {
    match request(path, &Request::List) {
        Response::Sessions(list) => list.into_iter().map(|s| s.name).collect(),
        other => panic!("list failed: {other:?}"),
    }
}
fn pid_after(screen: &str, marker: &str) -> i32 {
    let rest = &screen[screen.find(marker).unwrap() + marker.len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().expect("a pid")
}
fn alive(pid: i32) -> bool {
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}
fn wait_gone(pid: i32) {
    let deadline = Instant::now() + PATIENCE;
    while alive(pid) {
        assert!(Instant::now() < deadline, "{pid} stayed alive");
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn processes_naming(root: &Path) -> Vec<i32> {
    let output = Command::new("/bin/ps")
        .args(["-A", "-o", "pid=", "-o", "args="])
        .output()
        .expect("ps");
    let needle = root.to_string_lossy().into_owned();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains(&needle) && line.contains("run-reserved"))
        .filter_map(|line| line.split_whitespace().next()?.parse().ok())
        .collect()
}
fn host_pid(root: &Root) -> i32 {
    let hosts = processes_naming(&root.0);
    assert_eq!(hosts.len(), 1, "exactly one host process: {hosts:?}");
    hosts[0]
}

#[test]
fn without_the_opt_in_new_sessions_stay_in_process() {
    let root = Root::new();
    let daemon = Daemon::spawn(&root, false);
    let path = root.socket();
    new_session(&path, "plain");
    send_line(&path, "plain", "echo IN:$((40+2))");
    wait_for(&path, "plain", "IN:42");
    assert!(processes_naming(&root.0).is_empty(), "no host process");
    assert!(!root.hosts().exists(), "no host inventory is created");
    drop(daemon);
}

#[test]
fn a_hosted_session_survives_daemon_sigkill_and_is_rediscovered() {
    let root = Root::new();
    let path = root.socket();
    let first = Daemon::spawn(&root, true);
    new_session(&path, "keeper");
    assert_eq!(
        send_line(&path, "keeper", "echo PID:$((6*7-42))$$"),
        Response::Ok
    );
    let screen = wait_for(&path, "keeper", "PID:0");
    let child = pid_after(&screen, "PID:0");
    let host = host_pid(&root);
    first.sigkill();

    // The child and its host outlive the daemon.
    std::thread::sleep(Duration::from_millis(300));
    assert!(alive(child), "child died with the daemon");
    assert!(alive(host), "host died with the daemon");

    // A restarted daemon, even without the opt-in, reattaches.
    let second = Daemon::spawn(&root, false);
    assert_eq!(names(&path), vec!["keeper".to_owned()]);
    let screen = wait_for(&path, "keeper", "PID:0");
    assert_eq!(pid_after(&screen, "PID:0"), child, "same child");
    assert_eq!(
        send_line(&path, "keeper", "echo AGAIN:$((1+1))"),
        Response::Ok
    );
    wait_for(&path, "keeper", "AGAIN:2");
    // Re-read after a quiet period, so a late duplicate would show.
    std::thread::sleep(Duration::from_millis(500));
    let screen = wait_for(&path, "keeper", "AGAIN:2");
    assert_eq!(screen.matches("AGAIN:2").count(), 1, "sent exactly once");
    // Routes that still need a local Session refuse clearly.
    match request(
        &path,
        &Request::MouseState {
            name: "keeper".into(),
        },
    ) {
        Response::Error(message) => assert!(message.contains("not supported yet"), "{message}"),
        other => panic!("expected a refusal, got {other:?}"),
    }

    // The host dies: the daemon keeps the name and answers Unavailable.
    assert_eq!(unsafe { libc::kill(host, libc::SIGKILL) }, 0);
    wait_gone(host);
    let unavailable = |response: Response| match response {
        Response::Error(message) => assert!(
            message.contains("unavailable"),
            "not an Unavailable answer: {message}"
        ),
        other => panic!("expected Unavailable, got {other:?}"),
    };
    unavailable(capture(&path, "keeper"));
    unavailable(send_line(&path, "keeper", "echo LOST"));
    second.sigkill();

    // After another restart the entry is still listed, and still Unavailable.
    let third = Daemon::spawn(&root, false);
    assert_eq!(names(&path), vec!["keeper".to_owned()]);
    unavailable(capture(&path, "keeper"));
    drop(third);
}

fn close(path: &Path, name: &str) -> Response {
    request(
        path,
        &Request::Close {
            name: name.into(),
            instance_id: None,
            confirm: None,
        },
    )
}

#[test]
fn closing_a_hosted_session_stops_its_host_and_retires_it() {
    let root = Root::new();
    let path = root.socket();
    let first = Daemon::spawn(&root, true);
    new_session(&path, "brief");
    send_line(&path, "brief", "echo UP:$((2+3))");
    wait_for(&path, "brief", "UP:5");
    let host = host_pid(&root);
    assert_eq!(close(&path, "brief"), Response::Ok);
    assert!(names(&path).is_empty());
    wait_gone(host);
    // Retired: the name can be used again once the teardown finished.
    let deadline = Instant::now() + PATIENCE;
    loop {
        let response = request(
            &path,
            &Request::New {
                name: Some("brief".into()),
                command: vec!["/bin/sh".into()],
                size: Size::new(80, 24),
                cwd: Some("/".into()),
                env: None,
            },
        );
        if response == Response::Value("brief".into()) {
            break;
        }
        assert!(Instant::now() < deadline, "name never freed: {response:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
    let second_host = host_pid(&root);
    assert_eq!(close(&path, "brief"), Response::Ok);
    first.sigkill();

    // Closed: a restarted daemon does not bring it back, even mid-teardown.
    let second = Daemon::spawn(&root, false);
    assert!(names(&path).is_empty());
    wait_gone(second_host);
    drop(second);
}

#[test]
fn a_session_whose_host_died_can_still_be_closed_and_stays_closed() {
    let root = Root::new();
    let path = root.socket();
    let first = Daemon::spawn(&root, true);
    new_session(&path, "orphan");
    send_line(&path, "orphan", "echo UP:$((2+3))");
    wait_for(&path, "orphan", "UP:5");
    let host = host_pid(&root);
    assert_eq!(unsafe { libc::kill(host, libc::SIGKILL) }, 0);
    wait_gone(host);
    assert_eq!(close(&path, "orphan"), Response::Ok);
    assert!(names(&path).is_empty());
    first.sigkill();
    let second = Daemon::spawn(&root, false);
    assert!(names(&path).is_empty(), "a closed session came back");
    drop(second);
}
