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
use remuda_core::agent::{
    AgentError, AgentProcess, Color, Cursor, MouseEncoding, MouseMode, MouseState, Result,
    ScreenSnapshot, Size, StyledCell,
};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
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
const SCROLLBACK_ROWS: usize = 10_000;
const SCROLLBACK_PROBE_CHUNK: usize = 512;

fn io<E: std::fmt::Display>(e: E) -> AgentError {
    AgentError::Io(e.to_string())
}

/// Temporarily selects a history viewport while guaranteeing the parser's
/// live viewport is restored, including while unwinding from a panic.
fn with_scrollback<T>(
    screen: &mut vt100::Screen,
    scrollback: usize,
    read: impl FnOnce(&vt100::Screen) -> T,
) -> T {
    let previous = screen.scrollback();
    screen.set_scrollback(scrollback);
    let restore = ScrollbackRestore { screen, previous };
    read(&*restore.screen)
}

struct ScrollbackRestore<'a> {
    screen: &'a mut vt100::Screen,
    previous: usize,
}

impl Drop for ScrollbackRestore<'_> {
    fn drop(&mut self) {
        self.screen.set_scrollback(self.previous);
    }
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
    scrollback_total: Arc<AtomicUsize>,
    master: Option<Box<dyn MasterPty + Send>>,
}

impl PtyAgent {
    /// Spawn `command` on a new pty of `size`.
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
        // No child_guard here on purpose — this child already dies with the
        // daemon by kernel accident (the master fd closes on any daemon
        // exit, SIGHUP-ing this session leader). See child_guard.rs and
        // native/tests/pty_survives_daemon_death.rs, which pins it.
        crate::child_guard::documented_pty_hangup_accident();
        drop(pair.slave); // Or the master never sees EOF when the child exits.

        let writer: SharedWriter = Arc::new(Mutex::new(pair.master.take_writer().map_err(io)?));
        let reader = pair.master.try_clone_reader().map_err(io)?;
        let screen = Arc::new(Mutex::new(vt100::Parser::new(
            size.rows(),
            size.cols(),
            SCROLLBACK_ROWS,
        )));
        let watchers: Watchers = Arc::new(Mutex::new(Vec::new()));
        let scrollback_total = Arc::new(AtomicUsize::new(0));
        spawn_reader(
            reader,
            Arc::clone(&screen),
            Arc::clone(&watchers),
            Arc::clone(&writer),
            Arc::clone(&scrollback_total),
        );

