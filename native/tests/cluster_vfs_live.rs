//! Live coverage of the cluster virtual file store (#742) across isolated
//! daemons on loopback: put on one node, read on another, delete everywhere,
//! and re-replicate after a holder is lost. The `n_plus_one` and `pre_sqlite`
//! tests also run an older build, named by `REMUDA_COMPAT_PREVIOUS`.

#![cfg(unix)]
// This integration test deliberately exercises the real loopback cluster wire.
#![allow(clippy::disallowed_types)]

use remuda_core::protocol::Request;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    exe: PathBuf,
    child: Option<Child>,
    root: PathBuf,
    runtime: PathBuf,
    name: String,
    address: SocketAddr,
}

impl Node {
    fn start() -> Self {
        Self::start_exe(PathBuf::from(env!("CARGO_BIN_EXE_remuda")))
    }

    fn start_exe(exe: PathBuf) -> Self {
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
            exe,
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
        let mut command = Command::new(&self.exe);
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

    /// Kill the daemon but keep the node's files (`kill` is also `Drop`'s).
    fn kill_daemon_only(&self) {
        let path = self.daemon_path();
        let _ = remuda_native::client::request(
            &path,
            &Request::Shutdown {
                requester_daemon_id: None,
                requester_session_id: None,
                requester_session_name: None,
                override_hosted: false,
                keep_sessions: false,
            },
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while remuda_native::ipc::connect(&path).is_ok() {
            assert!(Instant::now() < deadline, "daemon did not stop");
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
        self.run_exe(&self.exe, args, stdin)
    }

    /// Run `exe` (this node's or another build's CLI) against this node.
    fn run_exe(&self, exe: &std::path::Path, args: &[&str], stdin: Option<&[u8]>) -> Output {
        let mut command = Command::new(exe);
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
    joined(Node::start(), Node::start(), Node::start())
}

fn joined(a: Node, b: Node, c: Node) -> (Node, Node, Node) {
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
    let said = put_min_copies(&a, "remuda:///min.md", 3);
    assert!(said.contains("3 copies confirmed"), "{said}");

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

    // B saw A's write before its own, so that is no conflict.
    let stat = a.stat_json("remuda:///butler/guidance/cluster.md").unwrap();
    assert_eq!(stat["conflicts"], serde_json::json!([]));
    let listed = successful(a.run(&["vfs", "ls", "--conflicts"]), "ls --conflicts");
    assert_eq!(listed.trim(), "No conflicts.");
    let lua = successful(
        a.run(&[
            "-e",
            "return #remuda.vfs.stat('remuda:///butler/guidance/cluster.md').conflicts",
        ]),
        "lua conflicts",
    );
    assert_eq!(lua.trim(), "0");
    let settle = a.run(&[
        "vfs",
        "resolve-conflict",
        "remuda:///butler/guidance/cluster.md",
        "--keep",
        "winner",
    ]);
    assert!(!settle.status.success());
    assert!(
        String::from_utf8_lossy(&settle.stderr).contains("no conflict at"),
        "{}",
        String::from_utf8_lossy(&settle.stderr)
    );

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

    // The lost holder comes back with its copy: the placement is the
    // original pair again, so the outsider trims its stand-in copy.
    let back = nodes[lost].as_mut().unwrap();
    back.spawn_daemon();
    back.enable_listener();
    let outsider_node = nodes[outsider].as_ref().unwrap();
    wait_until(
        "outsider trims its stand-in copy",
        Duration::from_secs(20),
        || !outsider_node.has_blob(&sha),
    );
    for holder in [lost, survivor] {
        assert!(nodes[holder].as_ref().unwrap().has_blob(&sha));
    }
}

/// A body whose one copy goes to `nodes[from]` with every node up, and to
/// `nodes[to]` once `nodes[from]` is gone.
fn body_moving(nodes: &[&Node], from: usize, to: usize) -> Vec<u8> {
    let fingerprints: Vec<String> = nodes.iter().map(|node| node.fingerprint()).collect();
    let all: Vec<&str> = fingerprints.iter().map(String::as_str).collect();
    let rest: Vec<&str> = all
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != from)
        .map(|(_, fp)| *fp)
        .collect();
    (0..)
        .map(|n| format!("drain body {n}").into_bytes())
        .find(|body| {
            let sha = sha256_hex(body);
            remuda_core::vfs::placement(&sha, 1, &all) == [all[from]]
                && remuda_core::vfs::placement(&sha, 1, &rest) == [all[to]]
        })
        .unwrap()
}

/// `put --replicas 1` of `body` on `node`.
fn put_one_copy(node: &Node, uri: &str, body: &[u8]) {
    successful(
        node.run_with_stdin(&["vfs", "put", "-", uri, "--replicas", "1"], Some(body)),
        "vfs put --replicas 1",
    );
}

#[test]
fn status_reports_files_peers_and_copies() {
    let _serial = live_test_guard();
    let (a, b, c) = three_nodes();
    successful(
        a.run_with_stdin(
            &[
                "vfs",
                "put",
                "-",
                "remuda:///s/one.txt",
                "--replicas",
                "3",
                "--wait",
            ],
            Some(b"one"),
        ),
        "vfs put",
    );
    let said = successful(a.run(&["vfs", "status"]), "vfs status");
    assert!(said.contains("Files: 1 live of 10000"), "{said}");
    assert!(said.contains("Conflicts: none"), "{said}");
    for node in [&b, &c] {
        let name = remuda_native::cluster::node_label(&node.fingerprint());
        assert!(
            said.lines()
                .any(|line| line.contains(&name) && line.contains(" up")),
            "{said}"
        );
    }
    let said = successful(a.run(&["vfs", "status", "--copies"]), "vfs status --copies");
    assert!(said.contains("Copies: 1 files complete"), "{said}");
    let json = successful(a.run(&["vfs", "status", "--json"]), "vfs status --json");
    let report: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(report["peers"].as_array().unwrap().len(), 2, "{json}");
    assert_eq!(report["live"], 1, "{json}");
}

#[test]
fn drain_has_the_next_node_take_what_only_the_leaving_node_holds() {
    let _serial = live_test_guard();
    let (a, b, c) = three_nodes();
    let body = body_moving(&[&a, &b, &c], 2, 1);
    let sha = sha256_hex(&body);
    put_one_copy(&c, "remuda:///leaving.txt", &body);
    assert!(!b.has_blob(&sha));

    let said = successful(c.run(&["vfs", "drain", "--timeout", "30"]), "vfs drain");
    assert!(said.contains("Safe to leave"), "{said}");
    assert!(b.has_blob(&sha));
    assert!(!a.has_blob(&sha));
}

/// `cluster leave` drains, a peer revokes the node and audits it, and the
/// node rejoins with an empty store: a file deleted meanwhile, whose
/// tombstone the others collected, does not come back.
#[test]
fn leave_is_revoked_by_a_peer_and_rejoins_with_an_empty_store() {
    let _serial = live_test_guard();
    let (a, b, c) = three_nodes();
    let body = body_moving(&[&a, &b, &c], 2, 1);
    let sha = sha256_hex(&body);
    put_one_copy(&c, "remuda:///leaving.txt", &body);
    put(&c, "remuda:///gone.txt", b"deleted while away");
    wait_for_body(&a, "remuda:///gone.txt", b"deleted while away");
    wait_until(
        "peers list the one-copy file",
        Duration::from_secs(20),
        || a.stat_json("remuda:///leaving.txt").is_some(),
    );
    let leaver = c.fingerprint();

    // b lacks the only copy, so the first ask is refused; drain fixes that.
    let said = successful(c.run(&["cluster", "leave", "--yes"]), "cluster leave");
    assert!(said.contains("revoked this node"), "{said}");
    assert!(b.has_blob(&sha));
    let state = c.root.join("state/remuda");
    let back = walk(&state.join("cluster"));
    assert!(back.is_empty(), "cluster folder came back: {back:?}");
    let left = std::fs::read_dir(&state)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.to_string_lossy().contains("cluster.left-"))
        .expect("moved cluster folder");
    assert!(left.join("vfs/index.db").is_file());
    assert!(!left.join("identity.key").exists());
    for node in [&a, &b] {
        wait_until(
            "peers see the leaver revoked",
            Duration::from_secs(20),
            || revoked(node, &leaver),
        );
    }
    let audited = [&a, &b].iter().any(|node| {
        std::fs::read_to_string(node.root.join("state/remuda/cluster/audit.jsonl"))
            .is_ok_and(|log| log.contains(r#""op":"leave""#) && log.contains(&leaver))
    });
    assert!(audited, "no peer audited the leave");

    // While c is away, gone.txt is deleted and its tombstone collected.
    successful(a.run(&["vfs", "rm", "remuda:///gone.txt"]), "vfs rm");
    wait_gone(&b, "remuda:///gone.txt");
    for node in [&a, &b] {
        let index = node.root.join("state/remuda/cluster/vfs/index.db");
        rusqlite::Connection::open(index)
            .unwrap()
            .execute("DELETE FROM entries WHERE path = 'gone.txt'", [])
            .unwrap();
    }

    successful(
        c.run(&["cluster", "init", "--no-listen"]),
        "cluster init again",
    );
    c.enable_listener();
    join(&a, &c);
    wait_for_body(&c, "remuda:///leaving.txt", &body);
    thread::sleep(Duration::from_millis(SYNC_INTERVAL_MS * 10));
    for node in [&a, &b, &c] {
        assert!(
            node.stat_json("remuda:///gone.txt").is_none(),
            "gone.txt came back"
        );
    }
}

/// Every path under `dir`, `dir` included; empty if it does not exist.
fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut out = vec![dir.to_path_buf()];
    for entry in read {
        let p = entry.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p))
        } else {
            out.push(p)
        }
    }
    out
}

