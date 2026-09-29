#![cfg(unix)]
#![allow(clippy::disallowed_types)]

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
        let init = command(&name, &runtime, &root, &state, &home)
            .args(["cluster", "init"])
            .output()
            .unwrap();
        assert!(init.status.success(), "cluster init failed: {init:?}");
        let key = fs::read(state.join("remuda/cluster/identity.key")).unwrap();
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
