//! A daemon starts only session hosts of its own build (#713). Hosts outlive
//! daemons, so an install that lands while a daemon runs must not hand the
//! old daemon a new host. Each test installs a real daemon and host into a
//! private bin directory, then plays the installer's part by hand.
#![cfg(unix)]

use remuda_core::protocol::{Request, Response};
use remuda_core::Size;
use remuda_native::{client, daemon};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(15);
static NEXT: AtomicU64 = AtomicU64::new(0);

/// A short private root holding the install dir and the runtime dir.
struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let base = if cfg!(target_os = "macos") {
            "/private/tmp"
        } else {
            "/tmp"
        };
        let path = PathBuf::from(base).join(format!(
            "rhb-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path.join("bin"))
            .unwrap();
        let root = Self(path);
        place(env!("CARGO_BIN_EXE_remuda"), &root.bin("remuda"));
        place(
            env!("CARGO_BIN_EXE_remuda-session-host"),
            &root.bin("remuda-session-host"),
        );
        root
    }
    fn bin(&self, name: &str) -> PathBuf {
        self.0.join("bin").join(name)
    }
    fn socket(&self) -> PathBuf {
        daemon::socket_path_in(&self.0, "s")
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        for (pid, _) in hosts(&self.0) {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Hard link when the file system allows it, else copy.
fn place(from: impl AsRef<Path>, to: &Path) {
    if std::fs::hard_link(from.as_ref(), to).is_err() {
        std::fs::copy(from.as_ref(), to).unwrap();
    }
}

/// Land `body` at `to` by rename, as install.sh does.
fn install_script(to: &Path, body: &str) {
    let incoming = to.with_extension("incoming");
    std::fs::write(&incoming, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&incoming, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&incoming, to).unwrap();
}

/// The build both test binaries were compiled with.
fn build() -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_remuda-session-host"))
        .arg("version")
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

struct Daemon(Child);
impl Daemon {
    /// The daemon from the root's install dir, opted in to hosting.
    fn spawn(root: &Root) -> Self {
        let daemon = Self(
            Command::new(root.bin("remuda"))
                .args(["-s", "s", "daemon"])
                .env("REMUDA_RUNTIME_DIR", &root.0)
                .env("HOME", &root.0)
                .env("XDG_STATE_HOME", root.0.join("state"))
                .env("REMUDA_EXPERIMENTAL_SESSION_HOST", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn daemon"),
        );
        let deadline = Instant::now() + PATIENCE;
        while remuda_native::ipc::connect(&root.socket()).is_err() {
            assert!(Instant::now() < deadline, "daemon never bound");
            std::thread::sleep(Duration::from_millis(10));
        }
        daemon
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
fn new_session(path: &Path, name: &str) -> Response {
    request(
        path,
        &Request::New {
            name: Some(name.into()),
            command: vec!["/bin/sh".into()],
            size: Size::new(80, 24),
            cwd: Some("/".into()),
            env: None,
        },
    )
}
fn names(path: &Path) -> Vec<String> {
    match request(path, &Request::List) {
        Response::Sessions(list) => list.into_iter().map(|s| s.name).collect(),
        other => panic!("list failed: {other:?}"),
    }
}
fn echo(path: &Path, name: &str, marker: &str) {
    let sent = request(
        path,
        &Request::SendLine {
            name: name.into(),
            text: format!("echo {marker}-$((40+2))"),
        },
    );
    assert!(!matches!(sent, Response::Error(_)), "{name}: {sent:?}");
    let want = format!("{marker}-42");
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Response::Screen(screen) = request(path, &Request::Capture { name: name.into() }) {
            if screen.contains(&want) {
                return;
            }
        }
        assert!(Instant::now() < deadline, "{name} never echoed {want}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Every host process under `root`, with the executable it was started from.
fn hosts(root: &Path) -> Vec<(i32, String)> {
    let output = Command::new("/bin/ps")
        .args(["-A", "-o", "pid=", "-o", "args="])
        .output()
        .expect("ps");
    let needle = root.to_string_lossy().into_owned();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains(&needle) && line.contains("run-reserved"))
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pid = words.next()?.parse().ok()?;
            Some((pid, words.next()?.to_owned()))
        })
        .collect()
}
fn executables(root: &Root) -> Vec<String> {
    let mut found: Vec<_> = hosts(&root.0).into_iter().map(|(_, exe)| exe).collect();
    found.sort();
    found
}

#[test]
fn an_install_landing_while_the_daemon_runs_never_gives_it_the_new_host() {
    let root = Root::new();
    let socket = root.socket();
    let build = build();
    let plain = root.bin("remuda-session-host").display().to_string();
    let versioned = root.bin(&format!("remuda-session-host-{build}"));
    let daemon = Daemon::spawn(&root);

    assert_eq!(
        new_session(&socket, "before"),
        Response::Value("before".into())
    );
    assert_eq!(executables(&root), vec![plain.clone()]);

    // The installer keeps the running build's host under its versioned name,
    // then lands a host of a newer build under the plain name.
    place(root.bin("remuda-session-host"), &versioned);
    install_script(
        &root.bin("remuda-session-host"),
        "[ \"$1\" = version ] && echo 9.9.9+newer && exit 0; echo started >&2; exit 0",
    );
    assert_eq!(
        new_session(&socket, "during"),
        Response::Value("during".into())
    );
    let mut want = vec![plain.clone(), versioned.display().to_string()];
    want.sort();
    assert_eq!(
        executables(&root),
        want,
        "the old daemon used its own build"
    );

    // With its versioned host gone too, the old daemon refuses at once and
    // writes nothing: no new host, no reservation holding the name.
    std::fs::remove_file(&versioned).unwrap();
    let started = Instant::now();
    let refused = new_session(&socket, "after");
    assert!(
        matches!(&refused, Response::Error(text)
            if text.contains("no session host of this daemon's build")
                && text.contains("9.9.9+newer")),
        "{refused:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(hosts(&root.0).len(), 2);
    assert!(!names(&socket).contains(&"after".to_owned()));

    // Sessions started before and during the install still work.
    echo(&socket, "before", "b");
    echo(&socket, "during", "d");
    drop(daemon);
}

#[test]
fn a_host_swapped_after_the_check_refuses_to_start_and_the_daemon_says_so_quickly() {
    let root = Root::new();
    let socket = root.socket();
    let build = build();
    // Answers `version` with the daemon's build, as the real host did when the
    // daemon checked, but is another build when started: it exits the way a
    // real host of another build exits.
    install_script(
        &root.bin("remuda-session-host"),
        &format!("[ \"$1\" = version ] && echo '{build}' && exit 0; exit 3"),
    );
    let daemon = Daemon::spawn(&root);
    let started = Instant::now();
    let refused = new_session(&socket, "swapped");
    assert!(
        matches!(&refused, Response::Error(text) if text.contains("is not this daemon's build")),
        "{refused:?}"
    );
    // Well inside the 10 s publication deadline.
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    drop(daemon);
}

#[test]
fn a_real_host_of_another_build_exits_before_reading_its_bootstrap() {
    let out = Command::new(env!("CARGO_BIN_EXE_remuda-session-host"))
        .args(["run-reserved", "/nonexistent/endpoint"])
        .env("REMUDA_SESSION_HOST_BUILD", "0.0.0+someone-else")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("daemon expects 0.0.0+someone-else"),
        "{stderr}"
    );
}
