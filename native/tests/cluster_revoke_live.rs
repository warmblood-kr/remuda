//! Live replication and revocation coverage across isolated cluster daemons.

#![cfg(unix)]
// This integration test deliberately exercises the real loopback cluster wire.
#![allow(clippy::disallowed_types)]

use remuda_core::protocol::{Request, Response};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use zeroize::Zeroizing;

static NEXT_NODE_ID: AtomicUsize = AtomicUsize::new(0);
static LIVE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn live_test_guard() -> std::sync::MutexGuard<'static, ()> {
    LIVE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct PrivateNode {
    child: Child,
    pid: u32,
    root: PathBuf,
    runtime: PathBuf,
    name: String,
    anti_entropy_interval_ms: Option<u64>,
}

impl PrivateNode {
    fn start(_label: &str) -> Self {
        Self::start_with_anti_entropy(_label, None)
    }

    fn start_with_anti_entropy(_label: &str, anti_entropy_interval_ms: Option<u64>) -> Self {
        let id = NEXT_NODE_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("cr{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let runtime = root.join("runtime");
        let home = root.join("home");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let name = format!("c{}-{id}", std::process::id());
        let child = spawn_daemon(&name, &runtime, &root, anti_entropy_interval_ms);
        let pid = child.id();
        let mut node = Self {
            child,
            pid,
            root,
            runtime,
            name,
            anti_entropy_interval_ms,
        };
        node.wait_ready();
        node
    }

    fn daemon_path(&self) -> PathBuf {
        remuda_native::daemon::socket_path_in(&self.runtime, &self.name)
    }

    fn restart_daemon(&mut self) {
        self.stop_daemon();
        self.child = spawn_daemon(
            &self.name,
            &self.runtime,
            &self.root,
            self.anti_entropy_interval_ms,
        );
        self.pid = self.child.id();
        self.wait_ready();
    }

    fn stop_daemon(&mut self) {
        assert!(self.runtime.starts_with(&self.root));
        assert_eq!(
            self.child.id(),
            self.pid,
            "stop only the recorded daemon PID"
        );
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn run(&self, args: &[&str]) -> Output {
        let root = &self.root;
        Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", &self.name])
            .args(args)
            .env("REMUDA_RUNTIME_DIR", &self.runtime)
            .env("HOME", root.join("home"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn wait_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while remuda_native::client::request(&self.daemon_path(), &Request::List).is_err() {
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!("private daemon exited before binding: {status}");
            }
            assert!(Instant::now() < deadline, "private daemon did not start");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn identity(&self) -> Zeroizing<Vec<u8>> {
        Zeroizing::new(std::fs::read(self.root.join("state/remuda/cluster/identity.key")).unwrap())
    }
}

fn spawn_daemon(
    name: &str,
    runtime: &Path,
    root: &Path,
    anti_entropy_interval_ms: Option<u64>,
) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_remuda"));
    command
        .args(["-s", name, "daemon"])
        .env("REMUDA_RUNTIME_DIR", runtime)
        .env("HOME", root.join("home"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env_remove("REMUDA_TEST_CLUSTER_SYNC_INTERVAL_MS")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    if let Some(interval) = anti_entropy_interval_ms {
        command.env("REMUDA_TEST_CLUSTER_SYNC_INTERVAL_MS", interval.to_string());
    }
    command.spawn().unwrap()
}

impl Drop for PrivateNode {
    fn drop(&mut self) {
        assert!(self.runtime.starts_with(&self.root));
        assert_eq!(
            self.child.id(),
            self.pid,
            "drop only the recorded daemon PID"
        );
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct ListenerProcess {
    child: Child,
    pid: u32,
    address: SocketAddr,
    root: PathBuf,
    runtime: PathBuf,
}

impl ListenerProcess {
    fn start(node: &PrivateNode) -> Self {
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = probe.local_addr().unwrap();
        drop(probe);
        let address_text = address.to_string();
        let child = Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args([
                "-s",
                &node.name,
                "cluster",
                "listen",
                "--bind",
                &address_text,
            ])
            .env("REMUDA_RUNTIME_DIR", &node.runtime)
            .env("HOME", node.root.join("home"))
            .env("XDG_STATE_HOME", node.root.join("state"))
            .env("XDG_CONFIG_HOME", node.root.join("config"))
            .env("XDG_DATA_HOME", node.root.join("data"))
            .env("XDG_CACHE_HOME", node.root.join("cache"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let pid = child.id();
        let mut listener = Self {
            child,
            pid,
            address,
            root: node.root.clone(),
            runtime: node.runtime.clone(),
        };
        listener.wait_ready();
        listener
    }

    fn wait_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!("cluster listener exited before binding: {status}");
            }
            if TcpStream::connect_timeout(&self.address, Duration::from_millis(50)).is_ok() {
                return;
            }
            assert!(Instant::now() < deadline, "cluster listener did not start");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn stop(&mut self) {
        assert!(self.runtime.starts_with(&self.root));
        assert_eq!(
            self.child.id(),
            self.pid,
            "stop only the recorded listener PID"
        );
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ListenerProcess {
    fn drop(&mut self) {
        assert!(self.runtime.starts_with(&self.root));
        assert!(self.pid > 0);
        assert_eq!(
            self.child.id(),
            self.pid,
            "drop only the recorded listener PID"
        );
        self.stop();
    }
}

struct BlackholePeer {
    address: SocketAddr,
    stop: std::sync::Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl BlackholePeer {
    fn start(address: SocketAddr) -> Self {
        let listener = TcpListener::bind(address).expect("bind stalled peer endpoint");
        listener.set_nonblocking(true).unwrap();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_millis(50)))
                            .unwrap();
                        let mut buffer = [0; 4096];
                        while !thread_stop.load(Ordering::Acquire) {
                            match stream.read(&mut buffer) {
                                Ok(0) => thread::sleep(Duration::from_millis(50)),
                                Ok(_) => {}
                                Err(error)
                                    if matches!(
                                        error.kind(),
                                        std::io::ErrorKind::WouldBlock
                                            | std::io::ErrorKind::TimedOut
                                    ) => {}
                                Err(_) => break,
                            }
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept stalled peer connection: {error}"),
                }
            }
        });
        Self {
            address,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for BlackholePeer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_millis(100));
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .expect("stalled peer thread should stop cleanly");
        }
    }
}

fn successful(output: Output, operation: &str) -> String {
    assert!(
        output.status.success(),
        "{operation} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn invite_command_args(output: &str) -> (&str, &str) {
    let command = output
        .lines()
        .find(|line| line.starts_with("  remuda cluster join "))
        .expect("printed join command")
        .trim();
    let command = command
        .strip_prefix("remuda cluster join '")
        .and_then(|command| command.strip_suffix('\''))
        .expect("shell-quoted join command");
    let (fingerprint, line) = command
        .split_once("' '")
        .expect("separate fingerprint and join line");
    assert!(!fingerprint.contains('\''));
    assert!(!line.contains('\''));
    (fingerprint, line)
}

fn identity_fingerprint(material: &[u8]) -> String {
    remuda_native::cluster::encoding::fingerprint(&material[32..])
}

fn authorized_node(
    material: &[u8],
    by: &str,
    endpoint: Option<&str>,
) -> remuda_native::cluster::AuthorizedNode {
    let public = &material[32..];
    let node_fp = remuda_native::cluster::encoding::fingerprint(public);
    remuda_native::cluster::AuthorizedNode {
        node_fp,
        static_pubkey: remuda_native::cluster::encoding::encode_base64(public),
        endpoint: endpoint.map(str::to_owned),
        delivered_by: None,
        format_major: 1,
        format_minor: 0,
        optional_fields: std::collections::BTreeMap::new(),
        state: remuda_native::cluster::NodeState::Admitted,
        version: 1,
        by: by.to_owned(),
    }
}

fn write_registry(node: &PrivateNode, registry: &remuda_native::cluster::Registry) {
    let path = node.root.join("state/remuda/cluster/authorized_nodes.json");
    std::fs::write(path, serde_json::to_vec(registry).unwrap()).unwrap();
}

fn read_registry(node: &PrivateNode) -> remuda_native::cluster::Registry {
    serde_json::from_slice(
        &std::fs::read(node.root.join("state/remuda/cluster/authorized_nodes.json")).unwrap(),
    )
    .unwrap()
}

fn list_from(peer_material: &[u8], server_material: &[u8], address: SocketAddr) -> Response {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let sealed = remuda_native::net::frame::seal_request(
        &peer_material[..32],
        &server_material[32..],
        now,
        &serde_json::to_vec(&Request::List).unwrap(),
    )
    .unwrap();
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    write!(
        stream,
        "POST /cluster HTTP/1.1\r\nHost: {address}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        sealed.message.len()
    )
    .unwrap();
    stream.write_all(&sealed.message).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let split = response
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .expect("HTTP response headers");
    let status = std::str::from_utf8(&response[..split]).unwrap();
    if status.lines().next().unwrap().contains(" 403 ") {
        return Response::error(String::from_utf8_lossy(&response[split + 4..]));
    }
    assert!(status.lines().next().unwrap().contains(" 200 "), "{status}");
    let payload = remuda_native::net::frame::open_response(sealed, &response[split + 4..])
        .expect("encrypted cluster response");
    serde_json::from_slice(&payload).unwrap()
}

fn assert_list_refused(peer: &[u8], server: &[u8], address: SocketAddr, attempt: &str) {
    match list_from(peer, server, address) {
        Response::Error(message) => assert!(
            message.contains("not admitted") || message.contains("revoked"),
            "{attempt} returned an unexpected refusal: {message}"
        ),
        response => panic!("{attempt} unexpectedly succeeded: {response:?}"),
    }
}

// Registry replication uses bounded one-shot pages; listener tests cover the
// authorization recheck for any held response.
fn prepare_local_rotation_state(node: &PrivateNode) -> (PathBuf, PathBuf, Vec<u8>) {
    successful(
        node.run(&["cluster", "control", "off"]),
        "disable remote control before identity rotation",
    );
    successful(
        node.run(&["cluster", "invite", "--bind", "127.0.0.1:9443"]),
        "mint a join token before identity rotation",
    );
    let cluster_dir = node.root.join("state/remuda/cluster");
    let token_path = cluster_dir.join("join_tokens.json");
    let settings_path = cluster_dir.join("settings.json");
    assert!(token_path.exists(), "invite did not persist a join token");
    let settings = std::fs::read(&settings_path).unwrap();
    (token_path, settings_path, settings)
}

fn assert_new_identity_rotation(
    node: &PrivateNode,
    old_fingerprint: &str,
    token_path: &Path,
    settings_path: &Path,
    settings_before_rotation: &[u8],
) {
    let no_yes = node.run(&["cluster", "init", "--new-identity"]);
    assert!(
        !no_yes.status.success(),
        "non-TTY identity rotation was not refused without --yes: {no_yes:?}"
    );
    assert!(
        String::from_utf8_lossy(&no_yes.stderr).contains("use --yes"),
        "non-TTY refusal did not explain --yes: {}",
        String::from_utf8_lossy(&no_yes.stderr)
    );
    let rotation_output = node.run(&["cluster", "init", "--new-identity", "--yes"]);
    let warning = String::from_utf8_lossy(&rotation_output.stderr).into_owned();
    successful(rotation_output, "rotate revoked node identity");
    assert!(
        warning.contains(
            "This creates a new identity; this machine leaves its current cluster and needs a new invite."
        ),
        "rotation did not warn that it leaves the current cluster: {warning}"
    );
    let rotated_identity = node.identity();
    let new_fingerprint = identity_fingerprint(&rotated_identity);
    assert_ne!(new_fingerprint, old_fingerprint);
    assert!(!token_path.exists(), "old join token state was not cleared");
    assert_eq!(
        std::fs::read(settings_path).unwrap(),
        settings_before_rotation,
        "identity rotation changed local settings"
    );
    let cluster_dir = node.root.join("state/remuda/cluster");
    assert!(
        !cluster_dir.join("revoked_notice.json").exists(),
        "new identity did not clear the revocation notice"
    );
    let registry = read_registry(node);
    assert_eq!(registry.authorized_nodes.len(), 1);
    assert_eq!(registry.authorized_nodes[0].node_fp, new_fingerprint);
}

// Revocation behavior across all daemons remains pinned by this test name.
#[test]
fn revoking_a_live_member_is_seen_by_all_other_daemons() {
    let _serial = live_test_guard();
    let a = PrivateNode::start("a");
    let b = PrivateNode::start("b");
    let c = PrivateNode::start("c");
    successful(a.run(&["cluster", "init"]), "initialize node A");
    successful(b.run(&["cluster", "init"]), "initialize node B");
    successful(c.run(&["cluster", "init"]), "initialize node C");
    let a_identity = a.identity();
    let b_identity = b.identity();
    let b_fingerprint = identity_fingerprint(&b_identity);

    let listener = ListenerProcess::start(&a);
    let b_listener = ListenerProcess::start(&b);
    let c_listener = ListenerProcess::start(&c);
    join_member(&a, &b, &a_identity, listener.address, b_listener.address);
    assert_joiner_stores_issuer(&b, &a_identity, listener.address);
    join_member(&a, &c, &a_identity, listener.address, c_listener.address);
    wait_for_member_and_probe(
        &c,
        &b_fingerprint,
        &b_identity,
        &c.identity(),
        c_listener.address,
    );

    assert!(matches!(
        list_from(&b_identity, &a_identity, listener.address),
        Response::Sessions(_)
    ));
    let convergence_deadline = Instant::now() + Duration::from_secs(30);
    successful(
        a.run(&["cluster", "revoke", &b_fingerprint, "--yes"]),
        "revoke node B on node A",
    );

    assert_list_refused(
        &b_identity,
        &a_identity,
        listener.address,
        "node B's next List request to A",
    );
    // Every remote frame uses a fresh TCP connection. Repeat to make sure a
    // reconnect is refused immediately too, rather than relying on old state.
    assert_list_refused(
        &b_identity,
        &a_identity,
        listener.address,
        "node B's new connection to A",
    );

    wait_for_revocation_on_until(&c, &b_fingerprint, convergence_deadline);
    let b_notice = wait_for_revoked_notice_on_until(&b, &b_fingerprint, convergence_deadline);
    assert!(
        b_notice.contains(
            "Next: run `remuda cluster init --new-identity`, then ask an admitted machine for a new invite"
        ),
        "missing recovery guidance: {b_notice}"
    );
    let self_row = b_notice
        .lines()
        .find(|line| line.contains(&b_fingerprint))
        .expect("node B's own registry row");
    assert!(
        self_row.contains("admitted"),
        "the receiver's local registry entry changed: {self_row}"
    );
    let b_status = successful(b.run(&["cluster"]), "read revoked node B status");
    assert!(
        b_status.contains("This node was revoked by"),
        "cluster status did not show B's revocation notice: {b_status}"
    );
    successful(b.run(&["cluster", "init"]), "locally initialize node B");
    let b_nodes_after_init = successful(
        b.run(&["cluster", "nodes"]),
        "read node B's nodes table after ordinary init",
    );
    assert!(
        b_nodes_after_init.contains("This node was revoked by"),
        "ordinary init cleared B's notice: {b_nodes_after_init}"
    );
    assert_list_refused(
        &b_identity,
        &c.identity(),
        c_listener.address,
        "node B's request to C after revocation",
    );

    let table = successful(a.run(&["cluster", "nodes"]), "read node A's nodes table");
    assert!(table.contains(&b_fingerprint), "{table}");
    let row = table
        .lines()
        .find(|line| line.contains(&b_fingerprint))
        .expect("node B row in nodes table");
    assert!(row.contains("revoked"), "node B row is not revoked: {row}");
}

#[test]
fn new_identity_init_changes_fingerprint_resets_registry_clears_notice_and_old_join_tokens() {
    let _serial = live_test_guard();
    let node = PrivateNode::start("identity-rotation-node");
    let peer = PrivateNode::start("identity-rotation-peer");
    successful(node.run(&["cluster", "init"]), "initialize rotation node");
    successful(peer.run(&["cluster", "init"]), "initialize registry peer");
    let old_identity = node.identity();
    let old_fingerprint = identity_fingerprint(&old_identity);
    let mut registry = read_registry(&node);
    let peer_identity = peer.identity();
    registry
        .authorized_nodes
        .push(authorized_node(&peer_identity, &old_fingerprint, None));
    write_registry(&node, &registry);
    let cluster_dir = node.root.join("state/remuda/cluster");
    let notice_path = cluster_dir.join("revoked_notice.json");
    std::fs::write(&notice_path, br#"{"by_fp":"SHA256:issuer","at":"123"}"#).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&notice_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let (token_path, settings_path, settings_before_rotation) = prepare_local_rotation_state(&node);

    assert_new_identity_rotation(
        &node,
        &old_fingerprint,
        &token_path,
        &settings_path,
        &settings_before_rotation,
    );
}

#[test]
fn joiner_succeeds_when_an_existing_peer_stalls() {
    let _serial = live_test_guard();
    let a = PrivateNode::start("join-stalled-a");
    let b = PrivateNode::start("join-stalled-b");
    let c = PrivateNode::start("join-stalled-c");
    successful(a.run(&["cluster", "init"]), "initialize node A");
    successful(b.run(&["cluster", "init"]), "initialize node B");
    successful(c.run(&["cluster", "init"]), "initialize node C");
    let a_identity = a.identity();
    let a_listener = ListenerProcess::start(&a);
    let mut b_listener = ListenerProcess::start(&b);
    let c_listener = ListenerProcess::start(&c);

    join_member(&a, &b, &a_identity, a_listener.address, b_listener.address);
    b_listener.stop();
    let _stalled_peer = BlackholePeer::start(b_listener.address);

    let invitation = successful(
        a.run(&[
            "cluster",
            "invite",
            "--bind",
            &a_listener.address.to_string(),
        ]),
        "mint invitation while an existing peer is stalled",
    );
    let started = Instant::now();
    let (invite_fingerprint, join_line) = invite_command_args(&invitation);
    let joined = c.run(&[
        "cluster",
        "join",
        invite_fingerprint,
        join_line,
        "--bind",
        &c_listener.address.to_string(),
    ]);
    let output = successful(joined, "join C while existing peer B is stalled");
    assert!(
        output.contains("Joined cluster."),
        "join command did not report success: {output}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(25),
        "join reply waited on an existing peer for {:?}",
        started.elapsed()
    );
    assert_joiner_stores_issuer(&c, &a_identity, a_listener.address);
}

fn join_member(
    issuer: &PrivateNode,
    joiner: &PrivateNode,
    issuer_identity: &[u8],
    issuer_address: SocketAddr,
    joiner_address: SocketAddr,
) {
    let address = issuer_address.to_string();
    let invite = successful(
        issuer.run(&["cluster", "invite", "--bind", &address]),
        "mint join invitation",
    );
    let (invite_fingerprint, join_line) = invite_command_args(&invite);
    assert_eq!(invite_fingerprint, identity_fingerprint(issuer_identity));
    successful(
        joiner.run(&[
            "cluster",
            "join",
            invite_fingerprint,
            join_line,
            "--bind",
            &joiner_address.to_string(),
        ]),
        "join member to issuer",
    );
}

fn assert_joiner_stores_issuer(
    joiner: &PrivateNode,
    issuer_identity: &[u8],
    issuer_address: SocketAddr,
) {
    let registry: remuda_native::cluster::Registry = serde_json::from_slice(
        &std::fs::read(
            joiner
                .root
                .join("state/remuda/cluster/authorized_nodes.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let issuer = registry
        .authorized_nodes
        .iter()
        .find(|entry| entry.node_fp == identity_fingerprint(issuer_identity))
        .expect("joiner stores the pinned issuer");
    assert_eq!(issuer.state, remuda_native::cluster::NodeState::Admitted);
    assert_eq!(
        issuer.endpoint.as_deref(),
        Some(issuer_address.to_string().as_str())
    );
    let self_fp = identity_fingerprint(&joiner.identity());
    let self_entry = registry
        .authorized_nodes
        .iter()
        .find(|entry| entry.node_fp == self_fp)
        .expect("joiner keeps its own registry entry");
    assert_eq!(self_entry.by, issuer.node_fp);
}

#[test]
fn join_via_non_founder_bootstraps_full_view_including_revoked_origins() {
    let _serial = live_test_guard();
    let a = PrivateNode::start("bootstrap-a");
    let b = PrivateNode::start("bootstrap-b");
    let x = PrivateNode::start("bootstrap-x");
    let c = PrivateNode::start("bootstrap-c");
    let d = PrivateNode::start("bootstrap-d");
    for node in [&a, &b, &x, &c, &d] {
        successful(node.run(&["cluster", "init"]), "initialize bootstrap node");
    }
    let a_id = a.identity();
    let b_id = b.identity();
    let x_id = x.identity();
    let c_id = c.identity();
    let a_fp = identity_fingerprint(&a_id);
    let b_fp = identity_fingerprint(&b_id);
    let x_fp = identity_fingerprint(&x_id);
    let c_fp = identity_fingerprint(&c_id);

    let a_listener = ListenerProcess::start(&a);
    let b_listener = ListenerProcess::start(&b);
    let x_listener = ListenerProcess::start(&x);
    let c_listener = ListenerProcess::start(&c);
    let d_listener = ListenerProcess::start(&d);

    join_member(&a, &b, &a_id, a_listener.address, b_listener.address);
    join_member(&b, &x, &b_id, b_listener.address, x_listener.address);
    wait_for_member_and_probe(&a, &x_fp, &x_id, &a_id, a_listener.address);
    successful(a.run(&["cluster", "revoke", &b_fp, "--yes"]), "revoke B");
    wait_for_revocation_on(&x, &b_fp);

    // C joins after B is revoked, through the founder. D then joins through
    // non-founder C, proving the issuer snapshot carries the complete view.
    join_member(&a, &c, &a_id, a_listener.address, c_listener.address);
    join_member(&c, &d, &c_id, c_listener.address, d_listener.address);

    let d_registry = read_registry(&d);
    let lookup = |fingerprint: &str| {
        d_registry
            .authorized_nodes
            .iter()
            .find(|entry| entry.node_fp == fingerprint)
            .unwrap_or_else(|| panic!("bootstrapped registry lacks {fingerprint}"))
    };
    assert_eq!(
        lookup(&a_fp).state,
        remuda_native::cluster::NodeState::Admitted
    );
    assert_eq!(
        lookup(&b_fp).state,
        remuda_native::cluster::NodeState::Revoked
    );
    assert_eq!(
        lookup(&x_fp).state,
        remuda_native::cluster::NodeState::Admitted
    );
    assert_eq!(lookup(&x_fp).by, b_fp);
    assert_eq!(
        lookup(&c_fp).state,
        remuda_native::cluster::NodeState::Admitted
    );
    assert_eq!(
        lookup(&identity_fingerprint(&d.identity())).state,
        remuda_native::cluster::NodeState::Admitted
    );
}

fn wait_for_member_and_probe(
    receiver: &PrivateNode,
    member_fp: &str,
    member_identity: &[u8],
    receiver_identity: &[u8],
    receiver_address: SocketAddr,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let members = successful(receiver.run(&["cluster", "nodes"]), "read nodes table");
        if members.contains(member_fp) {
            assert!(matches!(
                list_from(member_identity, receiver_identity, receiver_address),
                Response::Sessions(_)
            ));
            return;
        }
        assert!(
            Instant::now() < deadline,
            "member did not converge: {members}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_revocation_on(receiver: &PrivateNode, member_fp: &str) {
    wait_for_revocation_on_until(
        receiver,
        member_fp,
        Instant::now() + Duration::from_secs(30),
    );
}

fn wait_for_revoked_notice_on_until(
    receiver: &PrivateNode,
    member_fp: &str,
    deadline: Instant,
) -> String {
    loop {
        let output = successful(
            receiver.run(&["cluster", "nodes"]),
            "read revoked node's nodes table",
        );
        if output.contains("This node was revoked by") && output.contains(member_fp) {
            return output;
        }
        assert!(
            Instant::now() < deadline,
            "revoked node did not receive its notice within 30 seconds: {output}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_revocation_on_until(receiver: &PrivateNode, member_fp: &str, deadline: Instant) {
    loop {
        let members = successful(
            receiver.run(&["cluster", "nodes"]),
            "read revoked nodes table",
        );
        if members
            .lines()
            .any(|line| line.contains(member_fp) && line.contains("revoked"))
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "tombstone did not converge: {members}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_endpoint_on(receiver: &PrivateNode, member_fp: &str, endpoint: SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let registry = read_registry(receiver);
        if registry.authorized_nodes.iter().any(|entry| {
            entry.node_fp == member_fp && entry.endpoint.as_deref() == Some(&endpoint.to_string())
        }) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "endpoint for {member_fp} did not converge"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn revoke_topology(
    label: &str,
) -> (
    PrivateNode,
    PrivateNode,
    PrivateNode,
    ListenerProcess,
    ListenerProcess,
    ListenerProcess,
    String,
    String,
    String,
) {
    let a = PrivateNode::start_with_anti_entropy(&format!("{label}-a"), Some(200));
    let b = PrivateNode::start_with_anti_entropy(&format!("{label}-b"), Some(200));
    let c = PrivateNode::start_with_anti_entropy(&format!("{label}-c"), Some(200));
    for node in [&a, &b, &c] {
        successful(
            node.run(&["cluster", "init"]),
            "initialize revoke topology node",
        );
    }
    let a_id = a.identity();
    let b_id = b.identity();
    let c_id = c.identity();
    let a_fp = identity_fingerprint(&a_id);
    let b_fp = identity_fingerprint(&b_id);
    let c_fp = identity_fingerprint(&c_id);
    let a_listener = ListenerProcess::start(&a);
    let b_listener = ListenerProcess::start(&b);
    let c_listener = ListenerProcess::start(&c);
    join_member(&a, &b, &a_id, a_listener.address, b_listener.address);
    join_member(&b, &c, &b_id, b_listener.address, c_listener.address);

    // The bootstrap snapshot must carry A's endpoint to C. B's admission
    // update must carry C's endpoint back to A so the direct push can work.
    wait_for_endpoint_on(&c, &a_fp, a_listener.address);
    wait_for_endpoint_on(&a, &c_fp, c_listener.address);
    (
        a, b, c, a_listener, b_listener, c_listener, a_fp, b_fp, c_fp,
    )
}

#[test]
fn revoke_cli_pushes_tombstone_across_a_b_c_join_chain() {
    let _serial = live_test_guard();
    let (a, _b, c, _a_listener, _b_listener, _c_listener, _a_fp, b_fp, c_fp) =
        revoke_topology("revoke-fast-path");
    let started = Instant::now();
    let output = a.run(&["cluster", "revoke", &b_fp, "--yes"]);
    assert!(
        output.status.success(),
        "revoke B with direct C push failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    wait_for_revocation_on_until(&c, &b_fp, started + Duration::from_secs(20));
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains(&format!("Registry push reached peer {c_fp}.")),
        "CLI revoke did not push the tombstone to peer C: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "revocation did not converge within the 20 second bound"
    );
}

#[test]
fn periodic_pull_catches_revocation_when_c_listener_is_unreachable() {
    let _serial = live_test_guard();
    let (a, _b, c, _a_listener, _b_listener, mut c_listener, _a_fp, b_fp, c_fp) =
        revoke_topology("revoke-fallback");
    c_listener.stop();

    let output = a.run(&["cluster", "revoke", &b_fp, "--yes"]);
    assert!(
        output.status.success(),
        "revoke failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!("Registry push did not reach peer {c_fp}")),
        "revoke command did not report C unreachable: stdout={} stderr={stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    wait_for_revocation_on(&c, &b_fp);
}

#[test]
fn revoke_then_new_admission_converges_after_revoked_member_admitted_a_peer() {
    let _serial = live_test_guard();
    let a = PrivateNode::start("origin-a");
    let b = PrivateNode::start("origin-b");
    let x = PrivateNode::start("origin-x");
    let y = PrivateNode::start("origin-y");
    for node in [&a, &b, &x, &y] {
        successful(node.run(&["cluster", "init"]), "initialize private node");
    }
    let a_id = a.identity();
    let b_id = b.identity();
    let x_id = x.identity();
    let b_fp = identity_fingerprint(&b_id);
    let x_fp = identity_fingerprint(&x_id);
    let a_listener = ListenerProcess::start(&a);
    let b_listener = ListenerProcess::start(&b);
    let x_listener = ListenerProcess::start(&x);
    let y_listener = ListenerProcess::start(&y);

    join_member(&a, &b, &a_id, a_listener.address, b_listener.address);
    join_member(&b, &x, &b_id, b_listener.address, x_listener.address);
    wait_for_member_and_probe(&a, &x_fp, &x_id, &a_id, a_listener.address);
    successful(
        a.run(&["cluster", "revoke", &b_fp, "--yes"]),
        "revoke old origin B",
    );

    join_member(&a, &y, &a_id, a_listener.address, y_listener.address);
    let y_fp = identity_fingerprint(&y.identity());
    let a_fp = identity_fingerprint(&a_id);
    wait_for_member_and_probe(&a, &y_fp, &y.identity(), &a_id, a_listener.address);
    wait_for_member_and_probe(&y, &a_fp, &a_id, &y.identity(), y_listener.address);
    wait_for_revocation_on(&y, &b_fp);
    let y_registry = read_registry(&y);
    assert!(y_registry.authorized_nodes.iter().any(|entry| {
        entry.node_fp == x_fp
            && entry.state == remuda_native::cluster::NodeState::Admitted
            && entry.by == b_fp
    }));
}

#[test]
fn admission_on_a_pushes_to_an_existing_peer() {
    let _serial = live_test_guard();
    let a = PrivateNode::start("push-a");
    let b = PrivateNode::start("push-b");
    let c = PrivateNode::start("push-c");
    successful(a.run(&["cluster", "init"]), "initialize node A");
    successful(b.run(&["cluster", "init"]), "initialize node B");
    successful(c.run(&["cluster", "init"]), "initialize node C");
    let status = a.run(&["cluster"]);
    assert!(String::from_utf8_lossy(&status.stdout).contains(
        "Any admitted member can admit new keys and revoke any member cluster-wide (see #282)."
    ));
    let a_listener = ListenerProcess::start(&a);
    let b_listener = ListenerProcess::start(&b);
    let c_listener = ListenerProcess::start(&c);

    let c_bind = c_listener.address.to_string();
    let invitation = successful(
        a.run(&[
            "cluster",
            "invite",
            "--bind",
            &a_listener.address.to_string(),
        ]),
        "mint invitation for C",
    );
    let (c_invite_fingerprint, c_line) = invite_command_args(&invitation);
    successful(
        c.run(&[
            "cluster",
            "join",
            c_invite_fingerprint,
            c_line,
            "--bind",
            &c_bind,
        ]),
        "join C to A",
    );

    let invitation = successful(
        a.run(&[
            "cluster",
            "invite",
            "--bind",
            &a_listener.address.to_string(),
        ]),
        "mint invitation for B",
    );
    let (b_invite_fingerprint, b_line) = invite_command_args(&invitation);
    successful(
        b.run(&[
            "cluster",
            "join",
            b_invite_fingerprint,
            b_line,
            "--bind",
            &b_listener.address.to_string(),
        ]),
        "join B to A",
    );

    let b_fingerprint = identity_fingerprint(&b.identity());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let nodes = successful(c.run(&["cluster", "nodes"]), "list C's members");
        if nodes.contains(&b_fingerprint) {
            let row = nodes
                .lines()
                .find(|line| line.contains(&b_fingerprint))
                .expect("B row on C");
            assert!(row.contains("admitted"), "B is not admitted on C: {row}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "A's admission of B did not reach C: {nodes}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn relayed_push_preserves_origin_and_converges_registry_digests() {
    let _serial = live_test_guard();
    let a = PrivateNode::start("push-origin-a");
    let b = PrivateNode::start("push-origin-b");
    let c = PrivateNode::start("push-origin-c");
    let x = PrivateNode::start("push-origin-x");
    for node in [&a, &b, &c, &x] {
        successful(node.run(&["cluster", "init"]), "initialize push test node");
    }
    let a_id = a.identity();
    let b_id = b.identity();
    let x_id = x.identity();
    let a_fp = identity_fingerprint(&a_id);
    let b_fp = identity_fingerprint(&b_id);
    let x_fp = identity_fingerprint(&x_id);
    let a_listener = ListenerProcess::start(&a);
    let b_listener = ListenerProcess::start(&b);
    let c_listener = ListenerProcess::start(&c);
    let x_listener = ListenerProcess::start(&x);

    join_member(&a, &b, &a_id, a_listener.address, b_listener.address);
    join_member(&a, &c, &a_id, a_listener.address, c_listener.address);
    join_member(&a, &x, &a_id, a_listener.address, x_listener.address);
    wait_for_member_and_probe(&b, &x_fp, &x_id, &b_id, b_listener.address);
    wait_for_member_and_probe(&c, &x_fp, &x_id, &c.identity(), c_listener.address);
    thread::sleep(Duration::from_millis(200));

    // Make X absent at C so the subsequent full push from B exercises the
    // new-admission path at C, where an accidental by rewrite changes digest.
    let mut c_registry = read_registry(&c);
    c_registry
        .authorized_nodes
        .retain(|entry| entry.node_fp != x_fp);
    write_registry(&c, &c_registry);

    let mut b_entry = read_registry(&b)
        .authorized_nodes
        .into_iter()
        .find(|entry| entry.node_fp == b_fp)
        .unwrap();
    b_entry.version += 1;
    b_entry.by = b_fp.clone();
    let update = remuda_native::cluster::registry::RegistryUpdate {
        sender_fp: b_fp.clone(),
        entries: vec![b_entry],
    };
    let response = remuda_native::net::cluster_client::ClusterClient::system()
        .request(
            b_listener.address,
            &b_id[32..],
            &b_id[..32],
            &Request::ClusterRegistryUpdate {
                update_json: String::from_utf8(update.encode().unwrap()).unwrap(),
            },
        )
        .unwrap();
    assert!(matches!(response, Response::ClusterRegistryAck { .. }));

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let b_registry = read_registry(&b);
        let c_registry = read_registry(&c);
        if c_registry
            .authorized_nodes
            .iter()
            .any(|entry| entry.node_fp == x_fp)
            && b_registry.digest().unwrap() == c_registry.digest().unwrap()
        {
            assert_eq!(
                c_registry
                    .authorized_nodes
                    .iter()
                    .find(|entry| entry.node_fp == x_fp)
                    .unwrap()
                    .by,
                a_fp
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "B push did not converge C's digest"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn offline_joined_node_converges_from_its_first_startup_sync() {
    let _serial = live_test_guard();
    let a = PrivateNode::start("offline-a");
    let mut b = PrivateNode::start("offline-b");
    let c = PrivateNode::start("offline-c");
    successful(a.run(&["cluster", "init"]), "initialize node A");
    successful(b.run(&["cluster", "init"]), "initialize node B");
    successful(c.run(&["cluster", "init"]), "initialize node C");
    let a_listener = ListenerProcess::start(&a);
    let c_listener = ListenerProcess::start(&c);

    let invitation = successful(
        a.run(&[
            "cluster",
            "invite",
            "--bind",
            &a_listener.address.to_string(),
        ]),
        "mint invitation for B",
    );
    let (b_invite_fingerprint, b_line) = invite_command_args(&invitation);
    successful(
        b.run(&["cluster", "join", b_invite_fingerprint, b_line]),
        "join B to A",
    );
    b.stop_daemon();

    let invitation = successful(
        a.run(&[
            "cluster",
            "invite",
            "--bind",
            &a_listener.address.to_string(),
        ]),
        "mint invitation for C",
    );
    let (c_invite_fingerprint, c_line) = invite_command_args(&invitation);
    successful(
        c.run(&[
            "cluster",
            "join",
            c_invite_fingerprint,
            c_line,
            "--bind",
            &c_listener.address.to_string(),
        ]),
        "join C to A while B is offline",
    );

    b.restart_daemon();
    let c_fingerprint = identity_fingerprint(&c.identity());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let members = successful(b.run(&["cluster", "nodes"]), "read B's nodes table");
        let row = members.lines().find(|line| line.contains(&c_fingerprint));
        if row.is_some_and(|row| row.contains("admitted")) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "B did not converge on startup sync: {members}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn revoke_survives_an_endpoint_mismatch_in_the_same_update() {
    let _serial = live_test_guard();
    let a = PrivateNode::start("endpoint-revoke-a");
    let b = PrivateNode::start("endpoint-revoke-b");
    let c = PrivateNode::start("endpoint-revoke-c");
    successful(a.run(&["cluster", "init"]), "initialize node A");
    successful(b.run(&["cluster", "init"]), "initialize node B");
    successful(c.run(&["cluster", "init"]), "initialize node C");
    let a_identity = a.identity();
    let b_identity = b.identity();
    let c_identity = c.identity();
    let a_fp = identity_fingerprint(&a_identity);
    let b_fp = identity_fingerprint(&b_identity);
    let c_fp = identity_fingerprint(&c_identity);

    let a_registry = remuda_native::cluster::Registry {
        authorized_nodes: vec![
            authorized_node(&a_identity, &a_fp, Some("127.0.0.1:43001")),
            authorized_node(&b_identity, &a_fp, Some("127.0.0.1:43002")),
            authorized_node(&c_identity, &a_fp, Some("127.0.0.1:43003")),
        ],
    };
    let b_registry = remuda_native::cluster::Registry {
        authorized_nodes: vec![
            authorized_node(&a_identity, &a_fp, Some("127.0.0.1:43001")),
            authorized_node(&b_identity, &a_fp, Some("127.0.0.1:43002")),
            // C advertised a newer routing hint directly to B.
            authorized_node(&c_identity, &b_fp, Some("127.0.0.1:43004")),
        ],
    };
    write_registry(&a, &a_registry);
    write_registry(&b, &b_registry);

    let b_listener = ListenerProcess::start(&b);
    successful(
        a.run(&["cluster", "revoke", &c_fp, "--yes"]),
        "revoke node C on A",
    );
    let a_registry: remuda_native::cluster::Registry = serde_json::from_slice(
        &std::fs::read(a.root.join("state/remuda/cluster/authorized_nodes.json")).unwrap(),
    )
    .unwrap();
    let c_tombstone = a_registry
        .authorized_nodes
        .into_iter()
        .find(|entry| entry.node_fp == c_fp)
        .expect("node C tombstone on A");
    assert_eq!(
        c_tombstone.state,
        remuda_native::cluster::NodeState::Revoked
    );
    assert_eq!(c_tombstone.endpoint.as_deref(), Some("127.0.0.1:43003"));
    assert_eq!(
        b_registry
            .authorized_nodes
            .iter()
            .find(|entry| entry.node_fp == c_fp)
            .and_then(|entry| entry.endpoint.as_deref()),
        Some("127.0.0.1:43004")
    );

    let update = remuda_native::cluster::registry::RegistryUpdate {
        sender_fp: a_fp,
        entries: vec![c_tombstone],
    };
    let update_json = String::from_utf8(update.encode().unwrap()).unwrap();
    let response = remuda_native::net::cluster_client::ClusterClient::system()
        .request(
            b_listener.address,
            &b_identity[32..],
            &a_identity[..32],
            &Request::ClusterRegistryUpdate { update_json },
        )
        .unwrap();
    assert!(
        matches!(response, Response::ClusterRegistryAck { .. }),
        "{response:?}"
    );

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let registry: remuda_native::cluster::Registry = serde_json::from_slice(
            &std::fs::read(b.root.join("state/remuda/cluster/authorized_nodes.json")).unwrap(),
        )
        .unwrap();
        if registry.authorized_nodes.iter().any(|entry| {
            entry.node_fp == c_fp && entry.state == remuda_native::cluster::NodeState::Revoked
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "B did not apply C's tombstone");
        thread::sleep(Duration::from_millis(20));
    }
    let b_registry: remuda_native::cluster::Registry = serde_json::from_slice(
        &std::fs::read(b.root.join("state/remuda/cluster/authorized_nodes.json")).unwrap(),
    )
    .unwrap();
    let c_entry = b_registry
        .authorized_nodes
        .iter()
        .find(|entry| entry.node_fp == c_fp)
        .expect("node C entry on B");
    assert_eq!(c_entry.state, remuda_native::cluster::NodeState::Revoked);
}
