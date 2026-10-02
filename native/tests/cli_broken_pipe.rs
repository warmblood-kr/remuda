#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Command, Output};

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("remuda-broken-pipe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("make private test directory");
        Self(dir)
    }

    fn remuda(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(args)
            .env("REMUDA_RUNTIME_DIR", &self.0)
            .env("XDG_DATA_HOME", self.0.join("data"))
            .env("HOME", &self.0)
            .env("REMUDA_NO_UPDATE_CHECK", "1")
            .output()
            .expect("run remuda")
    }

    fn pipeline(&self, args: &[&str], repetitions: usize) -> Output {
        let mut command = vec![env!("CARGO_BIN_EXE_remuda")];
        command.extend_from_slice(args);
        let command = command
            .iter()
            .map(|arg| format!("'{}'", arg.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" ");
        let producer = if repetitions == 1 {
            command
        } else {
            format!("for ((i=0; i<{repetitions}; i++)); do {command}; done")
        };
        let script =
            format!("set -o pipefail; {producer} | {{ IFS= read -r line && [[ -n \"$line\" ]]; }}");
        Command::new("bash")
            .args(["-o", "pipefail", "-c", &script])
            .env("REMUDA_RUNTIME_DIR", &self.0)
            .env("XDG_DATA_HOME", self.0.join("data"))
            .env("HOME", &self.0)
            .env("REMUDA_NO_UPDATE_CHECK", "1")
            .output()
            .expect("run one-line reader pipeline")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", "s", "stop", "-f"])
            .env("REMUDA_RUNTIME_DIR", &self.0)
            .env("XDG_DATA_HOME", self.0.join("data"))
            .env("HOME", &self.0)
            .env("REMUDA_NO_UPDATE_CHECK", "1")
            .output();
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_reader_pipeline_succeeded(command: &str, output: Output) {
    let stderr = stderr(&output);
    assert!(
        output.status.success(),
        "the one-line reader accepted input, but remuda {command} failed: {stderr}"
    );
    assert!(
        !stderr.contains("failed printing to stdout"),
        "remuda {command} leaked a broken pipe panic: {stderr}"
    );
}

#[test]
fn listing_commands_allow_the_reader_to_stop_after_one_line() {
    let scratch = Scratch::new();
    let mods = scratch.0.join("data/remuda/mods");
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

    let mut lua = String::from("for i=1,48 do ");
    lua.push_str(
        "remuda.session.new(string.rep('x', 1800)..string.format('%03d', i), {'/bin/sleep','60'}) end",
    );
    let setup = scratch.remuda(&["-s", "s", "-e", &lua]);
    assert!(
        setup.status.success(),
        "start listing fixture: {}",
        stderr(&setup)
    );

    for (command, args, repetitions) in [
        ("ls", vec!["-s", "s", "ls"], 1),
        ("mod list", vec!["mod", "list"], 1),
        ("doc", vec!["-s", "s", "doc"], 4),
    ] {
        let full_output = scratch.remuda(&args);
        assert!(
            full_output.status.success(),
            "capture remuda {command}: {}",
            stderr(&full_output)
        );
        assert!(
            full_output.stdout.len() * repetitions > 64 * 1024,
            "remuda {command} produced {} bytes, not more than a pipe buffer",
            full_output.stdout.len() * repetitions
        );
        assert_reader_pipeline_succeeded(command, scratch.pipeline(&args, repetitions));
    }

    let _ = scratch.remuda(&["-s", "s", "stop", "-f"]);
}
