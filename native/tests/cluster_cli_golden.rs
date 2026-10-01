#![cfg(unix)]
#![allow(clippy::disallowed_types)]

use std::fs;
use std::io::{BufRead, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Scratch {
    root: PathBuf,
    runtime: PathBuf,
    name: String,
}

impl Scratch {
    fn new() -> Self {
        let base = if cfg!(target_os = "macos") {
            PathBuf::from("/private/tmp")
        } else {
            std::env::temp_dir()
        }
        .canonicalize()
        .expect("canonical temp directory");
        let root = base.join(format!(
            "cluster-golden-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let runtime = root.join("runtime");
        let directories = [
            runtime.clone(),
            root.join("home"),
            root.join("config"),
            root.join("data"),
            root.join("cache"),
            root.join("state"),
        ];
        for path in &directories {
            fs::create_dir_all(path).expect("create isolated cli directory");
        }
        Self {
            root,
            runtime,
            name: format!("golden-{}", std::process::id()),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run remuda")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_remuda"));
        command
            .args(["-s", &self.name])
            .args(args)
            .env("REMUDA_RUNTIME_DIR", &self.runtime)
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("REMUDA_NO_UPDATE_CHECK", "1");
        command
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        assert!(self.runtime.starts_with(&self.root));
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct TrackedChild(std::process::Child);

impl Drop for TrackedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_daemon(scratch: &Scratch) -> TrackedChild {
    let child = scratch
        .command(&["daemon"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start isolated daemon");
    let mut daemon = TrackedChild(child);
    let socket = remuda_native::daemon::socket_path_in(&scratch.runtime, &scratch.name);
    let deadline = Instant::now() + Duration::from_secs(10);
    while remuda_native::client::request(&socket, &remuda_core::protocol::Request::List).is_err() {
        if let Some(status) = daemon.0.try_wait().expect("check isolated daemon") {
            panic!("isolated daemon exited before ready: {status}");
        }
        assert!(Instant::now() < deadline, "isolated daemon did not bind");
        std::thread::sleep(Duration::from_millis(20));
    }
    daemon
}

fn wait_until_child_exits(child: &mut std::process::Child, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "child did not exit in time");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn golden(name: &str, output: &Output, root: &Path) {
    // REMUDA_BLESS_GOLDEN=1 rewrites fixtures; the default path compares only.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/cluster");
    let code = format!("{}\n", output.status.code().unwrap_or(128));
    let stdout = normalize(&String::from_utf8_lossy(&output.stdout), root);
    let stderr = normalize(&String::from_utf8_lossy(&output.stderr), root);
    assert!(
        !has_token_shaped_text(&stdout) && !has_token_shaped_text(&stderr),
        "normalizer left token-shaped text in {name}"
    );
    let actual = [
        ("status", code),
        ("stdout", escape_fixture(&stdout)),
        ("stderr", escape_fixture(&stderr)),
    ];
    for (suffix, contents) in actual {
        let path = dir.join(format!("{name}.{suffix}"));
        if std::env::var("REMUDA_BLESS_GOLDEN").as_deref() == Ok("1") {
            fs::create_dir_all(&dir).expect("create golden directory");
            fs::write(path, contents).expect("write golden file");
        } else {
            let expected = fs::read_to_string(&path).unwrap_or_else(|error| {
                panic!(
                    "read {}: {error}; set REMUDA_BLESS_GOLDEN=1 to bless",
                    path.display()
                )
            });
            assert_eq!(contents, expected, "golden mismatch: {}", path.display());
        }
    }
}

fn escape_fixture(input: &str) -> String {
    let mut escaped = String::new();
    for part in input.split_inclusive('\n') {
        let has_newline = part.ends_with('\n');
        let line = part.strip_suffix('\n').unwrap_or(part);
        let line = line.replace('\r', "\\r");
        let trimmed = line.trim_end_matches(' ');
        escaped.push_str(trimmed);
        for _ in 0..line.len() - trimmed.len() {
            escaped.push_str("\\x20");
        }
        if has_newline {
            escaped.push('\n');
        }
    }
    escaped
}

fn normalize(input: &str, root: &Path) -> String {
    let mut text = input.replace(&root.display().to_string(), "<TEMP>");
    if let Some(index) = text.find("Unix time ") {
        let start = index + "Unix time ".len();
        let end = text[start..]
            .find(|ch: char| !ch.is_ascii_digit())
            .map_or(text.len(), |offset| start + offset);
        text.replace_range(start..end, "<TIME>");
    }
    text = replace_nodes(&text);
    text = replace_listener_addresses(&text);
    text = replace_addresses(&text);
    text = replace_fingerprints_and_base64(&text);
    assert!(
        !has_token_shaped_text(&text),
        "token-shaped data survived normalization: {text}"
    );
    text
}

fn replace_listener_addresses(input: &str) -> String {
    let prefixes = ["Listening on ", "Listener: on "];
    let mut text = input.to_owned();
    for prefix in prefixes {
        let mut out = String::new();
        let mut rest = text.as_str();
        while let Some(index) = rest.find(prefix) {
            out.push_str(&rest[..index + prefix.len()]);
            let tail = &rest[index + prefix.len()..];
            let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
            let address = &tail[..end];
            if address.parse::<SocketAddr>().is_ok() {
                out.push_str("<LISTEN_ADDR>");
                rest = &tail[end..];
            } else {
                out.push_str(address);
                rest = tail;
            }
        }
        out.push_str(rest);
        text = out;
    }
    text
}

fn replace_nodes(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    while let Some(index) = rest.find("node-") {
        out.push_str(&rest[..index]);
        let tail = &rest[index + 5..];
        let count = tail
            .chars()
            .take_while(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
            .count();
        if (8..=10).contains(&count) {
            out.push_str("<NODE>");
            rest = &tail[count..];
        } else {
            out.push_str("node-");
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

fn replace_addresses(input: &str) -> String {
    let mut out = String::new();
    let mut candidate = String::new();
    for character in input.chars() {
        if character.is_ascii_hexdigit() || matches!(character, '.' | ':' | '[' | ']' | '%') {
            candidate.push(character);
        } else {
            push_address_candidate(&mut out, &mut candidate);
            out.push(character);
        }
    }
    push_address_candidate(&mut out, &mut candidate);
    out
}

fn push_address_candidate(out: &mut String, candidate: &mut String) {
    if candidate.parse::<SocketAddr>().is_ok() {
        out.push_str("<ADDR>");
    } else {
        out.push_str(candidate);
    }
    candidate.clear();
}

fn replace_fingerprints_and_base64(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    while !rest.is_empty() {
        if rest.starts_with("SHA256:") {
            let count = rest[7..]
                .chars()
                .take_while(|ch| is_base64_char(*ch))
                .count();
            if count >= 32 {
                out.push_str("<FINGERPRINT>");
                rest = &rest[7 + count..];
                continue;
            }
        }
        let count = rest.chars().take_while(|ch| is_base64_char(*ch)).count();
        if count >= 40 {
            out.push_str("<KEY_OR_TOKEN>");
            rest = &rest[count..];
        } else {
            let ch = rest.chars().next().expect("nonempty rest");
            out.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
    out
}

fn is_base64_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '+' | '/' | '=')
}

fn has_token_shaped_text(input: &str) -> bool {
    input
        .split(|ch: char| !is_base64_char(ch))
        .any(|word| word.len() >= 40)
}

fn invitation_join_line(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let command = stdout
        .lines()
        .find(|line| line.starts_with("  remuda cluster join "))
        .expect("invite prints its join command");
    command
        .split('\'')
        .nth(3)
        .expect("join line is shell quoted")
        .to_owned()
}

fn invitation_command_args(output: &Output) -> (String, String) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let command = stdout
        .lines()
        .find(|line| line.starts_with("  remuda cluster join "))
        .expect("invite prints its join command")
        .trim();
    let command = command
        .strip_prefix("remuda cluster join '")
        .and_then(|command| command.strip_suffix('\''))
        .expect("invite command is fully quoted");
    let (fingerprint, line) = command
        .split_once("' '")
        .expect("invite command has fingerprint and line arguments");
    (fingerprint.to_owned(), line.to_owned())
}

fn add_peer_for_revoke(local: &Scratch, peer: &Scratch) -> String {
    use remuda_native::cluster::{encoding, AuthorizedNode, NodeState, Registry};

    let state = local.root.join("state/remuda/cluster");
    let mut registry: Registry = serde_json::from_slice(
        &fs::read(state.join("authorized_nodes.json")).expect("local registry"),
    )
    .expect("decode local registry");
    let peer_key =
        fs::read(peer.root.join("state/remuda/cluster/identity.key")).expect("peer identity key");
    let peer_public = &peer_key[32..];
    let peer_fp = encoding::fingerprint(peer_public);
    let local_key =
        fs::read(local.root.join("state/remuda/cluster/identity.key")).expect("local identity key");
    let local_fp = encoding::fingerprint(&local_key[32..]);
    registry.authorized_nodes.push(AuthorizedNode {
        node_fp: peer_fp.clone(),
        static_pubkey: encoding::encode_base64(peer_public),
        format_major: remuda_native::cluster::registry::REGISTRY_FORMAT_MAJOR,
        format_minor: remuda_native::cluster::registry::REGISTRY_FORMAT_MINOR,
        endpoint: None,
        delivered_by: None,
        state: NodeState::Admitted,
        version: 1,
        by: local_fp,
        optional_fields: Default::default(),
    });
    fs::write(
        state.join("authorized_nodes.json"),
        serde_json::to_vec_pretty(&registry).expect("encode registry"),
    )
    .expect("save peer registry entry");
    remuda_native::cluster::node_label(&peer_fp)
}

fn run_tty_cancel_revoke(scratch: &Scratch, node: &str) -> Output {
    use std::io;
    use std::ptr;

    let mut master_fd = -1;
    let mut slave_fd = -1;
    // SAFETY: openpty initializes both descriptors on success; ownership is
    // transferred to File immediately below.
    let opened = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    assert_eq!(opened, 0, "open revoke confirmation pty");
    // SAFETY: both descriptors were returned by successful openpty.
    let master = unsafe { fs::File::from_raw_fd(master_fd) };
    // SAFETY: dup returns a new descriptor owned by its File.
    let stdin_fd = unsafe { libc::dup(slave_fd) };
    assert!(stdin_fd >= 0, "duplicate pty for stdin");
    // SAFETY: the original descriptor is owned by this File.
    let stderr = unsafe { fs::File::from_raw_fd(slave_fd) };
    // SAFETY: the duplicate descriptor is owned by this File.
    let stdin = unsafe { fs::File::from_raw_fd(stdin_fd) };
    let mut command = scratch.command(&["cluster", "revoke", node]);
    command
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr));
    let mut child = command.spawn().expect("spawn revoke with a terminal");
    drop(command);
    // SAFETY: fcntl only changes the flags on this owned pty master descriptor.
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0, "read pty flags");
    // SAFETY: same owned descriptor; preserve existing flags and enable nonblocking reads.
    let changed =
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
    assert!(changed >= 0, "set pty nonblocking");
    let mut input = master.try_clone().expect("clone pty master");
    let mut terminal_stderr = Vec::new();
    let mut byte = [0; 128];
    let mut status = None;
    let mut answered = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "revoke confirmation did not finish"
        );
        match (&master).read(&mut byte) {
            Ok(0) => break,
            Ok(size) => {
                terminal_stderr.extend_from_slice(&byte[..size]);
                if !answered && String::from_utf8_lossy(&terminal_stderr).contains("[y/N] ") {
                    input.write_all(b"n\n").expect("answer no to revoke");
                    answered = true;
                }
            }
            Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if status.is_none() {
                    status = child.try_wait().expect("check revoke prompt");
                } else {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(error) => panic!("read revoke terminal: {error}"),
        }
    }
    assert!(answered, "revoke confirmation prompt was not observed");
    drop(input);
    let status = status.unwrap_or_else(|| child.wait().expect("wait for revoke prompt"));
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .expect("stdout pipe")
        .read_to_end(&mut stdout)
        .expect("read revoke stdout");
    Output {
        status,
        stdout,
        stderr: terminal_stderr,
    }
}

#[test]
fn normalizer_self_test_removes_token_shaped_material() {
    let root = Path::new("/private/tmp/cluster-golden-self-test");
    let token = "KRBAdTaMOXo+UrPjtDzDXoXS0lgffkokWwzccQvlXIU=";
    let normalized = normalize(
        &format!("remuda-join-v1 127.0.0.1:0 SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA= {token}"),
        root,
    );
    assert!(!normalized.contains(token));
    assert!(!has_token_shaped_text(&normalized));
    let listener = normalize(
        "Listening on 192.168.100.100:7441 (only admitted machines can connect)\nListener: on [fd00::1]:7441 (auto)",
        root,
    );
    assert_eq!(
        listener,
        "Listening on <LISTEN_ADDR> (only admitted machines can connect)\nListener: on <LISTEN_ADDR> (auto)"
    );
}

#[test]
fn cluster_cli_output_matches_goldens() {
    let scratch = Scratch::new();
    let _daemon = start_daemon(&scratch);
    golden_uninitialized_cases(&scratch);
    golden_initialized_cases(&scratch);
    golden_invite_join_cases(&scratch);
    golden_revoke_cancel_case(&scratch);
    golden_remaining_verb_errors(&scratch);
}

#[test]
fn listen_off_without_daemon_saves_config_without_starting_one() {
    use remuda_native::cluster::listener_config::{self, ListenerBind, ListenerConfig};

    let scratch = Scratch::new();
    let daemon = start_daemon(&scratch);
    assert!(scratch
        .run(&["cluster", "init", "--no-listen"])
        .status
        .success());
    let original = ListenerConfig {
        enabled: true,
        bind: ListenerBind::Explicit("127.0.0.1:0".parse().unwrap()),
        allow_public: true,
    };
    let cluster_dir = scratch.root.join("state/remuda/cluster");
    listener_config::write_at(&cluster_dir, &original).unwrap();
    drop(daemon);

    let output = scratch.run(&["cluster", "listen", "--off"]);
    assert!(output.status.success(), "listen --off failed: {output:?}");
    assert_eq!(
        listener_config::read_at(&cluster_dir).unwrap(),
        Some(ListenerConfig {
            enabled: false,
            allow_public: false,
            ..original
        })
    );
    assert!(String::from_utf8_lossy(&output.stdout)
        .contains("Cluster listener stays off (daemon not running)."));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Next:"));
    assert!(
        remuda_native::ipc::connect(&remuda_native::daemon::socket_path_in(
            &scratch.runtime,
            &scratch.name
        ))
        .is_err()
    );
}

fn golden_uninitialized_cases(scratch: &Scratch) {
    golden(
        "status_uninitialized",
        &scratch.run(&["cluster"]),
        &scratch.root,
    );
    golden("help", &scratch.run(&["cluster", "help"]), &scratch.root);
    golden(
        "unknown_verb",
        &scratch.run(&["cluster", "wat"]),
        &scratch.root,
    );
    golden(
        "nodes_uninitialized",
        &scratch.run(&["cluster", "nodes"]),
        &scratch.root,
    );
    golden(
        "revoke_uninitialized",
        &scratch.run(&["cluster", "revoke", "node-missing", "--yes"]),
        &scratch.root,
    );
}

fn golden_initialized_cases(scratch: &Scratch) {
    golden(
        "init",
        &scratch.run(&["cluster", "init", "--no-listen"]),
        &scratch.root,
    );
    golden(
        "init_existing",
        &scratch.run(&["cluster", "init", "--no-listen"]),
        &scratch.root,
    );
    golden(
        "status_initialized",
        &scratch.run(&["cluster"]),
        &scratch.root,
    );
    golden("nodes", &scratch.run(&["cluster", "nodes"]), &scratch.root);
}

fn golden_invite_join_cases(scratch: &Scratch) {
    let reservation = TcpListener::bind("127.0.0.1:0").expect("reserve invite port");
    let address = reservation.local_addr().expect("read invite port");
    drop(reservation);
    let address = address.to_string();
    let invite = scratch.run(&["cluster", "invite", "--bind", &address]);
    golden("invite", &invite, &scratch.root);
    golden(
        "invite_usage",
        &scratch.run(&["cluster", "invite"]),
        &scratch.root,
    );
    golden(
        "join_usage",
        &scratch.run(&["cluster", "join"]),
        &scratch.root,
    );
    golden(
        "join_invalid_line",
        &scratch.run(&["cluster", "join", "not-a-join-line"]),
        &scratch.root,
    );
    let join_line = invitation_join_line(&invite);
    golden(
        "join_unpinned_non_tty",
        &scratch.run(&["cluster", "join", &join_line]),
        &scratch.root,
    );
    golden(
        "join_pin_mismatch",
        &scratch.run(&["cluster", "join", "SHA256:wrong", &join_line]),
        &scratch.root,
    );
}

fn golden_revoke_cancel_case(scratch: &Scratch) {
    golden(
        "nodes_usage",
        &scratch.run(&["cluster", "nodes", "extra"]),
        &scratch.root,
    );
    golden(
        "revoke_usage",
        &scratch.run(&["cluster", "revoke"]),
        &scratch.root,
    );
    golden(
        "revoke_unknown_node",
        &scratch.run(&["cluster", "revoke", "node-missing", "--yes"]),
        &scratch.root,
    );
    let peer = Scratch::new();
    let _peer_daemon = start_daemon(&peer);
    peer.run(&["cluster", "init", "--no-listen"]);
    let peer_label = add_peer_for_revoke(scratch, &peer);
    golden(
        "revoke_cancel",
        &run_tty_cancel_revoke(scratch, &peer_label),
        &scratch.root,
    );
}

fn golden_remaining_verb_errors(scratch: &Scratch) {
    golden(
        "control_off",
        &scratch.run(&["cluster", "control", "off"]),
        &scratch.root,
    );
    golden(
        "control_on",
        &scratch.run(&["cluster", "control", "on"]),
        &scratch.root,
    );
    golden(
        "control_usage",
        &scratch.run(&["cluster", "control", "maybe"]),
        &scratch.root,
    );
    golden(
        "remote_argument",
        &scratch.run(&["cluster", "remote", "not-a-target"]),
        &scratch.root,
    );
    golden(
        "listen_argument",
        &scratch.run(&["cluster", "listen", "--bind"]),
        &scratch.root,
    );
    golden(
        "listen_invalid_address",
        &scratch.run(&["cluster", "listen", "--bind", "bad-address"]),
        &scratch.root,
    );
    golden(
        "listen_off",
        &scratch.run(&["cluster", "listen", "--off"]),
        &scratch.root,
    );
    golden(
        "call_usage",
        &scratch.run(&["cluster", "call", "node-example", "list"]),
        &scratch.root,
    );
    golden(
        "call_error_4",
        &scratch.run(&[
            "cluster",
            "call",
            "node-missing",
            "list",
            "--addr",
            "127.0.0.1:9",
        ]),
        &scratch.root,
    );
}

#[test]
fn cluster_join_and_peer_call_outputs_match_goldens() {
    use remuda_native::cluster::Registry;

    let inviter = Scratch::new();
    let _daemon = start_daemon(&inviter);
    let initialized = inviter.run(&["cluster", "init", "--no-listen"]);
    assert!(
        initialized.status.success(),
        "inviter init failed: {initialized:?}"
    );
    let reservation = TcpListener::bind("127.0.0.1:0").expect("reserve inviter port");
    let address = reservation.local_addr().expect("read reserved address");
    drop(reservation);
    let address_text = address.to_string();
    let invite = inviter.run(&["cluster", "invite", "--bind", &address_text]);
    assert!(invite.status.success(), "invite failed: {invite:?}");
    let (fingerprint, join_line) = invitation_command_args(&invite);

    let joiner = Scratch::new();
    let _joiner_daemon = start_daemon(&joiner);
    let initialized = joiner.run(&["cluster", "init", "--no-listen"]);
    assert!(
        initialized.status.success(),
        "joiner init failed: {initialized:?}"
    );
    configure_listener(&joiner, false, "127.0.0.1:0".parse().unwrap());
    let joined = joiner.run(&["cluster", "join", &fingerprint, &join_line]);
    golden("join_success", &joined, &joiner.root);

    let label = remuda_native::cluster::node_label(&fingerprint);
    let unreachable = joiner.run(&["cluster", "call", &label, "list", "--addr", "127.0.0.1:9"]);
    golden("call_error_3", &unreachable, &joiner.root);

    let registry_path = joiner
        .root
        .join("state/remuda/cluster/authorized_nodes.json");
    let mut registry: Registry =
        serde_json::from_slice(&fs::read(&registry_path).expect("joined registry"))
            .expect("decode joined registry");
    let joiner_key = fs::read(joiner.root.join("state/remuda/cluster/identity.key"))
        .expect("joiner identity key");
    let wrong_pin = remuda_native::cluster::encoding::encode_base64(&joiner_key[32..]);
    registry
        .authorized_nodes
        .iter_mut()
        .find(|entry| entry.node_fp == fingerprint)
        .expect("inviter registry entry")
        .static_pubkey = wrong_pin;
    fs::write(
        &registry_path,
        serde_json::to_vec_pretty(&registry).expect("encode joined registry"),
    )
    .expect("write modified registry");
    let bad_auth = joiner.run(&["cluster", "call", &label, "list", "--addr", &address_text]);
    golden("call_error_5", &bad_auth, &joiner.root);
}

#[test]
fn rerunning_a_join_line_on_a_member_says_already_a_member_without_network() {
    let inviter = Scratch::new();
    let inviter_daemon = initialized_node(&inviter);
    let reservation = TcpListener::bind("127.0.0.1:0").expect("reserve inviter port");
    let address = reservation.local_addr().expect("read reserved address");
    drop(reservation);
    let invite = inviter.run(&["cluster", "invite", "--bind", &address.to_string()]);
    assert!(invite.status.success(), "invite failed: {invite:?}");
    let (fingerprint, join_line) = invitation_command_args(&invite);

    let joiner = Scratch::new();
    let _joiner_daemon = initialized_node(&joiner);
    let joined = joiner.run(&["cluster", "join", &fingerprint, &join_line]);
    assert!(joined.status.success(), "first join failed: {joined:?}");

    // The issuer is gone: any network attempt would fail, so success proves none.
    drop(inviter_daemon);
    let again = joiner.run(&["cluster", "join", &fingerprint, &join_line]);
    let stdout = String::from_utf8_lossy(&again.stdout);
    let stderr = String::from_utf8_lossy(&again.stderr);
    assert!(again.status.success(), "rerun should be a no-op: {again:?}");
    let label = remuda_native::cluster::node_label(&fingerprint);
    assert!(
        stdout.contains(&format!("Already a member of {label}'s cluster."))
            && stdout.contains("Next: remuda cluster nodes"),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stderr.contains("invalid join response"),
        "stderr: {stderr}"
    );
}

#[test]
fn rerunning_a_join_line_on_a_revoked_node_prints_the_revocation_notice() {
    use std::os::unix::fs::OpenOptionsExt;

    let inviter = Scratch::new();
    let inviter_daemon = initialized_node(&inviter);
    let reservation = TcpListener::bind("127.0.0.1:0").expect("reserve inviter port");
    let address = reservation.local_addr().expect("read reserved address");
    drop(reservation);
    let invite = inviter.run(&["cluster", "invite", "--bind", &address.to_string()]);
    assert!(invite.status.success(), "invite failed: {invite:?}");
    let (fingerprint, join_line) = invitation_command_args(&invite);

    let joiner = Scratch::new();
    let _joiner_daemon = initialized_node(&joiner);
    let joined = joiner.run(&["cluster", "join", &fingerprint, &join_line]);
    assert!(joined.status.success(), "first join failed: {joined:?}");

    // A node never stores its own tombstone; the notice is all it has.
    let notice = serde_json::json!({ "by_fp": fingerprint, "at": "1" });
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(joiner.root.join("state/remuda/cluster/revoked_notice.json"))
        .expect("create revoked notice")
        .write_all(notice.to_string().as_bytes())
        .expect("write revoked notice");

    drop(inviter_daemon);
    let again = joiner.run(&["cluster", "join", &fingerprint, &join_line]);
    let stdout = String::from_utf8_lossy(&again.stdout);
    let stderr = String::from_utf8_lossy(&again.stderr);
    assert!(
        !again.status.success(),
        "a revoked node is not a member: {again:?}"
    );
    assert!(!stdout.contains("Already a member"), "stdout: {stdout}");
    assert!(
        stderr.contains("This node was revoked by")
            && stderr.contains("Next: run `remuda cluster init --new-identity`"),
        "stderr: {stderr}"
    );
}

#[test]
fn a_revoked_issuer_entry_is_not_treated_as_membership() {
    use remuda_native::cluster::{NodeState, Registry};

    let inviter = Scratch::new();
    let inviter_daemon = initialized_node(&inviter);
    let reservation = TcpListener::bind("127.0.0.1:0").expect("reserve inviter port");
    let address = reservation.local_addr().expect("read reserved address");
    drop(reservation);
    let invite = inviter.run(&["cluster", "invite", "--bind", &address.to_string()]);
    assert!(invite.status.success(), "invite failed: {invite:?}");
    let (fingerprint, join_line) = invitation_command_args(&invite);

    let joiner = Scratch::new();
    let _joiner_daemon = initialized_node(&joiner);
    let joined = joiner.run(&["cluster", "join", &fingerprint, &join_line]);
    assert!(joined.status.success(), "first join failed: {joined:?}");

    let path = joiner
        .root
        .join("state/remuda/cluster/authorized_nodes.json");
    let mut registry: Registry =
        serde_json::from_slice(&fs::read(&path).expect("joiner registry")).expect("decode");
    let issuer = registry
        .authorized_nodes
        .iter_mut()
        .find(|entry| entry.node_fp == fingerprint)
        .expect("issuer entry after join");
    issuer.state = NodeState::Revoked;
    fs::write(&path, serde_json::to_vec_pretty(&registry).expect("encode")).expect("save");

    // The issuer is gone, so the old (network) path must fail.
    drop(inviter_daemon);
    let again = joiner.run(&["cluster", "join", &fingerprint, &join_line]);
    let stdout = String::from_utf8_lossy(&again.stdout);
    assert!(!stdout.contains("Already a member"), "stdout: {stdout}");
    assert!(
        !again.status.success(),
        "a revoked issuer is not membership: {again:?}"
    );
}

#[test]
fn failed_join_restores_the_exact_saved_listener_config() {
    use remuda_native::cluster::listener_config::{ListenerBind, ListenerConfig};

    let inviter = Scratch::new();
    let inviter_daemon = start_daemon(&inviter);
    let initialized = inviter.run(&["cluster", "init", "--no-listen"]);
    assert!(initialized.status.success(), "init failed: {initialized:?}");
    let invite = inviter.run(&["cluster", "invite", "--bind", "127.0.0.1:0"]);
    assert!(invite.status.success(), "invite failed: {invite:?}");
    let (fingerprint, join_line) = invitation_command_args(&invite);
    drop(inviter_daemon);

    let missing_file_result =
        failed_join_with_listener_config(&fingerprint, &join_line, None, false, false);
    let original_addr = "127.0.0.1:0".parse().unwrap();
    let saved = ListenerConfig {
        enabled: true,
        bind: ListenerBind::Explicit(original_addr),
        allow_public: false,
    };
    let saved_file_result = failed_join_with_listener_config(
        &fingerprint,
        &join_line,
        Some(saved.clone()),
        true,
        false,
    );

    assert_eq!(
        saved_file_result,
        Some(saved),
        "failure changed the saved listener config"
    );
    assert_eq!(missing_file_result, None, "failure created listener.json");

    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let failed_status_saved = ListenerConfig {
        enabled: true,
        bind: ListenerBind::Explicit(occupied.local_addr().unwrap()),
        allow_public: false,
    };
    let failed_status_result = failed_join_with_listener_config(
        &fingerprint,
        &join_line,
        Some(failed_status_saved.clone()),
        true,
        true,
    );
    assert_eq!(
        failed_status_result,
        Some(failed_status_saved),
        "failure disabled or changed an enabled config whose listener status was Failed"
    );
}

#[test]
fn failed_join_preserves_listener_change_made_while_join_is_blocked() {
    use remuda_native::cluster::{encoding, join_line::JoinLine, listener_config};
    use std::process::Stdio;
    use zeroize::Zeroizing;

    struct ChildGuard(Option<std::process::Child>);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if let Some(child) = self.0.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    let scratch = Scratch::new();
    let _daemon = initialized_node(&scratch);
    let cluster_dir = scratch.root.join("state/remuda/cluster");
    let fake_issuer = TcpListener::bind("127.0.0.1:0").expect("bind stalled fake issuer");
    let issuer_addr = fake_issuer.local_addr().unwrap();
    let key = fs::read(cluster_dir.join("identity.key")).unwrap();
    let issuer_public: [u8; 32] = key[32..].try_into().unwrap();
    let fingerprint = encoding::fingerprint(&issuer_public);
    let invitation = JoinLine {
        issuer_addr,
        issuer_fingerprint: fingerprint.clone(),
        issuer_static_pubkey: issuer_public,
        token: Zeroizing::new(encoding::encode_base64(&[7; 32])),
    }
    .encode()
    .unwrap();
    let (connected_tx, connected_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (stream, _) = fake_issuer.accept().expect("accept join connection");
        connected_tx.send(()).unwrap();
        release_rx
            .recv_timeout(Duration::from_secs(8))
            .expect("test did not release stalled issuer");
        drop(stream);
    });
    let join_bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut child = ChildGuard(Some(
        scratch
            .command(&[
                "cluster",
                "join",
                &fingerprint,
                &invitation,
                "--bind",
                &join_bind.to_string(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start isolated join command"),
    ));
    connected_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("join never connected to stalled issuer");

    match remuda_native::cluster::listener_control::status(&remuda_native::daemon::socket_path_in(
        &scratch.runtime,
        &scratch.name,
    )) {
        remuda_core::protocol::ListenerStatus::On { addr, .. } => {
            assert_eq!(addr.ip(), join_bind.ip());
            assert_ne!(addr.port(), 0, "listener did not report its assigned port");
        }
        status => panic!("join did not start its listener: {status:?}"),
    }

    let stopped = scratch.run(&["cluster", "listen", "--off"]);
    assert!(
        stopped.status.success(),
        "concurrent listen --off failed: {stopped:?}"
    );
    let changed = listener_config::read_at(&cluster_dir).unwrap();
    assert_eq!(
        changed,
        Some(remuda_native::cluster::listener_config::ListenerConfig {
            enabled: false,
            bind: remuda_native::cluster::listener_config::ListenerBind::Explicit(join_bind),
            allow_public: false,
        })
    );

    release_tx.send(()).unwrap();
    let joined = child.0.take().unwrap().wait_with_output().unwrap();
    server.join().unwrap();
    assert!(
        !joined.status.success(),
        "stalled fake issuer unexpectedly joined"
    );
    assert!(
        String::from_utf8_lossy(&joined.stderr).lines().any(|line| {
            line == "cluster join: listener config changed during join; leaving the current config unchanged. Next: check `remuda cluster`."
        }),
        "missing one-line concurrent-change note: {}",
        String::from_utf8_lossy(&joined.stderr)
    );
    assert_eq!(
        listener_config::read_at(&cluster_dir).unwrap(),
        changed,
        "failed join overwrote the listener change made while it was blocked"
    );
}

fn failed_join_with_listener_config(
    fingerprint: &str,
    join_line: &str,
    initial: Option<remuda_native::cluster::listener_config::ListenerConfig>,
    enable_initial: bool,
    expect_failed_initial: bool,
) -> Option<remuda_native::cluster::listener_config::ListenerConfig> {
    use remuda_native::cluster::listener_config;

    let joiner = Scratch::new();
    let _joiner_daemon = start_daemon(&joiner);
    assert!(joiner
        .run(&["cluster", "init", "--no-listen"])
        .status
        .success());
    let cluster_dir = joiner.root.join("state/remuda/cluster");
    let config_path = cluster_dir.join("listener.json");
    match initial {
        Some(ref config) => listener_config::write_at(&cluster_dir, config).unwrap(),
        None => fs::remove_file(&config_path).unwrap(),
    }
    if enable_initial {
        let daemon_path = remuda_native::daemon::socket_path_in(&joiner.runtime, &joiner.name);
        let response = remuda_native::client::request(
            &daemon_path,
            &remuda_core::protocol::Request::ClusterListener(
                remuda_core::protocol::ListenerOp::Reload,
            ),
        )
        .unwrap();
        match response {
            remuda_core::protocol::Response::ClusterListenerStatus(status) => {
                assert_eq!(
                    matches!(status, remuda_core::protocol::ListenerStatus::Failed(_)),
                    expect_failed_initial,
                    "unexpected initial listener status: {status:?}"
                );
            }
            response => panic!("unexpected listener reload response: {response:?}"),
        }
    }

    let replacement_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let failed = joiner.run(&[
        "cluster",
        "join",
        fingerprint,
        join_line,
        "--bind",
        &replacement_addr.to_string(),
    ]);
    assert!(
        !failed.status.success(),
        "join to stopped inviter succeeded: {failed:?}"
    );
    listener_config::read_at(&cluster_dir).unwrap()
}

fn initialized_node(scratch: &Scratch) -> TrackedChild {
    let daemon = start_daemon(scratch);
    let initialized = scratch.run(&["cluster", "init", "--no-listen"]);
    assert!(
        initialized.status.success(),
        "cluster init failed: {}",
        String::from_utf8_lossy(&initialized.stderr)
    );
    assert_eq!(
        configure_listener(scratch, false, "127.0.0.1:0".parse().unwrap()),
        remuda_core::protocol::ListenerStatus::Off
    );
    daemon
}

fn configure_listener(
    scratch: &Scratch,
    enabled: bool,
    address: SocketAddr,
) -> remuda_core::protocol::ListenerStatus {
    use remuda_core::protocol::{ListenerOp, Request, Response};
    use remuda_native::cluster::listener_config::{self, ListenerBind, ListenerConfig};

    listener_config::write_at(
        &scratch.root.join("state/remuda/cluster"),
        &ListenerConfig {
            enabled,
            bind: ListenerBind::Explicit(address),
            allow_public: false,
        },
    )
    .expect("write isolated listener configuration");
    match remuda_native::client::request(
        &remuda_native::daemon::socket_path_in(&scratch.runtime, &scratch.name),
        &Request::ClusterListener(ListenerOp::Reload),
    )
    .expect("reload isolated listener")
    {
        Response::ClusterListenerStatus(status) => status,
        response => panic!("unexpected listener reload response: {response:?}"),
    }
}

fn explicit_listener_address(scratch: &Scratch) -> SocketAddr {
    use remuda_core::protocol::ListenerStatus;

    match configure_listener(scratch, true, "127.0.0.1:0".parse().unwrap()) {
        ListenerStatus::On { addr, .. } => addr,
        status => panic!("expected listener On, got {status:?}"),
    }
}

#[test]
fn d4_invite_without_flags_uses_the_daemon_bound_address() {
    use remuda_native::cluster::join_line::JoinLine;

    let scratch = Scratch::new();
    let _daemon = initialized_node(&scratch);
    let bound = explicit_listener_address(&scratch);
    let invite = scratch.run(&["cluster", "invite"]);
    assert!(
        invite.status.success(),
        "invite failed: {}",
        String::from_utf8_lossy(&invite.stderr)
    );
    let line = JoinLine::decode(&invitation_join_line(&invite)).expect("decode invite line");
    assert_eq!(line.issuer_addr, bound);
}

#[test]
fn d4_invite_addr_overrides_only_the_advertised_address() {
    use remuda_native::cluster::join_line::JoinLine;

    let scratch = Scratch::new();
    let _daemon = initialized_node(&scratch);
    let bound = explicit_listener_address(&scratch);
    let advertised: SocketAddr = "203.0.113.9:9443".parse().unwrap();
    let advertised_text = advertised.to_string();
    let invite = scratch.run(&["cluster", "invite", "--addr", &advertised_text]);
    assert!(
        invite.status.success(),
        "invite failed: {}",
        String::from_utf8_lossy(&invite.stderr)
    );
    let line = JoinLine::decode(&invitation_join_line(&invite)).expect("decode invite line");
    assert_eq!(line.issuer_addr, advertised);
    assert_eq!(
        remuda_native::cluster::listener_control::status(&remuda_native::daemon::socket_path_in(
            &scratch.runtime,
            &scratch.name
        )),
        remuda_core::protocol::ListenerStatus::On {
            addr: bound,
            auto: false,
            advertise_addr: Some(bound),
            listen_addrs: vec![bound],
        }
    );
}

#[test]
fn d4_invite_refuses_a_failed_listener_without_printing_a_join_line() {
    use remuda_core::protocol::ListenerStatus;
    use std::net::TcpListener;

    let scratch = Scratch::new();
    let _daemon = initialized_node(&scratch);
    let blocker = TcpListener::bind("127.0.0.1:0").expect("reserve a blocked address");
    let status = configure_listener(&scratch, true, blocker.local_addr().unwrap());
    assert!(
        matches!(status, ListenerStatus::Failed(_)),
        "expected Failed, got {status:?}"
    );

    let invite = scratch.run(&["cluster", "invite"]);
    let stdout = String::from_utf8_lossy(&invite.stdout);
    let stderr = String::from_utf8_lossy(&invite.stderr);
    assert!(
        !invite.status.success(),
        "invite unexpectedly succeeded: {stdout}"
    );
    assert!(
        !stdout.contains("remuda-join-v1"),
        "failed invite printed a join line: {stdout}"
    );
    assert!(
        stderr.contains("remuda cluster listen"),
        "missing listener fix: {stderr}"
    );
}

#[test]
fn explicit_join_sends_the_joiners_advertised_address_to_the_issuer_registry() {
    use remuda_native::cluster::{encoding, Registry};

    let inviter = Scratch::new();
    let _inviter_daemon = initialized_node(&inviter);
    let issuer_addr = explicit_listener_address(&inviter);
    let issuer_addr_text = issuer_addr.to_string();
    let invite = inviter.run(&["cluster", "invite", "--bind", &issuer_addr_text]);
    assert!(invite.status.success(), "invite failed: {invite:?}");
    let (fingerprint, join_line) = invitation_command_args(&invite);

    let joiner = Scratch::new();
    let _joiner_daemon = initialized_node(&joiner);
    let joined = joiner.run(&[
        "cluster",
        "join",
        &fingerprint,
        &join_line,
        "--bind",
        "127.0.0.1:0",
    ]);
    assert!(
        joined.status.success(),
        "join failed: {}",
        String::from_utf8_lossy(&joined.stderr)
    );
    let (joiner_bind_addr, joiner_advertise_addr) =
        match remuda_native::cluster::listener_control::status(
            &remuda_native::daemon::socket_path_in(&joiner.runtime, &joiner.name),
        ) {
            remuda_core::protocol::ListenerStatus::On {
                addr,
                advertise_addr: Some(advertise_addr),
                auto: false,
                ..
            } => (addr, advertise_addr),
            status => panic!("join did not enable B's listener: {status:?}"),
        };
    assert_eq!(
        joiner_bind_addr.ip(),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );
    assert_eq!(joiner_bind_addr.port(), joiner_advertise_addr.port());

    let registry_path = inviter
        .root
        .join("state/remuda/cluster/authorized_nodes.json");
    let registry: Registry = serde_json::from_slice(&fs::read(registry_path).unwrap()).unwrap();
    let joiner_key = fs::read(joiner.root.join("state/remuda/cluster/identity.key")).unwrap();
    let joiner_fingerprint = encoding::fingerprint(&joiner_key[32..]);
    let entry = registry
        .authorized_nodes
        .iter()
        .find(|entry| entry.node_fp == joiner_fingerprint)
        .expect("joiner entry in issuer registry");
    assert_eq!(
        entry.endpoint.as_deref(),
        Some(joiner_advertise_addr.to_string().as_str())
    );
}

#[test]
fn d4_join_sends_the_joiners_bound_address_to_the_issuer_registry() {
    use remuda_native::cluster::{encoding, Registry};

    let inviter = Scratch::new();
    let _inviter_daemon = initialized_node(&inviter);
    let issuer_addr = explicit_listener_address(&inviter);
    let issuer_addr_text = issuer_addr.to_string();
    let invite = inviter.run(&["cluster", "invite", "--bind", &issuer_addr_text]);
    assert!(invite.status.success(), "invite failed: {invite:?}");
    let (fingerprint, join_line) = invitation_command_args(&invite);

    let joiner = Scratch::new();
    let _joiner_daemon = initialized_node(&joiner);
    let joined = joiner.run(&[
        "cluster",
        "join",
        &fingerprint,
        &join_line,
        "--bind",
        "127.0.0.1:0",
    ]);
    assert!(
        joined.status.success(),
        "join failed: {}",
        String::from_utf8_lossy(&joined.stderr)
    );
    let joiner_addr = match remuda_native::cluster::listener_control::status(
        &remuda_native::daemon::socket_path_in(&joiner.runtime, &joiner.name),
    ) {
        remuda_core::protocol::ListenerStatus::On {
            addr,
            auto: false,
            advertise_addr: Some(advertise_addr),
            ..
        } => {
            assert_eq!(
                advertise_addr, addr,
                "explicit bind must be advertised as-is"
            );
            addr
        }
        status => panic!("join did not enable B's listener: {status:?}"),
    };

    let registry_path = inviter
        .root
        .join("state/remuda/cluster/authorized_nodes.json");
    let registry: Registry = serde_json::from_slice(&fs::read(registry_path).unwrap()).unwrap();
    let joiner_key = fs::read(joiner.root.join("state/remuda/cluster/identity.key")).unwrap();
    let joiner_fingerprint = encoding::fingerprint(&joiner_key[32..]);
    let entry = registry
        .authorized_nodes
        .iter()
        .find(|entry| entry.node_fp == joiner_fingerprint)
        .expect("joiner entry in issuer registry");
    assert_eq!(
        entry.endpoint.as_deref(),
        Some(joiner_addr.to_string().as_str())
    );
}

#[test]
fn d4_explicit_listener_prints_the_listener_address_and_exposure_note() {
    let scratch = Scratch::new();
    let _daemon = start_daemon(&scratch);
    let initialized = scratch.run(&["cluster", "init", "--no-listen"]);
    assert!(initialized.status.success(), "init failed: {initialized:?}");
    let bound = explicit_listener_address(&scratch);
    let output = scratch.run(&["cluster"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("Listening on {bound}")),
        "missing listener line: {stdout}"
    );
}

#[test]
fn d4_init_no_listen_leaves_the_listener_off() {
    let scratch = Scratch::new();
    let _daemon = start_daemon(&scratch);
    let initialized = scratch.run(&["cluster", "init", "--no-listen"]);
    let stdout = String::from_utf8_lossy(&initialized.stdout);
    assert!(initialized.status.success(), "init failed: {stdout}");
    assert!(stdout.contains("Listener off (--no-listen)"), "{stdout}");
    assert_eq!(
        remuda_native::cluster::listener_control::status(&remuda_native::daemon::socket_path_in(
            &scratch.runtime,
            &scratch.name
        )),
        remuda_core::protocol::ListenerStatus::Off
    );
}

#[test]
fn cluster_listen_bind_configures_the_daemon_and_returns() {
    use remuda_core::protocol::ListenerStatus;
    use remuda_native::cluster::listener_config::{self, ListenerBind, ListenerConfig};
    use std::process::Stdio;

    let scratch = Scratch::new();
    let _daemon = start_daemon(&scratch);
    assert!(scratch
        .run(&["cluster", "init", "--no-listen"])
        .status
        .success());
    let bind_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let bind_text = bind_addr.to_string();
    let mut child = scratch
        .command(&["cluster", "listen", "--bind", &bind_text])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start cluster listen command");
    let deadline = Instant::now() + Duration::from_secs(3);
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let returned = child.try_wait().unwrap().is_some();
    if !returned {
        let _ = child.kill();
    }
    let output = child.wait_with_output().expect("wait for listen command");
    assert!(
        returned,
        "cluster listen --bind should return after daemon reload, got {output:?}"
    );
    assert!(output.status.success(), "listen failed: {output:?}");
    let bound_addr = match remuda_native::cluster::listener_control::status(
        &remuda_native::daemon::socket_path_in(&scratch.runtime, &scratch.name),
    ) {
        ListenerStatus::On {
            addr,
            auto: false,
            advertise_addr: Some(advertise_addr),
            listen_addrs,
        } => {
            assert_eq!(advertise_addr, addr);
            assert_eq!(listen_addrs, vec![addr]);
            addr
        }
        status => panic!("expected listener On, got {status:?}"),
    };
    assert_eq!(bound_addr.ip(), bind_addr.ip());
    assert_ne!(
        bound_addr.port(),
        0,
        "listener did not report its assigned port"
    );
    assert_eq!(
        listener_config::read_at(&scratch.root.join("state/remuda/cluster")).unwrap(),
        Some(ListenerConfig {
            enabled: true,
            bind: ListenerBind::Explicit(bind_addr),
            allow_public: false,
        })
    );
    let invite = scratch.run(&["cluster", "invite"]);
    assert!(
        invite.status.success(),
        "invite failed after listen: {invite:?}"
    );

    let stopped = scratch.run(&["cluster", "listen", "--off"]);
    assert!(stopped.status.success(), "listen --off failed: {stopped:?}");
    assert_foreground_listener_binds_and_stays_running(&scratch);
}

fn assert_foreground_listener_binds_and_stays_running(scratch: &Scratch) {
    use std::process::Stdio;

    let foreground_bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let foreground_text = foreground_bind.to_string();
    let child = scratch
        .command(&[
            "cluster",
            "listen",
            "--bind",
            &foreground_text,
            "--foreground",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start foreground listener command");
    let mut child = TrackedChild(child);
    let mut stderr = std::io::BufReader::new(child.0.stderr.take().unwrap());
    let mut address_line = String::new();
    stderr
        .read_line(&mut address_line)
        .expect("read foreground listener address");
    let foreground_addr: SocketAddr = address_line
        .trim()
        .strip_prefix("remuda: Listening on ")
        .expect("foreground listener address line")
        .parse()
        .expect("parse foreground listener address");
    assert_eq!(foreground_addr.ip(), foreground_bind.ip());
    assert_ne!(foreground_addr.port(), 0);
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut foreground_reachable = false;
    while Instant::now() < deadline {
        if std::net::TcpStream::connect_timeout(&foreground_addr, Duration::from_millis(50)).is_ok()
        {
            foreground_reachable = true;
            break;
        }
        if child.0.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        foreground_reachable,
        "--foreground did not bind {foreground_addr}"
    );
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "--foreground returned early"
    );
}

#[test]
fn listen_public_opt_in_persists_but_off_and_plain_bind_clear_it() {
    use remuda_core::protocol::ListenerStatus;
    use remuda_native::cluster::listener_config::{self, ListenerBind, ListenerConfig};

    let scratch = Scratch::new();
    let daemon = start_daemon(&scratch);
    assert!(scratch
        .run(&["cluster", "init", "--no-listen"])
        .status
        .success());
    let cluster_dir = scratch.root.join("state/remuda/cluster");
    let first_bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let first_text = first_bind.to_string();
    let public = scratch.run(&["cluster", "listen", "--bind", &first_text, "--allow-public"]);
    assert!(public.status.success(), "public opt-in failed: {public:?}");
    let first_addr = match remuda_native::cluster::listener_control::status(
        &remuda_native::daemon::socket_path_in(&scratch.runtime, &scratch.name),
    ) {
        ListenerStatus::On { addr, .. } => addr,
        status => panic!("public listener did not start: {status:?}"),
    };
    assert_eq!(first_addr.ip(), first_bind.ip());
    assert_ne!(first_addr.port(), 0);
    assert_eq!(
        listener_config::read_at(&cluster_dir).unwrap(),
        Some(ListenerConfig {
            enabled: true,
            bind: ListenerBind::Explicit(first_bind),
            allow_public: true,
        })
    );

    drop(daemon);
    let _restarted_daemon = start_daemon(&scratch);
    let daemon_path = remuda_native::daemon::socket_path_in(&scratch.runtime, &scratch.name);
    match remuda_native::cluster::listener_control::status(&daemon_path) {
        ListenerStatus::On {
            addr,
            auto: false,
            advertise_addr: Some(advertise_addr),
            listen_addrs,
        } => {
            assert_eq!(addr.ip(), first_bind.ip());
            assert_ne!(addr.port(), 0);
            assert_eq!(advertise_addr, addr);
            assert_eq!(listen_addrs, vec![addr]);
        }
        status => panic!("saved listener did not reload after restart: {status:?}"),
    }
    let invite = scratch.run(&["cluster", "invite"]);
    assert!(invite.status.success(), "no-flag invite failed: {invite:?}");
    assert!(
        listener_config::read_at(&cluster_dir)
            .unwrap()
            .unwrap()
            .allow_public,
        "no-flag invite should reuse the saved opt-in"
    );

    let second_bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let second_text = second_bind.to_string();
    let private_bind = scratch.run(&["cluster", "listen", "--bind", &second_text]);
    assert!(
        private_bind.status.success(),
        "plain bind failed: {private_bind:?}"
    );
    let second_addr = match remuda_native::cluster::listener_control::status(&daemon_path) {
        ListenerStatus::On { addr, .. } => addr,
        status => panic!("plain listener did not start: {status:?}"),
    };
    assert_eq!(second_addr.ip(), second_bind.ip());
    assert_ne!(second_addr.port(), 0);
    assert_eq!(
        listener_config::read_at(&cluster_dir).unwrap(),
        Some(ListenerConfig {
            enabled: true,
            bind: ListenerBind::Explicit(second_bind),
            allow_public: false,
        })
    );

    let public_again = scratch.run(&[
        "cluster",
        "listen",
        "--bind",
        &second_text,
        "--allow-public",
    ]);
    assert!(public_again.status.success());
    let stopped = scratch.run(&["cluster", "listen", "--off"]);
    assert!(stopped.status.success(), "listen --off failed: {stopped:?}");
    let saved_after_off = listener_config::read_at(&cluster_dir).unwrap().unwrap();
    assert!(!saved_after_off.enabled);
    assert!(!saved_after_off.allow_public);
    assert_eq!(
        remuda_native::cluster::listener_control::status(&daemon_path),
        ListenerStatus::Off
    );
}

#[test]
fn d4_failed_join_turns_off_a_listener_enabled_by_the_join_command() {
    use remuda_core::protocol::ListenerStatus;
    use remuda_native::cluster::{encoding, join_line::JoinLine};
    use std::net::TcpListener;
    use std::process::Stdio;
    use zeroize::Zeroizing;

    let scratch = Scratch::new();
    let _daemon = initialized_node(&scratch);
    let bound = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    assert_eq!(
        configure_listener(&scratch, false, bound),
        ListenerStatus::Off,
        "listener must be off before join"
    );
    let fake_issuer = TcpListener::bind("127.0.0.1:0").expect("bind delayed fake issuer");
    let issuer_addr = fake_issuer.local_addr().unwrap();
    let joiner_key = fs::read(scratch.root.join("state/remuda/cluster/identity.key")).unwrap();
    let issuer_public: [u8; 32] = joiner_key[32..].try_into().unwrap();
    let fingerprint = encoding::fingerprint(&issuer_public);
    let invitation = JoinLine {
        issuer_addr,
        issuer_fingerprint: fingerprint.clone(),
        issuer_static_pubkey: issuer_public,
        token: Zeroizing::new(encoding::encode_base64(&[7; 32])),
    }
    .encode()
    .unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = fake_issuer.accept().expect("accept failed join");
        std::thread::sleep(Duration::from_millis(750));
        stream
            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .expect("reject failed join");
    });
    let mut child = scratch
        .command(&[
            "cluster",
            "join",
            &fingerprint,
            &invitation,
            "--bind",
            "127.0.0.1:0",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start isolated join command");
    let daemon_path = remuda_native::daemon::socket_path_in(&scratch.runtime, &scratch.name);
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut saw_listener_on = false;
    while Instant::now() < deadline {
        if let ListenerStatus::On { .. } =
            remuda_native::cluster::listener_control::status(&daemon_path)
        {
            saw_listener_on = true;
            break;
        }
        if child.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let joined = child.wait_with_output().expect("wait for failed join");
    server.join().expect("fake issuer thread");
    assert!(
        saw_listener_on,
        "join never enabled B's listener before the failure"
    );
    assert!(
        !joined.status.success(),
        "fake issuer unexpectedly accepted join"
    );
    assert_eq!(
        remuda_native::cluster::listener_control::status(&daemon_path),
        ListenerStatus::Off,
        "failed join left its newly enabled listener running"
    );
}

#[test]
fn d4_sigint_during_join_restores_listener_config_and_exits_130() {
    assert_join_signal_restores_listener_config(libc::SIGINT, 130);
}

#[test]
fn d4_sigterm_during_join_restores_listener_config_and_exits_143() {
    assert_join_signal_restores_listener_config(libc::SIGTERM, 143);
}

#[test]
fn d4_sighup_during_join_restores_listener_config_and_exits_129() {
    assert_join_signal_restores_listener_config(libc::SIGHUP, 129);
}

fn assert_join_signal_restores_listener_config(signal: libc::c_int, expected_exit: i32) {
    use remuda_core::protocol::ListenerStatus;
    use remuda_native::cluster::{encoding, join_line::JoinLine, listener_config};
    use std::process::Stdio;
    use zeroize::Zeroizing;

    struct ChildGuard(Option<std::process::Child>);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if let Some(child) = self.0.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    let scratch = Scratch::new();
    let _daemon = initialized_node(&scratch);
    let cluster_dir = scratch.root.join("state/remuda/cluster");
    let saved_config = listener_config::read_at(&cluster_dir).unwrap();
    assert!(matches!(saved_config, Some(ref config) if !config.enabled));

    let fake_issuer = TcpListener::bind("127.0.0.1:0").expect("bind fake issuer");
    fake_issuer.set_nonblocking(true).unwrap();
    let issuer_addr = fake_issuer.local_addr().unwrap();
    let key = fs::read(cluster_dir.join("identity.key")).unwrap();
    let issuer_public: [u8; 32] = key[32..].try_into().unwrap();
    let fingerprint = encoding::fingerprint(&issuer_public);
    let invitation = JoinLine {
        issuer_addr,
        issuer_fingerprint: fingerprint.clone(),
        issuer_static_pubkey: issuer_public,
        token: Zeroizing::new(encoding::encode_base64(&[7; 32])),
    }
    .encode()
    .unwrap();
    let (connected_tx, connected_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = loop {
            match fake_issuer.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "join did not connect to fake issuer"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("fake issuer accept failed: {error}"),
            }
        };
        connected_tx.send(()).unwrap();
        let _ = release_rx.recv_timeout(Duration::from_secs(5));
        drop(stream);
    });
    let child = scratch
        .command(&[
            "cluster",
            "join",
            &fingerprint,
            &invitation,
            "--bind",
            "127.0.0.1:0",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start isolated join command");
    let mut child = ChildGuard(Some(child));
    connected_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("join never connected to fake issuer");
    let child_pid = child.0.as_ref().unwrap().id() as libc::pid_t;
    assert_eq!(unsafe { libc::kill(child_pid, signal) }, 0);
    wait_until_child_exits(child.0.as_mut().unwrap(), Duration::from_secs(3));
    let output = child.0.take().unwrap().wait_with_output().unwrap();
    release_tx.send(()).unwrap();
    server.join().unwrap();
    assert_eq!(
        listener_config::read_at(&cluster_dir).unwrap(),
        saved_config,
        "signal did not restore the saved listener config"
    );
    assert_eq!(output.status.code(), Some(expected_exit));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Join cancelled."), "{stderr}");
    assert!(stderr.contains("Next:"), "{stderr}");
    assert_eq!(
        remuda_native::cluster::listener_control::status(&remuda_native::daemon::socket_path_in(
            &scratch.runtime,
            &scratch.name
        )),
        ListenerStatus::Off
    );
}

#[test]
fn d4_failed_join_keeps_a_preexisting_listener_on() {
    use remuda_core::protocol::ListenerStatus;
    use remuda_native::cluster::{encoding, join_line::JoinLine};
    use zeroize::Zeroizing;

    let scratch = Scratch::new();
    let _daemon = initialized_node(&scratch);
    let bound = explicit_listener_address(&scratch);
    let issuer_key = [7; 32];
    let fingerprint = encoding::fingerprint(&issuer_key);
    let invitation = JoinLine {
        issuer_addr: "127.0.0.1:9".parse().unwrap(),
        issuer_fingerprint: fingerprint.clone(),
        issuer_static_pubkey: issuer_key,
        token: Zeroizing::new(encoding::encode_base64(&[8; 32])),
    }
    .encode()
    .unwrap();
    let joined = scratch.run(&[
        "cluster",
        "join",
        "SHA256:wrong",
        &invitation,
        "--bind",
        "127.0.0.1:0",
    ]);
    assert!(
        !joined.status.success(),
        "wrong fingerprint unexpectedly joined"
    );
    assert_eq!(
        remuda_native::cluster::listener_control::status(&remuda_native::daemon::socket_path_in(
            &scratch.runtime,
            &scratch.name
        )),
        ListenerStatus::On {
            addr: bound,
            auto: false,
            advertise_addr: Some(bound),
            listen_addrs: vec![bound],
        },
        "failed join stopped a listener that was already on"
    );
}
