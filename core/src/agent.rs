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
use serde::{Deserialize, Serialize};
use std::sync::mpsc::Receiver;
use std::sync::Arc;

/// Terminal dimensions, normally clamped to the smallest usable interactive
/// terminal. A pane may explicitly retain its narrower visible width.
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

    /// A pane must tell its child the width the user can actually see. This
    /// keeps the ordinary 80-column safety floor everywhere else while
    /// allowing a constrained pane to opt into a narrower terminal.
    pub fn for_pane(cols: u16, rows: u16) -> Self {
        Self {
            cols: cols.max(1),
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

/// Cursor position, zero-based. Caution: the column is load-bearing — ghost
/// text is told from typed input by cursor column, not by colour, so a backend
/// that cannot report the cursor cannot detect it at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Cursor {
    pub row: u16,
    pub col: u16,
    /// The child's own DECTCEM request (`\x1b[?25l`/`\x1b[?25h`) — a hidden
    /// caret must stay hidden in the preview too. See steps/027.
    pub visible: bool,
}

/// A terminal colour, shaped like `vt100::Color` so a backend maps it 1:1.
/// See [`StyledCell`] for why `core` names this at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Color {
    #[default]
    Default,
    Idx(u8),
    Rgb(u8, u8, u8),
}

/// One screen cell, styled. `text` rather than `char`: a combining character
/// or a wide glyph's continuation cell can need more or fewer than one
/// `char`; see [`AgentProcess::screen_cells`] for the plain-text fallback.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct StyledCell {
    pub text: String,
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    /// True for the first (and only rendered) half of a wide CJK character —
    /// it claims 2 display columns. A wide glyph's continuation carries
    /// empty `text` and `wide: false`, so it claims 0. See steps/023.
    pub wide: bool,
}

/// Mouse reporting selected by the child terminal application.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum MouseMode {
    #[default]
    None,
    Press,
    PressRelease,
    ButtonMotion,
    AnyMotion,
}

/// Mouse event encoding selected by the child terminal application.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum MouseEncoding {
    #[default]
    Default,
    Utf8,
    Sgr,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct MouseState {
    pub mode: MouseMode,
    pub encoding: MouseEncoding,
}

#[derive(Debug)]
pub enum AgentError {
    /// The child is gone. Distinct from an I/O failure: a caller may
    /// legitimately want to reap and respawn rather than propagate.
    Exited,
    /// Someone is attached and driving this session by hand. Orchestrated
    /// input is refused rather than queued — see [`crate::session::Session`].
    Attached,
    /// A previous PTY write is still active; no second write was queued.
    Busy,
    /// The bounded write deadline elapsed; bytes may still finish later.
    WriteTimeout {
        timeout: core::time::Duration,
    },
    /// A `feed` act's `Pause`s summed past the caller's cap — refused before
    /// anything is written, not clamped, so a seconds/millis mixup errors
    /// instead of silently running a shorter pause than asked for.
    PauseTooLong {
        total: core::time::Duration,
        cap: core::time::Duration,
    },
    Io(String),
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentError::Exited => write!(f, "agent process has exited"),
            AgentError::Attached => write!(f, "a human is attached to this session"),
            AgentError::Busy => write!(f, "a session input write is already in flight"),
            AgentError::WriteTimeout { timeout } => {
                write!(
                    f,
                    "PTY write exceeded {timeout:?}; delivery may be partial or late"
                )
            }
            AgentError::PauseTooLong { total, cap } => {
                write!(f, "feed's pauses total {total:?}, over the {cap:?} cap")
            }
            AgentError::Io(m) => write!(f, "agent io error: {m}"),
        }
    }
}

pub type Result<T> = core::result::Result<T, AgentError>;

/// A backend writer that can wait independently of the locked process object.
pub trait AgentWriter: Send + Sync {
    fn write_bounded(&self, bytes: &[u8]) -> Result<()>;
    /// Write these bytes once and wait for their actual completion. Interactive
    /// input uses this path so a timeout cannot silently drop a keystroke or
    /// cause a possibly partial write to be replayed.
    fn write_to_completion(&self, bytes: &[u8]) -> Result<()> {
        self.write_bounded(bytes)
    }
    /// As `write_to_completion`, but stop waiting if the caller is no longer
    /// allowed to deliver this input. Backends with blocking completion paths
    /// should poll `cancelled` while waiting.
    fn write_to_completion_while(&self, bytes: &[u8], cancelled: &dyn Fn() -> bool) -> Result<()> {
        if cancelled() {
            return Err(AgentError::Attached);
        }
        self.write_to_completion(bytes)
    }
    fn is_busy(&self) -> bool;
}

/// Exit information retained by a process-backed agent after it is reaped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExitInfo {
    pub exit_code: Option<u32>,
    pub signal: Option<i32>,
    pub signal_name: Option<String>,
}

/// A styled screen and its scrollback measurements from one parser snapshot.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ScreenSnapshot {
    pub cells: Vec<Vec<StyledCell>>,
    pub wrapped: Vec<bool>,
    pub cursor: Cursor,
    pub scrollback_len: usize,
    pub scrollback_total: usize,
}

/// A captured screen with the session identity and generation captured with it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct VersionedSnapshot {
    pub snapshot: ScreenSnapshot,
    pub output_version: Option<u64>,
    pub instance_id: Option<String>,
}

