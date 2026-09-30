#![cfg(unix)]
#![allow(clippy::disallowed_types)]

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
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
        let base = if Path::new("/private/tmp").is_dir() {
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

fn start_listener(scratch: &Scratch) -> (TrackedChild, SocketAddr) {
    let reservation = TcpListener::bind("127.0.0.1:0").expect("reserve loopback port");
    let address = reservation.local_addr().expect("reserved address");
    drop(reservation);
    let address_text = address.to_string();
    let child = scratch
        .command(&["cluster", "listen", "--bind", &address_text])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start isolated listener");
    let mut listener = TrackedChild(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok() {
            return (listener, address);
        }
        if let Some(status) = listener.0.try_wait().expect("check isolated listener") {
            panic!("isolated listener exited before ready: {status}");
        }
        assert!(Instant::now() < deadline, "isolated listener did not bind");
        std::thread::sleep(Duration::from_millis(20));
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
    text = replace_addresses(&text);
    text = replace_fingerprints_and_base64(&text);
    assert!(
        !has_token_shaped_text(&text),
        "token-shaped data survived normalization: {text}"
    );
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
    let mut rest = input;
    while let Some(index) = rest.find("127.0.0.1:") {
        out.push_str(&rest[..index]);
        let tail = &rest[index + "127.0.0.1:".len()..];
        let count = tail.chars().take_while(char::is_ascii_digit).count();
        if count > 0 {
            out.push_str("<ADDR>");
            rest = &tail[count..];
        } else {
            out.push_str("127.0.0.1:");
            rest = tail;
        }
    }
    out.push_str(rest);
    out
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
        &format!("remuda-join-v1 127.0.0.1:7441 SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA= {token}"),
        root,
    );
    assert!(!normalized.contains(token));
    assert!(!has_token_shaped_text(&normalized));
}

#[test]
fn cluster_cli_output_matches_goldens() {
    let scratch = Scratch::new();
    golden_uninitialized_cases(&scratch);
    golden_initialized_cases(&scratch);
    golden_invite_join_cases(&scratch);
    golden_revoke_cancel_case(&scratch);
    golden_remaining_verb_errors(&scratch);
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
    golden("init", &scratch.run(&["cluster", "init"]), &scratch.root);
    golden(
        "init_existing",
        &scratch.run(&["cluster", "init"]),
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
    let invite = scratch.run(&["cluster", "invite", "--bind", "127.0.0.1:7441"]);
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
    peer.run(&["cluster", "init"]);
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
        &scratch.run(&["cluster", "listen"]),
        &scratch.root,
    );
    golden(
        "listen_invalid_address",
        &scratch.run(&["cluster", "listen", "--bind", "bad-address"]),
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
    let initialized = inviter.run(&["cluster", "init"]);
    assert!(
        initialized.status.success(),
        "inviter init failed: {initialized:?}"
    );
    let _daemon = start_daemon(&inviter);
    let (_listener, address) = start_listener(&inviter);
    let address_text = address.to_string();
    let invite = inviter.run(&["cluster", "invite", "--bind", &address_text]);
    assert!(invite.status.success(), "invite failed: {invite:?}");
    let (fingerprint, join_line) = invitation_command_args(&invite);

    let joiner = Scratch::new();
    let initialized = joiner.run(&["cluster", "init"]);
    assert!(
        initialized.status.success(),
        "joiner init failed: {initialized:?}"
    );
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