        Ok(Self {
            size,
            screen,
            writer,
            child,
            watchers,
            scrollback_total,
            master: Some(pair.master),
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
    scrollback_total: Arc<AtomicUsize>,
) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            let asked = buf[..n].windows(DSR_CURSOR.len()).any(|w| w == DSR_CURSOR);
            let at = match screen.lock() {
                Ok(mut parser) => process_output(&mut parser, &buf[..n], &scrollback_total),
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

/// Process bounded chunks while a temporary nonzero scroll offset counts
/// every full-screen row pushed into history, even when the retained buffer is full.
fn process_output(
    parser: &mut vt100::Parser,
    bytes: &[u8],
    scrollback_total: &AtomicUsize,
) -> (u16, u16) {
    for chunk in bytes.chunks(SCROLLBACK_PROBE_CHUNK) {
        let screen = parser.screen_mut();
        let previous = screen.scrollback();
        screen.set_scrollback(usize::MAX);
        let before_len = screen.scrollback();
        screen.set_scrollback(1);
        let probe_start = screen.scrollback();

        parser.process(chunk);

        let screen = parser.screen_mut();
        let probe_end = screen.scrollback();
        screen.set_scrollback(usize::MAX);
        let after_len = screen.scrollback();
        screen.set_scrollback(previous);

        let scrolled = if probe_start == 0 {
            after_len.saturating_sub(before_len)
        } else {
            probe_end.saturating_sub(probe_start)
        };
        let _ = scrollback_total.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |total| {
            Some(total.saturating_add(scrolled))
        });
    }
    let (row, col) = display_cursor(parser.screen());
    (row + 1, col + 1)
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

/// The cursor as a terminal shows and reports it: vt100 leaves it one past the
/// last column in DECAWM pending wrap, where xterm/iTerm/tmux draw and report
/// (DSR 6n) the last column, so clamp to `cols - 1`.
pub(crate) fn display_cursor(screen: &vt100::Screen) -> (u16, u16) {
    let (row, col) = screen.cursor_position();
    (row, col.min(screen.size().1.saturating_sub(1)))
}

pub(crate) fn styled_cells(screen: &vt100::Screen, size: Size) -> Vec<Vec<StyledCell>> {
    (0..size.rows())
        .map(|row| {
            (0..size.cols())
                .map(|col| {
                    let cell = screen.cell(row, col);
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
        .collect()
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

    /// The screen and input modes as terminal bytes, cursor included.
    /// `state_formatted` emits a full repaint plus mouse, paste, and keypad
    /// modes, which a just-attached terminal needs even if the child is idle.
    fn screen_bytes(&mut self) -> Result<Vec<u8>> {
        let parser = self.screen.lock().map_err(|_| io("screen lock poisoned"))?;
        Ok(parser.screen().state_formatted())
    }

    fn mouse_tracking(&mut self) -> bool {
        self.screen
            .lock()
            .map(|parser| parser.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None)
            .unwrap_or(false)
    }

    fn mouse_state(&mut self) -> MouseState {
        self.screen
            .lock()
            .map(|parser| {
                let screen = parser.screen();
                MouseState {
                    mode: match screen.mouse_protocol_mode() {
                        vt100::MouseProtocolMode::None => MouseMode::None,
                        vt100::MouseProtocolMode::Press => MouseMode::Press,
                        vt100::MouseProtocolMode::PressRelease => MouseMode::PressRelease,
                        vt100::MouseProtocolMode::ButtonMotion => MouseMode::ButtonMotion,
                        vt100::MouseProtocolMode::AnyMotion => MouseMode::AnyMotion,
                    },
                    encoding: match screen.mouse_protocol_encoding() {
                        vt100::MouseProtocolEncoding::Default => MouseEncoding::Default,
                        vt100::MouseProtocolEncoding::Utf8 => MouseEncoding::Utf8,
                        vt100::MouseProtocolEncoding::Sgr => MouseEncoding::Sgr,
                    },
                }
            })
            .unwrap_or_default()
    }

    /// Styled cells off `vt100`'s own grid, including its wide/continuation
    /// judgment (`is_wide`/`is_wide_continuation`) rather than re-deriving
    /// it — this is the path that actually keeps colour. See steps/020, 023.
    fn screen_cells(&mut self) -> Result<Vec<Vec<StyledCell>>> {
        let parser = self.screen.lock().map_err(|_| io("screen lock poisoned"))?;
        Ok(styled_cells(parser.screen(), self.size))
    }

    fn screen_cells_at(&mut self, scrollback: usize) -> Result<Vec<Vec<StyledCell>>> {
        let mut parser = self.screen.lock().map_err(|_| io("screen lock poisoned"))?;
        Ok(with_scrollback(parser.screen_mut(), scrollback, |screen| {
            styled_cells(screen, self.size)
        }))
    }

    fn scrollback_len(&mut self) -> usize {
        let Ok(mut parser) = self.screen.lock() else {
            return 0;
        };
        let screen = parser.screen_mut();
        let previous = screen.scrollback();
        screen.set_scrollback(usize::MAX);
        let length = screen.scrollback();
        screen.set_scrollback(previous);
        length
    }

    fn scrollback_total(&mut self) -> usize {
        self.scrollback_total.load(Ordering::Relaxed)
    }

    fn row_wrapped_at(&mut self, scrollback: usize) -> Result<Vec<bool>> {
        let mut parser = self.screen.lock().map_err(|_| io("screen lock poisoned"))?;
        Ok(with_scrollback(parser.screen_mut(), scrollback, |screen| {
            (0..self.size.rows())
                .map(|row| screen.row_wrapped(row))
                .collect()
        }))
    }

    fn screen_snapshot_at(&mut self, scrollback: usize) -> Result<ScreenSnapshot> {
        let mut parser = self.screen.lock().map_err(|_| io("screen lock poisoned"))?;
        let size = self.size;
        let screen = parser.screen_mut();
        let previous = screen.scrollback();
        screen.set_scrollback(usize::MAX);
        let scrollback_len = screen.scrollback();
        screen.set_scrollback(previous);

        let (cells, wrapped, cursor) = with_scrollback(screen, scrollback, |screen| {
            let cursor = if scrollback == 0 {
                let (row, col) = display_cursor(screen);
                Cursor {
                    row,
                    col,
                    visible: !screen.hide_cursor(),
                }
            } else {
                Cursor {
                    row: 0,
                    col: 0,
                    visible: false,
                }
            };
            (
                styled_cells(screen, size),
                (0..size.rows())
                    .map(|row| screen.row_wrapped(row))
                    .collect(),
                cursor,
            )
        });
        let scrollback_total = self.scrollback_total.load(Ordering::Relaxed);

        Ok(ScreenSnapshot {
            cells,
            wrapped,
            cursor,
            scrollback_len,
            scrollback_total,
        })
    }

    fn subscribe(&mut self) -> Option<Receiver<Vec<u8>>> {
        let (tx, rx) = channel();
        self.watchers.lock().ok()?.push(tx);
        Some(rx)
    }

    fn cursor(&mut self) -> Result<Cursor> {
        let parser = self.screen.lock().map_err(|_| io("screen lock poisoned"))?;
        let screen = parser.screen();
        let (row, col) = display_cursor(screen);
        Ok(Cursor {
            row,
            col,
            visible: !screen.hide_cursor(),
        })
    }

    fn is_alive(&mut self) -> bool {
        let alive = matches!(self.child.try_wait(), Ok(None));
        if !alive {
            // ConPTY keeps its output pipe open after the child exits until
            // ClosePseudoConsole runs. Release the master so the reader thread
            // sees EOF and attached clients receive the ordinary end-of-stream.
            self.master.take();
        }
        alive
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

    fn resize(&mut self, size: Size) -> Result<()> {
        self.master
            .as_ref()
            .ok_or(AgentError::Exited)?
            .resize(PtySize {
                rows: size.rows(),
                cols: size.cols(),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io)?;
        self.screen
            .lock()
            .map_err(|_| io("screen lock poisoned"))?
            .screen_mut()
            .set_size(size.rows(), size.cols());
        self.size = size;
        Ok(())
    }

    fn size(&self) -> Size {
        self.size
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn temporary_scrollback_view_restores_after_panic() {
        let mut parser = vt100::Parser::new(2, 5, 4);
        parser.process(b"one\r\ntwo\r\nthree\r\nfour\r\n");
        parser.screen_mut().set_scrollback(1);
        let original = parser.screen().scrollback();

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_scrollback(parser.screen_mut(), 0, |_| panic!("exercise unwind"));
        }));

        assert!(panic.is_err());
        assert_eq!(parser.screen().scrollback(), original);
    }

    #[test]
    fn monotonic_scrollback_counter_advances_after_retained_history_is_full() {
        let mut parser = vt100::Parser::new(24, 80, SCROLLBACK_ROWS);
        let total = AtomicUsize::new(0);
        let fill: String = (0..SCROLLBACK_ROWS + 100)
            .map(|row| format!("row-{row:05}\r\n"))
            .collect();
        process_output(&mut parser, fill.as_bytes(), &total);
        assert_eq!(retained_history_len(&mut parser), SCROLLBACK_ROWS);
        assert!(total.load(Ordering::Relaxed) >= SCROLLBACK_ROWS);

        let before = total.load(Ordering::Relaxed);
        process_output(&mut parser, b"tail\r\ntail\r\ntail\r\n", &total);
        assert_eq!(retained_history_len(&mut parser), SCROLLBACK_ROWS);
        assert_eq!(total.load(Ordering::Relaxed) - before, 3);
    }

    #[test]
    fn scrollback_counter_is_exact_when_crlf_is_split_across_output_chunks() {
        let mut parser = vt100::Parser::new(2, 10, 20);
        let total = AtomicUsize::new(0);
        for part in [b"a\r".as_slice(), b"\n", b"b\r", b"\n", b"c\r", b"\n"] {
            process_output(&mut parser, part, &total);
        }
        assert_eq!(total.load(Ordering::Relaxed), 2);
        assert_eq!(retained_history_len(&mut parser), 2);
    }

    fn retained_history_len(parser: &mut vt100::Parser) -> usize {
        let screen = parser.screen_mut();
        let previous = screen.scrollback();
        screen.set_scrollback(usize::MAX);
        let len = screen.scrollback();
        screen.set_scrollback(previous);
        len
    }

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

    /// [MEASURED, Linux] `vt100::Screen::hide_cursor` is `mode(MODE_HIDE_CURSOR)`
    /// under the hood — it tracks the child's own DECTCEM request rather than
    /// this process inferring it from raw bytes. See steps/027.
    #[test]
    fn cursor_reports_the_childs_own_hide_request() {
        let mut cmd = CommandBuilder::new("printf");
        cmd.arg("before\x1b[?25lhidden");
        let mut agent = PtyAgent::spawn(cmd, Size::new(80, 24)).unwrap();
        wait_for(&mut agent, "hidden");

        let cursor = agent.cursor().unwrap();
        assert!(
            !cursor.visible,
            "the child asked for \\x1b[?25l — the pane must not paint a caret: {cursor:?}"
        );
    }
}
