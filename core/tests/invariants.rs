//! The invariants the design doc says must hold, exercised against a scripted
//! agent. Nothing here spawns a process, opens a pty, or touches the network:
//! this suite is the MVP's first completion criterion, "the whole suite runs
//! on a machine that has never installed `claude`."

use remuda_core::agent::{AgentError, AgentProcess, Cursor, Result, Size};
use remuda_core::protocol::Step;
use remuda_core::{Clock, ManualClock, ScriptedAgent, Session};
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
        Ok(Cursor {
            row: 0,
            col: 0,
            visible: true,
        })
    }
    fn is_alive(&mut self) -> bool {
        true
    }
    fn terminate(&mut self) -> Result<()> {
        Ok(())
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

/// True when every write is a whole instruction — a body with its Enter on the
/// end, never a body alone and never a bare Enter.
///
/// This used to check that a body write was *followed by* an Enter write,
/// because `send_line` wrote the two separately under one lock. Since
/// 2026-09-10 `send_line` is one burst through `Session::send`, so the pair
/// cannot be observed apart at all and the predicate states the stronger claim
/// directly. The negative control below still fails it, which is the only
/// reason this is a strengthening rather than a loosening.
fn every_write_is_a_whole_instruction(writes: &[Vec<u8>]) -> bool {
    writes.iter().all(|w| {
        matches!(w.split_last(), Some((&b'\r', body)) if !body.is_empty() && !body.contains(&b'\r'))
    })
}

#[test]
fn send_line_writes_the_body_and_its_enter_as_one_burst() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let (session, _clock) = session_with(Box::new(RecordingAgent::new(writes.clone())));

    session.send_line("hello").unwrap();

    let got = writes.lock().unwrap().clone();
    assert_eq!(got, vec![b"hello\r".to_vec()]);
}

