#![cfg(unix)]
#![allow(clippy::disallowed_types)]

#[path = "daemon_support/hosts.rs"]
mod hosts;
use remuda_core::protocol::{Request, Response};
use remuda_native::cluster::{encoding, AuthorizedNode, NodeState, Registry, ResolvedTarget};
use remuda_native::cluster_remote::{
    ClusterRemoteTransport, RemotePoller, RemoteSource, RemoteState, RemoteTarget,
};
use remuda_native::net::cluster_client::{ClientTimeouts, ClusterClient};
use remuda_native::SystemWallClock;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Node {
    root: PathBuf,
    runtime: PathBuf,
    state: PathBuf,
    name: String,
    public: Vec<u8>,
    private: Vec<u8>,
    daemon: Child,
}

impl Node {
    fn start(label: &str) -> Self {
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        // The default macOS TMPDIR (/var/folders/...) leaves too little of the
        // 104-byte sun_path for runtime/remuda/<node>.sock.
        let temp_root = if cfg!(target_os = "macos") {
            PathBuf::from("/private/tmp")
        } else {
            std::env::temp_dir()
        };
        let root = temp_root.join(format!("rp-{}-{serial}", std::process::id()));
        let runtime = root.join("runtime");
        let state = root.join("state");
        let home = root.join("home");
        for dir in [&runtime, &state, &home] {
            fs::create_dir_all(dir).unwrap();
        }
        let name = format!("{label}-{serial}");
        let daemon = spawn_daemon(&name, &runtime, &root, &state);
        let mut node = Self {
            root,
            runtime,
            state,
            name,
            public: Vec::new(),
            private: Vec::new(),
            daemon,
        };
        node.wait_ready();
        let init = node
            .command()
            .args(["cluster", "init", "--no-listen"])
            .output()
            .unwrap();
        assert!(init.status.success(), "cluster init failed: {init:?}");
        let key =
            remuda_native::cluster::identity::load_keypair_at(&node.state.join("remuda/cluster"))
                .unwrap();
        node.public = key[32..].to_vec();
        node.private = key[..32].to_vec();
        node
    }

    fn fingerprint(&self) -> String {
        encoding::fingerprint(&self.public)
    }

    fn command(&self) -> Command {
        command(
            &self.name,
            &self.runtime,
            &self.root,
            &self.state,
            &self.root.join("home"),
        )
    }

