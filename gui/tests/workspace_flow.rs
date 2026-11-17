//! Multi-document workspace against the real private VFS: two tabs, independent
//! navigation, a side-by-side split, close, Markdown and code projections.
//! No user daemon or session is touched.

#[path = "daemon_support/hosts.rs"]
mod hosts;

use remuda_core::protocol::Request;
use remuda_gui::documents::Workspace;
use remuda_native::client;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct Daemon {
    child: Child,
    runtime: PathBuf,
    root: PathBuf,
}

impl Daemon {
    fn start(bin: &Path) -> Self {
        let root = if Path::new("/private/tmp").is_dir() {
            PathBuf::from("/private/tmp")
        } else {
            std::fs::canonicalize(std::env::temp_dir()).unwrap()
        };
        let runtime = root.join(format!(
            "gw-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&runtime);
        std::fs::create_dir_all(runtime.join("home")).unwrap();
        let child = Command::new(bin)
            .args(["-s", "gw", "daemon"])
            .env("REMUDA_RUNTIME_DIR", &runtime)
            .env("HOME", runtime.join("home"))
            .env("XDG_CONFIG_HOME", runtime.join("config"))
            .env("XDG_DATA_HOME", runtime.join("data"))
            .env("XDG_CACHE_HOME", runtime.join("cache"))
            .env("XDG_STATE_HOME", runtime.join("state"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut daemon = Self {
            child,
            runtime,
            root,
        };
        daemon.wait_ready(bin);
        daemon.init_cluster(bin);
        daemon
    }

    fn socket(&self) -> PathBuf {
        remuda_native::daemon::socket_path_in(&self.runtime, "gw")
    }

    fn wait_ready(&mut self, _bin: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while client::request(&self.socket(), &Request::List).is_err() {
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!("private daemon exited before binding: {status}");
            }
            assert!(Instant::now() < deadline, "private daemon did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn init_cluster(&self, bin: &Path) {
        let out = Command::new(bin)
            .args(["-s", "gw", "cluster", "init", "--no-listen"])
            .env("REMUDA_RUNTIME_DIR", &self.runtime)
            .env("HOME", self.runtime.join("home"))
            .env("XDG_CONFIG_HOME", self.runtime.join("config"))
            .env("XDG_DATA_HOME", self.runtime.join("data"))
            .env("XDG_CACHE_HOME", self.runtime.join("cache"))
            .env("XDG_STATE_HOME", self.runtime.join("state"))
            .output()
            .unwrap();
        assert!(out.status.success(), "cluster init failed: {out:?}");
    }

    fn put(&self, bin: &Path, path: &str, bytes: &[u8]) {
        let mut child = Command::new(bin)
            .args(["-s", "gw", "vfs", "put", "-", &format!("remuda:///{path}")])
            .env("REMUDA_RUNTIME_DIR", &self.runtime)
            .env("HOME", self.runtime.join("home"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(bytes).unwrap();
        assert!(child.wait().unwrap().success(), "vfs put {path} failed");
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        assert!(
            self.runtime.starts_with(&self.root),
            "runtime must be scratch"
        );
        let _ = self.child.kill();
        let _ = self.child.wait();
        hosts::stop_hosts_under(&self.runtime);
        let _ = std::fs::remove_dir_all(&self.runtime);
    }
}

fn remuda_bin() -> PathBuf {
    let path = std::env::var_os("REMUDA_BIN").map_or_else(
        || {
            PathBuf::from(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../target/debug/remuda"
            ))
        },
        PathBuf::from,
    );
    assert!(
        path.is_file(),
        "build the daemon binary first: cargo build -p remuda-native --bin remuda ({})",
        path.display()
    );
    path
}

fn settle(workspace: &mut Workspace) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        workspace.poll();
        let loading = workspace
            .tabs()
            .iter()
            .any(|pane| pane.status().starts_with("LOADING"));
        if !loading {
            return;
        }
        assert!(Instant::now() < deadline, "workspace load did not settle");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn frame(workspace: &Workspace, index: usize) -> String {
    workspace.frame(index, 120, 40).join("\n")
}

#[test]
fn workspace_opens_two_documents_switches_splits_and_closes() {
    let bin = remuda_bin();
    let daemon = Daemon::start(&bin);
    daemon.put(
        &bin,
        "alpha.md",
        b"# Alpha\n\nRead [beta](beta.md) and **this**.\n\n- one\n",
    );
    daemon.put(
        &bin,
        "beta.py",
        b"def greet(name):\n    return \"hi \" + name\n",
    );
    daemon.put(&bin, "gamma.md", b"# Gamma\n\nGamma prose.\n");

    let mut workspace = Workspace::new(daemon.socket());
    settle(&mut workspace);

    workspace.open_in_new_tab("alpha.md");
    settle(&mut workspace);
    assert_eq!(workspace.len(), 2);
    assert_eq!(workspace.active_index(), 1);
    assert_eq!(workspace.title(1), "Alpha");
    let alpha = frame(&workspace, 1);
    assert!(alpha.contains("Read beta [1] and this."), "{alpha}");
    assert!(
        !alpha.contains("# Alpha") && !alpha.contains("**this**"),
        "{alpha}"
    );

    workspace.open_in_new_tab("beta.py");
    settle(&mut workspace);
    assert_eq!(workspace.len(), 3);
    assert_eq!(workspace.active_index(), 2);
    let beta = frame(&workspace, 2);
    assert!(beta.contains("def greet(name):"), "{beta}");
    assert!(beta.contains("\"hi \""), "{beta}");
    assert_eq!(workspace.language(2), Some("python"));

    // Independent navigation: navigate the alpha tab to a third document,
    // switch to beta, and switch back; the alpha tab keeps its own location.
    workspace.activate(1);
    assert_eq!(workspace.active().location(), "alpha.md");
    assert!(frame(&workspace, 1).contains("Read beta [1]"));
    workspace.open_in_active("gamma.md");
    settle(&mut workspace);
    assert_eq!(workspace.active().location(), "gamma.md");
    workspace.activate(2);
    assert_eq!(workspace.active().location(), "beta.py");
    workspace.activate(1);
    assert_eq!(workspace.active().location(), "gamma.md");
    assert!(frame(&workspace, 1).contains("Gamma prose."));

    // Two documents side by side: gamma (active) and beta.
    assert!(workspace.split_with(2));
    assert_eq!(workspace.split_index(), Some(2));
    let left = frame(&workspace, workspace.active_index());
    let right = frame(&workspace, 2);
    assert!(left.contains("Gamma prose."), "{left}");
    assert!(right.contains("def greet"), "{right}");

    // Closing the split tab removes the second pane and keeps the other tabs.
    let split = workspace.split_index().unwrap();
    let before = workspace.len();
    assert!(workspace.close(split));
    assert_eq!(workspace.len(), before - 1);
    assert_eq!(workspace.split_index(), None);
    assert!(!workspace.tabs().is_empty());

    // The workspace never drops below one tab.
    while !workspace.is_empty() {
        let active = workspace.active_index();
        if !workspace.close(active) {
            break;
        }
        if workspace.len() <= 1 {
            break;
        }
    }
    assert_eq!(workspace.len(), 1);

    let bounded = workspace.frame(0, usize::MAX, usize::MAX);
    assert!(bounded.len() <= remuda_gui::documents::MAX_ROWS);
    assert!(bounded
        .iter()
        .all(|row| row.chars().count() <= remuda_gui::documents::MAX_COLUMNS));
}
