//! A caller that captures `remuda ...` output through pipes must get EOF when
//! the CLI exits, even if the CLI auto-started a daemon. On Windows, std
//! spawns with bInheritHandles=TRUE, so the daemon used to inherit the CLI's
//! own stdout/stderr pipe ends and hold them open for its lifetime. Issue #111.

use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const PATIENCE: Duration = Duration::from_secs(30);

#[test]
fn captured_output_ends_when_the_cli_exits_not_the_daemon() {
    let dir = std::env::temp_dir().join(format!("rdo-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let remuda = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_remuda"));
        command
            .args(["-s", "s"])
            .args(args)
            .env("REMUDA_RUNTIME_DIR", &dir)
            .env("HOME", &dir)
            .env("LOCALAPPDATA", &dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    };

    let child = remuda(&["-e", "return 1"]).spawn().expect("spawn remuda");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let captured = rx.recv_timeout(PATIENCE);
    // Stopping the daemon also frees a hung reader thread on failure.
    let _ = remuda(&["stop", "-f"]).output();

    let output = captured
        .expect("output capture outlived the CLI: the daemon holds its pipes")
        .expect("wait for remuda");
    assert!(output.status.success(), "{output:?}");
}
