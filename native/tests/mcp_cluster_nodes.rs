//! MCP `nodes`: local cluster membership discovery, no daemon and no network.
//!
//! Each test holds a process-wide env lock and uses its own temp state home.
//! Registry fixtures use real X25519 keypairs so the strict registry loader
//! accepts them, and a `UnixListener` sentinel bound at the daemon socket fails
//! the test if `nodes` opens or probes a socket instead of reading files.

#![cfg(unix)]

use remuda_native::cluster::NodeIdentity;
use remuda_native::mcp;
use serde_json::{json, Value};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Bind a listener at the daemon socket and count any connection. The accept
/// loop closes each connection immediately so a stray client cannot hang.
struct SocketSentinel {
    path: PathBuf,
    halt: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SocketSentinel {
    fn bind(path: &Path, hits: Arc<AtomicUsize>) -> io::Result<Self> {
        let _ = std::fs::remove_file(path);
        let listener = std::os::unix::net::UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        let halt = Arc::new(AtomicBool::new(false));
        let worker_halt = Arc::clone(&halt);
        let thread = std::thread::spawn(move || loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    hits.fetch_add(1, Ordering::SeqCst);
                    drop(stream);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if worker_halt.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(_) => break,
            }
        });
        Ok(Self {
            path: path.to_path_buf(),
            halt,
            thread: Some(thread),
        })
    }

    fn stop(&mut self) {
        self.halt.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for SocketSentinel {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Fixture {
    root: PathBuf,
    state: PathBuf,
    socket: PathBuf,
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    hits: Arc<AtomicUsize>,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let base = if Path::new("/private/tmp").is_dir() {
            PathBuf::from("/private/tmp")
        } else {
            std::env::temp_dir()
        };
        let root = base.join(format!("rnodes-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let state = root.join("state");
        let runtime = root.join("runtime");
        let home = root.join("home");
        for dir in [&state, &runtime, &home] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let mut saved = Vec::new();
        for (key, value) in [
            ("XDG_STATE_HOME", &state),
            ("HOME", &home),
            ("REMUDA_RUNTIME_DIR", &runtime),
        ] {
            saved.push((key, std::env::var_os(key)));
            std::env::set_var(key, value);
        }
        Self {
            socket: runtime.join("nodes-test.sock"),
            root,
            state,
            saved,
            hits: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Create the identity and self registry entry in-process. The CLI
    /// `cluster init` would auto-start a daemon, so it is deliberately unused.
    fn init(&self) -> NodeIdentity {
        let (identity, _, _) = remuda_native::cluster::init_with_notice().expect("cluster init");
        identity
    }

    fn registry_path(&self) -> PathBuf {
        self.state.join("remuda/cluster/authorized_nodes.json")
    }

    fn settings_path(&self) -> PathBuf {
        self.state.join("remuda/cluster/settings.json")
    }

    fn write_private(&self, path: &Path, bytes: &[u8]) {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn write_registry(&self, entries: &[Value]) {
        let bytes = serde_json::to_vec_pretty(&json!({"authorized_nodes": entries})).unwrap();
        self.write_private(&self.registry_path(), &bytes);
    }

    fn write_settings(&self, aliases: &[(&str, &str)]) {
        let aliases: serde_json::Map<String, Value> = aliases
            .iter()
            .map(|(fingerprint, alias)| ((*fingerprint).to_string(), json!(alias)))
            .collect();
        let bytes = serde_json::to_vec(&json!({
            "allow_remote_control": true,
            "node_aliases": aliases,
        }))
        .unwrap();
        self.write_private(&self.settings_path(), &bytes);
    }

    fn ask(&self, request: Value) -> Value {
        let line = mcp::handle(&self.socket, &request.to_string())
            .expect("a request with an id gets a reply");
        serde_json::from_str(&line).expect("the reply is JSON")
    }

    fn tools(&self) -> Vec<Value> {
        let reply = self.ask(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list"
        }));
        reply["result"]["tools"]
            .as_array()
            .expect("tools is a list")
            .clone()
    }

    /// Call `nodes` with a socket sentinel bound. `tools/list` is exempt
    /// because the existing descriptor path may already ask a live daemon for
    /// Lua tools; a bare `nodes` call must not.
    fn call(&self, arguments: Value) -> Value {
        let sentinel = SocketSentinel::bind(&self.socket, Arc::clone(&self.hits))
            .expect("bind the socket sentinel");
        let reply = self.ask(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "nodes", "arguments": arguments}
        }));
        drop(sentinel);
        reply["result"].clone()
    }

    /// A `tools/call` with no `arguments` field at all; the tool must treat it
    /// like `{}` rather than as a missing required parameter.
    fn call_without_arguments(&self) -> Value {
        let sentinel = SocketSentinel::bind(&self.socket, Arc::clone(&self.hits))
            .expect("bind the socket sentinel");
        let reply = self.ask(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "nodes"}
        }));
        drop(sentinel);
        reply["result"].clone()
    }

    fn assert_no_socket(&self) {
        assert_eq!(
            self.hits.load(Ordering::SeqCst),
            0,
            "nodes opened the daemon socket; node discovery must read cluster files only"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        for (key, value) in self.saved.drain(..).rev() {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn text(reply: &Value) -> String {
    reply["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// A peer with a real X25519 keypair, so its fingerprint, base64 public key
/// and low-order check all satisfy the strict registry loader.
#[derive(Clone)]
struct Node {
    fingerprint: String,
    public_key: String,
}

impl Node {
    fn new() -> Self {
        let params: snow::params::NoiseParams = "Noise_NN_25519_ChaChaPoly_SHA256"
            .parse()
            .expect("static Noise pattern");
        let keypair = snow::Builder::new(params)
            .generate_keypair()
            .expect("generate a node keypair");
        Self {
            fingerprint: remuda_native::cluster::encoding::fingerprint(&keypair.public),
            public_key: remuda_native::cluster::encoding::encode_base64(&keypair.public),
        }
    }

    fn from_identity(identity: &NodeIdentity) -> Self {
        Self {
            fingerprint: identity.node_fp.clone(),
            public_key: remuda_native::cluster::encoding::encode_base64(&identity.static_pubkey),
        }
    }
}

fn alias(name: Option<&str>, at: u64, by: &str) -> Value {
    json!({"at": at, "by": by, "name": name})
}

fn entry(
    node: &Node,
    by: &str,
    state: Option<&str>,
    alias: Option<Value>,
    endpoint: Option<&str>,
) -> Value {
    let mut entry = json!({
        "node_fp": node.fingerprint,
        "static_pubkey": node.public_key,
        "format_major": 1,
        "format_minor": 0,
        "version": 1,
        "by": by,
    });
    if let Some(state) = state {
        entry["state"] = json!(state);
    }
    if let Some(alias) = alias {
        entry["alias"] = alias;
    }
    if let Some(endpoint) = endpoint {
        entry["endpoint"] = json!(endpoint);
    }
    entry
}

fn field<'a>(line: &'a str, name: &str) -> &'a str {
    line.split('\t')
        .find_map(|part| part.strip_prefix(name))
        .unwrap_or_default()
}

fn fields(line: &str) -> Vec<&str> {
    line.split('\t').collect()
}

#[test]
fn descriptor_lists_nodes_with_no_arguments() {
    let _guard = lock();
    let fixture = Fixture::new("descriptor");
    let tools = fixture.tools();
    let nodes = tools
        .iter()
        .find(|tool| tool["name"] == "nodes")
        .unwrap_or_else(|| panic!("nodes missing from tools/list"));
    assert_eq!(nodes["inputSchema"]["type"], "object");
    assert_eq!(nodes["inputSchema"]["properties"], json!({}));
    assert!(
        nodes["inputSchema"].get("required").is_none(),
        "nodes has no required arguments"
    );
    let description = nodes["description"]
        .as_str()
        .unwrap_or_default()
        .to_lowercase();
    assert!(
        description.contains("local"),
        "description must say it is local: {description}"
    );
    assert!(
        description.contains("membership"),
        "description must say membership: {description}"
    );

    for arguments in [json!({"unexpected": true}), json!({"node": "x"})] {
        let bad = fixture.call(arguments.clone());
        assert_eq!(bad["isError"], true, "{arguments}: {bad}");
        assert!(
            text(&bad).contains("takes no arguments"),
            "{arguments}: {}",
            text(&bad)
        );
    }
    fixture.assert_no_socket();
}

#[test]
fn no_cluster_is_a_typed_error_not_empty_success() {
    let _guard = lock();
    let fixture = Fixture::new("no-cluster");
    let reply = fixture.call(json!({}));
    assert_eq!(reply["isError"], true, "{reply}");
    assert!(
        text(&reply).contains("cluster is not initialized"),
        "{}",
        text(&reply)
    );
    fixture.assert_no_socket();
}

#[test]
fn omitted_arguments_field_is_accepted() {
    let _guard = lock();
    let fixture = Fixture::new("omitted");
    fixture.init();
    fixture.write_registry(&[]);
    let reply = fixture.call_without_arguments();
    assert_eq!(reply["isError"], false, "{reply}");
    assert_eq!(text(&reply), "", "omitted arguments must behave like {{}}");
    fixture.assert_no_socket();
}

#[test]
fn self_alias_endpoint_and_order_are_reported() {
    let _guard = lock();
    let fixture = Fixture::new("self");
    let identity = fixture.init();
    let by = identity.node_fp.clone();
    let me = Node::from_identity(&identity);
    let peer = Node::new();
    let revoked = Node::new();
    fixture.write_registry(&[
        entry(
            &me,
            &by,
            Some("admitted"),
            Some(alias(Some("studio"), 1, &by)),
            Some("192.0.2.10:7441"),
        ),
        entry(&peer, &by, Some("admitted"), None, None),
        entry(
            &revoked,
            &by,
            Some("revoked"),
            Some(alias(Some("old-box"), 1, &by)),
            Some("10.1.2.3:7441"),
        ),
    ]);
    let reply = fixture.call(json!({}));
    assert_eq!(reply["isError"], false, "{reply}");
    let body = text(&reply);
    assert!(!body.contains("192.0.2.10"), "raw endpoint leaked: {body}");
    assert!(!body.contains("10.1.2.3"), "raw endpoint leaked: {body}");
    assert!(!body.contains("7441"), "raw port leaked: {body}");
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 3, "{body}");

    let fingerprints: Vec<&str> = lines
        .iter()
        .map(|line| field(line, "fingerprint="))
        .collect();
    let mut sorted = fingerprints.clone();
    sorted.sort_unstable();
    assert_eq!(fingerprints, sorted, "unstable order: {body}");

    let self_row = lines
        .iter()
        .find(|line| field(line, "self=") == "true")
        .expect("self row");
    let self_id = remuda_native::cluster::node_label(&identity.node_fp);
    assert!(
        self_row.starts_with(&format!("id={self_id}\t")),
        "{self_row}"
    );
    assert_eq!(field(self_row, "alias="), "studio", "{self_row}");
    assert_eq!(field(self_row, "endpoint="), "present", "{self_row}");
    assert_eq!(field(self_row, "state="), "admitted", "{self_row}");

    let revoked_row = lines
        .iter()
        .find(|line| field(line, "state=") == "revoked")
        .expect("revoked row");
    assert_eq!(field(revoked_row, "alias="), "old-box", "{revoked_row}");
    assert_eq!(field(revoked_row, "endpoint="), "present", "{revoked_row}");

    let plain = lines
        .iter()
        .find(|line| field(line, "fingerprint=") == peer.fingerprint)
        .expect("plain peer row");
    assert_eq!(field(plain, "alias="), "", "{plain}");
    assert_eq!(field(plain, "endpoint="), "missing", "{plain}");
    assert_eq!(field(plain, "self="), "false", "{plain}");
    fixture.assert_no_socket();
}

#[test]
fn output_order_is_independent_of_registry_file_order() {
    let _guard = lock();
    let fixture = Fixture::new("order");
    let identity = fixture.init();
    let by = identity.node_fp.clone();
    let nodes = [Node::new(), Node::new(), Node::new()];
    let rows: Vec<Value> = nodes
        .iter()
        .map(|node| entry(node, &by, Some("admitted"), None, None))
        .collect();

    fixture.write_registry(&rows);
    let first = text(&fixture.call(json!({})));
    fixture.write_registry(&[rows[2].clone(), rows[0].clone(), rows[1].clone()]);
    let second = text(&fixture.call(json!({})));
    assert_eq!(first, second, "file order changed the output");
    let fingerprints: Vec<&str> = second
        .lines()
        .map(|line| field(line, "fingerprint="))
        .collect();
    let mut sorted = fingerprints.clone();
    sorted.sort_unstable();
    assert_eq!(fingerprints, sorted, "not fingerprint order: {second}");
    fixture.assert_no_socket();
}

/// The fixtures above must be entries the strict loader already accepts, not
/// states that only a lenient reader tolerates. `snapshot()` is that loader
/// plus the existing registry merge, so this passing proves the keypairs,
/// fingerprints, base64 keys and endpoints are real.
#[test]
fn generated_fixtures_pass_the_strict_registry_loader() {
    let _guard = lock();
    let fixture = Fixture::new("strict");
    let identity = fixture.init();
    let by = identity.node_fp.clone();
    let me = Node::from_identity(&identity);
    let admitted = Node::new();
    let revoked = Node::new();
    fixture.write_registry(&[
        entry(
            &me,
            &by,
            Some("admitted"),
            Some(alias(Some("studio"), 1, &by)),
            Some("192.0.2.10:7441"),
        ),
        entry(
            &admitted,
            &by,
            Some("admitted"),
            Some(alias(Some("peer"), 1, &by)),
            None,
        ),
        entry(&revoked, &by, Some("revoked"), None, Some("10.1.2.3:7441")),
    ]);
    let snapshot = remuda_native::cluster::snapshot()
        .expect("the strict loader reads the fixture")
        .expect("the cluster directory exists");
    assert_eq!(snapshot.registry.authorized_nodes.len(), 3);
    assert!(snapshot
        .registry
        .authorized_nodes
        .iter()
        .any(|entry| entry.node_fp == admitted.fingerprint
            && entry.state == remuda_native::cluster::NodeState::Admitted));
    assert!(snapshot
        .registry
        .authorized_nodes
        .iter()
        .any(|entry| entry.node_fp == revoked.fingerprint
            && entry.state == remuda_native::cluster::NodeState::Revoked));
    fixture.assert_no_socket();
}

#[test]
fn old_schema_defaults_to_admitted_empty_and_missing() {
    let _guard = lock();
    let fixture = Fixture::new("old");
    let identity = fixture.init();
    let by = identity.node_fp.clone();
    let node = Node::new();
    let mut row = entry(&node, &by, None, None, None);
    row.as_object_mut().unwrap().remove("format_major");
    row.as_object_mut().unwrap().remove("format_minor");
    fixture.write_registry(&[row]);
    let reply = fixture.call(json!({}));
    assert_eq!(reply["isError"], false, "{reply}");
    let body = text(&reply);
    let row = body.lines().next().expect("one row");
    assert_eq!(field(row, "state="), "admitted", "{row}");
    assert_eq!(field(row, "alias="), "", "{row}");
    assert_eq!(field(row, "endpoint="), "missing", "{row}");
    assert_eq!(field(row, "self="), "false", "{row}");
    fixture.assert_no_socket();
}

#[test]
fn duplicate_registry_rows_fold_once() {
    let _guard = lock();
    let fixture = Fixture::new("duplicate");
    let identity = fixture.init();
    let by = identity.node_fp.clone();
    let node = Node::new();
    let mut older = entry(
        &node,
        &by,
        Some("admitted"),
        Some(alias(Some("one"), 1, &by)),
        None,
    );
    older["version"] = json!(1);
    let mut newer = entry(
        &node,
        &by,
        Some("admitted"),
        Some(alias(Some("two"), 2, &by)),
        None,
    );
    newer["version"] = json!(2);
    let me = Node::from_identity(&identity);
    let mut me_older = entry(
        &me,
        &by,
        Some("admitted"),
        Some(alias(Some("me-one"), 1, &by)),
        None,
    );
    me_older["version"] = json!(1);
    let mut me_newer = entry(
        &me,
        &by,
        Some("admitted"),
        Some(alias(Some("me-two"), 2, &by)),
        None,
    );
    me_newer["version"] = json!(2);
    fixture.write_registry(&[older, newer, me_older, me_newer]);
    let reply = fixture.call(json!({}));
    assert_eq!(reply["isError"], false, "{reply}");
    let body = text(&reply);
    let rows: Vec<&str> = body
        .lines()
        .filter(|line| field(line, "fingerprint=") == node.fingerprint)
        .collect();
    assert_eq!(rows.len(), 1, "duplicate rows were not folded: {body}");
    assert_eq!(field(rows[0], "alias="), "two", "{}", rows[0]);
    let self_rows: Vec<&str> = body
        .lines()
        .filter(|line| field(line, "self=") == "true")
        .collect();
    assert_eq!(
        self_rows.len(),
        1,
        "duplicate self rows were not folded: {body}"
    );
    assert_eq!(field(self_rows[0], "alias="), "me-two", "{}", self_rows[0]);
    fixture.assert_no_socket();
}

#[test]
fn settings_alias_merges_and_replicated_tombstone_clears_it() {
    let _guard = lock();
    let fixture = Fixture::new("aliases");
    let identity = fixture.init();
    let by = identity.node_fp.clone();
    let legacy = Node::new();
    let cleared = Node::new();
    fixture.write_settings(&[
        (&legacy.fingerprint, "from-settings"),
        (&cleared.fingerprint, "old-settings"),
    ]);
    fixture.write_registry(&[
        entry(&legacy, &by, Some("admitted"), None, None),
        entry(
            &cleared,
            &by,
            Some("admitted"),
            Some(alias(None, 10, &by)),
            None,
        ),
    ]);
    let reply = fixture.call(json!({}));
    assert_eq!(reply["isError"], false, "{reply}");
    let body = text(&reply);
    let legacy_row = body
        .lines()
        .find(|line| field(line, "fingerprint=") == legacy.fingerprint)
        .expect("legacy row");
    assert_eq!(field(legacy_row, "alias="), "from-settings", "{legacy_row}");
    let cleared_row = body
        .lines()
        .find(|line| field(line, "fingerprint=") == cleared.fingerprint)
        .expect("cleared row");
    assert_eq!(
        field(cleared_row, "alias="),
        "",
        "tombstone did not clear: {cleared_row}"
    );
    fixture.assert_no_socket();
}

#[test]
fn invalid_aliases_are_blanked_without_breaking_framing() {
    let _guard = lock();
    let fixture = Fixture::new("sanitize");
    let identity = fixture.init();
    let by = identity.node_fp.clone();
    let names = [
        "",
        "evil\tname",
        "evil\u{1b}[31mname",
        "evil\rname",
        "evil\nname",
        "bad/name",
        " leading",
        &"a".repeat(65),
    ];
    let nodes: Vec<Node> = names.iter().map(|_| Node::new()).collect();
    let rows: Vec<Value> = nodes
        .iter()
        .zip(names.iter())
        .map(|(node, name)| {
            entry(
                node,
                &by,
                Some("admitted"),
                Some(alias(Some(name), 1, &by)),
                None,
            )
        })
        .collect();
    fixture.write_registry(&rows);
    let reply = fixture.call(json!({}));
    assert_eq!(reply["isError"], false, "{reply}");
    let body = text(&reply);
    assert!(!body.contains('\u{1b}'), "ESC leaked: {body:?}");
    assert!(!body.contains('\r'), "CR leaked: {body:?}");
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), names.len(), "line framing changed: {body:?}");
    for line in lines {
        assert_eq!(fields(line).len(), 6, "partial/extra fields: {line:?}");
        assert_eq!(
            field(line, "alias="),
            "",
            "invalid alias not blanked: {line:?}"
        );
    }
    fixture.assert_no_socket();
}

