#![cfg(unix)]

use std::{fs, os::unix::fs::symlink, process::Command};

#[test]
fn discovery_distinguishes_uninitialized_from_invalid_state_without_enrolling() {
    let dir = std::env::temp_dir().join(format!("remuda-gui-nodes-{}", std::process::id()));
    fs::create_dir(&dir).expect("private test directory");
    let absent = dir.join("absent");
    let output = Command::new(env!("CARGO_BIN_EXE_remuda-gui"))
        .arg("--internal-nodes-worker")
        .env("XDG_STATE_HOME", &absent)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"[]");
    assert!(!absent.exists(), "discovery must not initialize a cluster");

    let invalid = dir.join("invalid");
    fs::create_dir_all(invalid.join("remuda")).unwrap();
    symlink(&dir, invalid.join("remuda/cluster")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_remuda-gui"))
        .arg("--internal-nodes-worker")
        .env("XDG_STATE_HOME", &invalid)
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "invalid state must not look local-only"
    );
    fs::remove_dir_all(dir).unwrap();
}