#[test]
fn send_appends_nothing_so_a_line_can_be_left_un_submitted() {
    // The capability 004 adds, and the footgun it admits to: a script may type
    // into a prompt and stop. Nothing invents the CR that would run it.
    let writes = Arc::new(Mutex::new(Vec::new()));
    let (session, _clock) = session_with(Box::new(RecordingAgent::new(writes.clone())));

    session.send(b"half a thought").unwrap();
    session.send(&[0x1b, b'[', b'A']).unwrap();

    let got = writes.lock().unwrap().clone();
    assert_eq!(
        got,
        vec![b"half a thought".to_vec(), vec![0x1b, b'[', b'A']],
        "send must deliver exactly the bytes given, one write per call"
    );
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
    assert_eq!(
        got.len(),
        16,
        "16 sends should produce 16 indivisible bursts"
    );
    assert!(
        every_write_is_a_whole_instruction(&got),
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
        !every_write_is_a_whole_instruction(&got),
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

// ---------------------------------------------------------------------------
// Registry — every session on this node, addressable by name.
//
// The cluster direction 정수님 set on 2026-09-10 (any node attaches a session
// hosted on any other node) makes this the local half of a distributed
// directory. These tests pin the properties that half has to have; see
// `remuda_core::registry` for why no consensus protocol is involved.
// ---------------------------------------------------------------------------

use remuda_core::Registry;
use std::sync::atomic::{AtomicBool, Ordering};

/// An agent whose liveness the test still controls after the session owns it.
struct FlagAgent {
    alive: Arc<AtomicBool>,
}

impl AgentProcess for FlagAgent {
    fn write(&mut self, _bytes: &[u8]) -> Result<()> {
        Ok(())
    }
    fn screen_text(&mut self) -> Result<String> {
        Ok(String::new())
    }
    fn cursor(&mut self) -> Result<Cursor> {
        Ok(Cursor {
            row: 0,
            col: 0,
            visible: true,
        })
    }
    fn is_alive(&mut self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
    fn terminate(&mut self) -> Result<()> {
        self.alive.store(false, Ordering::SeqCst);
        Ok(())
    }
    fn size(&self) -> Size {
        Size::default()
    }
}

fn named(name: &str, agent: Box<dyn AgentProcess>) -> Session {
    Session::new(name, agent, Arc::new(ManualClock::new()))
}

#[test]
fn a_name_collision_is_refused_and_the_first_session_survives() {
    let registry = Registry::new();
    let first_writes = Arc::new(Mutex::new(Vec::new()));

    registry
        .register(named(
            "worker",
            Box::new(RecordingAgent::new(first_writes.clone())),
        ))
        .expect("first registration");

    let second_writes = Arc::new(Mutex::new(Vec::new()));
    let rejected = registry.register(named(
        "worker",
        Box::new(RecordingAgent::new(second_writes.clone())),
    ));

    // Returned unregistered, not swallowed: the caller still holds it and can
    // rename or drop it deliberately.
    assert!(rejected.is_err(), "a taken name must be refused");

    // And "worker" is still the FIRST session, not a replacement. Silently
    // replacing would leave a live pty with no handle to reach it while the
    // caller saw success — the whole reason register returns a Result.
    registry
        .send_line("worker", "still me")
        .expect("session present")
        .expect("write ok");
    assert_eq!(
        first_writes.lock().unwrap().len(),
        1,
        "one indivisible burst"
    );
    assert!(
        second_writes.lock().unwrap().is_empty(),
        "the rejected session must never have been wired up"
    );
}

#[test]
fn many_handles_drive_one_session() {
    // Attaching is meant to be routine, so a viewer's handle and the core's
    // handle must be the same session — not two views that can diverge.
    let registry = Registry::new();
    let writes = Arc::new(Mutex::new(Vec::new()));
    registry
        .register(named(
            "worker",
            Box::new(RecordingAgent::new(writes.clone())),
        ))
        .expect("registration");

    let viewer = registry.get("worker").expect("handle for the viewer");
    let core = registry.get("worker").expect("handle for the core");
    assert!(Arc::ptr_eq(&viewer, &core), "one session, not two");

    viewer.send_line("from the viewer").expect("viewer write");
    core.send_line("from the core").expect("core write");
    assert_eq!(writes.lock().unwrap().len(), 2, "one burst per send");
}

#[test]
fn listing_is_a_sorted_owned_snapshot() {
    let registry = Registry::new();
    for name in ["charlie", "alpha", "bravo"] {
        registry
            .register(named(name, Box::new(ScriptedAgent::new(vec![]))))
            .expect("registration");
    }

    let names: Vec<String> = registry.list().into_iter().map(|s| s.name).collect();
    assert_eq!(names, ["alpha", "bravo", "charlie"], "sorted by name");

    // The snapshot outlives the lock and borrows nothing, which is what lets it
    // cross a process — or a machine — boundary later.
    let snapshot = registry.list();
    registry.remove("alpha");
    assert_eq!(
        snapshot.len(),
        3,
        "a taken snapshot does not change under us"
    );
    assert_eq!(registry.list().len(), 2, "but the registry did");
}

#[test]
fn reap_drops_the_dead_and_keeps_the_living() {
    let registry = Registry::new();
    let doomed = Arc::new(AtomicBool::new(true));
    registry
        .register(named(
            "doomed",
            Box::new(FlagAgent {
                alive: doomed.clone(),
            }),
        ))
        .expect("registration");
    registry
        .register(named(
            "healthy",
            Box::new(FlagAgent {
                alive: Arc::new(AtomicBool::new(true)),
            }),
        ))
        .expect("registration");

    // Negative control: nothing has died, so reaping must take nothing. Without
    // this, a reap that removes everything passes the assertion below.
    assert!(registry.reap().is_empty(), "nothing dead yet");
    assert_eq!(registry.list().len(), 2);

    doomed.store(false, Ordering::SeqCst);
    assert_eq!(registry.reap(), ["doomed"], "only the dead one");
    assert_eq!(registry.list().len(), 1);
    assert!(registry.get("healthy").is_some());
}

// ---------------------------------------------------------------------------
// Step 006 — ending a session.
//
// Death does not imply removal: a session whose process exited stays listed,
// screen intact, until something explicitly closes it (`close = terminate,
// then remove`). These pin that `close` does both halves, in the right order,
// and does neither when attached.
// ---------------------------------------------------------------------------

#[test]
fn close_terminates_a_live_session_and_stops_tracking_it() {
    let registry = Registry::new();
    let alive = Arc::new(AtomicBool::new(true));
    registry
        .register(named(
            "worker",
            Box::new(FlagAgent {
                alive: alive.clone(),
            }),
        ))
        .expect("registration");

    assert!(matches!(registry.close("worker"), Some(Ok(()))));
    assert!(
        !alive.load(Ordering::SeqCst),
        "the process must actually end"
    );
    assert!(registry.get("worker").is_none(), "and stop being tracked");
}

#[test]
fn close_on_an_already_dead_session_is_not_an_error() {
    // The real backend's own measured surprise (steps/006-lifetime.md Actual):
    // a naive `terminate` re-signals a pid `is_alive` has already reaped, and
    // that fails. `close` on a session that died on its own — the case 정수님
    // named directly — must still succeed, or nobody could ever clear one.
    let registry = Registry::new();
    registry
        .register(named(
            "worker",
            Box::new(FlagAgent {
                alive: Arc::new(AtomicBool::new(false)),
            }),
        ))
        .expect("registration");

    assert!(matches!(registry.close("worker"), Some(Ok(()))));
    assert!(registry.get("worker").is_none());
}

#[test]
fn close_on_an_unknown_name_is_none_not_an_error() {
    let registry = Registry::new();
    assert!(registry.close("ghost").is_none());
}

#[test]
fn close_is_refused_while_attached_and_the_session_survives() {
    let registry = Registry::new();
    let alive = Arc::new(AtomicBool::new(true));
    registry
        .register(named(
            "worker",
            Box::new(FlagAgent {
                alive: alive.clone(),
            }),
        ))
        .expect("registration");

    let session = registry.get("worker").expect("handle");
    let held = session.attach().expect("attach");

    assert!(
        matches!(registry.close("worker"), Some(Err(AgentError::Attached))),
        "tearing the pty out from under an attached human is worse than \
         making them detach first"
    );
    assert!(
        alive.load(Ordering::SeqCst),
        "a refused close must not have touched the process"
    );
    assert!(registry.get("worker").is_some(), "and the entry survives");

    drop(held);
    assert!(
        matches!(registry.close("worker"), Some(Ok(()))),
        "detaching lets close through, exactly as it does for send"
    );
}

// ---------------------------------------------------------------------------
// Invariant 3 — raw keystrokes exist only while exactly one human holds it.
//
// 정수님, 2026-09-10: a tmux-like manager where "user can select a session to
// attach". A human at a terminal types their own Enter, which needs the raw
// write invariant 1 refuses to expose. These pin the reconciliation.
// ---------------------------------------------------------------------------

#[test]
fn orchestrated_input_is_refused_while_a_human_is_attached() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let (session, _clock) = session_with(Box::new(RecordingAgent::new(writes.clone())));

    // Negative control: unattached, the core can drive normally. Without this
    // the assertion below would also pass on a session that never accepts
    // anything at all.
    session.send_line("before").expect("core drives when free");
    assert_eq!(writes.lock().unwrap().len(), 1, "one indivisible burst");

    let held = session.attach().expect("first attach");
    assert!(session.is_attached());
    assert!(
        matches!(session.send_line("during"), Err(AgentError::Attached)),
        "the core must be told, not queued behind the human"
    );
    // The raw vocabulary added in 004 goes through the same refusal. Checked
    // separately from `send_line` because a keystroke reaching a session a
    // person is driving is the original incident, and `send` is the newest and
    // easiest way to arrive there.
    assert!(
        matches!(session.send(b"\x1b[A"), Err(AgentError::Attached)),
        "raw input must be refused while a human holds the session"
    );
    assert_eq!(
        writes.lock().unwrap().len(),
        1,
        "neither refused act may have reached the pty at all"
    );

    drop(held);
    assert!(!session.is_attached());
    session
        .send_line("after")
        .expect("detach restores the core");
    assert_eq!(writes.lock().unwrap().len(), 2);
}

#[test]
fn a_second_viewer_cannot_attach() {
    let (session, _clock) = session_with(Box::new(ScriptedAgent::new(vec![])));
    let first = session.attach().expect("first attach");
    assert!(
        session.attach().is_none(),
        "two people on one keyboard is the same defect as core-plus-human"
    );
    drop(first);
    assert!(session.attach().is_some(), "detaching frees the seat");
}

#[test]
fn a_raw_keystroke_carries_no_invented_enter() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let (session, _clock) = session_with(Box::new(RecordingAgent::new(writes.clone())));
    let held = session.attach().expect("attach");

    held.write_raw(b"ls").expect("raw write");

    // Exactly the bytes typed. send_line appends CR because it delivers a whole
    // instruction; a keystroke is not an instruction, and submitting a
    // half-typed line on the human's behalf is the failure this guards.
    let recorded = writes.lock().unwrap().clone();
    assert_eq!(recorded, vec![b"ls".to_vec()], "one write, no Enter");
}

// ---------------------------------------------------------------------------
// `feed` — an input act as a sequence of bursts and pauses (PRINCIPLES.md §6,
// "Widened again, 2026-09-11"). A caller that must type, wait, then submit
// needs that whole sequence to still be one indivisible act; these pin that
// `input_lock` — not the `agent` lock a `Burst` briefly holds — is what makes
// that true even across the pause.
// ---------------------------------------------------------------------------

/// Advances `clock` by `pause` over and over, until `feeder` actually
/// finishes — never a fixed number of times.
///
/// A single `clock.advance(pause)` right after seeing the first burst can
/// race a feeder thread between that write returning and its call into
/// `Clock::sleep`: if the advance lands first, `sleep` computes its target
/// from a clock already moved, needing another full `pause` that a *fixed*
/// count of advances is never provably enough to supply — whatever count is
/// picked, a feeder scheduled late enough always reads `now()` after the last
/// one (measured: a real 60s+ hang under `cargo test --workspace`, twice,
/// after two different fixed counts). Advancing again on every iteration the
/// feeder is not yet `is_finished()` has no such bound to guess.
fn advance_until_finished<T>(
    clock: &ManualClock,
    pause: Duration,
    feeder: &std::thread::JoinHandle<T>,
) {
    for _ in 0..100_000 {
        if feeder.is_finished() {
            return;
        }
        clock.advance(pause);
        std::thread::yield_now();
    }
    panic!(
        "feed did not finish after {:?} rounds of advancing past its pause",
        100_000
    );
}

#[test]
fn feed_bursts_and_pauses_are_one_indivisible_act() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let (session, clock) = session_with(Box::new(RecordingAgent::new(writes.clone())));
    let session = Arc::new(session);
    let barrier = Arc::new(std::sync::Barrier::new(2));

    let feeder = {
        let session = session.clone();
        std::thread::spawn(move || {
            session
                .feed(&[
                    Step::Burst(b"first".to_vec()),
                    Step::Pause(1000),
                    Step::Burst(b"second".to_vec()),
                ])
                .unwrap();
        })
    };

    // Spin rather than sleep (denied) until the first burst has landed, so the
    // interloper below races a feed act that is provably inside its pause.
    while writes.lock().unwrap().is_empty() {
        std::thread::yield_now();
    }

    let interloper = {
        let session = session.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            session.send(b"interloper").unwrap();
        })
    };
    barrier.wait();
    // The interloper is now contending for `input_lock`, which the pause does
    // not release — advancing the clock is what finally lets either proceed.
    advance_until_finished(&clock, Duration::from_secs(1), &feeder);

    feeder.join().unwrap();
    interloper.join().unwrap();

    let got = writes.lock().unwrap().clone();
    assert_eq!(
        got,
        vec![
            b"first".to_vec(),
            b"second".to_vec(),
            b"interloper".to_vec()
        ],
        "the interloper must land only after the whole feed act finished: {got:?}"
    );
}