/// Whether `node`'s registry lists `fingerprint` as revoked.
fn revoked(node: &Node, fingerprint: &str) -> bool {
    let registry: remuda_native::cluster::Registry = serde_json::from_slice(
        &std::fs::read(node.root.join("state/remuda/cluster/authorized_nodes.json")).unwrap(),
    )
    .unwrap();
    registry.authorized_nodes.iter().any(|entry| {
        entry.node_fp == fingerprint && entry.state == remuda_native::cluster::NodeState::Revoked
    })
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
    let text = String::from_utf8_lossy(&missing.stderr);
    assert!(text.contains("no file at x"), "{text}");

    successful(a.run(&["vfs", "config", "--cluster", "home"]), "vfs config");
    successful(
        a.run_with_stdin(&["vfs", "put", "-", "remuda://home/x"], Some(b"1")),
        "named-cluster put",
    );
    assert_eq!(
        successful(a.run(&["vfs", "get", "remuda:///x"]), "get"),
        "1"
    );

    // Lua writes the store only through remuda.vfs.put and rm (#778).
    let put = successful(
        a.run(&["-e", "local s, e = remuda.vfs.put('remuda://home/lua.txt', 'from lua'); return s and s.path or e"]),
        "lua vfs.put",
    );
    assert_eq!(put.trim(), "lua.txt");
    assert_eq!(
        successful(a.run(&["vfs", "get", "remuda:///lua.txt"]), "get lua file"),
        "from lua"
    );
    let removed = successful(
        a.run(&["-e", "return tostring(remuda.vfs.rm('remuda:///lua.txt'))"]),
        "lua vfs.rm",
    );
    assert_eq!(removed.trim(), "true");
    assert!(!a.run(&["vfs", "get", "remuda:///lua.txt"]).status.success());

    // A node with no cluster identity says so; its daemon answers (#778).
    let mut bare = Node {
        exe: a.exe.clone(),
        child: None,
        root: a.root.join("bare"),
        runtime: a.root.join("bare-runtime"),
        name: format!("{}-bare", a.name),
        address: a.address,
    };
    std::fs::create_dir_all(&bare.runtime).unwrap();
    std::fs::create_dir_all(bare.root.join("home")).unwrap();
    bare.spawn_daemon();
    let output = bare.run(&["vfs", "ls"]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("no cluster"), "{text}");

    // With no daemon, vfs fails with one line and touches no store (#778).
    bare.kill();
    let index = a.root.join("state/remuda/cluster/vfs/index.db");
    a.kill_daemon_only();
    std::fs::remove_dir_all(a.root.join("state/remuda/cluster/vfs")).unwrap();
    let output = a.run_with_stdin(&["vfs", "put", "-", "remuda:///y"], Some(b"2"));
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.contains("needs the remuda daemon"), "{text}");
    assert!(
        !index.exists(),
        "a CLI with no daemon created {}",
        index.display()
    );
}

