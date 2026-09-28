//! The daemon end to end: over a real socket, and through a real terminal.
//!
//! 정수님, 2026-09-10: *"a tmux like pty manager, which support daemon mode.
//! with session list so that user can select a session to attach."* These are
//! the three claims in that sentence — sessions outlive their client, they can
//! be listed, and one can be attached — each exercised rather than asserted.
//!
//! The attach test runs the shipped `remuda` binary **inside a pty of our own**,
//! which is the only way to exercise raw mode and the detach key at all: those
//! paths are unreachable without a controlling terminal. remuda is used to test
//! remuda, and that is not circular — the pty under the test is this crate's,
//! the terminal under test is the binary's.

use remuda_core::protocol::{Request, Response};
use remuda_core::{Session, Size};
use remuda_native::{client, daemon, ipc, CommandBuilder, PtyAgent, SystemClock};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(10);

#[path = "daemon_support/spawn.rs"]
mod spawn;
use spawn::Daemon;

/// A runtime directory of our own. Short enough for `sun_path` (~108 bytes) —
/// a long path fails at bind with a message no caller would guess from a
/// timeout, which is what the binary's startup-error handling exists for.
fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("remuda-t{}-{tag}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// The address, derived the way the shipped binary derives it. Hand-building
/// one that merely resembles it is what made the attach test fail first time.
fn scratch(tag: &str) -> PathBuf {
    daemon::socket_path_in(&scratch_dir(tag), "s")
}

/// Start a daemon and return once it actually answers, not once it was spawned.
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

fn new_session(path: &Path, name: &str) {
    let response = client::request(
        path,
        &Request::New {
            name: Some(name.to_string()),
            command: vec!["sh".into()],
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .expect("new");
    assert_eq!(
        response,
        Response::Value(name.to_string()),
        "New answers with the name it gave the session"
    );
}

fn capture(path: &Path, name: &str) -> String {
    match client::request(
        path,
        &Request::Capture {
            name: name.to_string(),
        },
    ) {
        Ok(Response::Screen(text)) => text,
        other => panic!("capture failed: {other:?}"),
    }
}

fn wait_for(path: &Path, name: &str, needle: &str) -> String {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let screen = capture(path, name);
        if screen.contains(needle) {
            return screen;
        }
        assert!(
            Instant::now() < deadline,
            "{needle:?} never appeared in {name}. screen:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_session_screen(session: &Session, needle: &str) {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let screen = session.screen_text().expect("viewer screen");
        if screen.contains(needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "attach did not repaint {needle:?}. viewer saw:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn collect_until_bytes(receiver: &Receiver<Vec<u8>>, needle: &[u8]) -> Vec<u8> {
    let deadline = Instant::now() + PATIENCE;
    let mut output = Vec::new();
    while !output.windows(needle.len()).any(|window| window == needle) {
        if Instant::now() >= deadline {
            panic!(
                "terminal output omitted {needle:?}; received {} bytes, tail: {}",
                output.len(),
                escaped_tail(&output)
            );
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining.min(Duration::from_millis(500))) {
            Ok(chunk) => output.extend_from_slice(&chunk),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                panic!(
                    "attach client exited before writing {needle:?}; received {} bytes, tail: {}",
                    output.len(),
                    escaped_tail(&output)
                )
            }
        }
    }
    output
}

fn escaped_tail(output: &[u8]) -> String {
    let start = output.len().saturating_sub(400);
    output[start..]
        .iter()
        .flat_map(|byte| std::ascii::escape_default(*byte))
        .map(char::from)
        .collect()
}

#[cfg(windows)]
fn assert_bytes_in_order(output: &[u8], needles: &[&[u8]]) {
    let mut cursor = 0;
    for needle in needles {
        let Some(offset) = output[cursor..]
            .windows(needle.len())
            .position(|window| window == *needle)
        else {
            panic!(
                "terminal output omitted ordered bytes {needle:?}; received {} bytes, tail: {}",
                output.len(),
                escaped_tail(output)
            );
        };
        cursor += offset + needle.len();
    }
}

fn assert_detach_restore(receiver: &Receiver<Vec<u8>>) {
    #[cfg(unix)]
    let _received_restore = collect_until_bytes(
        receiver,
        b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l\x1b[?2004l",
    );
    #[cfg(windows)]
    {
        let received = collect_until_bytes(receiver, b"remuda: detached");
        assert_bytes_in_order(
            &received,
            // ConPTY strips the mouse resets, reorders the shell's own ?2004l
            // and repaints (even truncates) lines, so only the typed round
            // trip and the detach prefix are stable; the release poll below
            // proves the detach itself.
            &[b"42-typed", b"remuda: detached"],
        );
    }
}

fn traced_input(path: &Path) -> Vec<u8> {
    std::fs::read_to_string(path)
        .expect("input trace file")
        .lines()
        .filter_map(|line| line.split_once(' '))
        .flat_map(|(_, bytes)| bytes.split_ascii_whitespace())
        .map(|byte| u8::from_str_radix(byte, 16).expect("hex byte"))
        .collect()
}

#[test]
fn a_second_listener_cannot_unlink_a_live_daemons_socket() {
    let path = scratch("live-listener");
    let _first = ipc::listen(&path).expect("first listener binds");

    let second = ipc::listen(&path).expect_err("a live listener must keep its address");
    // Windows refuses a second FILE_FLAG_FIRST_PIPE_INSTANCE pipe with
    // ERROR_ACCESS_DENIED, which std reports as PermissionDenied.
    let expected = if cfg!(windows) {
        std::io::ErrorKind::PermissionDenied
    } else {
        std::io::ErrorKind::AddrInUse
    };
    assert_eq!(second.kind(), expected);
    assert!(
        ipc::connect(&path).is_ok(),
        "the first daemon remains reachable"
    );
}

#[test]
fn sessions_are_listed_and_kept_apart() {
    let path = scratch("list");
    let _daemon = daemon_at(&path);

    // Negative control: the list is empty before anything is started, so a
    // "sessions appear" assertion cannot be satisfied by a list that is simply
    // always full.
    match client::request(&path, &Request::List).expect("list") {
        Response::Sessions(s) => assert!(s.is_empty(), "a fresh daemon holds nothing"),
        other => panic!("unexpected: {other:?}"),
    }

    new_session(&path, "alpha");
    new_session(&path, "bravo");

    let names = match client::request(&path, &Request::List).expect("list") {
        Response::Sessions(s) => s.into_iter().map(|s| s.name).collect::<Vec<_>>(),
        other => panic!("unexpected: {other:?}"),
    };
    assert_eq!(names, ["alpha", "bravo"]);

    // Instructions must land in the session they name and nowhere else — the
    // property a shared pty would break.
    client::request(
        &path,
        &Request::SendLine {
            name: "alpha".into(),
            text: "echo $((6*7))-alpha".into(),
        },
    )
    .expect("send");

    wait_for(&path, "alpha", "42-alpha");
    let bravo = capture(&path, "bravo");
    assert!(
        !bravo.contains("42-alpha"),
        "bravo saw alpha's output:\n{bravo}"
    );
}

#[test]
fn an_unnamed_session_names_itself_after_the_program_and_dedupes() {
    // Bug A by construction: the person types the program, never a name, so no
    // leading positional can eat the word they meant to run.
    let path = scratch("generated");
    let _daemon = daemon_at(&path);

    let make = || {
        client::request(
            &path,
            &Request::New {
                name: None,
                command: vec!["sh".into()],
                size: Size::new(80, 24),
                cwd: None,
                env: None,
            },
        )
        .expect("new")
    };

    assert_eq!(make(), Response::Value("sh".into()));
    assert_eq!(make(), Response::Value("sh-2".into()));
    assert_eq!(make(), Response::Value("sh-3".into()));

    // And the caller can address what it just made, which is the whole reason
    // `New` had to start answering with a value.
    let seen: Vec<String> = match client::request(&path, &Request::List).expect("ls") {
        Response::Sessions(sessions) => sessions.into_iter().map(|s| s.name).collect(),
        other => panic!("unexpected: {other:?}"),
    };
    assert_eq!(seen, vec!["sh", "sh-2", "sh-3"]);
}

#[test]
fn a_taken_name_is_refused_in_words() {
    let path = scratch("dup");
    let _daemon = daemon_at(&path);
    new_session(&path, "only");

    let again = client::request(
        &path,
        &Request::New {
            name: Some("only".into()),
            command: vec!["sh".into()],
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .expect("second new");

    match again {
        Response::Error(reason) => assert!(
            reason.contains("name taken"),
            "the reason must say what went wrong, got: {reason}"
        ),
        other => panic!("a duplicate name must be refused, got {other:?}"),
    }
}

#[test]
fn a_session_launches_into_the_cwd_it_is_given() {
    // Without a `cwd`, a session inherits the daemon's own directory — this
    // proves the caller can override that, not merely that the daemon starts.
    let path = scratch("cwd");
    let _daemon = daemon_at(&path);

    let dir = scratch_dir("cwd-target");
    let response = client::request(
        &path,
        &Request::New {
            name: Some("in-tmp".into()),
            command: vec!["pwd".into()],
            size: Size::new(80, 24),
            cwd: Some(dir.to_string_lossy().into_owned()),
            env: None,
        },
    )
    .expect("new");
    assert_eq!(response, Response::Value("in-tmp".into()));

    // `pwd`'s own rendering of a path is platform-specific (Windows' shell
    // spells it `\\?\C:\...`, a POSIX shell `/c/...`) — the directory's own
    // name is the one substring both agree on, so that is what we look for.
    let needle = dir
        .file_name()
        .and_then(|n| n.to_str())
        .expect("scratch dir has a name")
        .to_string();
    wait_for(&path, "in-tmp", &needle);
}

#[test]
fn a_live_session_resizes_and_reports_its_new_size() {
    let path = scratch("resize");
    let _daemon = daemon_at(&path);
    new_session(&path, "resizable");

    let target = Size::new(120, 36);
    assert_eq!(
        client::request(
            &path,
            &Request::Resize {
                name: "resizable".into(),
                size: target
            },
        )
        .expect("resize"),
        Response::Ok
    );
    let sessions = match client::request(&path, &Request::List).expect("list") {
        Response::Sessions(sessions) => sessions,
        other => panic!("unexpected: {other:?}"),
    };
    assert_eq!(sessions[0].size, target);
    client::request(
        &path,
        &Request::SendLine {
            name: "resizable".into(),
            text: "stty size".into(),
        },
    )
    .expect("ask terminal size");
    wait_for(&path, "resizable", "36 120");
}

#[test]
fn a_topic_directory_can_be_made_listed_and_removed_even_with_a_space_in_its_name() {
    // A space in the name is the whole point: `os.execute("mkdir -p ...")`
    // would mangle this, real `std::fs` calls do not.
    let path = scratch("dirverbs");
    let _daemon = daemon_at(&path);

    let base = scratch_dir("dirverbs-base");
    let target = base.join("topic with a space");
    let target_str = target.to_string_lossy().into_owned();

    let response = client::request(
        &path,
        &Request::Mkdir {
            path: target_str.clone(),
        },
    )
    .expect("mkdir");
    assert_eq!(response, Response::Ok);
    assert!(target.is_dir());

    std::fs::write(target.join("note.txt"), b"hi").expect("write");

    match client::request(
        &path,
        &Request::ListDir {
            path: target_str.clone(),
        },
    )
    .expect("list_dir")
    {
        Response::Entries(names) => assert_eq!(names, vec!["note.txt".to_string()]),
        other => panic!("unexpected: {other:?}"),
    }

    let response = client::request(&path, &Request::RemoveDirAll { path: target_str })
        .expect("remove_dir_all");
    assert_eq!(response, Response::Ok);
    assert!(!target.exists());
}

#[test]
fn a_wire_size_below_the_floor_is_clamped_not_honoured() {
    // A constructor that clamps is worth nothing if a peer can post JSON around
    // it. 11 columns is the width that silently ate keystrokes in the
    // implementation this replaces.
    let request: Request =
        serde_json::from_str(r#"{"New":{"name":"tiny","command":[],"size":{"cols":11,"rows":2}}}"#)
            .expect("parse");

    match request {
        Request::New { size, .. } => {
            assert_eq!(
                (size.cols(), size.rows()),
                (80, 24),
                "clamped on the way in"
            );
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// EXPR evaluated with `r` bound to session "target"'s Lua `ls()` row.
fn target_row(path: &Path, expr: &str) -> String {
    let code = format!(
        "for _, r in ipairs(remuda.ls()) do if r.name == 'target' then return tostring({expr}) end end"
    );
    match client::request(path, &Request::Eval { code, name: None }) {
        Ok(Response::Value(value)) => value,
        other => panic!("ls row: {other:?}"),
    }
}

#[test]
fn a_human_attaches_through_a_real_terminal_and_detaches_with_ctrl_backslash() {
    // The binary derives its socket as $REMUDA_RUNTIME_DIR/remuda/default.sock,
    // so the daemon must listen exactly there. Pointing the test somewhere else
    // is what made the first run fail — and it failed as "no repaint", which
    // names the wrong wall just like the swallowed startup error did.
    let dir = scratch_dir("attach");
    let path = daemon::socket_path_in(&dir, "default");
    let _daemon = daemon_at(&path);
    new_session(&path, "target");

    // Put something on screen BEFORE attaching, so the repaint has something to
    // prove. A viewer that only streams would show a blank terminal here.
    client::request(
        &path,
        &Request::SendLine {
            name: "target".into(),
            text: "echo $((11*11))-before".into(),
        },
    )
    .expect("send");
    wait_for(&path, "target", "121-before");

    // The real binary, on a real pty, so raw mode is actually entered.
    let trace_path = scratch_dir("attach-input-trace").join("input.hex");
    let _ = std::fs::remove_file(&trace_path);
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_remuda"));
    cmd.arg("attach");
    cmd.arg("target");
    cmd.env("REMUDA_RUNTIME_DIR", &dir);
    cmd.env("REMUDA_TRACE_INPUT", &trace_path);
    let viewer = Session::new(
        "viewer",
        Box::new(PtyAgent::spawn(cmd, Size::new(80, 24)).expect("spawn viewer")),
        Arc::new(SystemClock::new()),
    );
    let held = viewer.attach().expect("drive the viewer");
    let viewer_output = held.subscribe().expect("capture raw terminal output");

    // 1. Repaint: what was already there arrives without the program redrawing.
    wait_for_session_screen(&viewer, "121-before");

    // #136: a scripted SendLine is not a human keystroke.
    let human_idle = || match client::request(&path, &Request::List) {
        Ok(Response::Sessions(list)) => list
            .into_iter()
            .find(|s| s.name == "target")
            .and_then(|s| s.human_idle),
        other => panic!("list: {other:?}"),
    };
    assert_eq!(human_idle(), None);
    // In Lua a never-typed session is idle forever, so the field is always
    // present and its absence can only mean an older core.
    assert_eq!(target_row(&path, "r.human_idle == math.huge"), "true");

    // 2. Keystrokes reach the far session, and its output comes back.
    held.write_raw(b"echo $((6*7))-typed\r").expect("type");
    wait_for(&path, "target", "42-typed");
    // ...and the attached human's keystrokes are what `human_idle` counts,
    // in the listing and in Lua's `ls()` row.
    assert!(
        human_idle().is_some(),
        "typing through attach sets human_idle"
    );
    assert_eq!(target_row(&path, "r.human_idle < math.huge"), "true");

    // 3. Ctrl-\ detaches. The proof is on the far side: close is refused
    //    while attached and accepted afterwards, so this cannot pass by the
    //    client merely exiting for some other reason. Managed input is
    //    accepted either way (95fbe7e).
    let close = || {
        client::request(
            &path,
            &Request::Close {
                name: "target".into(),
            },
        )
    };
    assert!(
        matches!(close(), Ok(Response::Error(_))),
        "while a human holds it, close must be refused"
    );

    held.write_raw(&[client::DETACH]).expect("Ctrl-\\");
    drop(held);

    assert_detach_restore(&viewer_output);
    let traced = traced_input(&trace_path);
    assert!(
        traced
            .windows(b"echo $((6*7))-typed\r".len())
            .any(|window| { window == b"echo $((6*7))-typed\r" }),
        "input trace omitted the typed bytes: {traced:?}"
    );
    assert!(
        traced.contains(&client::DETACH),
        "input trace omitted Ctrl-\\: {traced:?}"
    );

    let resumed = client::request(
        &path,
        &Request::SendLine {
            name: "target".into(),
            text: "echo $((9*9))-after".into(),
        },
    );
    assert!(matches!(resumed, Ok(Response::Ok)), "{resumed:?}");
    wait_for(&path, "target", "81-after");

    // The server drops the hold asynchronously after Ctrl-\, so poll: close
    // must succeed once the release lands, and never before the deadline.
    let deadline = Instant::now() + PATIENCE;
    loop {
        let closed = close();
        if matches!(closed, Ok(Response::Ok)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "after detach the hold must be released, so close succeeds: {closed:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn session_listing_reports_the_child_mouse_tracking_mode() {
    let path = scratch("mouse-tracking-list");
    let _daemon = daemon_at(&path);
    new_session(&path, "target");

    let listed = || match client::request(&path, &Request::List) {
        Ok(Response::Sessions(sessions)) => sessions
            .into_iter()
            .find(|session| session.name == "target")
            .expect("target is listed"),
        other => panic!("list: {other:?}"),
    };
    assert!(!listed().mouse_tracking);

    client::request(
        &path,
        &Request::SendLine {
            name: "target".into(),
            text: "printf '\\033[?1000h'; printf 'ready-%s\\n' mode".into(),
        },
    )
    .expect("enable mouse tracking");
    wait_for(&path, "target", "ready-mode");
    assert!(listed().mouse_tracking);
}

#[test]
fn a_registered_schedule_actually_fires_through_a_real_daemon() {
    let path = scratch("schedule");
    let _daemon = daemon_at(&path);

    // `every` is seconds on native's own clock, kept tiny so the test's
    // PATIENCE window covers many ticks rather than racing a single one.
    let register = Request::Eval {
        code: r#"
            remuda.fired = 0
            remuda.schedule({
              name = "test-schedule",
              every = 0.01,
              run = function() remuda.fired = remuda.fired + 1 end,
            })
        "#
        .to_string(),
        name: None,
    };
    match client::request(&path, &register).expect("register") {
        Response::Value(_) => {}
        other => panic!("unexpected: {other:?}"),
    }

    let deadline = Instant::now() + PATIENCE;
    loop {
        let fired = client::request(
            &path,
            &Request::Eval {
                code: "return remuda.fired".to_string(),
                name: None,
            },
        )
        .expect("read fired");
        // >= 2, not != "0" — the name promises PERIODIC firing, and a
        // scheduler that fires once and stops must fail this test.
        if matches!(&fired, Response::Value(v) if v.parse::<u32>().is_ok_and(|n| n >= 2)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the schedule did not fire at least twice: {fired:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn eval(path: &Path, code: &str) -> String {
    match client::request(
        path,
        &Request::Eval {
            code: code.to_string(),
            name: None,
        },
    )
    .expect("eval")
    {
        Response::Value(v) => v,
        other => panic!("unexpected: {other:?}"),
    }
}

fn read_count(path: &Path, code: &str) -> u32 {
    eval(path, code).parse().expect("a number")
}

#[test]
fn a_session_exited_hook_fires_when_a_real_session_dies() {
    // `session_exited` is remuda's own first real event: fired once for each
    // session the daemon's ticker notices has died. Nothing calls
    // `remuda.emit("session_exited", ...)` anywhere yet, so this must fail red.
    let path = scratch("session-exited");
    let _daemon = daemon_at(&path);

    eval(
        &path,
        r#"
            remuda._session_exited_names = {}
            remuda.on("session_exited", function(name)
                table.insert(remuda._session_exited_names, name)
            end)
        "#,
    );

    // Exits on its own almost immediately, so the ticker's very next tick
    // (TICK_PERIOD is 1s, see daemon.rs) has something dead to reap well
    // inside PATIENCE.
    let response = client::request(
        &path,
        &Request::New {
            name: Some("short-lived".into()),
            command: vec!["sh".into(), "-c".into(), "exit 0".into()],
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .expect("new");
    assert_eq!(response, Response::Value("short-lived".into()));

    // Negative control: a session that stays alive throughout must never be
    // named — this is fine to trivially hold today too, since nothing fires
    // the event for anyone yet.
    new_session(&path, "long-lived");

    let deadline = Instant::now() + PATIENCE;
    loop {
        let seen = eval(
            &path,
            "return table.concat(remuda._session_exited_names, ',')",
        );
        if seen.split(',').any(|n| n == "short-lived") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "session_exited never fired for short-lived. seen so far: {seen:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let seen = eval(
        &path,
        "return table.concat(remuda._session_exited_names, ',')",
    );
    assert!(
        !seen.split(',').any(|n| n == "long-lived"),
        "a still-alive session must never appear in session_exited names: {seen:?}"
    );
}

#[test]
fn a_session_exited_hook_fires_once_when_a_session_is_closed() {
    // `close` removes the entry itself, so the reaper never sees it die: the
    // close path must fire the event, and the reaper must not fire it again.
    let path = scratch("session-exited-close");
    let _daemon = daemon_at(&path);

    eval(
        &path,
        r#"
            remuda._session_exited_names = {}
            remuda.on("session_exited", function(name)
                table.insert(remuda._session_exited_names, name)
            end)
        "#,
    );
    new_session(&path, "closed");
    let response = client::request(
        &path,
        &Request::Close {
            name: "closed".into(),
        },
    )
    .expect("close");
    assert_eq!(response, Response::Ok);

    let count = "local n = 0 for _, v in ipairs(remuda._session_exited_names) do if v == 'closed' then n = n + 1 end end return n";
    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, count) == 0 {
        assert!(
            Instant::now() < deadline,
            "session_exited never fired for a closed session"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // Two more ticks: a late reap must not fire a duplicate.
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(
        read_count(&path, count),
        1,
        "a closed session must fire exactly once"
    );
}

#[test]
fn a_session_exited_hook_still_fires_once_when_ls_reaps_before_the_tick() {
    // `Registry::reap()` removes what it finds and hands it only to whoever
    // calls first. A `List` that reaps well inside TICK_PERIOD must not make
    // the ticker's own later reap silently find nothing to emit for — and it
    // must not double-emit either, once every reap site funnels through one
    // notifying path.
    let path = scratch("session-exited-race");
    let _daemon = daemon_at(&path);

    eval(
        &path,
        r#"
            remuda._race_exited_names = {}
            remuda.on("session_exited", function(name)
                table.insert(remuda._race_exited_names, name)
            end)
        "#,
    );

    let response = client::request(
        &path,
        &Request::New {
            name: Some("race-short-lived".into()),
            command: vec!["sh".into(), "-c".into(), "exit 0".into()],
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .expect("new");
    assert_eq!(response, Response::Value("race-short-lived".into()));

    // Well inside TICK_PERIOD (1s) — this reaps the session before the
    // ticker's own tick has a chance to.
    std::thread::sleep(Duration::from_millis(80));
    client::request(&path, &Request::List).expect("list");

    // Give the ticker a full period too, so a double-emit (both paths firing)
    // would have every chance to show up if the funnel were not idempotent.
    std::thread::sleep(Duration::from_millis(1200));

    let names = eval(&path, "return table.concat(remuda._race_exited_names, ',')");
    let count = names
        .split(',')
        .filter(|n| *n == "race-short-lived")
        .count();
    assert_eq!(
        count, 1,
        "expected exactly one session_exited for race-short-lived, got {count}: {names:?}"
    );
}

#[test]
fn a_hostile_session_name_reaches_the_hook_byte_for_byte() {
    // `Request::New`'s name has no validation at all beyond uniqueness (see
    // `spawn`/`Registry::register`) — a caller may hand over anything. The
    // emit path splices it into a Lua source string via `mcp::lua_string`, so
    // a quote or backslash proves that escaping is real, not merely untested.
    let path = scratch("session-exited-hostile");
    let _daemon = daemon_at(&path);
    let hostile = r#"it's a "test" \with\backslashes"#;

    eval(&path, "remuda._hostile_exited_names = {}");
    eval(
        &path,
        "remuda.on(\"session_exited\", function(name) \
            table.insert(remuda._hostile_exited_names, name) \
         end)",
    );

    let response = client::request(
        &path,
        &Request::New {
            name: Some(hostile.to_string()),
            command: vec!["sh".into(), "-c".into(), "exit 0".into()],
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .expect("new");
    assert_eq!(response, Response::Value(hostile.to_string()));

    let deadline = Instant::now() + PATIENCE;
    loop {
        let count = read_count(&path, "return #remuda._hostile_exited_names");
        if count >= 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "session_exited never fired for the hostile name"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Exact match, not a substring or a count — proof the name arrived intact
    // rather than truncated or escaped-then-left-escaped by a naive splice.
    assert_eq!(
        eval(&path, "return remuda._hostile_exited_names[1]"),
        hostile,
        "the hostile name did not survive the emit path unchanged"
    );
    assert_eq!(
        read_count(&path, "return #remuda._hostile_exited_names"),
        1,
        "no injected code should have run more than once, nor duplicated the entry"
    );
}

#[test]
fn cancelling_one_same_label_schedule_leaves_the_other_firing() {
    // The gap measured on 09-13: a name-keyed table means a second
    // registrant under the same label silently replaces the first. A handle
    // fixes it — two schedules can share a label and coexist, and only the
    // handle that was actually cancelled stops.
    let path = scratch("schedule-cancel");
    let _daemon = daemon_at(&path);

    eval(
        &path,
        r#"
            remuda.fired_a, remuda.fired_b = 0, 0
            remuda.handle_a = remuda.schedule({
              name = "dup",
              every = 0.01,
              run = function() remuda.fired_a = remuda.fired_a + 1 end,
            })
            remuda.handle_b = remuda.schedule({
              name = "dup",
              every = 0.01,
              run = function() remuda.fired_b = remuda.fired_b + 1 end,
            })
        "#,
    );

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return remuda.fired_a") < 2
        || read_count(&path, "return remuda.fired_b") < 2
    {
        assert!(
            Instant::now() < deadline,
            "both same-label schedules must fire independently"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    eval(&path, "remuda.cancel(remuda.handle_a)");
    let a_at_cancel = read_count(&path, "return remuda.fired_a");

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return remuda.fired_b") < a_at_cancel + 2 {
        assert!(
            Instant::now() < deadline,
            "the surviving schedule must keep firing"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        read_count(&path, "return remuda.fired_a"),
        a_at_cancel,
        "the cancelled schedule must not fire again"
    );
}

#[test]
fn one_throwing_schedule_does_not_starve_the_others() {
    // Handles are table keys, so iteration order is unknown: a throw that
    // escapes the loop skips whichever schedules happen to come after it.
    // `every` is far beyond the real ticker's clock, so only the ticks this
    // test drives by hand can fire anything.
    let path = scratch("schedule-throw");
    let _daemon = daemon_at(&path);

    eval(
        &path,
        r#"
            remuda.healthy_fired = 0
            remuda.schedule({ every = 1e9, run = function() error("boom") end })
            for _ = 1, 20 do
              remuda.schedule({
                every = 1e9,
                run = function() remuda.healthy_fired = remuda.healthy_fired + 1 end,
              })
            end
        "#,
    );

    assert_eq!(
        eval(
            &path,
            "return tostring(pcall(remuda._run_due_schedules, 2e9))"
        ),
        "true",
        "a throwing schedule must not abort the tick"
    );
    assert_eq!(
        read_count(&path, "return remuda.healthy_fired"),
        20,
        "every healthy schedule must fire despite the throwing one"
    );
}

#[test]
fn a_schedule_registered_during_a_tick_first_fires_on_the_next_tick() {
    // Adding keys to a table mid-`pairs` is undefined in Lua: new entries may
    // or may not be visited, or `next` may raise. A snapshot makes it exact.
    let path = scratch("schedule-mutate");
    let _daemon = daemon_at(&path);

    eval(
        &path,
        r#"
            remuda.late_fired = 0
            remuda.schedule({ every = 1e9, run = function()
              for _ = 1, 50 do
                remuda.schedule({
                  every = 1e9,
                  run = function() remuda.late_fired = remuda.late_fired + 1 end,
                })
              end
            end })
        "#,
    );

    assert_eq!(
        eval(
            &path,
            "return tostring(pcall(remuda._run_due_schedules, 2e9))"
        ),
        "true"
    );
    assert_eq!(
        read_count(&path, "return remuda.late_fired"),
        0,
        "schedules added during a tick must wait for the next one"
    );
    eval(&path, "remuda._run_due_schedules(4e9)");
    assert_eq!(read_count(&path, "return remuda.late_fired"), 50);
}

#[test]
fn event_counts_reads_zero_before_any_emit_and_n_after_real_fires() {
    // `remuda.event_counts()` does not exist yet — this must fail red with a
    // Lua "attempt to call a nil value" error, not a compile error.
    let path = scratch("event-counts");
    let _daemon = daemon_at(&path);

    eval(&path, r#"remuda.on("counter_test_event", function() end)"#);

    // A name `emit` has never been called with is absent, not zero — Lua's
    // `nil` is the only value that says so.
    assert_eq!(
        eval(&path, "return remuda.event_counts()['counter_test_event']"),
        "nil",
        "an event never emitted must be absent from event_counts(), not zero"
    );

    for _ in 0..3 {
        eval(&path, "remuda.emit('counter_test_event')");
    }

    assert_eq!(
        read_count(&path, "return remuda.event_counts()['counter_test_event']"),
        3,
        "event_counts() must count every real emit call"
    );
}

#[test]
fn schedule_fires_reads_zero_before_any_run_and_n_after_real_ticks() {
    // `remuda.schedule_fires()` does not exist yet — same nil-call failure
    // expected as `event_counts()` above.
    let path = scratch("schedule-fires");
    let _daemon = daemon_at(&path);

    eval(
        &path,
        r#"
            remuda.schedule({
              name = "counter_test_schedule",
              every = 1,
              run = function() end,
            })
        "#,
    );

    // Before any tick has elapsed, an unfired named schedule is absent too.
    assert_eq!(
        eval(
            &path,
            "return remuda.schedule_fires()['counter_test_schedule']"
        ),
        "nil",
        "a schedule that has never fired must be absent, not zero"
    );

    let deadline = Instant::now() + PATIENCE;
    loop {
        let fires = eval(
            &path,
            "return remuda.schedule_fires()['counter_test_schedule']",
        );
        if fires.parse::<u32>().is_ok_and(|n| n >= 3) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "counter_test_schedule did not fire at least 3 times: {fires:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // An unnamed schedule (`spec.name` left nil) must never appear as a key
    // at all — `t[nil] = x` is a Lua error, so the increment must be skipped
    // rather than crash the daemon. A side-channel counter proves it really
    // fired even though `schedule_fires()` must stay silent about it.
    eval(&path, "remuda._unnamed_fired = 0");
    eval(
        &path,
        r#"
            remuda.schedule({
              every = 1,
              run = function() remuda._unnamed_fired = remuda._unnamed_fired + 1 end,
            })
        "#,
    );

    let before_keys = eval(
        &path,
        "local n = 0 for _ in pairs(remuda.schedule_fires()) do n = n + 1 end return n",
    );

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return remuda._unnamed_fired") < 3 {
        assert!(
            Instant::now() < deadline,
            "the unnamed schedule never fired"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let after_keys = eval(
        &path,
        "local n = 0 for _ in pairs(remuda.schedule_fires()) do n = n + 1 end return n",
    );
    assert_eq!(
        before_keys, after_keys,
        "an unnamed schedule must add no key to schedule_fires(), even after firing"
    );
}

#[test]
fn mutating_the_returned_event_counts_table_does_not_change_internal_state() {
    // Both accessors must hand back a snapshot, the same guarantee
    // `remuda.emit`'s own hook snapshot already keeps for `remuda.hooks`
    // (see tools.lua) — a caller mutating what it was handed must never
    // reach back into the daemon's own counters.
    let path = scratch("counters-are-copies");
    let _daemon = daemon_at(&path);

    eval(&path, r#"remuda.on("copy_test_event", function() end)"#);
    eval(&path, "remuda.emit('copy_test_event')");
    eval(
        &path,
        "local t = remuda.event_counts() t['copy_test_event'] = 9999",
    );
    assert_eq!(
        read_count(&path, "return remuda.event_counts()['copy_test_event']"),
        1,
        "event_counts() must return a copy, not a live reference"
    );

    eval(
        &path,
        r#"
            remuda.schedule({
              name = "copy_test_schedule",
              every = 1,
              run = function() end,
            })
        "#,
    );
    let deadline = Instant::now() + PATIENCE;
    loop {
        let fires = eval(
            &path,
            "return remuda.schedule_fires()['copy_test_schedule']",
        );
        if fires.parse::<u32>().is_ok_and(|n| n >= 1) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "copy_test_schedule never fired: {fires:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let before = read_count(
        &path,
        "return remuda.schedule_fires()['copy_test_schedule']",
    );
    eval(
        &path,
        "local t = remuda.schedule_fires() t['copy_test_schedule'] = 9999",
    );
    assert_eq!(
        read_count(
            &path,
            "return remuda.schedule_fires()['copy_test_schedule']"
        ),
        before,
        "schedule_fires() must return a copy, not a live reference"
    );
}

#[test]
fn the_daemon_names_the_build_it_was_started_from() {
    let path = scratch("version");
    let _daemon = daemon_at(&path);

    match client::request(&path, &Request::Version).expect("version") {
        Response::Value(said) => assert_eq!(said, remuda_native::dist::BUILD_VERSION),
        other => panic!("unexpected: {other:?}"),
    }
}

// Unix only: the regression is SIGPIPE, and `true` is not a Windows command.
#[test]
#[cfg(unix)]
fn auto_started_daemon_survives_a_fast_process_exit() {
    let dir = scratch_dir("autostart-fast-process");

    let start = remuda_timed(
        &dir,
        &[
            "-s",
            "s",
            "-e",
            "remuda.process{argv = {'true'}, on_exit = 'fast-exit'}",
        ],
    );
    assert!(
        start.status.success(),
        "auto-started process call failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );

    // Give the daemon time to deliver the exit event before asking the same
    // auto-started instance a second question.
    std::thread::sleep(Duration::from_millis(100));
    let answer = remuda_timed(&dir, &["-s", "s", "-e", "return 1"]);
    assert!(
        answer.status.success(),
        "the daemon died after its fast child exited: {}",
        String::from_utf8_lossy(&answer.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&answer.stdout).trim(), "1");

    let _ = remuda(&dir, &["-s", "s", "stop", "-f"]);
}

/// A daemon's log is post-mortem evidence: the next auto-start must append to
/// it, not wipe it. Unix only: CLI auto-start with piped output hangs on Windows.
#[test]
#[cfg(unix)]
fn an_auto_start_appends_to_the_previous_daemons_log() {
    let dir = scratch_dir("log-append");
    let log = daemon::socket_path_in(&dir, "s").with_extension("log");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, "panic trace from the last daemon\n").unwrap();

    let out = remuda_timed(&dir, &["-s", "s", "-e", "return 1"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = remuda(&dir, &["-s", "s", "stop", "-f"]);

    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("panic trace from the last daemon"), "{text}");
    assert!(text.contains("--- remuda daemon start pid "), "{text}");
}

/// Appending must not grow forever: past ~1 MiB the old log moves to `.log.1`.
#[test]
#[cfg(unix)]
fn an_oversized_daemon_log_is_rotated_on_auto_start() {
    let dir = scratch_dir("log-rotate");
    let log = daemon::socket_path_in(&dir, "s").with_extension("log");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    let big = format!("old marker\n{}", "x".repeat(1_100_000));
    std::fs::write(&log, big).unwrap();

    let out = remuda_timed(&dir, &["-s", "s", "-e", "return 1"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = remuda(&dir, &["-s", "s", "stop", "-f"]);

    let rotated = std::fs::read_to_string(log.with_extension("log.1")).unwrap();
    assert!(rotated.starts_with("old marker"));
    let fresh = std::fs::read_to_string(&log).unwrap();
    assert!(fresh.len() < 10_000, "{} bytes", fresh.len());
    assert!(fresh.contains("--- remuda daemon start pid "), "{fresh}");
}

/// What a person types, with its own pipes and no terminal — so `restart`
/// reaches the "nothing to ask on" branch rather than blocking on a prompt.
fn remuda(dir: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(args)
        .env("REMUDA_RUNTIME_DIR", dir)
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        .output()
        .expect("run remuda")
}

/// Bounded on purpose, like `Daemon::left_on_its_own` — an unbounded wait on
/// a command that hangs is the same failure this exists to catch, fast.
fn remuda_timed(dir: &Path, args: &[&str]) -> std::process::Output {
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(args)
        .env("REMUDA_RUNTIME_DIR", dir)
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        // An auto-started daemon boots the user config; keep it off the real one.
        .env("HOME", dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn remuda");

    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{args:?} did not exit within PATIENCE"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().expect("collect output")
}

/// `restart` drives the SHIPPED BINARY, not `daemon::serve` on a thread: the
/// stop is a `process::exit`, so an in-process daemon would take the test
/// runner with it — which is also why this is the only honest way to test it.
#[test]
fn stop_stops_a_daemon_and_leaves_the_next_command_free_to_start_one() {
    let dir = scratch_dir("stop");
    let path = daemon::socket_path_in(&dir, "s");
    let mut daemon = Daemon::spawn(&dir);

    // Positive control: it is answering before we ask it to stop, so "the
    // socket is silent" below cannot pass on a daemon that never came up.
    assert!(
        String::from_utf8_lossy(&remuda(&dir, &["-s", "s", "ls"]).stdout).contains("no sessions"),
        "the daemon was not answering to begin with"
    );

    let out = remuda(&dir, &["-s", "s", "stop"]);
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(out.status.success(), "restart failed: {said}");
    assert!(said.contains("stopped the daemon"), "{said}");
    assert!(
        remuda_native::ipc::connect(&path).is_err(),
        "the daemon is still answering after restart"
    );
    assert!(daemon.left_on_its_own(), "it did not exit 0 on its own");
}

#[test]
fn stop_with_no_daemon_running_is_not_an_error() {
    let dir = scratch_dir("stop-empty");
    let out = remuda(&dir, &["-s", "s", "stop"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no daemon running"));
}

/// A live herd must not be thrown away by a command that was typed by habit.
/// With no terminal to ask on, the refusal names the flag rather than prompting
/// into a pipe that will never answer.
#[test]
fn stop_refuses_to_kill_a_live_session_without_being_told_twice() {
    let dir = scratch_dir("stop-live");
    let path = daemon::socket_path_in(&dir, "s");
    let mut daemon = Daemon::spawn(&dir);
    new_session(&path, "keeper");

    let out = remuda(&dir, &["-s", "s", "stop"]);
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        !out.status.success(),
        "it killed a live herd unasked: {said}"
    );
    assert!(
        said.contains("keeper"),
        "it did not name what it would lose: {said}"
    );
    assert!(
        remuda_native::ipc::connect(&path).is_ok(),
        "the daemon died despite refusing"
    );

    // -f is the way past it, and the same daemon now goes.
    let forced = remuda(&dir, &["-s", "s", "stop", "-f"]);
    assert!(
        forced.status.success(),
        "{}",
        String::from_utf8_lossy(&forced.stderr)
    );
    assert!(daemon.left_on_its_own(), "-f did not stop it");
}

/// A name with no matching arm is a plain error naming the package, not a
/// panic or a silent no-op. Daemon pre-started for the same reason as above
/// (auto-start on Windows not covered here; see warmblood-kr/remuda#54).
#[test]
fn exec_of_an_unknown_package_fails_and_names_it() {
    let dir = scratch_dir("exec-unknown");
    let _daemon = Daemon::spawn(&dir);

    let out = remuda_timed(&dir, &["-s", "s", "exec", "definitely-not-a-real-package"]);
    assert!(
        !out.status.success(),
        "an unknown package should not succeed"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no such package"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Lua's long-bracket string form has no escape processing at all — safe
/// for embedding a raw filesystem path (backslashes included) into eval
/// source text without escaping it first.
fn lua_raw_string(s: &str) -> String {
    format!("[[{s}]]")
}

#[test]
fn process_delivers_stdout_lines_in_order_then_an_exit_event() {
    let dir = scratch_dir("process-lines");
    let _daemon = Daemon::spawn(&dir);
    let path = daemon::socket_path_in(&dir, "s");
    let exe = lua_raw_string(env!("CARGO_BIN_EXE_remuda"));

    eval(&path, "remuda.t1_lines = {}");
    eval(&path, "remuda.t1_exit = nil");
    eval(
        &path,
        "remuda.on('t1-line', function(l) table.insert(remuda.t1_lines, l) end)",
    );
    eval(
        &path,
        "remuda.on('t1-exit', function(c) remuda.t1_exit = c end)",
    );
    eval(
        &path,
        &format!(
            "remuda.process{{argv = {{{exe}, '_print_lines', '5', '0'}}, on_line = 't1-line', on_exit = 't1-exit'}}"
        ),
    );

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return remuda.t1_exit and 1 or 0") == 0 {
        assert!(Instant::now() < deadline, "exit event never arrived");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        eval(&path, "return table.concat(remuda.t1_lines, ',')"),
        "1,2,3,4,5"
    );
}

#[test]
fn a_silent_process_yields_only_the_exit_event() {
    let dir = scratch_dir("process-silent");
    let _daemon = Daemon::spawn(&dir);
    let path = daemon::socket_path_in(&dir, "s");
    let exe = lua_raw_string(env!("CARGO_BIN_EXE_remuda"));

    eval(&path, "remuda.silent_lines = 0");
    eval(&path, "remuda.silent_exit = nil");
    eval(
        &path,
        "remuda.on('silent-line', function() remuda.silent_lines = remuda.silent_lines + 1 end)",
    );
    eval(
        &path,
        "remuda.on('silent-exit', function() remuda.silent_exit = true end)",
    );
    eval(
        &path,
        &format!(
            "remuda.process{{argv = {{{exe}, '_print_lines', '0', '0'}}, on_line = 'silent-line', on_exit = 'silent-exit'}}"
        ),
    );

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return remuda.silent_exit and 1 or 0") == 0 {
        assert!(Instant::now() < deadline, "exit event never arrived");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(read_count(&path, "return remuda.silent_lines"), 0);
}

/// A pid this daemon spawned is gone: `kill(pid, 0)` — no signal delivered,
/// only whether one *could* be — is ESRCH once the pid is reaped. Anything
/// else (success, or a permission error) means it is still around.
#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Find a pid's own child, by exact pid, one time — never a glob/grep
/// pattern (this investigation's own history: zsh's globber has produced a
/// false "no matches" from `ps -ef | grep [r]emuda` more than once).
#[cfg(unix)]
fn child_pid_of(parent: i32, deadline: Instant) -> Option<i32> {
    loop {
        // `pgrep -P`, not `ps --ppid`: the latter is GNU-only.
        let out = std::process::Command::new("pgrep")
            .args(["-P", &parent.to_string()])
            .output()
            .expect("ps");
        let text = String::from_utf8_lossy(&out.stdout);
        if let Some(pid) = text.split_whitespace().next().and_then(|s| s.parse().ok()) {
            return Some(pid);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// [MEASURED, Linux] This is the actual leak (`process.rs`'s plain-pipe
/// children, "ALWAYS survive daemon death... zero process-group isolation,
/// zero signal handling anywhere") and this is `child_guard::harden`'s fix
/// for it: a real out-of-process daemon, SIGKILLed for real, and its DIRECT
/// `remuda.process` child is gone within PATIENCE — not by any daemon code
/// running (none does, on SIGKILL), but because the kernel itself delivers
/// PDEATHSIG the moment the daemon dies.
///
/// The GRANDCHILD (`sleep`, forked by the `sh` direct child before the kill)
/// is asserted to *survive* the same SIGKILL, on purpose: PDEATHSIG is
/// registered on the direct child alone and is cleared across fork(2), so it
/// cannot reach a process it never touched, and nothing else runs on a raw
/// SIGKILL to sweep the group. This pins the exact boundary named in
/// child_guard.rs's own doc comment and in steps/ "Known ceilings", rather
/// than asserting past it. See
/// `a_clean_shutdown_reaps_a_processs_whole_group_including_a_grandchild`
/// for the one path that DOES reach this grandchild.
///
/// Negative control, run by hand for this round (see steps/ writeup): with
/// `child_guard::harden`'s call in `process.rs::spawn` commented out, this
/// test's first `while pid_alive(direct_pid)` loop times out — nothing tells
/// the kernel to kill the direct child when the daemon dies. Restoring the
/// call makes it pass again.
#[cfg(target_os = "linux")]
#[test]
fn a_sigkilled_daemon_reaps_its_direct_process_child_but_not_an_already_forked_grandchild() {
    let dir = scratch_dir("orphan-reap");
    let daemon = Daemon::spawn(&dir);
    let path = daemon::socket_path_in(&dir, "s");

    eval(&path, "remuda.og_lines = {}");
    eval(
        &path,
        "remuda.on('og-line', function(l) table.insert(remuda.og_lines, l) end)",
    );
    eval(
        &path,
        "remuda.process{argv = {'sh', '-c', 'echo $$; sleep 60'}, on_line = 'og-line'}",
    );

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return #remuda.og_lines") == 0 {
        assert!(
            Instant::now() < deadline,
            "the direct child never printed its own pid"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let direct_pid: i32 = eval(&path, "return remuda.og_lines[1]")
        .trim()
        .parse()
        .expect("the printed $$ is a pid");

    let deadline = Instant::now() + PATIENCE;
    let grandchild_pid = child_pid_of(direct_pid, deadline)
        .unwrap_or_else(|| panic!("sleep never forked as a child of {direct_pid}"));
    assert!(
        pid_alive(direct_pid),
        "sanity: direct child must be alive before the kill"
    );
    assert!(
        pid_alive(grandchild_pid),
        "sanity: grandchild must be alive before the kill"
    );

    let daemon_pid = daemon.0.id() as libc::pid_t;
    assert_eq!(
        unsafe { libc::kill(daemon_pid, libc::SIGKILL) },
        0,
        "SIGKILL of the real daemon pid must succeed"
    );

    let deadline = Instant::now() + PATIENCE;
    while pid_alive(direct_pid) {
        assert!(
            Instant::now() < deadline,
            "the direct child ({direct_pid}) outlived a SIGKILLed daemon — child_guard::harden \
             did not reap it"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // Pin the ceiling: give the kernel a beat, then confirm the grandchild
    // is still here — a raw SIGKILL of the daemon runs no code, so nothing
    // built this round could have reached it.
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        pid_alive(grandchild_pid),
        "a grandchild dying too would mean either this test's premise changed or something \
         now reaps process groups on a signal this round never wired that to — worth knowing, \
         not assuming"
    );

    // Clean up the leak this test just proved exists, so it doesn't actually
    // linger on the machine that ran it.
    unsafe {
        libc::kill(grandchild_pid, libc::SIGKILL);
    }
}

/// [MEASURED, unix] The one path that DOES reach a grandchild: a clean
/// `remuda stop` sends `Request::Shutdown`, which runs
/// `reap_processes_before_exit` (daemon.rs) before the process exits —
/// `remuda.processes()` + `remuda._process_killpg(id)`, which `killpg`s the
/// whole process group `child_guard::harden` put the direct child in. Both
/// the direct child and the grandchild it already forked are gone.
#[cfg(unix)]
#[test]
fn a_clean_shutdown_reaps_a_processs_whole_group_including_a_grandchild() {
    let dir = scratch_dir("orphan-reap-clean");
    let _daemon = Daemon::spawn(&dir);
    let path = daemon::socket_path_in(&dir, "s");

    eval(&path, "remuda.cg_lines = {}");
    eval(
        &path,
        "remuda.on('cg-line', function(l) table.insert(remuda.cg_lines, l) end)",
    );
    eval(
        &path,
        "remuda.process{argv = {'sh', '-c', 'echo $$; sleep 60'}, on_line = 'cg-line'}",
    );

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return #remuda.cg_lines") == 0 {
        assert!(
            Instant::now() < deadline,
            "the direct child never printed its own pid"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let direct_pid: i32 = eval(&path, "return remuda.cg_lines[1]")
        .trim()
        .parse()
        .expect("the printed $$ is a pid");

    let deadline = Instant::now() + PATIENCE;
    let grandchild_pid = child_pid_of(direct_pid, deadline)
        .unwrap_or_else(|| panic!("sleep never forked as a child of {direct_pid}"));
    assert!(
        pid_alive(direct_pid) && pid_alive(grandchild_pid),
        "sanity: both alive pre-restart"
    );

    // No live pty session was ever created here, so `remuda stop` (no
    // `-f`) proceeds straight to `Request::Shutdown` without a confirmation
    // prompt — see `confirm_losses` in src/bin/remuda.rs.
    let out = remuda(&dir, &["-s", "s", "stop"]);
    assert!(
        out.status.success(),
        "restart failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let deadline = Instant::now() + PATIENCE;
    while pid_alive(direct_pid) || pid_alive(grandchild_pid) {
        assert!(
            Instant::now() < deadline,
            "a clean shutdown left something behind: direct {direct_pid} alive={}, grandchild \
             {grandchild_pid} alive={}",
            pid_alive(direct_pid),
            pid_alive(grandchild_pid)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_line_flood_does_not_starve_the_schedule_ticker() {
    let dir = scratch_dir("process-flood");
    let _daemon = Daemon::spawn(&dir);
    let path = daemon::socket_path_in(&dir, "s");
    let exe = lua_raw_string(env!("CARGO_BIN_EXE_remuda"));
    const FLOOD_LINES: u32 = 3000;
    // A per-line delay, not a bare line count: `daemon.rs`'s own `TICK_PERIOD`
    // is a fixed 1 real second (see its doc comment), independent of the
    // Lua schedule's own `every`, so the ticker's very first wakeup cannot
    // come sooner than that regardless of how this test paces its flood. A
    // delay-free flood of any size a modern machine can push drains in well
    // under one second — measured at ~0.2s for 100k lines — which starves
    // the observation window, not the ticker: there is no failure to see if
    // the flood is already over before the first tick could possibly fire.
    // Pacing by wall-clock sleep (not raw throughput) makes the minimum
    // flood duration (here, >= 6s) independent of the machine's speed.
    const FLOOD_LINE_DELAY_MS: u32 = 2;
    let flood_deadline = Instant::now() + Duration::from_secs(60);

    eval(&path, "remuda.flood_count = 0");
    eval(
        &path,
        "remuda.on('flood-line', function() remuda.flood_count = remuda.flood_count + 1 end)",
    );
    eval(&path, "remuda.ticks = 0");
    eval(
        &path,
        "remuda.schedule{every = 0.05, run = function() remuda.ticks = remuda.ticks + 1 end}",
    );
    eval(
        &path,
        &format!(
            "remuda.process{{argv = {{{exe}, '_print_lines', '{FLOOD_LINES}', '{FLOOD_LINE_DELAY_MS}'}}, on_line = 'flood-line'}}"
        ),
    );

    let mut last_ticks = read_count(&path, "return remuda.ticks");
    let mut ticker_advanced_during_flood = false;
    loop {
        let count = read_count(&path, "return remuda.flood_count");
        if count >= FLOOD_LINES {
            break;
        }
        assert!(
            Instant::now() < flood_deadline,
            "flood never finished ({count}/{FLOOD_LINES})"
        );
        std::thread::sleep(Duration::from_millis(100));
        let ticks = read_count(&path, "return remuda.ticks");
        if ticks > last_ticks {
            ticker_advanced_during_flood = true;
        }
        last_ticks = ticks;
    }
    assert!(
        ticker_advanced_during_flood,
        "the schedule ticker never advanced during the flood"
    );
    assert_eq!(read_count(&path, "return remuda.flood_count"), FLOOD_LINES);

    // A named, generous bound — this asserts "not starved," not a specific
    // performance target.
    let consecutive_skips = read_count(&path, "return remuda.schedule_skips().consecutive");
    assert!(
        consecutive_skips < 20,
        "consecutive schedule skips too high under flood: {consecutive_skips}"
    );
}

#[test]
fn killing_a_process_mid_stream_yields_exit_and_nothing_after() {
    let dir = scratch_dir("process-kill");
    let _daemon = Daemon::spawn(&dir);
    let path = daemon::socket_path_in(&dir, "s");
    let exe = lua_raw_string(env!("CARGO_BIN_EXE_remuda"));

    eval(&path, "remuda.km_lines = 0");
    eval(&path, "remuda.km_exit = nil");
    eval(
        &path,
        "remuda.on('km-line', function() remuda.km_lines = remuda.km_lines + 1 end)",
    );
    eval(
        &path,
        "remuda.on('km-exit', function() remuda.km_exit = true end)",
    );
    // A slow trickle so there is a real window to kill it mid-stream rather
    // than racing its own natural exit.
    eval(
        &path,
        &format!(
            "remuda.km_handle = remuda.process{{argv = {{{exe}, '_print_lines', '100000', '10'}}, on_line = 'km-line', on_exit = 'km-exit'}}"
        ),
    );

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return remuda.km_lines") < 3 {
        assert!(
            Instant::now() < deadline,
            "process never started producing lines"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    eval(&path, "remuda.kill(remuda.km_handle)");

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return remuda.km_exit and 1 or 0") == 0 {
        assert!(
            Instant::now() < deadline,
            "exit event never arrived after kill"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let lines_at_exit = read_count(&path, "return remuda.km_lines");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        read_count(&path, "return remuda.km_lines"),
        lines_at_exit,
        "a line arrived after the exit event"
    );
}

/// The paced flood above (`a_line_flood_does_not_starve_the_schedule_ticker`)
/// is deliberately slow enough that `process.rs`'s 4096-line `BUFFER_CAP`
/// never fills, so backpressure itself is never exercised there. This test
/// writes as fast as `_print_lines` can (delay 0) and gives it 5x
/// `BUFFER_CAP` worth of lines, so the buffer must fill — and proves it did,
/// three independent ways:
///
/// 1. The child is *still running* (`remuda.processes()` still lists its id)
///    well after an unblocked 20000-tiny-line write would already have
///    exited on its own — it can only still be alive because its own
///    `write()` is blocked on a full pipe.
/// 2. Total wall-clock time to drain is far longer than an unpaced write of
///    20000 short lines takes with no consumer at all (well under 100ms,
///    unmeasured here, but self-evidently near-instant) — the elapsed time
///    is explainable only by the child being made to wait.
/// 3. The schedule ticker keeps advancing throughout, so none of the above
///    comes at the cost of starving the Image's own FIFO — the property the
///    paced flood test already covers, still held even under real pressure.
///
/// The `on_line` hook itself is a busy loop, not a sleep — Lua has no
/// builtin sleep, and this is simplest thing that reliably costs enough
/// real wall time per line to keep the buffer pinned near `BUFFER_CAP` for
/// long enough to observe, rather than draining in a blink.
#[test]
fn an_unpaced_flood_exercises_real_backpressure_and_the_child_blocks() {
    let dir = scratch_dir("process-unpaced-flood");
    let _daemon = Daemon::spawn(&dir);
    let path = daemon::socket_path_in(&dir, "s");
    let exe = lua_raw_string(env!("CARGO_BIN_EXE_remuda"));
    const FLOOD_LINES: u32 = 20_000;

    eval(&path, "remuda.up_count = 0");
    eval(&path, "remuda.up_last = 0");
    eval(&path, "remuda.up_broken = false");
    eval(&path, "remuda.up_exit = nil");
    eval(
        &path,
        // Ordering is checked inline, one line at a time, rather than
        // buffered into a 20000-entry table and checked after: `up_broken`
        // catches any gap or repeat the instant it happens, and `up_last`
        // at the end is proof every line 1..N arrived, not a sample of them.
        "remuda.on('up-line', function(l)
            local n = tonumber(l)
            if n ~= remuda.up_last + 1 then remuda.up_broken = true end
            remuda.up_last = n
            local busy = 0
            for i = 1, 10000 do busy = busy + i end
            remuda.up_count = remuda.up_count + 1
        end)",
    );
    eval(
        &path,
        "remuda.on('up-exit', function() remuda.up_exit = true end)",
    );
    eval(&path, "remuda.up_ticks = 0");
    eval(
        &path,
        "remuda.schedule{every = 0.05, run = function() remuda.up_ticks = remuda.up_ticks + 1 end}",
    );

    let spawned_at = Instant::now();
    eval(
        &path,
        &format!(
            "remuda.up_handle = remuda.process{{argv = {{{exe}, '_print_lines', '{FLOOD_LINES}', '0'}}, on_line = 'up-line', on_exit = 'up-exit'}}"
        ),
    );

    // Proof #1: still alive well after an unblocked flood of this size would
    // have finished. `BUFFER_CAP` (4096) plus a typical OS pipe (tens of
    // thousands of bytes, a few thousand lines of this size) is nowhere near
    // 20000 lines, so a child that is still running here is blocked on a
    // full pipe, not merely "still working".
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        read_count(
            &path,
            "return (function() \
                for _, id in ipairs(remuda.processes()) do \
                    if id == remuda.up_handle then return 1 end \
                end \
                return 0 \
             end)()"
        ),
        1,
        "the flooding process had already exited after 300ms — \
         backpressure did not hold it, so the buffer/pipe never filled"
    );

    let flood_deadline = Instant::now() + Duration::from_secs(60);
    let mut last_ticks = read_count(&path, "return remuda.up_ticks");
    let mut ticker_advanced_during_flood = false;
    loop {
        let count = read_count(&path, "return remuda.up_count");
        if count >= FLOOD_LINES {
            break;
        }
        assert!(
            Instant::now() < flood_deadline,
            "flood never finished ({count}/{FLOOD_LINES})"
        );
        std::thread::sleep(Duration::from_millis(50));
        let ticks = read_count(&path, "return remuda.up_ticks");
        if ticks > last_ticks {
            ticker_advanced_during_flood = true;
        }
        last_ticks = ticks;
    }

    // Proof #2: it took distinctly longer than an unpaced, unblocked write
    // of 20000 short lines could possibly take on its own. Generous enough
    // to survive a slow CI runner (including Windows), tight enough that
    // "no backpressure" (near-instant) cannot pass it by accident.
    let elapsed = spawned_at.elapsed();
    assert!(
        elapsed > Duration::from_millis(500),
        "the flood drained in {elapsed:?} — too fast to have been backpressured"
    );

    assert!(
        ticker_advanced_during_flood,
        "the schedule ticker never advanced during the unpaced flood"
    );

    assert_eq!(
        read_count(&path, "return remuda.up_broken and 1 or 0"),
        0,
        "a line arrived out of order or was skipped"
    );
    assert_eq!(
        read_count(&path, "return remuda.up_last"),
        FLOOD_LINES,
        "the last line received was not the expected final line"
    );

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return remuda.up_exit and 1 or 0") == 0 {
        assert!(Instant::now() < deadline, "exit event never arrived");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The whole point of steps/035: a `~/.config/remuda/init.lua` present before
/// the daemon exists is evaluated automatically, with no `exec`/`eval` from
/// this test, every time a FRESH daemon boots. The instrument is a marker the
/// file itself writes, and the restart leg proves the load runs on every boot
/// from the same unchanged file, not just the first.
#[test]
#[cfg(unix)]
fn a_fresh_daemon_auto_loads_the_user_config_on_every_boot() {
    let dir = scratch_dir("boot-loader-positive");
    let home = dir.join("home");
    let marker = dir.join("init-ran.marker");
    let _ = std::fs::remove_file(&marker);
    std::fs::create_dir_all(home.join(".config/remuda")).expect("mkdir config dir");
    std::fs::write(
        home.join(".config/remuda/init.lua"),
        format!(
            "local f = io.open({:?}, 'w') f:write('ran') f:close()",
            marker.display().to_string()
        ),
    )
    .expect("write init.lua");

    let wait_for_marker = |leg: &str| {
        let deadline = Instant::now() + PATIENCE;
        while !marker.exists() {
            assert!(
                Instant::now() < deadline,
                "{leg}: the daemon never evaluated its user config at boot"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    let mut daemon = Daemon::spawn_with_home(&dir, &home);
    wait_for_marker("first boot");

    let out = remuda(&dir, &["-s", "s", "stop", "-f"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        daemon.left_on_its_own(),
        "the old daemon did not exit on its own"
    );

    // Same home, config untouched; only the evidence of the first run goes.
    std::fs::remove_file(&marker).expect("remove first-boot marker");
    let _daemon2 = Daemon::spawn_with_home(&dir, &home);
    wait_for_marker("after restart");
}

/// Negative control for the whole mechanism above (steps/035's own DoD
/// wording: "with the feature turned off, the same procedure must go RED —
/// if it does not go red, the instrument is not measuring the feature").
/// Absence of `~/.config/remuda/init.lua` IS "feature off" here, the same
/// way a vanilla Neovim/Hammerspoon install with no config is: other files
/// may sit under `~/.config/remuda` (here, butler credentials), but with no
/// init.lua a fresh daemon must never start anything on its own.
#[test]
#[cfg(unix)]
fn a_fresh_daemon_with_no_user_config_never_auto_starts_a_session() {
    let dir = scratch_dir("boot-loader-negative");
    let home = dir.join("home-empty");
    let butler_dir = home.join(".config/remuda/butler");
    std::fs::create_dir_all(&butler_dir).expect("mkdir conventional butler dir");
    std::fs::write(butler_dir.join("token"), "test-token\n").expect("write token");
    std::fs::write(
        butler_dir.join("config"),
        "http://127.0.0.1:1\n!room:example.org\n@butler:example.org\n\n",
    )
    .expect("write config");
    // Deliberately: no `~/.config/remuda/init.lua` written at all.

    let _daemon = Daemon::spawn_with_home(&dir, &home);

    // A couple of TICK_PERIODs' worth of margin to rule out a delayed load,
    // not just an instant-after check.
    std::thread::sleep(Duration::from_millis(1200));
    let listed = remuda(&dir, &["-s", "s", "ls"]);
    let listed_out = String::from_utf8_lossy(&listed.stdout);
    assert!(
        listed_out.contains("no sessions"),
        "a fresh daemon with no init.lua at all auto-registered something: {listed_out}"
    );
}

/// Regression test against the exact hazard `load_user_config`'s own doc
/// comment (daemon.rs) names: because `Image::spawn`'s `ready` chain (image.rs)
/// is checked on EVERY job forever, an `Err` inside it poisons the image
/// PERMANENTLY -- moving the user-config load back inside that chain (as
/// opposed to the separate, later `image.eval` call it uses today) would let
/// one colleague's own typo in their `~/.config/remuda/init.lua` brick every
/// other session on their daemon, forever, until a manual restart. A broken
/// file must be reported and then the daemon must go on being an entirely
/// ordinary, working daemon.
#[test]
#[cfg(unix)]
fn a_broken_user_config_is_reported_but_never_bricks_the_daemon() {
    let dir = scratch_dir("boot-loader-blast-radius");
    let home = dir.join("home");
    std::fs::create_dir_all(home.join(".config/remuda")).expect("mkdir config dir");
    std::fs::write(
        home.join(".config/remuda/init.lua"),
        "this is not valid lua $$$\n",
    )
    .expect("write broken init.lua");

    let _daemon = Daemon::spawn_with_home(&dir, &home);
    let path = daemon::socket_path_in(&dir, "s");

    // The daemon is still a normal, working daemon: `ls` answers cleanly --
    // if the hazard above ever regressed, this would instead come back as
    // an error containing "image failed to start".
    match client::request(&path, &Request::List).expect("list") {
        Response::Sessions(s) => assert!(s.is_empty(), "a fresh daemon holds nothing"),
        other => panic!("unexpected: {other:?}"),
    }

    // Direct proof the image itself is not poisoned, not merely that `ls`
    // (which never touches the `ready` chain either) happens to still work:
    // an entirely unrelated, ordinary session can still be created.
    new_session(&path, "plain");
    let listed = remuda(&dir, &["-s", "s", "ls"]);
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("plain"),
        "an ordinary session could not be created after a broken user config \
         -- the image is poisoned"
    );
}
