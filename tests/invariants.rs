//! The invariants the design doc says must hold, exercised against a scripted
//! agent. Nothing here spawns a process, opens a pty, or touches the network:
//! this suite is the MVP's first completion criterion, "the whole suite runs
//! on a machine that has never installed `claude`."

use remuda::agent::{AgentError, AgentProcess, Cursor, Result, Size};
use remuda::{Clock, ManualClock, ScriptedAgent, Session};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// An agent that reports every `write` into a buffer the test still owns after
/// the session has taken the agent.
///
/// Calls are kept as separate entries rather than concatenated: whether two
/// writers interleaved is exactly the information a flattened buffer destroys.
struct RecordingAgent {
    writes: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl RecordingAgent {
    fn new(writes: Arc<Mutex<Vec<Vec<u8>>>>) -> Self {
        Self { writes }
    }
}

impl AgentProcess for RecordingAgent {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.writes.lock().unwrap().push(bytes.to_vec());
        Ok(())
    }
    fn screen_text(&mut self) -> Result<String> {
        Ok(String::new())
    }
    fn cursor(&mut self) -> Result<Cursor> {
        Ok(Cursor { row: 0, col: 0 })
    }
    fn is_alive(&mut self) -> bool {
        true
    }
    fn size(&self) -> Size {
        Size::default()
    }
}

fn session_with(agent: Box<dyn AgentProcess>) -> (Session, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new());
    let session = Session::new("test", agent, clock.clone());
    (session, clock)
}

/// True when every body write is immediately followed by its Enter — i.e. no
/// second writer got between them.
fn every_body_is_followed_by_enter(writes: &[Vec<u8>]) -> bool {
    let mut i = 0;
    while i < writes.len() {
        if writes[i] == b"\r" {
            // An Enter with no body before it means a pair was broken apart.
            return false;
        }
        match writes.get(i + 1) {
            Some(next) if next == b"\r" => i += 2,
            _ => return false,
        }
    }
    true
}

#[test]
fn send_line_writes_the_body_then_enter() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let (session, _clock) = session_with(Box::new(RecordingAgent::new(writes.clone())));

    session.send_line("hello").unwrap();

    let got = writes.lock().unwrap().clone();
    assert_eq!(got, vec![b"hello".to_vec(), b"\r".to_vec()]);
}

#[test]
fn concurrent_send_lines_never_interleave() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let (session, _clock) = session_with(Box::new(RecordingAgent::new(writes.clone())));
    let session = Arc::new(session);

    let mut handles = Vec::new();
    for i in 0..16 {
        let s = session.clone();
        handles.push(std::thread::spawn(move || {
            s.send_line(&format!("line-{i}")).unwrap();
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let got = writes.lock().unwrap().clone();
    assert_eq!(got.len(), 32, "16 sends should produce 16 body+enter pairs");
    assert!(
        every_body_is_followed_by_enter(&got),
        "a body write was separated from its Enter: {got:?}"
    );
}

/// NEGATIVE CONTROL for the test above.
///
/// The interleaving assertion is only evidence if it can actually go red. Here
/// writers share an agent with *no* session lock spanning their two writes —
/// the exact shape `Session::send_line` exists to prevent. If this ever stops
/// detecting interleaving, the positive test above has stopped meaning
/// anything.
///
/// A `Barrier`, not a sleep or a yield: an earlier draft of this control used
/// `yield_now()` and **passed**, because the threads happened to finish their
/// pairs before being descheduled. A control that only fires when the
/// scheduler cooperates is not a control. Every thread now writes its body,
/// waits for all the others, and only then writes its Enter, so interleaving
/// is guaranteed by construction rather than by timing.
#[test]
fn control_unlocked_writers_do_interleave() {
    use std::sync::Barrier;

    const WRITERS: usize = 8;
    let writes = Arc::new(Mutex::new(Vec::new()));
    let agent = Arc::new(Mutex::new(RecordingAgent::new(writes.clone())));
    let barrier = Arc::new(Barrier::new(WRITERS));

    let mut handles = Vec::new();
    for i in 0..WRITERS {
        let a = agent.clone();
        let b = barrier.clone();
        handles.push(std::thread::spawn(move || {
            // Two separate lock acquisitions: the bug this design forbids.
            a.lock()
                .unwrap()
                .write(format!("line-{i}").as_bytes())
                .unwrap();
            b.wait();
            a.lock().unwrap().write(b"\r").unwrap();
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let got = writes.lock().unwrap().clone();
    assert_eq!(got.len(), WRITERS * 2);
    assert!(
        !every_body_is_followed_by_enter(&got),
        "control failed to interleave, so the positive test proves nothing"
    );
}

#[test]
fn size_is_clamped_to_the_floor_that_keeps_input_visible() {
    // 11 columns is the width that silently dropped keystrokes in the Emacs
    // implementation.
    let tiny = Size::new(11, 3);
    assert_eq!(tiny.cols(), Size::MIN_COLS);
    assert_eq!(tiny.rows(), Size::MIN_ROWS);

    let roomy = Size::new(200, 60);
    assert_eq!(roomy.cols(), 200);
    assert_eq!(roomy.rows(), 60);
}

#[test]
fn session_size_is_fixed_at_spawn() {
    let agent = ScriptedAgent::new(vec![]).with_size(Size::new(120, 40));
    let (session, _clock) = session_with(Box::new(agent));

    assert_eq!(session.size(), Size::new(120, 40));
    // There is no resize method to call. A viewer attaching cannot change
    // this value, which is the whole point of invariant 2 on Session.
}

#[test]
fn idle_time_is_driven_by_the_injected_clock() {
    let (session, clock) = session_with(Box::new(ScriptedAgent::new(vec![])));

    session.send_line("work").unwrap();
    assert_eq!(session.idle_for(), Duration::ZERO);

    clock.advance(Duration::from_secs(90));
    assert_eq!(session.idle_for(), Duration::from_secs(90));

    // Input resets it, without any real time having passed.
    session.send_line("more work").unwrap();
    assert_eq!(session.idle_for(), Duration::ZERO);
}

#[test]
fn a_dead_agent_reports_exited_rather_than_swallowing_input() {
    let mut agent = ScriptedAgent::new(vec![]);
    agent.kill();
    let (session, _clock) = session_with(Box::new(agent));

    assert!(!session.is_alive());
    match session.send_line("anyone there") {
        Err(AgentError::Exited) => {}
        other => panic!("expected Exited, got {other:?}"),
    }
}

#[test]
fn screens_are_served_in_order_then_the_last_one_repeats() {
    let agent = ScriptedAgent::new(vec!["first".into(), "second".into()]);
    let (session, _clock) = session_with(Box::new(agent));

    assert_eq!(session.screen_text().unwrap(), "first");
    assert_eq!(session.screen_text().unwrap(), "second");
    assert_eq!(session.screen_text().unwrap(), "second");
}

#[test]
fn manual_clock_only_moves_when_moved() {
    let clock = ManualClock::new();
    assert_eq!(clock.now(), Duration::ZERO);
    clock.advance(Duration::from_millis(250));
    clock.advance(Duration::from_millis(250));
    assert_eq!(clock.now(), Duration::from_millis(500));
}
