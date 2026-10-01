//! A Windows user folder with a non-ASCII name: Lua's own `os` and `io` (the C
//! runtime) and the Rust-backed `remuda.*` words must agree on its bytes.
//! Lua's C runtime speaks the process ANSI code page and Rust speaks UTF-8,
//! so the same path must read as UTF-8 on both sides, in both directions.
#![cfg(windows)]

use remuda_core::protocol::{Request, Response};
use remuda_native::{client, daemon};
use std::path::{Path, PathBuf};

#[path = "daemon_support/spawn.rs"]
mod spawn;

/// Hangul and a space: outside every single-byte code page.
const USER: &str = "한글 사용자";

/// A directory of our own, removed when the test ends, passing or not.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("remuda-u{}-{tag}", std::process::id()));
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

/// What `code` returns in the daemon under `runtime`, or the Lua error it
/// raises, as text: a refusal is reported beside the others, not a panic.
fn outcome(runtime: &Path, code: &str) -> String {
    let request = Request::Eval {
        code: format!("local _, value = pcall(function() {code} end)\nreturn tostring(value)"),
        name: None,
    };
    let socket = daemon::socket_path_in(runtime, "s");
    match client::request(&socket, &request).expect("eval request") {
        Response::Value(value) => value,
        other => format!("{other:?}"),
    }
}

fn check(wrong: &mut Vec<String>, what: &str, got: &str, want: &str) {
    if got != want {
        wrong.push(format!("{what}: got {got:?}, want {want:?}"));
    }
}

fn lua_string(path: &Path) -> String {
    format!("{:?}", path.to_str().expect("utf-8 scratch path"))
}

const READ_NOTE: &str = r#"local file = assert(io.open(dir .. "\\note.txt"))
    local text = file:read("a")
    file:close()
    return text"#;

#[test]
fn a_hangul_user_folder_is_one_path_for_lua_and_for_rust() {
    let dir = Scratch::new("user");
    let user = dir.0.join(USER);
    std::fs::create_dir_all(&user).expect("create user folder");
    std::fs::write(user.join("note.txt"), "from rust").expect("write note");
    let mut command = spawn::base_command(&dir.0);
    command.env("USERPROFILE", &user);
    let _daemon = spawn::spawn_and_wait(command, &dir.0);

    let from_env = r#"local dir = os.getenv("USERPROFILE")"#;
    let mut wrong = Vec::new();
    check(
        &mut wrong,
        "os.getenv",
        &outcome(&dir.0, &format!("{from_env} return dir")),
        user.to_str().expect("utf-8 scratch path"),
    );
    check(
        &mut wrong,
        "remuda.mkdir with a path from os.getenv",
        &outcome(
            &dir.0,
            &format!(r#"{from_env} remuda.mkdir(dir .. "\\made") return "made""#),
        ),
        "made",
    );
    if !user.join("made").is_dir() {
        wrong.push("remuda.mkdir: the directory is not in the user folder".into());
    }
    check(
        &mut wrong,
        "io.open with a path from os.getenv",
        &outcome(&dir.0, &format!("{from_env} {READ_NOTE}")),
        "from rust",
    );
    // The other direction: a path as a Rust word returns it, in UTF-8.
    check(
        &mut wrong,
        "io.open with a UTF-8 path",
        &outcome(
            &dir.0,
            &format!("local dir = {} {READ_NOTE}", lua_string(&user)),
        ),
        "from rust",
    );
    check(
        &mut wrong,
        "remuda.fs.lock with a path from os.getenv",
        &outcome(
            &dir.0,
            &format!(
                r#"{from_env} local handle, why = remuda.fs.lock(dir .. "\\lock")
                return handle and "locked" or tostring(why)"#
            ),
        ),
        "locked",
    );
    assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
}

/// The runtime directory and the data home themselves under the Hangul name.
#[test]
fn a_daemon_under_a_hangul_runtime_directory_can_use_its_data_home() {
    let dir = Scratch::new("home");
    let runtime = dir.0.join(USER);
    std::fs::create_dir_all(&runtime).expect("create runtime directory");
    let _daemon = spawn::Daemon::spawn(&runtime);
    // `base_command` puts HOME and LOCALAPPDATA here.
    let home = runtime.join("home");

    let from_env = r#"local dir = os.getenv("LOCALAPPDATA")"#;
    let mut wrong = Vec::new();
    check(
        &mut wrong,
        "os.getenv",
        &outcome(&runtime, &format!("{from_env} return dir")),
        home.to_str().expect("utf-8 scratch path"),
    );
    check(
        &mut wrong,
        "remuda.mkdir with a path from os.getenv",
        &outcome(
            &runtime,
            &format!(r#"{from_env} remuda.mkdir(dir .. "\\made") return "made""#),
        ),
        "made",
    );
    if !home.join("made").is_dir() {
        wrong.push("remuda.mkdir: the directory is not in the data home".into());
    }
    assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
}
