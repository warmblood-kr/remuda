//! buffer and window: the two nouns `tools.lua` adds so an extension can hold
//! named text and a screen rectangle without reaching into `Registry`.
//!
//! Pure Lua state (see `tools.lua`'s own doc comment) — every claim here is
//! checked the same way `remuda.tool`/`remuda.schedule` already are
//! elsewhere: `Request::Eval` against a real daemon, never a Rust-side type.

use remuda_core::protocol::{Request, Response};
use remuda_native::{client, daemon};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(10);

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("remuda-bw{}-{tag}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn daemon_at(path: &Path) -> impl Drop {
    let serving = path.to_path_buf();
    std::thread::spawn(move || {
        let _ = daemon::serve(&serving);
    });
    let deadline = Instant::now() + PATIENCE;
    while remuda_native::ipc::connect(path).is_err() {
        assert!(Instant::now() < deadline, "daemon never bound {path:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
    Cleanup(path.to_path_buf())
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// One `Eval`, returned as text. Panics with the daemon's own message on a
/// Lua error, so a failing assertion below points at the actual raise.
fn eval(path: &Path, code: &str) -> String {
    match client::request(
        path,
        &Request::Eval {
            code: code.to_string(),
            name: None,
        },
    ) {
        Ok(Response::Value(text)) => text,
        other => panic!("eval failed for {code:?}: {other:?}"),
    }
}

#[test]
fn a_buffer_is_created_once_and_shared_by_name() {
    let dir = scratch("buffer");
    let socket = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&socket);

    assert_eq!(
        eval(&socket, "return remuda.buffer.list()[1] or ''"),
        "",
        "a buffer exists before it was ever named — this test proves nothing"
    );

    eval(&socket, "remuda.buffer.new('*inbox*'):set('first')");
    assert_eq!(
        eval(&socket, "return remuda.buffer.new('*inbox*'):get()"),
        "first",
        "new() on an existing name must return the same buffer, not a fresh empty one"
    );

    eval(&socket, "remuda.buffer.new('*inbox*'):append('-more')");
    assert_eq!(
        eval(&socket, "return remuda.buffer.new('*inbox*'):get()"),
        "first-more",
        "append must add to the existing text, not replace it"
    );

    assert_eq!(
        eval(&socket, "return table.concat(remuda.buffer.list(), ',')"),
        "*inbox*",
        "a created buffer must be listed by name"
    );
}

#[test]
fn a_buffer_does_not_know_what_its_text_means() {
    let dir = scratch("buffer-opaque");
    let socket = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&socket);

    // The design's own boundary: a buffer is a bag of bytes. Lua source as
    // text must round-trip unexamined, not be interpreted or escaped.
    eval(
        &socket,
        r#"remuda.buffer.new('*raw*'):set('" .. os.exit() .. "')"#,
    );
    assert_eq!(
        eval(&socket, "return remuda.buffer.new('*raw*'):get()"),
        r#"" .. os.exit() .. ""#,
        "buffer content must round-trip as opaque text, not be interpreted"
    );
}

#[test]
fn a_window_split_makes_a_second_independent_window() {
    let dir = scratch("window");
    let socket = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&socket);

    assert_eq!(
        eval(&socket, "return remuda.window.current().id"),
        "main",
        "current() before any split is the one implicit window"
    );
    assert_eq!(
        eval(&socket, "return remuda.window.current().id"),
        "main",
        "current() called twice must return the same window, not a new one each time"
    );

    let side_id = eval(
        &socket,
        "local side = remuda.window.current():split('right'); return side.id",
    );
    assert_ne!(
        side_id, "main",
        "a split window must be a distinct window from the one it split from"
    );

    eval(
        &socket,
        "remuda.window.current():split('right') -- discard, just to prove split doesn't mutate current\n\
         return 1",
    );
    assert_eq!(
        eval(&socket, "return remuda.window.current().id"),
        "main",
        "splitting must not change what current() answers"
    );
}

#[test]
fn a_window_shows_a_target_and_closing_it_kills_nothing() {
    let dir = scratch("window-show-close");
    let socket = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&socket);

    eval(
        &socket,
        "remuda.buffer.new('*sessions*'):set('alpha\\nbeta')",
    );
    let side_id = eval(
        &socket,
        "local side = remuda.window.current():split('right')\n\
         side:show(remuda.buffer.new('*sessions*'))\n\
         return side.id",
    );

    // Closing the window must not touch the buffer it showed — the tmux
    // rejection this model deliberately does not resurrect (steps/006).
    eval(&socket, &format!("remuda.windows['{side_id}']:close()"));
    assert_eq!(
        eval(&socket, "return remuda.buffer.new('*sessions*'):get()"),
        "alpha\nbeta",
        "closing a window that showed a buffer must not clear or delete that buffer"
    );
    assert_eq!(
        eval(
            &socket,
            &format!("return remuda.windows['{side_id}'] == nil and 'gone' or 'still-there'")
        ),
        "gone",
        "close() must actually remove the window"
    );
}
