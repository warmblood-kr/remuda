//! The MVP's second completion criterion, against real processes on real ptys:
//! *stand up a set of sessions, attach and detach, and have instructions land.*
//!
//! Unlike the core suite these tests spawn a shell, so they live in the host
//! crate. That split is the point: `remuda-core`'s invariants stay provable on
//! a machine with no pty at all.

use remuda_core::{Session, Size};
use remuda_native::{CommandBuilder, PtyAgent, SystemClock};
use std::sync::Arc;
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(10);

fn shell() -> CommandBuilder {
    let mut cmd = CommandBuilder::new("sh");
    cmd.env("PS1", "$ ");
    cmd
}

fn session(name: &str) -> Session {
    let agent = PtyAgent::spawn(shell(), Size::new(80, 24)).expect("spawn on a pty");
    Session::new(name, Box::new(agent), Arc::new(SystemClock::new()))
}

/// Poll the screen until `needle` appears. A pty is asynchronous; the
/// alternative to polling is a fixed sleep, which is slower AND flakier.
fn wait_for(session: &Session, needle: &str) -> String {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let screen = session.screen_text().expect("read screen");
        if screen.contains(needle) {
            return screen;
        }
        assert!(
            Instant::now() < deadline,
            "{needle:?} never appeared. screen was:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn an_instruction_reaches_a_live_process_and_its_output_comes_back() {
    let session = session("solo");
    session.send_line("echo $((6*7))-landed").expect("send");
    wait_for(&session, "42-landed");
    assert!(session.is_alive());
}

/// COMPLETION CRITERION (2): three sessions up at once, each one addressed
/// separately, no instruction landing in the wrong terminal.
///
/// The failure this guards is not hypothetical — a single stray submit landing
/// on another session's screen is a defect the Emacs implementation actually
/// had.
#[test]
fn three_live_sessions_stay_separate() {
    let sessions: Vec<Session> = ["one", "two", "three"].iter().map(|n| session(n)).collect();

    for (i, s) in sessions.iter().enumerate() {
        s.send_line(&format!("echo $((10+{i}))-marker"))
            .expect("send");
    }

    for (i, s) in sessions.iter().enumerate() {
        let screen = wait_for(s, &format!("{}-marker", 10 + i));
        for other in 0..sessions.len() {
            if other != i {
                assert!(
                    !screen.contains(&format!("{}-marker", 10 + other)),
                    "session {i} shows session {other}'s instruction:\n{screen}"
                );
            }
        }
    }
}

/// "Attach" is reading the screen; "detach" is stopping. Neither disturbs the
/// process, and the session keeps accepting work afterwards — which is what
/// makes a viewer safe to come and go.
#[test]
fn attaching_and_detaching_does_not_disturb_the_agent() {
    let session = session("attachable");
    session.send_line("echo $((1+1))-before").expect("send");
    wait_for(&session, "2-before");

    for _ in 0..25 {
        let _ = session.screen_text().expect("read while attached");
        let _ = session.cursor().expect("cursor while attached");
    }

    session.send_line("echo $((3+4))-after").expect("send");
    wait_for(&session, "7-after");
    assert!(session.is_alive(), "the agent survived attach/detach");
}

/// The pty is opened at the size given and nothing can change it — `Session`
/// has no resize method to call. Attaching used to shrink a shared session to
/// the smallest connected client; here it cannot.
#[test]
fn the_terminal_keeps_the_size_it_was_spawned_with() {
    let session = session("sized");
    assert_eq!(session.size(), Size::new(80, 24));

    // `stty` is coreutils and present everywhere; `tput` needs ncurses, which
    // the CI runner does not have — it failed there, correctly, once the
    // `|| echo 80` fallback was removed. A fallback would have made this test
    // measure the fallback instead of the pty.
    session
        .send_line("echo w=$(stty size | cut -d' ' -f2)")
        .expect("send");
    let screen = wait_for(&session, "w=8");
    assert!(
        screen.contains("w=80"),
        "the pty reported a different width than we asked for:\n{screen}"
    );
}

/// A cursor that never moves would silently break ghost-text detection, which
/// distinguishes real typed input from autocomplete by column.
#[test]
fn the_cursor_reports_a_real_position() {
    let session = session("cursor");
    session.send_line("echo $((8*8))-probe").expect("send");
    wait_for(&session, "64-probe");

    let cursor = session.cursor().expect("cursor");
    assert!(
        cursor.row < 24 && cursor.col < 80,
        "cursor off-screen: {cursor:?}"
    );
    assert!(
        cursor.row > 0,
        "cursor never left the first row: {cursor:?}"
    );
}

#[test]
fn a_dead_process_reports_exited_rather_than_swallowing_input() {
    let session = session("doomed");
    session.send_line("exit").expect("send exit");

    let deadline = Instant::now() + PATIENCE;
    while session.is_alive() {
        assert!(Instant::now() < deadline, "the shell never exited");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(session.send_line("anyone there").is_err());
}

// ---------------------------------------------------------------------------
// Attaching, as a human would: raw keystrokes in, a live byte stream out.
//
// The polling path above is what a machine uses. A person at a terminal cannot
// poll — they need the bytes as the program emits them, and a repaint of what
// is already on screen the moment they arrive.
// ---------------------------------------------------------------------------

#[test]
fn an_attached_viewer_gets_a_repaint_and_then_a_live_stream() {
    let session = session("attachable");

    // Something is already on screen before anyone attaches — the case a
    // repaint exists for. A viewer that only streams would see none of this.
    session.send_line("echo $((11*11))-before").expect("send");
    wait_for(&session, "121-before");

    let held = session.attach().expect("attach");

    let painted = held.screen_bytes().expect("repaint");
    let painted = String::from_utf8_lossy(&painted);
    assert!(
        painted.contains("121-before"),
        "attaching must paint what is already there. got:\n{painted}"
    );

    let stream = held.subscribe().expect("a real pty can stream");

    // Type it the way a human does: the keystrokes, then their own Enter.
    held.write_raw(b"echo $((6*7))-live").expect("keystrokes");
    held.write_raw(b"\r").expect("the human's own Enter");

    // The answer cannot appear in the echo of the question, so matching it
    // proves the shell ran rather than proving the pty echoed.
    let deadline = Instant::now() + PATIENCE;
    let mut seen = String::new();
    while !seen.contains("42-live") {
        assert!(
            Instant::now() < deadline,
            "streamed bytes never carried the result. saw:\n{seen}"
        );
        let chunk = stream
            .recv_timeout(Duration::from_millis(500))
            .expect("stream stays open while the process lives");
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
}

#[test]
fn detaching_leaves_the_agent_untouched_and_the_core_back_in_charge() {
    let session = session("handover");
    let held = session.attach().expect("attach");
    held.write_raw(b"echo $((5*5))-typed\r")
        .expect("keystrokes");
    wait_for(&session, "25-typed");

    // While held, the core is refused — and the refusal must not have leaked
    // any bytes into the pty.
    assert!(session.send_line("echo LEAKED").is_err());

    drop(held);
    session
        .send_line("echo $((9*9))-after")
        .expect("the core resumes on detach");
    let screen = wait_for(&session, "81-after");
    assert!(
        !screen.contains("LEAKED"),
        "a refused instruction must never reach the process. screen:\n{screen}"
    );
}