/// The previous build's `remuda`. CI sets the directory; there is no default,
/// so a compat test cannot pass without running the older build.
// N is the PR's base sha: never assume N lacks a feature that base already
// merged. Ask N (`vfs help`, change-page caps) and assert per answer.
fn previous() -> PathBuf {
    let dir = std::env::var_os("REMUDA_COMPAT_PREVIOUS")
        .expect("REMUDA_COMPAT_PREVIOUS names the previous build's binary directory");
    let exe = PathBuf::from(dir).join("remuda");
    assert!(exe.is_file(), "{} missing", exe.display());
    exe
}

/// Whether the previous build's `remuda vfs help` lists `verb`. Help runs
/// without a daemon; a scratch home keeps it away from real state.
fn previous_has_vfs_verb(verb: &str) -> bool {
    let home = std::env::temp_dir().join(format!("cvp{}-{verb}", std::process::id()));
    let output = Command::new(previous())
        .args(["vfs", "help"])
        .env("HOME", &home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("REMUDA_RUNTIME_DIR", home.join("runtime"))
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&home);
    String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        line.trim_start()
            .strip_prefix(verb)
            .is_some_and(|rest| rest.starts_with(' '))
    })
}

/// Skip a compat test whose N predates what it tests. Jobs whose N must have
/// everything set REMUDA_COMPAT_NO_SKIP, so a skip there fails instead.
fn skip(reason: &str) {
    assert!(
        std::env::var_os("REMUDA_COMPAT_NO_SKIP").is_none(),
        "REMUDA_COMPAT_NO_SKIP is set but {reason}"
    );
    eprintln!("skipped: {reason}");
}