#[test]
fn control_characters_in_identity_fields_are_a_typed_error() {
    let _guard = lock();
    let fixture = Fixture::new("control-fp");
    let identity = fixture.init();
    let by = identity.node_fp.clone();
    let node = Node::new();
    let mut row = entry(&node, &by, Some("admitted"), None, None);
    row["node_fp"] = json!("SHA256:bad\u{1b}[31m");
    fixture.write_registry(&[row]);
    let reply = fixture.call(json!({}));
    assert_eq!(reply["isError"], true, "{reply}");
    assert!(
        text(&reply).contains("cluster registry is unreadable"),
        "{}",
        text(&reply)
    );
    fixture.assert_no_socket();
}

#[test]
fn identity_missing_with_registry_present_is_not_initialized() {
    let _guard = lock();
    let fixture = Fixture::new("no-identity");
    let identity = fixture.init();
    let by = identity.node_fp.clone();
    let node = Node::new();
    fixture.write_registry(&[entry(&node, &by, Some("admitted"), None, None)]);
    std::fs::remove_file(fixture.state.join("remuda/cluster/identity.key")).unwrap();
    let reply = fixture.call(json!({}));
    assert_eq!(reply["isError"], true, "{reply}");
    assert!(
        text(&reply).contains("cluster is not initialized"),
        "{}",
        text(&reply)
    );
    fixture.assert_no_socket();
}

