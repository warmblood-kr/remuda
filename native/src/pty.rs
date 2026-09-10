//! A real agent: a process on a real pty, with its screen kept as a grid.
//!
//! # Why a terminal emulator is not optional here
//!
//! A coding agent redraws its screen. Accumulating raw pty bytes gives you the
//! *history of the drawing*, not the picture — and `Cursor` is unobtainable
//! from raw bytes at all. Ghost/autocomplete text is told from real typed input
//! **by cursor column**, so ⚠ cursor queryability is a hard requirement on
//! whatever parses the stream, not a nice-to-have.
//!
//! # Why `vt100` and not `termwiz`
//!
//! `vt100` offers exactly the three things [`AgentProcess`] asks for — feed
//! bytes, read the grid, read the cursor — in one crate, whereas termwiz's
//! equivalent read path is documented as existing *"primarily for testing"*. A
//! pty hands over bytes, so there is no compatibility surface between the two
//! crates to keep aligned. Reversible on purpose: the choice sits behind
//! [`AgentProcess`], the seam built so the vendor can change without touching
//! a line of policy.

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use remuda_core::agent::{AgentError, AgentProcess, Color, Cursor, Result, Size, StyledCell};
use std::io::{Read, Write};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

/// Live viewers of one pty's output. Shared with the reader thread, which is
/// the only producer; every consumer holds the other end of a channel.
type Watchers = Arc<Mutex<Vec<Sender<Vec<u8>>>>>;

/// The pty's input end. Shared, because the reader thread must answer the
/// terminal's own questions — see [`DSR_CURSOR`].
type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// "Where is the cursor?" — a query the terminal must answer. ⚠ ConPTY asks it
/// BEFORE emitting anything and waits: unanswered, the child is alive and the
/// screen is blank forever. Measured on a windows-latest runner; see steps/010.
const DSR_CURSOR: &[u8] = b"\x1b[6n";

fn io<E: std::fmt::Display>(e: E) -> AgentError {
    AgentError::Io(e.to_string())
}

/// `vt100::Color` and [`Color`] are shaped identically on purpose; this is
/// the one place that fact is spent.
fn color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Default,
        vt100::Color::Idx(n) => Color::Idx(n),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// A child process attached to a pty, with its screen parsed into a grid.
pub struct PtyAgent {
    size: Size,
    screen: Arc<Mutex<vt100::Parser>>,
    writer: SharedWriter,
    child: Box<dyn Child + Send + Sync>,
    watchers: Watchers,
    _master: Box<dyn MasterPty + Send>,
}

impl PtyAgent {
    /// Spawn `command` on a new pty of `size`. The pty is opened at `size` and
    /// never resized, so a viewer attaching later cannot shrink the terminal
    /// out from under a running agent.
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

