#![cfg(unix)]
#![allow(clippy::disallowed_types)]

#[path = "daemon_support/hosts.rs"]
mod hosts;
use remuda_native::cluster::{encoding, AuthorizedNode, NodeState, Registry};
use std::fs;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

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
        let temp = if Path::new("/private/tmp").is_dir() {
            PathBuf::from("/private/tmp")
        } else {
            std::env::temp_dir()
        };
        let root = temp
            .canonicalize()
            .unwrap()
            .join(format!("rc{}-{serial}", std::process::id()));
        let runtime = root.join("runtime");
        let state = root.join("state");
        let home = root.join("home");
        for dir in [&runtime, &state, &home] {
            fs::create_dir_all(dir).unwrap();
        }
        let name = format!("{label}-{serial}");
        let daemon = command(&name, &runtime, &root, &state, &home)
            .args(["daemon"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
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
        while remuda_native::client::request(&path, &remuda_core::protocol::Request::List).is_err()
        {
            if let Some(status) = self.daemon.try_wait().unwrap() {
                panic!("daemon exited before ready: {status}");
            }
            assert!(Instant::now() < deadline, "daemon did not bind");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        assert!(self.runtime.starts_with(&self.root));
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        hosts::stop_hosts_under(&self.root);
        let _ = fs::remove_dir_all(&self.root);
    }
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

struct Listener(Child);

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
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
                    format_major: remuda_native::cluster::registry::REGISTRY_FORMAT_MAJOR,
                    format_minor: remuda_native::cluster::registry::REGISTRY_FORMAT_MINOR,
                    endpoint: None,
                    delivered_by: None,
                    state: NodeState::Admitted,
                    version: 1,
                    by: left.fingerprint(),
                    optional_fields: Default::default(),
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

fn listener(server: &Node) -> (Listener, SocketAddr) {
    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = reservation.local_addr().unwrap();
    drop(reservation);
    let address = addr.to_string();
    let child = server
        .command()
        .args(["cluster", "listen", "--bind", &address])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut listener = Listener(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match TcpStream::connect(addr) {
            Ok(stream) => {
                drop(stream);
                return (listener, addr);
            }
            Err(_) => {
                if let Some(status) = listener.0.try_wait().unwrap() {
                    panic!("listener exited before ready: {status}");
                }
                assert!(Instant::now() < deadline, "listener did not bind");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// One authenticated request the fake peer received.
struct FakePeer {
    address: SocketAddr,
    public: Vec<u8>,
    task: std::thread::JoinHandle<Vec<remuda_core::protocol::Request>>,
}

enum FakeAnswer {
    Reply(remuda_core::protocol::Response),
    Raw(Vec<u8>),
    Stall(std::time::Duration),
    Close,
}

fn read_http_body(stream: &TcpStream) -> Vec<u8> {
    use std::io::{BufRead, BufReader, Read};
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut length = 0;
    loop {
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).unwrap();
        if line == b"\r\n" || line == b"\n" {
            break;
        }
        if let Ok(line) = std::str::from_utf8(&line) {
            if line.to_ascii_lowercase().starts_with("content-length:") {
                length = line.split_once(':').unwrap().1.trim().parse().unwrap();
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    body
}

fn write_http_body(stream: &mut TcpStream, body: &[u8]) {
    use std::io::Write;
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    stream.write_all(body).unwrap();
    stream.flush().unwrap();
}

/// A single-request Noise peer whose exact request and answer the test owns.
fn fake_peer(
    answer: impl Fn(&remuda_core::protocol::Request) -> FakeAnswer + Send + 'static,
) -> FakePeer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let responder = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
        .generate_keypair()
        .unwrap();
    let public = responder.public.clone();
    let private = responder.private.clone();
    let task = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let body = read_http_body(&stream);
        let opened =
            remuda_native::net::frame::open_request(&private, &body).expect("open request");
        let request: remuda_core::protocol::Request =
            serde_json::from_slice(&opened.payload).expect("request JSON");
        match answer(&request) {
            FakeAnswer::Reply(response) => {
                let payload = serde_json::to_vec(&response).unwrap();
                let sealed =
                    remuda_native::net::frame::seal_response(opened, &payload).expect("seal");
                write_http_body(&mut stream, &sealed);
            }
            FakeAnswer::Raw(body) => write_http_body(&mut stream, &body),
            FakeAnswer::Stall(delay) => {
                std::thread::sleep(delay);
                drop(stream);
            }
            FakeAnswer::Close => drop(stream),
        }
        vec![request]
    });
    FakePeer {
        address,
        public,
        task,
    }
}

fn admit_peer(client: &Node, public: &[u8], address: Option<SocketAddr>) -> String {
    let fingerprint = encoding::fingerprint(public);
    let path = client.state.join("remuda/cluster/authorized_nodes.json");
    let mut registry: Registry = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    registry.authorized_nodes.push(AuthorizedNode {
        node_fp: fingerprint.clone(),
        static_pubkey: encoding::encode_base64(public),
        format_major: remuda_native::cluster::registry::REGISTRY_FORMAT_MAJOR,
        format_minor: remuda_native::cluster::registry::REGISTRY_FORMAT_MINOR,
        endpoint: address.map(|address| address.to_string()),
        delivered_by: None,
        state: NodeState::Admitted,
        version: 1,
        by: client.fingerprint(),
        optional_fields: Default::default(),
    });
    fs::write(&path, serde_json::to_vec_pretty(&registry).unwrap()).unwrap();
    fingerprint
}

fn set_endpoint(client: &Node, fingerprint: &str, address: SocketAddr) {
    let path = client.state.join("remuda/cluster/authorized_nodes.json");
    let mut registry: Registry = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    registry
        .authorized_nodes
        .iter_mut()
        .find(|entry| entry.node_fp == fingerprint)
        .expect("peer entry")
        .endpoint = Some(address.to_string());
    fs::write(&path, serde_json::to_vec_pretty(&registry).unwrap()).unwrap();
}

fn mcp_call(node: &Node, name: &str, arguments: serde_json::Value) -> serde_json::Value {
    use std::io::{BufRead, BufReader, Write};
    let mut child = node
        .command()
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": name, "arguments": arguments},
    });
    writeln!(child.stdin.as_mut().unwrap(), "{request}").unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let _ = child.kill();
    let _ = child.wait();
    serde_json::from_str(&line).unwrap_or_else(|error| {
        panic!("MCP reply is not JSON ({error}): {line}");
    })
}

fn mcp_text(reply: &serde_json::Value) -> String {
    reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn fake_summary(name: &str, instance: Option<&str>) -> remuda_core::SessionSummary {
    remuda_core::SessionSummary {
        id: format!("id-{name}"),
        name: name.to_owned(),
        instance_id: instance.map(str::to_owned),
        output_version: Some(1),
        alive: true,
        idle: std::time::Duration::from_secs(2),
        output_idle: Some(std::time::Duration::from_secs(3)),
        size: remuda_core::Size::new(80, 24),
        attached: false,
        human_idle: None,
        mouse_tracking: false,
        unavailable: false,
        program_status: None,
        program_status_age_ms: None,
        remote_input_disabled: false,
    }
}

fn remote_request(
    peer: &Node,
    server: &Node,
    addr: SocketAddr,
    request: &remuda_core::protocol::Request,
) -> remuda_core::protocol::Response {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::time::{SystemTime, UNIX_EPOCH};

    let payload = serde_json::to_vec(request).unwrap();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let sealed =
        remuda_native::net::frame::seal_request(&peer.private, &server.public, timestamp, &payload)
            .unwrap();
    let mut stream = TcpStream::connect(addr).unwrap();
    write!(
        stream,
        "POST /cluster HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        sealed.message.len()
    )
    .unwrap();
    stream.write_all(&sealed.message).unwrap();
    stream.flush().unwrap();
    let mut response = BufReader::new(stream);
    let mut status_line = String::new();
    response.read_line(&mut status_line).unwrap();
    assert!(
        status_line.contains(" 200 "),
        "unexpected HTTP response: {status_line}"
    );
    let mut content_length = None;
    loop {
        let mut line = String::new();
        response.read_line(&mut line).unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let mut ciphertext = vec![0; content_length.expect("response content length")];
    response.read_exact(&mut ciphertext).unwrap();
    let plaintext = remuda_native::net::frame::open_response(sealed, &ciphertext).unwrap();
    serde_json::from_slice(&plaintext).unwrap()
}

fn start_proof_session(server: &Node) -> String {
    use remuda_core::protocol::{Request, Response};

    let start = server
        .command()
        .args(["-e", "remuda.new('proof', {'/bin/sh', '-c', 'sleep 30'})"])
        .output()
        .unwrap();
    assert!(start.status.success(), "session start failed: {start:?}");
    let daemon = remuda_native::daemon::socket_path_in(&server.runtime, &server.name);
    match remuda_native::client::request(&daemon, &Request::List).unwrap() {
        Response::Sessions(sessions) => sessions
            .into_iter()
            .find(|session| session.name == "proof")
            .and_then(|session| session.instance_id)
            .expect("proof instance id"),
        other => panic!("unexpected List response: {other:?}"),
    }
}

#[test]
fn cluster_call_lists_and_captures_through_the_black_box_cli() {
    let client = Node::start("client");
    let server = Node::start("server");
    admit_pair(&client, &server);
    let (_listener, addr) = listener(&server);

    let start = server
        .command()
        .args([
            "-e",
            "remuda.new('proof', {'/bin/sh', '-c', 'printf CLUSTER_CALL_VISIBLE; sleep 15'})",
        ])
        .output()
        .unwrap();
    assert!(start.status.success(), "session start failed: {start:?}");

    let label = remuda_native::cluster::node_label(&server.fingerprint());
    let address = addr.to_string();
    let list = client
        .command()
        .args(["cluster", "call", &label, "list", "--addr", &address])
        .output()
        .unwrap();
    assert!(list.status.success(), "list failed: {list:?}");
    assert!(String::from_utf8_lossy(&list.stdout).contains("proof\tlive"));

    let capture = client
        .command()
        .args([
            "cluster", "call", &label, "capture", "proof", "--addr", &address,
        ])
        .output()
        .unwrap();
    assert!(capture.status.success(), "capture failed: {capture:?}");
    assert!(String::from_utf8_lossy(&capture.stdout).contains("CLUSTER_CALL_VISIBLE"));

    let json = client
        .command()
        .args([
            "cluster", "call", &label, "list", "--json", "--addr", &address,
        ])
        .output()
        .unwrap();
    assert!(json.status.success(), "json list failed: {json:?}");
    let parsed: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert!(parsed.get("Sessions").is_some());
}

fn install_legacy_alias(client: &Node, fingerprint: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = client.state.join("remuda/cluster/settings.json");
    fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "allow_remote_control": true,
            "node_aliases": { (fingerprint): "legacy-workstation" },
        }))
        .unwrap(),
    )
    .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn assert_remote_failure(client: &Node, target: &str, expected: &str) {
    let remote = client
        .command()
        .args(["cluster", "remote", target])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&remote.stderr);
    assert!(!remote.status.success(), "remote target: {remote:?}");
    assert!(stderr.contains(expected), "{remote:?}");
    if expected == "revoked" {
        assert!(!stderr.contains("no registry endpoint"), "{remote:?}");
    }
}

#[test]
fn cluster_alias_addressing_survives_rename_clear_and_revoke() {
    let client = Node::start("alias-client");
    let server = Node::start("alias-server");
    admit_pair(&client, &server);
    let (_listener, addr) = listener(&server);
    let fingerprint = server.fingerprint();
    let node_id = remuda_native::cluster::node_label(&fingerprint);
    let address = addr.to_string();
    let run = |args: &[&str]| client.command().args(args).output().unwrap();
    let call = |target: &str| run(&["cluster", "call", target, "list", "--addr", &address]);

    install_legacy_alias(&client, &fingerprint);
    let legacy = call("legacy-workstation");
    assert!(legacy.status.success(), "legacy alias failed: {legacy:?}");
    let assigned = run(&["cluster", "alias", &fingerprint, "workstation"]);
    assert!(assigned.status.success(), "assign failed: {assigned:?}");
    let old = call("legacy-workstation");
    assert_eq!(
        old.status.code(),
        Some(4),
        "superseded legacy alias: {old:?}"
    );
    for target in ["workstation", node_id.as_str(), fingerprint.as_str()] {
        let listed = call(target);
        assert!(listed.status.success(), "list {target} failed: {listed:?}");
    }
    for target in ["workstation", "workstation/proof"] {
        assert_remote_failure(&client, target, "no registry endpoint");
    }

    let local_fp = client.fingerprint();
    let collision = run(&["cluster", "alias", &local_fp, "workstation"]);
    assert!(
        collision.status.success(),
        "duplicate assignment: {collision:?}"
    );
    let ambiguous = call("workstation");
    assert_eq!(
        ambiguous.status.code(),
        Some(2),
        "ambiguous call: {ambiguous:?}"
    );
    assert!(String::from_utf8_lossy(&ambiguous.stderr).contains("ambiguous"));
    for args in [
        vec!["cluster", "remote", "workstation"],
        vec!["cluster", "remote", "workstation/proof"],
        vec!["cluster", "revoke", "workstation", "--yes"],
        vec!["cluster", "alias", "workstation", "renamed"],
    ] {
        let rejected = run(&args);
        assert!(
            !rejected.status.success(),
            "ambiguous mutation: {rejected:?}"
        );
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("ambiguous"));
    }
    let cleared = run(&["cluster", "alias", &local_fp, "--clear"]);
    assert!(cleared.status.success(), "clear collision: {cleared:?}");

    let renamed = run(&["cluster", "alias", "workstation", "작업실"]);
    assert!(renamed.status.success(), "rename failed: {renamed:?}");
    let old = call("workstation");
    assert_eq!(old.status.code(), Some(4), "old alias: {old:?}");
    let cleared = run(&["cluster", "alias", "작업실", "--clear"]);
    assert!(cleared.status.success(), "clear failed: {cleared:?}");
    let old = call("작업실");
    assert_eq!(old.status.code(), Some(4), "cleared alias: {old:?}");

    let assigned = run(&["cluster", "alias", &node_id, "workstation"]);
    assert!(assigned.status.success(), "reassign failed: {assigned:?}");
    let revoked = run(&["cluster", "revoke", "workstation", "--yes"]);
    assert!(revoked.status.success(), "revoke failed: {revoked:?}");
    let listed = call("workstation");
    assert_eq!(listed.status.code(), Some(4), "revoked alias: {listed:?}");
    assert!(String::from_utf8_lossy(&listed.stderr).contains("revoked"));
    for target in ["workstation", "workstation/proof"] {
        assert_remote_failure(&client, target, "revoked");
    }
}

