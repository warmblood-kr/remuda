//! Host-only terminal preparation; not yet wired into PtyAgent.
use super::strings::Strings;
use super::viewport::{Modes, RenderingProfile, Viewport};
use remuda_core::agent::{Cursor, MouseEncoding, MouseMode, MouseState, VersionedSnapshot};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
const PARSE_BUDGET: usize = 4096;
const MAX_CELLS: usize = 65_536;
/// Scrollback a host keeps, as an in-process session does.
pub(crate) const HISTORY_ROWS: usize = 10_000;
/// The history older daemons accept in a viewport; a plain `Viewport` is
/// clamped to it so they keep working against a newer host.
pub(crate) const LEGACY_HISTORY_ROWS: u16 = 1000;
const MAX_HISTORY_CELLS: usize = HISTORY_ROWS * 1000;
const MAX_QUERIES: usize = 32;
#[derive(Default)]
struct Queries {
    positions: VecDeque<(u16, u16)>,
    overflow: u64,
}
impl vt100::Callbacks for Queries {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        if i1.is_none() && i2.is_none() && c == 'n' && params == [&[6_u16][..]] {
            if self.positions.len() == MAX_QUERIES {
                self.overflow = self.overflow.saturating_add(1);
            } else {
                let (row, col) = screen.cursor_position();
                self.positions
                    .push_back((row + 1, col.min(screen.size().1 - 1) + 1));
            }
        }
    }
}
pub(crate) struct Terminal {
    parser: vt100::Parser<Queries>,
    strings: Strings,
    history: usize,
    generation: u64,
    scrollback_total: AtomicUsize,
}
impl Terminal {
    pub fn validate_geometry(rows: u16, cols: u16, history: usize) -> Result<(), &'static str> {
        geometry(rows, cols, history)
    }
    pub fn new(rows: u16, cols: u16, history: usize) -> Result<Self, &'static str> {
        geometry(rows, cols, history)?;
        Ok(Self {
            parser: vt100::Parser::new_with_callbacks(rows, cols, history, Queries::default()),
            strings: Strings::default(),
            history,
            generation: 0,
            scrollback_total: AtomicUsize::new(0),
        })
    }
    /// Consume at most one budget; caller must release its lock/yield before
    /// continuing. This is a work limit, not a measured latency guarantee.
    pub fn process(&mut self, bytes: &[u8]) -> Result<usize, &'static str> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let next = self
            .generation
            .checked_add(1)
            .ok_or("terminal generation exhausted")?;
        let count = bytes.len().min(PARSE_BUDGET);
        self.strings.feed(&bytes[..count], |bytes| {
            crate::pty::process_output(&mut self.parser, bytes, &self.scrollback_total);
        });
        self.generation = next;
        Ok(count)
    }
    pub fn prepare_resize(&self, rows: u16, cols: u16) -> Result<u64, &'static str> {
        geometry(rows, cols, self.history)?;
        self.generation
            .checked_add(1)
            .ok_or("terminal generation exhausted")
    }
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<(), &'static str> {
        let next = self.prepare_resize(rows, cols)?;
        self.parser.screen_mut().set_size(rows, cols);
        self.generation = next;
        Ok(())
    }
    pub fn snapshot(&mut self, scrollback: usize) -> VersionedSnapshot {
        let viewport = self.viewport(scrollback);
        let generation = viewport.generation;
        VersionedSnapshot {
            snapshot: viewport.into_screen(),
            output_version: Some(generation),
            instance_id: None,
        }
    }
    pub fn viewport(&mut self, scrollback: usize) -> Viewport {
        let (rows, cols) = self.parser.screen().size();
        let screen = self.parser.screen_mut();
        let previous = screen.scrollback();
        screen.set_scrollback(usize::MAX);
        let history_len = screen.scrollback() as u16;
        screen.set_scrollback(previous);
        let (cells, wrapped, cursor, offset, modes) =
            crate::pty::with_scrollback(screen, scrollback, |screen| {
                let (row, col) = crate::pty::display_cursor(screen);
                (
                    crate::pty::styled_cells_in(screen, rows, cols),
                    (0..rows).map(|row| screen.row_wrapped(row)).collect(),
                    Cursor {
                        row,
                        col,
                        visible: screen.scrollback() == 0 && !screen.hide_cursor(),
                    },
                    screen.scrollback() as u16,
                    modes(screen),
                )
            });
        Viewport {
            schema: 1,
            rendering_profile: RenderingProfile::Vt100Fixed22,
            generation: self.generation,
            rows,
            cols,
            offset,
            history_len,
            history_total: self.scrollback_total.load(Ordering::Relaxed) as u64,
            cells,
            wrapped,
            cursor,
            modes,
        }
    }
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    #[cfg(test)]
    pub fn generation_fixture(&mut self, generation: u64) {
        self.generation = generation;
    }
    pub fn take_query(&mut self) -> Option<(u16, u16)> {
        self.parser.callbacks_mut().positions.pop_front()
    }
    /// Runtime must expose these explicit unsupported/degraded counters;
    /// overflowing terminal replies cannot be reported as a healthy session.
    pub fn health(&self) -> (u64, u64) {
        (self.strings.unsupported, self.parser.callbacks().overflow)
    }
}
fn modes(screen: &vt100::Screen) -> Modes {
    Modes {
        mouse: MouseState {
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
            bracketed_paste: screen.bracketed_paste(),
        },
        application_cursor: screen.application_cursor(),
        application_keypad: screen.application_keypad(),
        alternate_screen: screen.alternate_screen(),
    }
}