/// NEGATIVE CONTROL for the test above.
///
/// The exact real-world idiom `feed` replaces: type, then submit, as two
/// SEPARATE `Session::send` calls with a gap — `tests/api/v1.lua`'s own
/// `insert` then `key RET`. Each call is individually atomic; the pair is not.
#[test]
fn control_separate_calls_with_a_gap_do_interleave() {
    use std::sync::Barrier;

    const WRITERS: usize = 8;
    let writes = Arc::new(Mutex::new(Vec::new()));
    let (session, _clock) = session_with(Box::new(RecordingAgent::new(writes.clone())));
    let session = Arc::new(session);
    let barrier = Arc::new(Barrier::new(WRITERS));

    let mut handles = Vec::new();
    for i in 0..WRITERS {
        let session = session.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            session.send(format!("line-{i}").as_bytes()).unwrap();
            barrier.wait();
            session.send(b"\r").unwrap();
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let got = writes.lock().unwrap().clone();
    assert_eq!(got.len(), WRITERS * 2);
    assert!(
        !every_write_is_a_whole_instruction(&got),
        "control failed to interleave, so the positive test proves nothing: {got:?}"
    );
}

#[test]
fn capture_does_not_wait_out_a_feed_pause() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let (session, clock) = session_with(Box::new(RecordingAgent::new(writes.clone())));
    let session = Arc::new(session);

    let feeder = {
        let session = session.clone();
        std::thread::spawn(move || {
            // Under `Session::MAX_TOTAL_PAUSE` — a pause over the cap is
            // refused before it writes anything, which is a different test.
            session
                .feed(&[
                    Step::Burst(b"first".to_vec()),
                    Step::Pause(2000),
                    Step::Burst(b"second".to_vec()),
                ])
                .unwrap();
        })
    };

    while writes.lock().unwrap().is_empty() {
        std::thread::yield_now();
    }

    // If a screen read waited on `input_lock`, this would hang forever: the
    // pause above is not released until the advancing below wakes it.
    session.screen_text().unwrap();

    advance_until_finished(&clock, Duration::from_secs(2), &feeder);
    feeder.join().unwrap();
}

