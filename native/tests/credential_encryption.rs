//! Credentials are encrypted at rest under a per-node key (#746): a copy of
//! the state directory without that key holds no secret, and a daemon start
//! re-encrypts a plaintext identity left by an older remuda.
#![cfg(unix)]

use remuda_native::cluster::encoding;
use remuda_native::cluster::identity::load_keypair_at;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Node {
    root: PathBuf,
    runtime: PathBuf,
    state: PathBuf,
    name: String,
}

struct DaemonGuard(Child);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Node {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "ce{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        let runtime = root.join("r");
        let state = root.join("state");
        fs::create_dir_all(&runtime).unwrap();
        fs::create_dir_all(&state).unwrap();
        Self {
            root,
            runtime,
            state,
            name: "s".into(),
        }
    }

    fn cluster_dir(&self) -> PathBuf {
        self.state.join("remuda/cluster")
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_remuda"));
        command
            .args(["-s", &self.name])
            .env("REMUDA_RUNTIME_DIR", &self.runtime)
            .env("HOME", &self.root)
            .env("XDG_STATE_HOME", &self.state);
        command
    }

    fn daemon(&self, log: &Path) -> DaemonGuard {
        let log = fs::File::create(log).unwrap();
        let mut daemon = DaemonGuard(
            self.command()
                .arg("daemon")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(log))
                .spawn()
                .unwrap(),
        );
        let socket = remuda_native::daemon::socket_path_in(&self.runtime, &self.name);
        let deadline = Instant::now() + Duration::from_secs(10);
        while remuda_native::client::request(&socket, &remuda_core::protocol::Request::List)
            .is_err()
        {
            if let Some(status) = daemon.0.try_wait().unwrap() {
                panic!("isolated daemon exited before binding: {status}");
            }
            assert!(Instant::now() < deadline, "isolated daemon did not bind");
            std::thread::sleep(Duration::from_millis(20));
        }
        daemon
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    /// Start a daemon, run `cluster init`, stop it, and return the keypair.
    fn initialized(&self) -> Vec<u8> {
        let daemon = self.daemon(&self.root.join("init.log"));
        let init = self.run(&["cluster", "init", "--no-listen"]);
        assert!(init.status.success(), "cluster init failed: {init:?}");
        drop(daemon);
        load_keypair_at(&self.cluster_dir()).unwrap().to_vec()
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Copy a directory tree, keeping modes, skipping top-level names in `skip`.
fn copy_tree(from: &Path, to: &Path, skip: &[&str]) {
    fs::create_dir_all(to).unwrap();
    fs::set_permissions(to, fs::metadata(from).unwrap().permissions()).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if skip.iter().any(|skipped| name == *skipped) {
            continue;
        }
        let kind = entry.file_type().unwrap();
        let target = to.join(&name);
        if kind.is_dir() {
            copy_tree(&entry.path(), &target, &[]);
        } else if kind.is_file() {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(files_under(&path));
        } else {
            files.push(path);
        }
    }
    files
}

/// The private key in any form a file or a transcript could carry it.
fn assert_no_secret(bytes: &[u8], private: &[u8], what: &str) {
    assert!(
        !bytes.windows(private.len()).any(|window| window == private),
        "{what} holds the raw private key"
    );
    let text = String::from_utf8_lossy(bytes);
    let hex: String = private.iter().map(|byte| format!("{byte:02x}")).collect();
    assert!(!text.contains(&hex), "{what} holds the private key in hex");
    assert!(
        !text.contains(&encoding::encode_base64(private)),
        "{what} holds the private key in base64"
    );
}