#[test]
fn remote_close_uses_the_local_control_gate_and_daemon_instance_check() {
    use remuda_core::protocol::{Request, Response};

    let client = Node::start("close-client");
    let server = Node::start("close-server");
    admit_pair(&client, &server);
    let (_listener, addr) = listener(&server);

    let instance_id = start_proof_session(&server);
    let daemon = remuda_native::daemon::socket_path_in(&server.runtime, &server.name);

    let off = server
        .command()
        .args(["cluster", "control", "off"])
        .output()
        .unwrap();
    assert!(off.status.success(), "control off failed: {off:?}");
    let disabled = remote_request(
        &client,
        &server,
        addr,
        &Request::Close {
            name: "proof".into(),
            instance_id: Some(instance_id.clone()),
            confirm: Some(true),
        },
    );
    assert_eq!(disabled, Response::RemoteControlDisabled);

    let on = server
        .command()
        .args(["cluster", "control", "on"])
        .output()
        .unwrap();
    assert!(on.status.success(), "control on failed: {on:?}");
    let legacy = remote_request(
        &client,
        &server,
        addr,
        &Request::Close {
            name: "proof".into(),
            instance_id: None,
            confirm: None,
        },
    );
    assert_eq!(legacy, Response::error("remote front refuses Close"));

    let closed_previous = remuda_native::client::request(
        &daemon,
        &Request::Close {
            name: "proof".into(),
            instance_id: Some(instance_id.clone()),
            confirm: Some(true),
        },
    )
    .unwrap();
    assert_eq!(closed_previous, Response::Ok);
    let replacement_instance = start_proof_session(&server);
    assert_ne!(replacement_instance, instance_id);

    let stale = remote_request(
        &client,
        &server,
        addr,
        &Request::Close {
            name: "proof".into(),
            instance_id: Some(instance_id),
            confirm: Some(true),
        },
    );
    assert!(
        matches!(stale, Response::Error(_)),
        "stale Close: {stale:?}"
    );
    assert!(matches!(
        remuda_native::client::request(&daemon, &Request::List),
        Ok(Response::Sessions(sessions)) if sessions.iter().any(|session| session.name == "proof")
    ));

    let closed = remote_request(
        &client,
        &server,
        addr,
        &Request::Close {
            name: "proof".into(),
            instance_id: Some(replacement_instance),
            confirm: Some(true),
        },
    );
    assert_eq!(closed, Response::Ok);
    assert!(matches!(
        remuda_native::client::request(&daemon, &Request::List),
        Ok(Response::Sessions(sessions)) if sessions.iter().all(|session| session.name != "proof")
    ));
}