fn put(node: &Node, uri: &str, body: &[u8]) {
    successful(
        node.run_with_stdin(&["vfs", "put", "-", uri, "--replicas", "2"], Some(body)),
        "vfs put",
    );
}

/// `put --replicas 3 --min-copies MIN`; returns stdout and stderr.
fn put_min_copies(node: &Node, uri: &str, min: u8) -> String {
    let min = min.to_string();
    let args = [
        "vfs",
        "put",
        "-",
        uri,
        "--replicas",
        "3",
        "--min-copies",
        &min,
    ];
    let output = node.run_with_stdin(&args, Some(b"three copies"));
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "put --min-copies: {said}");
    said
}

fn wait_for_body(node: &Node, uri: &str, body: &[u8]) {
    wait_until("content reaches peer", Duration::from_secs(20), || {
        node.stat_json(uri)
            .is_some_and(|stat| stat["sha256"] == sha256_hex(body))
    });
    let got = successful(node.run(&["vfs", "get", uri]), "vfs get");
    assert_eq!(got.as_bytes(), body);
}

fn wait_gone(node: &Node, uri: &str) {
    wait_until("tombstone reaches peer", Duration::from_secs(20), || {
        node.stat_json(uri).is_none()
    });
}

/// Two nodes of this build and one of the previous build sync both ways (#778).
#[test]
#[ignore = "N/N+1: needs REMUDA_COMPAT_PREVIOUS; run by CI jobs host-compat-n-plus-one and vfs-compat-pre-sqlite"]
fn n_plus_one_vfs_three_daemons_of_two_builds_sync() {
    let _serial = live_test_guard();
    if !previous_has_vfs_verb("put") {
        skip("N has no vfs");
        return;
    }
    let (a, old, c) = joined(Node::start(), Node::start_exe(previous()), Node::start());
    let (from_old, from_new) = (
        "remuda:///mixed/from-old.txt",
        "remuda:///mixed/from-new.txt",
    );

    put(&old, from_old, b"written by N");
    for node in [&a, &c] {
        wait_for_body(node, from_old, b"written by N");
    }
    put(&a, from_new, b"written by N+1");
    for node in [&old, &c] {
        wait_for_body(node, from_new, b"written by N+1");
    }

    // --min-copies counts the older node's copy through BlobStat.
    let said = put_min_copies(&a, "remuda:///mixed/three.txt", 3);
    assert!(said.contains("3 copies confirmed"), "{said}");

    // A newer write on the older node wins on both newer nodes.
    put(&old, from_new, b"rewritten by N");
    for node in [&a, &c] {
        wait_for_body(node, from_new, b"rewritten by N");
    }

    // N writes without a version vector: last write wins as before, and no
    // newer node reports a conflict for it.
    for node in [&a, &c] {
        assert_eq!(kept_conflicts(node), 0);
    }

    // Drain asks only peers that offer "take". CI's N is the PR base, so it
    // may have drain too: then it takes the copy; otherwise it is named and
    // drain exits 75 while its file is short.
    let body = body_moving(&[&a, &old, &c], 2, 1);
    put_one_copy(&c, "remuda:///mixed/leaving.txt", &body);
    let help = successful(old.run(&["vfs", "help"]), "vfs help on N");
    let old_takes = help
        .lines()
        .any(|line| line.trim_start().starts_with("drain "));
    let timeout = if old_takes { "30" } else { "3" };
    let drained = c.run(&["vfs", "drain", "--timeout", timeout]);
    let said = String::from_utf8_lossy(&drained.stderr);
    if old_takes {
        assert_eq!(drained.status.code(), Some(0), "{said}");
    } else {
        assert_eq!(drained.status.code(), Some(75), "{said}");
        assert!(said.contains("too old to take copies"), "{said}");
    }

    // Deletes cross in both directions.
    successful(c.run(&["vfs", "rm", from_old]), "rm on N+1");
    wait_gone(&old, from_old);
    successful(old.run(&["vfs", "rm", from_new]), "rm on N");
    for node in [&a, &c] {
        wait_gone(node, from_new);
    }
}