    fn wait_ready(&mut self) {
        let path = remuda_native::daemon::socket_path_in(&self.runtime, &self.name);
        let deadline = Instant::now() + Duration::from_secs(10);
        while remuda_native::client::request(&path, &Request::List).is_err() {
            if let Some(status) = self.daemon.try_wait().unwrap() {
                panic!("daemon exited before ready: {status}");
            }
            assert!(Instant::now() < deadline, "daemon did not bind");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn start_session(&self, script: &str) {
        self.start_named_session("proof", script);
    }

    fn start_named_session(&self, name: &str, script: &str) {
        let lua = format!(
            "remuda.new({}, {{'/bin/sh', '-c', {}}})",
            serde_json::to_string(name).unwrap(),
            serde_json::to_string(script).unwrap()
        );
        let start = self.command().args(["-e", &lua]).output().unwrap();
        assert!(start.status.success(), "session start failed: {start:?}");
    }

    fn stop_daemon(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }

    fn restart_daemon(&mut self) {
        self.stop_daemon();
        self.daemon = spawn_daemon(&self.name, &self.runtime, &self.root, &self.state);
        self.wait_ready();
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        assert!(self.runtime.starts_with(&self.root));
        let _ = self.command().args(["stop", "-f", "--yes"]).output();
        self.stop_daemon();
        hosts::stop_hosts_under(&self.root);
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn spawn_daemon(name: &str, runtime: &Path, root: &Path, state: &Path) -> Child {
    command(name, runtime, root, state, &root.join("home"))
        .args(["daemon"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap()
}

fn command(name: &str, runtime: &Path, root: &Path, state: &Path, home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_remuda"));
    command
        .args(["-s", name])
        .env("REMUDA_RUNTIME_DIR", runtime)
        .env("XDG_RUNTIME_DIR", runtime)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", state);
    command
}

struct Listener {
    address: SocketAddr,
}

impl Listener {
    fn start(node: &Node) -> Self {
        let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reservation.local_addr().unwrap();
        drop(reservation);
        let listener = Self { address };
        use remuda_native::cluster::listener_config::{self, ListenerBind, ListenerConfig};
        listener_config::write_at(
            &node.state.join("remuda/cluster"),
            &ListenerConfig {
                enabled: true,
                bind: ListenerBind::Explicit(listener.address),
                allow_public: false,
            },
        )
        .unwrap();
        let response = remuda_native::client::request(
            &remuda_native::daemon::socket_path_in(&node.runtime, &node.name),
            &Request::ClusterListener(remuda_core::protocol::ListenerOp::Reload),
        )
        .unwrap();
        let remuda_core::protocol::Response::ClusterListenerStatus(status) = response else {
            panic!("unexpected listener response: {response:?}");
        };
        assert!(
            matches!(status, remuda_core::protocol::ListenerStatus::On { addr, .. } if addr == listener.address),
            "listener did not bind its configured address: {status:?}"
        );
        listener.wait_ready();
        listener
    }

    fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match TcpStream::connect(self.address) {
                Ok(stream) => return drop(stream),
                Err(_) => assert!(Instant::now() < deadline, "listener did not bind"),
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn restart(&self, _node: &Node) {
        self.wait_ready();
    }
}

fn admit_pair(left: &Node, right: &Node) {
    for node in [left, right] {
        let registry = Registry {
            authorized_nodes: [left, right]
                .into_iter()
                .map(|member| AuthorizedNode {
                    node_fp: member.fingerprint(),
                    static_pubkey: encoding::encode_base64(&member.public),
                    format_major: 1,
                    format_minor: 0,
                    endpoint: None,
                    delivered_by: None,
                    optional_fields: Default::default(),
                    state: NodeState::Admitted,
                    version: 1,
                    by: left.fingerprint(),
                })
                .collect(),
        };
        fs::write(
            node.state.join("remuda/cluster/authorized_nodes.json"),
            serde_json::to_vec_pretty(&registry).unwrap(),
        )
        .unwrap();
    }
}

fn proof_session(
    source: &dyn RemoteSource,
) -> Option<remuda_native::cluster_remote::RemoteSessionSnapshot> {
    source
        .snapshot()
        .nodes
        .first()?
        .sessions
        .iter()
        .find(|session| session.name == "proof")
        .cloned()
}

fn session_text(session: &remuda_native::cluster_remote::RemoteSessionSnapshot) -> String {
    session
        .screen
        .as_ref()
        .map(|screen| {
            screen
                .cells
                .iter()
                .flat_map(|row| row.iter().map(|cell| cell.text.as_str()))
                .collect()
        })
        .unwrap_or_default()
}

struct RemoteTui {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    pid: u32,
    writer: Box<dyn Write + Send>,
    cols: u16,
    rows: u16,
    output: Arc<std::sync::Mutex<Vec<u8>>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl RemoteTui {
    fn start(node: &Node, target: Option<&str>) -> Self {
        Self::start_at_size(node, target, 100, 24)
    }

    fn start_at_size(node: &Node, target: Option<&str>, cols: u16, rows: u16) -> Self {
        let pty = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = portable_pty::CommandBuilder::new(env!("CARGO_BIN_EXE_remuda"));
        command.args(["-s", &node.name, "cluster", "remote"]);
        if let Some(target) = target {
            command.arg(target);
        }
        command.env("REMUDA_RUNTIME_DIR", &node.runtime);
        command.env("XDG_RUNTIME_DIR", &node.runtime);
        command.env("HOME", node.root.join("home"));
        command.env("XDG_CONFIG_HOME", node.root.join("config"));
        command.env("XDG_DATA_HOME", node.root.join("data"));
        command.env("XDG_CACHE_HOME", node.root.join("cache"));
        command.env("XDG_STATE_HOME", &node.state);
        let child = pty.slave.spawn_command(command).expect("spawn remote TUI");
        let pid = child.process_id().expect("remote TUI child PID");
        drop(pty.slave);
        let writer = pty.master.take_writer().unwrap();
        let mut source = pty.master.try_clone_reader().unwrap();
        let output = Arc::new(std::sync::Mutex::new(Vec::new()));
        let reader_output = output.clone();
        let reader = std::thread::spawn(move || {
            let mut buffer = [0; 2048];
            while let Ok(count) = source.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                reader_output
                    .lock()
                    .unwrap()
                    .extend_from_slice(&buffer[..count]);
            }
        });
        Self {
            child,
            pid,
            writer,
            cols,
            rows,
            output,
            reader: Some(reader),
        }
    }

    fn screen_text(&self) -> String {
        let output = self.output.lock().unwrap();
        let mut parser = vt100::Parser::new(self.rows, self.cols, 0);
        parser.process(&output);
        parser.screen().contents()
    }

    fn output_len(&self) -> usize {
        self.output.lock().unwrap().len()
    }

    fn raw_text_from(&self, offset: usize) -> String {
        let output = self.output.lock().unwrap();
        String::from_utf8_lossy(&output[offset.min(output.len())..]).into_owned()
    }

    fn screens_from(&self, offset: usize) -> (String, String) {
        let output = self.output.lock().unwrap();
        let offset = offset.min(output.len());
        let mut parser = vt100::Parser::new(self.rows, self.cols, 0);
        parser.process(&output[..offset]);
        let before = parser.screen().contents();
        parser.process(&output[offset..]);
        let after = parser.screen().contents();
        (before, after)
    }

    fn any_screen_contains_from(&self, offset: usize, needles: &[&str]) -> bool {
        let output = self.output.lock().unwrap();
        let offset = offset.min(output.len());
        let mut parser = vt100::Parser::new(self.rows, self.cols, 0);
        parser.process(&output[..offset]);
        if needles
            .iter()
            .any(|needle| parser.screen().contents().contains(needle))
        {
            return true;
        }
        for byte in &output[offset..] {
            parser.process(std::slice::from_ref(byte));
            let screen = parser.screen().contents();
            if needles.iter().any(|needle| screen.contains(needle)) {
                return true;
            }
        }
        false
    }

    fn wait_for(&self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let text = self.screen_text();
            if text.contains(needle) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "TUI did not render {needle:?}: {text}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_from(&self, offset: usize, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let (before, after) = self.screens_from(offset);
            if after.contains(needle) && !before.contains(needle) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "TUI did not render {needle:?} after input; screen at offset:\n{before}\ncurrent screen:\n{after}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for RemoteTui {
    fn drop(&mut self) {
        assert_eq!(self.child.process_id(), Some(self.pid));
        unsafe {
            libc::kill(self.pid as libc::pid_t, libc::SIGKILL);
        }
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn wait_for(label: &str, timeout: Duration, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "remote poll condition did not settle: {label}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn real_remote_tui_paints_the_selected_remote_session_screen() {
    let client_node = Node::start("tui-remote-client");
    let server_node = Node::start("tui-remote-server");
    admit_pair(&client_node, &server_node);
    let listener = Listener::start(&server_node);
    server_node.start_session(
        "for i in $(seq 1 22); do printf 'FILLER\\n'; done; while true; do printf 'PR8-MARKER'; sleep 1; done",
    );

    let registry_path = client_node
        .state
        .join("remuda/cluster/authorized_nodes.json");
    let mut registry: Registry =
        serde_json::from_slice(&fs::read(&registry_path).unwrap()).unwrap();
    let server = registry
        .authorized_nodes
        .iter_mut()
        .find(|entry| entry.node_fp == server_node.fingerprint())
        .unwrap();
    server.endpoint = Some(listener.address.to_string());
    fs::write(
        &registry_path,
        serde_json::to_vec_pretty(&registry).unwrap(),
    )
    .unwrap();

    let server_label = remuda_native::cluster::node_label(&server_node.fingerprint());
    let mut tui = RemoteTui::start(&client_node, None);
    tui.wait_for(&server_label, Duration::from_secs(10));
    tui.writer.write_all(b"\x1b[B\x1b[C").unwrap();
    tui.wait_for("proof", Duration::from_secs(10));
    tui.writer.write_all(b"\x1b[B\r").unwrap();
    tui.wait_for("remote live · reachable", Duration::from_secs(10));
    tui.wait_for(&format!("$ {server_label}/proof>"), Duration::from_secs(3));
    tui.wait_for("PR8-MARKER", Duration::from_secs(10));
}

fn assert_remote_screen_columns(tui: &RemoteTui, cols: u16, server_label: &str) {
    let mut parser = vt100::Parser::new(24, cols, 0);
    parser.process(&tui.output.lock().unwrap());
    let screen_cells = (0..24)
        .map(|row| {
            (0..cols)
                .map(|col| {
                    parser
                        .screen()
                        .cell(row, col)
                        .map_or(" ", |cell| cell.contents())
                        .to_owned()
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let screen = screen_cells
        .iter()
        .map(|row| row.concat())
        .collect::<Vec<_>>();
    let column_of = |needle: &str| {
        screen_cells
            .iter()
            .filter_map(|row| {
                let text = row.concat();
                text.find(needle)
                    .map(|byte_column| text[..byte_column].chars().count())
            })
            .min()
            .unwrap_or_else(|| panic!("{needle:?} missing from screen:\n{}", screen.join("\n")))
    };

    assert_eq!(
        column_of("remuda · cluster"),
        0,
        "{cols} columns:\n{}",
        screen.join("\n")
    );
    assert_eq!(
        column_of(&format!("{server_label} ")),
        3,
        "{cols} columns:\n{}",
        screen.join("\n")
    );
    let separator = if cols >= 60 { "│" } else { "─" };
    let separator_rows = screen_cells
        .iter()
        .filter_map(|row| {
            row.iter()
                .position(|cell| cell == separator)
                .map(|column| (row, column))
        })
        .collect::<Vec<_>>();
    assert!(
        !separator_rows.is_empty(),
        "{cols} columns: no pane divider"
    );
    let separator_column = if cols >= 60 {
        let first = separator_rows.first().expect("separator rows").1;
        assert!(
            (24..=48).contains(&first),
            "{cols} columns: pane divider at {first}, outside 24..=48"
        );
        first
    } else {
        0
    };
    for (row, column) in &separator_rows {
        assert_eq!(
            *column,
            separator_column,
            "{cols} columns: pane divider shifted from its row: {}",
            row.concat()
        );
    }

    let marker_column = if cols >= 60 {
        separator_rows
            .iter()
            .find_map(|(row, column)| {
                let after = row.get(*column + 1..)?.concat();
                after
                    .find("CRLF-MARKER")
                    .map(|offset| *column + 1 + after[..offset].chars().count())
            })
            .unwrap_or_else(|| {
                panic!(
                    "CRLF-MARKER missing from the remote pane:\n{}",
                    screen.join("\n")
                )
            })
    } else {
        column_of("CRLF-MARKER")
    };
    let expected_marker_column = if cols >= 60 { separator_column + 1 } else { 0 };
    assert_eq!(
        marker_column,
        expected_marker_column,
        "{cols} columns; remote pane must begin at its layout column:\n{}",
        screen.join("\n")
    );
}

#[test]
fn real_remote_tui_frames_start_each_row_at_column_zero_at_wide_and_narrow_sizes() {
    let client_node = Node::start("crlf-client");
    let server_node = Node::start("crlf-server");
    admit_pair(&client_node, &server_node);
    let listener = Listener::start(&server_node);
    server_node.start_session(
        "for i in $(seq 1 22); do printf 'FILLER\\n'; done; while true; do printf '\\rCRLF-MARKER'; sleep 1; done",
    );

    let registry_path = client_node
        .state
        .join("remuda/cluster/authorized_nodes.json");
    let mut registry: Registry =
        serde_json::from_slice(&fs::read(&registry_path).unwrap()).unwrap();
    registry
        .authorized_nodes
        .iter_mut()
        .find(|entry| entry.node_fp == server_node.fingerprint())
        .unwrap()
        .endpoint = Some(listener.address.to_string());
    fs::write(
        &registry_path,
        serde_json::to_vec_pretty(&registry).unwrap(),
    )
    .unwrap();

    let server_label = remuda_native::cluster::node_label(&server_node.fingerprint());
    for cols in [80, 40] {
        let mut tui = RemoteTui::start_at_size(&client_node, None, cols, 24);
        tui.wait_for(&server_label, Duration::from_secs(10));
        tui.wait_for(" · sync ", Duration::from_secs(10));
        tui.writer.write_all(b"\x1b[B\x1b[C").unwrap();
        tui.wait_for("proof", Duration::from_secs(10));
        tui.writer.write_all(b"\x1b[B\r").unwrap();
        tui.wait_for("remote live · reachable", Duration::from_secs(10));
        tui.wait_for("CRLF-MARKER", Duration::from_secs(10));
        assert_remote_screen_columns(&tui, cols, &server_label);
    }
}

#[test]
#[cfg(unix)]
fn real_ctrl_backslash_byte_returns_from_remote_composer_to_tree() {
    let client_node = Node::start("key-client");
    let server_node = Node::start("key-server");
    admit_pair(&client_node, &server_node);
    let listener = Listener::start(&server_node);
    server_node.start_session("sleep 30");

    let registry_path = client_node
        .state
        .join("remuda/cluster/authorized_nodes.json");
    let mut registry: Registry =
        serde_json::from_slice(&fs::read(&registry_path).unwrap()).unwrap();
    let server = registry
        .authorized_nodes
        .iter_mut()
        .find(|entry| entry.node_fp == server_node.fingerprint())
        .unwrap();
    server.endpoint = Some(listener.address.to_string());
    fs::write(
        &registry_path,
        serde_json::to_vec_pretty(&registry).unwrap(),
    )
    .unwrap();

    let server_label = remuda_native::cluster::node_label(&server_node.fingerprint());
    let mut tui = RemoteTui::start(&client_node, None);
    tui.wait_for(&server_label, Duration::from_secs(10));
    tui.writer.write_all(b"\x1b[B\x1b[C").unwrap();
    tui.wait_for("proof", Duration::from_secs(10));
    tui.writer.write_all(b"\x1b[B\r").unwrap();
    tui.wait_for("remote live · reachable", Duration::from_secs(10));
    tui.wait_for(&format!("$ {server_label}/proof>"), Duration::from_secs(3));

    let output_start = tui.output_len();
    tui.writer.write_all(b"\x1c").unwrap();
    tui.wait_for_from(output_start, "? help", Duration::from_secs(3));
}

#[test]
#[cfg(unix)]
fn real_remote_keys_mode_forwards_character_escape_and_ctrl_c() {
    // This checks the in-process path (docs/concepts/session-host-parity.md).
    if remuda_native::session_host::hosting() {
        eprintln!("skip: hosted remote input is fail-closed until its opt-out survives a daemon restart (core registry apply_hosted_remote_input_batch)");
        return;
    }
    let client_node = Node::start("keys-live-client");
    let server_node = Node::start("keys-live-server");
    admit_pair(&client_node, &server_node);
    let listener = Listener::start(&server_node);
    let control = server_node
        .command()
        .args(["cluster", "control", "on"])
        .output()
        .unwrap();
    assert!(
        control.status.success(),
        "cluster control failed: {control:?}"
    );
    server_node.start_named_session(
        "proof",
        "stty raw -echo; printf 'READY\\r\\n'; while :; do byte=$(dd bs=1 count=1 2>/dev/null | od -An -tu1 | tr -d ' '); case \"$byte\" in 121) printf '\\r\\ngot=y\\r\\n' ;; 27) printf '\\r\\ngot=esc\\r\\n' ;; 3) printf '\\r\\ngot=ctrl-c\\r\\n' ;; esac; done",
    );

    let registry_path = client_node
        .state
        .join("remuda/cluster/authorized_nodes.json");
    let mut registry: Registry =
        serde_json::from_slice(&fs::read(&registry_path).unwrap()).unwrap();
    let server = registry
        .authorized_nodes
        .iter_mut()
        .find(|entry| entry.node_fp == server_node.fingerprint())
        .unwrap();
    server.endpoint = Some(listener.address.to_string());
    fs::write(
        &registry_path,
        serde_json::to_vec_pretty(&registry).unwrap(),
    )
    .unwrap();

    let server_label = remuda_native::cluster::node_label(&server_node.fingerprint());
    let mut tui = RemoteTui::start(&client_node, None);
    tui.wait_for(&server_label, Duration::from_secs(10));
    tui.writer.write_all(b"\x1b[B\x1b[C").unwrap();
    tui.wait_for("proof", Duration::from_secs(10));
    tui.writer.write_all(b"\x1b[B\r").unwrap();
    tui.wait_for("remote live · reachable", Duration::from_secs(10));
    tui.wait_for(&format!("$ {server_label}/proof>"), Duration::from_secs(3));
    tui.writer.write_all(b"\x1c").unwrap();
    tui.writer.write_all(b"k").unwrap();
    tui.wait_for(
        &format!("KEYS {server_label}/proof · Ctrl-\\ back"),
        Duration::from_secs(3),
    );

    let output_start = tui.output_len();
    tui.writer.write_all(b"y").unwrap();
    tui.wait_for_from(output_start, "got=y", Duration::from_secs(10));
    let output_start = tui.output_len();
    tui.writer.write_all(b"\x1b").unwrap();
    tui.wait_for_from(output_start, "got=esc", Duration::from_secs(10));
    let output_start = tui.output_len();
    tui.writer.write_all(b"\x03").unwrap();
    tui.wait_for_from(output_start, "got=ctrl-c", Duration::from_secs(10));
}

#[test]
#[cfg(unix)]
fn real_remote_keys_mode_holds_multiline_paste_until_its_end() {
    // This checks the in-process path (docs/concepts/session-host-parity.md).
    if remuda_native::session_host::hosting() {
        eprintln!("skip: hosted remote input is fail-closed until its opt-out survives a daemon restart (core registry apply_hosted_remote_input_batch)");
        return;
    }
    let client_node = Node::start("paste-live-client");
    let server_node = Node::start("paste-live-server");
    admit_pair(&client_node, &server_node);
    let listener = Listener::start(&server_node);
    let control = server_node
        .command()
        .args(["cluster", "control", "on"])
        .output()
        .unwrap();
    assert!(
        control.status.success(),
        "cluster control failed: {control:?}"
    );
    server_node.start_named_session(
        "proof",
        "stty raw -echo; printf 'READY\\r\\n'; seen=0; count=0; received=''; while :; do byte=$(dd bs=1 count=1 2>/dev/null | od -An -tu1 | tr -d ' '); received=\"$received$byte,\"; count=$((count+1)); if [ \"$count\" = 11 ]; then printf '\\r\\nreceived=%s\\r\\n' \"$received\"; count=0; received=''; fi; case \"$byte\" in 13) printf '\\r\\ngot=enter\\r\\n' ;; 108) if [ \"$seen\" = 0 ]; then seen=1; printf '\\r\\ngot=payload\\r\\n'; fi ;; esac; done",
    );

    let registry_path = client_node
        .state
        .join("remuda/cluster/authorized_nodes.json");
    let mut registry: Registry =
        serde_json::from_slice(&fs::read(&registry_path).unwrap()).unwrap();
    let server = registry
        .authorized_nodes
        .iter_mut()
        .find(|entry| entry.node_fp == server_node.fingerprint())
        .unwrap();
    server.endpoint = Some(listener.address.to_string());
    fs::write(
        &registry_path,
        serde_json::to_vec_pretty(&registry).unwrap(),
    )
    .unwrap();

    let server_label = remuda_native::cluster::node_label(&server_node.fingerprint());
    let mut tui = RemoteTui::start(&client_node, None);
    tui.wait_for(&server_label, Duration::from_secs(10));
    tui.writer.write_all(b"\x1b[B\x1b[C").unwrap();
    tui.wait_for("proof", Duration::from_secs(10));
    tui.writer.write_all(b"\x1b[B\r").unwrap();
    tui.wait_for("remote live · reachable", Duration::from_secs(10));
    tui.wait_for(&format!("$ {server_label}/proof>"), Duration::from_secs(3));
    tui.writer.write_all(b"\x1c").unwrap();
    let keys_start = tui.output_len();
    tui.writer.write_all(b"k").unwrap();
    tui.wait_for(
        &format!("KEYS {server_label}/proof · Ctrl-\\ back"),
        Duration::from_secs(3),
    );
    assert!(
        tui.raw_text_from(keys_start).contains("\x1b[?2004h"),
        "keys mode must enable bracketed paste capture"
    );

    let output_start = tui.output_len();
    tui.writer.write_all(b"\x1b[200~line1\rline2").unwrap();
    std::thread::sleep(Duration::from_millis(250));
    assert!(
        !tui.any_screen_contains_from(output_start, &["got=payload", "got=enter", "Input sent"],),
        "remote received input on an intermediate screen before paste terminator"
    );

    tui.writer.write_all(b"\x1b[201~").unwrap();
    let back_start = tui.output_len();
    tui.writer.write_all(b"\x1c").unwrap();
    tui.wait_for_from(back_start, "? help", Duration::from_secs(3));
    tui.wait_for_from(output_start, "Input sent", Duration::from_secs(10));
    let payload = "received=108,105,110,101,49,13,108,105,110,101,50,";
    tui.wait_for_from(output_start, payload, Duration::from_secs(10));
    let output = tui.output.lock().unwrap();
    let mut parser = vt100::Parser::new(24, 100, 0);
    parser.process(&output);
    let screen = parser.screen().contents();
    let complete_paste_inputs = screen.lines().filter(|line| line.contains(payload)).count();
    assert_eq!(
        complete_paste_inputs, 1,
        "the completed paste should arrive exactly once with all bytes intact; screen:\n{screen}"
    );
    assert!(
        !screen.contains("Input sent ·"),
        "acknowledgement must omit command contents"
    );
}

#[test]
#[cfg(unix)]
fn real_remote_keys_mode_respects_cluster_control_off() {
    let client_node = Node::start("keys-denied-client");
    let server_node = Node::start("keys-denied-server");
    admit_pair(&client_node, &server_node);
    let listener = Listener::start(&server_node);
    let control = server_node
        .command()
        .args(["cluster", "control", "off"])
        .output()
        .unwrap();
    assert!(
        control.status.success(),
        "cluster control failed: {control:?}"
    );
    server_node.start_named_session(
        "proof",
        "stty raw -echo; printf 'READY\\r\\n'; while :; do byte=$(dd bs=1 count=1 2>/dev/null | od -An -tu1 | tr -d ' '); case \"$byte\" in 121) printf '\\r\\ngot=y\\r\\n' ;; esac; done",
    );

    let registry_path = client_node
        .state
        .join("remuda/cluster/authorized_nodes.json");
    let mut registry: Registry =
        serde_json::from_slice(&fs::read(&registry_path).unwrap()).unwrap();
    let server = registry
        .authorized_nodes
        .iter_mut()
        .find(|entry| entry.node_fp == server_node.fingerprint())
        .unwrap();
    server.endpoint = Some(listener.address.to_string());
    fs::write(
        &registry_path,
        serde_json::to_vec_pretty(&registry).unwrap(),
    )
    .unwrap();

    let server_label = remuda_native::cluster::node_label(&server_node.fingerprint());
    let mut tui = RemoteTui::start(&client_node, None);
    tui.wait_for(&server_label, Duration::from_secs(10));
    tui.writer.write_all(b"\x1b[B\x1b[C").unwrap();
    tui.wait_for("proof", Duration::from_secs(10));
    tui.writer.write_all(b"\x1b[B\r").unwrap();
    tui.wait_for("remote live · reachable", Duration::from_secs(10));
    tui.wait_for(&format!("$ {server_label}/proof>"), Duration::from_secs(3));
    tui.writer.write_all(b"\x1c").unwrap();
    tui.writer.write_all(b"k").unwrap();
    tui.wait_for(
        &format!("KEYS {server_label}/proof · Ctrl-\\ back"),
        Duration::from_secs(3),
    );

    let input_start = tui.output_len();
    tui.writer.write_all(b"y").unwrap();
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        !tui.any_screen_contains_from(input_start, &["got=y"]),
        "server accepted input while cluster control was disabled on an intermediate screen"
    );
}

#[test]
fn driver_hosted_remote_tui_capture_shows_live_output_with_trailing_blanks() {
    let client_node = Node::start("driver-remote-client");
    let driver_node = Node::start("driver-remote-driver");
    let server_node = Node::start("driver-remote-server");
    let _client_listener = Listener::start(&client_node);
    let _server_listener = Listener::start(&server_node);
    let invitation = client_node
        .command()
        .args(["cluster", "invite"])
        .output()
        .unwrap();
    assert!(
        invitation.status.success(),
        "cluster invite failed: {invitation:?}"
    );
    let invitation_text = String::from_utf8(invitation.stdout).unwrap();
    let invite_output = invitation_text
        .lines()
        .find(|line| line.starts_with("  remuda cluster join "))
        .expect("printed join command")
        .trim();
    let join_command = invite_output
        .strip_prefix("remuda cluster join '")
        .and_then(|command| command.strip_suffix('\''))
        .expect("shell-quoted join command");
    let (fingerprint, join_line) = join_command
        .split_once("' '")
        .expect("separate fingerprint and join line");
    assert!(!fingerprint.contains('\''));
    assert!(!join_line.contains('\''));
    let join = server_node
        .command()
        .args(["cluster", "join", fingerprint, join_line])
        .output()
        .unwrap();
    assert!(join.status.success(), "cluster join failed: {join:?}");
    server_node.start_session("while :; do echo PR8-MARKER; sleep 1; done");

    let remote = format!(
        "exec env REMUDA_RUNTIME_DIR={} XDG_RUNTIME_DIR={} HOME={} XDG_CONFIG_HOME={} XDG_DATA_HOME={} XDG_CACHE_HOME={} XDG_STATE_HOME={} {} -s {} cluster remote",
        client_node.runtime.display(),
        client_node.runtime.display(),
        client_node.root.join("home").display(),
        client_node.root.join("config").display(),
        client_node.root.join("data").display(),
        client_node.root.join("cache").display(),
        client_node.state.display(),
        env!("CARGO_BIN_EXE_remuda"),
        client_node.name,
    );
    driver_node.start_named_session("screen", &remote);
    driver_node.start_named_session("tree", &remote);
    let capture = || match remuda_native::client::request(
        &remuda_native::daemon::socket_path_in(&driver_node.runtime, &driver_node.name),
        &Request::Capture {
            name: "screen".into(),
        },
    )
    .unwrap()
    {
        Response::Screen(screen) => screen,
        other => panic!("unexpected screen capture response: {other:?}"),
    };
    std::thread::sleep(Duration::from_secs(3));
    for name in ["screen", "tree"] {
        for input in ["\\27[B", "\\27[C", "\\27[B"] {
            let lua = format!("remuda.insert('{name}', '{input}')");
            let result = driver_node.command().args(["-e", &lua]).output().unwrap();
            assert!(result.status.success(), "remuda.insert failed: {result:?}");
        }
    }
    let lua = "remuda.insert('screen', '\\r')";
    let result = driver_node.command().args(["-e", lua]).output().unwrap();
    assert!(result.status.success(), "remuda.insert failed: {result:?}");

    wait_for_capture_request(&server_node);
    std::thread::sleep(Duration::from_millis(500));

    let deadline = Instant::now() + Duration::from_secs(3);
    let screen = loop {
        let screen = capture();
        if screen.contains("remote live · reachable") {
            break screen;
        }
        assert!(
            Instant::now() < deadline,
            "remote session was not selected:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let in_pane = |l: &str| {
        l.split_once('│')
            .is_some_and(|(_, p)| p.starts_with("PR8-MARKER"))
    };
    assert!(
        screen.lines().any(in_pane),
        "driver-hosted 80x24 right pane omitted the live marker:\n{screen}"
    );
    assert!(
        screen.contains("PR8-MARKER"),
        "driver-hosted 80x24 screen pane omitted the live marker:\n{screen}"
    );
}

fn wait_for_capture_request(server_node: &Node) {
    let request_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let output = server_node
            .command()
            .args(["-e", "print(remuda.request_counts().capture_styled)"])
            .output()
            .unwrap();
        assert!(output.status.success(), "request count failed: {output:?}");
        let count = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u64>()
            .unwrap_or_default();
        if count > 0 {
            return;
        }
        assert!(
            Instant::now() < request_deadline,
            "remote pane never requested a screen capture"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn selected_remote_sync_keeps_last_screen_offline_and_resumes_after_listener_restart() {
    const BEFORE_DOWN: &str = "REMOTE_BEFORE_DOWN";
    const AFTER_RECONNECT: &str = "REMOTE_AFTER_RECONNECT";

    let client_node = Node::start("remote-client");
    let mut server_node = Node::start("remote-server");
    admit_pair(&client_node, &server_node);
    let listener = Listener::start(&server_node);
    let marker_before_down = server_node.root.join("emit-before-down");
    let shell = format!(
        "printf REMOTE_START; while [ ! -e '{}' ]; do sleep 0.02; done; while true; do printf '{BEFORE_DOWN}\\n'; sleep 0.1; done",
        marker_before_down.display(),
    );
    server_node.start_session(&shell);
    server_node.start_named_session("unselected", "printf SHOULD_NOT_CAPTURE; sleep 30");

    let server_public = server_node.public.clone();
    let address = listener.address;
    let timeouts = Duration::from_secs(25);
    let client = ClusterClient::with_timeouts(
        Arc::new(SystemWallClock::new()),
        ClientTimeouts {
            connect: Duration::from_secs(2),
            read: timeouts,
            total: timeouts,
        },
    );
    let transport = Arc::new(ClusterRemoteTransport::with_client(
        Zeroizing::new(client_node.private.clone()),
        client,
        move |_target| {
            Ok(ResolvedTarget {
                address,
                pinned_static_key: server_public.clone(),
            })
        },
    ));
    let label = remuda_native::cluster::node_label(&server_node.fingerprint());
    let target = RemoteTarget {
        name: label.clone(),
        registry_key: server_node.fingerprint(),
        addr_override: Some(address),
    };
    let poller = RemotePoller::new(vec![target.clone()], transport);
    let source = poller.source();
    let selection = poller.selection();
    poller.start().unwrap();

    wait_for(
        "initial list without screen",
        Duration::from_secs(10),
        || proof_session(source.as_ref()).is_some_and(|session| session.screen.is_none()),
    );
    selection.select(target.registry_key.clone(), "proof");
    wait_for("selected session screen", Duration::from_secs(10), || {
        proof_session(source.as_ref())
            .is_some_and(|session| session_text(&session).contains("REMOTE_START"))
    });
    let first_list = source.snapshot();
    let listed_sessions = &first_list.nodes[0].sessions;
    assert!(listed_sessions
        .iter()
        .any(|session| { session.name == "proof" && session.screen.is_some() }));
    assert!(listed_sessions
        .iter()
        .any(|session| { session.name == "unselected" && session.screen.is_none() }));
    fs::write(&marker_before_down, b"go").unwrap();
    wait_for("updated selected screen", Duration::from_secs(10), || {
        proof_session(source.as_ref())
            .is_some_and(|session| session_text(&session).contains(BEFORE_DOWN))
    });

    server_node.stop_daemon();
    wait_for(
        "daemon stop becomes unreachable",
        Duration::from_secs(6),
        || {
            source
                .snapshot()
                .nodes
                .first()
                .is_some_and(|node| node.state == RemoteState::Unreachable)
        },
    );
    assert!(proof_session(source.as_ref())
        .is_some_and(|session| session_text(&session).contains(BEFORE_DOWN)));

    server_node.restart_daemon();
    listener.restart(&server_node);
    server_node.start_session("printf REMOTE_AFTER_RECONNECT; sleep 30");
    wait_for(
        "listener restart and screen recovery",
        Duration::from_secs(20),
        || {
            let snapshot = source.snapshot();
            snapshot.nodes.first().is_some_and(|node| {
                node.state == RemoteState::Reachable
                    && proof_session(source.as_ref())
                        .is_some_and(|session| session_text(&session).contains(AFTER_RECONNECT))
            })
        },
    );

    let old_instance = proof_session(source.as_ref()).unwrap().instance_id;
    server_node.restart_daemon();
    wait_for(
        "daemon restart removes prior instance",
        Duration::from_secs(10),
        || proof_session(source.as_ref()).is_none(),
    );
    server_node.start_session("printf REMOTE_NEW_INSTANCE; sleep 30");
    wait_for(
        "new daemon session is captured",
        Duration::from_secs(15),
        || {
            proof_session(source.as_ref()).is_some_and(|session| {
                session.instance_id != old_instance
                    && session.alive
                    && session_text(&session).contains("REMOTE_NEW_INSTANCE")
                    && !session_text(&session).contains("REMOTE_BEFORE_DOWN")
                    && !session_text(&session).contains("REMOTE_AFTER_RECONNECT")
            })
        },
    );
}

#[test]
fn selected_remote_session_with_hangul_text_syncs_within_the_frame_limit() {
    const HANGUL: &str = "안녕하세요 한글 패널 내용";
    let client_node = Node::start("hangul-client");
    let server_node = Node::start("hangul-server");
    admit_pair(&client_node, &server_node);
    let listener = Listener::start(&server_node);
    server_node.start_session(&format!("printf '{HANGUL}\\n'; sleep 600"));
    // A wide pane: blank cells alone used to overflow the Noise frame.
    let out = server_node
        .command()
        .args(["resize", "proof", "500", "150"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");

    let server_public = server_node.public.clone();
    let address = listener.address;
    let transport = Arc::new(ClusterRemoteTransport::with_client(
        Zeroizing::new(client_node.private.clone()),
        ClusterClient::with_timeouts(
            Arc::new(SystemWallClock::new()),
            ClientTimeouts {
                connect: Duration::from_secs(2),
                read: Duration::from_secs(25),
                total: Duration::from_secs(25),
            },
        ),
        move |_target| {
            Ok(ResolvedTarget {
                address,
                pinned_static_key: server_public.clone(),
            })
        },
    ));
    let target = RemoteTarget {
        name: remuda_native::cluster::node_label(&server_node.fingerprint()),
        registry_key: server_node.fingerprint(),
        addr_override: Some(address),
    };
    let poller = RemotePoller::new(vec![target.clone()], transport);
    let source = poller.source();
    poller
        .selection()
        .select(target.registry_key.clone(), "proof");
    poller.start().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let session = proof_session(source.as_ref());
        if session
            .as_ref()
            .is_some_and(|s| session_text(s).contains("안녕하세요"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no Hangul screen; last_error = {:?}",
            session.map(|s| s.last_error)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn selected_remote_session_shows_a_change_soon_after_the_previous_one() {
    let client_node = Node::start("cadence-client");
    let server_node = Node::start("cadence-server");
    admit_pair(&client_node, &server_node);
    let listener = Listener::start(&server_node);
    let marks = server_node.root.join("marks");
    fs::create_dir_all(&marks).unwrap();
    server_node.start_session(&format!(
        "printf READY; i=1; while true; do while [ ! -e '{}/m'$i ]; do sleep 0.01; done; printf ' <M'$i'>'; i=$((i+1)); done",
        marks.display()
    ));
    let server_public = server_node.public.clone();
    let address = listener.address;
    let timeouts = Duration::from_secs(25);
    let client = ClusterClient::with_timeouts(
        Arc::new(SystemWallClock::new()),
        ClientTimeouts {
            connect: Duration::from_secs(2),
            read: timeouts,
            total: timeouts,
        },
    );
    let transport = Arc::new(ClusterRemoteTransport::with_client(
        Zeroizing::new(client_node.private.clone()),
        client,
        move |_target| {
            Ok(ResolvedTarget {
                address,
                pinned_static_key: server_public.clone(),
            })
        },
    ));
    let target = RemoteTarget {
        name: remuda_native::cluster::node_label(&server_node.fingerprint()),
        registry_key: server_node.fingerprint(),
        addr_override: Some(address),
    };
    let poller = RemotePoller::new(vec![target.clone()], transport);
    let source = poller.source();
    poller
        .selection()
        .select(target.registry_key.clone(), "proof");
    poller.start().unwrap();
    wait_for("selected session screen", Duration::from_secs(10), || {
        proof_session(source.as_ref())
            .is_some_and(|session| session_text(&session).contains("READY"))
    });
    // Before #622 the next poll waited 2s after each delivery: ~1.5s here.
    for mark in 1..=3 {
        std::thread::sleep(Duration::from_millis(500));
        let needle = format!("<M{mark}>");
        let start = Instant::now();
        fs::write(marks.join(format!("m{mark}")), b"").unwrap();
        wait_for(&needle, Duration::from_secs(10), || {
            proof_session(source.as_ref())
                .is_some_and(|session| session_text(&session).contains(&needle))
        });
        let latency = start.elapsed();
        assert!(
            latency < Duration::from_millis(1000),
            "{needle} after {latency:?}"
        );
    }
}

#[test]
fn switching_away_from_a_quiet_selected_session_does_not_wait_out_its_sync() {
    let client_node = Node::start("switch-client");
    let server_node = Node::start("switch-server");
    admit_pair(&client_node, &server_node);
    let listener = Listener::start(&server_node);
    server_node.start_session("printf QUIET_PROOF; sleep 60");
    server_node.start_named_session("other", "printf OTHER_SCREEN; sleep 60");
    let server_public = server_node.public.clone();
    let address = listener.address;
    let timeouts = Duration::from_secs(25);
    let client = ClusterClient::with_timeouts(
        Arc::new(SystemWallClock::new()),
        ClientTimeouts {
            connect: Duration::from_secs(2),
            read: timeouts,
            total: timeouts,
        },
    );
    let transport = Arc::new(ClusterRemoteTransport::with_client(
        Zeroizing::new(client_node.private.clone()),
        client,
        move |_target| {
            Ok(ResolvedTarget {
                address,
                pinned_static_key: server_public.clone(),
            })
        },
    ));
    let target = RemoteTarget {
        name: remuda_native::cluster::node_label(&server_node.fingerprint()),
        registry_key: server_node.fingerprint(),
        addr_override: Some(address),
    };
    let poller = RemotePoller::new(vec![target.clone()], transport);
    let source = poller.source();
    let selection = poller.selection();
    selection.select(target.registry_key.clone(), "proof");
    poller.start().unwrap();
    wait_for("quiet selected screen", Duration::from_secs(10), || {
        proof_session(source.as_ref())
            .is_some_and(|session| session_text(&session).contains("QUIET_PROOF"))
    });
    // Let the follow-up Sync on the quiet session start its long-poll.
    std::thread::sleep(Duration::from_millis(500));
    selection.select(target.registry_key.clone(), "other");
    let start = Instant::now();
    wait_for("switched screen", Duration::from_secs(30), || {
        source.snapshot().nodes.first().is_some_and(|node| {
            node.sessions.iter().any(|session| {
                session.name == "other" && session_text(session).contains("OTHER_SCREEN")
            })
        })
    });
    let waited = start.elapsed();
    // A 20s Sync hold made this take ~20s before the hold was bounded.
    assert!(waited < Duration::from_secs(5), "switch took {waited:?}");
}

#[test]
fn remote_lua_session_view_binds_the_display_to_the_exact_peer_session() {
    let client_node = Node::start("view-client");
    let server_node = Node::start("view-server");
    admit_pair(&client_node, &server_node);
    let listener = Listener::start(&server_node);
    server_node.start_named_session("proof", "sleep 30");
    let config = server_node.command().args(["-e", "remuda.session_list_decorate = function(s) return {layout='compact', agent_kind='codex', description='model · 42%'} end"]).output().unwrap();
    assert!(config.status.success(), "{config:?}");
    let client = ClusterClient::with_timeouts(
        Arc::new(SystemWallClock::new()),
        ClientTimeouts {
            connect: Duration::from_secs(2),
            read: Duration::from_secs(3),
            total: Duration::from_secs(3),
        },
    );
    let reply = client
        .request(
            listener.address,
            &server_node.public,
            &client_node.private,
            &Request::RenderedSessionList { width: 40 },
        )
        .unwrap();
    let Response::RenderedSessionList(view) = reply else {
        panic!("unexpected response {reply:?}");
    };
    let checked = view.validate_for_display(None, 40).unwrap();
    assert_eq!(checked.sessions().len(), 1);
    assert_eq!(checked.sessions()[0].name, "proof");
    assert!(checked.sessions()[0].instance_id.is_some());
    assert!(checked.entries()[0].lines[0].contains("codex"));
    assert!(checked.entries()[0].lines[0].contains("model · 42%"));
}

#[test]
fn remote_node_pane_shows_the_peer_lua_decorated_session_list() {
    let client_node = Node::start("view-ui-client");
    let server_node = Node::start("view-ui-server");
    admit_pair(&client_node, &server_node);
    let listener = Listener::start(&server_node);
    server_node.start_named_session("proof", "sleep 30");
    let config = server_node.command().args(["-e", "remuda.session_list_decorate = function(s) return {layout='compact', agent_kind='codex', description='MODELTEST'} end"]).output().unwrap();
    assert!(config.status.success(), "{config:?}");
    let path = client_node
        .state
        .join("remuda/cluster/authorized_nodes.json");
    let mut registry: Registry = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    registry
        .authorized_nodes
        .iter_mut()
        .find(|entry| entry.node_fp == server_node.fingerprint())
        .unwrap()
        .endpoint = Some(listener.address.to_string());
    fs::write(&path, serde_json::to_vec_pretty(&registry).unwrap()).unwrap();
    let tui = RemoteTui::start(&client_node, Some(&server_node.fingerprint()));
    tui.wait_for("proof", Duration::from_secs(10));
    tui.wait_for("MODELTEST", Duration::from_secs(10));
    assert!(tui.screen_text().contains("codex"));
}

fn pane_pair(label: &str) -> (Node, Node) {
    let client = Node::start(&format!("{label}-client"));
    let server = Node::start(&format!("{label}-server"));
    admit_pair(&client, &server);
    let listener = Listener::start(&server);
    let path = client.state.join("remuda/cluster/authorized_nodes.json");
    let mut registry: Registry = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    registry
        .authorized_nodes
        .iter_mut()
        .find(|entry| entry.node_fp == server.fingerprint())
        .unwrap()
        .endpoint = Some(listener.address.to_string());
    fs::write(&path, serde_json::to_vec_pretty(&registry).unwrap()).unwrap();
    (client, server)
}

fn local_session(server: &Node, name: &str) -> Response {
    remuda_native::client::request(
        &remuda_native::daemon::socket_path_in(&server.runtime, &server.name),
        &Request::New {
            name: Some(name.into()),
            command: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
            size: remuda_core::Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .unwrap()
}

#[test]
fn node_pane_confirmation_does_not_kill_a_recreated_session() {
    let (client, server) = pane_pair("view-recreate");
    assert!(matches!(
        local_session(&server, "proof"),
        Response::Value(_)
    ));
    let mut tui = RemoteTui::start(&client, Some(&server.fingerprint()));
    tui.wait_for("proof", Duration::from_secs(10));
    tui.writer.write_all(b"x").unwrap();
    tui.wait_for("kill proof?", Duration::from_secs(5));
    let socket = remuda_native::daemon::socket_path_in(&server.runtime, &server.name);
    let response = remuda_native::client::request(
        &socket,
        &Request::Close {
            name: "proof".into(),
            instance_id: None,
            confirm: None,
        },
    )
    .unwrap();
    assert_eq!(response, Response::Ok);
    assert!(matches!(
        local_session(&server, "proof"),
        Response::Value(_)
    ));
    std::thread::sleep(Duration::from_millis(700));
    tui.writer.write_all(b"y").unwrap();
    std::thread::sleep(Duration::from_millis(700));
    let Response::Sessions(rows) = remuda_native::client::request(&socket, &Request::List).unwrap()
    else {
        panic!("list failed");
    };
    assert!(
        rows.iter().any(|row| row.name == "proof" && row.alive),
        "new session must remain alive"
    );
}

#[test]
fn node_pane_confirmation_never_emits_peer_terminal_commands() {
    let (client, server) = pane_pair("view-escape");
    let name = "proof\x1b]0;PWNED\x07";
    assert!(matches!(local_session(&server, name), Response::Value(_)));
    let mut tui = RemoteTui::start(&client, Some(&server.fingerprint()));
    tui.wait_for("proof", Duration::from_secs(10));
    let offset = tui.output_len();
    tui.writer.write_all(b"x").unwrap();
    tui.wait_for("kill \"proof", Duration::from_secs(5));
    assert!(
        !tui.raw_text_from(offset).contains("\x1b]0;PWNED\x07"),
        "peer name must not emit terminal command"
    );
}

#[test]
fn node_pane_keeps_selected_instance_when_decorated_view_falls_back() {
    let (client, server) = pane_pair("view-order");
    assert!(matches!(local_session(&server, "aa"), Response::Value(_)));
    assert!(matches!(local_session(&server, "zz"), Response::Value(_)));
    let config = server.command().args(["-e", "remuda.session_order=function() return {{name='zz'},{name='aa'}} end; remuda.session_list_decorate=function() return {layout='compact', description='MODELTEST'} end"]).output().unwrap();
    assert!(config.status.success());
    let mut tui = RemoteTui::start(&client, Some(&server.fingerprint()));
    tui.wait_for("MODELTEST", Duration::from_secs(10));
    let disable = server
        .command()
        .args([
            "-e",
            "remuda._render_sessions_snapshot=function() error('unsupported') end",
        ])
        .output()
        .unwrap();
    assert!(disable.status.success());
    wait_for("view fallback", Duration::from_secs(8), || {
        !tui.screen_text().contains("MODELTEST")
    });
    tui.writer.write_all(b"x").unwrap();
    tui.wait_for("kill aa?", Duration::from_secs(4));
}

#[test]
fn node_pane_fallback_keeps_spaced_and_combining_names_actionable() {
    for (tag, name, label) in [
        ("spaces", " leading ", "\" leading \""),
        ("combining", "e\u{301}", "\"e\\u{301}\""),
    ] {
        let (client, server) = pane_pair(tag);
        assert!(matches!(local_session(&server, name), Response::Value(_)));
        let mut tui = RemoteTui::start(&client, Some(&server.fingerprint()));
        tui.wait_for(label, Duration::from_secs(10));
        tui.writer.write_all(b"x").unwrap();
        tui.wait_for(&format!("kill {label}?"), Duration::from_secs(5));
        tui.writer.write_all(b"y").unwrap();
        let socket = remuda_native::daemon::socket_path_in(&server.runtime, &server.name);
        wait_for(
            "closed exact instance",
            Duration::from_secs(5),
            || matches!(remuda_native::client::request(&socket, &Request::List), Ok(Response::Sessions(rows)) if !rows.iter().any(|row| row.name == name && row.alive)),
        );
    }
}

#[test]
fn node_pane_refuses_actions_when_distinct_names_have_the_same_fallback_label() {
    let (client, server) = pane_pair("view-label-collision");
    let name_control = "proof\x1b";
    let name_literal = "\"proof\\u{1b}\"";
    assert!(matches!(
        local_session(&server, name_control),
        Response::Value(_)
    ));
    assert!(matches!(
        local_session(&server, name_literal),
        Response::Value(_)
    ));
    let mut tui = RemoteTui::start(&client, Some(&server.fingerprint()));
    tui.wait_for("proof\\u{1b}", Duration::from_secs(10));
    tui.writer.write_all(b"x").unwrap();
    tui.wait_for("kill", Duration::from_secs(3));
    tui.writer.write_all(b"y").unwrap();
    std::thread::sleep(Duration::from_millis(700));
    let socket = remuda_native::daemon::socket_path_in(&server.runtime, &server.name);
    let Response::Sessions(rows) = remuda_native::client::request(&socket, &Request::List).unwrap()
    else {
        panic!("list failed");
    };
    assert!(rows.iter().any(|row| row.name == name_control && row.alive));
    assert!(rows.iter().any(|row| row.name == name_literal && row.alive));
}

#[test]
fn malicious_duplicate_names_never_close_or_focus_an_ambiguous_instance() {
    use std::io::BufRead;
    use std::sync::atomic::AtomicBool;
    let client = Node::start("duplicate-client");
    let peer = Node::start("duplicate-peer");
    admit_pair(&client, &peer);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let path = client.state.join("remuda/cluster/authorized_nodes.json");
    let mut registry: Registry = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    registry
        .authorized_nodes
        .iter_mut()
        .find(|entry| entry.node_fp == peer.fingerprint())
        .unwrap()
        .endpoint = Some(address.to_string());
    fs::write(&path, serde_json::to_vec_pretty(&registry).unwrap()).unwrap();
    let observed = Arc::new(std::sync::Mutex::new(Vec::<Request>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let peer_private = peer.private.clone();
    let log = observed.clone();
    let done = stop.clone();
    let worker = std::thread::spawn(move || {
        while !done.load(Ordering::SeqCst) {
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(_) => break,
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let mut length = None;
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<usize>().ok();
                }
            }
            let Some(length) = length.filter(|n| *n <= 65535) else {
                continue;
            };
            let mut body = vec![0; length];
            if reader.read_exact(&mut body).is_err() {
                continue;
            }
            let Ok(opened) = remuda_native::net::frame::open_request(&peer_private, &body) else {
                continue;
            };
            let Ok(request) = serde_json::from_slice::<Request>(&opened.payload) else {
                continue;
            };
            log.lock().unwrap().push(request.clone());
            let row = |instance: &str| remuda_core::SessionSummary {
                id: instance.into(),
                name: "proof".into(),
                instance_id: Some(instance.into()),
                output_version: None,
                alive: true,
                idle: Duration::ZERO,
                output_idle: None,
                size: remuda_core::Size::new(80, 24),
                attached: false,
                human_idle: None,
                mouse_tracking: false,
                unavailable: false,
                program_status: None,
                program_status_age_ms: None,
                remote_input_disabled: false,
            };
            let response = match request {
                Request::List => Response::Sessions(vec![row("first"), row("second")]),
                Request::CaptureStyled { .. } => Response::StyledScreen {
                    rows: vec![],
                    instance_id: Some("first".into()),
                    output_version: Some(1),
                    wrapped: vec![],
                    scrollback_len: 0,
                    scrollback_total: 0,
                    cursor: remuda_core::agent::Cursor {
                        row: 0,
                        col: 0,
                        visible: false,
                    },
                },
                Request::RenderedSessionList { .. } => Response::Error("unsupported".into()),
                _ => Response::Ok,
            };
            let body = remuda_native::net::frame::seal_response(
                opened,
                &serde_json::to_vec(&response).unwrap(),
            )
            .unwrap();
            let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    let mut tui = RemoteTui::start(&client, Some(&peer.fingerprint()));
    tui.wait_for("proof", Duration::from_secs(8));
    tui.writer.write_all(b"\x1b[Bx").unwrap();
    tui.wait_for("duplicate session names", Duration::from_secs(4));
    tui.writer.write_all(b"y").unwrap();
    tui.writer.write_all(b"mhello\r\rtyped").unwrap();
    std::thread::sleep(Duration::from_millis(700));
    let requests = observed.lock().unwrap().clone();
    stop.store(true, Ordering::SeqCst);
    worker.join().unwrap();
    assert!(
        !requests.iter().any(|request| matches!(
            request,
            Request::Close { .. } | Request::Input { .. } | Request::InputLine { .. }
        )),
        "ambiguous target was acted on: {requests:?}"
    );
    assert!(
        !requests
            .iter()
            .any(|request| matches!(request, Request::CaptureStyled { .. })),
        "ambiguous preview must not be requested: {requests:?}"
    );
}

#[test]
fn node_pane_distinguishes_ascii_space_from_nonbreaking_space() {
    let (client, server) = pane_pair("view-nbsp");
    assert!(matches!(local_session(&server, "a b"), Response::Value(_)));
    assert!(matches!(
        local_session(&server, "a\u{a0}b"),
        Response::Value(_)
    ));
    let tui = RemoteTui::start(&client, Some(&server.fingerprint()));
    tui.wait_for("a b", Duration::from_secs(10));
    tui.wait_for("\\u{a0}", Duration::from_secs(10));
}

#[test]
fn node_pane_live_keys_reach_the_displayed_exact_instance() {
    let (client, server) = pane_pair("view-live-keys");
    server.start_named_session(
        "proof",
        "while IFS= read -r line; do printf 'RECV:%s\\n' \"$line\"; done",
    );
    let mut tui = RemoteTui::start(&client, Some(&server.fingerprint()));
    tui.wait_for("proof", Duration::from_secs(10));
    std::thread::sleep(Duration::from_millis(500));
    tui.writer.write_all(b"\r").unwrap();
    tui.wait_for("every key goes", Duration::from_secs(5));
    tui.writer.write_all(b"hello\r").unwrap();
    tui.wait_for("RECV:hello", Duration::from_secs(8));
}

#[test]
fn node_pane_default_hosted_remote_keys_survive_reload() {
    let (client, server) = pane_pair("hc");
    server.start_named_session("proof", "stty -echo; printf 'HOST_PID:%s\\nHOST_READY' \"$$\"; while IFS= read -r line; do printf 'HOST_RECEIVED:%s\\n' \"$line\"; done");
    let socket = remuda_native::daemon::socket_path_in(&server.runtime, &server.name);
    let instance = match remuda_native::client::request(&socket, &Request::List).unwrap() {
        Response::Sessions(rows) => rows
            .into_iter()
            .find(|row| row.name == "proof")
            .unwrap()
            .instance_id
            .unwrap(),
        other => panic!("unexpected list: {other:?}"),
    };
    {
        let mut tui = RemoteTui::start(&client, Some(&server.fingerprint()));
        tui.wait_for("HOST_READY", Duration::from_secs(10));
        tui.writer.write_all(b"\r").unwrap();
        tui.wait_for("every key goes", Duration::from_secs(5));
        tui.writer.write_all(b"before-reload\r").unwrap();
        tui.wait_for("HOST_RECEIVED:before-reload", Duration::from_secs(10));
    }
    let pid = || {
        let Response::Screen(text) = remuda_native::client::request(
            &socket,
            &Request::Capture {
                name: "proof".into(),
            },
        )
        .unwrap() else {
            panic!("capture failed")
        };
        text.lines()
            .find_map(|line| line.strip_prefix("HOST_PID:"))
            .expect("PID marker")
            .trim()
            .parse::<u32>()
            .unwrap()
    };
    let original_pid = pid();
    let reload = server.command().args(["reload"]).output().unwrap();
    assert!(
        reload.status.success(),
        "reload: {} {}",
        String::from_utf8_lossy(&reload.stdout),
        String::from_utf8_lossy(&reload.stderr)
    );
    let after = match remuda_native::client::request(&socket, &Request::List).unwrap() {
        Response::Sessions(rows) => rows
            .into_iter()
            .find(|row| row.name == "proof")
            .unwrap()
            .instance_id
            .unwrap(),
        other => panic!("unexpected list after reload: {other:?}"),
    };
    assert_eq!(after, instance);
    assert_eq!(
        pid(),
        original_pid,
        "reload must keep the same child process"
    );
    let mut tui = RemoteTui::start(&client, Some(&server.fingerprint()));
    tui.wait_for("HOST_RECEIVED:before-reload", Duration::from_secs(10));
    tui.writer.write_all(b"\r").unwrap();
    tui.wait_for("every key goes", Duration::from_secs(5));
    tui.writer.write_all(b"after-reload\r").unwrap();
    tui.wait_for("HOST_RECEIVED:after-reload", Duration::from_secs(10));
}

#[test]
fn default_hosted_launch_remote_opt_out_survives_reload() {
    let (client, server) = pane_pair("ho");
    let socket = remuda_native::daemon::socket_path_in(&server.runtime, &server.name);
    let response = remuda_native::client::request(
        &socket,
        &Request::NewWithInstance {
            name: Some("proof".into()),
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "stty -echo; while IFS= read -r line; do printf 'RECV:%s\\n' \"$line\"; done"
                    .into(),
            ],
            size: remuda_core::Size::new(80, 24),
            cwd: None,
            env: None,
            remote_input_disabled: true,
        },
    )
    .unwrap();
    let instance = match response {
        Response::SessionStarted { instance_id, .. } => instance_id,
        other => panic!("failed to create protected session: {other:?}"),
    };
    let registry: Registry = serde_json::from_slice(
        &fs::read(client.state.join("remuda/cluster/authorized_nodes.json")).unwrap(),
    )
    .unwrap();
    let address = registry
        .authorized_nodes
        .iter()
        .find(|entry| entry.node_fp == server.fingerprint())
        .and_then(|entry| entry.endpoint.as_ref())
        .unwrap()
        .parse()
        .unwrap();
    let peer = ClusterClient::system();
    let request = || Request::Input {
        name: "proof".into(),
        instance_id: instance.clone(),
        client_id: "2".repeat(32),
        seq: 1,
        bytes: b"do-not-send\r".to_vec(),
    };
    assert_eq!(
        peer.request(address, &server.public, &client.private, &request())
            .unwrap(),
        Response::RemoteControlDisabled
    );
    let reload = server.command().args(["reload"]).output().unwrap();
    assert!(
        reload.status.success(),
        "reload: {} {}",
        String::from_utf8_lossy(&reload.stdout),
        String::from_utf8_lossy(&reload.stderr)
    );
    assert_eq!(
        peer.request(address, &server.public, &client.private, &request())
            .unwrap(),
        Response::RemoteControlDisabled
    );
    let screen = remuda_native::client::request(
        &socket,
        &Request::Capture {
            name: "proof".into(),
        },
    )
    .unwrap();
    assert!(matches!(screen, Response::Screen(text) if !text.contains("do-not-send")));
}

#[test]
fn hosted_remote_pane_multiline_paste_reaches_session() {
    let (client, server) = pane_pair("host-paste");
    server.start_named_session("proof", "stty -echo; printf 'PASTE_READY\\n'; while IFS= read -r line; do printf 'PASTE_RECEIVED:%s\\n' \"$line\"; done");
    let mut tui = RemoteTui::start(&client, Some(&server.fingerprint()));
    tui.wait_for("PASTE_READY", Duration::from_secs(10));
    tui.writer.write_all(b"\r").unwrap();
    tui.wait_for("every key goes", Duration::from_secs(5));
    let at = tui.output_len();
    tui.writer
        .write_all(b"\x1b[200~first-line\nsecond-line\n\x1b[201~")
        .unwrap();
    tui.wait_for_from(at, "PASTE_RECEIVED:first-line", Duration::from_secs(10));
    tui.wait_for_from(at, "PASTE_RECEIVED:second-line", Duration::from_secs(10));
}
