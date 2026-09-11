//! A session: one agent process, owned by the core, outliving every viewer.
//!
//! Three invariants live here, and all of them are enforced by what this type
//! does — or does not — expose, rather than by anyone remembering a rule. The
//! third is the interesting one: it does not forbid the dangerous operation, it
//! scopes it to the only situation in which it is safe.

use crate::agent::{AgentError, AgentProcess, Cursor, Result, Size, StyledCell};
use crate::clock::Clock;
use crate::protocol::Step;
use core::time::Duration;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

/// A running agent, addressable by name. Three invariants, enforced by what
/// this type does not expose: (1) no *divisible* write, so one input act cannot
/// be split; (2) no resize; (3) raw keystrokes only on an [`Attached`] guard.
pub struct Session {
    name: String,
    agent: Mutex<Box<dyn AgentProcess>>,
    size: Size,
    clock: Arc<dyn Clock>,
    /// Reading of [`Clock::now`] taken at the last successful `send_line`.
    /// Meaningful only as a difference against a later reading.
    last_input_at: Mutex<Duration>,
    /// Set while an [`Attached`] guard is alive. Separate from the agent lock so
    /// two orchestrated `send_line`s still serialize against each other without
    /// the second reporting a spurious "busy".
    attached: AtomicBool,
    /// Bumped every time `attach` succeeds. A `feed` act records this at
    /// start; a burst refuses if it has since changed, even if `attached` is
    /// false again by then — an attach-then-detach fully inside one `Pause`.
    attach_generation: AtomicU64,
    /// Held for the whole of one input act — every `Burst` **and** every
    /// `Pause` between them — so a second sender cannot land a write during a
    /// pause, when the `agent` lock is briefly free. See [`Self::feed`].
    input_lock: Mutex<()>,
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
        let size = agent.size();
        let started = clock.now();
        Self {
            name: name.into(),
            agent: Mutex::new(agent),
            size,
            clock,
            last_input_at: Mutex::new(started),
            attached: AtomicBool::new(false),
            attach_generation: AtomicU64::new(0),
            input_lock: Mutex::new(()),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The size fixed when the agent was spawned. Read-only by design; see
    /// invariant 2 on this type.
    pub fn size(&self) -> Size {
        self.size
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
        self.write_one_burst(bytes, None)
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
        // Recorded once, after the lock is held so nothing can attach and
        // bump it before this act's own baseline is fixed — see
        // `write_one_burst`, which refuses a burst the moment this changes,
        // even across a `Pause` where a human attached and already detached.
        let started_at_generation = self.attach_generation.load(Ordering::SeqCst);
        for step in steps {
            match step {
                Step::Burst(bytes) => self.write_one_burst(bytes, Some(started_at_generation))?,
                Step::Pause(millis) => self.clock.sleep(Duration::from_millis(*millis)),
            }
        }
        Ok(())
    }

    /// The one place that actually touches the pty. Callers hold `input_lock`
    /// before calling this — it does not take that lock itself, since `feed`
    /// needs to call it once per burst without releasing it in between.
    fn write_one_burst(&self, bytes: &[u8], started_at_generation: Option<u64>) -> Result<()> {
        // `None` (a bare `send`) checks only the live flag — one burst has no
        // pause for an attach-then-detach to hide inside. `Some` (a `feed`
        // burst) also refuses if attachment happened at any point since the
        // act started, even if it is not held any more by the time this runs.
        let attached_now = self.attached.load(Ordering::SeqCst);
        let attached_during_act = started_at_generation
            .is_some_and(|gen| self.attach_generation.load(Ordering::SeqCst) != gen);
        if attached_now || attached_during_act {
            return Err(AgentError::Attached);
        }

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

    pub fn is_alive(&self) -> bool {
        match self.agent.lock() {
            Ok(mut agent) => agent.is_alive(),
            // A poisoned lock means a writer panicked mid-session. Reporting
            // "alive" would invite more writes into a session whose state is
            // unknown.
            Err(_) => false,
        }
    }

    /// End the child. Refused while attached, as `send` is, and idempotent on an
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

    /// Take exclusive hold for a human at a terminal. `None` means someone is
    /// already attached; the second attacher is refused, never merged in.
    pub fn attach(&self) -> Option<Attached<'_>> {
        self.attached
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| {
                // Bumped on every successful attach, never on a refused one —
                // `feed` compares against this to catch an attach-then-detach
                // that happened entirely inside one of its `Pause`s.
                self.attach_generation.fetch_add(1, Ordering::SeqCst);
                Attached { session: self }
            })
    }

    /// Whether a human currently holds this session.
    pub fn is_attached(&self) -> bool {
        self.attached.load(Ordering::SeqCst)
    }

    /// How long since input was last accepted — the fleet's "this one is parked"
    /// signal, read off the injected clock so a test can drive it.
    pub fn idle_for(&self) -> Duration {
        let last = self.last_input_at.lock().map(|d| *d).unwrap_or_default();
        self.clock.now().saturating_sub(last)
    }
}

/// Exclusive hold on a session by one attached viewer. Caution: this guard is
/// the *only* place raw bytes can be written (invariant 3). Dropping it
/// detaches and re-enables `send_line`; the agent and its pty are undisturbed.
pub struct Attached<'a> {
    session: &'a Session,
}

impl Attached<'_> {
    /// Type exactly these bytes. No Enter is appended: the human sends their
    /// own, and inventing one here would submit a half-typed line.
    pub fn write_raw(&self, bytes: &[u8]) -> Result<()> {
        let mut agent = self
            .session
            .agent
            .lock()
            .map_err(|_| AgentError::Io("session lock poisoned".into()))?;
        agent.write(bytes)
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
        self.session.attached.store(false, Ordering::SeqCst);
    }
}
