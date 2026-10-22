//! The VFS index across `remuda reload` (#778, #713): only the daemon that
//! serves the socket writes `index.db`. A candidate claims the index just
//! before it serves; if it dies, the old daemon claims it back and writes.
#![cfg(unix)]
// The listener test probes the cluster port from outside, as a peer would.
#![allow(clippy::disallowed_types)]

#[path = "daemon_support/reap.rs"]
mod reap;

use remuda_native::daemon;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(15);

struct Root(PathBuf);
impl Root {
    fn new(tag: &str) -> Self {
        let base = if cfg!(target_os = "macos") {
            "/private/tmp"
        } else {
            "/tmp"
        };
        let path = PathBuf::from(base).join(format!("rvf-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        Self(path)
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_remuda"));
        command
            .args(["-s", "s"])
            .env("REMUDA_RUNTIME_DIR", &self.0)
            .env("HOME", &self.0)
            .env("XDG_STATE_HOME", self.0.join("state"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_DATA_HOME", self.0.join("data"))
            .env("REMUDA_NO_UPDATE_CHECK", "1")
            .env_remove("REMUDA_EXPERIMENTAL_SESSION_HOST")
            .env_remove("REMUDA_TEST_RELOAD_FAULT");
        command
    }
    fn remuda(&self, args: &[&str], stdin: Option<&[u8]>) -> Output {
        use std::io::Write;
        let mut child = self
            .command()
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(bytes) = stdin {
            child.stdin.take().unwrap().write_all(bytes).unwrap();
        }
        child.wait_with_output().unwrap()
    }
    fn daemon(&self, env: &[(&str, &str)]) -> Child {
        let child = self
            .command()
            .arg("daemon")
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let socket = daemon::socket_path_in(&self.0, "s");
        let deadline = Instant::now() + PATIENCE;
        while remuda_native::ipc::connect(&socket).is_err() {
            assert!(Instant::now() < deadline, "daemon never bound");
            std::thread::sleep(Duration::from_millis(10));
        }
        child
    }
    fn index(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.0.join("state/remuda/cluster/vfs/index.db")).unwrap()
    }
    fn writer(&self) -> String {
        self.index()
            .query_row("SELECT value FROM meta WHERE key = 'writer'", [], |row| {
                row.get(0)
            })
            .unwrap()
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        // By checked pid, never by pattern (see reap.rs).
        reap::kill_processes_under(&self.0);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ok(output: Output, what: &str) -> String {
    assert!(
        output.status.success(),
        "{what} failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn put(root: &Root, uri: &str, body: &[u8]) {
    ok(
        root.remuda(&["vfs", "put", "-", uri], Some(body)),
        "vfs put",
    );
}

#[test]
fn a_candidate_killed_mid_reload_leaves_the_index_to_the_old_daemon() {
    let root = Root::new("dies");
    let mut old = root.daemon(&[
        ("REMUDA_TEST_RELOAD_READY_MS", "15000"),
        ("REMUDA_TEST_RELOAD_FAULT", "exit-before-serving"),
    ]);
    ok(
        root.remuda(&["cluster", "init", "--no-listen"], None),
        "cluster init",
    );
    put(&root, "remuda:///before.txt", b"before");
    let owner = root.writer();

    // The candidate claims the index, then dies before it serves.
    let out = root.remuda(&["reload"], None);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("rolled back"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(old.try_wait().unwrap().is_none(), "the old daemon exited");

    // The old daemon took the index back: its writes go through.
    assert_eq!(root.writer(), owner);
    put(&root, "remuda:///after.txt", b"after");
    let listed = ok(root.remuda(&["vfs", "ls"], None), "vfs ls");
    assert!(
        listed.contains("before.txt") && listed.contains("after.txt"),
        "{listed}"
    );
    let check: String = root
        .index()
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(check, "ok");
    let _ = old.kill();
    let _ = old.wait();
}

#[test]
fn after_a_reload_the_new_daemon_owns_the_index() {
    let root = Root::new("ok");
    let mut old = root.daemon(&[("REMUDA_TEST_RELOAD_READY_MS", "15000")]);
    ok(
        root.remuda(&["cluster", "init", "--no-listen"], None),
        "cluster init",
    );
    put(&root, "remuda:///before.txt", b"before");
    let owner = root.writer();
    // A check starts a candidate that never serves, so it never claims.
    let checked = ok(root.remuda(&["reload", "--check"], None), "reload --check");
    assert!(checked.starts_with("ready:"), "{checked}");
    assert_eq!(
        root.writer(),
        owner,
        "a --check candidate claimed the index"
    );
    put(&root, "remuda:///checked.txt", b"still the old daemon");
    let out = ok(root.remuda(&["reload"], None), "reload");
    assert!(out.starts_with("reloaded:"), "{out}");
    let deadline = Instant::now() + PATIENCE;
    while old.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the old daemon did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_ne!(
        root.writer(),
        owner,
        "the new daemon did not claim the index"
    );
    put(&root, "remuda:///after.txt", b"after");
    let listed = ok(root.remuda(&["vfs", "ls"], None), "vfs ls");
    assert!(
        listed.contains("before.txt") && listed.contains("after.txt"),
        "{listed}"
    );
}

/// The cluster listener crosses a reload: the old daemon stops it before
/// the handover and the new one starts it on the same address; a rollback
/// starts it again in the old daemon.
#[test]
fn the_cluster_listener_follows_whichever_daemon_serves() {
    for fault in [None, Some("exit-before-serving")] {
        let root = Root::new(if fault.is_some() { "lback" } else { "lmove" });
        let mut env = vec![("REMUDA_TEST_RELOAD_READY_MS", "15000")];
        env.extend(fault.map(|fault| ("REMUDA_TEST_RELOAD_FAULT", fault)));
        let mut old = root.daemon(&env);
        ok(
            root.remuda(&["cluster", "init", "--no-listen"], None),
            "cluster init",
        );
        let address = listen(&root);

        let out = root.remuda(&["reload"], None);
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if fault.is_some() {
            assert_eq!(out.status.code(), Some(1), "{stderr}");
            assert!(stderr.contains("rolled back"), "{stderr}");
            assert!(old.try_wait().unwrap().is_none(), "the old daemon exited");
        } else {
            assert!(out.status.success(), "{stderr}");
            let deadline = Instant::now() + PATIENCE;
            while old.try_wait().unwrap().is_none() {
                assert!(Instant::now() < deadline, "the old daemon did not exit");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let deadline = Instant::now() + PATIENCE;
        while std::net::TcpStream::connect_timeout(&address, Duration::from_millis(50)).is_err() {
            assert!(
                Instant::now() < deadline,
                "{fault:?}: nothing listens on {address} after the reload"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = old.kill();
        let _ = old.wait();
    }
}

/// Turn on the cluster listener at a free loopback port; returns it once
/// it accepts.
fn listen(root: &Root) -> std::net::SocketAddr {
    use remuda_native::cluster::listener_config::{self, ListenerBind, ListenerConfig};
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = probe.local_addr().unwrap();
    drop(probe);
    listener_config::write_at(
        &root.0.join("state/remuda/cluster"),
        &ListenerConfig {
            enabled: true,
            bind: ListenerBind::Explicit(address),
            allow_public: false,
        },
    )
    .unwrap();
    remuda_native::client::request(
        &daemon::socket_path_in(&root.0, "s"),
        &remuda_core::protocol::Request::ClusterListener(remuda_core::protocol::ListenerOp::Reload),
    )
    .unwrap();
    let deadline = Instant::now() + PATIENCE;
    while std::net::TcpStream::connect_timeout(&address, Duration::from_millis(50)).is_err() {
        assert!(
            Instant::now() < deadline,
            "the cluster listener did not start"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    address
}
