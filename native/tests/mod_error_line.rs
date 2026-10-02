#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct ModHome(PathBuf);

impl ModHome {
    fn new(label: &str, handler: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("me-{label}-{}", std::process::id()));
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
            format!("remuda.extension_command('sample', function() {handler} end)"),
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
            .args(["-s", "s"])
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
        self.0.join("remuda/s.sock").with_extension("log")
    }
}

impl Drop for ModHome {
    fn drop(&mut self) {
        let _ = self.remuda(&["stop", "-f", "--yes"]);
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn diagnostic_lines(output: &Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| {
            !line.starts_with("remuda: started a daemon")
                && !line.starts_with("remuda: started mod ")
        })
        .map(str::to_string)
        .collect()
}

#[test]
fn plain_mod_handler_error_is_one_line_plus_traceback_next_step() {
    let home = ModHome::new("plain", "error('handler exploded')");
    let output = home.remuda(&["sample", "doctor"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines = diagnostic_lines(&output);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(lines.len(), 2, "one error line plus Next: {output:?}");
    assert!(lines[0].contains("handler exploded"), "{output:?}");
    assert_eq!(
        lines[1],
        "Next: rerun with REMUDA_TRACEBACK=1 to see the full Lua traceback."
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

#[test]
fn deliberate_multiline_mod_error_keeps_its_next_without_traceback() {
    let home = ModHome::new("multiline", "error('a\\nNext: do x', 0)");
    let output = home.remuda(&["sample", "doctor"]);
    let lines = diagnostic_lines(&output);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(lines, ["remuda: runtime error: a", "Next: do x"]);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("stack traceback"));
}

#[test]
fn typed_mod_failure_keeps_its_message_and_exit_code() {
    let home = ModHome::new("typed", "remuda.fail('extension rejected', 23)");
    let output = home.remuda(&["sample", "doctor"]);

    assert_eq!(output.status.code(), Some(23), "{output:?}");
    assert_eq!(diagnostic_lines(&output), ["extension rejected"]);
}

#[test]
fn mod_error_strips_terminal_controls_with_and_without_traceback() {
    let home = ModHome::new("controls", "error('before' .. string.char(27) .. 'after')");
    for output in [
        home.remuda(&["sample", "doctor"]),
        home.remuda_with_traceback(&["sample", "doctor"], true),
    ] {
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains('\u{1b}'),
            "terminal control leaked: {output:?}"
        );
        assert!(stderr.contains("beforeafter"), "{output:?}");
    }
}

#[test]
fn daemon_mod_error_log_escapes_and_caps_the_error() {
    let home = ModHome::new(
        "long-log",
        "error('start' .. string.char(27) .. string.rep('x', 3500) .. '\\nNext: hidden' .. string.rep('y', 3000), 0)",
    );
    let output = home.remuda(&["sample", "doctor"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");

    let daemon_log = fs::read_to_string(home.log_path()).expect("read private daemon log");
    let line = daemon_log
        .lines()
        .find(|line| line.contains("remuda daemon: mod command failed:"))
        .expect("mod failure log line");
    assert!(line.contains("\\nNext: hidden"), "{line:?}");
    assert!(
        line.contains("\\u{1b}") || line.contains("\\x1b"),
        "{line:?}"
    );
    assert!(
        line.ends_with("..."),
        "expected truncation marker: {line:?}"
    );
    assert!(line.len() <= "remuda daemon: mod command failed: ".len() + 4096);
}
