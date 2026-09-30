#![cfg(unix)]
#![allow(clippy::disallowed_types)]

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
        let root = std::env::temp_dir().join(format!("rp-{}-{serial}", std::process::id()));
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
        let key = fs::read(node.state.join("remuda/cluster/identity.key")).unwrap();
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
        self.stop_daemon();
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
    output: Arc<std::sync::Mutex<Vec<u8>>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl RemoteTui {
    fn start(node: &Node, target: Option<&str>) -> Self {
        let pty = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 100,
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
            output,
            reader: Some(reader),
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    fn output_len(&self) -> usize {
        self.output.lock().unwrap().len()
    }

    fn text_from(&self, offset: usize) -> String {
        let output = self.output.lock().unwrap();
        String::from_utf8_lossy(&output[offset.min(output.len())..]).into_owned()
    }

    fn wait_for(&self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let text = self.text();
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
            let text = self.text_from(offset);
            if text.contains(needle) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "TUI did not render {needle:?} after input: {text}"
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
    tui.wait_for_from(
        output_start,
        "Remote session is read-only · q detach",
        Duration::from_secs(3),
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
