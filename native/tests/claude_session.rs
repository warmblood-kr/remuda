//! A real `claude` process, not `sh`: the first interactive TUI this suite
//! drives, and the only session type that greets a fresh directory with a
//! trust dialog before anything else. Every other test in this crate spawns
//! `sh`, which is never interactive and never shows this screen — so it is
//! the one hazard those tests structurally cannot exercise.
//!
//! PRINCIPLES.md §4: a pty echoes its input, so this never waits for a
//! substring of what was typed. It waits for `"42"`, the answer to an
//! arithmetic question the model must compute — a string that cannot appear
//! merely because the bytes we sent were echoed back. (An earlier draft of
//! this test asked for the digits "42" by name in the prompt text itself —
//! it passed, for no reason at all, until the leak was removed and it went
//! red. That red run is the negative control: proof the original green was
//! spurious, not a hunch that it might have been.)
//!
//! Byte-identity is not keystroke-identity. A pty is a byte stream with no
//! keypress framing, so two writes and one write carrying the same bytes are
//! indistinguishable to whatever reads them raw — but this TUI does not read
//! raw: a burst of printable text immediately followed by `\r` in a single
//! write reads as paste-in-progress, not as "text, then a distinct Enter."
//! That cost this test twice, once per write in the sequence (clearing the
//! trust dialog, then submitting the question) — expect a third site
//! whenever the next such test adds a write this one doesn't have.
//!
//! A round trip through a real model also has a latency floor. This test's
//! own history is the cautionary example: it once passed in ~3s because the
//! assertion was spurious (see above); once that was fixed, a genuine pass
//! takes ~5-6s. A fast green on a real-API test is itself worth a second
//! look, not just a slow one.

