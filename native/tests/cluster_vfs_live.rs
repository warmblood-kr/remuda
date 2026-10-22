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
        let mut command = Command::new(&self.exe);
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
    assert!(String::from_utf8_lossy(&output.stderr).contains("no cluster"));

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
fn previous() -> PathBuf {
    let dir = std::env::var_os("REMUDA_COMPAT_PREVIOUS")
        .expect("REMUDA_COMPAT_PREVIOUS names the previous build's binary directory");
    let exe = PathBuf::from(dir).join("remuda");
    assert!(exe.is_file(), "{} missing", exe.display());
    exe
}

fn put(node: &Node, uri: &str, body: &[u8]) {
    successful(
        node.run_with_stdin(&["vfs", "put", "-", uri, "--replicas", "2"], Some(body)),
        "vfs put",
    );
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

    // Deletes cross in both directions.
    successful(c.run(&["vfs", "rm", from_old]), "rm on N+1");
    wait_gone(&old, from_old);
    successful(old.run(&["vfs", "rm", from_new]), "rm on N");
    for node in [&a, &c] {
        wait_gone(node, from_new);
    }
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
