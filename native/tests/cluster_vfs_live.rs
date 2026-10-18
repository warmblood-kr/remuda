//! Live coverage of the cluster virtual file store (#742) across isolated
//! daemons on loopback: put on one node, read on another, delete everywhere,
//! and re-replicate after a holder is lost.

#![cfg(unix)]
// This integration test deliberately exercises the real loopback cluster wire.
#![allow(clippy::disallowed_types)]

use remuda_core::protocol::Request;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static NEXT_NODE_ID: AtomicUsize = AtomicUsize::new(0);
static LIVE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const SYNC_INTERVAL_MS: u64 = 200;
const DOWN_GRACE_MS: u64 = 600;

fn live_test_guard() -> std::sync::MutexGuard<'static, ()> {
    LIVE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Node {
    child: Option<Child>,
    root: PathBuf,
    runtime: PathBuf,
    name: String,
    address: SocketAddr,
}

impl Node {
    fn start() -> Self {
        let id = NEXT_NODE_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("cv{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let runtime = root.join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = probe.local_addr().unwrap();
        drop(probe);
        let mut node = Self {
            child: None,
            root,
            runtime,
            name: format!("v{}-{id}", std::process::id()),
            address,
        };
        node.spawn_daemon();
        successful(
            node.run(&["cluster", "init", "--no-listen"]),
            "cluster init",
        );
        node.enable_listener();
        node
    }

    fn env(&self, command: &mut Command) {
        let root = &self.root;
        command
            .env("REMUDA_RUNTIME_DIR", &self.runtime)
            .env("HOME", root.join("home"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("REMUDA_NO_UPDATE_CHECK", "1")
            .env(
                "REMUDA_TEST_CLUSTER_SYNC_INTERVAL_MS",
                SYNC_INTERVAL_MS.to_string(),
            )
            .env("REMUDA_TEST_VFS_DOWN_GRACE_MS", DOWN_GRACE_MS.to_string());
    }

    fn spawn_daemon(&mut self) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_remuda"));
        command.args(["-s", &self.name, "daemon"]);
        self.env(&mut command);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        self.child = Some(command.spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(10);
        while remuda_native::client::request(&self.daemon_path(), &Request::List).is_err() {
            assert!(Instant::now() < deadline, "private daemon did not start");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn daemon_path(&self) -> PathBuf {
        remuda_native::daemon::socket_path_in(&self.runtime, &self.name)
    }

    fn enable_listener(&self) {
        use remuda_native::cluster::listener_config::{self, ListenerBind, ListenerConfig};
        listener_config::write_at(
            &self.root.join("state/remuda/cluster"),
            &ListenerConfig {
                enabled: true,
                bind: ListenerBind::Explicit(self.address),
                allow_public: false,
            },
        )
        .unwrap();
        remuda_native::client::request(
            &self.daemon_path(),
            &Request::ClusterListener(remuda_core::protocol::ListenerOp::Reload),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect_timeout(&self.address, Duration::from_millis(50)).is_err() {
            assert!(Instant::now() < deadline, "cluster listener did not start");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Kill the daemon, which also closes its cluster listener.
    fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_stdin(args, None)
    }

    fn run_with_stdin(&self, args: &[&str], stdin: Option<&[u8]>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_remuda"));
        command.args(["-s", &self.name]).args(args);
        self.env(&mut command);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        command.stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        let mut child = command.spawn().unwrap();
        if let Some(bytes) = stdin {
            use std::io::Write;
            child.stdin.take().unwrap().write_all(bytes).unwrap();
        }
        child.wait_with_output().unwrap()
    }

    fn fingerprint(&self) -> String {
        let key = remuda_native::cluster::identity::load_keypair_at(
            &self.root.join("state/remuda/cluster"),
        )
        .unwrap();
        remuda_native::cluster::encoding::fingerprint(&key[32..])
    }

    fn has_blob(&self, sha256: &str) -> bool {
        self.root
            .join("state/remuda/cluster/vfs/blobs")
            .join(&sha256[..2])
            .join(sha256)
            .is_file()
    }

    fn stat_json(&self, uri: &str) -> Option<serde_json::Value> {
        let output = self.run(&["vfs", "stat", uri, "--json"]);
        output
            .status
            .success()
            .then(|| serde_json::from_slice(&output.stdout).unwrap())
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.kill();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn successful(output: Output, operation: &str) -> String {
    assert!(
        output.status.success(),
        "{operation} failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn invite_args(output: &str) -> (String, String) {
    let command = output
        .lines()
        .find(|line| line.starts_with("  remuda cluster join "))
        .expect("printed join command")
        .trim()
        .strip_prefix("remuda cluster join '")
        .and_then(|command| command.strip_suffix('\''))
        .expect("shell-quoted join command")
        .to_owned();
    let (fingerprint, line) = command.split_once("' '").unwrap();
    (fingerprint.to_owned(), line.to_owned())
}

fn join(issuer: &Node, joiner: &Node) {
    let invite = successful(
        issuer.run(&["cluster", "invite", "--bind", &issuer.address.to_string()]),
        "cluster invite",
    );
    let (fingerprint, line) = invite_args(&invite);
    successful(
        joiner.run(&[
            "cluster",
            "join",
            &fingerprint,
            &line,
            "--bind",
            &joiner.address.to_string(),
        ]),
        "cluster join",
    );
}

fn registry_endpoints(node: &Node) -> usize {
    let registry: remuda_native::cluster::Registry = serde_json::from_slice(
        &std::fs::read(node.root.join("state/remuda/cluster/authorized_nodes.json")).unwrap(),
    )
    .unwrap();
    registry
        .authorized_nodes
        .iter()
        .filter(|entry| entry.endpoint.is_some())
        .count()
}

/// Three joined nodes that all know every endpoint.
fn three_nodes() -> (Node, Node, Node) {
    let a = Node::start();
    let b = Node::start();
    let c = Node::start();
    join(&a, &b);
    join(&b, &c);
    wait_until(
        "every node knows three endpoints",
        Duration::from_secs(20),
        || {
            [&a, &b, &c]
                .iter()
                .all(|node| registry_endpoints(node) == 3)
        },
    );
    (a, b, c)
}

fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        thread::sleep(Duration::from_millis(100));
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    remuda_native::cluster::vfs::hex(&sha2::Sha256::digest(bytes))
}

#[test]
fn put_on_one_node_reads_on_every_node_and_delete_reaches_all() {
    let _serial = live_test_guard();
    let (a, b, c) = three_nodes();
    let body = b"cluster guidance v1\n";
    let put = successful(
        a.run_with_stdin(
            &[
                "vfs",
                "put",
                "-",
                "remuda:///butler/guidance/cluster.md",
                "--replicas",
                "2",
                "--wait",
            ],
            Some(body),
        ),
        "vfs put",
    );
    assert!(put.contains("2 copies confirmed"), "{put}");

    // The name reaches every node, and every node can read the content,
    // fetching it from a holder when it has no copy.
    for node in [&b, &c] {
        wait_until("index entry reaches peer", Duration::from_secs(10), || {
            node.stat_json("remuda:///butler/guidance/cluster.md")
                .is_some()
        });
        let got = node.run(&["vfs", "get", "remuda:///butler/guidance/cluster.md"]);
        assert_eq!(successful(got, "vfs get"), String::from_utf8_lossy(body));
    }
    let stat = c.stat_json("remuda:///butler/guidance/cluster.md").unwrap();
    assert_eq!(stat["sha256"], sha256_hex(body));
    assert_eq!(stat["replicas"], 2);

    // Lua reads the local copy only.
    let lua = successful(
        c.run(&[
            "-e",
            "local s = remuda.vfs.stat('remuda:///butler/guidance/cluster.md'); return s.version .. ' ' .. tostring(s['local']) .. ' ' .. remuda.vfs.read('remuda:///butler/guidance/cluster.md')",
        ]),
        "lua vfs",
    );
    assert!(lua.contains("true cluster guidance v1"), "{lua}");
    let missing = successful(
        c.run(&[
            "-e",
            "local v, why = remuda.vfs.stat('remuda:///nope'); return why",
        ]),
        "lua missing",
    );
    assert!(missing.starts_with("not_found:"), "{missing}");

    // Listing by prefix.
    let listed = successful(b.run(&["vfs", "ls", "remuda:///butler/"]), "vfs ls");
    assert!(listed.contains("butler/guidance/cluster.md"), "{listed}");

    // A newer write from another node wins everywhere.
    successful(
        b.run_with_stdin(
            &["vfs", "put", "-", "remuda:///butler/guidance/cluster.md"],
            Some(b"v2"),
        ),
        "second put",
    );
    wait_until("newer version reaches A", Duration::from_secs(10), || {
        a.stat_json("remuda:///butler/guidance/cluster.md")
            .is_some_and(|stat| stat["sha256"] == sha256_hex(b"v2"))
    });

    // Delete on C reaches A and B.
    successful(
        c.run(&["vfs", "rm", "remuda:///butler/guidance/cluster.md"]),
        "vfs rm",
    );
    for node in [&a, &b] {
        wait_until("tombstone reaches peer", Duration::from_secs(10), || {
            node.stat_json("remuda:///butler/guidance/cluster.md")
                .is_none()
        });
    }
}

#[test]
fn losing_a_holder_makes_the_next_node_take_a_copy() {
    let _serial = live_test_guard();
    let (a, b, c) = three_nodes();
    let fingerprints = [a.fingerprint(), b.fingerprint(), c.fingerprint()];
    let candidates: Vec<&str> = fingerprints.iter().map(String::as_str).collect();
    // The writer always keeps its copy, so pick content whose placement
    // includes the writer A. The outsider then starts with no copy.
    let (body, sha, placement) = (0..)
        .map(|attempt| format!("survives a lost node #{attempt}").into_bytes())
        .map(|body| {
            let sha = sha256_hex(&body);
            let placement: Vec<String> = remuda_core::vfs::placement(&sha, 2, &candidates)
                .into_iter()
                .map(str::to_owned)
                .collect();
            (body, sha, placement)
        })
        .find(|(_, _, placement)| placement.contains(&fingerprints[0]))
        .unwrap();
    successful(
        a.run_with_stdin(
            &[
                "vfs",
                "put",
                "-",
                "remuda:///shared/report.txt",
                "--replicas",
                "2",
                "--wait",
            ],
            Some(&body),
        ),
        "vfs put",
    );
    let mut nodes = [Some(a), Some(b), Some(c)];
    let index_of = |fp: &str| fingerprints.iter().position(|known| known == fp).unwrap();
    let outsider = (0..3)
        .find(|index| !placement.contains(&fingerprints[*index]))
        .unwrap();
    for holder in &placement {
        let node = nodes[index_of(holder)].as_ref().unwrap();
        wait_until(
            "placement node holds a copy",
            Duration::from_secs(10),
            || node.has_blob(&sha),
        );
    }

    // Several repair passes later the outsider still holds no copy: it is
    // not in the placement while every holder is up.
    thread::sleep(Duration::from_millis(SYNC_INTERVAL_MS * 5));
    assert!(!nodes[outsider].as_ref().unwrap().has_blob(&sha));

    // Lose one holder for good.
    let lost = index_of(&placement[0]);
    nodes[lost].as_mut().unwrap().kill();
    let survivor = index_of(&placement[1]);

    // After the grace period the outsider counts the lost node as down,
    // joins the placement, and pulls a copy from the survivor.
    let outsider_node = nodes[outsider].as_ref().unwrap();
    wait_until(
        "outsider takes over a copy",
        Duration::from_secs(20),
        || outsider_node.has_blob(&sha),
    );
    assert!(nodes[survivor].as_ref().unwrap().has_blob(&sha));
    let got = successful(
        outsider_node.run(&["vfs", "get", "remuda:///shared/report.txt", "--local"]),
        "local get on new holder",
    );
    assert_eq!(got.as_bytes(), &body[..]);
}

#[test]
fn clear_errors_for_wrong_cluster_missing_and_no_cluster() {
    let _serial = live_test_guard();
    let a = Node::start();
    let wrong = a.run(&["vfs", "get", "remuda://elsewhere/x"]);
    assert!(!wrong.status.success());
    let text = String::from_utf8_lossy(&wrong.stderr);
    assert!(
        text.contains("'elsewhere'") && text.contains("'default'"),
        "{text}"
    );
    let missing = a.run(&["vfs", "get", "remuda:///x"]);
    assert!(String::from_utf8_lossy(&missing.stderr).contains("no file at x"));

    successful(a.run(&["vfs", "config", "--cluster", "home"]), "vfs config");
    successful(
        a.run_with_stdin(&["vfs", "put", "-", "remuda://home/x"], Some(b"1")),
        "named-cluster put",
    );
    assert_eq!(
        successful(a.run(&["vfs", "get", "remuda:///x"]), "get"),
        "1"
    );

    // A node with no cluster identity says so.
    let bare = Node {
        child: None,
        root: a.root.join("bare"),
        runtime: a.runtime.clone(),
        name: a.name.clone(),
        address: a.address,
    };
    let output = bare.run(&["vfs", "ls"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no cluster"));
}
