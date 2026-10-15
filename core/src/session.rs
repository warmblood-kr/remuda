//! A session: one agent process, owned by the core, outliving every viewer.
//!
//! Input acts stay indivisible, sessions do not resize, and one viewer owns an
//! attachment at a time.

use crate::agent::{
    AgentError, AgentProcess, ChainOutcome, Cursor, ExitInfo, MouseState, OutputWakeup, Result,
    ScreenSnapshot, Size, StyledCell, VersionedSnapshot,
};
use crate::clock::Clock;
use crate::input::{
    ClearInputRateLimiter, ExpectedInstanceInputError, InputBatch, InputDeduplicator, InputError,
    InputOutcome, InputRateLimiter,
};
use crate::protocol::Step;
use core::time::Duration;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex};

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

fn input_tail(text: &str) -> String {
    let compact: Vec<char> = normalize_input_text(text)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    compact
        .into_iter()
        .rev()
        .take(20)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn normalize_input_text(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|ch| {
            *ch == '\t'
                || *ch == '\n'
                || !matches!(*ch as u32, 0x00..=0x08 | 0x0b..=0x1f | 0x7f | 0x80..=0x9f)
        })
        .collect()
}

#[cfg(test)]
mod input_text_tests {
    use super::{normalize_input_text, occurrence_count};

    #[test]
    fn strips_terminal_controls_by_unicode_character_and_preserves_korean() {
        assert_eq!(
            normalize_input_text("a\x01\tb\r\nc\x7f\u{0085}\u{009b}한글"),
            "a\tb\nc한글"
        );
    }

    #[test]
    fn occurrence_count_counts_visible_tail_matches() {
        assert_eq!(occurrence_count("old: abc / new: abc", "abc"), 2);
        assert_eq!(occurrence_count("old: abc", "missing"), 0);
        assert_eq!(occurrence_count("anything", ""), 0);
    }
}

#[cfg(test)]
mod expected_instance_input_tests {
    use super::*;

    #[test]
    fn wrong_instance_is_rejected_before_any_text_or_key_write() {
        let (session, _clock, writes, _started, _release) =
            super::clear_input_tests::session(false);
        assert_eq!(
            session.type_text_if_instance("stale-instance", "must not land", Duration::ZERO),
            Err(ExpectedInstanceInputError::WrongInstance)
        );
        assert_eq!(
            session.send_key_if_instance("stale-instance", b"\x1b[A"),
            Err(ExpectedInstanceInputError::WrongInstance)
        );
        assert!(writes.lock().unwrap().is_empty());
    }

    #[test]
    fn full_text_and_key_use_the_captured_instance_without_adding_key_bytes() {
        let (session, _clock, writes, _started, _release) =
            super::clear_input_tests::session(false);
        let instance = session.instance_id().to_owned();
        assert_eq!(
            session
                .type_text_if_instance(&instance, "alpha\nbeta", Duration::ZERO)
                .unwrap(),
            InputSubmitOutcome::Submitted
        );
        session.send_key_if_instance(&instance, b"\x1b[A").unwrap();
        assert_eq!(
            *writes.lock().unwrap(),
            vec![b"alpha\nbeta\r".to_vec(), b"\x1b[A".to_vec()]
        );
    }

    #[test]
    fn stopped_instance_is_a_prewrite_refusal() {
        let (session, _clock, writes, _started, _release) =
            super::clear_input_tests::session(false);
        let instance = session.instance_id().to_owned();
        session.terminate().unwrap();
        assert_eq!(
            session.type_text_if_instance(&instance, "no write", Duration::ZERO),
            Err(ExpectedInstanceInputError::NotStarted)
        );
        assert_eq!(
            session.send_key_if_instance(&instance, b"x"),
            Err(ExpectedInstanceInputError::NotStarted)
        );
        assert!(writes.lock().unwrap().is_empty());
    }
}

#[cfg(test)]
mod clear_input_tests {
    use super::*;
    use crate::agent::{AgentError, AgentProcess, AgentWriter, Cursor, Size};
    use crate::clock::ManualClock;
    use std::sync::mpsc::{self, Receiver, Sender};

    struct TestWriter {
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
        started: Sender<()>,
        blocking: bool,
        timed_out: bool,
        blocked_once: AtomicBool,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl AgentWriter for TestWriter {
        fn write_bounded(&self, bytes: &[u8]) -> Result<()> {
            self.write_to_completion(bytes)
        }
        fn write_to_completion(&self, bytes: &[u8]) -> Result<()> {
            let should_block = self.blocking && !self.blocked_once.swap(true, Ordering::SeqCst);
            if !self.blocked_once.load(Ordering::SeqCst) || should_block {
                let _ = self.started.send(());
            }
            if should_block {
                let (ready, changed) = &*self.release;
                let mut ready = ready.lock().unwrap();
                while !*ready {
                    ready = changed.wait(ready).unwrap();
                }
            }
            self.writes.lock().unwrap().push(bytes.to_vec());
            Ok(())
        }
        fn is_busy(&self) -> bool {
            let (ready, _) = &*self.release;
            self.timed_out || (self.blocking && !*ready.lock().unwrap())
        }
        fn is_timed_out(&self) -> bool {
            self.timed_out
        }
    }

