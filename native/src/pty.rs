//! A real agent: a process on a real pty, with its screen kept as a grid.
//!
//! # Why a terminal emulator is not optional here
//!
//! A coding agent redraws its screen. Accumulating raw pty bytes gives you the
//! *history of the drawing*, not the picture — and `Cursor` is unobtainable
//! from raw bytes at all. The existing Emacs implementation distinguishes real
//! typed input from ghost/autocomplete text **by cursor column**, so cursor
//! queryability is a hard requirement on whatever parses the stream, not a
//! nice-to-have.
//!
//! # Why `vt100` and not `termwiz`
//!
//! The design doc picked `termwiz` because it ships from the same monorepo as
//! `portable-pty`. That reasoning is weak at this particular seam: a pty hands
//! over bytes, and bytes are bytes — there is no compatibility surface between
//! the two crates to keep aligned. Meanwhile the doc's own note records that
//! termwiz's `screen_chars_to_string` is documented as existing *"primarily for
//! testing"*, and it would have been our main read path. `vt100` offers exactly
//! the three things [`AgentProcess`] asks for — feed bytes, read the grid, read
//! the cursor — in one crate.
//!
//! This is a reversible choice on purpose: it sits behind [`AgentProcess`], the
//! seam built on day one precisely so the vendor underneath can change without
//! touching a line of policy.

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use remuda_core::agent::{AgentError, AgentProcess, Cursor, Result, Size};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

fn io<E: std::fmt::Display>(e: E) -> AgentError {
    AgentError::Io(e.to_string())
}

/// A child process attached to a pty, with its screen parsed into a grid.
pub struct PtyAgent {
    size: Size,
    screen: Arc<Mutex<vt100::Parser>>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    _master: Box<dyn MasterPty + Send>,
}

impl PtyAgent {
    /// Spawn `command` on a new pty of `size`.
    ///
    /// The pty is opened at `size` and never resized — [`remuda_core::Session`]
    /// exposes no resize method, so a viewer attaching later cannot shrink the
    /// terminal out from under a running agent. That was a real defect in the
    /// implementation this replaces.
    pub fn spawn(command: CommandBuilder, size: Size) -> Result<Self> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: size.rows(),
                cols: size.cols(),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io)?;

        let child = pair.slave.spawn_command(command).map_err(io)?;
        drop(pair.slave); // Or the master never sees EOF when the child exits.

        let writer = pair.master.take_writer().map_err(io)?;
        let reader = pair.master.try_clone_reader().map_err(io)?;
        let screen = Arc::new(Mutex::new(vt100::Parser::new(size.rows(), size.cols(), 0)));
        spawn_reader(reader, Arc::clone(&screen));

        Ok(Self {
            size,
            screen,
            writer,
            child,
            _master: pair.master,
        })
    }
}

/// Drain the pty into the grid until EOF, on its own thread.
///
/// Reading a pty blocks, and the whole point of the grid is that a caller can
/// ask "what is on screen *now*" without having pumped it first.
fn spawn_reader(mut reader: Box<dyn Read + Send>, screen: Arc<Mutex<vt100::Parser>>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            if let Ok(mut parser) = screen.lock() {
                parser.process(&buf[..n]);
            } else {
                break; // Poisoned: the grid can no longer be trusted.
            }
        }
    });
}

impl AgentProcess for PtyAgent {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if !self.is_alive() {
            return Err(AgentError::Exited);
        }
        self.writer.write_all(bytes).map_err(io)?;
        self.writer.flush().map_err(io)
    }

    fn screen_text(&mut self) -> Result<String> {
        let parser = self.screen.lock().map_err(|_| io("screen lock poisoned"))?;
        Ok(parser.screen().contents())
    }

    fn cursor(&mut self) -> Result<Cursor> {
        let parser = self.screen.lock().map_err(|_| io("screen lock poisoned"))?;
        let (row, col) = parser.screen().cursor_position();
        Ok(Cursor { row, col })
    }

    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn size(&self) -> Size {
        self.size
    }
}
