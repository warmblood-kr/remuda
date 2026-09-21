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
use remuda_native::{client, daemon, CommandBuilder, PtyAgent, SystemClock};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(10);

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
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_remuda"));
    cmd.arg("attach");
    cmd.arg("target");
    cmd.env("REMUDA_RUNTIME_DIR", &dir);
    let viewer = Session::new(
        "viewer",
        Box::new(PtyAgent::spawn(cmd, Size::new(80, 24)).expect("spawn viewer")),
        Arc::new(SystemClock::new()),
    );

    // 1. Repaint: what was already there arrives without the program redrawing.
    let deadline = Instant::now() + PATIENCE;
    loop {
        let screen = viewer.screen_text().expect("viewer screen");
        if screen.contains("121-before") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "attach did not repaint the existing screen. viewer saw:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // 2. Keystrokes reach the far session, and its output comes back.
    let held = viewer.attach().expect("drive the viewer");
    held.write_raw(b"echo $((6*7))-typed\r").expect("type");
    wait_for(&path, "target", "42-typed");

    // 3. Ctrl-\ detaches. The proof is on the far side: the core is refused
    //    while attached and accepted afterwards, so this cannot pass by the
    //    client merely exiting for some other reason.
    assert!(
        matches!(
            client::request(
                &path,
                &Request::SendLine {
                    name: "target".into(),
                    text: "echo LEAKED".into()
                }
            ),
            Ok(Response::Error(_))
        ),
        "while a human holds it, the core must be refused"
    );

    held.write_raw(&[client::DETACH]).expect("Ctrl-\\");
    drop(held);

    let deadline = Instant::now() + PATIENCE;
    loop {
        let resumed = client::request(
            &path,
            &Request::SendLine {
                name: "target".into(),
                text: "echo $((9*9))-after".into(),
            },
        );
        if matches!(resumed, Ok(Response::Ok)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the core never regained the session after detach: {resumed:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let screen = wait_for(&path, "target", "81-after");
    assert!(
        !screen.contains("LEAKED"),
        "a refused instruction reached the process anyway:\n{screen}"
    );
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
        Response::Value(said) => assert_eq!(said, remuda_native::dist::VERSION),
        other => panic!("unexpected: {other:?}"),
    }
}

/// A daemon as its own PROCESS, with its streams pointed at nothing. Inheriting
/// the harness's stdout would let a leaked daemon hold cargo's pipe open, which
/// turns any failure below into a hung job instead of a red one.
struct Daemon(std::process::Child);

impl Daemon {
    fn spawn(dir: &Path) -> Self {
        let child = std::process::Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", "s", "daemon"])
            .env("REMUDA_RUNTIME_DIR", dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn daemon");
        let path = daemon::socket_path_in(dir, "s");
        let deadline = Instant::now() + PATIENCE;
        while remuda_native::ipc::connect(&path).is_err() {
            assert!(Instant::now() < deadline, "daemon never bound {path:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
        Self(child)
    }

    /// Like `spawn`, but pins the daemon process's own `PWD` -- `None` unsets
    /// it entirely, rather than leaving whatever the test runner happened to
    /// have, so a directory-derived name test is not at the mercy of `cargo
    /// test`'s own working directory.
    fn spawn_with_pwd(dir: &Path, pwd: Option<&str>) -> Self {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_remuda"));
        cmd.args(["-s", "s", "daemon"])
            .env("REMUDA_RUNTIME_DIR", dir);
        match pwd {
            Some(p) => cmd.env("PWD", p),
            None => cmd.env_remove("PWD"),
        };
        let child = cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn daemon");
        let path = daemon::socket_path_in(dir, "s");
        let deadline = Instant::now() + PATIENCE;
        while remuda_native::ipc::connect(&path).is_err() {
            assert!(Instant::now() < deadline, "daemon never bound {path:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
        Self(child)
    }

    /// Like `spawn`, but layers extra environment variables onto the daemon
    /// process itself -- not the CLI client that later asks it to `exec`
    /// something. `os.getenv` inside `init.lua` reads the daemon's own
    /// environment, so a real (non-`_butler_test_mode`) run needs
    /// `REMUDA_BUTLER_TOKEN`/`REMUDA_BUTLER_CONFIG` set here, additive to
    /// `spawn`'s existing behavior.
    fn spawn_with_env(dir: &Path, extra_env: &[(&str, &str)]) -> Self {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_remuda"));
        cmd.args(["-s", "s", "daemon"])
            .env("REMUDA_RUNTIME_DIR", dir);
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let child = cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn daemon");
        let path = daemon::socket_path_in(dir, "s");
        let deadline = Instant::now() + PATIENCE;
        while remuda_native::ipc::connect(&path).is_err() {
            assert!(Instant::now() < deadline, "daemon never bound {path:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
        Self(child)
    }

    /// Like `spawn_with_env`, but for the birth-environment-poisoning
    /// scenario itself: a daemon born with `HOME` pinned to a scratch
    /// directory and NEITHER `REMUDA_BUTLER_TOKEN`/`REMUDA_BUTLER_CONFIG`
    /// nor `XDG_CONFIG_HOME` present at all (`env_remove`, not merely
    /// unset-by-omission -- `cargo test`'s own process could otherwise leak
    /// either through, making the test non-deterministic on a machine where
    /// they happen to be set).
    fn spawn_with_home(dir: &Path, home: &Path) -> Self {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_remuda"));
        cmd.args(["-s", "s", "daemon"])
            .env("REMUDA_RUNTIME_DIR", dir)
            .env("HOME", home)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("REMUDA_BUTLER_TOKEN")
            .env_remove("REMUDA_BUTLER_CONFIG");
        let child = cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn daemon");
        let path = daemon::socket_path_in(dir, "s");
        let deadline = Instant::now() + PATIENCE;
        while remuda_native::ipc::connect(&path).is_err() {
            assert!(Instant::now() < deadline, "daemon never bound {path:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
        Self(child)
    }

    /// Bounded on purpose: an unbounded `wait` on a daemon that did not stop is
    /// the same hang this whole struct exists to avoid.
    fn left_on_its_own(&mut self) -> bool {
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.0.try_wait() {
                return status.success();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
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
fn restart_stops_a_daemon_and_leaves_the_next_command_free_to_start_one() {
    let dir = scratch_dir("restart");
    let path = daemon::socket_path_in(&dir, "s");
    let mut daemon = Daemon::spawn(&dir);

    // Positive control: it is answering before we ask it to stop, so "the
    // socket is silent" below cannot pass on a daemon that never came up.
    assert!(
        String::from_utf8_lossy(&remuda(&dir, &["-s", "s", "ls"]).stdout).contains("no sessions"),
        "the daemon was not answering to begin with"
    );

    let out = remuda(&dir, &["-s", "s", "restart"]);
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
fn restart_with_no_daemon_running_is_not_an_error() {
    let dir = scratch_dir("restart-empty");
    let out = remuda(&dir, &["-s", "s", "restart"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no daemon running"));
}

/// A live herd must not be thrown away by a command that was typed by habit.
/// With no terminal to ask on, the refusal names the flag rather than prompting
/// into a pipe that will never answer.
#[test]
fn restart_refuses_to_kill_a_live_session_without_being_told_twice() {
    let dir = scratch_dir("restart-live");
    let path = daemon::socket_path_in(&dir, "s");
    let mut daemon = Daemon::spawn(&dir);
    new_session(&path, "keeper");

    let out = remuda(&dir, &["-s", "s", "restart"]);
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
    let forced = remuda(&dir, &["-s", "s", "restart", "-f"]);
    assert!(
        forced.status.success(),
        "{}",
        String::from_utf8_lossy(&forced.stderr)
    );
    assert!(daemon.left_on_its_own(), "-f did not stop it");
}

/// `exec` runs a built-in package's entry file in the daemon's own image —
/// not a fresh interpreter — so a global it sets is visible to a later `-e`
/// against the same daemon. The daemon is pre-started here (rather than
/// relying on `with_daemon`'s auto-start) to avoid its piped stderr, which a
/// leaked auto-started daemon can inherit on Windows. That means `remuda
/// exec`'s real-life auto-start path — invoked from a script with no daemon
/// already running — is deliberately NOT covered by this test on Windows;
/// see warmblood-kr/remuda#54.
///
/// `packages/butler/init.lua` now needs real Matrix config to run past this
/// point (`REMUDA_BUTLER_TOKEN`/`REMUDA_BUTLER_CONFIG`), which this test
/// deliberately never provides — `remuda._butler_test_mode` is exactly the
/// escape hatch it exposes for that (see `native/tests/daemon.rs`'s own
/// `butler_test_daemon` and the tests around it for the full package), so
/// this test asserts only the general "same living image" property, via the
/// two globals every mode of `init.lua` sets before that point.
#[test]
fn exec_butler_runs_the_builtin_package_in_the_daemons_image() {
    let dir = scratch_dir("exec-butler");
    let _daemon = Daemon::spawn(&dir);

    let mode = remuda_timed(&dir, &["-s", "s", "-e", "remuda._butler_test_mode = true"]);
    assert!(
        mode.status.success(),
        "{}",
        String::from_utf8_lossy(&mode.stderr)
    );

    let out = remuda_timed(&dir, &["-s", "s", "exec", "butler"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let read = remuda_timed(
        &dir,
        &[
            "-s",
            "s",
            "-e",
            "return (remuda._butler_helper_src ~= nil and remuda._butler_reply_src ~= nil) \
             and 'ok' or 'missing'",
        ],
    );
    assert!(
        read.status.success(),
        "{}",
        String::from_utf8_lossy(&read.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&read.stdout).trim(),
        "ok",
        "the butler package did not set its embedded-source globals in this image"
    );
}

/// `remuda._butler_initial_name` is set before the test-mode return (see
/// `init.lua`), so this reaches real code without needing the live `claude`
/// launch that `remuda._butler_test_mode` exists to avoid.
#[test]
fn butler_initial_name_is_the_launch_directorys_basename() {
    let dir = scratch_dir("butler-name-basename");
    let _daemon = Daemon::spawn_with_pwd(&dir, Some("/home/x/my-project/"));

    let mode = remuda_timed(&dir, &["-s", "s", "-e", "remuda._butler_test_mode = true"]);
    assert!(
        mode.status.success(),
        "{}",
        String::from_utf8_lossy(&mode.stderr)
    );

    let out = remuda_timed(&dir, &["-s", "s", "exec", "butler"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let name = remuda_timed(
        &dir,
        &["-s", "s", "-e", "return remuda._butler_initial_name"],
    );
    assert!(
        name.status.success(),
        "{}",
        String::from_utf8_lossy(&name.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&name.stdout).trim(), "my-project");
}

/// Negative control for the same code path: no `PWD` at all (never set for
/// a daemon started outside an interactive shell) must not panic or produce
/// an empty name -- it must fall back to the literal "butler".
#[test]
fn butler_initial_name_falls_back_to_butler_without_a_pwd() {
    let dir = scratch_dir("butler-name-fallback");
    let _daemon = Daemon::spawn_with_pwd(&dir, None);

    let mode = remuda_timed(&dir, &["-s", "s", "-e", "remuda._butler_test_mode = true"]);
    assert!(
        mode.status.success(),
        "{}",
        String::from_utf8_lossy(&mode.stderr)
    );

    let out = remuda_timed(&dir, &["-s", "s", "exec", "butler"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let name = remuda_timed(
        &dir,
        &["-s", "s", "-e", "return remuda._butler_initial_name"],
    );
    assert!(
        name.status.success(),
        "{}",
        String::from_utf8_lossy(&name.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&name.stdout).trim(), "butler");
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
#[cfg(target_os = "linux")]
fn pid_alive(pid: i32) -> bool {
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Find a pid's own child, by exact pid, one time — never a glob/grep
/// pattern (this investigation's own history: zsh's globber has produced a
/// false "no matches" from `ps -ef | grep [r]emuda` more than once).
#[cfg(target_os = "linux")]
fn child_pid_of(parent: i32, deadline: Instant) -> Option<i32> {
    loop {
        let out = std::process::Command::new("ps")
            .args(["-o", "pid=", "--ppid", &parent.to_string()])
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

/// [MEASURED, Linux] The one path that DOES reach a grandchild: a clean
/// `remuda restart` sends `Request::Shutdown`, which runs
/// `reap_processes_before_exit` (daemon.rs) before the process exits —
/// `remuda.processes()` + `remuda._process_killpg(id)`, which `killpg`s the
/// whole process group `child_guard::harden` put the direct child in. Both
/// the direct child and the grandchild it already forked are gone.
#[cfg(target_os = "linux")]
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

    // No live pty session was ever created here, so `remuda restart` (no
    // `-f`) proceeds straight to `Request::Shutdown` without a confirmation
    // prompt — see `confirm_losses` in src/bin/remuda.rs.
    let out = remuda(&dir, &["-s", "s", "restart"]);
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

// --- packages/butler: the Matrix bridge's Python/bash helpers -------------
//
// Everything below runs against a stub HTTP server of our own
// (`tests/support/matrix_stub_server.py`), never a real Matrix homeserver.
// `HELPER_SRC`/`REPLY_SRC` are read straight out of the real
// `packages/butler/init.lua` by running it (in test mode) in a daemon's
// living image via `remuda exec butler`, then referenced BY NAME
// (`remuda._butler_helper_src` / `remuda._butler_reply_src`) from later
// `remuda.process` calls against that same image — the exact embedded
// source, with no separate string round-trip through Rust needed.

/// The stub Matrix homeserver, as its own process — plain
/// `std::process::Command`, not `remuda.process`: this is test
/// infrastructure standing in for a homeserver, not part of the package
/// under test.
struct StubServer {
    child: std::process::Child,
    port: u16,
}

impl StubServer {
    fn spawn(fixture: &Path, get_log: &Path, put_log: &Path, send_status: u16) -> Self {
        let mut child = std::process::Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/support/matrix_stub_server.py"
            ))
            .arg(fixture)
            .arg(get_log)
            .arg(put_log)
            .arg(send_status.to_string())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn stub matrix server");
        let stdout = child.stdout.take().expect("stub stdout piped");
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        std::io::BufRead::read_line(&mut reader, &mut line).expect("read stub port line");
        let port: u16 = line
            .trim()
            .parse()
            .expect("stub printed a port on its first line");
        Self { child, port }
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for StubServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One canned `/sync` response per line, in the shape the stub expects.
fn write_fixture(path: &Path, responses: &[serde_json::Value]) {
    let mut text = String::new();
    for r in responses {
        text.push_str(&r.to_string());
        text.push('\n');
    }
    std::fs::write(path, text).expect("write fixture");
}

/// The token file and 4-line config file (`homeserver`, `room id`, `self
/// mxid`, comma-separated allowed sender mxids) `HELPER_SRC`/`REPLY_SRC`
/// both expect.
fn butler_config(
    dir: &Path,
    tag: &str,
    homeserver: &str,
    room: &str,
    self_mxid: &str,
    allowed_senders: &str,
) -> (PathBuf, PathBuf) {
    let token_path = dir.join(format!("{tag}.token"));
    let config_path = dir.join(format!("{tag}.config"));
    std::fs::write(&token_path, "test-token\n").expect("write token");
    std::fs::write(
        &config_path,
        format!("{homeserver}\n{room}\n{self_mxid}\n{allowed_senders}\n"),
    )
    .expect("write config");
    (token_path, config_path)
}

/// Pre-start a daemon, then run the real `packages/butler/init.lua` inside
/// it with `remuda._butler_test_mode` set — the same `remuda exec`
/// invocation `exec_butler_runs_the_builtin_package_in_the_daemons_image`
/// uses — so `remuda._butler_helper_src`/`remuda._butler_reply_src` hold
/// the exact embedded source, without starting a real Claude session or
/// needing `REMUDA_BUTLER_TOKEN`/`REMUDA_BUTLER_CONFIG`. Both globals stay
/// live in this same image afterward, so a later `eval` can reference them
/// by name directly inside a `remuda.process{argv = {...}}` call.
fn butler_test_daemon(dir: &Path) -> (Daemon, PathBuf) {
    let daemon = Daemon::spawn(dir);
    let path = daemon::socket_path_in(dir, "s");
    eval(&path, "remuda._butler_test_mode = true");
    let out = remuda_timed(dir, &["-s", "s", "exec", "butler"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    (daemon, path)
}

#[test]
fn butler_helper_filters_to_the_allowlisted_room() {
    let dir = scratch_dir("butler-allowlist");
    let (_daemon, path) = butler_test_daemon(&dir);

    let allowed_room = "!allowed:example.org";
    let other_room = "!other:example.org";
    let self_mxid = "@bot:example.org";

    let get_log = dir.join("allowlist-get.log");
    let put_log = dir.join("allowlist-put.log");
    let fixture = dir.join("allowlist-fixture.jsonl");
    std::fs::write(&get_log, "").unwrap();
    std::fs::write(&put_log, "").unwrap();
    write_fixture(
        &fixture,
        &[
            serde_json::json!({"rooms": {"join": {}}, "next_batch": "aw-0"}),
            serde_json::json!({
                "rooms": {"join": {
                    allowed_room: {"timeline": {"events": [
                        {"type": "m.room.message", "event_id": "$1", "sender": "@alice:example.org",
                         "content": {"msgtype": "m.text", "body": "hello from allowed room"}}
                    ]}},
                    other_room: {"timeline": {"events": [
                        {"type": "m.room.message", "event_id": "$2", "sender": "@bob:example.org",
                         "content": {"msgtype": "m.text", "body": "hello from other room"}}
                    ]}}
                }},
                "next_batch": "aw-1"
            }),
        ],
    );

    let stub = StubServer::spawn(&fixture, &get_log, &put_log, 200);
    let (token_path, config_path) = butler_config(
        &dir,
        "allowlist",
        &stub.base_url(),
        allowed_room,
        self_mxid,
        "@alice:example.org",
    );

    eval(&path, "remuda.allow_lines = {}");
    eval(
        &path,
        "remuda.on('allow-line', function(l) table.insert(remuda.allow_lines, l) end)",
    );
    eval(
        &path,
        &format!(
            "remuda.allow_handle = remuda.process{{argv = {{'python3', '-c', remuda._butler_helper_src, {}, {}}}, on_line = 'allow-line'}}",
            lua_raw_string(&token_path.to_string_lossy()),
            lua_raw_string(&config_path.to_string_lossy()),
        ),
    );

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return #remuda.allow_lines") == 0 {
        assert!(
            Instant::now() < deadline,
            "no line ever arrived from the allowlisted room"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // A message from the non-allowlisted room must produce ZERO emits, not
    // just "the count happens to work out" — give it a real window to show
    // up wrongly before checking.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        read_count(&path, "return #remuda.allow_lines"),
        1,
        "exactly one emit was expected — the non-allowlisted room's event leaked through"
    );
    let line = eval(&path, "return remuda.allow_lines[1]");
    assert!(
        line.starts_with("@alice:example.org\t"),
        "wrong event emitted: {line}"
    );

    eval(&path, "remuda.kill(remuda.allow_handle)");
}

#[test]
fn butler_helper_drops_a_non_allowlisted_sender_in_the_same_room() {
    let dir = scratch_dir("butler-sender-allowlist");
    let (_daemon, path) = butler_test_daemon(&dir);

    let room = "!senders:example.org";
    let self_mxid = "@bot:example.org";
    let allowed_sender = "@alice:example.org";
    let other_member = "@mallory:example.org";

    let get_log = dir.join("senders-get.log");
    let put_log = dir.join("senders-put.log");
    let fixture = dir.join("senders-fixture.jsonl");
    std::fs::write(&get_log, "").unwrap();
    std::fs::write(&put_log, "").unwrap();
    write_fixture(
        &fixture,
        &[
            serde_json::json!({"rooms": {"join": {}}, "next_batch": "sa-0"}),
            serde_json::json!({
                "rooms": {"join": {
                    room: {"timeline": {"events": [
                        {"type": "m.room.message", "event_id": "$1", "sender": other_member,
                         "content": {"msgtype": "m.text", "body": "not on the allowlist"}},
                        {"type": "m.room.message", "event_id": "$2", "sender": allowed_sender,
                         "content": {"msgtype": "m.text", "body": "hello from the allowed sender"}}
                    ]}}
                }},
                "next_batch": "sa-1"
            }),
        ],
    );

    let stub = StubServer::spawn(&fixture, &get_log, &put_log, 200);
    let (token_path, config_path) = butler_config(
        &dir,
        "senders",
        &stub.base_url(),
        room,
        self_mxid,
        allowed_sender,
    );

    eval(&path, "remuda.sender_lines = {}");
    eval(
        &path,
        "remuda.on('sender-line', function(l) table.insert(remuda.sender_lines, l) end)",
    );
    eval(
        &path,
        &format!(
            "remuda.sender_handle = remuda.process{{argv = {{'python3', '-c', remuda._butler_helper_src, {}, {}}}, on_line = 'sender-line'}}",
            lua_raw_string(&token_path.to_string_lossy()),
            lua_raw_string(&config_path.to_string_lossy()),
        ),
    );

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return #remuda.sender_lines") == 0 {
        assert!(
            Instant::now() < deadline,
            "no line ever arrived from the allowlisted sender"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // The non-allowlisted member's message must produce ZERO emits, not just
    // "the count happens to work out" -- give it a real window to show up
    // wrongly before checking.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        read_count(&path, "return #remuda.sender_lines"),
        1,
        "exactly one emit was expected -- the non-allowlisted sender's event leaked through"
    );
    let line = eval(&path, "return remuda.sender_lines[1]");
    assert!(
        line.starts_with("@alice:example.org\t"),
        "wrong event emitted: {line}"
    );

    eval(&path, "remuda.kill(remuda.sender_handle)");
}

#[test]
fn butler_helper_ignores_its_own_messages() {
    let dir = scratch_dir("butler-self");
    let (_daemon, path) = butler_test_daemon(&dir);

    let room = "!self:example.org";
    let self_mxid = "@bot:example.org";

    let get_log = dir.join("self-get.log");
    let put_log = dir.join("self-put.log");
    let fixture = dir.join("self-fixture.jsonl");
    std::fs::write(&get_log, "").unwrap();
    std::fs::write(&put_log, "").unwrap();
    write_fixture(
        &fixture,
        &[
            serde_json::json!({"rooms": {"join": {}}, "next_batch": "sf-0"}),
            serde_json::json!({
                "rooms": {"join": {
                    room: {"timeline": {"events": [
                        {"type": "m.room.message", "event_id": "$1", "sender": self_mxid,
                         "content": {"msgtype": "m.text", "body": "an echo of my own reply"}}
                    ]}}
                }},
                "next_batch": "sf-1"
            }),
        ],
    );

    let stub = StubServer::spawn(&fixture, &get_log, &put_log, 200);
    let (token_path, config_path) =
        butler_config(&dir, "self", &stub.base_url(), room, self_mxid, "");

    eval(&path, "remuda.self_lines = {}");
    eval(
        &path,
        "remuda.on('self-line', function(l) table.insert(remuda.self_lines, l) end)",
    );
    eval(
        &path,
        &format!(
            "remuda.self_handle = remuda.process{{argv = {{'python3', '-c', remuda._butler_helper_src, {}, {}}}, on_line = 'self-line'}}",
            lua_raw_string(&token_path.to_string_lossy()),
            lua_raw_string(&config_path.to_string_lossy()),
        ),
    );

    // Proof the fixture's event was actually reached (not just "nothing ran
    // yet"): wait for the stub to have answered both the baseline call and
    // the one carrying the self-authored event.
    let deadline = Instant::now() + PATIENCE;
    while std::fs::read_to_string(&get_log)
        .unwrap_or_default()
        .lines()
        .count()
        < 2
    {
        assert!(
            Instant::now() < deadline,
            "the helper never even polled past the baseline"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        read_count(&path, "return #remuda.self_lines"),
        0,
        "a message from the bridge's own account must never be re-ingested"
    );

    eval(&path, "remuda.kill(remuda.self_handle)");
}

#[test]
fn butler_helper_escapes_a_multiline_message_into_exactly_one_line() {
    let dir = scratch_dir("butler-escape");
    let (_daemon, path) = butler_test_daemon(&dir);

    let room = "!escape:example.org";
    let self_mxid = "@bot:example.org";
    // Includes a real newline, a standalone backslash, AND a literal
    // backslash-then-"n" (not a real newline) — the last one is what a
    // naive two-pass unescape (newline-pass then backslash-pass) gets
    // wrong, since the backslash it doubles for its own escaping can end
    // up immediately before a literal "n" and be misread as a newline
    // escape on the first pass.
    let original_body = "line one\nline two with a \\ backslash and a literal \\n as text";

    let get_log = dir.join("escape-get.log");
    let put_log = dir.join("escape-put.log");
    let fixture = dir.join("escape-fixture.jsonl");
    std::fs::write(&get_log, "").unwrap();
    std::fs::write(&put_log, "").unwrap();
    write_fixture(
        &fixture,
        &[
            serde_json::json!({"rooms": {"join": {}}, "next_batch": "es-0"}),
            serde_json::json!({
                "rooms": {"join": {
                    room: {"timeline": {"events": [
                        {"type": "m.room.message", "event_id": "$1", "sender": "@carol:example.org",
                         "content": {"msgtype": "m.text", "body": original_body}}
                    ]}}
                }},
                "next_batch": "es-1"
            }),
        ],
    );

    let stub = StubServer::spawn(&fixture, &get_log, &put_log, 200);
    let (token_path, config_path) = butler_config(
        &dir,
        "escape",
        &stub.base_url(),
        room,
        self_mxid,
        "@carol:example.org",
    );

    eval(&path, "remuda.esc_lines = {}");
    eval(
        &path,
        "remuda.on('esc-line', function(l) table.insert(remuda.esc_lines, l) end)",
    );
    eval(
        &path,
        &format!(
            "remuda.esc_handle = remuda.process{{argv = {{'python3', '-c', remuda._butler_helper_src, {}, {}}}, on_line = 'esc-line'}}",
            lua_raw_string(&token_path.to_string_lossy()),
            lua_raw_string(&config_path.to_string_lossy()),
        ),
    );

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return #remuda.esc_lines") == 0 {
        assert!(Instant::now() < deadline, "no line ever arrived");
        std::thread::sleep(Duration::from_millis(20));
    }
    // Exactly one emit for one Matrix event — not two, not a parse error —
    // even though the body contains a real newline.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        read_count(&path, "return #remuda.esc_lines"),
        1,
        "one Matrix event with an embedded newline must become exactly one physical line"
    );

    let raw_line = eval(&path, "return remuda.esc_lines[1]");
    assert!(
        !raw_line.contains('\n'),
        "the delivered line itself contained a raw newline: {raw_line:?}"
    );

    // Round-trip: the exact unescape logic `init.lua`'s own `butler-matrix-line`
    // hook uses, run against the raw received line.
    let reconstructed = eval(
        &path,
        &format!(
            r#"
            local line = {raw}
            local _, body = line:match("^([^\t]*)\t(.*)$")
            body = body:gsub("\\(.)", function(c) return c == "n" and "\n" or c end)
            return body
            "#,
            raw = lua_raw_string(&raw_line)
        ),
    );
    assert_eq!(
        reconstructed, original_body,
        "the round trip through escape/unescape did not reproduce the original body"
    );

    eval(&path, "remuda.kill(remuda.esc_handle)");
}

/// Writes an empty get/put log and a fixture, then starts a stub server on
/// it. Shared by tests that need more than one stub instance in sequence
/// (e.g. proving persistence across a restart), so each instance is one
/// line instead of the six it takes to set up by hand.
fn spawn_stub(dir: &Path, tag: &str, responses: &[serde_json::Value]) -> (StubServer, PathBuf) {
    let get_log = dir.join(format!("{tag}-get.log"));
    let put_log = dir.join(format!("{tag}-put.log"));
    let fixture = dir.join(format!("{tag}-fixture.jsonl"));
    std::fs::write(&get_log, "").unwrap();
    std::fs::write(&put_log, "").unwrap();
    write_fixture(&fixture, responses);
    (
        StubServer::spawn(&fixture, &get_log, &put_log, 200),
        get_log,
    )
}

#[test]
// The 4th config line (sender allowlist) pushed this back over clippy's
// too-many-lines threshold; the two-instance restart scenario this proves
// doesn't split further without losing the point of the test.
#[allow(clippy::too_many_lines)]
fn butler_helper_persists_since_across_a_restart() {
    let dir = scratch_dir("butler-persist");
    let (_daemon, path) = butler_test_daemon(&dir);

    let room = "!persist:example.org";
    let self_mxid = "@bot:example.org";

    // First instance: a baseline, then one qualifying event.
    let (stub1, _get_log1) = spawn_stub(
        &dir,
        "persist1",
        &[
            serde_json::json!({"rooms": {"join": {}}, "next_batch": "d-since-0"}),
            serde_json::json!({
                "rooms": {"join": {
                    room: {"timeline": {"events": [
                        {"type": "m.room.message", "event_id": "$1", "sender": "@dave:example.org",
                         "content": {"msgtype": "m.text", "body": "first batch"}}
                    ]}}
                }},
                "next_batch": "d-since-1"
            }),
        ],
    );
    let (token_path, config_path) = butler_config(
        &dir,
        "persist",
        &stub1.base_url(),
        room,
        self_mxid,
        "@dave:example.org",
    );
    let since_path = format!("{}.since", config_path.display());

    eval(&path, "remuda.persist_lines = {}");
    eval(
        &path,
        "remuda.on('persist-line', function(l) table.insert(remuda.persist_lines, l) end)",
    );
    eval(
        &path,
        &format!(
            "remuda.persist_handle = remuda.process{{argv = {{'python3', '-c', remuda._butler_helper_src, {}, {}}}, on_line = 'persist-line'}}",
            lua_raw_string(&token_path.to_string_lossy()),
            lua_raw_string(&config_path.to_string_lossy()),
        ),
    );

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return #remuda.persist_lines") == 0 {
        assert!(
            Instant::now() < deadline,
            "the first instance never emitted its line"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    eval(&path, "remuda.kill(remuda.persist_handle)");
    std::thread::sleep(Duration::from_millis(200));
    drop(stub1);

    let since_before = std::fs::read_to_string(&since_path).expect("since file exists");
    assert!(
        since_before.contains("d-since-1"),
        "unexpected since file contents: {since_before}"
    );

    // Second instance: same config path (so the same since file), a fresh
    // stub with its own second batch, and the config file repointed at it.
    let (stub2, get_log2) = spawn_stub(
        &dir,
        "persist2",
        &[serde_json::json!({"rooms": {"join": {}}, "next_batch": "d-since-2"})],
    );
    std::fs::write(
        &config_path,
        format!(
            "{}\n{}\n{}\n@dave:example.org\n",
            stub2.base_url(),
            room,
            self_mxid
        ),
    )
    .expect("repoint config at the second stub");

    eval(
        &path,
        &format!(
            "remuda.persist_handle2 = remuda.process{{argv = {{'python3', '-c', remuda._butler_helper_src, {}, {}}}}}",
            lua_raw_string(&token_path.to_string_lossy()),
            lua_raw_string(&config_path.to_string_lossy()),
        ),
    );

    let deadline = Instant::now() + PATIENCE;
    loop {
        if !std::fs::read_to_string(&get_log2)
            .unwrap_or_default()
            .trim()
            .is_empty()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the second instance never called the new stub"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let first_request = std::fs::read_to_string(&get_log2).expect("get log2");
    let first_line = first_request.lines().next().expect("at least one request");
    assert!(
        first_line.contains("since=d-since-1"),
        "the second instance did not resume from the persisted since token: {first_line}"
    );
    assert!(
        !first_line.contains("since=d-since-0") && !first_line.contains("timeout=0"),
        "the second instance replayed the baseline instead of resuming: {first_line}"
    );

    eval(&path, "remuda.kill(remuda.persist_handle2)");
}

// `REPLY_SRC` is a bash script by design (packages/butler/init.lua) -- the
// real deployment target is a single Linux host, and there is no plan to
// run this specific package's helpers on Windows. Windows CI does have a
// `bash` on PATH (Git Bash), but the script's coreutils-flavored pieces
// (`date +%s%N`, `sed -n`) are not guaranteed to behave identically there,
// and that gap is not worth chasing for a component that will never run on
// that platform in practice.
#[test]
#[cfg(unix)]
fn matrix_reply_tool_queues_a_send_and_reports_its_own_exit() {
    let dir = scratch_dir("butler-reply");
    let (_daemon, path) = butler_test_daemon(&dir);

    let room = "!reply:example.org";
    let self_mxid = "@bot:example.org";
    let empty_fixture = dir.join("reply-fixture.jsonl");
    std::fs::write(&empty_fixture, "").expect("write empty fixture");

    // Success case.
    let get_log = dir.join("reply-get.log");
    let put_log = dir.join("reply-put.log");
    std::fs::write(&get_log, "").unwrap();
    std::fs::write(&put_log, "").unwrap();
    let stub_ok = StubServer::spawn(&empty_fixture, &get_log, &put_log, 200);
    let (token_path, config_path) =
        butler_config(&dir, "reply-ok", &stub_ok.base_url(), room, self_mxid, "");

    eval(&path, "remuda.reply_exit_ok = nil");
    eval(
        &path,
        "remuda.on('reply-exit-ok', function(c) remuda.reply_exit_ok = c end)",
    );
    eval(
        &path,
        &format!(
            "remuda.process{{argv = {{'bash', '-c', remuda._butler_reply_src, '_', {}, {}, 'hello'}}, on_exit = 'reply-exit-ok'}}",
            lua_raw_string(&token_path.to_string_lossy()),
            lua_raw_string(&config_path.to_string_lossy()),
        ),
    );

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return remuda.reply_exit_ok and 1 or 0") == 0 {
        assert!(
            Instant::now() < deadline,
            "on_exit never fired for the successful send"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        read_count(&path, "return remuda.reply_exit_ok"),
        0,
        "a successful send must exit 0"
    );
    let sent = std::fs::read_to_string(&put_log).expect("put log");
    assert!(
        sent.contains("hello"),
        "the stub never received the reply body: {sent}"
    );

    // Failure case: a fresh stub configured to answer 400.
    let get_log2 = dir.join("reply-get2.log");
    let put_log2 = dir.join("reply-put2.log");
    std::fs::write(&get_log2, "").unwrap();
    std::fs::write(&put_log2, "").unwrap();
    let stub_fail = StubServer::spawn(&empty_fixture, &get_log2, &put_log2, 400);
    let (token_path2, config_path2) = butler_config(
        &dir,
        "reply-fail",
        &stub_fail.base_url(),
        room,
        self_mxid,
        "",
    );

    eval(&path, "remuda.reply_exit_fail = nil");
    eval(
        &path,
        "remuda.on('reply-exit-fail', function(c) remuda.reply_exit_fail = c end)",
    );
    eval(
        &path,
        &format!(
            "remuda.process{{argv = {{'bash', '-c', remuda._butler_reply_src, '_', {}, {}, 'hello'}}, on_exit = 'reply-exit-fail'}}",
            lua_raw_string(&token_path2.to_string_lossy()),
            lua_raw_string(&config_path2.to_string_lossy()),
        ),
    );

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return remuda.reply_exit_fail and 1 or 0") == 0 {
        assert!(
            Instant::now() < deadline,
            "on_exit never fired for the failing send"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        read_count(&path, "return remuda.reply_exit_fail") != 0,
        "a failing send (HTTP 400) must be observably distinguishable from success via a nonzero exit code"
    );
}

// Fix for "TOKEN IN ARGV": the reply script must never pass the bearer
// token as a curl argument, since a process's argv is visible to any other
// user via `ps`. Proven by intercepting curl itself: a fake `curl` on a
// PATH of our own logs exactly the argv and stdin it received, then exits
// 0 without ever making a network call -- REPLY_SRC's own token-handling is
// what's under test here, not the network path (already covered by
// `matrix_reply_tool_queues_a_send_and_reports_its_own_exit`).
#[test]
#[cfg(unix)]
fn matrix_reply_tool_never_puts_the_token_in_curls_argv() {
    use std::os::unix::fs::PermissionsExt;

    let dir = scratch_dir("butler-reply-token");
    let (_daemon, path) = butler_test_daemon(&dir);
    let reply_src = eval(&path, "return remuda._butler_reply_src");

    let token_path = dir.join("token-argv.token");
    let config_path = dir.join("token-argv.config");
    let token = "s3cr3t-token-value";
    std::fs::write(&token_path, format!("{token}\n")).expect("write token");
    std::fs::write(
        &config_path,
        "http://127.0.0.1:1\n!room:example.org\n@bot:example.org\n",
    )
    .expect("write config");

    let fake_curl_dir = dir.join("fake-bin");
    std::fs::create_dir_all(&fake_curl_dir).expect("fake bin dir");
    let argv_log = dir.join("curl-argv.log");
    let stdin_log = dir.join("curl-stdin.log");
    std::fs::write(&argv_log, "").expect("init argv log");
    let fake_curl = fake_curl_dir.join("curl");
    std::fs::write(
        &fake_curl,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> '{}'; done\ncat > '{}'\nexit 0\n",
            argv_log.display(),
            stdin_log.display(),
        ),
    )
    .expect("write fake curl");
    std::fs::set_permissions(&fake_curl, std::fs::Permissions::from_mode(0o755))
        .expect("chmod fake curl");

    let path_with_fake_curl = format!(
        "{}:{}",
        fake_curl_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let status = std::process::Command::new("bash")
        .arg("-c")
        .arg(&reply_src)
        .arg("_")
        .arg(&token_path)
        .arg(&config_path)
        .arg("hello")
        .env("PATH", path_with_fake_curl)
        .status()
        .expect("run REPLY_SRC with the fake curl on PATH");
    assert!(
        status.success(),
        "REPLY_SRC exited nonzero against the fake curl"
    );

    let logged_argv = std::fs::read_to_string(&argv_log).expect("read fake curl's argv log");
    assert!(
        !logged_argv.contains(token),
        "the token appeared in curl's own argv: {logged_argv:?}"
    );

    let logged_stdin = std::fs::read_to_string(&stdin_log).unwrap_or_default();
    assert!(
        logged_stdin.contains(&format!("Authorization: Bearer {token}")),
        "the Authorization header was never delivered to curl via stdin: {logged_stdin:?}"
    );
}

/// No test in this file drives `init.lua` past `_butler_test_mode`'s early
/// return (see `butler_test_daemon`), so nothing here ever reaches the real
/// `remuda.new(nil, {"claude", ...})` launch and its argv. Reproducing that
/// live would mean giving the daemon process itself (not the CLI call that
/// asks it to `exec butler`) a `claude` on `PATH` plus real
/// `REMUDA_BUTLER_TOKEN`/`REMUDA_BUTLER_CONFIG` — machinery this suite
/// doesn't have today. This is the proportionate substitute: a compile-time
/// check, via `include_str!`, that the shipped launch argv still carries
/// `--allowedTools`/`mcp__remuda__run_script` in the right slot (between
/// `--permission-mode auto` and `--append-system-prompt`), so a later edit
/// can't silently drop or reorder it.
#[test]
fn butler_launch_argv_allows_the_run_script_tool_for_schedule_registration() {
    let init_lua = include_str!("../../packages/butler/init.lua");

    let permission_mode_idx = init_lua
        .find("\"--permission-mode\",")
        .expect("butler launch argv lost --permission-mode");
    let allowed_tools_idx = init_lua
        .find("\"--allowedTools\",")
        .expect("butler launch argv is missing --allowedTools");
    let run_script_idx = init_lua
        .find("\"mcp__remuda__run_script\",")
        .expect("butler launch argv is missing the mcp__remuda__run_script tool name");
    let append_system_prompt_idx = init_lua
        .find("\"--append-system-prompt\",")
        .expect("butler launch argv lost --append-system-prompt");

    assert!(
        permission_mode_idx < allowed_tools_idx
            && allowed_tools_idx < run_script_idx
            && run_script_idx < append_system_prompt_idx,
        "expected --allowedTools, mcp__remuda__run_script between --permission-mode and \
         --append-system-prompt in the butler launch argv, got byte offsets: \
         permission-mode={permission_mode_idx} allowedTools={allowed_tools_idx} \
         run_script={run_script_idx} append-system-prompt={append_system_prompt_idx}"
    );
}

/// Same live-`claude` limitation as the test above blocks a real kill-and-
/// watch-it-come-back test for the respawn watchdog. This checks, at the
/// source level, that the watchdog reuses one launch function (so a
/// respawn can't drift from a fresh start) and clears its hook group before
/// registering (so a second `exec butler` can't double-launch a session).
#[test]
fn butler_session_exited_hook_relaunches_via_the_shared_launch_function() {
    // Normalized once: a `\n`-only search below would miss a real call on a
    // checkout where git converts this file to CRLF (Windows runners do).
    let init_lua = include_str!("../../packages/butler/init.lua").replace("\r\n", "\n");

    let launch_fn_idx = init_lua
        .find("local function launch_butler()")
        .expect("butler package lost its shared launch function");
    let initial_call_idx = init_lua
        .find("launch_butler()\n")
        .expect("butler package never calls launch_butler() at load time");
    let clear_hooks_idx = init_lua
        .find("remuda.clear_hooks({ group = \"butler\" })")
        .expect("butler package lost its clear_hooks guard against a second exec");
    let session_exited_idx = init_lua
        .find("remuda.on(\"session_exited\",")
        .expect("butler package lost its session_exited watchdog");

    assert!(
        launch_fn_idx < initial_call_idx && initial_call_idx < clear_hooks_idx,
        "expected launch_butler to be defined, then called once, before the \
         watchdog is registered"
    );
    assert!(
        clear_hooks_idx < session_exited_idx,
        "expected clear_hooks({{group = \"butler\"}}) to run before the \
         session_exited hook is (re-)registered, so a second exec can't double it"
    );

    let hook_body_end = init_lua[session_exited_idx..]
        .find("end, { group = \"butler\" })")
        .map(|i| session_exited_idx + i)
        .expect("session_exited hook is not registered in the \"butler\" group");
    let hook_body = &init_lua[session_exited_idx..hook_body_end];
    assert!(
        hook_body.contains("launch_butler()"),
        "the session_exited watchdog must relaunch via launch_butler(), not \
         its own remuda.new call: {hook_body:?}"
    );
}

/// The watchdog end to end, proven by a real effect rather than a call
/// record. `_butler_test_mode` is never set here -- this is the first test
/// in this file to run the real `launch_butler`/`session_exited` code past
/// `init.lua`'s test-mode early return (the source-level substitutes are the
/// two tests just above this one). `remuda._butler_argv` stands in for a
/// real `claude` with a short-but-real self-exiting process, and
/// `remuda._butler_skip_relay` keeps the forever-retrying Matrix relay (see
/// `HELPER_SRC`'s `while True`) from ever spawning against these dummy
/// credentials -- see the teardown assertion at the end of this test for why
/// that belief alone is not trusted.
#[test]
#[cfg(unix)]
fn butler_watchdog_relaunches_a_session_that_really_died() {
    let dir = scratch_dir("butler-watchdog");
    let (token_path, config_path) = butler_config(
        &dir,
        "watchdog",
        "http://127.0.0.1:1",
        "!room:example.org",
        "@butler:example.org",
        "",
    );
    let token_str = token_path.to_string_lossy().to_string();
    let config_str = config_path.to_string_lossy().to_string();
    let daemon = Daemon::spawn_with_env(
        &dir,
        &[
            ("REMUDA_BUTLER_TOKEN", token_str.as_str()),
            ("REMUDA_BUTLER_CONFIG", config_str.as_str()),
        ],
    );
    let path = daemon::socket_path_in(&dir, "s");

    // A separate hook group: `init.lua` clears the "butler" group itself on
    // every `exec` (see its own comment above `remuda.clear_hooks`), so an
    // observer registered there would be wiped the moment `exec butler` runs.
    eval(
        &path,
        r#"
            remuda._watchdog_exits = {}
            remuda.on("session_exited", function(name)
                table.insert(remuda._watchdog_exits, name)
            end, { group = "test-observer" })
        "#,
    );
    // A real, short-but-nonzero-lived self-exiting process standing in for
    // `claude` -- 0.3s per life is long enough that two full lives can't
    // complete in near-zero time, which is what the latency-floor assertion
    // below turns into a real check rather than a comment's promise.
    eval(
        &path,
        r#"remuda._butler_argv = {"sh", "-c", "sleep 0.3; exit 0"}"#,
    );
    eval(&path, "remuda._butler_skip_relay = true");

    let out = remuda_timed(&dir, &["-s", "s", "exec", "butler"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let initial_name = eval(&path, "return remuda._butler_initial_name");

    let start = Instant::now();
    let deadline = start + PATIENCE;
    loop {
        let seen = eval(&path, "return table.concat(remuda._watchdog_exits, ',')");
        let count = seen.split(',').filter(|n| *n == initial_name).count();
        if count >= 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "expected at least 2 session_exited events for {initial_name:?}, saw: {seen:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let elapsed = start.elapsed();
    // A name can only exit twice if a genuinely new process was registered
    // under it between the two deaths: `Registry::register` refuses a name
    // still in use, and only `remove`/`close` clear that entry (see
    // core/src/registry.rs) -- so 2 real `session_exited` events for one
    // name is direct evidence `launch_butler()` itself ran and spawned a
    // fresh process, not merely that the callback fired. Two full lives at
    // 0.3s each, plus at least one reap cycle, cannot land here in
    // near-zero time; a value that small would mean the exit/relaunch never
    // actually happened.
    assert!(
        elapsed >= Duration::from_millis(400),
        "two real respawn cycles took only {elapsed:?} -- implausibly fast for two \
         `sleep 0.3` lives, suggesting the observation was short-circuited"
    );

    // Teardown is verified by the resource, not the belief that
    // `_butler_skip_relay` prevented anything: kill the daemon, then prove
    // via a real `ps` snapshot that nothing survives referencing this
    // test's own (unique per invocation) token path. If the relay guard
    // ever failed silently, its forever-retrying `while True` loop (see
    // `HELPER_SRC`) would still be running right here -- nothing in this
    // codebase reaps a `remuda.process{}` child when the daemon dies
    // (`native/src/process.rs` spawns a plain `std::process::Command`, no
    // process group).
    drop(daemon);
    let ps = std::process::Command::new("ps")
        .args(["-eo", "pid,args"])
        .output()
        .expect("ps");
    let ps_out = String::from_utf8_lossy(&ps.stdout);
    let leaked: Vec<&str> = ps_out.lines().filter(|l| l.contains(&token_str)).collect();
    assert!(
        leaked.is_empty(),
        "orphan process(es) still reference this test's own token path after teardown: {leaked:?}"
    );
}

/// Registration itself needs no live session at all -- `remuda.schedule`
/// does not touch `butler_name`, only the `run` callback does when it later
/// fires -- so this exercises `remuda._butler_register_compaction_schedule()`
/// exactly the way a launched session's own `run_script` call would: a
/// separate `eval` against an already-running daemon, called twice, standing
/// in for a re-`exec` or a confused agent calling it more than once.
/// `remuda.schedule` has no group-based bulk-cancel (only `remuda.clear_hooks`
/// does, and only for hooks -- see `tools.lua`'s own doc comment on
/// `remuda.schedule`), so without the cancel-before-register guard this would
/// leave TWO handles both named "butler-compaction" in `remuda.schedules`.
#[test]
#[cfg(unix)]
fn butler_compaction_schedule_registration_is_idempotent() {
    let dir = scratch_dir("butler-compaction-idem");
    let (token_path, config_path) = butler_config(
        &dir,
        "compaction-idem",
        "http://127.0.0.1:1",
        "!room:example.org",
        "@butler:example.org",
        "",
    );
    let token_str = token_path.to_string_lossy().to_string();
    let config_str = config_path.to_string_lossy().to_string();
    let daemon = Daemon::spawn_with_env(
        &dir,
        &[
            ("REMUDA_BUTLER_TOKEN", token_str.as_str()),
            ("REMUDA_BUTLER_CONFIG", config_str.as_str()),
        ],
    );
    let path = daemon::socket_path_in(&dir, "s");

    eval(
        &path,
        r#"remuda._butler_argv = {"sh", "-c", "sleep 5; exit 0"}"#,
    );
    eval(&path, "remuda._butler_skip_relay = true");

    let out = remuda_timed(&dir, &["-s", "s", "exec", "butler"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Two separate Eval requests, exactly like two separate `run_script`
    // calls would arrive -- a plain Lua local inside init.lua would not even
    // be reachable a second time this way, which is the whole reason the
    // handle lives on `remuda` itself.
    eval(&path, "remuda._butler_register_compaction_schedule()");
    eval(&path, "remuda._butler_register_compaction_schedule()");

    let live = read_count(
        &path,
        r#"
            local n = 0
            for _, s in pairs(remuda.schedules) do
                if s.name == "butler-compaction" then n = n + 1 end
            end
            return n
        "#,
    );
    assert_eq!(
        live, 1,
        "two calls to remuda._butler_register_compaction_schedule() left {live} live \
         \"butler-compaction\" schedules -- expected exactly 1 (cancel-before-register)"
    );

    drop(daemon);
    let ps = std::process::Command::new("ps")
        .args(["-eo", "pid,args"])
        .output()
        .expect("ps");
    let ps_out = String::from_utf8_lossy(&ps.stdout);
    let leaked: Vec<&str> = ps_out.lines().filter(|l| l.contains(&token_str)).collect();
    assert!(
        leaked.is_empty(),
        "orphan process(es) still reference this test's own token path after teardown: {leaked:?}"
    );
}

/// The schedule end to end, on a real spawned `sh` standing in for the
/// launched `claude` session -- the same substitution
/// `butler_watchdog_relaunches_a_session_that_really_died` uses. Split into
/// two phases against ONE session's one life: busy (kept below the 2s idle
/// threshold `Session::idle_for`/`Session.is_busy` read, by repeatedly
/// sending a bare Enter -- the same proven-harmless no-op the Matrix relay's
/// own submit hook already relies on) must never show `/compact`; idle
/// (input stops) must eventually show it, typed and echoed by the pty the
/// moment `remuda.send` writes it. `remuda._butler_compaction_interval` is
/// set tiny, but the daemon's own tick (`native/src/daemon.rs::TICK_PERIOD`,
/// 1s) is the real firing granularity -- so a "tiny" interval only means
/// "fires every tick", not sub-second.
#[test]
#[cfg(unix)]
fn butler_compaction_schedule_sends_compact_when_idle_but_not_when_busy() {
    let dir = scratch_dir("butler-compaction-fire");
    let (token_path, config_path) = butler_config(
        &dir,
        "compaction-fire",
        "http://127.0.0.1:1",
        "!room:example.org",
        "@butler:example.org",
        "",
    );
    let token_str = token_path.to_string_lossy().to_string();
    let config_str = config_path.to_string_lossy().to_string();
    let daemon = Daemon::spawn_with_env(
        &dir,
        &[
            ("REMUDA_BUTLER_TOKEN", token_str.as_str()),
            ("REMUDA_BUTLER_CONFIG", config_str.as_str()),
        ],
    );
    let path = daemon::socket_path_in(&dir, "s");

    eval(&path, "remuda._butler_compaction_interval = 0.05");
    eval(&path, r#"remuda._butler_argv = {"sh"}"#);
    eval(&path, "remuda._butler_skip_relay = true");

    let out = remuda_timed(&dir, &["-s", "s", "exec", "butler"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let butler_name = eval(&path, "return remuda._butler_initial_name");

    // Simulates the launched session's own one-time `run_script` call the
    // system prompt asks for.
    eval(&path, "remuda._butler_register_compaction_schedule()");

    // Busy phase: outrun the 2s idle threshold for a few seconds, spanning
    // at least two real 1s daemon ticks, and confirm the guard actually
    // holds the send back.
    let busy_until = Instant::now() + Duration::from_millis(2500);
    while Instant::now() < busy_until {
        eval(&path, &format!("remuda.send({butler_name:?}, \"\")"));
        std::thread::sleep(Duration::from_millis(300));
    }
    let screen_while_busy = capture(&path, &butler_name);
    assert!(
        !screen_while_busy.contains("/compact"),
        "compaction fired while the session was kept busy:\n{screen_while_busy}"
    );

    // Idle phase: stop feeding input and wait for the next tick to see the
    // guard flip and /compact actually get typed (echoed immediately by the
    // pty, no need to wait for the delayed Enter confirm).
    let deadline = Instant::now() + PATIENCE;
    loop {
        let screen = capture(&path, &butler_name);
        if screen.contains("/compact") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "compaction never fired once the session went idle. last screen:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    drop(daemon);
    let ps = std::process::Command::new("ps")
        .args(["-eo", "pid,args"])
        .output()
        .expect("ps");
    let ps_out = String::from_utf8_lossy(&ps.stdout);
    let leaked: Vec<&str> = ps_out.lines().filter(|l| l.contains(&token_str)).collect();
    assert!(
        leaked.is_empty(),
        "orphan process(es) still reference this test's own token path after teardown: {leaked:?}"
    );
}

/// Minimal shape check for `os.date("!%Y-%m-%dT%H:%M:%SZ")` -- exactly what
/// `_butler_trace` in `packages/butler/init.lua` writes -- without pulling a
/// date-parsing crate into this test binary for one field.
fn looks_like_iso8601_utc(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 20
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[13] == b':'
        && b[16] == b':'
        && b[19] == b'Z'
        && s[0..4].bytes().all(|c| c.is_ascii_digit())
        && s[5..7].bytes().all(|c| c.is_ascii_digit())
        && s[8..10].bytes().all(|c| c.is_ascii_digit())
        && s[11..13].bytes().all(|c| c.is_ascii_digit())
        && s[14..16].bytes().all(|c| c.is_ascii_digit())
        && s[17..19].bytes().all(|c| c.is_ascii_digit())
}

/// The screen-based witness just above (`..._sends_compact_when_idle...`)
/// proves the *effect* fired, but nothing about it survives past that pty --
/// gone the moment the session closes, and gone entirely across a daemon
/// restart, which is exactly the gap `_butler_trace` closes. Same busy/idle
/// two-phase control as that test, but the witness here is the trace FILE's
/// real bytes read back from disk, proving what a human opening this file on
/// some later day, with no session left to look at, would actually see.
#[test]
#[cfg(unix)]
fn butler_compaction_trace_records_registered_skipped_and_sent() {
    let dir = scratch_dir("butler-compaction-trace");
    let (token_path, config_path) = butler_config(
        &dir,
        "compaction-trace",
        "http://127.0.0.1:1",
        "!room:example.org",
        "@butler:example.org",
        "",
    );
    let token_str = token_path.to_string_lossy().to_string();
    let config_str = config_path.to_string_lossy().to_string();
    let daemon = Daemon::spawn_with_env(
        &dir,
        &[
            ("REMUDA_BUTLER_TOKEN", token_str.as_str()),
            ("REMUDA_BUTLER_CONFIG", config_str.as_str()),
        ],
    );
    let path = daemon::socket_path_in(&dir, "s");

    // This test's own tempdir, never the real `~/.config/remuda/`.
    let trace_path = dir.join("compaction-trace.log");
    eval(&path, "remuda._butler_compaction_interval = 0.05");
    eval(
        &path,
        &format!(
            "remuda._butler_compaction_trace_path = {}",
            lua_raw_string(&trace_path.to_string_lossy())
        ),
    );
    eval(&path, r#"remuda._butler_argv = {"sh"}"#);
    eval(&path, "remuda._butler_skip_relay = true");

    let out = remuda_timed(&dir, &["-s", "s", "exec", "butler"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let butler_name = eval(&path, "return remuda._butler_initial_name");

    // Simulates the launched session's own one-time `run_script` call the
    // system prompt asks for -- this alone must already leave a "registered"
    // line, before any tick has had a chance to run.
    eval(&path, "remuda._butler_register_compaction_schedule()");
    let registered = std::fs::read_to_string(&trace_path).unwrap_or_default();
    assert!(
        registered.contains("\tregistered\t"),
        "no \"registered\" trace line after registration:\n{registered}"
    );

    // Busy phase, same as the sibling screen-based test: outrun the 2s idle
    // threshold for a few seconds, spanning several real 1s daemon ticks,
    // and prove the guard actually left a "skipped_busy" line for it — not
    // just that /compact was withheld.
    let busy_until = Instant::now() + Duration::from_millis(2500);
    while Instant::now() < busy_until {
        eval(&path, &format!("remuda.send({butler_name:?}, \"\")"));
        std::thread::sleep(Duration::from_millis(300));
    }
    let while_busy = std::fs::read_to_string(&trace_path).unwrap_or_default();
    assert!(
        while_busy.contains("\tskipped_busy\t"),
        "no \"skipped_busy\" trace line recorded during the busy phase:\n{while_busy}"
    );
    assert!(
        !while_busy.contains("\tsent\t"),
        "a \"sent\" trace line appeared while the session was kept busy:\n{while_busy}"
    );

    // Idle phase: stop feeding input and wait for the next tick to record a
    // real "sent" line.
    let deadline = Instant::now() + PATIENCE;
    loop {
        let content = std::fs::read_to_string(&trace_path).unwrap_or_default();
        if content.contains("\tsent\t") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no \"sent\" trace line ever appeared once the session went idle:\n{content}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Witness discipline: read the ACTUAL bytes back from disk and validate
    // every line's shape, not just substring presence.
    let content = std::fs::read_to_string(&trace_path).expect("read trace file");
    let lines: Vec<&str> = content.lines().collect();
    assert!(!lines.is_empty(), "trace file was empty");
    for line in &lines {
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(
            fields.len(),
            3,
            "line {line:?} does not have exactly 3 tab-separated fields"
        );
        assert!(
            looks_like_iso8601_utc(fields[0]),
            "first field {:?} is not a parseable ISO-8601 UTC timestamp in line {line:?}",
            fields[0]
        );
    }

    drop(daemon);
    let ps = std::process::Command::new("ps")
        .args(["-eo", "pid,args"])
        .output()
        .expect("ps");
    let ps_out = String::from_utf8_lossy(&ps.stdout);
    let leaked: Vec<&str> = ps_out.lines().filter(|l| l.contains(&token_str)).collect();
    assert!(
        leaked.is_empty(),
        "orphan process(es) still reference this test's own token path after teardown: {leaked:?}"
    );
}

/// Same real-process substitution as
/// `butler_watchdog_relaunches_a_session_that_really_died`, but the witness
/// here is the trace FILE `_butler_session_trace` in `packages/butler/init.lua`
/// writes from the `session_exited` hook, not the in-memory
/// `remuda._watchdog_exits` list -- proving the watchdog's own trace records
/// both the exit and the relaunch it triggers, not just that they happened.
#[test]
#[cfg(unix)]
fn butler_watchdog_records_session_exit_and_relaunch_in_a_trace_file() {
    let dir = scratch_dir("butler-watchdog-trace");
    let (token_path, config_path) = butler_config(
        &dir,
        "watchdog-trace",
        "http://127.0.0.1:1",
        "!room:example.org",
        "@butler:example.org",
        "",
    );
    let token_str = token_path.to_string_lossy().to_string();
    let config_str = config_path.to_string_lossy().to_string();
    let daemon = Daemon::spawn_with_env(
        &dir,
        &[
            ("REMUDA_BUTLER_TOKEN", token_str.as_str()),
            ("REMUDA_BUTLER_CONFIG", config_str.as_str()),
        ],
    );
    let path = daemon::socket_path_in(&dir, "s");

    // This test's own tempdir, never the real `~/.config/remuda/`.
    let trace_path = dir.join("session-trace.log");
    eval(
        &path,
        &format!(
            "remuda._butler_session_trace_path = {}",
            lua_raw_string(&trace_path.to_string_lossy())
        ),
    );
    // Same observer group as `butler_watchdog_relaunches_a_session_that_really_died`
    // -- `init.lua` clears the "butler" group on every `exec`, so a hook
    // registered there would not survive it.
    eval(
        &path,
        r#"
            remuda._watchdog_exits = {}
            remuda.on("session_exited", function(name)
                table.insert(remuda._watchdog_exits, name)
            end, { group = "test-observer" })
        "#,
    );
    eval(
        &path,
        r#"remuda._butler_argv = {"sh", "-c", "sleep 0.3; exit 0"}"#,
    );
    eval(&path, "remuda._butler_skip_relay = true");

    let out = remuda_timed(&dir, &["-s", "s", "exec", "butler"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let initial_name = eval(&path, "return remuda._butler_initial_name");

    let deadline = Instant::now() + PATIENCE;
    loop {
        let seen = eval(&path, "return table.concat(remuda._watchdog_exits, ',')");
        let count = seen.split(',').filter(|n| *n == initial_name).count();
        if count >= 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "expected at least 2 session_exited events for {initial_name:?}, saw: {seen:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Witness discipline: read the ACTUAL bytes back from disk, same as
    // `butler_compaction_trace_records_registered_skipped_and_sent`.
    let content = std::fs::read_to_string(&trace_path).expect("read trace file");
    let lines: Vec<Vec<&str>> = content.lines().map(|l| l.split('\t').collect()).collect();
    assert!(!lines.is_empty(), "trace file was empty");
    for fields in &lines {
        assert_eq!(fields.len(), 3, "not 3 tab-separated fields: {fields:?}");
        assert!(
            looks_like_iso8601_utc(fields[0]),
            "not a parseable ISO-8601 UTC timestamp: {fields:?}"
        );
    }
    assert!(
        lines
            .iter()
            .any(|f| f[1] == "session_exited" && f[2] == initial_name),
        "no \"session_exited\" trace line for {initial_name:?}:\n{content}"
    );
    assert!(
        lines
            .iter()
            .any(|f| f[1] == "relaunching" && f[2] == initial_name),
        "no \"relaunching\" trace line for {initial_name:?}:\n{content}"
    );

    drop(daemon);
    let ps = std::process::Command::new("ps")
        .args(["-eo", "pid,args"])
        .output()
        .expect("ps");
    let ps_out = String::from_utf8_lossy(&ps.stdout);
    let leaked: Vec<&str> = ps_out.lines().filter(|l| l.contains(&token_str)).collect();
    assert!(
        leaked.is_empty(),
        "orphan process(es) still reference this test's own token path after teardown: {leaked:?}"
    );
}

/// Sibling of `butler_watchdog_records_session_exit_and_relaunch_in_a_trace_file`,
/// but proving the DEFAULT-fallback branch itself: `remuda._butler_session_trace_path`
/// is never set here, so this only passes if `_butler_session_trace` in
/// `packages/butler/init.lua` actually falls back to
/// `$HOME/.config/remuda/session-trace.log`, the same default-path idiom
/// `_butler_trace` (compaction) already uses. Same `Daemon::spawn_with_home`
/// HOME-redirection precedent as
/// `butler_exec_finds_credentials_at_the_conventional_path_with_zero_env_vars`,
/// so this never touches a real machine's real `~/.config/remuda/`.
#[test]
#[cfg(unix)]
fn butler_session_trace_defaults_to_the_conventional_path_with_no_seam_set() {
    let dir = scratch_dir("butler-session-trace-default");
    let home = dir.join("home");
    let butler_dir = home.join(".config/remuda/butler");
    std::fs::create_dir_all(&butler_dir).expect("mkdir conventional butler dir");
    std::fs::write(butler_dir.join("token"), "test-token\n").expect("write token");
    std::fs::write(
        butler_dir.join("config"),
        "http://127.0.0.1:1\n!room:example.org\n@butler:example.org\n\n",
    )
    .expect("write config");

    // Negative control: the default-derived trace file must not exist before
    // any session close is ever triggered.
    let trace_path = home.join(".config/remuda/session-trace.log");
    assert!(
        !trace_path.exists(),
        "default session trace file already exists before any session closed: {trace_path:?}"
    );

    // `remuda._butler_session_trace_path` is deliberately left unset here --
    // exercising the real default-fallback branch, not the test seam
    // `butler_watchdog_records_session_exit_and_relaunch_in_a_trace_file`
    // already covers.
    let daemon = Daemon::spawn_with_home(&dir, &home);
    let path = daemon::socket_path_in(&dir, "s");

    // Same observer group as the sibling watchdog trace test -- `init.lua`
    // clears the "butler" group on every `exec`, so a hook registered there
    // would not survive it.
    eval(
        &path,
        r#"
            remuda._watchdog_exits = {}
            remuda.on("session_exited", function(name)
                table.insert(remuda._watchdog_exits, name)
            end, { group = "test-observer" })
        "#,
    );
    eval(
        &path,
        r#"remuda._butler_argv = {"sh", "-c", "sleep 0.3; exit 0"}"#,
    );
    eval(&path, "remuda._butler_skip_relay = true");

    let out = remuda_timed(&dir, &["-s", "s", "exec", "butler"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let initial_name = eval(&path, "return remuda._butler_initial_name");

    let deadline = Instant::now() + PATIENCE;
    loop {
        let seen = eval(&path, "return table.concat(remuda._watchdog_exits, ',')");
        let count = seen.split(',').filter(|n| *n == initial_name).count();
        if count >= 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "expected at least 2 session_exited events for {initial_name:?}, saw: {seen:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Witness discipline: read the ACTUAL bytes back from disk, from the
    // DEFAULT-derived path, never a seam-supplied one.
    let content = std::fs::read_to_string(&trace_path)
        .unwrap_or_else(|e| panic!("read default trace file {trace_path:?}: {e}"));
    let lines: Vec<Vec<&str>> = content.lines().map(|l| l.split('\t').collect()).collect();
    assert!(!lines.is_empty(), "default trace file was empty");
    for fields in &lines {
        assert_eq!(fields.len(), 3, "not 3 tab-separated fields: {fields:?}");
        assert!(
            looks_like_iso8601_utc(fields[0]),
            "not a parseable ISO-8601 UTC timestamp: {fields:?}"
        );
    }
    assert!(
        lines
            .iter()
            .any(|f| f[1] == "session_exited" && f[2] == initial_name),
        "no \"session_exited\" trace line for {initial_name:?}:\n{content}"
    );
    assert!(
        lines
            .iter()
            .any(|f| f[1] == "relaunching" && f[2] == initial_name),
        "no \"relaunching\" trace line for {initial_name:?}:\n{content}"
    );

    drop(daemon);
}

/// This documents the case where NO persistence layer is installed: `remuda`
/// itself never re-execs butler on its own, restart or not — the only way
/// `packages/butler/init.lua`'s real code ever runs is an explicit `remuda
/// exec butler`. That stays true regardless of `docs/install-butler.sh`,
/// because the systemd timer / launchd agent it installs lives entirely
/// outside this binary; this test's daemon has none of that wired in, so it
/// still shows the session does not come back unassisted. It is not proof
/// that persistence is unsolved in general — see the sibling test below,
/// `a_supervisor_polling_remuda_ls_can_relaunch_butler_after_a_restart`, for
/// the positive leg with an external poller in the loop. If this test ever
/// fails with nothing installed, that's a real capability change in `remuda`
/// itself and the L6 cell's DoD must be revisited, not this assertion
/// loosened. Separately: this test is also the negative-control leg for
/// `remuda ls`'s exact-match liveness check -- the generated `butler-poll.sh`,
/// `install-butler.sh`'s install-time probe, and the sibling positive-leg
/// test below all depend on it staying true.
#[test]
#[cfg(unix)]
fn a_daemon_restart_does_not_relaunch_the_butler_session() {
    let dir = scratch_dir("butler-restart-ceiling");
    let (token_path, config_path) = butler_config(
        &dir,
        "restart-ceiling",
        "http://127.0.0.1:1",
        "!room:example.org",
        "@butler:example.org",
        "",
    );
    let token_str = token_path.to_string_lossy().to_string();
    let config_str = config_path.to_string_lossy().to_string();
    let mut daemon = Daemon::spawn_with_env(
        &dir,
        &[
            ("REMUDA_BUTLER_TOKEN", token_str.as_str()),
            ("REMUDA_BUTLER_CONFIG", config_str.as_str()),
        ],
    );
    let path = daemon::socket_path_in(&dir, "s");

    eval(
        &path,
        r#"remuda._butler_argv = {"sh", "-c", "sleep 0.3; exit 0"}"#,
    );
    eval(&path, "remuda._butler_skip_relay = true");
    let out = remuda_timed(&dir, &["-s", "s", "exec", "butler"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let initial_name = eval(&path, "return remuda._butler_initial_name");

    // Confirm the watchdog is genuinely live before the restart -- otherwise
    // "it's not there after" would prove nothing. `list` names the current
    // session under its (possibly-respawned) butler name.
    let deadline = Instant::now() + PATIENCE;
    loop {
        let listed = remuda(&dir, &["-s", "s", "ls"]);
        if String::from_utf8_lossy(&listed.stdout).contains(&initial_name) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the butler session never showed up in `ls` before the restart"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Same restart path as `restart_stops_a_daemon_and_leaves_the_next_command_free_to_start_one`,
    // with `-f`: the live butler session makes a bare `restart` refuse (see
    // `restart_refuses_to_kill_a_live_session_without_being_told_twice`).
    let out = remuda(&dir, &["-s", "s", "restart", "-f"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        daemon.left_on_its_own(),
        "the old daemon did not exit on its own"
    );

    // A fresh daemon against the same runtime dir/socket -- the same
    // credentials are still on disk and still exported into this new
    // process's own environment, so if anything anywhere auto-relaunched
    // butler, this daemon has everything it would need to do so.
    let _daemon2 = Daemon::spawn_with_env(
        &dir,
        &[
            ("REMUDA_BUTLER_TOKEN", token_str.as_str()),
            ("REMUDA_BUTLER_CONFIG", config_str.as_str()),
        ],
    );

    // Wait through a couple of TICK_PERIODs (1s each, see daemon.rs) to rule
    // out some delayed auto-recovery this comment doesn't know about, not
    // just an instant-after check.
    std::thread::sleep(Duration::from_millis(2500));
    let listed = remuda(&dir, &["-s", "s", "ls"]);
    let listed_out = String::from_utf8_lossy(&listed.stdout);
    assert!(
        !listed_out.contains(&initial_name),
        "the butler session came back after a restart with no `exec butler` -- \
         this is a real capability change; see this test's own doc comment: {listed_out}"
    );
}

/// The positive leg: nothing in `remuda` itself auto-relaunches butler (the
/// test above), but the mechanism an external supervisor would drive --
/// poll `remuda ls` for the exact session name, and if it is absent re-run
/// `remuda exec butler` -- already works today with no production-code
/// change. This pins that fact down so it can't silently regress.
///
/// Shared-instrument note: this test and the ceiling test above both use
/// "does a session named exactly `butler` exist" as ground truth. That is
/// only a safe foundation *because* the ceiling test exists as a negative
/// control -- if `remuda ls` ever reported the name present when the
/// process structurally is not, this test would go green for the wrong
/// reason, and a real supervisor built on the same check would silently
/// never relaunch. Don't delete or loosen either test in isolation.
#[test]
#[cfg(unix)]
fn a_supervisor_polling_remuda_ls_can_relaunch_butler_after_a_restart() {
    let dir = scratch_dir("butler-restart-relaunch");
    let (token_path, config_path) = butler_config(
        &dir,
        "restart-relaunch",
        "http://127.0.0.1:1",
        "!room:example.org",
        "@butler:example.org",
        "",
    );
    let token_str = token_path.to_string_lossy().to_string();
    let config_str = config_path.to_string_lossy().to_string();
    let mut daemon = Daemon::spawn_with_env(
        &dir,
        &[
            ("REMUDA_BUTLER_TOKEN", token_str.as_str()),
            ("REMUDA_BUTLER_CONFIG", config_str.as_str()),
        ],
    );
    let path = daemon::socket_path_in(&dir, "s");

    eval(
        &path,
        r#"remuda._butler_argv = {"sh", "-c", "sleep 0.3; exit 0"}"#,
    );
    eval(&path, "remuda._butler_skip_relay = true");
    let out = remuda_timed(&dir, &["-s", "s", "exec", "butler"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let initial_name = eval(&path, "return remuda._butler_initial_name");

    let deadline = Instant::now() + PATIENCE;
    loop {
        let listed = remuda(&dir, &["-s", "s", "ls"]);
        if String::from_utf8_lossy(&listed.stdout).contains(&initial_name) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the butler session never showed up in `ls` before the restart"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let out = remuda(&dir, &["-s", "s", "restart", "-f"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        daemon.left_on_its_own(),
        "the old daemon did not exit on its own"
    );

    // A fresh daemon, same credentials on disk and still exported into this
    // new process's env -- the same shape the systemd/launchd unit's daemon
    // auto-start would produce after a crash.
    let _daemon2 = Daemon::spawn_with_env(
        &dir,
        &[
            ("REMUDA_BUTLER_TOKEN", token_str.as_str()),
            ("REMUDA_BUTLER_CONFIG", config_str.as_str()),
        ],
    );

    // Same test-mode wiring as the first `exec butler` above -- a real
    // supervisor's poll would start a genuine Claude Code session, which
    // this harness cannot exercise; `_butler_argv`/`_butler_skip_relay` swap
    // in a `sleep` in its place while leaving every other line of
    // `init.lua` (the actual relaunch mechanism under test) untouched.
    eval(
        &path,
        r#"remuda._butler_argv = {"sh", "-c", "sleep 0.3; exit 0"}"#,
    );
    eval(&path, "remuda._butler_skip_relay = true");

    // This is exactly what the supervisor's poll runs after finding the name
    // absent from `remuda ls`: re-`exec butler`. `os.getenv("PWD")` inside
    // `init.lua` reads the *daemon's* own env (the eval request carries only
    // source text, not this CLI call's env), and `daemon2` above already
    // inherited the same ambient `PWD` as the first daemon -- so this call
    // reproduces the same session name without needing to touch `PWD` here.
    // A real installer still unsets `PWD` on its own `exec butler` call
    // (`docs/install-butler.sh`), because in the wild that call may be the
    // one lazily auto-starting a dead daemon from scratch (`with_daemon`),
    // which is a different `PWD` than this test's pre-spawned `daemon2`.
    let out = remuda_timed(&dir, &["-s", "s", "exec", "butler"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let deadline = Instant::now() + PATIENCE;
    loop {
        let listed = remuda(&dir, &["-s", "s", "ls"]);
        if String::from_utf8_lossy(&listed.stdout).contains(&initial_name) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the re-`exec butler` call never produced a session named {initial_name:?} after restart"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The actual bug this suite exists to catch: a daemon born with ZERO
/// butler env vars (the exact poisoning scenario -- some unrelated `remuda`
/// command lazily birthing a daemon before butler's own installer/env ever
/// gets a chance to run) must still be able to register butler, as long as
/// the token/config files sit at `init.lua`'s conventional default path
/// under `HOME`. Every other test in this file that reaches past
/// `_butler_test_mode` sets `REMUDA_BUTLER_TOKEN`/`REMUDA_BUTLER_CONFIG`
/// explicitly via `Daemon::spawn_with_env` -- none of them cover this path.
#[test]
#[cfg(unix)]
fn butler_exec_finds_credentials_at_the_conventional_path_with_zero_env_vars() {
    let dir = scratch_dir("butler-conventional");
    let home = dir.join("home");
    let butler_dir = home.join(".config/remuda/butler");
    std::fs::create_dir_all(&butler_dir).expect("mkdir conventional butler dir");
    std::fs::write(butler_dir.join("token"), "test-token\n").expect("write token");
    std::fs::write(
        butler_dir.join("config"),
        "http://127.0.0.1:1\n!room:example.org\n@butler:example.org\n\n",
    )
    .expect("write config");

    let daemon = Daemon::spawn_with_home(&dir, &home);
    let path = daemon::socket_path_in(&dir, "s");

    // Same test-mode substitution as the watchdog/restart tests: a short-
    // lived real process stands in for `claude`, and the relay (which would
    // otherwise retry forever against these dummy credentials) is skipped.
    eval(
        &path,
        r#"remuda._butler_argv = {"sh", "-c", "sleep 0.3; exit 0"}"#,
    );
    eval(&path, "remuda._butler_skip_relay = true");

    let out = remuda_timed(&dir, &["-s", "s", "exec", "butler"]);
    assert!(
        out.status.success(),
        "expected exec butler to succeed using only the conventional HOME-based \
         path, with no REMUDA_BUTLER_TOKEN/CONFIG at all -- stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let initial_name = eval(&path, "return remuda._butler_initial_name");
    let deadline = Instant::now() + PATIENCE;
    loop {
        let listed = remuda(&dir, &["-s", "s", "ls"]);
        if String::from_utf8_lossy(&listed.stdout).contains(&initial_name) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the butler session never showed up in `ls`"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    drop(daemon);
}

/// The other half of the same fix: when neither the conventional file nor
/// the env override exists, this must fail loudly and immediately -- never
/// silently proceed with a bad path, and never get partway into writing the
/// `.mcp.json` companion file `init.lua` builds right after resolving
/// `config_path`.
#[test]
#[cfg(unix)]
fn butler_exec_fails_loudly_with_no_token_file_and_no_env_override() {
    let dir = scratch_dir("butler-no-creds");
    let home = dir.join("home-empty");
    std::fs::create_dir_all(&home).expect("mkdir empty home");

    let daemon = Daemon::spawn_with_home(&dir, &home);

    let out = remuda_timed(&dir, &["-s", "s", "exec", "butler"]);
    assert!(
        !out.status.success(),
        "expected exec butler to fail with no token file and no env override"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let expected_path = home
        .join(".config/remuda/butler/token")
        .to_string_lossy()
        .to_string();
    assert!(
        stderr.contains("no token file at") && stderr.contains(&expected_path),
        "expected the error to name the exact conventional path it tried: {stderr:?}"
    );
    assert!(
        stderr.contains("REMUDA_BUTLER_TOKEN"),
        "expected the error to mention the override var: {stderr:?}"
    );

    // Never got far enough to write the `.mcp.json` companion file -- proof
    // this failed before doing anything else, not partway through.
    let mcp_json = home.join(".config/remuda/butler/config.mcp.json");
    assert!(
        !mcp_json.exists(),
        "exec butler wrote {mcp_json:?} despite failing to resolve the token file first"
    );

    drop(daemon);
}

/// The whole point of this step (steps/035): a `~/.config/remuda/init.lua`
/// present before the daemon exists at all is evaluated automatically --
/// with no human hand and no `remuda exec`/`eval` call anywhere in THIS
/// test -- every time a FRESH daemon boots, mirroring how Neovim/Hammerspoon
/// /WezTerm auto-load a user init file. The instrument is `remuda ls`'s
/// session list, never "the loader ran without erroring".
///
/// The `_butler_argv`/`_butler_skip_relay` test substitutions that every
/// other butler test injects via a separate `eval` *before* calling `exec
/// butler` are, here, baked into the auto-loaded file itself -- so it is the
/// DAEMON's own boot-time load that performs the substitution and the exec,
/// never this test. Real `docs/install-butler.sh` output has no such
/// substitutions (it can't know about `claude` not existing on a test box);
/// this is the same mechanism with a stand-in child process, same as every
/// other butler test that can't spawn a real `claude`.
///
/// The restart leg is the DoD's own wording: after killing and restarting
/// the daemon, with no human hand, `remuda ls` must show butler AGAIN, from
/// the SAME home dir with the SAME (unchanged) config file -- proving the
/// mechanism survives the exact scenario it exists for, not just a first
/// boot.
#[test]
#[cfg(unix)]
fn a_fresh_daemon_auto_loads_the_user_config_and_registers_butler_with_no_human_hand() {
    let dir = scratch_dir("boot-loader-positive");
    let home = dir.join("home");
    let butler_dir = home.join(".config/remuda/butler");
    std::fs::create_dir_all(&butler_dir).expect("mkdir conventional butler dir");
    std::fs::write(butler_dir.join("token"), "test-token\n").expect("write token");
    std::fs::write(
        butler_dir.join("config"),
        "http://127.0.0.1:1\n!room:example.org\n@butler:example.org\n\n",
    )
    .expect("write config");
    std::fs::create_dir_all(home.join(".config/remuda")).expect("mkdir config dir");
    std::fs::write(
        home.join(".config/remuda/init.lua"),
        "remuda._butler_argv = {\"sh\", \"-c\", \"sleep 0.3; exit 0\"}\n\
         remuda._butler_skip_relay = true\n\
         remuda.exec(\"butler\")\n",
    )
    .expect("write init.lua");

    let mut daemon = Daemon::spawn_with_home(&dir, &home);
    let path = daemon::socket_path_in(&dir, "s");

    // No `exec butler` and no `eval` calling `remuda.exec` anywhere in this
    // test -- if butler shows up, the daemon's own boot-time loader did it.
    let deadline = Instant::now() + PATIENCE;
    let initial_name = loop {
        let name = eval(&path, "return remuda._butler_initial_name or ''");
        if !name.is_empty() {
            let listed = remuda(&dir, &["-s", "s", "ls"]);
            if String::from_utf8_lossy(&listed.stdout).contains(&name) {
                break name;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the daemon never auto-registered a butler session from its own \
             boot-time config load"
        );
        std::thread::sleep(Duration::from_millis(50));
    };

    // Restart leg. `-f`: a live butler session makes a bare `restart` refuse
    // (see `restart_refuses_to_kill_a_live_session_without_being_told_twice`).
    let out = remuda(&dir, &["-s", "s", "restart", "-f"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        daemon.left_on_its_own(),
        "the old daemon did not exit on its own"
    );

    // A fresh daemon, same home dir, config file untouched on disk. Again:
    // no `exec butler` call anywhere in this test.
    let _daemon2 = Daemon::spawn_with_home(&dir, &home);
    let deadline = Instant::now() + PATIENCE;
    loop {
        let listed = remuda(&dir, &["-s", "s", "ls"]);
        if String::from_utf8_lossy(&listed.stdout).contains(&initial_name) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "after killing and restarting the daemon, remuda ls never showed \
             butler again -- the boot-time loader must run on EVERY fresh \
             daemon, not just the first"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Negative control for the whole mechanism above (steps/035's own DoD
/// wording: "with the feature turned off, the same procedure must go RED —
/// if it does not go red, the instrument is not measuring the feature").
/// Absence of `~/.config/remuda/init.lua` IS "feature off" here, the same
/// way a vanilla Neovim/Hammerspoon install with no config is: valid
/// credentials sit at the conventional butler path (same fixture as the
/// positive test above), but with no init.lua to call `remuda.exec("butler")`
/// a fresh daemon must never show a butler session. Without this test, the
/// positive test above could pass for the wrong reason (e.g. `remuda ls`
/// always reporting butler present regardless of what actually ran).
#[test]
#[cfg(unix)]
fn a_fresh_daemon_with_no_user_config_never_auto_registers_butler() {
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

    // A couple of TICK_PERIODs' worth of margin (same as
    // `a_daemon_restart_does_not_relaunch_the_butler_session`'s own wait) to
    // rule out a delayed load, not just an instant-after check.
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
