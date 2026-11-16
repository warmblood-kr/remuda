//! Assembled GUI document flow: the GUI pane, a private daemon, and the real
//! VFS. Exercises list -> open Markdown -> follow a link -> back, plus error,
//! unsupported-image and bound checks. No user daemon or session is touched.

#[path = "daemon_support/hosts.rs"]
mod hosts;

use remuda_core::protocol::Request;
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
        let root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let runtime = root.join(format!(
            "gui-docs-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&runtime);
        std::fs::create_dir_all(runtime.join("home")).unwrap();
        let child = Command::new(bin)
            .args(["-s", "gui-docs", "daemon"])
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
        remuda_native::daemon::socket_path_in(&self.runtime, "gui-docs")
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
            .args(["-s", "gui-docs", "cluster", "init", "--no-listen"])
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
            .args(["-s", "gui-docs", "vfs", "put", "-", &format!("remuda:///{path}")])
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
        assert!(self.runtime.starts_with(&self.root), "runtime must be scratch");
        let _ = self.child.kill();
        let _ = self.child.wait();
        hosts::stop_hosts_under(&self.runtime);
        let _ = std::fs::remove_dir_all(&self.runtime);
    }
}

fn remuda_bin() -> PathBuf {
    let path = std::env::var_os("REMUDA_BIN").map_or_else(
        || PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../target/debug/remuda")),
        PathBuf::from,
    );
    assert!(
        path.is_file(),
        "build the daemon binary first: cargo build -p remuda-native --bin remuda ({})",
        path.display()
    );
    path
}

fn settle(pane: &mut remuda_gui::documents::Pane) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pane.poll();
        if !pane.status().starts_with("LOADING") {
            return;
        }
        assert!(Instant::now() < deadline, "document load did not settle");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn frame(pane: &remuda_gui::documents::Pane) -> String {
    pane.frame(120, 30).join("\n")
}

fn selected_row(pane: &remuda_gui::documents::Pane) -> String {
    pane.frame(120, 30)
        .into_iter()
        .find(|row| row.starts_with("> "))
        .unwrap_or_default()
}

#[test]
fn gui_document_pane_lists_opens_follows_a_link_and_returns() {
    let bin = remuda_bin();
    let daemon = Daemon::start(&bin);
    daemon.put(
        &bin,
        "guide.md",
        b"# Guide\n\nRead **this** [next](next.md).\n\n- one\n",
    );
    daemon.put(&bin, "next.md", b"# Next\n\nSecond page prose.\n");
    daemon.put(&bin, "image.png", &[0x89, b'P', b'N', b'G', 0x00, 0xff]);

    let mut pane = remuda_gui::documents::Pane::new(daemon.socket());
    settle(&mut pane);
    let listing = frame(&pane);
    assert!(listing.contains("guide.md"), "{listing}");
    assert!(listing.contains("next.md"), "{listing}");
    if !selected_row(&pane).contains("guide.md") {
        pane.move_selection(true);
    }
    pane.open_selected();
    settle(&mut pane);
    let shown = frame(&pane);
    assert!(shown.contains("Read this next [1]."), "{shown}");
    assert!(
        !shown.contains("# Guide") && !shown.contains("**this**"),
        "{shown}"
    );
    assert_eq!(pane.location(), "guide.md");

    pane.select_link(true);
    assert!(
        pane.status().contains("remuda:///next.md") || frame(&pane).contains("remuda:///next.md"),
        "{}",
        pane.status()
    );
    pane.open_selected();
    settle(&mut pane);
    assert!(frame(&pane).contains("Second page prose."), "{}", frame(&pane));
    assert_eq!(pane.location(), "next.md");

    pane.back();
    assert!(frame(&pane).contains("Read this next [1]."), "{}", frame(&pane));
    assert_eq!(pane.location(), "guide.md");

    pane.open_document("missing.md");
    settle(&mut pane);
    assert!(pane.status().starts_with("ERROR"), "{}", pane.status());

    pane.open_document("image.png");
    settle(&mut pane);
    assert!(
        pane.status().contains("image preview is not implemented"),
        "{}",
        pane.status()
    );

    let bounded = pane.frame(usize::MAX, usize::MAX);
    assert!(
        bounded.len() <= remuda_gui::documents::MAX_ROWS,
        "{}",
        bounded.len()
    );
    assert!(bounded
        .iter()
        .all(|row| row.chars().count() <= remuda_gui::documents::MAX_COLUMNS));
}
