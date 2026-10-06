#![cfg(unix)]
#![allow(clippy::disallowed_types)] // Loopback sockets are test-only fixtures for the network boundary.

use remuda_core::clock::ManualWallClock;
use remuda_core::protocol::{Request, Response};
use remuda_native::cluster::{encoding, AuthorizedNode, NodeState, Registry};
use remuda_native::net::cluster_client::{ClientError, ClientTimeouts, ClusterClient};
use std::fs;
use std::io::{BufRead, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct PrivateNode {
    root: PathBuf,
    runtime: PathBuf,
    state: PathBuf,
    name: String,
    private: Vec<u8>,
    public: Vec<u8>,
    daemon: Child,
}

impl PrivateNode {
    fn start(label: &str) -> Self {
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("rcc-{}-{serial}", std::process::id()));
        let runtime = root.join("r");
        let state = root.join("s");
        let home = root.join("h");
        fs::create_dir_all(&runtime).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&home).unwrap();
        let name = format!("{}-{serial}", &label[..1]);
        let daemon = node_command(&name, &runtime, &root, &state, &home)
            .args(["daemon"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn isolated daemon");
        let mut node = Self {
            root,
            runtime,
            state,
            name,
            private: Vec::new(),
            public: Vec::new(),
            daemon,
        };
        node.wait_ready();
        let init = node_command(&node.name, &node.runtime, &node.root, &node.state, &home)
            .args(["cluster", "init", "--no-listen"])
            .output()
            .expect("run cluster init in isolated state");
        assert!(init.status.success(), "cluster init failed: {init:?}");
        let key = fs::read(node.state.join("remuda/cluster/identity.key")).unwrap();
        assert_eq!(key.len(), 64);
        node.private = key[..32].to_vec();
        node.public = key[32..].to_vec();
        node
    }

    fn wait_ready(&mut self) {
        let path = remuda_native::daemon::socket_path_in(&self.runtime, &self.name);
        let deadline = Instant::now() + Duration::from_secs(10);
        while remuda_native::client::request(&path, &Request::List).is_err() {
            if let Some(status) = self.daemon.try_wait().unwrap() {
                panic!("isolated daemon exited before ready: {status}");
            }
            assert!(Instant::now() < deadline, "isolated daemon did not bind");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn child_command(&self) -> Command {
        let home = self.root.join("home");
        node_command(&self.name, &self.runtime, &self.root, &self.state, &home)
    }

    fn fingerprint(&self) -> String {
        encoding::fingerprint(&self.public)
    }
}

impl Drop for PrivateNode {
    fn drop(&mut self) {
        assert!(
            self.runtime.starts_with(&self.root),
            "only stop a daemon in scratch runtime"
        );
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct ListenerProcess {
    child: Child,
    root: PathBuf,
    runtime: PathBuf,
}

impl Drop for ListenerProcess {
    fn drop(&mut self) {
        assert!(
            self.runtime.starts_with(&self.root),
            "listener runtime must be scratch"
        );
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn node_command(name: &str, runtime: &Path, root: &Path, state: &Path, home: &Path) -> Command {
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

fn pair() -> (PrivateNode, PrivateNode) {
    let client = PrivateNode::start("client");
    let server = PrivateNode::start("server");
    let mut registry = Registry::default();
    for node in [&server, &client] {
        registry.authorized_nodes.push(AuthorizedNode {
            node_fp: node.fingerprint(),
            static_pubkey: encoding::encode_base64(&node.public),
            delivered_by: None,
            format_major: remuda_native::cluster::registry::REGISTRY_FORMAT_MAJOR,
            format_minor: remuda_native::cluster::registry::REGISTRY_FORMAT_MINOR,
            optional_fields: std::collections::BTreeMap::new(),
            endpoint: None,
            state: NodeState::Admitted,
            version: 1,
            by: server.fingerprint(),
        });
    }
    fs::write(
        server.state.join("remuda/cluster/authorized_nodes.json"),
        serde_json::to_vec_pretty(&registry).unwrap(),
    )
    .unwrap();
    (client, server)
}

fn start_listener(server: &PrivateNode) -> (ListenerProcess, SocketAddr) {
    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let bind = address.to_string();
    let child = server
        .child_command()
        .args(["cluster", "listen", "--bind", &bind])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start PR7 listener on loopback");
    let mut process = ListenerProcess {
        child,
        root: server.root.clone(),
        runtime: server.runtime.clone(),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match TcpStream::connect(address) {
            Ok(stream) => {
                drop(stream);
                break;
            }
            Err(_) => {
                if let Some(status) = process.child.try_wait().unwrap() {
                    panic!("PR7 listener exited before ready: {status}");
                }
                assert!(Instant::now() < deadline, "PR7 listener did not bind");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    (process, address)
}

fn client() -> ClusterClient {
    ClusterClient::system()
}

fn request(
    client_node: &PrivateNode,
    peer: SocketAddr,
    responder: &[u8],
    request: &Request,
) -> Result<Response, ClientError> {
    client().request(peer, responder, &client_node.private, request)
}

#[test]
fn client_list_succeeds_between_private_daemons() {
    let (client_node, server) = pair();
    let (_listener, address) = start_listener(&server);
    let result = request(&client_node, address, &server.public, &Request::List);
    assert!(
        matches!(result, Ok(Response::Sessions(_))),
        "unexpected result: {result:?}"
    );
}

#[test]
fn client_wrong_pinned_key_fails_as_crypto() {
    let (client_node, server) = pair();
    let (_listener, address) = start_listener(&server);
    let (proxy_address, captured) = capture_proxy(address);
    let wrong = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
        .generate_keypair()
        .unwrap();
    let input = Request::Input {
        name: "dev".into(),
        instance_id: "instance".into(),
        client_id: "00000000000000000000000000000001".into(),
        seq: 1,
        bytes: b"plaintext-client-secret\r".to_vec(),
    };
    let result = request(&client_node, proxy_address, &wrong.public, &input);
    let wire_request = captured.join().unwrap();
    assert!(
        matches!(result, Err(ClientError::Crypto)),
        "unexpected result: {result:?}"
    );
    assert!(
        !wire_request
            .windows(b"plaintext-client-secret".len())
            .any(|window| window == b"plaintext-client-secret"),
        "request payload was visible to a responder using a different key"
    );
}

#[test]
fn client_revoked_peer_is_refused() {
    let (client_node, server) = pair();
    let path = server.state.join("remuda/cluster/authorized_nodes.json");
    let mut registry: Registry = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    registry
        .authorized_nodes
        .iter_mut()
        .find(|entry| entry.node_fp == client_node.fingerprint())
        .unwrap()
        .state = NodeState::Revoked;
    fs::write(&path, serde_json::to_vec_pretty(&registry).unwrap()).unwrap();
    let (_listener, address) = start_listener(&server);
    let result = request(&client_node, address, &server.public, &Request::List);
    assert!(
        matches!(result, Err(ClientError::Refused(403))),
        "unexpected result: {result:?}"
    );
}

fn test_server(
    response: Vec<u8>,
    hold: Duration,
) -> (SocketAddr, snow::Keypair, std::thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let responder = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
        .generate_keypair()
        .unwrap();
    let task = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let _ = stream.read_to_end(&mut request);
        if !hold.is_zero() {
            std::thread::sleep(hold);
        } else {
            stream.write_all(&response).unwrap();
        }
        request
    });
    (address, responder, task)
}

fn http_response(body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

fn read_request_body(stream: &TcpStream) -> Vec<u8> {
    let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
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

fn capture_proxy(target: SocketAddr) -> (SocketAddr, std::thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let task = std::thread::spawn(move || {
        let (mut frontend, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        frontend.read_to_end(&mut request).unwrap();
        let mut backend = TcpStream::connect(target).unwrap();
        backend.write_all(&request).unwrap();
        backend.shutdown(std::net::Shutdown::Write).unwrap();
        let mut response = Vec::new();
        backend.read_to_end(&mut response).unwrap();
        frontend.write_all(&response).unwrap();
        request
    });
    (address, task)
}

fn short_client() -> ClusterClient {
    ClusterClient::with_timeouts(
        std::sync::Arc::new(ManualWallClock::new(1_800_000_000)),
        ClientTimeouts {
            connect: Duration::from_millis(100),
            read: Duration::from_millis(100),
            total: Duration::from_millis(100),
        },
    )
}

fn response_test_client() -> ClusterClient {
    ClusterClient::with_timeouts(
        std::sync::Arc::new(ManualWallClock::new(1_800_000_000)),
        ClientTimeouts {
            connect: Duration::from_secs(2),
            read: Duration::from_secs(5),
            total: Duration::from_secs(5),
        },
    )
}

#[test]
fn client_obeys_total_timeout() {
    let (address, responder, task) = test_server(Vec::new(), Duration::from_millis(400));
    let initiator = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
        .generate_keypair()
        .unwrap();
    let started = Instant::now();
    let result = short_client().request(
        address,
        &responder.public,
        &initiator.private,
        &Request::List,
    );
    let elapsed = started.elapsed();
    let _ = task.join().unwrap();
    assert!(matches!(result, Err(ClientError::Timeout)));
    assert!(
        elapsed < Duration::from_millis(350),
        "timeout exceeded total deadline: {elapsed:?}"
    );
}

#[test]
fn client_rejects_plaintext_200_as_crypto() {
    let (address, responder, task) =
        test_server(http_response(br#"{"Sessions":[]}"#), Duration::ZERO);
    let initiator = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
        .generate_keypair()
        .unwrap();
    let result = response_test_client().request(
        address,
        &responder.public,
        &initiator.private,
        &Request::List,
    );
    task.join().unwrap();
    assert!(matches!(result, Err(ClientError::Crypto)), "{result:?}");
}

#[test]
fn client_rejects_a_tampered_noise_message_two() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let responder = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
        .generate_keypair()
        .unwrap();
    let private = responder.private.clone();
    let task = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_request_body(&stream);
        let opened = remuda_native::net::frame::open_request(&private, &request).unwrap();
        let payload = serde_json::to_vec(&Response::Sessions(Vec::new())).unwrap();
        let mut body = remuda_native::net::frame::seal_response(opened, &payload).unwrap();
        *body.last_mut().unwrap() ^= 1;
        stream.write_all(&http_response(&body)).unwrap();
    });
    let initiator = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
        .generate_keypair()
        .unwrap();
    let result = response_test_client().request(
        address,
        &responder.public,
        &initiator.private,
        &Request::List,
    );
    task.join().unwrap();
    assert!(matches!(result, Err(ClientError::Crypto)), "{result:?}");
}

#[test]
fn client_rejects_a_low_order_pinned_key_before_connecting() {
    let initiator = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
        .generate_keypair()
        .unwrap();
    let address = "127.0.0.1:9".parse().unwrap();
    let result = short_client().request(address, &[0; 32], &initiator.private, &Request::List);
    assert!(matches!(result, Err(ClientError::Crypto)), "{result:?}");
}

#[test]
fn client_reports_a_peer_that_is_not_listening() {
    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let responder = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
        .generate_keypair()
        .unwrap();
    let initiator = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
        .generate_keypair()
        .unwrap();
    let result = short_client().request(
        address,
        &responder.public,
        &initiator.private,
        &Request::List,
    );
    assert!(matches!(result, Err(ClientError::PeerNotListening)));
}

#[test]
fn client_uses_the_injected_wall_clock_for_msg1() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let responder = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
        .generate_keypair()
        .unwrap();
    let responder_private = responder.private.clone();
    let task = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
        let mut content_length = 0usize;
        loop {
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).unwrap();
            if line == b"\r\n" || line == b"\n" {
                break;
            }
            if let Ok(text) = std::str::from_utf8(&line) {
                if let Some((_, value)) = text.split_once(':') {
                    if text.to_ascii_lowercase().starts_with("content-length:") {
                        content_length = value.trim().parse().unwrap();
                    }
                }
            }
        }
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body).unwrap();
        let opened = remuda_native::net::frame::open_request(&responder_private, &body).unwrap();
        let timestamp = opened.timestamp_seconds;
        let response = serde_json::to_vec(&Response::Sessions(Vec::new())).unwrap();
        let encrypted = remuda_native::net::frame::seal_response(opened, &response).unwrap();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            encrypted.len()
        )
        .unwrap();
        stream.write_all(&encrypted).unwrap();
        timestamp
    });
    let clock = std::sync::Arc::new(ManualWallClock::new(123_456));
    let client = ClusterClient::new(clock);
    let initiator = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
        .generate_keypair()
        .unwrap();
    let result = client.request(
        address,
        &responder.public,
        &initiator.private,
        &Request::List,
    );
    assert!(
        matches!(result, Ok(Response::Sessions(_))),
        "unexpected result: {result:?}"
    );
    assert_eq!(task.join().unwrap(), 123_456);
}

#[test]
fn client_rejects_oversized_response() {
    let (address, responder, task) = test_server(
        b"HTTP/1.1 200 OK\r\nContent-Length: 65536\r\nConnection: close\r\n\r\n".to_vec(),
        Duration::ZERO,
    );
    let initiator = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
        .generate_keypair()
        .unwrap();
    let result = response_test_client().request(
        address,
        &responder.public,
        &initiator.private,
        &Request::List,
    );
    let _ = task.join().unwrap();
    assert!(
        matches!(result, Err(ClientError::BadResponse)),
        "unexpected result: {result:?}"
    );
}

#[test]
fn client_rejects_a_real_oversized_response_body_before_reading_it() {
    let mut response =
        b"HTTP/1.1 200 OK\r\nContent-Length: 70000\r\nConnection: close\r\n\r\n".to_vec();
    response.extend(std::iter::repeat_n(b'x', 70_000));
    let (address, responder, task) = test_server(response, Duration::ZERO);
    let initiator = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
        .generate_keypair()
        .unwrap();
    let result = response_test_client().request(
        address,
        &responder.public,
        &initiator.private,
        &Request::List,
    );
    task.join().unwrap();
    assert!(
        matches!(result, Err(ClientError::BadResponse)),
        "{result:?}"
    );
}

#[test]
fn client_total_deadline_wins_over_a_slow_drip_below_the_read_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let responder = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
        .generate_keypair()
        .unwrap();
    let task = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        stream.read_to_end(&mut request).unwrap();
        for byte in b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n" {
            if stream.write_all(&[*byte]).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    let initiator = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
        .generate_keypair()
        .unwrap();
    let client = ClusterClient::with_timeouts(
        std::sync::Arc::new(ManualWallClock::new(1_800_000_000)),
        ClientTimeouts {
            connect: Duration::from_millis(100),
            read: Duration::from_millis(200),
            total: Duration::from_millis(600),
        },
    );
    let started = Instant::now();
    let result = client.request(
        address,
        &responder.public,
        &initiator.private,
        &Request::List,
    );
    let elapsed = started.elapsed();
    task.join().unwrap();
    assert!(matches!(result, Err(ClientError::Timeout)), "{result:?}");
    assert!(
        (Duration::from_millis(500)..Duration::from_millis(900)).contains(&elapsed),
        "deadline elapsed {elapsed:?}"
    );
}

/// Fake peer: reads the request (returned for header checks), then streams `respond(opened)` in small pieces.
fn streaming_server(
    pace: Duration,
    respond: impl FnOnce(remuda_native::net::frame::OpenedRequest) -> Vec<u8> + Send + 'static,
) -> (SocketAddr, snow::Keypair, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let responder = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
        .generate_keypair()
        .unwrap();
    let private = responder.private.clone();
    let task = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut raw = Vec::new();
        let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
        let mut head = String::new();
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = v.trim().parse().unwrap();
            }
            head.push_str(&line);
        }
        raw.resize(length, 0);
        reader.read_exact(&mut raw).unwrap();
        let opened = remuda_native::net::frame::open_request(&private, &raw).unwrap();
        let body = respond(opened);
        let _ = stream.write_all(&http_response_head(body.len()));
        for piece in body.chunks(7_000) {
            if stream.write_all(piece).is_err() {
                break;
            }
            if !pace.is_zero() {
                std::thread::sleep(pace);
            }
        }
        head
    });
    (address, responder, task)
}