#[test]
fn cluster_call_rejects_unknown_nodes_and_write_verbs_with_documented_codes() {
    let client = Node::start("client");
    let address = "127.0.0.1:9";
    let unknown = client
        .command()
        .args(["cluster", "call", "node-missing", "list", "--addr", address])
        .output()
        .unwrap();
    assert_eq!(unknown.status.code(), Some(4));
    let input = client
        .command()
        .args([
            "cluster",
            "call",
            "node-missing",
            "input",
            "x",
            "--addr",
            address,
        ])
        .output()
        .unwrap();
    assert_eq!(input.status.code(), Some(2));
}

#[test]
fn cluster_call_maps_revoked_members_and_unreachable_peers_to_their_exit_codes() {
    let client = Node::start("client");
    let server = Node::start("server");
    admit_pair(&client, &server);
    let label = remuda_native::cluster::node_label(&server.fingerprint());
    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap().to_string();
    drop(reservation);

    let unreachable = client
        .command()
        .args(["cluster", "call", &label, "list", "--addr", &address])
        .output()
        .unwrap();
    assert_eq!(unreachable.status.code(), Some(3));

    let registry_path = client.state.join("remuda/cluster/authorized_nodes.json");
    let mut registry: Registry =
        serde_json::from_slice(&fs::read(&registry_path).unwrap()).unwrap();
    registry
        .authorized_nodes
        .iter_mut()
        .find(|entry| entry.node_fp == server.fingerprint())
        .unwrap()
        .state = NodeState::Revoked;
    fs::write(
        &registry_path,
        serde_json::to_vec_pretty(&registry).unwrap(),
    )
    .unwrap();
    let revoked = client
        .command()
        .args(["cluster", "call", &label, "list", "--addr", &address])
        .output()
        .unwrap();
    assert_eq!(revoked.status.code(), Some(4));
}

