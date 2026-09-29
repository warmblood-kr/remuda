//! A session: one agent process, owned by the core, outliving every viewer.
//!
//! Input acts stay indivisible, sessions do not resize, and one viewer owns an
//! attachment at a time.

use crate::agent::{
    AgentError, AgentProcess, Cursor, ExitInfo, MouseState, OutputWakeup, Result, ScreenSnapshot,
    Size, StyledCell, VersionedSnapshot,
};
use crate::clock::Clock;
use crate::input::{InputBatch, InputDeduplicator, InputError, InputOutcome, InputRateLimiter};
use crate::protocol::Step;
use core::time::Duration;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
}

struct PendingInput {
    tail: String,
    baseline_occurrences: usize,
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
    /// Set while the current [`Attached`] guard is alive.
    attached: AtomicBool,
    /// Set under the registry lock before a close request terminates the child.
    closing: AtomicBool,
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
}

/// Generate a unique session-start identity from host entropy and a process counter.
pub fn generate_instance_id(seed: u128) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("{seed:032x}-{:016x}", NEXT.fetch_add(1, Ordering::Relaxed))
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
        mut agent: Box<dyn AgentProcess>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let instance_id = generate_instance_id(clock.instance_id_seed());
        let size = agent.size();
        let started = clock.now();
        let last_output_at = Arc::new(Mutex::new(started));
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
            id: id.into(),
            name: name.into(),
            agent: Mutex::new(agent),
            size: Mutex::new(size),
            clock,
            last_input_at: Mutex::new(started),
            last_output_at,
            output_changed,
            instance_id,
            last_human_input_at: Mutex::new(None),
            attached: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            attach_slot: Mutex::new(None),
            next_attach_generation: AtomicU64::new(1),
            input_lock: Mutex::new(false),
            input_ready: Condvar::new(),
            pending_input: Mutex::new(None),
            input_dedup: Mutex::new(InputDeduplicator::new()),
            input_rate: Mutex::new(InputRateLimiter::default()),
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
        self.type_text(text, Duration::ZERO).map(|_| ())
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
        self.submit_locked(&tail, baseline)
    }

    /// Deliver TEXT and submit it as one act. This is the composite used by
    /// type_text and SendLine; no other input sender can split the two units.
    pub fn type_text(&self, text: &str, settle: Duration) -> Result<InputSubmitOutcome> {
        let body = normalize_input_text(text);
        if body.is_empty() {
            let _held = self.acquire_input_lock()?;
            if let Ok(mut pending) = self.pending_input.lock() {
                *pending = None;
            }
            self.write_one_burst(crate::keys::RETURN_BYTES)?;
            return Ok(InputSubmitOutcome::Submitted);
        }
        if self.output_version().is_none() || self.screen_text().is_err() {
            let mut bytes = body.into_bytes();
            bytes.extend_from_slice(crate::keys::RETURN_BYTES);
            let _held = self.acquire_input_lock()?;
            self.write_one_burst(&bytes)?;
            return Ok(InputSubmitOutcome::Submitted);
        }

        let tail = input_tail(&body);
        let _held = self.acquire_input_lock()?;
        if let Ok(mut pending) = self.pending_input.lock() {
            *pending = None;
        }
        let baseline = self.tail_occurrences(&tail);
        self.input_text_locked(&body)?;
        if !settle.is_zero() {
            std::thread::sleep(settle);
        }
        self.submit_locked(&tail, baseline)
    }

    fn submit_locked(&self, tail: &str, baseline_occurrences: usize) -> Result<InputSubmitOutcome> {
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
        let visible = self.wait_for_visible_tail(tail, baseline_occurrences, &mut version);
        self.wait_for_screen_quiet(&mut version);
        let before_raw = self.screen_text().unwrap_or_default();
        let before_compact = compact_screen(&before_raw);
        let before_version = self.output_version().unwrap_or(version);
        self.write_one_burst(crate::keys::RETURN_BYTES)?;
        if !visible {
            return Ok(InputSubmitOutcome::Unverified);
        }

        version = before_version;
        let deadline = self.clock.now() + Duration::from_secs(1);
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
        if !composer_newline {
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

    fn wait_for_screen_quiet(&self, version: &mut u64) {
        let deadline = self.clock.now() + Duration::from_millis(300);
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
        self.finish_input_batch(batch, self.write_one_burst(batch.bytes))
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
            Err(AgentError::Busy) => {
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
            .input_writer_busy()
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
        self.write_one_burst_with(bytes, false, &|| false)
    }

    fn write_one_burst_to_completion_while(
        &self,
        bytes: &[u8],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<()> {
        self.write_one_burst_with(bytes, true, cancelled)
    }

    fn write_one_burst_with(
        &self,
        bytes: &[u8],
        wait_to_completion: bool,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<()> {
        let writer = {
            let mut agent = self
                .agent
                .lock()
                .map_err(|_| AgentError::Io("session lock poisoned".into()))?;
            if !agent.is_alive() {
                return Err(AgentError::Exited);
            }
            agent.input_writer()
        };
        let result = if let Some(writer) = writer {
            if wait_to_completion {
                writer.write_to_completion_while(bytes, cancelled)
            } else {
                writer.write_bounded(bytes)
            }
        } else {
            let mut agent = self
                .agent
                .lock()
                .map_err(|_| AgentError::Io("session lock poisoned".into()))?;
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
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            if !*locked {
                *locked = true;
                return Ok(InputActGuard { session: self });
            }
            if self.input_writer_busy()? {
                return Err(AgentError::Busy);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
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

    fn input_writer_busy(&self) -> Result<bool> {
        let mut agent = self
            .agent
            .lock()
            .map_err(|_| AgentError::Io("session lock poisoned".into()))?;
        Ok(agent.input_writer().is_some_and(|writer| writer.is_busy()))
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
        let alive = match self.agent.lock() {
            Ok(mut agent) => agent.is_alive(),
            // A poisoned lock means a writer panicked mid-session. Reporting
            // "alive" would invite more writes into a session whose state is
            // unknown.
            Err(_) => false,
        };
        if !alive {
            Self::notify_output_changed(&self.output_changed);
        }
        alive
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
        let mut agent = self.agent.lock().ok()?;
        agent.is_alive().then(|| agent.process_id()).flatten()
    }

    /// End the child. Refused while attached, and idempotent on an
    /// already-dead agent. Does not remove the session from a registry — the
    /// last screen survives; [`crate::Registry::close`] does both.
    pub fn terminate(&self) -> Result<()> {
        self.terminate_inner(true)
    }

    pub(crate) fn terminate_for_close(&self) -> Result<()> {
        self.terminate_inner(false)
    }

    fn terminate_inner(&self, wake_waiters: bool) -> Result<()> {
        if self.attached.load(Ordering::SeqCst) {
            return Err(AgentError::Attached);
        }
        let mut agent = self
            .agent
            .lock()
            .map_err(|_| AgentError::Io("session lock poisoned".into()))?;
        let result = agent.terminate();
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

    /// Type exactly these bytes. No Enter is appended: the human sends their
    /// own, and inventing one here would submit a half-typed line.
    pub fn write_raw(&self, bytes: &[u8]) -> Result<()> {
        self.write_raw_while(bytes, &|| false)
    }

    /// Type bytes unless displaced or cancelled; the slot is released while waiting.
    /// A takeover may still let one pending buffer land after `attach()` returns.
    /// Single-flight writes do not interleave, and the buffer is never replayed.
    pub fn write_raw_while(&self, bytes: &[u8], cancelled: &dyn Fn() -> bool) -> Result<()> {
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
        self.session
            .write_one_burst_to_completion_while(bytes, &|| self.is_displaced() || cancelled())?;
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
