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
use remuda_native::{client, daemon};
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

    // Step 1: prove the hazard is actually there before clearing it — an
    // assertion here is what makes the next step meaningful rather than
    // assumed.
    wait_until(&socket, &name, LOCAL_PATIENCE, |screen| {
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
        &socket,
        &Request::SendLine {
            name: name.clone(),
            text: "\x1b[B".to_string(),
        },
    )
    .expect("send down arrow + enter");

    // Step 3: confirm we actually got past it, rather than assuming the send
    // above worked.
    wait_until(&socket, &name, LOCAL_PATIENCE, |screen| {
        !screen.contains("trust this folder") && !screen.contains("Accessing workspace")
    });
    // Same progressive-repaint hazard as the dialog itself: the main TUI's
    // first frame is not yet its settled one.
    std::thread::sleep(Duration::from_secs(1));

    // Step 4: a real question with an answer that cannot appear from echo
    // alone — PRINCIPLES.md §4.
    //
    // Measured: `SendLine`'s text+`\r` arrive as one write, and this TUI does
    // not reliably submit on that — a burst of printable characters
    // immediately followed by `\r` reads as an in-progress paste rather than
    // "text, then Enter." (This is the same class of hazard as the dialog's
    // progressive repaint, not a new one: input arriving before the receiver
    // is ready to act on it as a distinct event.) A second, separately-timed
    // `SendLine("")` — bare `\r` as its own write, well after the text
    // landed — is a standalone keystroke the same heuristic would not mistake
    // for paste content, and it costs nothing if the first `\r` already
    // worked (Enter on an empty, already-submitted box is a no-op here).
    client::request(
        &socket,
        &Request::SendLine {
            name: name.clone(),
            text: "What is 6 times 7? Reply with only the numeric answer, no words.".to_string(),
        },
    )
    .expect("send question");
    std::thread::sleep(Duration::from_secs(2));
    client::request(
        &socket,
        &Request::SendLine {
            name: name.clone(),
            text: String::new(),
        },
    )
    .expect("send confirming enter");

    let screen = wait_until(&socket, &name, API_PATIENCE, |screen| screen.contains("42"));
    assert!(
        screen.contains("42"),
        "the real claude session never answered 42:\n{screen}"
    );

    client::request(&socket, &Request::Close { name: name.clone() }).expect("close");
}