#[test]
fn permission_unreadable_registry_is_typed() {
    let _guard = lock();
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new("unreadable");
    let identity = fixture.init();
    let by = identity.node_fp.clone();
    let node = Node::new();
    fixture.write_registry(&[entry(&node, &by, Some("admitted"), None, None)]);
    std::fs::set_permissions(
        fixture.registry_path(),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let reply = fixture.call(json!({}));
    assert_eq!(reply["isError"], true, "{reply}");
    assert!(
        text(&reply).contains("cluster registry is unreadable"),
        "{}",
        text(&reply)
    );
    fixture.assert_no_socket();
}

#[test]
fn invalid_state_is_typed_but_missing_state_is_admitted() {
    let _guard = lock();
    let fixture = Fixture::new("state");
    let identity = fixture.init();
    let by = identity.node_fp.clone();
    let node = Node::new();
    for boundary in ["pending", "", "Admitted", "revoked "] {
        fixture.write_registry(&[entry(&node, &by, Some(boundary), None, None)]);
        let invalid = fixture.call(json!({}));
        assert_eq!(invalid["isError"], true, "{boundary:?}: {invalid}");
        assert!(
            text(&invalid).contains("cluster registry is unreadable"),
            "{boundary:?}: {}",
            text(&invalid)
        );
    }

    fixture.write_registry(&[entry(&node, &by, None, None, None)]);
    let missing = fixture.call(json!({}));
    assert_eq!(missing["isError"], false, "{missing}");
    let body = text(&missing);
    let row = body.lines().next().expect("one row");
    assert_eq!(field(row, "state="), "admitted", "{row}");
    fixture.assert_no_socket();
}

#[test]
fn empty_endpoint_is_missing_and_malformed_endpoint_is_typed() {
    let _guard = lock();
    let fixture = Fixture::new("endpoint");
    let identity = fixture.init();
    let by = identity.node_fp.clone();
    let node = Node::new();
    fixture.write_registry(&[entry(&node, &by, Some("admitted"), None, Some(""))]);
    let empty = fixture.call(json!({}));
    assert_eq!(empty["isError"], false, "{empty}");
    let body = text(&empty);
    let row = body.lines().next().expect("one row");
    assert_eq!(field(row, "endpoint="), "missing", "{row}");

    fixture.write_registry(&[entry(
        &node,
        &by,
        Some("admitted"),
        None,
        Some("not-an-endpoint"),
    )]);
    let malformed = fixture.call(json!({}));
    assert_eq!(malformed["isError"], true, "{malformed}");
    assert!(
        text(&malformed).contains("cluster registry is unreadable"),
        "{}",
        text(&malformed)
    );
    assert!(
        !text(&malformed).contains("not-an-endpoint"),
        "the error echoed the endpoint: {}",
        text(&malformed)
    );
    fixture.assert_no_socket();
}

// A real row is 129 bytes (id 16 + fingerprint 62 + state 14 + alias 6 +
// self 10 + endpoint 16 + 5 tabs). 256 rows plus 255 newlines is 33,279
// bytes, already over the 32 KiB cap before the marker, so the byte bound
// always binds first. The 256-line cap is an upper bound, never a target.
#[test]
fn row_cap_and_byte_cap_use_the_exact_marker() {
    let _guard = lock();
    let fixture = Fixture::new("caps");
    let identity = fixture.init();
    let by = identity.node_fp.clone();
    let rows: Vec<Value> = (0..300)
        .map(|_| entry(&Node::new(), &by, Some("admitted"), None, None))
        .collect();
    fixture.write_registry(&rows);
    let reply = fixture.call(json!({}));
    assert_eq!(reply["isError"], false, "{reply}");
    let body = text(&reply);
    let lines: Vec<&str> = body.lines().collect();
    assert!(body.len() <= 32 * 1024, "byte cap exceeded: {}", body.len());
    assert!(lines.len() <= 256, "row cap exceeded: {}", lines.len());
    assert_eq!(lines.last().copied(), Some("... [nodes truncated]"));
    let row_len = lines.first().expect("one row").len();
    assert!(
        body.len() + row_len + 1 > 32 * 1024,
        "another whole row would still fit: body {}, row {row_len}",
        body.len()
    );
    for line in &lines[..lines.len() - 1] {
        assert_eq!(fields(line).len(), 6, "partial line: {line:?}");
    }
    fixture.assert_no_socket();
}

#[test]
fn byte_bound_uses_whole_lines_and_the_exact_marker() {
    let _guard = lock();
    let fixture = Fixture::new("bytes");
    let identity = fixture.init();
    let by = identity.node_fp.clone();
    let rows: Vec<Value> = (0..256)
        .map(|_| {
            entry(
                &Node::new(),
                &by,
                Some("admitted"),
                Some(alias(Some(&"a".repeat(64)), 1, &by)),
                None,
            )
        })
        .collect();
    fixture.write_registry(&rows);
    let reply = fixture.call(json!({}));
    assert_eq!(reply["isError"], false, "{reply}");
    let body = text(&reply);
    assert!(
        body.len() <= 32 * 1024,
        "byte bound exceeded: {}",
        body.len()
    );
    assert!(
        body.len() > 30 * 1024,
        "did not reach 30 KiB: {}",
        body.len()
    );
    assert!(
        body.ends_with("... [nodes truncated]"),
        "byte marker missing"
    );
    for line in body.lines() {
        if line == "... [nodes truncated]" {
            continue;
        }
        assert_eq!(fields(line).len(), 6, "partial line: {line:?}");
        assert_eq!(field(line, "alias=").len(), 64, "cut alias: {line:?}");
    }
    fixture.assert_no_socket();
}

#[test]
fn zero_rows_is_a_valid_empty_success() {
    let _guard = lock();
    let fixture = Fixture::new("zero");
    fixture.init();
    fixture.write_registry(&[]);
    let reply = fixture.call(json!({}));
    assert_eq!(reply["isError"], false, "{reply}");
    assert_eq!(text(&reply), "", "zero rows must be empty success");
    fixture.assert_no_socket();
}

#[test]
fn corrupt_registry_is_typed() {
    let _guard = lock();
    let fixture = Fixture::new("corrupt-registry");
    fixture.init();
    fixture.write_private(&fixture.registry_path(), b"{");
    let reply = fixture.call(json!({}));
    assert_eq!(reply["isError"], true, "{reply}");
    assert!(
        text(&reply).contains("cluster registry is unreadable"),
        "{}",
        text(&reply)
    );
    assert!(
        !text(&reply).contains('{'),
        "the error echoed raw JSON: {}",
        text(&reply)
    );
    fixture.assert_no_socket();
}

#[test]
fn oversized_registry_is_typed() {
    let _guard = lock();
    let fixture = Fixture::new("oversized");
    fixture.init();
    fixture.write_private(&fixture.registry_path(), &vec![b' '; 1024 * 1024 + 1]);
    let reply = fixture.call(json!({}));
    assert_eq!(reply["isError"], true, "{reply}");
    assert!(
        text(&reply).contains("cluster registry is too large"),
        "{}",
        text(&reply)
    );
    fixture.assert_no_socket();
}

#[test]
fn corrupt_identity_is_typed() {
    let _guard = lock();
    let fixture = Fixture::new("corrupt-identity");
    fixture.init();
    fixture.write_private(
        &fixture.state.join("remuda/cluster/identity.key"),
        b"not an identity",
    );
    let reply = fixture.call(json!({}));
    assert_eq!(reply["isError"], true, "{reply}");
    assert!(
        text(&reply).contains("cluster identity is unreadable"),
        "{}",
        text(&reply)
    );
    fixture.assert_no_socket();
}

#[test]
fn environment_is_restored_when_a_fixture_drops() {
    let _guard = lock();
    let keys = ["XDG_STATE_HOME", "HOME", "REMUDA_RUNTIME_DIR"];
    let before: Vec<Option<std::ffi::OsString>> = keys.iter().map(std::env::var_os).collect();
    {
        let _fixture = Fixture::new("env-restore");
    }
    let after: Vec<Option<std::ffi::OsString>> = keys.iter().map(std::env::var_os).collect();
    assert_eq!(before, after, "fixture did not restore the environment");
}

#[test]
fn socket_sentinel_detects_a_connection() {
    let _guard = lock();
    let root = std::env::temp_dir().join(format!("rnodes-sentinel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let socket = root.join("probe.sock");
    let hits = Arc::new(AtomicUsize::new(0));
    let mut sentinel = SocketSentinel::bind(&socket, Arc::clone(&hits)).expect("bind sentinel");
    std::os::unix::net::UnixStream::connect(&socket).expect("connect to sentinel");
    for _ in 0..100 {
        if hits.load(Ordering::SeqCst) > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    sentinel.stop();
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "sentinel missed a connection"
    );
}
