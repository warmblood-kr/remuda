#![cfg(unix)]

use std::fs;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct DaemonGuard(Child);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn init_and_status_never_print_private_key_material() {
    let root = std::env::temp_dir().join(format!(
        "ci{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&root).unwrap();
    let runtime = root.join("r");
    let state = root.join("state");
    fs::create_dir_all(&runtime).unwrap();
    fs::create_dir_all(&state).unwrap();
    let name = "s";
    let mut daemon = DaemonGuard(
        Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", name, "daemon"])
            .env("REMUDA_RUNTIME_DIR", &runtime)
            .env("HOME", &root)
            .env("XDG_STATE_HOME", &state)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let socket = remuda_native::daemon::socket_path_in(&runtime, name);
    let deadline = Instant::now() + Duration::from_secs(10);
    while remuda_native::client::request(&socket, &remuda_core::protocol::Request::List).is_err() {
        if let Some(status) = daemon.0.try_wait().unwrap() {
            panic!("isolated daemon exited before binding: {status}");
        }
        assert!(Instant::now() < deadline, "isolated daemon did not bind");
        std::thread::sleep(Duration::from_millis(20));
    }
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", name])
            .args(args)
            .env("REMUDA_RUNTIME_DIR", &runtime)
            .env("HOME", &root)
            .env("XDG_STATE_HOME", &state)
            .output()
            .unwrap()
    };
    let initialized = run(&["cluster", "init", "--no-listen"]);
    assert!(
        initialized.status.success(),
        "cluster init failed: {initialized:?}"
    );
    let key = fs::read(state.join("remuda/cluster/identity.key")).unwrap();
    let repeated = run(&["cluster", "init", "--no-listen"]);
    assert!(repeated.status.success());
    assert!(String::from_utf8_lossy(&repeated.stdout).contains("Already initialized"));
    let status = run(&["cluster"]);
    assert!(status.status.success());
    let default_status = String::from_utf8_lossy(&status.stdout);
    assert!(default_status.contains("Remote control: enabled"));
    assert!(default_status.contains("compromised admitted node"));
    assert!(default_status.contains("close every session"));
    let disabled = run(&["cluster", "control", "off"]);
    assert!(disabled.status.success());
    assert!(String::from_utf8_lossy(&disabled.stdout).contains("Remote control disabled"));
    let settings = fs::read_to_string(state.join("remuda/cluster/settings.json")).unwrap();
    assert!(settings.contains("\"allow_remote_control\":false"));
    let disabled_status = run(&["cluster"]);
    assert!(disabled_status.status.success());
    assert!(String::from_utf8_lossy(&disabled_status.stdout).contains("Remote control: disabled"));
    let enabled = run(&["cluster", "control", "on"]);
    assert!(enabled.status.success());
    let enabled_status = run(&["cluster"]);
    assert!(String::from_utf8_lossy(&enabled_status.stdout).contains("Remote control: enabled"));

    let mut transcript = initialized.stdout;
    transcript.extend(initialized.stderr);
    transcript.extend(repeated.stdout);
    transcript.extend(repeated.stderr);
    transcript.extend(status.stdout);
    transcript.extend(status.stderr);
    transcript.extend(disabled.stdout);
    transcript.extend(disabled.stderr);
    transcript.extend(disabled_status.stdout);
    transcript.extend(disabled_status.stderr);
    transcript.extend(enabled.stdout);
    transcript.extend(enabled.stderr);
    transcript.extend(enabled_status.stdout);
    transcript.extend(enabled_status.stderr);
    assert!(!transcript.windows(32).any(|window| window == &key[..32]));
    let hex = key[..32]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert!(!String::from_utf8_lossy(&transcript).contains(&hex));
    assert!(!String::from_utf8_lossy(&transcript)
        .contains(&remuda_native::cluster::encoding::encode_base64(&key[..32])));
    drop(daemon);
    let _ = fs::remove_dir_all(root);
}
