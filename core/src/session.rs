//! A session: one agent process, owned by the core, outliving every viewer.
//!
//! Three invariants live here, and all of them are enforced by what this type
//! does — or does not — expose, rather than by anyone remembering a rule. The
//! third is the interesting one: it does not forbid the dangerous operation, it
//! scopes it to the only situation in which it is safe.

use crate::agent::{AgentError, AgentProcess, Cursor, Result, Size};
use crate::clock::Clock;
use core::time::Duration;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

/// A running agent, addressable by name.
///
/// # Invariant 1 — one input act cannot be split by a second writer
///
/// [`Self::send`] and [`Self::send_line`] are the only ways to put input into a
/// session, and each delivers its whole burst under a single lock acquisition.
/// There is no *divisible* write — no method that takes the lock and hands it
/// back part-way through an act — so no caller can send a body, lose the lock,
/// and have another writer's Enter submit it. The Emacs implementation had
/// exactly this bug shape: a stray submit landed on whatever was highlighted,
/// and the intended text was swallowed with both sides reporting success.
///
/// This was first written as "there is no public raw write", which described
/// the one atom that existed at the time rather than the property. Widening the
/// atom to any byte burst (2026-09-10, for the Lua input vocabulary) left the
/// property untouched and made the earlier wording read like a repeal, which is
/// why it now names divisibility instead.
///
/// # Invariant 2 — nobody can resize the pty
///
/// There is no `resize`, and [`Size`] is immutable once constructed. Attaching
/// is meant to be routine — it is how a human logs the agent in — and in
/// zellij, whose behaviour was measured for this design, attaching resizes the
/// shared session down to the smallest client. Here an attacher gets a view
/// and may scroll or crop; the program underneath never sees SIGWINCH.
///
/// # Invariant 3 — raw keystrokes exist only while exactly one human holds it
///
/// A human at an attached terminal types Enter themselves, so attaching needs
/// the very raw write invariant 1 refuses to expose. The two are reconciled by
/// *exclusivity* rather than by a rule: [`Self::attach`] hands out an
/// [`Attached`] guard, at most one at a time, and `write_raw` lives **only on
/// that guard**. While it is held, [`Self::send_line`] returns
/// [`AgentError::Attached`] instead of queueing.
///
/// Refusing is the point. Orchestrated input landing in a session a person is
/// driving is the exact fleet incident this design exists to prevent: a stray
/// submit lands on whatever the human had highlighted. "The core is busy" is
/// information the caller can act on; a silently interleaved keystroke is not.
pub struct Session {
    name: String,
    agent: Mutex<Box<dyn AgentProcess>>,
    size: Size,
    clock: Arc<dyn Clock>,
    /// Reading of [`Clock::now`] taken at the last successful `send_line`.
    /// Meaningful only as a difference against a later reading.
    last_input_at: Mutex<Duration>,
    /// Set while an [`Attached`] guard is alive. Separate from the agent lock
    /// on purpose: two orchestrated `send_line`s must still serialize against
    /// each other, and folding this into that lock would turn the second one
    /// into a spurious "busy".
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
    ///
    /// Carriage return, not newline: a pty in canonical mode takes CR as submit,
    /// and TUIs that read raw keys expect the same byte a real Enter key
    /// produces. That decision lives here and nowhere else, which is the reason
    /// this stays its own method rather than becoming a caller's `+ "\r"`.
    pub fn send_line(&self, text: &str) -> Result<()> {
        let mut line = Vec::with_capacity(text.len() + 1);
        line.extend_from_slice(text.as_bytes());
        line.push(b'\r');
        self.send(&line)
    }

    /// Deliver a burst of input as one indivisible act, appending nothing.
    ///
    /// This is the atom, and [`Self::send_line`] is one particular burst. It is
    /// what every keystroke that is not a line of text has to go through — an
    /// arrow key, a Ctrl chord, a mouse report.
    ///
    /// **Why this does not repeal invariant 1.** That invariant's own reason is
    /// that no caller may "send a body, lose the lock, and have another writer's
    /// Enter submit it". The property is the *atomicity of one input act*, not
    /// the absence of raw bytes. A burst is written under a single lock
    /// acquisition with nothing awaiting inside it, so a second sender still
    /// cannot land in the middle of one. The operation that does not exist here
    /// is a **divisible** write, and it still cannot be written: there is no
    /// method that takes the lock and gives it back part-way through an act.
    ///
    /// What genuinely changes is that a half-typed line can now be left sitting
    /// at a prompt. That is a script doing a deliberate thing, in the same class
    /// as `sh script.sh`, and not the concurrency defect this type was built
    /// against.
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

    pub fn is_alive(&self) -> bool {
        match self.agent.lock() {
            Ok(mut agent) => agent.is_alive(),
            // A poisoned lock means a writer panicked mid-session. Reporting
            // "alive" would invite more writes into a session whose state is
            // unknown.
            Err(_) => false,
        }
    }

    /// End the child. Refused while attached, for the same reason `send` is:
    /// tearing the pty out from under a human at an attached terminal is
    /// worse than making them detach first (step 006).
    ///
    /// Idempotent on an already-dead agent — see [`AgentProcess::terminate`].
    /// This does not remove the session from a registry; a caller that only
    /// wants "make sure the process is gone" can call this without also
    /// discarding the last screen. [`crate::Registry::close`] does both.
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

    /// Take exclusive hold for a human at a terminal.
    ///
    /// `None` means someone is already attached. Two people driving one
    /// keyboard is the same defect as the core and a human driving it, so the
    /// second attacher is refused rather than merged in.
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

    /// How long since input was last accepted.
    ///
    /// Idleness is the fleet's signal for "this one is parked", so it must be
    /// drivable by a test rather than by waiting.
    pub fn idle_for(&self) -> Duration {
        let last = self.last_input_at.lock().map(|d| *d).unwrap_or_default();
        self.clock.now().saturating_sub(last)
    }
}

/// Exclusive hold on a session by one attached viewer.
///
/// This guard is the *only* place raw bytes can be written. That is what
/// reconciles invariant 1 (no public raw write) with a human who must type
/// their own Enter: the dangerous operation is not forbidden, it is scoped to
/// the one situation where exactly one writer exists. Dropping the guard
/// detaches, and the core's `send_line` starts working again.
///
/// Detaching does not disturb the agent — the process keeps running, and the
/// pty keeps the size it was spawned with (invariant 2), so nothing about the
/// program underneath can tell that a viewer came and went.
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
