//! `remuda version --hosts` reports each session host's build through a real
//! daemon (#713). Hosts outlive daemons, so the operator needs to see which
//! builds are still serving sessions before a reload or a rollback.
#![cfg(unix)]

#[path = "daemon_support/reap.rs"]
mod reap;

use remuda_core::protocol::{HostBuild, Request, Response};
use remuda_core::Size;
use remuda_native::{client, daemon};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// A short private root: host socket paths must fit `sun_path`.
struct Root(PathBuf);
impl Root {
    fn new(tag: &str) -> Self {
        let base = if cfg!(target_os = "macos") {
            "/private/tmp"
        } else {
            "/tmp"
        };
        let path = PathBuf::from(base).join(format!("rhv-{}-{tag}", std::process::id()));
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
        let deadline = Instant::now() + Duration::from_secs(15);
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

fn new_session(path: &Path, name: &str) {
    let response = client::request(
        path,
        &Request::New {
            name: Some(name.into()),
            command: vec!["/bin/sh".into()],
            size: Size::new(80, 24),
            cwd: Some("/".into()),
            env: None,
        },
    )
    .unwrap();
    assert_eq!(response, Response::Value(name.into()));
}

/// Both test binaries come from one build, so the daemon's build is the host's.
fn daemon_build(path: &Path) -> String {
    match client::request(path, &Request::Version).unwrap() {
        Response::Value(build) => build,
        other => panic!("version: {other:?}"),
    }
}

#[test]
fn version_hosts_lists_each_hosted_session_with_its_hosts_build() {
    let root = Root::new("list");
    let socket = root.socket();
    let _daemon = Daemon::spawn(&root);
    new_session(&socket, "one");
    new_session(&socket, "two");
    let build = daemon_build(&socket);

    let answer = client::request(&socket, &Request::SessionHostBuilds).unwrap();
    assert_eq!(
        answer,
        Response::SessionHostBuilds(vec![
            HostBuild {
                name: "one".into(),
                build: Some(build.clone()),
            },
            HostBuild {
                name: "two".into(),
                build: Some(build.clone()),
            },
        ])
    );

    let out = root.remuda(&["version", "--hosts"]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        text.lines().collect::<Vec<_>>(),
        [
            format!("remuda {build}"),
            format!("daemon {build}"),
            format!("host one {build}"),
            format!("host two {build}"),
        ],
        "{text}"
    );
}

#[test]
fn version_hosts_without_a_daemon_fails_and_starts_none() {
    let root = Root::new("none");
    let out = root.remuda(&["version", "--hosts"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no daemon running"));
    assert!(remuda_native::ipc::connect(&root.socket()).is_err());
}
