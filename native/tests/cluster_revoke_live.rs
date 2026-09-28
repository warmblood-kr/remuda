//! Live revocation coverage across two isolated cluster daemons.

#![cfg(unix)]
// This integration test deliberately exercises the real loopback cluster wire.
#![allow(clippy::disallowed_types)]

use remuda_core::protocol::{Request, Response};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use zeroize::Zeroizing;

static NEXT_NODE_ID: AtomicUsize = AtomicUsize::new(0);

struct PrivateNode {
    child: Child,
    root: PathBuf,
    runtime: PathBuf,
    name: String,
}

impl PrivateNode {
    fn start(_label: &str) -> Self {
        let id = NEXT_NODE_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("cr{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let runtime = root.join("runtime");
        let home = root.join("home");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let name = format!("c{}-{id}", std::process::id());
        let child = spawn_daemon(&name, &runtime, &root);
        let mut node = Self {
            child,
            root,
            runtime,
            name,
        };
        node.wait_ready();
        node
    }

    fn daemon_path(&self) -> PathBuf {
        remuda_native::daemon::socket_path_in(&self.runtime, &self.name)
    }

    fn restart_daemon(&mut self) {
        self.stop_daemon();
        self.child = spawn_daemon(&self.name, &self.runtime, &self.root);
        self.wait_ready();
    }

    fn stop_daemon(&mut self) {
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

fn spawn_daemon(name: &str, runtime: &PathBuf, root: &PathBuf) -> Child {
    Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(["-s", name, "daemon"])
        .env("REMUDA_RUNTIME_DIR", runtime)
        .env("HOME", root.join("home"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap()
}

impl Drop for PrivateNode {
    fn drop(&mut self) {
        assert!(self.runtime.starts_with(&self.root));
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct ListenerProcess {
    child: Child,
    address: SocketAddr,
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
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut listener = Self { child, address };
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
}

impl Drop for ListenerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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
        state: remuda_native::cluster::NodeState::Admitted,
        version: 1,
        by: by.to_owned(),
    }
}

fn write_registry(node: &PrivateNode, registry: &remuda_native::cluster::Registry) {
    let path = node.root.join("state/remuda/cluster/authorized_nodes.json");
    std::fs::write(path, serde_json::to_vec(registry).unwrap()).unwrap();
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

// TODO after #254 merges: add a two-daemon revoke-during-held-Sync integration
// check, then assert the following Sync poll fails. The listener socket test
// covers the held-response authorization recheck until the Sync API exists.
#[test]
fn revoking_a_live_member_is_seen_by_all_other_daemons() {
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
    let address = listener.address.to_string();
    let invite = successful(
        a.run(&["cluster", "invite", "--bind", &address]),
        "mint join invitation",
    );
    let join_line = invite.lines().nth(1).unwrap();
    successful(
        b.run(&[
            "cluster",
            "join",
            &identity_fingerprint(&a_identity),
            join_line,
            "--bind",
            &b_listener.address.to_string(),
        ]),
        "join node B to node A",
    );

    let b_registry: remuda_native::cluster::Registry = serde_json::from_slice(
        &std::fs::read(b.root.join("state/remuda/cluster/authorized_nodes.json")).unwrap(),
    )
    .unwrap();
    let recorded_issuer = b_registry
        .authorized_nodes
        .iter()
        .find(|entry| entry.node_fp == identity_fingerprint(&a_identity))
        .expect("joiner stores the pinned issuer");
    assert_eq!(
        recorded_issuer.state,
        remuda_native::cluster::NodeState::Admitted
    );
    assert_eq!(recorded_issuer.endpoint.as_deref(), Some(address.as_str()));

    let invite = successful(
        a.run(&["cluster", "invite", "--bind", &address]),
        "mint join invitation for node C",
    );
    let join_line = invite.lines().nth(1).unwrap();
    successful(
        c.run(&[
            "cluster",
            "join",
            &identity_fingerprint(&a_identity),
            join_line,
            "--bind",
            &c_listener.address.to_string(),
        ]),
        "join node C to node A",
    );

    let sync_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let members = successful(c.run(&["cluster", "nodes"]), "read C's nodes table");
        if members.contains(&b_fingerprint) {
            assert!(matches!(
                list_from(&b_identity, &c.identity(), c_listener.address),
                Response::Sessions(_)
            ));
            break;
        }
        assert!(
            Instant::now() < sync_deadline,
            "node B did not reach C before revocation: {members}"
        );
        thread::sleep(Duration::from_millis(50));
    }

    assert!(matches!(
        list_from(&b_identity, &a_identity, listener.address),
        Response::Sessions(_)
    ));
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

    let revoke_sync_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let members = successful(c.run(&["cluster", "nodes"]), "read C's revoked nodes table");
        let row = members.lines().find(|line| line.contains(&b_fingerprint));
        if row.is_some_and(|row| row.contains("revoked")) {
            break;
        }
        assert!(
            Instant::now() < revoke_sync_deadline,
            "node C did not receive B's tombstone: {members}"
        );
        thread::sleep(Duration::from_millis(50));
    }
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
fn admission_on_a_pushes_to_an_existing_peer() {
    let a = PrivateNode::start("push-a");
    let b = PrivateNode::start("push-b");
    let c = PrivateNode::start("push-c");
    successful(a.run(&["cluster", "init"]), "initialize node A");
    successful(b.run(&["cluster", "init"]), "initialize node B");
    successful(c.run(&["cluster", "init"]), "initialize node C");
    let a_identity = a.identity();
    let a_fingerprint = identity_fingerprint(&a_identity);
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
    let c_line = invitation.lines().nth(1).unwrap();
    successful(
        c.run(&["cluster", "join", &a_fingerprint, c_line, "--bind", &c_bind]),
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
    let b_line = invitation.lines().nth(1).unwrap();
    successful(
        b.run(&[
            "cluster",
            "join",
            &a_fingerprint,
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
fn offline_joined_node_converges_from_its_first_startup_sync() {
    let a = PrivateNode::start("offline-a");
    let mut b = PrivateNode::start("offline-b");
    let c = PrivateNode::start("offline-c");
    successful(a.run(&["cluster", "init"]), "initialize node A");
    successful(b.run(&["cluster", "init"]), "initialize node B");
    successful(c.run(&["cluster", "init"]), "initialize node C");
    let a_identity = a.identity();
    let a_fingerprint = identity_fingerprint(&a_identity);
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
    successful(
        b.run(&[
            "cluster",
            "join",
            &a_fingerprint,
            invitation.lines().nth(1).unwrap(),
        ]),
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
    successful(
        c.run(&[
            "cluster",
            "join",
            &a_fingerprint,
            invitation.lines().nth(1).unwrap(),
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
