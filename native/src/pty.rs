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
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

/// Live viewers of one pty's output. Shared with the reader thread, which is
/// the only producer; every consumer holds the other end of a channel.
type Watchers = Arc<Mutex<Vec<Sender<Vec<u8>>>>>;

fn io<E: std::fmt::Display>(e: E) -> AgentError {
    AgentError::Io(e.to_string())
}

/// A child process attached to a pty, with its screen parsed into a grid.
pub struct PtyAgent {
    size: Size,
    screen: Arc<Mutex<vt100::Parser>>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    watchers: Watchers,
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
        let watchers: Watchers = Arc::new(Mutex::new(Vec::new()));
        spawn_reader(reader, Arc::clone(&screen), Arc::clone(&watchers));

        Ok(Self {
            size,
            screen,
            writer,
            child,
            watchers,
            _master: pair.master,
        })
    }
}

/// Drain the pty into the grid until EOF, on its own thread, tee-ing every
/// chunk to whoever is watching live.
///
/// Reading a pty blocks, and the whole point of the grid is that a caller can
/// ask "what is on screen *now*" without having pumped it first. The tee is the
/// other half: a machine polls the grid, a human needs the bytes as they come.
///
/// A subscriber that has hung up is dropped on its next failed send, so an
/// attach/detach cycle leaks nothing.
fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    screen: Arc<Mutex<vt100::Parser>>,
    watchers: Watchers,
) {
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
            if let Ok(mut watchers) = watchers.lock() {
                watchers.retain(|w| w.send(buf[..n].to_vec()).is_ok());
            }
        }
        // EOF: drop every sender so each attached viewer's recv() ends instead
        // of blocking forever on a process that is gone.
        if let Ok(mut watchers) = watchers.lock() {
            watchers.clear();
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

    /// The screen as terminal bytes, cursor position included.
    ///
    /// `contents_formatted` emits a full repaint — clear, then the grid with
    /// its attributes — which is exactly what a terminal that just attached
    /// needs. Without it a viewer stares at nothing until the program happens
    /// to redraw on its own, which a paused agent never does.
    fn screen_bytes(&mut self) -> Result<Vec<u8>> {
        let parser = self.screen.lock().map_err(|_| io("screen lock poisoned"))?;
        Ok(parser.screen().contents_formatted())
    }

    fn subscribe(&mut self) -> Option<Receiver<Vec<u8>>> {
        let (tx, rx) = channel();
        self.watchers.lock().ok()?.push(tx);
        Some(rx)
    }

    fn cursor(&mut self) -> Result<Cursor> {
        let parser = self.screen.lock().map_err(|_| io("screen lock poisoned"))?;
        let (row, col) = parser.screen().cursor_position();
        Ok(Cursor { row, col })
    }

    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Measured, not assumed (`steps/006-lifetime.md` Actual): the comment
    /// this replaced claimed a signal to an already-exited pid "still returns
    /// success" and was wrong. `is_alive` calls `try_wait`, which on unix
    /// *reaps* the child the moment it returns `Some` — after that the pid is
    /// gone, not merely a zombie, and `kill()` on it fails with ESRCH. So the
    /// idempotence this trait promises is implemented here explicitly: if the
    /// process is already gone, there is nothing to signal.
    fn terminate(&mut self) -> Result<()> {
        if !self.is_alive() {
            return Ok(());
        }
        self.child.kill().map_err(io)
    }

    fn size(&self) -> Size {
        self.size
    }
}
