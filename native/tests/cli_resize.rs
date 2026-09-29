#![cfg(unix)]

use std::path::Path;
use std::process::Command;

#[path = "daemon_support/spawn.rs"]
mod spawn;
use spawn::Daemon;

fn run(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(["-s", "s"])
        .args(args)
        .env("REMUDA_RUNTIME_DIR", dir)
        .env("HOME", dir.join("home"))
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        .output()
        .expect("run remuda")
}

#[test]
fn resize_command_updates_session_and_rejects_bad_requests() {
    let dir = std::env::temp_dir().join(format!("remuda-cli-resize-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create private runtime directory");
    let daemon = Daemon::spawn(&dir);
    let created = run(&dir, &["-e", "remuda.session.new('resize-cli', {'sh'})"]);
    assert!(created.status.success(), "create session: {created:?}");

    let resized = run(&dir, &["resize", "resize-cli", "97", "33"]);
    assert!(resized.status.success(), "resize: {resized:?}");
    let dimensions = run(&dir, &["-e", "for _,s in ipairs(remuda.ls()) do if s.name == 'resize-cli' then assert(s.cols == 97 and s.rows == 33) end end"]);
    assert!(dimensions.status.success(), "dimensions: {dimensions:?}");

    for args in [
        &["resize", "resize-cli", "19", "24"][..],
        &["resize", "resize-cli", "80", "501"][..],
        &["resize", "absent", "80", "24"][..],
    ] {
        let output = run(&dir, args);
        assert!(!output.status.success(), "{args:?} unexpectedly succeeded");
    }
    drop(daemon);
    std::fs::remove_dir_all(&dir).expect("remove private runtime directory");
}
