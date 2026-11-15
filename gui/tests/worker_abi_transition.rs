#![cfg(unix)]

use std::{fs, process::Command};

// Run with REMUDA_GUI_OLD_BINARY pointing at a binary built from 76cd7fa.
#[test]
#[ignore = "requires a separately built pre-fence GUI binary"]
fn actual_old_and_new_node_workers_use_distinguishable_formats() {
    let old = std::env::var_os("REMUDA_GUI_OLD_BINARY").expect("old binary path");
    let dir = std::env::temp_dir().join(format!("remuda-abi-{}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    let run = |binary: &std::ffi::OsStr, tagged: bool| {
        let mut cmd = Command::new(binary);
        cmd.arg("--internal-nodes-worker")
            .env("XDG_STATE_HOME", &dir);
        if tagged {
            cmd.arg("--remuda-gui-worker-abi-1");
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success());
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
    };
    let new = std::ffi::OsStr::new(env!("CARGO_BIN_EXE_remuda-gui"));
    assert_eq!(run(&old, false), serde_json::json!([]));
    assert_eq!(run(&old, true), serde_json::json!([]));
    assert_eq!(run(new, false), serde_json::json!([]));
    assert_eq!(
        run(new, true),
        serde_json::json!({"abi":"--remuda-gui-worker-abi-1","payload":[]})
    );
    assert!(!dir.join("remuda").exists());
    fs::remove_dir_all(dir).unwrap();
}
