#![cfg(unix)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn scratch(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("remuda-ext-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn install_test_extension(data_home: &Path) {
    let package = data_home.join("remuda/mods/probe");
    std::fs::create_dir_all(package.join("packages/probe")).unwrap();
    std::fs::write(
        package.join("extension.toml"),
        "name = \"probe\"\nentry = \"packages/probe/init.lua\"\napi = \"remuda-lua-v1\"\ncommand = \"probe\"\n",
    )
    .unwrap();
    std::fs::write(
        package.join("packages/probe/init.lua"),
        "remuda.extension_command('probe', function() return 'ok' end)",
    )
    .unwrap();
}

#[test]
fn extension_opt_out_reports_missing_daemon_without_starting_one() {
    let dir = scratch("no-start");
    let data_home = dir.join("data");
    install_test_extension(&data_home);
    let socket = remuda_native::daemon::socket_path_in(&dir, "s");
    let mut child = Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(["-s", "s", "--stdin", "probe", "statusline"])
        .env("REMUDA_RUNTIME_DIR", &dir)
        .env("REMUDA_NO_AUTOSTART", "1")
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        .env("XDG_DATA_HOME", data_home)
        .env("HOME", &dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run extension command");
    child.stdin.take().unwrap().write_all(b"{}").unwrap();
    let output = child
        .wait_with_output()
        .expect("wait for extension command");
    assert!(
        !output.status.success(),
        "missing daemon was accepted: {output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no daemon running for \"s\""),
        "unexpected extension failure: {output:?}"
    );
    assert!(!socket.exists(), "extension command autostarted a daemon");
    let _ = std::fs::remove_dir_all(dir);
}

/// A peer that answers the version probe and the loader promptly but withholds
/// the dispatch reply must still be cut off by `REMUDA_CLIENT_TIMEOUT_MS`.
#[test]
fn extension_dispatch_reply_honors_client_timeout() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    use std::time::{Duration, Instant};

    let dir = scratch("timeout");
    let data_home = dir.join("data");
    install_test_extension(&data_home);
    let socket = remuda_native::daemon::socket_path_in(&dir, "s");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut line = String::new();
                if BufReader::new(&stream).read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                if line.contains("_dispatch_extension_command") {
                    // Withhold the reply, holding the connection open.
                    std::thread::sleep(Duration::from_secs(60));
                } else if line.contains("Eval") {
                    let _ = (&stream).write_all(b"{\"Value\":\"false\"}\n");
                } else {
                    let _ = (&stream).write_all(b"{\"Value\":\"peer\"}\n");
                }
            });
        }
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(["-s", "s", "--stdin", "probe", "statusline"])
        .env("REMUDA_RUNTIME_DIR", &dir)
        .env("REMUDA_NO_AUTOSTART", "1")
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        .env("REMUDA_CLIENT_TIMEOUT_MS", "1500")
        .env("XDG_DATA_HOME", data_home)
        .env("HOME", &dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run extension command");
    child.stdin.take().unwrap().write_all(b"{}").unwrap();
    let started = Instant::now();
    // Outer watchdog: the unfixed client waits 305 s, so kill it and fail.
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("client ignored REMUDA_CLIENT_TIMEOUT_MS on the dispatch reply");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        !output.status.success(),
        "stalled reply succeeded: {output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("timed out after"),
        "no timeout diagnostic: {output:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}
