//! Verbs that must answer without touching any daemon, and a socket path too
//! long to bind that must say so instead of blaming a second daemon. #115.

#![cfg(unix)]

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn remuda(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(["-s", "s"])
        .args(args)
        .env("REMUDA_RUNTIME_DIR", dir)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("HOME", dir)
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        .output()
        .expect("run remuda")
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rnd{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A stand-in at the private server's socket that only records connections.
#[test]
fn help_and_version_never_connect_to_a_daemon() {
    for args in [
        &["--help"][..],
        &["-h"],
        &["help"],
        &["--version"],
        &["-V"],
        &["version"],
    ] {
        let dir = scratch("help");
        let socket = remuda_native::daemon::socket_path_in(&dir, "s");
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&socket).expect("bind stand-in");
        let (tx, rx) = std::sync::mpsc::channel();
        // Accept and hang up at once, so a connecting CLI fails fast, not hangs.
        std::thread::spawn(move || {
            while listener.accept().is_ok() {
                let _ = tx.send(());
            }
        });

        let out = remuda(&dir, args);
        let touched = rx.try_recv().is_ok();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(out.status.success(), "{args:?} failed: {out:?}");
        assert!(!touched, "{args:?} connected to the daemon socket");
    }
}

#[test]
fn a_socket_path_over_sun_path_names_the_length_not_a_second_daemon() {
    let dir = scratch("long").join("x".repeat(120));
    std::fs::create_dir_all(&dir).unwrap();

    let out = remuda(&dir, &["ls"]);
    let said = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir_all(dir.parent().unwrap());

    assert!(
        !out.status.success(),
        "ls with an unbindable path succeeded"
    );
    assert!(
        !said.contains("second daemon"),
        "blamed a second daemon: {said}"
    );
    assert!(said.contains("REMUDA_RUNTIME_DIR"), "no cure named: {said}");
}
