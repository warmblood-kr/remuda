//! `remuda.system.find_command` in a real daemon whose PATH we control: it
//! finds what is on PATH, never the current directory, and runs nothing.

use remuda_core::protocol::{Request, Response};
use remuda_native::{client, daemon};
use std::path::{Path, PathBuf};

#[path = "daemon_support/spawn.rs"]
mod spawn;

/// A directory of our own, short enough for `sun_path` (~108 bytes), removed
/// when the test ends, passing or not.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        // macOS refuses a runtime directory reached through the /tmp symlink.
        let root = if cfg!(unix) {
            PathBuf::from("/tmp")
                .canonicalize()
                .expect("canonical /tmp")
        } else {
            std::env::temp_dir()
        };
        let dir = root.join(format!("remuda-f{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The file name a command called `name` has on this OS.
fn command_file(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.cmd")
    } else {
        name.to_owned()
    }
}

/// A command that would leave a mark if anything ever ran it.
fn write_command(dir: &Path, name: &str, mark: &Path) {
    let file = dir.join(command_file(name));
    let body = if cfg!(windows) {
        format!("@echo ran> \"{}\"\r\n", mark.display())
    } else {
        format!("#!/bin/sh\necho ran > '{}'\n", mark.display())
    };
    std::fs::write(&file, body).expect("write command");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

#[test]
fn find_command_searches_absolute_path_entries_only_and_runs_nothing() {
    let scratch = Scratch::new("find");
    let (bin, cwd) = (scratch.0.join("bin"), scratch.0.join("cwd"));
    let mark = scratch.0.join("ran.txt");
    for dir in [&bin, &cwd, &cwd.join("relbin"), &bin.join("adir")] {
        std::fs::create_dir_all(dir).expect("create directory");
    }
    write_command(&bin, "stub", &mark);
    write_command(&cwd, "cwdtool", &mark);
    write_command(&cwd.join("relbin"), "reltool", &mark);
    // On Unix a file without an execute bit is not a command.
    std::fs::write(bin.join("notexec"), "data").expect("write plain file");

    // PATH: a relative entry, an empty entry (the current directory on Unix),
    // then the one absolute entry.
    let separator = if cfg!(windows) { ";" } else { ":" };
    let path = ["relbin", "", bin.to_str().expect("utf-8 path")].join(separator);
    let mut command = spawn::base_command(&scratch.0);
    command.env("PATH", path).current_dir(&cwd);
    let _daemon = spawn::spawn_and_wait(command, &scratch.0);

    let find = |name: &str| {
        let socket = daemon::socket_path_in(&scratch.0, "s");
        let code = format!(
            "local path, reason = remuda.system.find_command({name:?})
             return tostring(path) .. '|' .. tostring(reason)"
        );
        let request = Request::Eval { code, name: None };
        match client::request(&socket, &request) {
            Ok(Response::Value(value)) => value,
            other => panic!("eval failed: {other:?}"),
        }
    };

    let found = find("stub");
    let expected = bin.join(command_file("stub"));
    let (path, reason) = found.split_once('|').expect("two values");
    assert_eq!(reason, "nil", "{found}");
    assert!(
        path.eq_ignore_ascii_case(expected.to_str().unwrap()),
        "{path} is not {}",
        expected.display()
    );

    for (name, why) in [
        ("cwdtool", "is not on PATH"),
        ("reltool", "is not on PATH"),
        ("adir", "is not on PATH"),
        ("nonexistent-xyz", "is not on PATH"),
        ("relbin/reltool", "needs a bare command name"),
        ("../cwd/cwdtool", "needs a bare command name"),
    ] {
        let answer = find(name);
        assert!(
            answer.starts_with("nil|") && answer.contains(why) && answer.contains("Next: "),
            "{name}: {answer}"
        );
        assert!(!answer.contains('\n'), "one line: {answer}");
    }
    if cfg!(unix) {
        assert!(
            find("notexec").starts_with("nil|"),
            "a plain file is not a command"
        );
    }
    assert!(!mark.exists(), "find_command ran a candidate");
}