fn geometry(rows: u16, cols: u16, history: usize) -> Result<(), &'static str> {
    if rows == 0
        || cols == 0
        || rows > 500
        || cols > 1000
        || usize::from(rows) * usize::from(cols) > MAX_CELLS
        || history > HISTORY_ROWS
        || history
            .checked_mul(usize::from(cols))
            .is_none_or(|cells| cells > MAX_HISTORY_CELLS)
    {
        return Err("host terminal geometry budget exceeded");
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn feed(terminal: &mut Terminal, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let count = terminal.process(bytes).unwrap();
            bytes = &bytes[count..];
        }
    }
    #[test]
    fn query_positions_and_modes_survive_every_byte_split() {
        let bytes =
            "abc\x1b[6ndef\x1b[6n\r\n\u{754c}e\u{301}\x1b[?2004h\x1b[?1049halt\x1b[?1049l\x1b[?25l"
                .as_bytes();
        let mut reference = vt100::Parser::new(24, 80, 10);
        reference.process(bytes);
        for split in 0..=bytes.len() {
            let mut terminal = Terminal::new(24, 80, 10).unwrap();
            feed(&mut terminal, &bytes[..split]);
            feed(&mut terminal, &bytes[split..]);
            assert_eq!(terminal.take_query(), Some((1, 4)));
            assert_eq!(terminal.take_query(), Some((1, 7)));
            assert_eq!(terminal.take_query(), None);
            assert_eq!(
                terminal.screen().state_formatted(),
                reference.screen().state_formatted()
            );
            assert_eq!(terminal.health(), (0, 0));
        }
    }
    #[test]
    fn work_geometry_reply_limits_and_generation_are_explicit() {
        assert!(Terminal::new(u16::MAX, 80, 10).is_err());
        assert!(Terminal::new(500, 1000, 1000).is_err());
        let mut terminal = Terminal::new(24, 80, 10).unwrap();
        assert_eq!(
            terminal.process(&vec![b'x'; PARSE_BUDGET + 1]).unwrap(),
            PARSE_BUDGET
        );
        assert_eq!(terminal.generation(), 1);
        assert!(terminal.resize(0, 0).is_err());
        terminal.resize(25, 81).unwrap();
        assert_eq!(terminal.generation(), 2);
        feed(&mut terminal, &b"\x1b[6n".repeat(MAX_QUERIES + 1));
        assert_eq!(terminal.health(), (0, 1));
        assert_eq!(terminal.parser.callbacks().positions.len(), MAX_QUERIES);
        terminal.generation = u64::MAX;
        assert!(terminal.process(b"no").is_err());
    }
}
