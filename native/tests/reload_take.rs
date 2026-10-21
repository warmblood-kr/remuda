//! A reload candidate takes over a daemon's endpoint (#713). This test plays
//! the old daemon: it holds the listener and the socket lock, starts a real
//! candidate, and hands both over with `Take`. The candidate claims the
//! session hosts and serves, answering even a client that connected before
//! the handover.
#![cfg(unix)]

use remuda_core::protocol::{Request, Response};
use remuda_core::Size;
use remuda_native::{client, daemon, reload};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
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
        let path = PathBuf::from(base).join(format!("rrt-{}-{tag}", std::process::id()));
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
    fn lock(&self) -> PathBuf {
        let mut lock = self.socket().into_os_string();
        lock.push(".lock");
        lock.into()
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_remuda"));
        command
            .args(["-s", "s"])
            .args(args)
            .env("REMUDA_RUNTIME_DIR", &self.0)
            .env("HOME", &self.0)
            .env("XDG_STATE_HOME", self.0.join("state"))
            .env("REMUDA_NO_UPDATE_CHECK", "1")
            .env("REMUDA_EXPERIMENTAL_SESSION_HOST", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
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

struct Reaped(Child);
impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn request(path: &Path, request: &Request) -> Response {
    client::request(path, request).expect("daemon request")
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
    }
}

/// A first daemon leaves one hosted session behind, then stops cleanly.
fn leave_a_hosted_session(root: &Root) {
    let path = root.socket();
    let mut first = Reaped(root.command(&["daemon"]).spawn().unwrap());
    let deadline = Instant::now() + PATIENCE;
    while remuda_native::ipc::connect(&path).is_err() {
        assert!(Instant::now() < deadline, "daemon never bound");
        std::thread::sleep(Duration::from_millis(10));
    }
    let made = request(
        &path,
        &Request::New {
            name: Some("work".into()),
            command: vec!["/bin/sh".into()],
            size: Size::new(80, 24),
            cwd: Some("/".into()),
            env: None,
        },
    );
    assert_eq!(made, Response::Value("work".into()));
    wait_for(&path, "work", "$");
    assert_eq!(request(&path, &shutdown()), Response::Ok);
    let deadline = Instant::now() + PATIENCE;
    while first.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "first daemon never exited");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// What the old daemon holds: the bound listener and the locked lock file.
struct Old {
    listener: UnixListener,
    lock: std::fs::File,
}
impl Old {
    fn new(root: &Root) -> Self {
        let listener = UnixListener::bind(root.socket()).unwrap();
        std::fs::set_permissions(root.socket(), std::fs::Permissions::from_mode(0o600)).unwrap();
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(root.lock())
            .unwrap();
        // SAFETY: flock on an fd this test owns.
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        Self { listener, lock }
    }
}

/// A candidate with its end of a socketpair on fd 3, as the daemon starts it.
fn candidate(root: &Root) -> (UnixStream, Reaped) {
    use std::os::unix::process::CommandExt;
    let (ours, theirs) = UnixStream::pair().unwrap();
    let fd = theirs.as_raw_fd();
    let mut command = root.command(&[reload::CANDIDATE_VERB, &root.socket().to_string_lossy()]);
    command.stderr(Stdio::inherit());
    // SAFETY: dup2 is async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(fd, reload::CANDIDATE_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().unwrap();
    drop(theirs);
    ours.set_read_timeout(Some(PATIENCE)).unwrap();
    (ours, Reaped(child))
}
fn frame(json: &str) -> Vec<u8> {
    let mut bytes = u32::try_from(json.len()).unwrap().to_be_bytes().to_vec();
    bytes.extend_from_slice(json.as_bytes());
    bytes
}
fn read(stream: &mut UnixStream) -> serde_json::Value {
    let mut length = [0; 4];
    stream.read_exact(&mut length).expect("candidate message");
    let mut bytes = vec![0; u32::from_be_bytes(length) as usize];
    stream.read_exact(&mut bytes).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}
fn hello_and_ready(stream: &mut UnixStream) {
    let hello = read(stream);
    assert_eq!(hello["kind"], "Hello", "{hello}");
    assert_eq!(hello["handoff"], reload::HANDOFF);
    let ready = read(stream);
    assert_eq!(ready["kind"], "Ready", "{ready}");
    assert_eq!(ready["hosts"][0]["name"], "work", "{ready}");
    assert!(ready["hosts"][0]["build"].is_string(), "{ready}");
}

#[test]
fn a_candidate_handed_the_endpoint_claims_the_hosts_and_serves() {
    let root = Root::new("take");
    let path = root.socket();
    leave_a_hosted_session(&root);
    let old = Old::new(&root);

    // Connected while the old daemon holds the listener and never accepts.
    let early = {
        let path = path.clone();
        std::thread::spawn(move || request(&path, &Request::Version))
    };
    std::thread::sleep(Duration::from_millis(100));

    let (mut stream, mut child) = candidate(&root);
    hello_and_ready(&mut stream);
    let fds: [RawFd; 2] = [old.listener.as_raw_fd(), old.lock.as_raw_fd()];
    reload::fds::send(&stream, &frame(r#"{"kind":"Take"}"#), &fds).unwrap();
    let serving = read(&mut stream);
    assert_eq!(serving["kind"], "Serving", "{serving}");
    // The old daemon lets go; the candidate holds the only copies.
    drop(old);

    assert!(
        matches!(early.join().unwrap(), Response::Value(_)),
        "the queued client was not answered"
    );
    let pid = child.0.id();
    assert_eq!(
        std::fs::read_to_string(root.lock()).unwrap().trim(),
        pid.to_string()
    );
    // SAFETY: getsid only reads.
    assert_eq!(unsafe { libc::getsid(pid as i32) }, pid as i32);
    // It drives the host the first daemon left: it claimed the inventory.
    assert_eq!(
        request(
            &path,
            &Request::SendLine {
                name: "work".into(),
                text: "echo TAKEN-$((2*3))".into(),
            },
        ),
        Response::Ok
    );
    wait_for(&path, "work", "TAKEN-6");

    // It owns the socket now: a clean stop removes it.
    assert_eq!(request(&path, &shutdown()), Response::Ok);
    let deadline = Instant::now() + PATIENCE;
    while child.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "candidate never exited");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!path.exists(), "the socket was left behind");
}

#[test]
fn a_take_without_both_fds_fails_before_claiming_anything() {
    let root = Root::new("short");
    let path = root.socket();
    leave_a_hosted_session(&root);
    let old = Old::new(&root);
    let (mut stream, mut child) = candidate(&root);
    hello_and_ready(&mut stream);
    reload::fds::send(
        &stream,
        &frame(r#"{"kind":"Take"}"#),
        &[old.listener.as_raw_fd()],
    )
    .unwrap();
    let failed = read(&mut stream);
    assert_eq!(failed["kind"], "Failed", "{failed}");
    assert_eq!(failed["reason"], "take carried 1 fds, not 2");
    assert!(!child.0.wait().unwrap().success());

    // Nothing was claimed: a daemon started on the old endpoint's place
    // drives the host at once.
    drop(old);
    std::fs::remove_file(&path).unwrap();
    let _next = Reaped(root.command(&["daemon"]).spawn().unwrap());
    let deadline = Instant::now() + PATIENCE;
    while remuda_native::ipc::connect(&path).is_err() {
        assert!(Instant::now() < deadline, "daemon never bound");
        std::thread::sleep(Duration::from_millis(10));
    }
    let started = Instant::now();
    assert_eq!(
        request(
            &path,
            &Request::SendLine {
                name: "work".into(),
                text: "echo NEXT".into(),
            },
        ),
        Response::Ok
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    let _ = std::io::stderr().flush();
}
