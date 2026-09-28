//! End-to-end coverage for the cluster tree's read-only local IPC path.

use remuda_core::protocol::{Request, Response};
use remuda_core::Size;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct PrivateDaemon {
    child: Child,
    root: PathBuf,
    runtime: PathBuf,
}

impl PrivateDaemon {
    fn start() -> Self {
        let root = PathBuf::from("/tmp");
        let runtime = root.join(format!("cluster-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&runtime);
        std::fs::create_dir_all(&runtime).unwrap();
        let home = runtime.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", "cluster-tree", "daemon"])
            .env("REMUDA_RUNTIME_DIR", &runtime)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", runtime.join("config"))
            .env("XDG_DATA_HOME", runtime.join("data"))
            .env("XDG_CACHE_HOME", runtime.join("cache"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        Self {
            child,
            root,
            runtime,
        }
    }

    fn path(&self) -> PathBuf {
        remuda_native::daemon::socket_path_in(&self.runtime, "cluster-tree")
    }

    fn wait_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while remuda_native::client::request(&self.path(), &Request::List).is_err() {
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!("private daemon exited before binding: {status}");
            }
            assert!(Instant::now() < deadline, "private daemon did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for PrivateDaemon {
    fn drop(&mut self) {
        assert!(
            self.runtime.starts_with(&self.root),
            "daemon runtime must be scratch"
        );
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.runtime);
    }
}

#[test]
fn local_tree_lists_and_captures_a_session_from_a_private_daemon() {
    let mut daemon = PrivateDaemon::start();
    daemon.wait_ready();
    let response = remuda_native::client::request(
        &daemon.path(),
        &Request::New {
            name: Some("tree-session".into()),
            command: vec!["sh".into(), "-c".into(), "sleep 30".into()],
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .unwrap();
    assert!(matches!(response, Response::Value(name) if name == "tree-session"));

    let frame = remuda_native::cluster_tui::read_frame(
        &daemon.path(),
        "studio",
        Some("studio/tree-session"),
        90,
        24,
    )
    .unwrap();
    assert!(frame.contains("studio / tree-session · live · snapshot 0s ago"));
    assert!(frame.contains("tree-session"));
}