use remuda_core::protocol::{Request, Response};
use remuda_core::Size;
use remuda_native::{client, daemon, mcp};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A real API round trip, not a local shell echo — give it room.
const API_PATIENCE: Duration = Duration::from_secs(60);
const LOCAL_PATIENCE: Duration = Duration::from_secs(10);

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("remuda-c{}-{tag}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Start a daemon and return once it actually answers, not once it was spawned.
fn daemon_at(path: &Path) -> impl Drop {
    let serving = path.to_path_buf();
    std::thread::spawn(move || {
        let _ = daemon::serve(&serving);
    });
    let deadline = Instant::now() + LOCAL_PATIENCE;
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

fn wait_until(
    path: &Path,
    name: &str,
    patience: Duration,
    mut ready: impl FnMut(&str) -> bool,
) -> String {
    let deadline = Instant::now() + patience;
    loop {
        let screen = capture(path, name);
        if ready(&screen) {
            return screen;
        }
        assert!(
            Instant::now() < deadline,
            "condition never became true within {patience:?}. last screen:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Clears the trust dialog a freshly-launched, never-trusted session shows
/// before anything else: proves it's there, sends Down+Enter, proves it's
/// gone. Shared by every test in this file that launches into a fresh cwd.
fn clear_trust_dialog(socket: &Path, name: &str) {
    // Step 1: prove the hazard is actually there before clearing it — an
    // assertion here is what makes the next step meaningful rather than
    // assumed.
    wait_until(socket, name, LOCAL_PATIENCE, |screen| {
        screen.contains("trust this folder") || screen.contains("Accessing workspace")
    });
    // The dialog renders progressively (it repaints once or twice while the
    // CLI finishes starting up); a key sent mid-repaint can land before the
    // input handler is attached and gets dropped silently. Let it settle.
    std::thread::sleep(Duration::from_secs(1));

    // Step 2: Down, then Enter — selects "Yes, I trust this folder" (the
    // default is "No, exit", so an unconditional Enter here would exit).
    // One `SendLine` (raw text plus its appended `\r`) rather than two raw
    // `Send`s: a pty is a byte stream with no keypress framing, so
    // `"\x1b[B"` + auto-appended `\r` lands identically to Down then Enter —
    // and it keeps this test entirely on the `SendLine`/`Capture` wire path
    // that is already exercised elsewhere, rather than also being the first
    // exerciser of raw `Send`'s bytes-array wire encoding.
    client::request(
        socket,
        &Request::SendLine {
            name: name.to_string(),
            text: "\x1b[B".to_string(),
        },
    )
    .expect("send down arrow + enter");

    // Step 3: confirm we actually got past it, rather than assuming the send
    // above worked.
    wait_until(socket, name, LOCAL_PATIENCE, |screen| {
        !screen.contains("trust this folder") && !screen.contains("Accessing workspace")
    });
    // Same progressive-repaint hazard as the dialog itself: the main TUI's
    // first frame is not yet its settled one.
    std::thread::sleep(Duration::from_secs(1));
}

/// Sends text, then a separately-timed confirming Enter, as two writes
/// rather than one. Measured: `SendLine`'s text+`\r` arrive as a single
/// write, and this TUI does not reliably submit on that — a burst of
/// printable characters immediately followed by `\r` reads as an
/// in-progress paste rather than "text, then Enter." (Same class of hazard
/// as the trust dialog's progressive repaint: input arriving before the
/// receiver is ready to act on it as a distinct event.) The second,
/// separately-timed `SendLine("")` — bare `\r` as its own write, well after
/// the text landed — is a standalone keystroke the same heuristic would not
/// mistake for paste content, and costs nothing if the first `\r` already
/// worked (Enter on an empty, already-submitted box is a no-op here).
fn send_and_submit(socket: &Path, name: &str, text: &str) {
    client::request(
        socket,
        &Request::SendLine {
            name: name.to_string(),
            text: text.to_string(),
        },
    )
    .expect("send text");
    std::thread::sleep(Duration::from_secs(2));
    client::request(
        socket,
        &Request::SendLine {
            name: name.to_string(),
            text: String::new(),
        },
    )
    .expect("send confirming enter");
}

#[test]
#[ignore = "needs a real, authenticated `claude` CLI on PATH — CI runners have \
            neither the binary nor Anthropic credentials, and installing \
            either into public CI is out of scope. Run locally with \
            `cargo test -p remuda-native --test claude_session -- --ignored`."]
fn typing_into_a_real_claude_session_survives_the_trust_dialog() {
    let dir = scratch("claude");
    let socket = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&socket);

    // A brand-new, never-trusted directory: the trust dialog only shows for a
    // cwd claude has never seen before.
    let session_cwd = scratch("claude-target");

    let name = "claude_e2e".to_string();
    let response = client::request(
        &socket,
        &Request::New {
            name: Some(name.clone()),
            command: vec!["claude".into()],
            size: Size::new(80, 24),
            cwd: Some(session_cwd.display().to_string()),
            env: None,
        },
    )
    .expect("New");
    assert_eq!(response, Response::Value(name.clone()));

    clear_trust_dialog(&socket, &name);

    // A real question with an answer that cannot appear from echo alone —
    // PRINCIPLES.md §4.
    send_and_submit(
        &socket,
        &name,
        "What is 6 times 7? Reply with only the numeric answer, no words.",
    );

    let screen = wait_until(&socket, &name, API_PATIENCE, |screen| screen.contains("42"));
    assert!(
        screen.contains("42"),
        "the real claude session never answered 42:\n{screen}"
    );

    client::request(&socket, &Request::Close { name: name.clone() }).expect("close");
}

/// Closes 「⒟ 그 세션이 MCP 로 remuda/butler 함수를 부른다」: a real `claude` session,
/// attached to remuda over MCP, calls a Lua-defined function through the
/// catch-all — the routing path `mcp::TOOLS` names go through never touches.
///
/// `mcp::TOOLS` inverts routing on membership: a name IN that list is
/// intercepted by the typed surface and never reaches Lua; a name ABSENT
/// falls through `remuda._call` into Lua (`native/src/mcp.rs`). Calling one
/// of the five would prove nothing about that fallthrough — so `name` below
/// is asserted absent from `TOOLS` before anything else runs, the same
/// "prove the hazard is real" discipline as the sibling test's step 1.
///
/// The tool is defined directly against the daemon (`Request::Eval`), never
/// by asking claude to define it. That isolates ⒟'s actual claim — a session
/// reaching an *already-registered* Lua tool through MCP — from "can claude
/// itself define one", a different claim `native/tests/mcp.rs`'s
/// `a_tool_defined_in_lua_is_listed_and_dispatched` already covers.
///
/// PRINCIPLES.md §4 applies to a tool call the same way it applies to an
/// answer: claude's own screen saying "I called it" is a self-report, not
/// evidence. The tool's `run` function writes a sentinel file instead, and
/// the assertion is on that file, never on the session's screen text.
///
/// Measured manually before writing this, with a throwaway daemon and a
/// disposable `claude` invocation, none of it committed:
/// - `remuda mcp` (`native/src/bin/remuda.rs`'s `mcp` verb) is a stdio
///   JSON-RPC bridge that is itself a client of the daemon over the same
///   `ipc.rs` socket this test already uses — so pointing `--mcp-config` at
///   `remuda -s <name> mcp` with `REMUDA_RUNTIME_DIR` set reaches this exact
///   daemon, no separate process needed.
/// - Attaching the server added no extra dialog: `/mcp` showed
///   "remuda · ✔ connected · N tools" immediately after the trust dialog was
///   cleared, both typed and Lua-defined tools listed together.
/// - Calling the tool DOES open a second, distinct dialog ("Tool use... Do
///   you want to proceed?") under `--permission-mode manual` and even
///   `acceptEdits` — confirming the steward's flagged risk was real, not
///   hypothetical. Its default is "1. Yes" (an unconditional Enter would
///   accept — the opposite framing from the trust dialog's "No, exit"
///   default). `--permission-mode auto` is the one mode that skips this
///   dialog entirely, so it is pinned here explicitly as a launch flag
///   rather than left to this account's own global default (which happens
///   to already be `auto`, but a differently-configured account must not
///   change what this test measures).
#[test]
#[ignore = "needs a real, authenticated `claude` CLI on PATH — CI runners have \
            neither the binary nor Anthropic credentials, and installing \
            either into public CI is out of scope. Run locally with \
            `cargo test -p remuda-native --test claude_session -- --ignored`. \
            ~15-20s: trust dialog + one real MCP tool-call round trip."]
fn a_real_claude_session_calls_a_lua_defined_tool_over_mcp() {
    let dir = scratch("mcp");
    let socket = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&socket);

    let session_cwd = scratch("mcp-target");
    let sentinel = dir.join("reached.txt");

    let name = "mark_reached_lua";
    assert!(
        !mcp::TOOLS.contains(&name),
        "{name} must be absent from the typed surface, or this test would \
         exercise the wrong routing path"
    );
    let code = format!(
        r#"remuda.tool{{
            name = "{name}",
            about = "Test-only: proves a real MCP call reached Lua by writing a file, never by claiming so on screen.",
            run = function()
                local f = io.open("{}", "w")
                f:write("reached")
                f:close()
                return "ok"
            end,
        }}"#,
        sentinel.display(),
    );
    match client::request(&socket, &Request::Eval { code, name: None }).expect("register tool") {
        Response::Value(_) => {}
        other => panic!("registering the test tool failed: {other:?}"),
    }

    let mcp_config = dir.join("mcp-config.json");
    std::fs::write(
        &mcp_config,
        format!(
            r#"{{"mcpServers":{{"remuda":{{"command":"{}","args":["-s","s","mcp"],"env":{{"REMUDA_RUNTIME_DIR":"{}"}}}}}}}}"#,
            env!("CARGO_BIN_EXE_remuda"),
            dir.display(),
        ),
    )
    .expect("write mcp config");

    let session = "mcp_e2e".to_string();
    let response = client::request(
        &socket,
        &Request::New {
            name: Some(session.clone()),
            command: vec![
                "claude".into(),
                "--mcp-config".into(),
                mcp_config.display().to_string(),
                "--strict-mcp-config".into(),
                "--permission-mode".into(),
                "auto".into(),
            ],
            size: Size::new(80, 24),
            cwd: Some(session_cwd.display().to_string()),
            env: None,
        },
    )
    .expect("New");
    assert_eq!(response, Response::Value(session.clone()));

    // Same trust dialog as the sibling test, same clear sequence.
    clear_trust_dialog(&socket, &session);

    // Same paste-vs-Enter framing hazard as the sibling test: text and its
    // submitting Enter go as two separately-timed writes.
    send_and_submit(
        &socket,
        &session,
        &format!(
            "Call the MCP tool named {name} right now with no arguments, then tell me the result."
        ),
    );

    // The out-of-band observable — never the session's own screen text.
    let deadline = Instant::now() + API_PATIENCE;
    while !sentinel.exists() {
        assert!(
            Instant::now() < deadline,
            "the sentinel file never appeared — the call never reached Lua. last screen:\n{}",
            capture(&socket, &session)
        );
        std::thread::sleep(Duration::from_millis(300));
    }
    assert_eq!(
        std::fs::read_to_string(&sentinel).expect("read sentinel"),
        "reached",
        "the sentinel file exists but was not written by this tool's run function"
    );

    client::request(&socket, &Request::Close { name: session }).expect("close");
}
