//! Closing a console window ends every process attached to that console. The
//! daemon a command starts must not be one of them, or the window's X takes
//! the daemon and every session with it.
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wdc{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).unwrap();
    dir
}

fn remuda(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(["-s", "s"])
        .args(args)
        .env("REMUDA_RUNTIME_DIR", dir)
        .env("HOME", dir.join("home"))
        .env("LOCALAPPDATA", dir.join("home"))
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("run remuda")
}

/// Every process attached to this test's own console.
fn console_pids() -> Vec<u32> {
    let mut pids = vec![0u32; 256];
    // SAFETY: the pointer and count describe `pids`.
    let count = unsafe {
        windows_sys::Win32::System::Console::GetConsoleProcessList(
            pids.as_mut_ptr(),
            pids.len() as u32,
        )
    } as usize;
    // No console means nothing below can tell the two outcomes apart; say so
    // rather than pass.
    assert!(
        count > 0,
        "this test needs a console of its own: {}",
        std::io::Error::last_os_error()
    );
    assert!(count <= pids.len(), "{count} processes share this console");
    pids.truncate(count);
    pids
}

#[test]
fn an_auto_started_daemon_does_not_share_the_launching_console() {
    let dir = scratch("console");
    let before = console_pids();

    let started = remuda(&dir, &["-e", "return 1"]);
    let said = String::from_utf8_lossy(&started.stderr).into_owned();
    let after = console_pids();
    // Stop it before judging, so a failure leaves no daemon behind.
    let _ = remuda(&dir, &["stop", "-f", "--yes"]);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(started.status.success(), "{said}");
    assert!(said.contains("started a daemon"), "{said}");
    // The command itself has exited, so anything new is the daemon it left.
    let left: Vec<u32> = after
        .into_iter()
        .filter(|pid| !before.contains(pid))
        .collect();
    assert!(
        left.is_empty(),
        "the daemon (pid {left:?}) is attached to the launching console; closing that window ends it and every session"
    );
}