#[test]
fn cluster_call_uses_the_registry_endpoint_without_addr() {
    let client = Node::start("endpoint-client");
    let server = Node::start("endpoint-server");
    admit_pair(&client, &server);
    let (_listener, address) = listener(&server);
    set_endpoint(&client, &server.fingerprint(), address);
    start_proof_session(&server);
    let label = remuda_native::cluster::node_label(&server.fingerprint());

    let call = client
        .command()
        .args(["cluster", "call", &label, "list"])
        .output()
        .unwrap();
    assert!(
        call.status.success(),
        "registry endpoint fallback failed: {call:?}"
    );
    let stdout = String::from_utf8_lossy(&call.stdout);
    assert!(
        stdout.contains("proof"),
        "list did not reach the peer: {stdout}"
    );
}

#[test]
fn cluster_call_without_addr_reports_missing_endpoint() {
    let client = Node::start("no-endpoint-client");
    let server = Node::start("no-endpoint-server");
    admit_pair(&client, &server);
    let label = remuda_native::cluster::node_label(&server.fingerprint());

    let call = client
        .command()
        .args(["cluster", "call", &label, "list"])
        .output()
        .unwrap();
    assert_eq!(call.status.code(), Some(2), "{call:?}");
    let stderr = String::from_utf8_lossy(&call.stderr);
    assert!(
        stderr.contains("no registry endpoint"),
        "missing endpoint must be typed, not a usage error: {stderr}"
    );
}

