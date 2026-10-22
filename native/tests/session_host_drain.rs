//! A clean stop drains host-backed work (#713, design #758): an act the host
//! is already running finishes and is answered, the hosts keep running, and
//! the next daemon takes over without waiting for a lease.
#![cfg(unix)]

#[path = "daemon_support/reap.rs"]
mod reap;

use remuda_core::protocol::{Request, Response, Step};
use remuda_core::Size;
use remuda_native::{client, daemon};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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
        let path = PathBuf::from(base).join(format!("rhdr-{}-{tag}", std::process::id()));
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
}
impl Drop for Root {
    fn drop(&mut self) {
        reap::kill_processes_under(&self.0);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Daemon(Child);
impl Daemon {
    fn spawn(root: &Root) -> Self {
        let daemon = Self(
            Command::new(env!("CARGO_BIN_EXE_remuda"))
                .args(["-s", "s", "daemon"])
                .env("REMUDA_RUNTIME_DIR", &root.0)
                .env("HOME", &root.0)
                .env("XDG_STATE_HOME", root.0.join("state"))
                .env("REMUDA_EXPERIMENTAL_SESSION_HOST", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + PATIENCE;
        while remuda_native::ipc::connect(&root.socket()).is_err() {
            assert!(Instant::now() < deadline, "daemon never bound");
            std::thread::sleep(Duration::from_millis(10));
        }
        daemon
    }
    /// When the process exited, waiting at most `PATIENCE`.
    fn exited(&mut self) -> Instant {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if self.0.try_wait().unwrap().is_some() {
                return Instant::now();
            }
            assert!(Instant::now() < deadline, "daemon never exited");
            std::thread::sleep(Duration::from_millis(5));
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
fn shutdown() -> Request {
    Request::Shutdown {
        requester_daemon_id: None,
        requester_session_id: None,
        requester_session_name: None,
        override_hosted: false,
        // The hosts must outlive this stop: the next daemon takes them.
        keep_sessions: true,
    }
}

#[test]
fn a_clean_stop_lets_an_admitted_hosted_act_finish_and_frees_the_host() {
    let root = Root::new("finish");
    let path = root.socket();
    let mut first = Daemon::spawn(&root);
    new_session(&path, "work");
    wait_for(&path, "work", "$");

    // One act that pauses mid-way: the command is typed, Enter comes later.
    let act = {
        let path = path.clone();
        std::thread::spawn(move || {
            let answer = request(
                &path,
                &Request::Feed {
                    name: "work".into(),
                    steps: vec![
                        Step::Burst(b"echo FED-$((6*7))".to_vec()),
                        Step::Pause(1500),
                        Step::Burst(b"\r".to_vec()),
                    ],
                },
            );
            (answer, Instant::now())
        })
    };
    std::thread::sleep(Duration::from_millis(400));
    assert!(!act.is_finished(), "the act should still be pausing");

    // The stop answers at once and the socket goes away at once.
    let asked = Instant::now();
    assert_eq!(request(&path, &shutdown()), Response::Ok);
    while remuda_native::ipc::connect(&path).is_ok() {
        assert!(asked.elapsed() < Duration::from_secs(2), "socket stayed up");
        std::thread::sleep(Duration::from_millis(10));
    }

    // The act still completes, and the daemon exits only after it.
    let (answer, answered) = act.join().unwrap();
    assert_eq!(answer, Response::Ok, "the admitted act was cut");
    let exited = first.exited();
    assert!(
        exited >= answered,
        "the daemon exited before the act finished"
    );

    // The next daemon finds the host, sees the act's effect, and drives it
    // at once: no lease was left held.
    let _second = Daemon::spawn(&root);
    wait_for(&path, "work", "FED-42");
    let started = Instant::now();
    assert_eq!(
        request(
            &path,
            &Request::SendLine {
                name: "work".into(),
                text: "echo NEXT-$((1+1))".into(),
            },
        ),
        Response::Ok
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    wait_for(&path, "work", "NEXT-2");
}
