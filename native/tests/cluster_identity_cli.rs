#![cfg(unix)]

use std::fs;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

#[test]
fn init_and_status_never_print_private_key_material() {
    let root = std::env::temp_dir().join(format!(
        "remuda-cluster-cli-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&root).unwrap();
    let state = root.join("state");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(args)
            .env("HOME", &root)
            .env("XDG_STATE_HOME", &state)
            .output()
            .unwrap()
    };
    let initialized = run(&["cluster", "init"]);
    assert!(initialized.status.success());
    let key = fs::read(state.join("remuda/cluster/identity.key")).unwrap();
    let repeated = run(&["cluster", "init"]);
    assert!(repeated.status.success());
    assert!(String::from_utf8_lossy(&repeated.stdout).contains("Already initialized"));
    let status = run(&["cluster"]);
    assert!(status.status.success());

    let mut transcript = initialized.stdout;
    transcript.extend(initialized.stderr);
    transcript.extend(repeated.stdout);
    transcript.extend(repeated.stderr);
    transcript.extend(status.stdout);
    transcript.extend(status.stderr);
    assert!(!transcript.windows(32).any(|window| window == &key[..32]));
    let hex = key[..32]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert!(!String::from_utf8_lossy(&transcript).contains(&hex));
    assert!(!String::from_utf8_lossy(&transcript)
        .contains(&remuda_native::cluster::encoding::encode_base64(&key[..32])));
    let _ = fs::remove_dir_all(root);
}
