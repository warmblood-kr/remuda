//! The seam between "an agent we are driving" and "a real terminal".
//!
//! §⑦ of the design doc calls this the one joint that cannot be added later,
//! for two reasons that turn out to be the same hole:
//!
//!   - tests must run on a machine that has never installed `claude`;
//!   - 정수님, 2026-09-09: "내부가 꼭 claude code가 아니어도 됩니다."
//!
//! Drill the hole once and both are satisfied: a scripted double for tests,
//! and a second vendor later, are the same substitution.

use core::fmt;
use std::sync::mpsc::Receiver;

/// Terminal dimensions. Immutable by construction — see [`Size::new`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Size {
    cols: u16,
    rows: u16,
}

impl Size {
    /// Below this, real TUIs stop rendering their input line and keystrokes
    /// are silently dropped. Measured on the Emacs implementation: a narrow
    /// split spawned a session at 11 columns and input vanished with no error.
    pub const MIN_COLS: u16 = 80;
    pub const MIN_ROWS: u16 = 24;

    /// Clamps up to the floor. A caller cannot construct a size that drops
    /// keystrokes, so no downstream code has to remember to check.
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols: cols.max(Self::MIN_COLS),
            rows: rows.max(Self::MIN_ROWS),
        }
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }
}

impl Default for Size {
    fn default() -> Self {
        Self::new(Self::MIN_COLS, Self::MIN_ROWS)
    }
}

/// Cursor position, zero-based.
///
/// The column is load-bearing and not decoration. The Emacs implementation
/// distinguishes text a human actually typed from a greyed-out completion
/// suggestion by cursor column, and its source records that colour-based
/// detection failed three times before that. A backend that renders a grid
/// but cannot report the cursor makes ghost-text detection structurally
/// impossible — which is invisible in an MVP and fires the first time the
/// core sends an instruction to a real worker.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cursor {
    pub row: u16,
    pub col: u16,
}

#[derive(Debug)]
pub enum AgentError {
    /// The child is gone. Distinct from an I/O failure: a caller may
    /// legitimately want to reap and respawn rather than propagate.
    Exited,
    /// Someone is attached and driving this session by hand. Orchestrated
    /// input is refused rather than queued — see [`crate::session::Session`].
    Attached,
    Io(String),
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentError::Exited => write!(f, "agent process has exited"),
            AgentError::Attached => write!(f, "a human is attached to this session"),
            AgentError::Io(m) => write!(f, "agent io error: {m}"),
        }
    }
}

pub type Result<T> = core::result::Result<T, AgentError>;

/// A live agent process presented as a screen we can read and a keyboard we
/// can type on.
///
/// Every method is synchronous, and that is a correctness requirement rather
/// than a simplification. The one dangerous interleaving in this whole design
/// is a body write and its Enter landing as two separately-awaited futures,
/// with a second writer scheduled in between. Keeping the trait sync means
/// that shape cannot be written, in any of the embedded languages evaluated.
pub trait AgentProcess: Send {
    /// Type raw bytes. Not public API on [`Session`] — see
    /// [`crate::session::Session::send_line`] for why callers never get this.
    fn write(&mut self, bytes: &[u8]) -> Result<()>;

    /// The visible screen, rendered as text, newline-separated.
    fn screen_text(&mut self) -> Result<String>;

    /// The visible screen as terminal bytes — escape sequences, colour and
    /// all — for painting onto a real terminal that has just attached.
    ///
    /// `screen_text` is for a machine reading the screen; this is for a human
    /// looking at it. Default: the text, which is correct but colourless.
    fn screen_bytes(&mut self) -> Result<Vec<u8>> {
        self.screen_text().map(String::into_bytes)
    }

    /// Subscribe to output as it arrives, for a viewer that must not poll.
    ///
    /// `None` means this backend cannot stream, which is the honest answer for
    /// a scripted double and the reason this is not a required method. A
    /// caller that gets `None` still has `screen_bytes`.
    fn subscribe(&mut self) -> Option<Receiver<Vec<u8>>> {
        None
    }

    fn cursor(&mut self) -> Result<Cursor>;

    fn is_alive(&mut self) -> bool;

    /// The size fixed at spawn. There is deliberately no setter; see
    /// [`crate::session::Session`].
    fn size(&self) -> Size;
}

/// An agent that answers from a script instead of running anything.
///
/// This is the reason the suite needs no `claude` binary, no pty, and no
/// network. It also records every byte written, which is what makes the
/// atomicity of `send_line` observable rather than merely asserted.
pub struct ScriptedAgent {
    /// Screens handed out in order; the last one repeats once exhausted.
    screens: Vec<String>,
    next_screen: usize,
    cursor: Cursor,
    alive: bool,
    size: Size,
    /// Every `write` call, in order and unmerged. Kept as separate entries
    /// rather than one buffer so a test can see whether two writers
    /// interleaved, which a concatenated buffer would hide.
    pub writes: Vec<Vec<u8>>,
}

impl ScriptedAgent {
    pub fn new(screens: Vec<String>) -> Self {
        Self {
            screens,
            next_screen: 0,
            cursor: Cursor { row: 0, col: 0 },
            alive: true,
            size: Size::default(),
            writes: Vec::new(),
        }
    }

    pub fn with_size(mut self, size: Size) -> Self {
        self.size = size;
        self
    }

    pub fn set_cursor(&mut self, cursor: Cursor) {
        self.cursor = cursor;
    }

    pub fn kill(&mut self) {
        self.alive = false;
    }

    /// Everything written so far, flattened. Convenient for assertions that
    /// do not care about call boundaries.
    pub fn written(&self) -> Vec<u8> {
        self.writes.iter().flatten().copied().collect()
    }
}

impl AgentProcess for ScriptedAgent {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if !self.alive {
            return Err(AgentError::Exited);
        }
        self.writes.push(bytes.to_vec());
        Ok(())
    }

    fn screen_text(&mut self) -> Result<String> {
        if self.screens.is_empty() {
            return Ok(String::new());
        }
        let i = self.next_screen.min(self.screens.len() - 1);
        self.next_screen += 1;
        Ok(self.screens[i].clone())
    }

    fn cursor(&mut self) -> Result<Cursor> {
        Ok(self.cursor)
    }

    fn is_alive(&mut self) -> bool {
        self.alive
    }

    fn size(&self) -> Size {
        self.size
    }
}