/// A live agent process: a screen we can read, a keyboard we can type on.
/// Caution: every method is sync for correctness, not simplicity — awaiting a
/// body write and its Enter separately lets a second writer land between them.
pub trait AgentProcess: Send {
    /// Type raw bytes. Not public API on [`Session`] — see
    /// [`crate::session::Session::send_line`] for why callers never get this.
    fn write(&mut self, bytes: &[u8]) -> Result<()>;

    /// An optional writer handle that can outlive the process lock while it
    /// waits for a bounded PTY write. Simpler agents keep using `write`.
    fn input_writer(&mut self) -> Option<Arc<dyn AgentWriter>> {
        None
    }

    /// The visible screen, rendered as text, newline-separated.
    fn screen_text(&mut self) -> Result<String>;

    /// The visible screen as terminal bytes — escapes, colour and all — for
    /// painting onto a terminal that has just attached. Default: the text,
    /// which is correct but colourless.
    fn screen_bytes(&mut self) -> Result<Vec<u8>> {
        self.screen_text().map(String::into_bytes)
    }

    /// Whether the child has enabled terminal mouse reporting. Backends that
    /// cannot know return false so callers do not inject unsolicited input.
    fn mouse_tracking(&mut self) -> bool {
        false
    }

    fn mouse_state(&mut self) -> MouseState {
        MouseState::default()
    }

    /// The operating-system process ID for a process-backed agent.
    /// Non-process implementations return `None`.
    fn process_id(&self) -> Option<u32> {
        None
    }

    /// The visible screen as styled cells, for a croppable colour pane.
    /// Default: every cell plain, from the same text `screen_text` gives —
    /// a backend that hasn't implemented styling degrades to colourless.
    fn screen_cells(&mut self) -> Result<Vec<Vec<StyledCell>>> {
        Ok(self
            .screen_text()?
            .lines()
            .map(|l| {
                l.chars()
                    .map(|c| StyledCell {
                        text: c.to_string(),
                        ..Default::default()
                    })
                    .collect()
            })
            .collect())
    }

    fn screen_cells_at(&mut self, _scrollback: usize) -> Result<Vec<Vec<StyledCell>>> {
        self.screen_cells()
    }

    /// Number of rows currently retained above the live screen.
    fn scrollback_len(&mut self) -> usize {
        0
    }

    /// Total rows that have scrolled into history over this process lifetime.
    fn scrollback_total(&mut self) -> usize {
        self.scrollback_len()
    }

    fn row_wrapped_at(&mut self, _scrollback: usize) -> Result<Vec<bool>> {
        Ok(Vec::new())
    }

    /// A backend generation, if it can synchronize it with screen capture.
    fn output_version(&mut self) -> Option<u64> {
        None
    }

    /// Capture the screen and its scrollback counters together when the
    /// backend can provide an atomic snapshot. The default preserves support
    /// for simpler agents that expose these values through separate calls.
    fn screen_snapshot_at(&mut self, scrollback: usize) -> Result<VersionedSnapshot> {
        let cells = self.screen_cells_at(scrollback)?;
        let wrapped = self.row_wrapped_at(scrollback)?;
        let scrollback_len = self.scrollback_len();
        let scrollback_total = self.scrollback_total();
        let cursor = if scrollback == 0 {
            self.cursor()?
        } else {
            Cursor {
                row: 0,
                col: 0,
                visible: false,
            }
        };
        Ok(VersionedSnapshot {
            snapshot: ScreenSnapshot {
                cells,
                wrapped,
                cursor,
                scrollback_len,
                scrollback_total,
            },
            output_version: self.output_version(),
            instance_id: None,
        })
    }

    /// Subscribe to output as it arrives. Sessions track output activity and
    /// viewers can consume it without screen polling. `None` means this backend
    /// cannot stream, so viewers fall back to `screen_bytes`.
    fn subscribe(&mut self) -> Option<Receiver<Vec<u8>>> {
        None
    }

    fn cursor(&mut self) -> Result<Cursor>;

    fn is_alive(&mut self) -> bool;

    /// The known exit status, if this backend has observed one.
    fn exit_info(&mut self) -> Option<ExitInfo> {
        None
    }

    /// End the child. Idempotent: calling it on an already-exited process is
    /// not an error, because a caller that raced a self-exit (step 006) must
    /// not be punished for the race it could not have won.
    fn terminate(&mut self) -> Result<()>;

    /// Resize the terminal. Backends without a real terminal can accept this
    /// as a no-op; a PTY backend updates both its PTY and screen parser.
    fn resize(&mut self, _size: Size) -> Result<()> {
        Ok(())
    }

    /// The current terminal size.
    fn size(&self) -> Size;
}

/// An agent that answers from a script instead of running anything — the suite
/// needs no `claude` binary, pty or network. Records every byte written, which
/// is what makes `send_line`'s atomicity observable rather than asserted.
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
            cursor: Cursor {
                row: 0,
                col: 0,
                visible: true,
            },
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

    fn terminate(&mut self) -> Result<()> {
        // Reuses the existing test helper: `kill()` is what a test calls to
        // simulate a self-exit, and `terminate()` is what an orchestrated
        // close calls. Both mean "this process is done" to a scripted double
        // that has no real process to signal.
        self.kill();
        Ok(())
    }

    fn resize(&mut self, size: Size) -> Result<()> {
        self.size = size;
        Ok(())
    }

    fn size(&self) -> Size {
        self.size
    }
}
