#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Command, Output};

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("remuda-broken-pipe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("make private test directory");
    dir
}

fn run(dir: &PathBuf, command: &str) -> Output {
    let binary = env!("CARGO_BIN_EXE_remuda");
    let script = format!(
        "set -o pipefail; '{}' {} | head -n 1 | grep -q .",
        binary.replace('\'', "'\\''"),
        command,
    );
    Command::new("bash")
        .args(["-o", "pipefail", "-c", &script])
        .env("REMUDA_RUNTIME_DIR", dir)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("HOME", dir)
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        .output()
        .expect("run pipeline")
}

#[test]
fn mod_list_ending_after_the_first_line_is_a_successful_pipeline() {
    let dir = scratch();
    let mods = dir.join("data/remuda/mods");
    std::fs::create_dir_all(&mods).expect("make mods directory");
    for index in 0..1024 {
        let name = format!("mod-{index:04}");
        let mod_dir = mods.join(&name);
        std::fs::create_dir_all(&mod_dir).expect("make mod directory");
        std::fs::write(
            mod_dir.join("extension.toml"),
            format!(
                "name = \"{name}\"\nversion = \"1.0.0\"\napi = \"remuda-lua-v1\"\nentry = \"packages/{name}/init.lua\"\n"
            ),
        )
        .expect("write mod manifest");
    }

    let output = run(&dir, "mod list");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        output.status.success(),
        "the reader accepted the first line, but the pipeline failed: {stderr}"
    );
    assert!(
        !stderr.contains("failed printing to stdout"),
        "broken pipe panic leaked to stderr: {stderr}"
    );
}