#[test]
fn cluster_call_without_addr_keeps_typed_failures() {
    let client = Node::start("typed-client");
    let server = Node::start("typed-server");
    admit_pair(&client, &server);
    let label = remuda_native::cluster::node_label(&server.fingerprint());
    let run = |target: &str| {
        client
            .command()
            .args(["cluster", "call", target, "list"])
            .output()
            .unwrap()
    };

    let unknown = run("node-missing");
    assert_eq!(unknown.status.code(), Some(4), "{unknown:?}");
    assert!(
        String::from_utf8_lossy(&unknown.stderr).contains("no cluster node matches"),
        "{unknown:?}"
    );

    for target in [server.fingerprint(), client.fingerprint()] {
        let assigned = client
            .command()
            .args(["cluster", "alias", &target, "twin"])
            .output()
            .unwrap();
        assert!(assigned.status.success(), "alias failed: {assigned:?}");
    }
    let ambiguous = run("twin");
    assert_eq!(ambiguous.status.code(), Some(2), "{ambiguous:?}");
    assert!(
        String::from_utf8_lossy(&ambiguous.stderr).contains("ambiguous"),
        "{ambiguous:?}"
    );

    let path = client.state.join("remuda/cluster/authorized_nodes.json");
    let mut registry: Registry = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    registry
        .authorized_nodes
        .iter_mut()
        .find(|entry| entry.node_fp == server.fingerprint())
        .unwrap()
        .endpoint = Some("not-an-address".into());
    fs::write(&path, serde_json::to_vec_pretty(&registry).unwrap()).unwrap();
    let malformed = run(&label);
    assert_eq!(malformed.status.code(), Some(5), "{malformed:?}");
    assert!(
        String::from_utf8_lossy(&malformed.stderr).contains("invalid endpoint in cluster registry"),
        "{malformed:?}"
    );

    let refused_peer = fake_peer(|_| {
        FakeAnswer::Reply(remuda_core::protocol::Response::Error(
            "peer says no".into(),
        ))
    });
    let refused_fp = admit_peer(&client, &refused_peer.public, Some(refused_peer.address));
    let refused_label = remuda_native::cluster::node_label(&refused_fp);
    let refused = client
        .command()
        .args(["cluster", "call", &refused_label, "list"])
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(4), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("peer says no"),
        "{refused:?}"
    );

    let protocol_peer = fake_peer(|_| FakeAnswer::Raw(vec![0xde, 0xad, 0xbe, 0xef]));
    let protocol_fp = admit_peer(&client, &protocol_peer.public, Some(protocol_peer.address));
    let protocol_label = remuda_native::cluster::node_label(&protocol_fp);
    let protocol = client
        .command()
        .args(["cluster", "call", &protocol_label, "list"])
        .output()
        .unwrap();
    assert_eq!(protocol.status.code(), Some(5), "{protocol:?}");

    let path = client.state.join("remuda/cluster/authorized_nodes.json");
    let mut registry: Registry = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    registry
        .authorized_nodes
        .iter_mut()
        .find(|entry| entry.node_fp == server.fingerprint())
        .unwrap()
        .state = NodeState::Revoked;
    fs::write(&path, serde_json::to_vec_pretty(&registry).unwrap()).unwrap();
    let revoked = run(&label);
    assert_eq!(revoked.status.code(), Some(4), "{revoked:?}");
    assert!(
        String::from_utf8_lossy(&revoked.stderr).contains("revoked"),
        "{revoked:?}"
    );
}

