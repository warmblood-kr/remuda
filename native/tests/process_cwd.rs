//! `cwd` on `remuda.process.run` and `remuda.process`: the child starts in
//! the directory the caller names, and a bad `cwd` is refused in one line.

use remuda_core::protocol::{Request, Response};
use remuda_native::{client, daemon};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
        let dir = root.join(format!("remuda-c{}-{tag}", std::process::id()));
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

/// One daemon process in its own runtime directory, with a work directory
/// that holds one marker file. Fields drop in order: the daemon dies first.
struct Node {
    _daemon: spawn::Daemon,
    dir: Scratch,
    work: PathBuf,
}

const MARKER: &str = "remuda-cwd-marker.txt";

impl Node {
    fn start(tag: &str) -> Self {
        let dir = Scratch::new(tag);
        let work = dir.0.join("work");
        std::fs::create_dir_all(&work).expect("create work directory");
        std::fs::write(work.join(MARKER), "x").expect("write marker");
        let daemon = spawn::Daemon::spawn(&dir.0);
        Self {
            _daemon: daemon,
            dir,
            work,
        }
    }

    fn request(&self, code: &str) -> Response {
        let socket = daemon::socket_path_in(&self.dir.0, "s");
        let request = Request::Eval {
            code: code.to_string(),
            name: None,
        };
        client::request(&socket, &request).expect("eval request")
    }

    fn eval(&self, code: &str) -> String {
        match self.request(code) {
            Response::Value(value) => value,
            other => panic!("eval failed: {other:?}"),
        }
    }

    /// The message of the Lua error `code` raises.
    fn error_of(&self, code: &str) -> String {
        self.eval(&format!(
            "local ok, err = pcall(function() {code} end)
             return ok and 'no error' or tostring(err)"
        ))
    }
}

fn lua_string(path: &Path) -> String {
    format!("{:?}", path.to_str().expect("utf-8 scratch path"))
}

/// A Lua argv that lists the names in the child's working directory.
fn list_argv() -> &'static str {
    if cfg!(windows) {
        "{ 'cmd.exe', '/c', 'dir', '/b' }"
    } else {
        "{ '/bin/ls' }"
    }
}

fn assert_refused(message: &str, shown_nowhere: &Path) {
    assert!(
        message.contains("cwd must be an absolute path to an existing directory")
            && message.contains("Next: pass the directory's full path."),
        "{message}"
    );
    assert!(!message.contains('\n'), "one line: {message}");
    let path = shown_nowhere.to_str().unwrap();
    assert!(!message.contains(path), "the path is not echoed: {message}");
}

#[test]
fn process_run_starts_the_child_in_cwd() {
    let node = Node::start("run");
    let argv = list_argv();
    let listed = node.eval(&format!(
        "local result = remuda.process.run({{ argv = {argv}, cwd = {} }})
         return tostring(result.code) .. '|' .. result.stdout",
        lua_string(&node.work)
    ));
    assert!(
        listed.starts_with("0|") && listed.contains(MARKER),
        "{listed}"
    );

    // Without cwd nothing changes: the child inherits the daemon's directory.
    let inherited = node.eval(&format!(
        "return remuda.process.run({{ argv = {argv} }}).stdout"
    ));
    assert!(!inherited.contains(MARKER), "{inherited}");
}

#[test]
fn process_run_refuses_a_bad_cwd_in_one_line_without_the_path() {
    let node = Node::start("refuse");
    let argv = list_argv();
    let file = node.work.join(MARKER);
    let missing = node.work.join("no-such-directory");
    for bad in [&file, &missing] {
        let message = node.error_of(&format!(
            "remuda.process.run({{ argv = {argv}, cwd = {} }})",
            lua_string(bad)
        ));
        assert_refused(&message, bad);
    }
    for bad in ["'work'", "'.'", "''", "42", "{}"] {
        let message = node.error_of(&format!(
            "remuda.process.run({{ argv = {argv}, cwd = {bad} }})"
        ));
        assert_refused(&message, &node.work);
    }
}

#[test]
fn async_process_starts_in_cwd_and_refuses_a_bad_one() {
    let node = Node::start("async");
    let argv = list_argv();
    node.eval(&format!(
        "_G.cwd_lines = {{}}
         remuda.on('cwd-line', function(line) _G.cwd_lines[#_G.cwd_lines + 1] = line end)
         remuda.process({{ argv = {argv}, cwd = {}, on_line = 'cwd-line' }})
         return 'started'",
        lua_string(&node.work)
    ));
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = String::new();
    while Instant::now() < deadline && !seen.contains(MARKER) {
        std::thread::sleep(Duration::from_millis(50));
        seen = node.eval("return table.concat(_G.cwd_lines, ',')");
    }
    assert!(seen.contains(MARKER), "lines: {seen:?}");

    let missing = node.work.join("no-such-directory");
    let message = node.error_of(&format!(
        "remuda.process({{ argv = {argv}, cwd = {} }})",
        lua_string(&missing)
    ));
    assert_refused(&message, &missing);
    let message = node.error_of(&format!(
        "remuda.process({{ argv = {argv}, cwd = 'work' }})"
    ));
    assert_refused(&message, &node.work);
}

#[cfg(unix)]
#[test]
fn a_symlink_to_a_directory_is_a_directory() {
    let node = Node::start("link");
    let link = node.dir.0.join("link");
    std::os::unix::fs::symlink(&node.work, &link).expect("symlink");
    let listed = node.eval(&format!(
        "return remuda.process.run({{ argv = {}, cwd = {} }}).stdout",
        list_argv(),
        lua_string(&link)
    ));
    assert!(listed.contains(MARKER), "{listed}");
}

/// With `cwd`, a relative program path would resolve differently per OS
/// (std calls that case platform specific and unstable), so it is refused.
#[test]
fn a_relative_program_path_with_cwd_is_refused_on_every_os() {
    let node = Node::start("rel");
    let cwd = lua_string(&node.work);
    let mut relative = vec!["'./tool'", "'bin/tool'"];
    if cfg!(windows) {
        relative.extend(["'.\\\\tool.cmd'", "'bin\\\\tool'"]);
    }
    for program in relative {
        for word in ["remuda.process.run", "remuda.process"] {
            let message = node.error_of(&format!(
                "{word}({{ argv = {{ {program} }}, cwd = {cwd} }})"
            ));
            assert!(
                message.contains("with cwd needs an absolute program path or a bare command name")
                    && message.contains("Next: pass the full path of the program."),
                "{word} {program}: {message}"
            );
            assert!(!message.contains('\n'), "one line: {message}");
        }
        // Without cwd the word is unchanged: no such refusal.
        let message = node.error_of(&format!("remuda.process.run({{ argv = {{ {program} }} }})"));
        assert!(!message.contains("with cwd needs"), "{program}: {message}");
    }

    // A bare command name is searched on PATH and still runs in cwd.
    let bare = if cfg!(windows) {
        "{ 'cmd.exe', '/c', 'dir', '/b' }"
    } else {
        "{ 'ls' }"
    };
    let listed = node.eval(&format!(
        "return remuda.process.run({{ argv = {bare}, cwd = {cwd} }}).stdout"
    ));
    assert!(listed.contains(MARKER), "{listed}");
}
