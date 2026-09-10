//! A session: one agent process, owned by the core, outliving every viewer.
//!
//! Three invariants live here, and all of them are enforced by what this type
//! does — or does not — expose, rather than by anyone remembering a rule. The
//! third is the interesting one: it does not forbid the dangerous operation, it
//! scopes it to the only situation in which it is safe.

use crate::agent::{AgentError, AgentProcess, Cursor, Result, Size, StyledCell};
use crate::clock::Clock;
use core::time::Duration;
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// atom [`Self::send_line`] is one case of. Written under a single lock
    /// acquisition, so a second sender cannot land in the middle of a burst.
    pub fn send(&self, bytes: &[u8]) -> Result<()> {
        if self.attached.load(Ordering::SeqCst) {
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
            .map(|_| Attached { session: self })
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