#[test]
fn cluster_call_refuses_a_local_target_without_listing_it() {
    let client = Node::start("local-target-client");
    start_proof_session(&client);
    let fingerprint = client.fingerprint();
    let node_id = remuda_native::cluster::node_label(&fingerprint);
    let assigned = client
        .command()
        .args(["cluster", "alias", &fingerprint, "local-twin"])
        .output()
        .unwrap();
    assert!(assigned.status.success(), "alias failed: {assigned:?}");

    for target in [fingerprint.as_str(), node_id.as_str(), "local-twin"] {
        let call = client
            .command()
            .args(["cluster", "call", target, "list"])
            .output()
            .unwrap();
        assert_eq!(call.status.code(), Some(2), "{target}: {call:?}");
        let stderr = String::from_utf8_lossy(&call.stderr);
        assert!(stderr.contains("this node"), "{target}: {stderr}");
        assert!(stderr.contains("no local fallback"), "{target}: {stderr}");
        assert!(
            !String::from_utf8_lossy(&call.stdout).contains("proof"),
            "a local target must not list local sessions: {target}: {call:?}"
        );
    }
}

#[test]
fn support_probe_reports_supported_without_touching_the_gate() {
    let client = Node::start("support-client");
    let peer = fake_peer(|request| match request {
        remuda_core::protocol::Request::RemoteInputSupport => {
            FakeAnswer::Reply(remuda_core::protocol::Response::Ok)
        }
        other => panic!("support must not send {other:?}"),
    });
    let fingerprint = admit_peer(&client, &peer.public, Some(peer.address));
    let label = remuda_native::cluster::node_label(&fingerprint);
    let settings = client.state.join("remuda/cluster/settings.json");
    let before = fs::read(&settings).ok();

    let call = client
        .command()
        .args(["cluster", "call", &label, "support"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&call.stdout);
    assert!(call.status.success(), "support failed: {call:?}");
    assert!(stdout.contains("supported"), "{stdout}");
    assert!(stdout.contains("per-session"), "{stdout}");
    assert_eq!(fs::read(&settings).ok(), before, "support changed settings");

    let requests = peer.task.join().unwrap();
    assert_eq!(
        requests.len(),
        1,
        "support must be one request: {requests:?}"
    );
    assert!(matches!(
        requests[0],
        remuda_core::protocol::Request::RemoteInputSupport
    ));

    let json_peer = fake_peer(|_| FakeAnswer::Reply(remuda_core::protocol::Response::Ok));
    let json_fp = admit_peer(&client, &json_peer.public, Some(json_peer.address));
    let json_label = remuda_native::cluster::node_label(&json_fp);
    let call = client
        .command()
        .args(["cluster", "call", &json_label, "support", "--json"])
        .output()
        .unwrap();
    assert!(call.status.success(), "support --json failed: {call:?}");
    let value: serde_json::Value = serde_json::from_slice(&call.stdout).unwrap();
    assert_eq!(value, serde_json::json!({"support": "supported"}));
}

#[test]
fn support_probe_classifies_an_old_peer_without_retry() {
    let client = Node::start("old-client");
    let peer = fake_peer(|_| {
        FakeAnswer::Reply(remuda_core::protocol::Response::Error(
            "invalid remote request".into(),
        ))
    });
    let fingerprint = admit_peer(&client, &peer.public, Some(peer.address));
    let label = remuda_native::cluster::node_label(&fingerprint);

    let call = client
        .command()
        .args(["cluster", "call", &label, "support"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&call.stderr);
    assert_eq!(call.status.code(), Some(4), "{call:?}");
    assert!(stderr.contains("old peer"), "{stderr}");
    assert!(stderr.contains("upgrade"), "{stderr}");
    assert!(stderr.contains("not written"), "{stderr}");

    let requests = peer.task.join().unwrap();
    assert_eq!(
        requests.len(),
        1,
        "old peer must not be retried: {requests:?}"
    );

    let json_peer = fake_peer(|_| {
        FakeAnswer::Reply(remuda_core::protocol::Response::Error(
            "invalid remote request".into(),
        ))
    });
    let json_fp = admit_peer(&client, &json_peer.public, Some(json_peer.address));
    let json_label = remuda_native::cluster::node_label(&json_fp);
    let call = client
        .command()
        .args(["cluster", "call", &json_label, "support", "--json"])
        .output()
        .unwrap();
    assert_eq!(call.status.code(), Some(4), "{call:?}");
    let value: serde_json::Value = serde_json::from_slice(&call.stdout).unwrap();
    assert_eq!(value, serde_json::json!({"support": "old-peer"}));
}

#[test]
fn support_probe_classifies_refusal_and_transport() {
    let client = Node::start("refused-client");
    let refused =
        fake_peer(|_| FakeAnswer::Reply(remuda_core::protocol::Response::RemoteControlDisabled));
    let refused_fp = admit_peer(&client, &refused.public, Some(refused.address));
    let refused_label = remuda_native::cluster::node_label(&refused_fp);
    let call = client
        .command()
        .args(["cluster", "call", &refused_label, "support"])
        .output()
        .unwrap();
    assert_eq!(call.status.code(), Some(4), "{call:?}");
    assert!(
        String::from_utf8_lossy(&call.stderr).contains("refused"),
        "{call:?}"
    );
    assert_eq!(refused.task.join().unwrap().len(), 1);

    let refused_json =
        fake_peer(|_| FakeAnswer::Reply(remuda_core::protocol::Response::RemoteControlDisabled));
    let refused_json_fp = admit_peer(&client, &refused_json.public, Some(refused_json.address));
    let refused_json_label = remuda_native::cluster::node_label(&refused_json_fp);
    let call = client
        .command()
        .args(["cluster", "call", &refused_json_label, "support", "--json"])
        .output()
        .unwrap();
    assert_eq!(call.status.code(), Some(4), "{call:?}");
    let value: serde_json::Value = serde_json::from_slice(&call.stdout).unwrap();
    assert_eq!(value, serde_json::json!({"support": "refused"}));

    let closed = fake_peer(|_| FakeAnswer::Close);
    let closed_fp = admit_peer(&client, &closed.public, Some(closed.address));
    let closed_label = remuda_native::cluster::node_label(&closed_fp);
    let call = client
        .command()
        .args(["cluster", "call", &closed_label, "support"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&call.stderr);
    assert_eq!(call.status.code(), Some(3), "{call:?}");
    assert!(
        stderr.contains("timeout") || stderr.contains("unreachable"),
        "EOF is transport, but the contract's stable words are timeout/unreachable: {stderr}"
    );
    assert_eq!(closed.task.join().unwrap().len(), 1);

    // A peer that accepts and never answers must time out, not be mistaken for
    // a closed-only implementation.
    let stalled = fake_peer(|_| FakeAnswer::Stall(std::time::Duration::from_secs(7)));
    let stalled_fp = admit_peer(&client, &stalled.public, Some(stalled.address));
    let stalled_label = remuda_native::cluster::node_label(&stalled_fp);
    let call = client
        .command()
        .args(["cluster", "call", &stalled_label, "support", "--json"])
        .output()
        .unwrap();
    assert_eq!(call.status.code(), Some(3), "{call:?}");
    let stderr = String::from_utf8_lossy(&call.stderr);
    assert!(
        stderr.contains("timeout") || stderr.contains("unreachable"),
        "{stderr}"
    );
    let value: serde_json::Value = serde_json::from_slice(&call.stdout).unwrap();
    assert_eq!(value, serde_json::json!({"support": "timeout"}));
    assert_eq!(stalled.task.join().unwrap().len(), 1);
}

#[test]
fn mcp_ls_with_node_bounds_and_sanitizes_peer_rows() {
    let client = Node::start("mcp-peer-client");
    let peer = fake_peer(|request| match request {
        remuda_core::protocol::Request::List => {
            let mut rows = vec![
                fake_summary("evil\tname\u{1b}]0;title\u{7}", Some("inst-1")),
                fake_summary("bad-instance", Some("bad\u{1b}")),
                {
                    let mut row = fake_summary("disabled", Some("inst-disabled"));
                    row.remote_input_disabled = true;
                    row
                },
            ];
            for index in 0..300 {
                rows.push(fake_summary(&format!("s{index:03}"), Some("inst-ok")));
            }
            FakeAnswer::Reply(remuda_core::protocol::Response::Sessions(rows))
        }
        other => panic!("ls must send List, not {other:?}"),
    });
    let fingerprint = admit_peer(&client, &peer.public, Some(peer.address));

    let reply = mcp_call(&client, "ls", serde_json::json!({"node": fingerprint}));
    let text = mcp_text(&reply);
    assert!(!text.is_empty(), "peer list was empty: {reply}");
    assert!(
        !text.contains('\u{1b}'),
        "terminal controls leaked: {text:?}"
    );
    assert!(text.contains("instance_id=inst-1"), "{text}");
    assert!(text.contains("unpinnable=true"), "{text}");
    assert!(
        !text.contains("instance_id=bad"),
        "a partial/invalid instance id leaked: {text}"
    );
    assert!(text.contains("evil name"), "name was not sanitized: {text}");
    assert!(text.contains("list truncated"), "row bound missing: {text}");
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 257, "row bound must stop at 256 + marker");
    assert_eq!(lines.last().copied(), Some("... [list truncated]"));
    assert!(lines[0].starts_with("evil name"), "{}", lines[0]);
    assert!(lines[0].contains("instance_id=inst-1"), "{}", lines[0]);
    assert!(lines[1].starts_with("bad-instance"), "{}", lines[1]);
    assert!(lines[1].contains("unpinnable=true"), "{}", lines[1]);
    assert!(
        lines[2].ends_with("\tremote_input_disabled=true"),
        "{}",
        lines[2]
    );
    assert!(lines[3].starts_with("s000"), "{}", lines[3]);

    let requests = peer.task.join().unwrap();
    assert_eq!(requests.len(), 1, "ls must be one request: {requests:?}");
}

#[test]
fn mcp_ls_with_node_truncates_at_the_byte_bound() {
    let client = Node::start("mcp-byte-client");
    let peer = fake_peer(|request| match request {
        remuda_core::protocol::Request::List => {
            let rows = (0..150)
                .map(|index| {
                    fake_summary(&format!("{index:03}{}", "x".repeat(300)), Some("inst-byte"))
                })
                .collect();
            FakeAnswer::Reply(remuda_core::protocol::Response::Sessions(rows))
        }
        other => panic!("ls must send List, not {other:?}"),
    });
    let fingerprint = admit_peer(&client, &peer.public, Some(peer.address));

    let reply = mcp_call(&client, "ls", serde_json::json!({"node": fingerprint}));
    let text = mcp_text(&reply);
    assert!(
        text.len() <= 32 * 1024,
        "byte bound exceeded: {}",
        text.len()
    );
    assert!(
        text.len() > 30 * 1024,
        "test rows did not reach the bound: {}",
        text.len()
    );
    assert!(
        text.ends_with("... [list truncated]"),
        "byte truncation marker missing: {:?}",
        &text[text.len().saturating_sub(80)..]
    );
    for line in text.lines() {
        if line == "... [list truncated]" {
            continue;
        }
        assert!(
            line.contains("\tinstance_id=inst-byte"),
            "a partial or rewritten instance id leaked: {line}"
        );
    }
}

#[test]
fn mcp_ls_without_node_is_byte_identical() {
    let client = Node::start("mcp-local-client");
    start_proof_session(&client);

    let omitted = mcp_text(&mcp_call(&client, "ls", serde_json::json!({})));
    // Captured from the base implementation; the shape must not drift while
    // adding the optional node path.
    assert_eq!(
        omitted, "proof\t80x24\talive=true\tidle=0s\toutput_idle=0s",
        "local ls output changed from its base golden"
    );
    let fingerprint = client.fingerprint();
    let local_target = mcp_text(&mcp_call(
        &client,
        "ls",
        serde_json::json!({"node": fingerprint}),
    ));
    assert_eq!(
        omitted, local_target,
        "a node naming this node must use the byte-identical local path"
    );
}

#[test]
fn support_ok_does_not_open_the_mcp_send_gate() {
    let client = Node::start("gate-separation-client");
    let peer = fake_peer(|request| match request {
        remuda_core::protocol::Request::RemoteInputSupport => {
            FakeAnswer::Reply(remuda_core::protocol::Response::Ok)
        }
        other => panic!("support must not send {other:?}"),
    });
    let fingerprint = admit_peer(&client, &peer.public, Some(peer.address));
    let label = remuda_native::cluster::node_label(&fingerprint);

    let support = client
        .command()
        .args(["cluster", "call", &label, "support"])
        .output()
        .unwrap();
    assert!(support.status.success(), "support failed: {support:?}");

    // The MCP send gate is a separate local setting and stays off. A successful
    // probe must not be treated as authorization to type.
    let reply = mcp_call(
        &client,
        "send",
        serde_json::json!({"node": label, "session": "agent", "text": "hello"}),
    );
    assert_eq!(reply["result"]["isError"], true, "{reply}");
    let text = mcp_text(&reply);
    assert!(
        text.contains("remote send is disabled"),
        "support leaked into the MCP gate: {text}"
    );

    let requests = peer.task.join().unwrap();
    assert_eq!(
        requests,
        vec![remuda_core::protocol::Request::RemoteInputSupport],
        "a refused send must not reach the peer"
    );
}
