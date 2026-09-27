//! `remuda exec NAME` must activate a lifecycle mod the same way
//! `remuda.exec(NAME)` does in the image — not evaluate its entry as a plain
//! chunk and discard the declaration. #98 item 3.

use std::fs;
use std::process::{Command, Output};

#[test]
fn cli_exec_activates_a_lifecycle_mod() {
    let dir = std::env::temp_dir().join(format!("rcx-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let mod_dir = dir.join("data/remuda/mods/sample");
    fs::create_dir_all(mod_dir.join("packages/sample")).unwrap();
    fs::write(
        mod_dir.join("extension.toml"),
        "name = \"sample\"\nentry = \"packages/sample/init.lua\"\napi = \"remuda-lua-v1\"\nlifecycle = \"remuda-module-v1\"\n",
    )
    .unwrap();
    fs::write(
        mod_dir.join("packages/sample/init.lua"),
        r#"return {
          api = "remuda-module-v1", state_version = 1,
          initialize = function() return { starts = 0 } end,
          start = function(state) state.starts = state.starts + 1 end,
          tools = {{ name = "sample_start_count",
            about = "Read how often this module started.",
            run = function(state) return tostring(state.starts) end }},
        }"#,
    )
    .unwrap();
    let remuda = |args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", "s"])
            .args(args)
            .env("REMUDA_RUNTIME_DIR", &dir)
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("HOME", &dir)
            .output()
            .expect("run remuda")
    };

    let exec = remuda(&["exec", "sample"]);
    let starts = remuda(&["-e", "return remuda.tools.sample_start_count()"]);
    remuda(&["stop", "-f"]);
    let _ = fs::remove_dir_all(&dir);

    assert!(exec.status.success(), "exec failed: {exec:?}");
    assert_eq!(
        String::from_utf8_lossy(&starts.stdout).trim(),
        "1",
        "the mod was not activated (tool missing or start not run): {starts:?}"
    );
}