#[test]
fn a_copied_state_directory_without_the_node_key_yields_no_secrets() {
    let node = Node::new();
    let keypair = node.initialized();
    let private = &keypair[..32];
    assert!(node.state.join("remuda/keys/node.key").is_file());

    // Copy everything Remuda keeps in its state directory except the node key.
    let copy = Node::new();
    copy_tree(
        &node.state.join("remuda"),
        &copy.state.join("remuda"),
        &["keys"],
    );
    let copied = files_under(&copy.state.join("remuda"));
    assert!(copied
        .iter()
        .any(|path| path.ends_with("cluster/identity.key")));
    for path in &copied {
        assert_no_secret(
            &fs::read(path).unwrap(),
            private,
            &path.display().to_string(),
        );
    }

    let error = load_keypair_at(&copy.cluster_dir()).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("node key"), "{error}");

    // The copied node refuses to read or replace the identity it cannot open.
    let sealed = fs::read(copy.cluster_dir().join("identity.key")).unwrap();
    let log = copy.root.join("copy.log");
    let daemon = copy.daemon(&log);
    let status = copy.run(&["cluster"]);
    let init = copy.run(&["cluster", "init", "--no-listen"]);
    drop(daemon);
    assert!(
        !init.status.success(),
        "init over an unreadable identity: {init:?}"
    );
    let refusal = String::from_utf8_lossy(&init.stderr);
    assert!(refusal.contains("node key"), "{refusal}");
    assert_eq!(
        fs::read(copy.cluster_dir().join("identity.key")).unwrap(),
        sealed,
        "the copied identity was replaced"
    );
    assert!(!copy.state.join("remuda/keys/node.key").exists());
    let mut transcript = status.stdout;
    transcript.extend(status.stderr);
    transcript.extend(init.stdout);
    transcript.extend(init.stderr);
    transcript.extend(fs::read(&log).unwrap());
    assert_no_secret(&transcript, private, "the copied node's output");

    // With the node key alongside, the same copy is readable: the key is the
    // only thing that was missing.
    let full = Node::new();
    copy_tree(&node.state.join("remuda"), &full.state.join("remuda"), &[]);
    assert_eq!(
        &load_keypair_at(&full.cluster_dir()).unwrap()[..],
        &keypair[..]
    );
}

#[test]
fn daemon_start_encrypts_a_plaintext_identity_once_and_reports_it() {
    let node = Node::new();
    let keypair = node.initialized();
    let identity = node.cluster_dir().join("identity.key");

    // Recreate a node written before #746: plaintext identity, no node key.
    fs::remove_file(&identity).unwrap();
    fs::write(&identity, &keypair).unwrap();
    fs::set_permissions(&identity, fs::Permissions::from_mode(0o600)).unwrap();
    fs::remove_dir_all(node.state.join("remuda/keys")).unwrap();

    let log = node.root.join("migrate.log");
    let daemon = node.daemon(&log);
    let after = node.run(&["cluster"]);
    drop(daemon);

    let on_disk = fs::read(&identity).unwrap();
    assert!(on_disk.starts_with(b"RMDSEAL1"), "identity was not sealed");
    assert_no_secret(&on_disk, &keypair[..32], "the migrated identity.key");
    assert_eq!(
        &load_keypair_at(&node.cluster_dir()).unwrap()[..],
        &keypair[..]
    );
    assert_eq!(
        fs::metadata(&identity).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(node.state.join("remuda/keys"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    // The fingerprint (and so cluster membership) did not change.
    let fingerprint = encoding::fingerprint(&keypair[32..]);
    assert!(
        String::from_utf8_lossy(&after.stdout).contains(&fingerprint),
        "{after:?}"
    );

    let report = fs::read_to_string(&log).unwrap();
    assert!(report.contains("encrypted"), "{report}");
    assert!(report.contains(&identity.display().to_string()), "{report}");
    assert!(report.contains("plaintext copy was removed"), "{report}");
    assert_no_secret(report.as_bytes(), &keypair[..32], "the daemon log");

    // A second start finds nothing left to migrate.
    let again = node.root.join("again.log");
    drop(node.daemon(&again));
    let second = fs::read_to_string(&again).unwrap();
    assert!(!second.contains("encrypted"), "{second}");
    assert_eq!(fs::read(&identity).unwrap(), on_disk);
}