fn http_response_head(length: usize) -> Vec<u8> {
    format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n").into_bytes()
}

fn call(
    client: &ClusterClient,
    address: SocketAddr,
    responder: &snow::Keypair,
) -> Result<Response, ClientError> {
    let initiator = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
        .generate_keypair()
        .unwrap();
    client.request(
        address,
        &responder.public,
        &initiator.private,
        &Request::List,
    )
}

fn big_sessions_json(min_len: usize) -> Vec<u8> {
    let mut json = br#"{"Error":""#.to_vec();
    json.resize(min_len, b'a');
    json.extend_from_slice(b"\"}");
    json
}

#[test]
fn client_sends_chunked_header_and_decodes_v1_reply() {
    let (address, responder, task) = streaming_server(Duration::ZERO, |opened| {
        remuda_native::net::frame::seal_response(opened, br#"{"Sessions":[]}"#).unwrap()
    });
    let result = call(&response_test_client(), address, &responder);
    let head = task.join().unwrap();
    assert!(head.contains("X-Remuda-Chunked: 1\r\n"), "{head}");
    assert!(matches!(result, Ok(Response::Sessions(_))), "{result:?}");
}

#[test]
fn client_reads_v2_replies_of_1mib_and_4mib_streamed_in_pieces() {
    for len in [1 << 20, 4 << 20] {
        let (address, responder, task) = streaming_server(Duration::ZERO, move |opened| {
            remuda_native::net::frame::seal_response_chunked(opened, &big_sessions_json(len - 2))
                .unwrap()
        });
        let result = call(&response_test_client(), address, &responder);
        task.join().unwrap();
        assert!(
            matches!(&result, Ok(Response::Error(e)) if e.len() == len - 2 - 10),
            "len {len}: {:?}",
            result.as_ref().map(|_| ())
        );
    }
}

#[test]
fn client_v2_slow_loris_body_hits_the_total_deadline() {
    let (address, responder, task) = streaming_server(Duration::from_millis(100), |opened| {
        remuda_native::net::frame::seal_response_chunked(opened, &big_sessions_json(1 << 20))
            .unwrap()
    });
    let client = ClusterClient::with_timeouts(
        std::sync::Arc::new(ManualWallClock::new(1_800_000_000)),
        ClientTimeouts {
            connect: Duration::from_millis(200),
            read: Duration::from_millis(300),
            total: Duration::from_millis(700),
        },
    );
    let started = Instant::now();
    let result = call(&client, address, &responder);
    let elapsed = started.elapsed();
    task.join().unwrap();
    assert!(matches!(result, Err(ClientError::Timeout)), "{result:?}");
    assert!(elapsed < Duration::from_millis(1_200), "{elapsed:?}");
}

#[test]
fn client_input_is_refused_by_listener() {
    let (client_node, server) = pair();
    let (_listener, address) = start_listener(&server);
    let input = Request::Input {
        name: "missing".into(),
        instance_id: "instance".into(),
        client_id: "00000000000000000000000000000001".into(),
        seq: 1,
        bytes: b"secret\r".to_vec(),
    };
    let result = request(&client_node, address, &server.public, &input);
    assert!(
        matches!(result, Ok(Response::Error(_))),
        "unexpected result: {result:?}"
    );
}
