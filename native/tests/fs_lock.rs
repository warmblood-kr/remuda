//! `remuda.fs.lock` between real daemon processes: who owns a path, and what
//! happens to the lock when the owner dies.

use remuda_core::protocol::{Request, Response};
use remuda_native::{client, daemon};
use std::path::{Path, PathBuf};

#[path = "daemon_support/spawn.rs"]
mod spawn;

/// A directory of our own, short enough for `sun_path` (~108 bytes).
fn scratch(tag: &str) -> PathBuf {
    let root = if cfg!(unix) {
        PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    let dir = root
        .canonicalize()
        .expect("canonical temp directory")
        .join(format!("remuda-l{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch");
    dir
}

struct Node {
    dir: PathBuf,
    daemon: spawn::Daemon,
}

impl Node {
    fn start(tag: &str) -> Self {
        let dir = scratch(tag);
        let daemon = spawn::Daemon::spawn(&dir);
        Self { dir, daemon }
    }

    fn eval(&self, code: &str) -> String {
        let socket = daemon::socket_path_in(&self.dir, "s");
        let request = Request::Eval {
            code: code.to_string(),
            name: None,
        };
        match client::request(&socket, &request) {
            Ok(Response::Value(value)) => value,
            other => panic!("eval failed: {other:?}"),
        }
    }

    /// Ask for the lock and describe the answer as one line of text.
    fn lock(&self, path: &Path) -> String {
        self.eval(&format!(
            "local handle, why, info = remuda.fs.lock({:?})
             if handle then _G.kept = handle return 'acquired' end
             return tostring(why) .. '|' .. tostring(info)",
            path.to_str().expect("utf-8 scratch path")
        ))
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.daemon.0.kill();
        let _ = self.daemon.0.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn a_second_daemon_sees_held_with_the_first_daemons_session_and_pid() {
    let shared = scratch("held-shared");
    let path = shared.join("lock");
    let first = Node::start("held-a");
    let second = Node::start("held-b");

    assert_eq!(first.lock(&path), "acquired");
    let answer = second.lock(&path);
    let expected = format!(
        "held|remuda-lock session=s pid={} since=",
        first.daemon.0.id()
    );
    assert!(answer.starts_with(&expected), "{answer:?}");

    let _ = std::fs::remove_dir_all(&shared);
}

#[test]
fn the_lock_is_released_when_the_owning_daemon_is_killed() {
    let shared = scratch("kill-shared");
    let path = shared.join("lock");
    let mut first = Node::start("kill-a");
    let second = Node::start("kill-b");

    assert_eq!(first.lock(&path), "acquired");
    assert!(second.lock(&path).starts_with("held|"));

    // SIGKILL on Unix, TerminateProcess on Windows: no cleanup code runs.
    first.daemon.0.kill().expect("kill the owning daemon");
    first.daemon.0.wait().expect("reap the owning daemon");
    assert_eq!(second.lock(&path), "acquired");

    let _ = std::fs::remove_dir_all(&shared);
}

#[test]
fn the_same_daemon_gets_the_same_handle_and_release_frees_it() {
    let shared = scratch("same-shared");
    let path = shared.join("lock");
    let first = Node::start("same-a");
    let second = Node::start("same-b");
    let quoted = format!("{:?}", path.to_str().expect("utf-8 scratch path"));

    let same = first.eval(&format!(
        "local a = assert(remuda.fs.lock({quoted}))
         local b = assert(remuda.fs.lock({quoted}))
         _G.kept = nil
         collectgarbage() collectgarbage()
         local c = assert(remuda.fs.lock({quoted}))
         return tostring(rawequal(a, b) and rawequal(a, c)) .. '|' .. tostring(a.path == {quoted})"
    ));
    assert_eq!(same, "true|true");
    // Dropping every Lua reference did not give the lock away.
    assert!(second.lock(&path).starts_with("held|"));

    let released = first.eval(&format!(
        "local handle = assert(remuda.fs.lock({quoted}))
         return tostring(handle:release()) .. '|' .. tostring(handle:release())"
    ));
    assert_eq!(released, "true|false");
    assert_eq!(second.lock(&path), "acquired");

    let _ = std::fs::remove_dir_all(&shared);
}