    struct TestAgent(Arc<TestWriter>);
    impl AgentProcess for TestAgent {
        fn write(&mut self, bytes: &[u8]) -> Result<()> {
            self.0.write_bounded(bytes)
        }
        fn input_writer(&mut self) -> Option<Arc<dyn AgentWriter>> {
            Some(self.0.clone())
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

    pub(super) type SessionFixture = (
        Arc<Session>,
        Arc<ManualClock>,
        Arc<Mutex<Vec<Vec<u8>>>>,
        Receiver<()>,
        Arc<(Mutex<bool>, Condvar)>,
    );

    pub(super) fn session(blocking: bool) -> SessionFixture {
        session_with_timeout(blocking, false)
    }

    fn session_with_timeout(blocking: bool, timed_out: bool) -> SessionFixture {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let release = Arc::new((Mutex::new(!blocking), Condvar::new()));
        let (started_tx, started_rx) = mpsc::channel();
        let writer = Arc::new(TestWriter {
            writes: Arc::clone(&writes),
            started: started_tx,
            blocking,
            timed_out,
            blocked_once: AtomicBool::new(false),
            release: Arc::clone(&release),
        });
        let clock = Arc::new(ManualClock::new());
        let session = Arc::new(Session::new(
            "test",
            Box::new(TestAgent(writer)),
            clock.clone(),
        ));
        (session, clock, writes, started_rx, release)
    }

    #[test]
    fn attach_write_waiting_after_clear_guard_lands_after_clear() {
        let (session, clock, writes, _started, _release) = session(false);
        let attached = session.attach();
        clock.advance(Duration::from_secs(10));
        let (spawned_tx, spawned_rx) = mpsc::channel();
        let (handle_tx, handle_rx) = mpsc::channel();
        let session_for_hook = Arc::clone(&session);
        *session.clear_hook.lock().unwrap() = Some(Box::new(move || {
            let session = Arc::clone(&session_for_hook);
            let handle = std::thread::spawn(move || {
                let attached = session.attach();
                let _ = spawned_tx.send(());
                attached.write_raw(b"x").unwrap();
            });
            handle_tx.send(handle).unwrap();
            spawned_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        }));
        session.clear_input(b"\x15").unwrap();
        drop(attached);
        handle_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(
            *writes.lock().unwrap(),
            vec![b"\x15".to_vec(), b"x".to_vec()]
        );
    }

    #[test]
    fn clear_refuses_while_attach_write_is_in_flight_even_after_idle_window() {
        let (session, clock, _writes, started, release) = session(true);
        let worker_session = Arc::clone(&session);
        let worker = std::thread::spawn(move || worker_session.attach().write_raw(b"x"));
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        clock.advance(Duration::from_secs(3));
        assert!(matches!(
            session.clear_input(b"\x15"),
            Err(AgentError::HumanInputRecent)
        ));
        let (ready, changed) = &*release;
        *ready.lock().unwrap() = true;
        changed.notify_all();
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn clear_refuses_a_timed_out_writer_without_writing() {
        let (session, clock, writes, _started, _release) = session_with_timeout(false, true);
        clock.advance(Duration::from_secs(3));
        assert!(matches!(
            session.clear_input(b"\x15"),
            Err(AgentError::Busy)
        ));
        assert!(writes.lock().unwrap().is_empty());
    }

    #[test]
    fn clear_rejects_untrusted_keys() {
        let (session, clock, writes, _started, _release) = session(false);
        clock.advance(Duration::from_secs(3));
        for key in [
            b"\x0f".as_slice(),
            b"\x1bOM",
            b"x",
            b"\x15\x15",
            b"\r",
            b"\n",
        ] {
            assert!(session.clear_input(key).is_err(), "accepted {key:?}");
        }
        assert!(writes.lock().unwrap().is_empty());
    }

    #[test]
    fn clear_key_length_is_limited_before_writer_activity() {
        let (session, clock, writes, _started, _release) = session(false);
        clock.advance(Duration::from_secs(3));
        for key in [Vec::new(), vec![b'x'; 17], vec![b'x'; 1_048_576]] {
            assert!(
                session.clear_input(&key).is_err(),
                "accepted {} bytes",
                key.len()
            );
        }
        assert!(writes.lock().unwrap().is_empty());
    }

    #[test]
    fn clear_calls_have_a_per_session_rate_budget() {
        let (session, clock, writes, _started, _release) = session(false);
        clock.advance(Duration::from_secs(3));
        for _ in 0..4 {
            session.clear_input(b"\x15").unwrap();
        }
        assert!(session.clear_input(b"\x15").is_err());
        assert_eq!(writes.lock().unwrap().len(), 4);
        clock.advance(Duration::from_secs(1));
        session.clear_input(b"\x15").unwrap();
        assert_eq!(writes.lock().unwrap().len(), 5);
    }

    #[test]
    fn clear_refuses_an_active_writer_without_queuing_behind_it() {
        let (session, clock, writes, started, release) = session(true);
        let writer = session.agent.lock().unwrap().input_writer().unwrap();
        let active = std::thread::spawn(move || writer.write_bounded(b"prior"));
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        clock.advance(Duration::from_secs(3));
        assert!(matches!(
            session.clear_input(b"\x15"),
            Err(AgentError::Busy)
        ));
        assert!(writes.lock().unwrap().is_empty());
        let (ready, changed) = &*release;
        *ready.lock().unwrap() = true;
        changed.notify_all();
        active.join().unwrap().unwrap();
    }

    #[test]
    fn idle_clear_still_works_and_clear_keys_cannot_submit_lines() {
        let (session, clock, writes, _started, _release) = session(false);
        clock.advance(Duration::from_secs(3));
        session.clear_input(b"\x15").unwrap();
        assert_eq!(*writes.lock().unwrap(), vec![b"\x15".to_vec()]);
        for key in [b"\r".as_slice(), b"\n".as_slice(), b"\x15\r".as_slice()] {
            assert!(
                matches!(session.clear_input(key), Err(AgentError::Io(message)) if message.contains("allowlisted"))
            );
        }
        assert_eq!(*writes.lock().unwrap(), vec![b"\x15".to_vec()]);
    }
}

#[cfg(test)]
mod submit_window_tests {
    use super::*;
    use crate::agent::{AgentProcess, Cursor, Size};
    use crate::clock::ManualClock;

    /// Each screen-version read moves the manual clock by one poll step, so a
    /// run depends only on the script, never on wall-clock luck (#637).
    const STEP: Duration = Duration::from_millis(50);
    /// type_text's screen probes before the budget starts, two STEPs here.
    const SETUP_PROBE_SLACK: Duration = Duration::from_millis(100);

    #[derive(Clone, Copy)]
    enum Redraw {
        Submit,
        ComposerNewline,
        Nothing,
    }

    struct Script {
        echo_after: Duration,
        trailing_after: Option<Duration>,
        redraw_after: Duration,
        redraw: Redraw,
        typed: String,
        typed_at: Option<Duration>,
        returns: Vec<Duration>,
    }

    /// A TUI whose paste echo, post-echo status redraw and post-Return redraw
    /// land at chosen offsets on the manual clock.
    struct SlowTui {
        clock: Arc<ManualClock>,
        script: Arc<Mutex<Script>>,
    }

    impl SlowTui {
        fn screen(&self) -> (String, u64) {
            let now = self.clock.now();
            let script = self.script.lock().unwrap();
            let mut screen = String::from("READY\n> ");
            let mut version = 1;
            if let Some(typed_at) = script.typed_at {
                let echoed = typed_at + script.echo_after;
                if now >= echoed {
                    screen.push_str(&script.typed);
                    version += 1;
                }
                if let Some(trailing) = script.trailing_after {
                    if now >= echoed + trailing {
                        screen.push_str("\n[pasted]");
                        version += 1;
                    }
                }
            }
            if let Some(first_return) = script.returns.first() {
                if now >= *first_return + script.redraw_after {
                    match script.redraw {
                        Redraw::Submit => screen.push_str("\nworking on it"),
                        Redraw::ComposerNewline => screen.push_str("\n  "),
                        Redraw::Nothing => version -= 1,
                    }
                    version += 1;
                }
            }
            (screen, version)
        }
    }

    impl AgentProcess for SlowTui {
        fn write(&mut self, bytes: &[u8]) -> Result<()> {
            let now = self.clock.now();
            let mut script = self.script.lock().unwrap();
            if bytes == crate::keys::RETURN_BYTES {
                script.returns.push(now);
            } else {
                script.typed = String::from_utf8_lossy(bytes).into_owned();
                script.typed_at = Some(now);
            }
            Ok(())
        }
        fn screen_text(&mut self) -> Result<String> {
            Ok(self.screen().0)
        }
        fn output_version(&mut self) -> Option<u64> {
            self.clock.advance(STEP);
            Some(self.screen().1)
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

    #[test]
    fn idempotent_line_waits_for_echo_and_sends_one_return_even_for_composer_newline() {
        for redraw in [Redraw::Submit, Redraw::ComposerNewline] {
            let clock = Arc::new(ManualClock::new());
            let script = Arc::new(Mutex::new(Script {
                echo_after: Duration::from_millis(300),
                trailing_after: None,
                redraw_after: Duration::from_millis(100),
                redraw,
                typed: String::new(),
                typed_at: None,
                returns: Vec::new(),
            }));
            let session = Session::new(
                "line",
                Box::new(SlowTui {
                    clock: clock.clone(),
                    script: script.clone(),
                }),
                clock.clone(),
            );
            let batch = crate::input::InputBatch {
                instance_id: session.instance_id(),
                client_id: [6; 16],
                seq: 1,
                bytes: b"alpha\r",
            };
            assert_eq!(
                session.apply_input_line_batch(batch).unwrap(),
                crate::input::InputOutcome::Ack { duplicate: false }
            );
            assert_eq!(
                session.apply_input_line_batch(batch).unwrap(),
                crate::input::InputOutcome::Ack { duplicate: true }
            );
            let script = script.lock().unwrap();
            assert_eq!(script.typed, "alpha");
            assert_eq!(script.returns.len(), 1);
            assert!(script.returns[0] >= script.typed_at.unwrap() + script.echo_after);
        }
    }

    /// Types "alpha" into a scripted TUI; returns the outcome, the Return times,
    /// and how long `type_text` took after the first Return and in total.
    fn type_into(
        echo_ms: u64,
        trailing_ms: Option<u64>,
        redraw_ms: u64,
        redraw: Redraw,
    ) -> (InputSubmitOutcome, Vec<Duration>, Duration, Duration) {
        let clock = Arc::new(ManualClock::new());
        let script = Arc::new(Mutex::new(Script {
            echo_after: Duration::from_millis(echo_ms),
            trailing_after: trailing_ms.map(Duration::from_millis),
            redraw_after: Duration::from_millis(redraw_ms),
            redraw,
            typed: String::new(),
            typed_at: None,
            returns: Vec::new(),
        }));
        let agent = SlowTui {
            clock: Arc::clone(&clock),
            script: Arc::clone(&script),
        };
        let session = Session::new("tui", Box::new(agent), clock.clone());
        let entered = clock.now();
        let outcome = session.type_text("alpha", Duration::ZERO).unwrap();
        let returns = script.lock().unwrap().returns.clone();
        let after_return = clock.now() - returns[0];
        (outcome, returns, after_return, clock.now() - entered)
    }

    #[test]
    fn a_prompt_redraw_after_return_is_submitted() {
        let (outcome, returns, _, _) = type_into(0, None, 200, Redraw::Submit);
        assert_eq!(outcome, InputSubmitOutcome::Submitted);
        assert_eq!(returns.len(), 1);
    }

    #[test]
    fn a_silent_submit_is_unverified_within_one_second() {
        let (outcome, returns, after_return, _) = type_into(0, None, 0, Redraw::Nothing);
        assert_eq!(outcome, InputSubmitOutcome::Unverified);
        assert_eq!(returns.len(), 1, "a silent submit must not be retried");
        assert!(
            after_return <= Duration::from_millis(1100),
            "{after_return:?}"
        );
    }

    #[test]
    fn a_prompt_composer_newline_gets_one_retry() {
        let (outcome, returns, _, _) = type_into(0, None, 200, Redraw::ComposerNewline);
        assert_eq!(outcome, InputSubmitOutcome::Unverified);
        assert_eq!(returns.len(), 2);
    }

    #[test]
    fn a_slow_tui_redraw_after_return_is_still_submitted() {
        let (outcome, returns, _, _) = type_into(600, None, 1500, Redraw::Submit);
        assert_eq!(outcome, InputSubmitOutcome::Submitted);
        assert_eq!(returns.len(), 1);
    }

    #[test]
    fn a_slow_tui_composer_newline_still_gets_its_retry() {
        let (_, returns, _, _) = type_into(600, None, 1500, Redraw::ComposerNewline);
        assert_eq!(
            returns.len(),
            2,
            "the late composer newline was not retried"
        );
    }

    #[test]
    fn a_slow_tui_late_paste_redraw_is_not_taken_for_a_submit() {
        let (outcome, returns, _, _) = type_into(600, Some(500), 0, Redraw::Nothing);
        assert_eq!(outcome, InputSubmitOutcome::Unverified);
        assert_eq!(returns.len(), 1);
    }

    #[test]
    fn the_whole_call_stays_within_the_submit_budget() {
        let (outcome, _, after_return, total) = type_into(1900, None, 0, Redraw::Nothing);
        assert_eq!(outcome, InputSubmitOutcome::Unverified);
        assert!(after_return >= Duration::from_secs(1), "{after_return:?}");
        assert!(
            total <= SUBMIT_BUDGET + SETUP_PROBE_SLACK,
            "{total:?} exceeds 6s after the paste plus the setup probes"
        );
    }
}

/// A child slow to echo the paste is slow to redraw after Return too (#637).
fn submit_window(echo: Duration) -> Duration {
    SUBMIT_WINDOW.max(echo * 3)
}

fn occurrence_count(screen: &str, tail: &str) -> usize {
    if tail.is_empty() {
        0
    } else {
        screen.match_indices(tail).count()
    }
}

fn compact_screen(screen: &str) -> String {
    screen.chars().filter(|ch| !ch.is_whitespace()).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputSubmitOutcome {
    /// The screen changed after Return, or the Return-only operation was sent.
    Submitted,
    /// Return was sent, but the visible screen did not verify submission.
    Unverified,
    /// The text is still being written to a slow child; Return follows when it lands,
    /// or is dropped after the late-submit bound.
    Late,
}

/// Pause between pasted text and its Return when the caller gives none.
pub const DEFAULT_INPUT_SETTLE: Duration = Duration::from_millis(100);
/// Quiet wait before Return and observation window after it, for a child that
/// echoes the paste at once. A slower echo stretches both (`submit_window`).
const SCREEN_QUIET: Duration = Duration::from_millis(300);
const SUBMIT_WINDOW: Duration = Duration::from_secs(1);
/// The echo wait, quiet wait and post-Return window share this budget, counted
/// from after the paste: type_text's setup probes before it (~0.1s) come on top.
const SUBMIT_BUDGET: Duration = Duration::from_secs(6);
/// A clear is refused while the most recent human keystroke is this recent.
const HUMAN_INPUT_GUARD: Duration = Duration::from_secs(2);
const MAX_CLEAR_INPUT_KEY_BYTES: usize = 16;
/// Core-owned allowlist: only the verified Ctrl+U clear-line binding is enabled.
const CLEAR_INPUT_KEYS: &[&[u8]] = &[b"\x15"];

struct PendingInput {
    tail: String,
    baseline_occurrences: usize,
}

#[derive(Clone, Copy)]
enum BurstWriteMode {
    Bounded,
    IfIdle,
    ToCompletion,
}

/// A running agent, addressable by name.
pub struct Session {
    id: String,
    name: String,
    agent: Mutex<Box<dyn AgentProcess>>,
    size: Mutex<Size>,
    clock: Arc<dyn Clock>,
    /// Reading of [`Clock::now`] taken at the last successful `send_line`.
    /// Meaningful only as a difference against a later reading.
    last_input_at: Mutex<Duration>,
    /// Reading of [`Clock::now`] when the agent last produced output. A
    /// dedicated receiver keeps this current even when no caller polls the
    /// screen; `idle_for` remains the distinct since-input measure.
    last_output_at: Arc<Mutex<Duration>>,
    /// Output waiters share this notification; screen/version reads remain
    /// under the agent's own lock and never hold it across the wait.
    output_changed: Arc<(Mutex<u64>, Condvar)>,
    instance_id: String,
    /// [`Clock::now`] at the last keystroke through an [`Attached`] guard —
    /// a human's, never a script's. `None` until one arrives (#136).
    last_human_input_at: Mutex<Option<Duration>>,
    /// Orders attached writes with destructive clear operations.
    human_gate: Mutex<()>,
    /// Attached writes which have started but have not completed.
    human_inflight: AtomicUsize,
    #[cfg(test)]
    clear_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Set while the current [`Attached`] guard is alive.
    attached: AtomicBool,
    /// Set under the registry lock before a close request terminates the child.
    closing: AtomicBool,
    /// Once any liveness probe observes exit, this launch can never become live again.
    alive: AtomicBool,
    /// Current attachment generation and its takeover signal. The generation
    /// keeps an old guard's drop from clearing a newer attachment.
    attach_slot: Mutex<Option<(u64, Arc<AtomicBool>)>>,
    next_attach_generation: AtomicU64,
    /// Held for the whole of one input act — every `Burst` **and** every
    /// `Pause` between them — so a second sender cannot land a write during a
    /// pause, when the `agent` lock is briefly free. See [`Self::feed`].
    input_lock: Mutex<bool>,
    input_ready: Condvar,
    pending_input: Mutex<Option<PendingInput>>,
    /// Bounded retry history for remote byte batches, kept per session.
    input_dedup: Mutex<InputDeduplicator>,
    /// Per-session byte budget, checked before taking `input_lock`.
    input_rate: Mutex<InputRateLimiter>,
    /// Per-session operation budget for destructive clear-key writes.
    clear_input_rate: Mutex<ClearInputRateLimiter>,
}

fn configure_writer_clock(
    agent: &mut dyn AgentProcess,
    clock: &Arc<dyn Clock>,
) -> (Duration, Arc<Mutex<Duration>>) {
    if let Some(writer) = agent.input_writer() {
        writer.set_clock(Arc::clone(clock));
    }
    let started = clock.now();
    (started, Arc::new(Mutex::new(started)))
}

/// Generate a unique session-start identity from host entropy and a process counter.
pub fn generate_instance_id(seed: u128) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("{seed:032x}-{:016x}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// Bounded identity allocated by the caller before child creation. This type
/// validates shape; the owning store must establish uniqueness and durability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartIdentity {
    id: String,
    instance: String,
}
impl StartIdentity {
    pub fn new(id: impl Into<String>, instance: impl Into<String>) -> Result<Self> {
        let identity = Self {
            id: id.into(),
            instance: instance.into(),
        };
        if [&identity.id, &identity.instance].iter().any(|value| {
            value.is_empty() || value.len() > 256 || value.chars().any(char::is_control)
        }) {
            return Err(AgentError::Io(
                "invalid preassigned session identity".into(),
            ));
        }
        Ok(identity)
    }
}

/// Identity and size only. Deliberately takes no lock: a `Debug` that locks
/// deadlocks exactly where you reach for it — in a panic message printed while
/// the session's own lock is held.
impl core::fmt::Debug for Session {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Session")
            .field("name", &self.name)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl Session {
    pub fn new(
        name: impl Into<String>,
        agent: Box<dyn AgentProcess>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self::new_with_id(name, Self::new_id(), agent, clock)
    }

    /// Generate a daemon-local identity before spawning a session process so
    /// it can be placed in that process's environment.
    pub fn new_id() -> String {
        format!(
            "{}-{}",
            std::process::id(),
            NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Construct a session with the identity injected into its child process.
    pub fn new_with_id(
        name: impl Into<String>,
        id: impl Into<String>,
        agent: Box<dyn AgentProcess>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let instance_id = generate_instance_id(clock.instance_id_seed());
        Self::build(name.into(), id.into(), instance_id, agent, clock)
    }

    /// Preserve the caller's preassigned instance without minting another owner
    /// identity after spawn. The caller must reserve it before creating a child.
    pub fn with_start_identity(
        name: impl Into<String>,
        identity: StartIdentity,
        agent: Box<dyn AgentProcess>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self::build(name.into(), identity.id, identity.instance, agent, clock)
    }

    #[allow(clippy::too_many_lines)]
    fn build(
        name: String,
        id: String,
        instance_id: String,
        mut agent: Box<dyn AgentProcess>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let (started, last_output_at) = configure_writer_clock(agent.as_mut(), &clock);
        let size = agent.size();
        let output_changed = Arc::new((Mutex::new(0_u64), Condvar::new()));
        if let Some(output) = agent.subscribe_output_wakeup() {
            let last_output_at = Arc::clone(&last_output_at);
            let output_changed = Arc::clone(&output_changed);
            let clock = Arc::clone(&clock);
            std::thread::spawn(move || {
                while output.recv().is_ok() {
                    output.version_after_wake();
                    if let Ok(mut at) = last_output_at.lock() {
                        *at = clock.now();
                    }
                    if let Ok(mut generation) = output_changed.0.lock() {
                        *generation = (*generation).wrapping_add(1);
                        output_changed.1.notify_all();
                    }
                }
                // EOF means the child exited or its output stream was closed.
                // Wake Sync waiters so they can observe the final screen now.
                Self::notify_output_changed(&output_changed);
            });
        } else if let Some(output) = agent.subscribe() {
            let last_output_at = Arc::clone(&last_output_at);
            let output_changed = Arc::clone(&output_changed);
            let clock = Arc::clone(&clock);
            std::thread::spawn(move || {
                while output.recv().is_ok() {
                    if let Ok(mut at) = last_output_at.lock() {
                        *at = clock.now();
                    }
                    if let Ok(mut generation) = output_changed.0.lock() {
                        *generation = (*generation).wrapping_add(1);
                        output_changed.1.notify_all();
                    }
                }
                // EOF means the child exited or its output stream was closed.
                // Wake Sync waiters so they can observe the final screen now.
                Self::notify_output_changed(&output_changed);
            });
        }
        Self {
            id,
            name,
            agent: Mutex::new(agent),
            size: Mutex::new(size),
            clock,
            last_input_at: Mutex::new(started),
            last_output_at,
            output_changed,
            instance_id,
            last_human_input_at: Mutex::new(None),
            human_gate: Mutex::new(()),
            human_inflight: AtomicUsize::new(0),
            #[cfg(test)]
            clear_hook: Mutex::new(None),
            attached: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            alive: AtomicBool::new(true),
            attach_slot: Mutex::new(None),
            next_attach_generation: AtomicU64::new(1),
            input_lock: Mutex::new(false),
            input_ready: Condvar::new(),
            pending_input: Mutex::new(None),
            input_dedup: Mutex::new(InputDeduplicator::new()),
            input_rate: Mutex::new(InputRateLimiter::default()),
            clear_input_rate: Mutex::new(ClearInputRateLimiter::default()),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub fn output_version(&self) -> Option<u64> {
        self.agent
            .lock()
            .ok()
            .and_then(|mut agent| agent.output_version())
    }

    /// Subscribe to the agent's output stream, when this backend supports it.
    pub fn subscribe(&self) -> Option<Receiver<Vec<u8>>> {
        self.agent.lock().ok()?.subscribe()
    }

    /// Subscribe to coalesced output-version changes without copying PTY data.
    pub fn subscribe_output_wakeup(&self) -> Option<OutputWakeup> {
        self.agent.lock().ok()?.subscribe_output_wakeup()
    }

    /// The current terminal size.
    pub fn size(&self) -> Size {
        self.size.lock().map(|size| *size).unwrap_or_default()
    }

    /// A stale pane must never resize a replacement session with the same name.
    pub fn resize_instance(&self, instance_id: &str, size: Size) -> Result<bool> {
        if self.instance_id != instance_id {
            return Ok(false);
        }
        if !self.is_alive() {
            return Err(AgentError::Exited);
        }
        if size.cols() > Size::MAX_RESIZE_COLS || size.rows() > Size::MAX_RESIZE_ROWS {
            return Err(AgentError::Io("resize exceeds viewport limits".into()));
        }
        self.resize(size)?;
        Ok(true)
    }

    /// Resize the backing terminal and publish its new dimensions together.
    /// The agent lock serializes this with output capture and input delivery;
    /// a failed backend resize leaves the advertised session size unchanged.
    pub fn resize(&self, size: Size) -> Result<()> {
        let mut agent = self
            .agent
            .lock()
            .map_err(|_| AgentError::Io("session lock poisoned".into()))?;
        agent.resize(size)?;
        let mut current = self
            .size
            .lock()
            .map_err(|_| AgentError::Io("session size lock poisoned".into()))?;
        *current = size;
        drop(current);
        drop(agent);
        Self::notify_output_changed(&self.output_changed);
        Ok(())
    }

    /// Deliver one instruction: the text, then Enter, as one indivisible act.
    /// The complete text/submit sequence shares one input lock. Backends
    /// without a rendered screen keep the historical single-burst behavior.
    pub fn send_line(&self, text: &str) -> Result<()> {
        match self.type_text(text, Duration::ZERO)? {
            InputSubmitOutcome::Late => Err(AgentError::SubmitPending),
            InputSubmitOutcome::Submitted | InputSubmitOutcome::Unverified => Ok(()),
        }
    }

    /// Deliver a normalized text burst, bracketed when the child enabled mode
    /// 2004. The burst contains no submit key.
    pub fn input_text(&self, text: &str) -> Result<()> {
        let body = normalize_input_text(text);
        let tail = input_tail(&body);
        let _held = self.acquire_input_lock()?;
        if let Ok(mut pending) = self.pending_input.lock() {
            *pending = None;
        }
        let baseline = self.tail_occurrences(&tail);
        self.input_text_locked(&body)?;
        if let Ok(mut pending) = self.pending_input.lock() {
            *pending = Some(PendingInput {
                tail,
                baseline_occurrences: baseline,
            });
        }
        Ok(())
    }

    fn input_text_locked(&self, body: &str) -> Result<()> {
        let bracketed = self.mouse_state().bracketed_paste;
        let burst = if bracketed {
            let mut bytes = Vec::with_capacity(body.len() + 12);
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(body.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
            bytes
        } else {
            body.as_bytes().to_vec()
        };
        self.write_one_burst(&burst)
    }

    /// Submit with a separate Return after the text is visible. A changed
    /// screen after Return means it was submitted; an unchanged screen after
    /// the bounded observation means Return became a composer newline.
    pub fn submit(&self, expect: &str) -> Result<InputSubmitOutcome> {
        let _held = self.acquire_input_lock()?;
        let tail = input_tail(expect);
        let baseline = self
            .pending_input
            .lock()
            .ok()
            .and_then(|mut pending| pending.take())
            .filter(|pending| pending.tail == tail)
            .map(|pending| pending.baseline_occurrences)
            .unwrap_or_else(|| self.tail_occurrences(&tail).saturating_sub(1));
        match self.submit_locked(&tail, baseline) {
            // The first Return write was refused without writing bytes. The
            // body came from an earlier input_text call, so keep the result
            // cautious and report submission as unconfirmed.
            Err(AgentError::Busy) => Err(AgentError::SubmitUncertain),
            result => result,
        }
    }

    /// Deliver TEXT and submit it as one act. This is the composite used by
    /// type_text and SendLine; no other input sender can split the two units.
    pub fn type_text(&self, text: &str, settle: Duration) -> Result<InputSubmitOutcome> {
        let _held = self.acquire_input_lock()?;
        self.type_text_locked(text, settle, true)
    }

    fn type_text_locked(
        &self,
        text: &str,
        settle: Duration,
        retry_return: bool,
    ) -> Result<InputSubmitOutcome> {
        let body = normalize_input_text(text);
        if body.is_empty() {
            if let Ok(mut pending) = self.pending_input.lock() {
                *pending = None;
            }
            self.write_one_burst(crate::keys::RETURN_BYTES)?;
            return Ok(InputSubmitOutcome::Submitted);
        }
        if self.output_version().is_none() || self.screen_text().is_err() {
            let mut bytes = body.into_bytes();
            bytes.extend_from_slice(crate::keys::RETURN_BYTES);
            self.write_one_burst(&bytes)?;
            return Ok(InputSubmitOutcome::Submitted);
        }

        let tail = input_tail(&body);
        if let Ok(mut pending) = self.pending_input.lock() {
            *pending = None;
        }
        let baseline = self.tail_occurrences(&tail);
        match self.input_text_locked(&body) {
            Ok(()) => {}
            Err(error @ AgentError::WriteTimeout { .. }) => {
                let outcome = {
                    let mut agent = self
                        .agent
                        .lock()
                        .map_err(|_| AgentError::Io("session lock poisoned".into()))?;
                    agent
                        .input_writer()
                        .map(|writer| {
                            writer.chain_after_stalled(
                                crate::keys::RETURN_BYTES,
                                settle.max(DEFAULT_INPUT_SETTLE),
                            )
                        })
                        .unwrap_or(ChainOutcome::Unsupported)
                };
                match outcome {
                    ChainOutcome::Chained => return Ok(InputSubmitOutcome::Late),
                    ChainOutcome::Landed => {}
                    ChainOutcome::Unsupported => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
        if !settle.is_zero() {
            self.clock.sleep(settle);
        }
        match self.submit_locked_with_retry(&tail, baseline, retry_return) {
            Err(AgentError::Busy) => Err(AgentError::SubmitUncertain),
            result => result,
        }
    }

    /// Type and submit only when this captured session is the expected launch.
    /// Identity/liveness are checked before writing; after entering the write
    /// path, errors are uncertain because bytes may have reached the PTY.
    pub fn type_text_if_instance(
        &self,
        expected_instance: &str,
        text: &str,
        settle: Duration,
    ) -> core::result::Result<InputSubmitOutcome, ExpectedInstanceInputError> {
        if self.instance_id() != expected_instance {
            return Err(ExpectedInstanceInputError::WrongInstance);
        }
        if !self.is_alive() {
            return Err(ExpectedInstanceInputError::NotStarted);
        }
        self.type_text(text, settle)
            .map_err(|_| ExpectedInstanceInputError::OutcomeUncertain)
    }

    /// Send exactly the supplied key bytes to this captured session instance.
    /// This does not append Return or otherwise change the key sequence.
    pub fn send_key_if_instance(
        &self,
        expected_instance: &str,
        bytes: &[u8],
    ) -> core::result::Result<(), ExpectedInstanceInputError> {
        if self.instance_id() != expected_instance {
            return Err(ExpectedInstanceInputError::WrongInstance);
        }
        if !self.is_alive() {
            return Err(ExpectedInstanceInputError::NotStarted);
        }
        self.send(bytes)
            .map_err(|_| ExpectedInstanceInputError::OutcomeUncertain)
    }

    fn submit_locked(&self, tail: &str, baseline_occurrences: usize) -> Result<InputSubmitOutcome> {
        self.submit_locked_with_retry(tail, baseline_occurrences, true)
    }

    fn submit_locked_with_retry(
        &self,
        tail: &str,
        baseline_occurrences: usize,
        retry_return: bool,
    ) -> Result<InputSubmitOutcome> {
        if tail.is_empty() {
            self.write_one_burst(crate::keys::RETURN_BYTES)?;
            return Ok(InputSubmitOutcome::Submitted);
        }
        let Some(mut version) = self.output_version() else {
            self.write_one_burst(crate::keys::RETURN_BYTES)?;
            return Ok(InputSubmitOutcome::Submitted);
        };
        if self.screen_text().is_err() {
            self.write_one_burst(crate::keys::RETURN_BYTES)?;
            return Ok(InputSubmitOutcome::Submitted);
        }
        let echo_started = self.clock.now();
        let budget_end = echo_started + SUBMIT_BUDGET;
        let visible = self.wait_for_visible_tail(tail, baseline_occurrences, &mut version);
        let echo = self.clock.now().saturating_sub(echo_started);
        let left = budget_end.saturating_sub(self.clock.now());
        self.wait_for_screen_quiet(&mut version, SCREEN_QUIET.max(echo).min(left));
        let before_raw = self.screen_text().unwrap_or_default();
        let before_compact = compact_screen(&before_raw);
        let before_version = self.output_version().unwrap_or(version);
        self.write_one_burst(crate::keys::RETURN_BYTES)?;
        if !visible {
            return Ok(InputSubmitOutcome::Unverified);
        }

        version = before_version;
        let left = budget_end.saturating_sub(self.clock.now());
        let deadline = self.clock.now() + submit_window(echo).min(left);
        let mut composer_newline = false;
        while self.clock.now() < deadline {
            if !self.is_alive() {
                return Ok(InputSubmitOutcome::Unverified);
            }
            let raw = self.screen_text().unwrap_or_default();
            if raw != before_raw {
                if compact_screen(&raw) == before_compact {
                    composer_newline = true;
                } else {
                    return Ok(InputSubmitOutcome::Submitted);
                }
            }
            let remaining = deadline.saturating_sub(self.clock.now());
            let timeout = remaining.min(Duration::from_millis(50));
            if let Ok(snapshot) = self.wait_for_output_after(version, timeout) {
                version = snapshot.output_version.unwrap_or(version);
            }
        }
        if !composer_newline || !retry_return {
            return Ok(InputSubmitOutcome::Unverified);
        }

        // The first Return inserted a composer newline. A single retry is
        // justified; after sending it, report that delivery is unverified
        // because the screen did not prove whether the child submitted it.
        self.write_one_burst(crate::keys::RETURN_BYTES)?;
        Ok(InputSubmitOutcome::Unverified)
    }

    fn wait_for_visible_tail(
        &self,
        tail: &str,
        baseline_occurrences: usize,
        version: &mut u64,
    ) -> bool {
        let deadline = self.clock.now() + Duration::from_secs(2);
        while self.clock.now() < deadline {
            if self
                .compact_screen()
                .is_some_and(|screen| occurrence_count(&screen, tail) > baseline_occurrences)
            {
                return true;
            }
            let remaining = deadline.saturating_sub(self.clock.now());
            let timeout = remaining.min(Duration::from_millis(50));
            match self.wait_for_output_after(*version, timeout) {
                Ok(snapshot) => *version = snapshot.output_version.unwrap_or(*version),
                Err(_) => return false,
            }
        }
        self.compact_screen()
            .is_some_and(|screen| occurrence_count(&screen, tail) > baseline_occurrences)
    }

    fn tail_occurrences(&self, tail: &str) -> usize {
        self.compact_screen()
            .map_or(0, |screen| occurrence_count(&screen, tail))
    }

    fn wait_for_screen_quiet(&self, version: &mut u64, quiet: Duration) {
        let deadline = self.clock.now() + quiet;
        while self.clock.now() < deadline {
            let remaining = deadline.saturating_sub(self.clock.now());
            let timeout = remaining.min(Duration::from_millis(75));
            match self.wait_for_output_after(*version, timeout) {
                Ok(snapshot) => *version = snapshot.output_version.unwrap_or(*version),
                Err(_) => return,
            }
        }
    }

    fn compact_screen(&self) -> Option<String> {
        self.screen_text()
            .ok()
            .map(|screen| compact_screen(&screen))
    }

    /// Deliver a burst of input as one indivisible act, appending nothing — the
    /// one-`Burst` case of [`Self::feed`]. Holds `input_lock`, so it cannot
    /// land inside a `feed` act's pause, nor a `feed` act land inside it.
    pub fn send(&self, bytes: &[u8]) -> Result<()> {
        let _held = self.acquire_input_lock()?;
        self.write_one_burst(bytes)
    }

    /// Write the core-allowlisted clear-line key as one atomic burst. Refuses
    /// untrusted keys, a busy writer, recent human input, or excess clear calls.
    /// Cleared text is `None` when this session cannot identify its composer.
    pub fn clear_input(&self, key: &[u8]) -> Result<Option<String>> {
        if key.is_empty() || key.len() > MAX_CLEAR_INPUT_KEY_BYTES {
            return Err(AgentError::Io(
                "clear key must contain 1 to 16 bytes".into(),
            ));
        }
        if !CLEAR_INPUT_KEYS.contains(&key) {
            return Err(AgentError::Io(
                "clear key is not allowlisted; only Ctrl+U (byte 0x15) is supported".into(),
            ));
        }
        let _held = self.acquire_input_lock()?;
        let _human_gate = self
            .human_gate
            .lock()
            .map_err(|_| AgentError::Io("session human input gate poisoned".into()))?;
        if self
            .human_idle_for()
            .is_some_and(|idle| idle < HUMAN_INPUT_GUARD)
            || self.human_inflight.load(Ordering::SeqCst) > 0
        {
            return Err(AgentError::HumanInputRecent);
        }
        self.clear_input_rate
            .lock()
            .map_err(|_| AgentError::Io("session clear input rate limit poisoned".into()))?
            .check_rate(self.clock.now())
            .map_err(|_| {
                AgentError::Io("clear input rate limit exceeded (4 calls per second)".into())
            })?;
        #[cfg(test)]
        if let Some(hook) = self.clear_hook.lock().unwrap().take() {
            hook();
        }
        self.write_one_burst_if_idle(key)?;
        Ok(None)
    }

    /// Check the shared per-session remote-input byte budget.
    pub fn check_rate(&self, bytes: usize) -> core::result::Result<(), InputError> {
        let mut rate = self
            .input_rate
            .lock()
            .map_err(|_| InputError::Unavailable)?;
        rate.check_rate(self.clock.now(), bytes)
    }

    /* old block removed below */

    fn refund_rate(&self, bytes: usize) {
        if let Ok(mut rate) = self.input_rate.lock() {
            rate.refund_rate(self.clock.now(), bytes);
        }
    }

    /// Apply a bounded remote batch once for this exact session instance.
    /// Local input shares `input_lock`, so each complete batch stays atomic.
    pub fn apply_input_batch(
        &self,
        batch: InputBatch<'_>,
    ) -> core::result::Result<InputOutcome, InputError> {
        self.apply_input_batch_mode(batch, false)
    }

    /// Apply a deduplicated line with exactly one Return and no automatic retry.
    pub fn apply_input_line_batch(
        &self,
        batch: InputBatch<'_>,
    ) -> core::result::Result<InputOutcome, InputError> {
        self.apply_input_batch_mode(batch, true)
    }

    fn apply_input_batch_mode(
        &self,
        batch: InputBatch<'_>,
        line: bool,
    ) -> core::result::Result<InputOutcome, InputError> {
        let text = if line {
            let bytes = batch.bytes.strip_suffix(b"\r").unwrap_or(batch.bytes);
            Some(std::str::from_utf8(bytes).map_err(|_| InputError::Unavailable)?)
        } else {
            None
        };
        if batch.seq == 0 {
            return Err(InputError::InvalidSequence);
        }
        if !self.is_alive() {
            return Ok(InputOutcome::Exited);
        }
        if batch.instance_id != self.instance_id {
            return Ok(InputOutcome::WrongInstance);
        }
        if let Some(outcome) = self.batch_result(batch.client_id, batch.seq)? {
            return Ok(outcome);
        }
        self.ensure_writer_idle()?;
        self.check_rate(batch.bytes.len())?;
        let _held = match self.acquire_input_lock() {
            Ok(held) => held,
            Err(AgentError::Busy) => {
                self.refund_rate(batch.bytes.len());
                return Err(InputError::Busy);
            }
            Err(_) => return Err(InputError::Unavailable),
        };
        if !self.is_alive() {
            self.refund_rate(batch.bytes.len());
            return Ok(InputOutcome::Exited);
        }
        if batch.instance_id != self.instance_id {
            self.refund_rate(batch.bytes.len());
            return Ok(InputOutcome::WrongInstance);
        }
        if let Err(error) = self.ensure_writer_idle() {
            self.refund_rate(batch.bytes.len());
            return Err(error);
        }
        if let Some(outcome) = self.reserve_batch(batch.client_id, batch.seq)? {
            self.refund_rate(batch.bytes.len());
            return Ok(outcome);
        }
        let result = match text {
            Some(text) => self
                .type_text_locked(text, Duration::ZERO, false)
                .and_then(|outcome| match outcome {
                    InputSubmitOutcome::Late => Err(AgentError::SubmitPending),
                    _ => Ok(()),
                })
                .map_err(|error| match error {
                    // The text is already delivered. Even if the Return was
                    // refused before writing, releasing this reservation would
                    // let a retry replay the text body.
                    AgentError::LateSubmitAbandoned { .. } => AgentError::SubmitUncertain,
                    error => error,
                }),
            None => self.write_one_burst(batch.bytes),
        };
        self.finish_input_batch(batch, result)
    }

    fn batch_result(
        &self,
        client_id: [u8; 16],
        seq: u64,
    ) -> core::result::Result<Option<InputOutcome>, InputError> {
        self.input_dedup
            .lock()
            .map(|mut deduplicator| deduplicator.lookup(client_id, seq))
            .map_err(|_| InputError::Unavailable)
    }

    fn reserve_batch(
        &self,
        client_id: [u8; 16],
        seq: u64,
    ) -> core::result::Result<Option<InputOutcome>, InputError> {
        let Ok(mut deduplicator) = self.input_dedup.lock() else {
            return Ok(Some(InputOutcome::Uncertain));
        };
        if let Some(outcome) = deduplicator.lookup(client_id, seq) {
            return Ok(Some(outcome));
        }
        if deduplicator.reserve(client_id, seq) {
            Ok(None)
        } else {
            Ok(Some(
                deduplicator
                    .lookup(client_id, seq)
                    .unwrap_or(InputOutcome::Uncertain),
            ))
        }
    }

    fn finish_input_batch(
        &self,
        batch: InputBatch<'_>,
        result: Result<()>,
    ) -> core::result::Result<InputOutcome, InputError> {
        match result {
            Ok(()) => {
                if let Ok(mut deduplicator) = self.input_dedup.lock() {
                    deduplicator.complete(batch.client_id, batch.seq, true);
                }
                Ok(InputOutcome::Ack { duplicate: false })
            }
            Err(AgentError::Busy | AgentError::LateSubmitAbandoned { .. }) => {
                if let Ok(mut deduplicator) = self.input_dedup.lock() {
                    deduplicator.release(batch.client_id, batch.seq);
                }
                self.refund_rate(batch.bytes.len());
                Err(InputError::Busy)
            }
            Err(AgentError::Exited) => {
                if let Ok(mut deduplicator) = self.input_dedup.lock() {
                    deduplicator.release(batch.client_id, batch.seq);
                }
                self.refund_rate(batch.bytes.len());
                Ok(InputOutcome::Exited)
            }
            Err(_) => {
                if let Ok(mut deduplicator) = self.input_dedup.lock() {
                    deduplicator.complete(batch.client_id, batch.seq, false);
                }
                Ok(InputOutcome::Uncertain)
            }
        }
    }

    fn ensure_writer_idle(&self) -> core::result::Result<(), InputError> {
        if self
            .input_writer_timed_out()
            .map_err(|_| InputError::Unavailable)?
        {
            Err(InputError::Busy)
        } else {
            Ok(())
        }
    }

    /// Above this, a `feed` act is refused rather than executed — a caller's
    /// seconds/millis mixup must not hold an Image hostage indefinitely. See
    /// PRINCIPLES.md §6; settle pauses run ~0.1s, so this leaves ample room.
    pub const MAX_TOTAL_PAUSE: Duration = Duration::from_secs(5);

    /// Deliver a sequence of bursts and pauses as one indivisible input act —
    /// `input_lock` spans the whole thing, but a pause holds no lock
    /// `screen_text`/`capture` need. See PRINCIPLES.md §6 for why.
    pub fn feed(&self, steps: &[Step]) -> Result<()> {
        let total_pause: Duration = steps
            .iter()
            .filter_map(|step| match step {
                Step::Pause(millis) => Some(Duration::from_millis(*millis)),
                Step::Burst(_) => None,
            })
            .sum();
        if total_pause > Self::MAX_TOTAL_PAUSE {
            return Err(AgentError::PauseTooLong {
                total: total_pause,
                cap: Self::MAX_TOTAL_PAUSE,
            });
        }

        let _held = self.acquire_input_lock()?;
        for step in steps {
            match step {
                Step::Burst(bytes) => self.write_one_burst(bytes)?,
                Step::Pause(millis) => self.clock.sleep(Duration::from_millis(*millis)),
            }
        }
        Ok(())
    }

    /// The one place that touches the backend. PTY handles wait without the
    /// process mutex; callers hold `input_lock` except an attachment writer.
    fn write_one_burst(&self, bytes: &[u8]) -> Result<()> {
        self.write_one_burst_with(bytes, BurstWriteMode::Bounded, &|| false, None)
    }

    fn write_one_burst_if_idle(&self, bytes: &[u8]) -> Result<()> {
        self.write_one_burst_with(bytes, BurstWriteMode::IfIdle, &|| false, None)
    }

    fn write_one_burst_to_completion_while_started(
        &self,
        bytes: &[u8],
        cancelled: &dyn Fn() -> bool,
        on_start: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<()> {
        self.write_one_burst_with(
            bytes,
            BurstWriteMode::ToCompletion,
            cancelled,
            Some(on_start),
        )
    }

    fn write_one_burst_with(
        &self,
        bytes: &[u8],
        mode: BurstWriteMode,
        cancelled: &dyn Fn() -> bool,
        on_start: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<()> {
        let writer = {
            let mut agent = self
                .agent
                .lock()
                .map_err(|_| AgentError::Io("session lock poisoned".into()))?;
            if !self.alive.load(Ordering::SeqCst) || !self.observe_alive(agent.is_alive()) {
                return Err(AgentError::Exited);
            }
            agent.input_writer()
        };
        let result = if let Some(writer) = writer {
            match mode {
                BurstWriteMode::ToCompletion => {
                    if let Some(on_start) = on_start {
                        writer.write_to_completion_while_started(bytes, cancelled, on_start)
                    } else {
                        writer.write_to_completion_while(bytes, cancelled)
                    }
                }
                BurstWriteMode::IfIdle => writer.write_bounded_if_idle(bytes),
                BurstWriteMode::Bounded => writer.write_bounded(bytes),
            }
        } else {
            let mut agent = self
                .agent
                .lock()
                .map_err(|_| AgentError::Io("session lock poisoned".into()))?;
            if let Some(on_start) = on_start {
                on_start();
            }
            agent.write(bytes)
        };
        if let Err(error) = result {
            return if matches!(error, AgentError::Exited) || !self.is_alive() {
                Err(AgentError::Exited)
            } else {
                Err(error)
            };
        }

        // Recorded only after the write lands, so a failed write does not look
        // like delivered input.
        if let Ok(mut at) = self.last_input_at.lock() {
            *at = self.clock.now();
        }
        Ok(())
    }

    fn acquire_input_lock(&self) -> Result<InputActGuard<'_>> {
        let mut locked = self
            .input_lock
            .lock()
            .map_err(|_| AgentError::Io("session input lock poisoned".into()))?;
        let deadline = self.clock.now() + Duration::from_millis(500);
        loop {
            if !*locked {
                *locked = true;
                return Ok(InputActGuard { session: self });
            }
            if self.input_writer_timed_out()? {
                return Err(AgentError::Busy);
            }
            let remaining = deadline.saturating_sub(self.clock.now());
            if remaining.is_zero() {
                return Err(AgentError::Busy);
            }
            locked = self
                .input_ready
                .wait_timeout(locked, remaining.min(Duration::from_millis(10)))
                .map_err(|_| AgentError::Io("session input lock poisoned".into()))?
                .0;
        }
    }

    fn input_writer_timed_out(&self) -> Result<bool> {
        let mut agent = self
            .agent
            .lock()
            .map_err(|_| AgentError::Io("session lock poisoned".into()))?;
        Ok(agent
            .input_writer()
            .is_some_and(|writer| writer.is_timed_out()))
    }

    pub fn screen_text(&self) -> Result<String> {
        let mut agent = self
            .agent
            .lock()
            .map_err(|_| AgentError::Io("session lock poisoned".into()))?;
        agent.screen_text()
    }

    pub fn mouse_tracking(&self) -> bool {
        self.agent
            .lock()
            .map(|mut agent| agent.mouse_tracking())
            .unwrap_or(false)
    }

    pub fn mouse_state(&self) -> MouseState {
        self.agent
            .lock()
            .map(|mut agent| agent.mouse_state())
            .unwrap_or_default()
    }

    pub fn cursor(&self) -> Result<Cursor> {
        let mut agent = self
            .agent
            .lock()
            .map_err(|_| AgentError::Io("session lock poisoned".into()))?;
        agent.cursor()
    }

    pub fn screen_cells(&self) -> Result<Vec<Vec<StyledCell>>> {
        let mut agent = self
            .agent
            .lock()
            .map_err(|_| AgentError::Io("session lock poisoned".into()))?;
        agent.screen_cells()
    }

    pub fn screen_cells_at(&self, scrollback: usize) -> Result<Vec<Vec<StyledCell>>> {
        let mut agent = self
            .agent
            .lock()
            .map_err(|_| AgentError::Io("session lock poisoned".into()))?;
        agent.screen_cells_at(scrollback)
    }

    pub fn scrollback_len(&self) -> usize {
        self.agent
            .lock()
            .map(|mut agent| agent.scrollback_len())
            .unwrap_or(0)
    }

    pub fn scrollback_total(&self) -> usize {
        self.agent
            .lock()
            .map(|mut agent| agent.scrollback_total())
            .unwrap_or(0)
    }

    pub fn row_wrapped_at(&self, scrollback: usize) -> Result<Vec<bool>> {
        let mut agent = self
            .agent
            .lock()
            .map_err(|_| AgentError::Io("session lock poisoned".into()))?;
        agent.row_wrapped_at(scrollback)
    }

    /// A screen and its output generation from one session-lock interval.
    pub fn screen_snapshot_at(&self, scrollback: usize) -> Result<ScreenSnapshot> {
        Ok(self.screen_snapshot_version_at(scrollback)?.snapshot)
    }

    pub fn screen_snapshot_version_at(&self, scrollback: usize) -> Result<VersionedSnapshot> {
        let mut agent = self
            .agent
            .lock()
            .map_err(|_| AgentError::Io("session lock poisoned".into()))?;
        let mut snapshot = agent.screen_snapshot_at(scrollback)?;
        snapshot.instance_id = Some(self.instance_id.clone());
        Ok(snapshot)
    }

    /// Wait for a screen version newer than `since`, or return the current
    /// atomic frame when the deadline expires. The condition lock is released
    /// by `wait_timeout`; the agent/screen lock is held only for each snapshot.
    pub fn wait_for_output_after(
        &self,
        since: u64,
        timeout: Duration,
    ) -> Result<VersionedSnapshot> {
        let deadline = self.clock.now().saturating_add(timeout);
        let (generation, wake) = &*self.output_changed;
        let mut guard = generation
            .lock()
            .map_err(|_| AgentError::Io("output wait lock poisoned".into()))?;
        loop {
            let observed = *guard;
            let snapshot = self.screen_snapshot_version_at(0)?;
            if snapshot.output_version.unwrap_or(0) > since {
                return Ok(snapshot);
            }
            // Drop the wait lock while checking process liveness. A process
            // exit is also signalled by the registry reaper and terminate().
            drop(guard);
            if !self.is_alive() {
                return Err(AgentError::Exited);
            }
            guard = generation
                .lock()
                .map_err(|_| AgentError::Io("output wait lock poisoned".into()))?;
            if *guard != observed {
                continue;
            }
            let remaining = deadline.saturating_sub(self.clock.now());
            if remaining.is_zero() {
                return Ok(snapshot);
            }
            let (next_guard, result) = wake
                .wait_timeout_while(guard, remaining, |current| *current == observed)
                .map_err(|_| AgentError::Io("output wait lock poisoned".into()))?;
            guard = next_guard;
            if result.timed_out() {
                return self.screen_snapshot_version_at(0);
            }
        }
    }

    pub fn is_alive(&self) -> bool {
        if !self.alive.load(Ordering::SeqCst) {
            Self::notify_output_changed(&self.output_changed);
            return false;
        }
        let alive = match self.agent.lock() {
            Ok(mut agent) => {
                self.alive.load(Ordering::SeqCst) && self.observe_alive(agent.is_alive())
            }
            // A poisoned lock means a writer panicked mid-session. Reporting
            // "alive" would invite more writes into a session whose state is
            // unknown.
            Err(_) => {
                self.alive.store(false, Ordering::SeqCst);
                false
            }
        };
        if !alive {
            Self::notify_output_changed(&self.output_changed);
        }
        alive
    }

    fn observe_alive(&self, alive: bool) -> bool {
        if !alive {
            self.alive.store(false, Ordering::SeqCst);
        }
        self.alive.load(Ordering::SeqCst)
    }

    fn notify_output_changed(output_changed: &Arc<(Mutex<u64>, Condvar)>) {
        if let Ok(mut generation) = output_changed.0.lock() {
            *generation = (*generation).wrapping_add(1);
            output_changed.1.notify_all();
        }
    }

    /// Whether an explicit close request has claimed this session.
    pub fn is_closing(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
    }

    pub(crate) fn mark_closing(&self) {
        self.closing.store(true, Ordering::SeqCst);
    }

    pub(crate) fn clear_closing(&self) {
        self.closing.store(false, Ordering::SeqCst);
    }

    /// The process exit information observed by this session's backend, if known.
    pub fn exit_info(&self) -> Option<ExitInfo> {
        self.agent.lock().ok()?.exit_info()
    }

    /// The child PID if this process-backed session is still running.
    pub fn process_id_if_alive(&self) -> Option<u32> {
        if !self.alive.load(Ordering::SeqCst) {
            return None;
        }
        let mut agent = match self.agent.lock() {
            Ok(agent) => agent,
            Err(_) => {
                self.alive.store(false, Ordering::SeqCst);
                return None;
            }
        };
        (self.alive.load(Ordering::SeqCst) && self.observe_alive(agent.is_alive()))
            .then(|| agent.process_id())
            .flatten()
    }

    /// Whether the backend holds `pid` as this session's own, even after its
    /// child exits. A query error leaves ownership unknown.
    pub fn owns_process(&self, pid: u32, process_handle: Option<usize>) -> std::io::Result<bool> {
        self.agent
            .lock()
            .map_err(|_| std::io::Error::other("session lock poisoned"))?
            .owns_process(pid, process_handle)
    }

    /// End the child. Refused while attached, and idempotent on an
    /// already-dead agent. Does not remove the session from a registry — the
    /// last screen survives; [`crate::Registry::close`] does both.
    pub fn terminate(&self) -> Result<()> {
        self.terminate_inner(true, true)
    }

    pub(crate) fn terminate_for_close(&self) -> Result<()> {
        self.terminate_inner(false, true)
    }

    pub(crate) fn terminate_for_reap(&self) -> Result<()> {
        self.terminate_inner(false, false)
    }

    fn terminate_inner(&self, wake_waiters: bool, refuse_attached: bool) -> Result<()> {
        if refuse_attached && self.attached.load(Ordering::SeqCst) {
            return Err(AgentError::Attached);
        }
        let mut agent = self
            .agent
            .lock()
            .map_err(|_| AgentError::Io("session lock poisoned".into()))?;
        if self.alive.load(Ordering::SeqCst) {
            self.observe_alive(agent.is_alive());
        }
        let result = agent.terminate();
        if result.is_ok() {
            self.alive.store(false, Ordering::SeqCst);
        } else {
            if self.alive.load(Ordering::SeqCst) {
                self.observe_alive(agent.is_alive());
            }
        }
        drop(agent);
        if result.is_ok() && wake_waiters {
            Self::notify_output_changed(&self.output_changed);
        }
        result
    }

    pub(crate) fn wake_sync_waiters(&self) {
        Self::notify_output_changed(&self.output_changed);
    }

    /// Take hold for a human at a terminal, displacing any current holder.
    pub fn attach(&self) -> Attached<'_> {
        let mut slot = self.attach_slot.lock().unwrap_or_else(|p| p.into_inner());
        let generation = self.next_attach_generation.fetch_add(1, Ordering::SeqCst);
        let displaced = Arc::new(AtomicBool::new(false));
        if let Some((_, old)) = slot.replace((generation, Arc::clone(&displaced))) {
            old.store(true, Ordering::SeqCst);
        }
        self.attached.store(true, Ordering::SeqCst);
        Attached {
            session: self,
            generation,
            displaced,
        }
    }

    /// Whether a human currently holds this session.
    pub fn is_attached(&self) -> bool {
        self.attached.load(Ordering::SeqCst)
    }

    /// Whether a live session has advanced beyond a tracked attachment.
    pub fn was_attachment_taken_over(&self, generation: u64) -> bool {
        self.is_alive()
            && self
                .next_attach_generation
                .load(Ordering::SeqCst)
                .saturating_sub(1)
                > generation
    }

    /// How long since input was last accepted — the fleet's "this one is parked"
    /// signal, read off the injected clock so a test can drive it.
    pub fn idle_for(&self) -> Duration {
        let last = self.last_input_at.lock().map(|d| *d).unwrap_or_default();
        self.clock.now().saturating_sub(last)
    }

    /// How long since the agent last produced output.
    pub fn output_idle_for(&self) -> Duration {
        let last = self.last_output_at.lock().map(|at| *at).unwrap_or_default();
        self.clock.now().saturating_sub(last)
    }

    /// How long since an attached human last typed, or `None` if none has.
    /// Lets a script hold back rather than type over a half-written line.
    pub fn human_idle_for(&self) -> Option<Duration> {
        let last = self.last_human_input_at.lock().ok().and_then(|at| *at)?;
        Some(self.clock.now().saturating_sub(last))
    }
}

struct InputActGuard<'a> {
    session: &'a Session,
}

impl Drop for InputActGuard<'_> {
    fn drop(&mut self) {
        let mut locked = self
            .session
            .input_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *locked = false;
        self.session.input_ready.notify_all();
    }
}

/// Current viewer's hold: the only guard that can write raw bytes. A displaced
/// guard cannot write or release its successor.
pub struct Attached<'a> {
    session: &'a Session,
    generation: u64,
    displaced: Arc<AtomicBool>,
}

impl Attached<'_> {
    /// The generation assigned when this client took the attachment.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether a newer client has taken this attachment over.
    pub fn is_displaced(&self) -> bool {
        self.displaced.load(Ordering::SeqCst)
    }

    /// Whether the active PTY input write has exceeded its bounded deadline.
    pub fn is_writer_timed_out(&self) -> bool {
        self.session
            .agent
            .lock()
            .ok()
            .and_then(|mut agent| agent.input_writer())
            .is_some_and(|writer| writer.is_timed_out())
    }

    /// Type exactly these bytes. No Enter is appended: the human sends their
    /// own, and inventing one here would submit a half-typed line.
    pub fn write_raw(&self, bytes: &[u8]) -> Result<()> {
        self.write_raw_while(bytes, &|| false)
    }

    /// Type bytes unless displaced or cancelled; the slot is released while waiting.
    /// A takeover may still let one pending buffer land after `attach()` returns.
    /// Single-flight writes do not interleave, and the buffer is never replayed.
    pub fn write_raw_while(&self, bytes: &[u8], cancelled: &dyn Fn() -> bool) -> Result<()> {
        self.write_raw_while_started(bytes, cancelled, Arc::new(|| {}))
    }

    /// Write bytes while notifying when the backend starts the PTY write.
    pub fn write_raw_while_started(
        &self,
        bytes: &[u8],
        cancelled: &dyn Fn() -> bool,
        on_start: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<()> {
        {
            let slot = self
                .session
                .attach_slot
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if self.is_displaced()
                || !slot
                    .as_ref()
                    .is_some_and(|(generation, _)| *generation == self.generation)
            {
                return Err(AgentError::Attached);
            }
        }
        let _human_gate = loop {
            match self.session.human_gate.try_lock() {
                Ok(gate) => break gate,
                Err(std::sync::TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    if self.is_displaced() || cancelled() {
                        return Err(AgentError::Attached);
                    }
                    std::thread::yield_now();
                }
            }
        };
        if let Ok(mut at) = self.session.last_human_input_at.lock() {
            *at = Some(self.session.clock.now());
        }
        self.session.human_inflight.fetch_add(1, Ordering::SeqCst);
        drop(_human_gate);
        let _inflight = HumanInflightGuard(&self.session.human_inflight);
        self.session.write_one_burst_to_completion_while_started(
            bytes,
            &|| self.is_displaced() || cancelled(),
            on_start,
        )?;
        // Only after the write lands, as `last_input_at` is.
        if let Ok(mut at) = self.session.last_human_input_at.lock() {
            *at = Some(self.session.clock.now());
        }
        Ok(())
    }

    /// The screen as terminal bytes, for painting on attach. Without this a
    /// viewer sees nothing until the program happens to redraw.
    pub fn screen_bytes(&self) -> Result<Vec<u8>> {
        let mut agent = self
            .session
            .agent
            .lock()
            .map_err(|_| AgentError::Io("session lock poisoned".into()))?;
        agent.screen_bytes()
    }

    /// Output as it arrives. `None` from a backend that cannot stream; the
    /// caller then has `screen_bytes` and nothing is silently lost.
    pub fn subscribe(&self) -> Option<Receiver<Vec<u8>>> {
        let mut agent = self.session.agent.lock().ok()?;
        agent.subscribe()
    }

    pub fn session(&self) -> &Session {
        self.session
    }
}

struct HumanInflightGuard<'a>(&'a AtomicUsize);

impl Drop for HumanInflightGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Drop for Attached<'_> {
    fn drop(&mut self) {
        let mut slot = self
            .session
            .attach_slot
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if slot
            .as_ref()
            .is_some_and(|(generation, _)| *generation == self.generation)
        {
            *slot = None;
            self.session.attached.store(false, Ordering::SeqCst);
        }
    }
}
