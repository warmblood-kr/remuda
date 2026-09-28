//! A session: one agent process, owned by the core, outliving every viewer.
//!
//! Input acts stay indivisible, sessions do not resize, and one viewer owns an
//! attachment at a time.

use crate::agent::{
    AgentError, AgentProcess, Cursor, ExitInfo, MouseState, Result, ScreenSnapshot, Size,
    StyledCell, VersionedSnapshot,
};
use crate::clock::Clock;
use crate::input::{InputBatch, InputDeduplicator, InputError, InputOutcome, InputRateLimiter};
use crate::protocol::Step;
use core::time::Duration;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

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
    input_lock: Mutex<()>,
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
        if let Some(output) = agent.subscribe() {
            let last_output_at = Arc::clone(&last_output_at);
            let clock = Arc::clone(&clock);
            std::thread::spawn(move || {
                while output.recv().is_ok() {
                    if let Ok(mut at) = last_output_at.lock() {
                        *at = clock.now();
                    }
                }
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
            instance_id,
            last_human_input_at: Mutex::new(None),
            attached: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            attach_slot: Mutex::new(None),
            next_attach_generation: AtomicU64::new(1),
            input_lock: Mutex::new(()),
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
        Ok(())
    }

    /// Deliver one instruction: the text, then Enter, as one indivisible act.
    /// Carriage return, not newline — canonical mode takes CR as submit, and
    /// raw-key TUIs expect the byte a real Enter produces. Decided only here.
    pub fn send_line(&self, text: &str) -> Result<()> {
        let mut line = Vec::with_capacity(text.len() + 1);
        line.extend_from_slice(text.as_bytes());
        line.push(b'\r');
        self.send(&line)
    }

    /// Deliver a burst of input as one indivisible act, appending nothing — the
    /// one-`Burst` case of [`Self::feed`]. Holds `input_lock`, so it cannot
    /// land inside a `feed` act's pause, nor a `feed` act land inside it.
    pub fn send(&self, bytes: &[u8]) -> Result<()> {
        let _held = self
            .input_lock
            .lock()
            .map_err(|_| AgentError::Io("session lock poisoned".into()))?;
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

    /// Apply a bounded remote batch once for this exact session instance.
    /// Local input shares `input_lock`, so each complete batch stays atomic.
    pub fn apply_input_batch(
        &self,
        batch: InputBatch<'_>,
    ) -> core::result::Result<InputOutcome, InputError> {
        if batch.seq == 0 {
            return Err(InputError::InvalidSequence);
        }
        self.check_rate(batch.bytes.len())?;
        let Ok(_held) = self.input_lock.lock() else {
            return Ok(InputOutcome::Uncertain);
        };
        if !self.is_alive() {
            return Ok(InputOutcome::Exited);
        }
        if batch.instance_id != self.instance_id {
            return Ok(InputOutcome::WrongInstance);
        }
        let Ok(mut deduplicator) = self.input_dedup.lock() else {
            return Ok(InputOutcome::Uncertain);
        };
        Ok(deduplicator.apply(batch.client_id, batch.seq, || {
            self.write_one_burst(batch.bytes)
        }))
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

        let _held = self
            .input_lock
            .lock()
            .map_err(|_| AgentError::Io("session lock poisoned".into()))?;
        for step in steps {
            match step {
                Step::Burst(bytes) => self.write_one_burst(bytes)?,
                Step::Pause(millis) => self.clock.sleep(Duration::from_millis(*millis)),
            }
        }
        Ok(())
    }

    /// The one place that actually touches the pty. Callers hold `input_lock`
    /// before calling this — it does not take that lock itself, since `feed`
    /// needs to call it once per burst without releasing it in between.
    fn write_one_burst(&self, bytes: &[u8]) -> Result<()> {
        let mut agent = self
            .agent
            .lock()
            .map_err(|_| AgentError::Io("session lock poisoned".into()))?;

        agent.write(bytes)?;

        // Recorded only after the write lands, so a failed write does not look
        // like delivered input.
        if let Ok(mut at) = self.last_input_at.lock() {
            *at = self.clock.now();
        }
        Ok(())
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

    pub fn is_alive(&self) -> bool {
        match self.agent.lock() {
            Ok(mut agent) => agent.is_alive(),
            // A poisoned lock means a writer panicked mid-session. Reporting
            // "alive" would invite more writes into a session whose state is
            // unknown.
            Err(_) => false,
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
        if self.attached.load(Ordering::SeqCst) {
            return Err(AgentError::Attached);
        }
        let mut agent = self
            .agent
            .lock()
            .map_err(|_| AgentError::Io("session lock poisoned".into()))?;
        agent.terminate()
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
        let mut agent = self
            .session
            .agent
            .lock()
            .map_err(|_| AgentError::Io("session lock poisoned".into()))?;
        agent.write(bytes)?;
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