/// A writer puts files while its daemon reloads into N and back (#778): every
/// put that succeeded is in the index afterwards and reaches another node.
/// N may refuse the handoff (another handoff version); that is allowed.
#[test]
#[ignore = "N/N+1: needs REMUDA_COMPAT_PREVIOUS; run by CI job host-compat-n-plus-one"]
fn n_plus_one_vfs_writes_survive_a_reload_to_n_and_back() {
    let _serial = live_test_guard();
    // Before index.db, N reads index.json; vfs-compat-pre-sqlite covers that.
    if !previous_has_vfs_verb("export-index") {
        skip("N has no index.db");
        return;
    }
    let (a, b, _c) = three_nodes();
    let this = a.exe.clone();
    let old = previous();
    let stop = AtomicBool::new(false);
    let (written, refused, tried, reloads) = thread::scope(|scope| {
        let writer = scope.spawn(|| {
            let mut written = Vec::new();
            let mut refused = Vec::new();
            let mut tried = 0;
            while !stop.load(Ordering::Relaxed) {
                let uri = format!("remuda:///reload/{tried}.txt");
                let args = ["vfs", "put", "-", &uri];
                // A CLI refuses a daemon of another build: while N serves,
                // N's own CLI writes.
                let puts: Vec<Output> = [&this, &old]
                    .iter()
                    .map(|exe| a.run_exe(exe, &args, Some(uri.as_bytes())))
                    .collect();
                if puts.iter().any(|put| put.status.success()) {
                    written.push(uri);
                } else {
                    let why: Vec<_> = puts
                        .iter()
                        .map(|put| String::from_utf8_lossy(&put.stderr).into_owned())
                        .collect();
                    refused.push(why.join(" | "));
                }
                tried += 1;
            }
            (written, refused, tried)
        });
        thread::sleep(Duration::from_millis(300));
        let mut reloads = Vec::new();
        for target in [old.clone(), this.clone()] {
            let out = a.run(&["reload", "--binary", &target.to_string_lossy()]);
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            eprintln!(
                "reload to {}: {:?}\n{stdout}{stderr}",
                target.display(),
                out.status.code()
            );
            let moved = stdout.starts_with("reloaded: ");
            assert!(
                moved || stderr.contains(" speaks handoff version "),
                "reload to {}: {stdout}{stderr}",
                target.display()
            );
            reloads.push(moved);
            thread::sleep(Duration::from_millis(300));
        }
        stop.store(true, Ordering::Relaxed);
        let (written, refused, tried) = writer.join().unwrap();
        (written, refused, tried, reloads)
    });
    eprintln!(
        "{} of {tried} puts succeeded; reloads moved: {reloads:?}",
        written.len()
    );
    // A put may fail only while the daemon moves, or because N refuses an
    // index.db at a newer schema than it knows (the documented downgrade).
    let expected = [
        "is not this build",
        "needs the remuda daemon",
        "newer than this remuda",
    ];
    for why in &refused {
        assert!(
            expected.iter().any(|known| why.contains(known)),
            "unexpected put failure: {why}"
        );
    }
    assert!(!written.is_empty());
    let index = a.root.join("state/remuda/cluster/vfs/index.db");
    let check: String = rusqlite::Connection::open(index)
        .unwrap()
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(check, "ok");
    for uri in &written {
        let body = uri.as_bytes();
        assert_eq!(
            a.stat_json(uri).map(|stat| stat["sha256"].clone()),
            Some(serde_json::Value::from(sha256_hex(body))),
            "{uri} was acknowledged but is missing on the node that wrote it"
        );
        wait_for_body(&b, uri, body);
    }
    // The daemon serving now may not be `a`'s child: stop it by request.
    let _ = remuda_native::client::request(
        &a.daemon_path(),
        &Request::Shutdown {
            requester_daemon_id: None,
            requester_session_id: None,
            requester_session_name: None,
            override_hosted: false,
            keep_sessions: false,
        },
    );
}

