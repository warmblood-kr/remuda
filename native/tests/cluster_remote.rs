#![cfg(unix)]
#![allow(clippy::disallowed_types)]

use remuda_core::protocol::Request;
use remuda_native::cluster::{encoding, AuthorizedNode, NodeState, Registry, ResolvedTarget};
use remuda_native::cluster_remote::{
    ClusterRemoteTransport, RemotePoller, RemoteSource, RemoteState, RemoteTarget,
};
use remuda_native::net::cluster_client::{ClientTimeouts, ClusterClient};
use remuda_native::SystemWallClock;
use std::fs;
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
        let init = command(&name, &runtime, &root, &state, &home)
            .args(["cluster", "init"])
            .output()
            .unwrap();
        assert!(init.status.success(), "cluster init failed: {init:?}");
        let key = fs::read(state.join("remuda/cluster/identity.key")).unwrap();
        let daemon = spawn_daemon(&name, &runtime, &root, &state);
        let mut node = Self {
            root,
            runtime,
            state,
            name,
            public: key[32..].to_vec(),
            private: key[..32].to_vec(),
            daemon,
        };
        node.wait_ready();
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
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", state);
    command
}

struct Listener {
    child: Option<Child>,
    address: SocketAddr,
}

impl Listener {
    fn start(node: &Node) -> Self {
        let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reservation.local_addr().unwrap();
        drop(reservation);
        let mut listener = Self {
            child: None,
            address,
        };
        listener.restart(node);
        listener
    }

    fn restart(&mut self, node: &Node) {
        assert!(self.child.is_none());
        let address = self.address.to_string();
        let child = node
            .command()
            .args(["cluster", "listen", "--bind", &address])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.child = Some(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match TcpStream::connect(self.address) {
                Ok(stream) => {
                    drop(stream);
                    return;
                }
                Err(_) => {
                    if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                        panic!("listener exited before ready: {status}");
                    }
                    assert!(Instant::now() < deadline, "listener did not bind");
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop();
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

fn snapshot_text(source: &dyn RemoteSource) -> Option<String> {
    source
        .snapshot()
        .nodes
        .first()?
        .sessions
        .first()?
        .screen
        .as_ref()
        .map(|screen| {
            screen
                .cells
                .iter()
                .flat_map(|row| row.iter().map(|cell| cell.text.as_str()))
                .collect::<String>()
        })
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

fn wait_for(timeout: Duration, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "remote poll condition did not settle"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn selected_remote_sync_keeps_last_screen_offline_and_resumes_after_listener_restart() {
    const BEFORE_DOWN: &str = "REMOTE_BEFORE_DOWN";
    const AFTER_RECONNECT: &str = "REMOTE_AFTER_RECONNECT";

    let client_node = Node::start("remote-client");
    let mut server_node = Node::start("remote-server");
    admit_pair(&client_node, &server_node);
    let mut listener = Listener::start(&server_node);
    let marker_before_down = server_node.root.join("emit-before-down");
    let marker_after_reconnect = server_node.root.join("emit-after-reconnect");
    let shell = format!(
        "printf REMOTE_START; while [ ! -e '{}' ]; do sleep 0.02; done; printf {BEFORE_DOWN}; while [ ! -e '{}' ]; do sleep 0.02; done; printf {AFTER_RECONNECT}; sleep 30",
        marker_before_down.display(),
        marker_after_reconnect.display(),
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
    let poller = RemotePoller::new(vec![target], transport);
    let source = poller.source();
    let selection = poller.selection();
    selection.select(label, "proof");
    poller.start().unwrap();

    wait_for(Duration::from_secs(10), || {
        snapshot_text(source.as_ref()).is_some_and(|text| text.contains("REMOTE_START"))
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
    wait_for(Duration::from_secs(10), || {
        snapshot_text(source.as_ref()).is_some_and(|text| text.contains(BEFORE_DOWN))
    });

    listener.stop();
    wait_for(Duration::from_secs(12), || {
        source
            .snapshot()
            .nodes
            .first()
            .is_some_and(|node| node.state == RemoteState::Unreachable)
    });
    assert!(snapshot_text(source.as_ref()).is_some_and(|text| text.contains(BEFORE_DOWN)));

    listener.restart(&server_node);
    let marker = marker_after_reconnect.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(3));
        let _ = fs::write(marker, b"go");
    });
    wait_for(Duration::from_secs(20), || {
        let snapshot = source.snapshot();
        snapshot.nodes.first().is_some_and(|node| {
            node.state == RemoteState::Reachable
                && snapshot_text(source.as_ref()).is_some_and(|text| text.contains(AFTER_RECONNECT))
        })
    });

    let old_instance = proof_session(source.as_ref()).unwrap().instance_id;
    server_node.restart_daemon();
    wait_for(Duration::from_secs(10), || {
        proof_session(source.as_ref()).is_none()
    });
    server_node.start_session("printf REMOTE_NEW_INSTANCE; sleep 30");
    wait_for(Duration::from_secs(15), || {
        proof_session(source.as_ref()).is_some_and(|session| {
            session.instance_id != old_instance
                && session.alive
                && session_text(&session).contains("REMOTE_NEW_INSTANCE")
                && !session_text(&session).contains("REMOTE_BEFORE_DOWN")
                && !session_text(&session).contains("REMOTE_AFTER_RECONNECT")
        })
    });
}
