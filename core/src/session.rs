//! A session: one agent process, owned by the core, outliving every viewer.
//!
//! Two invariants live here, and both are enforced by what this type does
//! *not* expose rather than by anyone remembering a rule.

use crate::agent::{AgentError, AgentProcess, Cursor, Result, Size};
use crate::clock::Clock;
use core::time::Duration;
use std::sync::{Arc, Mutex};

/// A running agent, addressable by name.
///
/// # Invariant 1 — a write and its Enter cannot be separated
///
/// [`Self::send_line`] is the only way to put input into a session. There is
/// no public `write`, so no caller can send a body, lose the lock, and have
/// another writer's Enter submit it. The Emacs implementation had exactly
/// this bug shape: a stray submit landed on whatever was highlighted, and the
/// intended text was swallowed with both sides reporting success.
///
/// # Invariant 2 — nobody can resize the pty
///
/// There is no `resize`, and [`Size`] is immutable once constructed. Attaching
/// is meant to be routine — it is how a human logs the agent in — and in
/// zellij, whose behaviour was measured for this design, attaching resizes the
/// shared session down to the smallest client. Here an attacher gets a view
/// and may scroll or crop; the program underneath never sees SIGWINCH.
pub struct Session {
    name: String,
    agent: Mutex<Box<dyn AgentProcess>>,
    size: Size,
    clock: Arc<dyn Clock>,
    /// Reading of [`Clock::now`] taken at the last successful `send_line`.
    /// Meaningful only as a difference against a later reading.
    last_input_at: Mutex<Duration>,
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
    /// Both writes happen under a single lock acquisition. Nothing awaits in
    /// between because nothing here is async, so no scheduler can interleave
    /// a second writer between the body and its submit.
    pub fn send_line(&self, text: &str) -> Result<()> {
        let mut agent = self
            .agent
            .lock()
            .map_err(|_| AgentError::Io("session lock poisoned".into()))?;

        agent.write(text.as_bytes())?;
        // Carriage return, not newline: a pty in canonical mode takes CR as
        // submit, and TUIs that read raw keys expect the same byte a real
        // Enter key produces.
        agent.write(b"\r")?;

        // Recorded only after both writes land, so a partial write does not
        // look like delivered input.
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

    /// How long since input was last accepted.
    ///
    /// Idleness is the fleet's signal for "this one is parked", so it must be
    /// drivable by a test rather than by waiting.
    pub fn idle_for(&self) -> Duration {
        let last = self.last_input_at.lock().map(|d| *d).unwrap_or_default();
        self.clock.now().saturating_sub(last)
    }
}