        let writer: SharedWriter = Arc::new(Mutex::new(pair.master.take_writer().map_err(io)?));
        let reader = pair.master.try_clone_reader().map_err(io)?;
        let screen = Arc::new(Mutex::new(vt100::Parser::new(size.rows(), size.cols(), 0)));
        let watchers: Watchers = Arc::new(Mutex::new(Vec::new()));
        spawn_reader(
            reader,
            Arc::clone(&screen),
            Arc::clone(&watchers),
            Arc::clone(&writer),
        );

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

/// Drain the pty into the grid until EOF, on its own thread, tee-ing every chunk
/// to live watchers — a machine polls the grid, a human needs bytes as they
/// come. A hung-up subscriber is dropped on its next failed send.
fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    screen: Arc<Mutex<vt100::Parser>>,
    watchers: Watchers,
    writer: SharedWriter,
) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            let asked = buf[..n].windows(DSR_CURSOR.len()).any(|w| w == DSR_CURSOR);
            let at = match screen.lock() {
                Ok(mut parser) => {
                    parser.process(&buf[..n]);
                    let (row, col) = parser.screen().cursor_position();
                    (row + 1, col + 1)
                }
                // Poisoned: the grid can no longer be trusted.
                Err(_) => break,
            };
            if asked {
                answer_cursor_query(&writer, at);
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

/// Reply to a cursor-position query. One `write_all` under the same lock every
/// other write takes, so no divisible write appears and PRINCIPLES §6
/// invariant 1 holds: no caller can land inside another's burst.
// ponytail: matched within one read. ConPTY writes the query as a single
// four-byte message; a split one would be missed until the next ask.
fn answer_cursor_query(writer: &SharedWriter, (row, col): (u16, u16)) {
    let reply = format!("\x1b[{row};{col}R");
    if let Ok(mut writer) = writer.lock() {
        let _ = writer.write_all(reply.as_bytes());
        let _ = writer.flush();
    }
}

impl AgentProcess for PtyAgent {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if !self.is_alive() {
            return Err(AgentError::Exited);
        }
        let mut writer = self.writer.lock().map_err(|_| io("writer lock poisoned"))?;
        writer.write_all(bytes).map_err(io)?;
        writer.flush().map_err(io)
    }

    fn screen_text(&mut self) -> Result<String> {
        let parser = self.screen.lock().map_err(|_| io("screen lock poisoned"))?;
        Ok(parser.screen().contents())
    }

    /// The screen as terminal bytes, cursor included. `contents_formatted`
    /// emits a full repaint, which is what a just-attached terminal needs — a
    /// paused agent never redraws on its own.
    fn screen_bytes(&mut self) -> Result<Vec<u8>> {
        let parser = self.screen.lock().map_err(|_| io("screen lock poisoned"))?;
        Ok(parser.screen().contents_formatted())
    }

    /// Styled cells off `vt100`'s own grid, including its wide/continuation
    /// judgment (`is_wide`/`is_wide_continuation`) rather than re-deriving
    /// it — this is the path that actually keeps colour. See steps/020, 023.
    fn screen_cells(&mut self) -> Result<Vec<Vec<StyledCell>>> {
        let parser = self.screen.lock().map_err(|_| io("screen lock poisoned"))?;
        let screen = parser.screen();
        Ok((0..self.size.rows())
            .map(|row| {
                (0..self.size.cols())
                    .map(|col| {
                        let cell = screen.cell(row, col);
                        // A wide cell's continuation carries no text of its
                        // own — the wide cell before it already claims both
                        // columns. Only a genuinely blank ordinary cell gets
                        // the space substitute, so it still claims 1 column.
                        let continuation = cell.is_some_and(|c| c.is_wide_continuation());
                        let text = if continuation {
                            String::new()
                        } else {
                            let t = cell.map_or("", |c| c.contents());
                            (if t.is_empty() { " " } else { t }).to_string()
                        };
                        StyledCell {
                            text,
                            wide: cell.is_some_and(|c| c.is_wide()),
                            fg: color(cell.map_or(vt100::Color::Default, |c| c.fgcolor())),
                            bg: color(cell.map_or(vt100::Color::Default, |c| c.bgcolor())),
                            bold: cell.is_some_and(|c| c.bold()),
                            dim: cell.is_some_and(|c| c.dim()),
                            italic: cell.is_some_and(|c| c.italic()),
                            underline: cell.is_some_and(|c| c.underline()),
                            inverse: cell.is_some_and(|c| c.inverse()),
                        }
                    })
                    .collect()
            })
            .collect())
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

    /// Caution: `is_alive` calls `try_wait`, which *reaps* the child on unix, so
    /// `kill()` afterwards fails with ESRCH. The trait's idempotence is
    /// therefore explicit here — an already-gone process is nothing to signal.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn wait_for(agent: &mut PtyAgent, needle: &str) {
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if agent.screen_text().unwrap().contains(needle) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// [MEASURED, Linux] Reproduces "no colour in the TUI pane": `screen_text`
    /// (what `Capture` serves) discards SGR that `screen_bytes` (what `attach`
    /// uses) keeps. See steps/018.
    #[test]
    fn screen_text_strips_colour_that_screen_bytes_keeps() {
        let mut cmd = CommandBuilder::new("printf");
        cmd.arg("\x1b[31mred\x1b[0m");
        let mut agent = PtyAgent::spawn(cmd, Size::new(80, 24)).unwrap();
        wait_for(&mut agent, "red");

        let text = agent.screen_text().unwrap();
        let bytes = agent.screen_bytes().unwrap();

        assert!(
            !text.contains('\x1b'),
            "screen_text (Capture, what the TUI pane reads) must be plain: {text:?}"
        );
        assert!(
            bytes.windows(2).any(|w| w == b"\x1b["),
            "screen_bytes (attach's initial repaint) must carry SGR: {bytes:?}"
        );
    }

    /// [MEASURED, Linux] `screen_cells` reads `vt100`'s own wide/continuation
    /// judgment for real Hangul, what `PtyAgent` actually hands the pane.
    /// See steps/023.
    #[test]
    fn screen_cells_marks_a_wide_hangul_cell_and_its_empty_continuation() {
        let mut cmd = CommandBuilder::new("printf");
        cmd.arg("안녕!");
        let mut agent = PtyAgent::spawn(cmd, Size::new(80, 24)).unwrap();
        wait_for(&mut agent, "안녕!");

        let cells = agent.screen_cells().unwrap();
        let row0 = &cells[0];
        assert_eq!(row0[0].text, "안");
        assert!(row0[0].wide, "안 is East-Asian Wide: {:?}", row0[0]);
        assert_eq!(
            row0[1].text, "",
            "a wide cell's continuation carries no text of its own: {:?}",
            row0[1]
        );
        assert!(!row0[1].wide);
        assert_eq!(row0[2].text, "녕");
        assert!(row0[2].wide);
        assert_eq!(row0[3].text, "");
        assert!(!row0[3].wide);
        assert_eq!(row0[4].text, "!");
        assert!(!row0[4].wide);
        assert_eq!(
            row0[5].text, " ",
            "a genuinely blank ordinary cell still claims 1 column: {:?}",
            row0[5]
        );
    }

    /// [MEASURED, Linux] `screen_cells` is the styled readout `screen_text`
    /// cannot be: the cells under "red" carry the colour, and a cell the
    /// child never touched stays `Color::Default`. See steps/020.
    #[test]
    fn screen_cells_carries_colour_that_screen_text_discards() {
        let mut cmd = CommandBuilder::new("printf");
        cmd.arg("\x1b[31mred\x1b[0m");
        let mut agent = PtyAgent::spawn(cmd, Size::new(80, 24)).unwrap();
        wait_for(&mut agent, "red");

        let cells = agent.screen_cells().unwrap();
        let row0 = &cells[0];
        assert_eq!(row0[0].text, "r");
        assert_eq!(row0[1].text, "e");
        assert_eq!(row0[2].text, "d");
        for c in &row0[0..3] {
            assert_eq!(c.fg, Color::Idx(1), "SGR 31 (red) is vt100 Idx(1): {c:?}");
        }
        assert_eq!(
            row0[3].fg,
            Color::Default,
            "a cell after the \\x1b[0m reset, never itself painted, stays default: {:?}",
            row0[3]
        );
    }
}
