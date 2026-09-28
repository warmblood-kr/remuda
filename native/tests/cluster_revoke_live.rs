//! Live revocation coverage across two isolated cluster daemons.

#![cfg(unix)]
// This integration test deliberately exercises the real loopback cluster wire.
#![allow(clippy::disallowed_types)]

use remuda_core::protocol::{Request, Response};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use zeroize::Zeroizing;

struct PrivateNode {
    child: Child,
    root: PathBuf,
    runtime: PathBuf,
    name: String,
}

impl PrivateNode {
    fn start(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!("cr{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let runtime = root.join("runtime");
        let home = root.join("home");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let name = format!("cluster-revoke-{label}-{}", std::process::id());
        let child = Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", &name, "daemon"])
            .env("REMUDA_RUNTIME_DIR", &runtime)
            .env("HOME", &home)
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
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
fn revoking_a_live_member_refuses_its_next_list_and_new_connections() {
    let a = PrivateNode::start("a");
    let b = PrivateNode::start("b");
    successful(a.run(&["cluster", "init"]), "initialize node A");
    successful(b.run(&["cluster", "init"]), "initialize node B");
    let a_identity = a.identity();
    let b_identity = b.identity();
    let b_fingerprint = identity_fingerprint(&b_identity);

    let listener = ListenerProcess::start(&a);
    let address = listener.address.to_string();
    let invite = successful(
        a.run(&["cluster", "invite", "--bind", &address]),
        "mint join invitation",
    );
    let join_line = invite.lines().nth(1).unwrap();
    let pin_mismatch = b.run(&["cluster", "join", "SHA256:wrong", join_line]);
    assert!(!pin_mismatch.status.success());
    let mismatch_error = String::from_utf8_lossy(&pin_mismatch.stderr);
    assert!(
        mismatch_error.contains("expected SHA256:wrong"),
        "{mismatch_error}"
    );
    assert!(
        mismatch_error.contains(&identity_fingerprint(&a_identity)),
        "{mismatch_error}"
    );
    successful(
        b.run(&[
            "cluster",
            "join",
            &identity_fingerprint(&a_identity),
            join_line,
        ]),
        "join node B to node A",
    );

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
        "node B's next List request",
    );
    // Every remote frame uses a fresh TCP connection. Repeat to make sure a
    // reconnect is refused immediately too, rather than relying on old state.
    assert_list_refused(
        &b_identity,
        &a_identity,
        listener.address,
        "node B's new connection",
    );

    let table = successful(a.run(&["cluster", "nodes"]), "read node A's nodes table");
    assert!(table.contains(&b_fingerprint), "{table}");
    let row = table
        .lines()
        .find(|line| line.contains(&b_fingerprint))
        .expect("node B row in nodes table");
    assert!(row.contains("revoked"), "node B row is not revoked: {row}");
}