/// Rows in `index.db`'s conflicts table (design step 4); 0 before that step.
fn kept_conflicts(node: &Node) -> i64 {
    let index = node.root.join("state/remuda/cluster/vfs/index.db");
    rusqlite::Connection::open(index)
        .unwrap()
        .query_row("SELECT count(*) FROM conflicts", [], |row| row.get(0))
        .unwrap_or(0)
}

/// The documented downgrade from `index.db` to a build that reads only
/// `index.json`, and the upgrade back, keep every name on a lone node.
#[test]
#[ignore = "needs REMUDA_COMPAT_PREVIOUS from before index.db; run by CI job vfs-compat-pre-sqlite"]
fn pre_sqlite_downgrade_and_upgrade_again_keep_every_name() {
    let _serial = live_test_guard();
    // Only an N from before index.db reads index.json; export-index came with it.
    if !previous_has_vfs_verb("put") {
        skip("N has no vfs");
        return;
    }
    if previous_has_vfs_verb("export-index") {
        skip("N already has index.db");
        return;
    }
    let mut node = Node::start();
    let current = node.exe.clone();
    put(&node, "remuda:///gone.txt", b"deleted under N+1");
    put(&node, "remuda:///kept.txt", b"written by N+1");
    successful(node.run(&["vfs", "rm", "remuda:///gone.txt"]), "rm");
    successful(node.run(&["vfs", "export-index"]), "export-index");

    node.kill_daemon_only();
    node.exe = previous();
    node.spawn_daemon();
    let listed = successful(node.run(&["vfs", "ls"]), "ls on N");
    assert!(
        listed.contains("kept.txt") && !listed.contains("gone.txt"),
        "{listed}"
    );
    put(
        &node,
        "remuda:///later.txt",
        b"written by N after the downgrade",
    );

    node.kill_daemon_only();
    node.exe = current;
    node.spawn_daemon();
    let listed = successful(node.run(&["vfs", "ls"]), "ls on N+1");
    assert!(
        listed.contains("kept.txt") && listed.contains("later.txt") && !listed.contains("gone.txt"),
        "{listed}"
    );
}