#[test]
fn attaching_during_a_feed_pause_refuses_the_remaining_bursts() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let (session, clock) = session_with(Box::new(RecordingAgent::new(writes.clone())));
    let session = Arc::new(session);

    let feeder = {
        let session = session.clone();
        std::thread::spawn(move || {
            session.feed(&[
                Step::Burst(b"first".to_vec()),
                Step::Pause(1000),
                Step::Burst(b"second".to_vec()),
            ])
        })
    };

    while writes.lock().unwrap().is_empty() {
        std::thread::yield_now();
    }

    // `attach` takes neither lock a feed act holds, so it must succeed right
    // away — proving there is no deadlock behind an act sitting in a pause.
    let held = session
        .attach()
        .expect("attach must not wait on a paused feed");

    advance_until_finished(&clock, Duration::from_secs(1), &feeder);
    let result = feeder.join().unwrap();

    assert!(
        matches!(result, Err(AgentError::Attached)),
        "the act must refuse once a human has attached mid-pause: {result:?}"
    );
    assert_eq!(
        writes.lock().unwrap().clone(),
        vec![b"first".to_vec()],
        "the second burst must never reach the pty once attached"
    );
    drop(held);
}

#[test]
fn feed_refuses_a_total_pause_over_the_cap() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let (session, _clock) = session_with(Box::new(RecordingAgent::new(writes.clone())));

    // Two pauses that individually look modest but sum past the cap — the
    // sum is what is checked, not any one `Pause`.
    let over_cap = Session::MAX_TOTAL_PAUSE + Duration::from_millis(1);
    let result = session.feed(&[
        Step::Burst(b"first".to_vec()),
        Step::Pause(over_cap.as_millis() as u64 / 2),
        Step::Burst(b"second".to_vec()),
        Step::Pause(over_cap.as_millis() as u64 / 2 + 1),
        Step::Burst(b"third".to_vec()),
    ]);

    assert!(
        matches!(result, Err(AgentError::PauseTooLong { .. })),
        "a total pause over the cap must be refused: {result:?}"
    );
    assert!(
        writes.lock().unwrap().is_empty(),
        "nothing may be written once any part of the act is refused"
    );
}
