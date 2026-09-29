#![cfg(unix)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("remuda-ext-no-start-{}", std::process::id()));
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
    let dir = scratch();
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