/// A previous build that knows fewer schema steps refuses a newer `index.db`
/// with one clear line, and touches nothing.
#[test]
#[ignore = "needs REMUDA_COMPAT_PREVIOUS from a build with index.db; run by CI job host-compat-n-plus-one"]
fn older_build_refuses_a_newer_index_db_cleanly() {
    let _serial = live_test_guard();
    if !previous_has_vfs_verb("export-index") {
        skip("N has no index.db");
        return;
    }
    let mut node = Node::start();
    put(&node, "remuda:///kept.txt", b"kept");
    node.kill_daemon_only();
    // As if a much later build had added schema steps.
    let index = node.root.join("state/remuda/cluster/vfs/index.db");
    rusqlite::Connection::open(&index)
        .unwrap()
        .pragma_update(None, "user_version", 99)
        .unwrap();
    node.exe = previous();
    node.spawn_daemon();
    let output = node.run(&["vfs", "ls"]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(
        text.contains("newer than this remuda") && text.contains("upgrade remuda"),
        "{text}"
    );
    let version: i64 = rusqlite::Connection::open(&index)
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 99);
}

/// `remuda vfs index-downgrade` steps index.db back so the previous build
/// (one or more schema steps behind) reads it; the upgrade after that keeps
/// it all.
#[test]
#[ignore = "needs REMUDA_COMPAT_PREVIOUS from a build with index.db; run by CI job host-compat-n-plus-one"]
fn older_build_reads_the_index_after_index_downgrade() {
    let _serial = live_test_guard();
    if !previous_has_vfs_verb("export-index") {
        skip("N has no index.db");
        return;
    }
    let mut node = Node::start();
    let current = node.exe.clone();
    put(&node, "remuda:///kept.txt", b"kept");
    let plan = successful(node.run(&["vfs", "index-downgrade", "1"]), "dry run");
    assert!(
        plan.contains("undo schema steps") && plan.contains("2 (change-cursors)"),
        "{plan}"
    );
    let done = successful(
        node.run(&["vfs", "index-downgrade", "1", "--yes"]),
        "index-downgrade",
    );
    assert!(done.contains("index.db is now at schema 1 (was "), "{done}");
    // This daemon no longer touches the index, so it cannot upgrade it again.
    let refused = node.run(&["vfs", "ls"]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("stepped back"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );

    node.kill_daemon_only();
    node.exe = previous();
    node.spawn_daemon();
    let listed = successful(node.run(&["vfs", "ls"]), "ls on N");
    assert!(listed.contains("kept.txt"), "{listed}");
    put(&node, "remuda:///from-n.txt", b"written by N");

    node.kill_daemon_only();
    node.exe = current;
    node.spawn_daemon();
    let listed = successful(node.run(&["vfs", "ls"]), "ls on N+1");
    assert!(
        listed.contains("kept.txt") && listed.contains("from-n.txt"),
        "{listed}"
    );
}
