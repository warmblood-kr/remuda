#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct ModHome(PathBuf);

impl ModHome {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("remuda-mod-error-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mod_dir = dir.join("data/remuda/mods/sample");
        fs::create_dir_all(mod_dir.join("packages/sample")).unwrap();
        fs::write(
            mod_dir.join("extension.toml"),
            "name = \"sample\"\nentry = \"packages/sample/init.lua\"\napi = \"remuda-lua-v1\"\ncommand = \"sample\"\n",
        )
        .unwrap();
        fs::write(
            mod_dir.join("packages/sample/init.lua"),
            "remuda.extension_command('sample', function() error('handler exploded') end)",
        )
        .unwrap();
        Self(dir)
    }

    fn remuda(&self, args: &[&str]) -> Output {
        self.remuda_with_traceback(args, false)
    }

    fn remuda_with_traceback(&self, args: &[&str], traceback: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_remuda"));
        command
            .args(["-s", "mod-error-test"])
            .args(args)
            .env("REMUDA_RUNTIME_DIR", &self.0)
            .env("XDG_DATA_HOME", self.0.join("data"))
            .env("HOME", &self.0)
            .env("REMUDA_NO_UPDATE_CHECK", "1");
        if traceback {
            command.env("REMUDA_TRACEBACK", "1");
        } else {
            command.env_remove("REMUDA_TRACEBACK");
        }
        command.output().expect("run remuda")
    }

    fn log_path(&self) -> PathBuf {
        let socket = self.0.join("remuda/mod-error-test.sock");
        socket.with_extension("log")
    }
}

impl Drop for ModHome {
    fn drop(&mut self) {
        let _ = self.remuda(&["stop", "-f", "--yes"]);
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn mod_handler_error_is_one_line_plus_traceback_next_step() {
    let home = ModHome::new();
    let output = home.remuda(&["sample", "doctor"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines: Vec<&str> = stderr
        .lines()
        .filter(|line| {
            !line.starts_with("remuda: started a daemon")
                && !line.starts_with("remuda: started mod ")
        })
        .collect();

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(lines.len(), 2, "one error line plus Next: {output:?}");
    assert!(lines[0].contains("handler exploded"), "{output:?}");
    assert!(
        lines[1] == "Next: rerun with REMUDA_TRACEBACK=1 to see the full Lua traceback.",
        "traceback recovery step missing: {output:?}"
    );
    assert!(!stderr.contains("stack traceback"), "{output:?}");

    let detailed = home.remuda_with_traceback(&["sample", "doctor"], true);
    let detailed_stderr = String::from_utf8_lossy(&detailed.stderr);
    assert_eq!(detailed.status.code(), Some(1), "{detailed:?}");
    assert!(detailed_stderr.contains("stack traceback:"), "{detailed:?}");
    assert!(
        detailed_stderr.contains("Next: rerun with REMUDA_TRACEBACK=1"),
        "{detailed:?}"
    );

    let daemon_log = fs::read_to_string(home.log_path()).expect("read private daemon log");
    assert!(daemon_log.contains("stack traceback:"), "{daemon_log}");
}
