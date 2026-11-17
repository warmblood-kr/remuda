//! An embedded PTY running the existing interactive attach client.
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};

const ROWS: u16 = 24;
const COLS: u16 = 80;

/// One terminal cell as vt100 resolved it, including the width information
/// GPUI text shaping would otherwise have to guess.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub text: String,
    pub fg: AnsiColor,
    pub bg: AnsiColor,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub wide: bool,
    pub continuation: bool,
}

/// The colour vocabulary a terminal cell can carry without tying the parser to
/// GPUI. The UI maps these to its palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnsiColor {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Resolve a cell's foreground/background pair. Default is a role, not one
/// shared colour: the default foreground and background must contrast, and
/// inverse swaps the resolved pair.
pub fn resolve_colors(fg: AnsiColor, bg: AnsiColor, inverse: bool) -> (u32, u32) {
    const DEFAULT_FG: u32 = 0xe8eaf0;
    const DEFAULT_BG: u32 = 0x101216;
    let fg = color_value(fg, DEFAULT_FG);
    let bg = color_value(bg, DEFAULT_BG);
    if inverse {
        (bg, fg)
    } else {
        (fg, bg)
    }
}

fn color_value(color: AnsiColor, default: u32) -> u32 {
    const PALETTE: [u32; 16] = [
        0x101216, 0xcc6666, 0x81b366, 0xd7b36b, 0x6f9fd8, 0xb18cd4, 0x6fb7b7, 0xc7c7c7,
        0x4b5263, 0xe27777, 0x9ccc7c, 0xe5c07c, 0x86b5e8, 0xc6a3e0, 0x82caca, 0xf0f0f0,
    ];
    match color {
        AnsiColor::Default => default,
        AnsiColor::Rgb(r, g, b) => u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b),
        AnsiColor::Indexed(index) if index < 16 => PALETTE[index as usize],
        AnsiColor::Indexed(index) => {
            let index = index - 16;
            if index < 216 {
                let r = index / 36;
                let g = (index / 6) % 6;
                let b = index % 6;
                let level = |n: u8| if n == 0 { 0 } else { 55 + n * 40 };
                u32::from(level(r)) << 16 | u32::from(level(g)) << 8 | u32::from(level(b))
            } else {
                let level = 8 + (index - 216) * 10;
                u32::from(level) << 16 | u32::from(level) << 8 | u32::from(level)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<Vec<Cell>>,
    pub cursor: (u16, u16),
    pub cursor_visible: bool,
}

/// A selection is tied to the exact displayed cells on mouse-down.
#[derive(Clone, Debug)]
pub struct Selection {
    pub frame: Frame,
    pub start: (usize, usize),
    pub end: (usize, usize),
    pub dragging: bool,
    pub moved: bool,
}

impl Selection {
    pub fn valid_for(&self, frame: &Frame) -> bool {
        self.frame.rows == frame.rows && self.frame.cols == frame.cols && self.frame.cells == frame.cells
    }
    fn glyph_start(&self, (row, col): (usize, usize)) -> (usize, usize) {
        let col = if col > 0 && self.frame.cells.get(row).and_then(|r| r.get(col)).is_some_and(|c| c.continuation) { col - 1 } else { col };
        (row, col)
    }
    pub fn contains(&self, row: usize, col: usize) -> bool {
        if !self.moved { return false; }
        let a = self.glyph_start(self.start);
        let b = self.glyph_start(self.end);
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let pos = self.glyph_start((row, col));
        pos >= a && pos <= b
    }
    pub fn text(&self) -> Option<String> {
        if !self.moved { return None; }
        let a = self.glyph_start(self.start);
        let b = self.glyph_start(self.end);
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let mut out = String::new();
        for row in a.0..=b.0 {
            if row > a.0 { out.push('\n'); }
            for (col, cell) in self.frame.cells.get(row)?.iter().enumerate() {
                if (row, col) < a || (row, col) > b || cell.continuation { continue; }
                if cell.text.is_empty() { out.push(' '); } else { out.push_str(&cell.text); }
            }
        }
        Some(out)
    }
}

fn ansi(color: vt100::Color) -> AnsiColor {
    match color {
        vt100::Color::Default => AnsiColor::Default,
        vt100::Color::Idx(index) => AnsiColor::Indexed(index),
        vt100::Color::Rgb(r, g, b) => AnsiColor::Rgb(r, g, b),
    }
}

/// Keep a huge terminal from creating unbounded GPUI elements. Width is the
/// property the user asked to preserve, so the excess is taken from rows.
pub const MAX_RENDER_CELLS: usize = 20_000;

pub fn clamp_grid(cols: u16, rows: u16) -> (u16, u16) {
    let cols = cols.max(1);
    let rows = rows.max(1);
    let max_rows = (MAX_RENDER_CELLS / usize::from(cols)).max(1) as u16;
    (cols, rows.min(max_rows))
}

/// Convert an already-captured `session_preview` grid to the renderer's frame.
/// The preview is a snapshot, so its dimensions are exactly the captured ones;
/// this function never asks a child to resize.
pub fn frame_from_styled(cells: Vec<Vec<remuda_core::agent::StyledCell>>) -> Frame {
    let rows = cells.len() as u16;
    let cols = cells.iter().map(Vec::len).max().unwrap_or(0) as u16;
    Frame {
        cols,
        rows,
        cells: cells
            .into_iter()
            .map(|row| {
                let mut previous_wide = false;
                row.into_iter()
                    .map(|cell| {
                        // Only the empty cell directly after a wide glyph is a
                        // continuation. A filtered general cell is also empty,
                        // but it must remain a one-column blank.
                        let continuation = previous_wide && cell.text.is_empty() && !cell.wide;
                        previous_wide = cell.wide && !cell.text.is_empty();
                        Cell {
                            text: cell.text,
                            fg: match cell.fg {
                                remuda_core::agent::Color::Default => AnsiColor::Default,
                                remuda_core::agent::Color::Idx(index) => AnsiColor::Indexed(index),
                                remuda_core::agent::Color::Rgb(r, g, b) => AnsiColor::Rgb(r, g, b),
                            },
                            bg: match cell.bg {
                                remuda_core::agent::Color::Default => AnsiColor::Default,
                                remuda_core::agent::Color::Idx(index) => AnsiColor::Indexed(index),
                                remuda_core::agent::Color::Rgb(r, g, b) => AnsiColor::Rgb(r, g, b),
                            },
                            bold: cell.bold,
                            dim: cell.dim,
                            italic: cell.italic,
                            underline: cell.underline,
                            inverse: cell.inverse,
                            wide: cell.wide,
                            continuation,
                        }
                    })
                    .collect()
            })
            .collect(),
        cursor: (0, 0),
        cursor_visible: false,
    }
}

fn frame_of(screen: &vt100::Screen) -> Frame {
    let (rows, cols) = screen.size();
    let mut cells = Vec::with_capacity(rows as usize);
    for row in 0..rows {
        let mut line = Vec::with_capacity(cols as usize);
        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                break;
            };
            line.push(Cell {
                text: cell.contents().to_string(),
                fg: ansi(cell.fgcolor()),
                bg: ansi(cell.bgcolor()),
                bold: cell.bold(),
                dim: cell.dim(),
                italic: cell.italic(),
                underline: cell.underline(),
                inverse: cell.inverse(),
                wide: cell.is_wide(),
                continuation: cell.is_wide_continuation(),
            });
        }
        cells.push(line);
    }
    Frame {
        cols,
        rows,
        cells,
        cursor: screen.cursor_position(),
        cursor_visible: !screen.hide_cursor(),
    }
}

pub struct Terminal {
    child: Option<Box<dyn Child + Send + Sync>>,
    master: Option<Box<dyn MasterPty + Send>>,
    input: Option<mpsc::SyncSender<Vec<u8>>>,
    parser: Arc<Mutex<vt100::Parser>>,
    ended: Arc<AtomicBool>,
    size: Mutex<(u16, u16)>,
}

impl Terminal {
    pub fn start(executable: &Path, name: &str, instance: &str, abi: &str) -> Result<Self, String> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| e.to_string())?;
        let mut command = CommandBuilder::new(executable);
        command.args([
            "--internal-preview-worker",
            "local",
            "",
            name,
            instance,
            "--terminal",
            "--attach-v1",
            abi,
        ]);
        command.env("TERM", "xterm-256color");
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| e.to_string())?;
        drop(pair.slave);
        let reader = match pair.master.try_clone_reader() {
            Ok(reader) => reader,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }
        };
        let writer = match pair.master.take_writer() {
            Ok(writer) => writer,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }
        };
        let parser = Arc::new(Mutex::new(vt100::Parser::new(ROWS, COLS, 0)));
        let ended = Arc::new(AtomicBool::new(false));
        let (input, receiver) = mpsc::sync_channel::<Vec<u8>>(64);
        std::thread::spawn({
            let parser = parser.clone();
            let ended = ended.clone();
            move || {
                let mut reader = reader;
                let mut bytes = [0; 8192];
                while let Ok(n) = reader.read(&mut bytes) {
                    if n == 0 {
                        break;
                    }
                    if let Ok(mut parser) = parser.lock() {
                        parser.process(&bytes[..n]);
                    } else {
                        break;
                    }
                }
                ended.store(true, Ordering::Release);
            }
        });
        std::thread::spawn(move || {
            let mut writer = writer;
            while let Ok(bytes) = receiver.recv() {
                if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child: Some(child),
            master: Some(pair.master),
            input: Some(input),
            parser,
            ended,
            size: Mutex::new((ROWS, COLS)),
        })
    }
    /// The attached child's grid, with attributes and wide-cell facts. The
    /// screen is always read at the size the PTY was last told about.
    pub fn frame(&self) -> Frame {
        self.parser
            .lock()
            .map(|p| frame_of(p.screen()))
            .unwrap_or(Frame {
                cols: 0,
                rows: 0,
                cells: Vec::new(),
                cursor: (0, 0),
                cursor_visible: false,
            })
    }

    /// Resize both the PTY (SIGWINCH to the child) and the vt100 grid. Returns
    /// whether anything changed, so a renderer can avoid redundant frames.
    pub fn resize(&self, rows: u16, cols: u16) -> bool {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let mut size = match self.size.lock() {
            Ok(size) => size,
            Err(_) => return false,
        };
        if *size == (rows, cols) {
            return false;
        }
        // Hold the parser lock across the PTY resize so the reader cannot parse
        // output produced for the new width into the old grid.
        let Ok(mut parser) = self.parser.lock() else {
            return false;
        };
        if let Some(master) = &self.master {
            if master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .is_err()
            {
                return false;
            }
        }
        parser.screen_mut().set_size(rows, cols);
        *size = (rows, cols);
        true
    }

    /// Plain text compatibility view for tests and diagnostics. New rendering
    /// paths should use [`Terminal::frame`] so colour and cell width survive.
    #[cfg(test)]
    pub fn screen(&self) -> String {
        let frame = self.frame();
        let mut text = String::new();
        for row in frame.cells {
            for cell in row {
                if cell.continuation {
                    continue;
                }
                if cell.text.is_empty() {
                    text.push(' ');
                } else {
                    text.push_str(&cell.text);
                }
            }
            text.push('\n');
        }
        text
    }

    /// A parser-only terminal for renderer tests: no child process, no PTY.
    #[cfg(test)]
    pub fn fixture(rows: u16, cols: u16, bytes: &[u8]) -> Self {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(bytes);
        Self {
            child: None,
            master: None,
            input: None,
            parser: Arc::new(Mutex::new(parser)),
            ended: Arc::new(AtomicBool::new(false)),
            size: Mutex::new((rows, cols)),
        }
    }

    #[cfg(test)]
    pub fn size(&self) -> (u16, u16) {
        self.size.lock().map(|size| *size).unwrap_or((0, 0))
    }
    pub fn ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }
    pub fn send(&self, bytes: Vec<u8>) -> Result<(), &'static str> {
        if self.ended() {
            return Err("Terminal ended");
        }
        self.input
            .as_ref()
            .ok_or("Terminal has no input")
            .and_then(|input| input.try_send(bytes).map_err(|_| "Terminal input busy or closed"))
    }

    pub fn paste(&self, text: &str) -> Result<(), &'static str> {
        if text.is_empty() { return Ok(()); }
        let bracketed = self.parser.lock().map_err(|_| "Terminal unavailable")?.screen().bracketed_paste();
        self.send(paste_bytes(text, bracketed)?)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn paste_bytes(text: &str, bracketed: bool) -> Result<Vec<u8>, &'static str> {
    if text.len() > remuda_native::strict_input::MAX_BATCH { return Err("Paste exceeds 12 KiB; nothing sent"); }
    if text.chars().any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r')) {
        return Err("Paste contains unsupported control characters; nothing sent");
    }
    let mut bytes = Vec::with_capacity(text.len() + if bracketed { 12 } else { 0 });
    if bracketed { bytes.extend_from_slice(b"\x1b[200~"); }
    bytes.extend_from_slice(text.as_bytes());
    if bracketed { bytes.extend_from_slice(b"\x1b[201~"); }
    Ok(bytes)
}

pub fn key_bytes(key: &gpui::Keystroke) -> Option<Vec<u8>> {
    if key.modifiers.platform || key.modifiers.function {
        return None;
    }
    if key.modifiers.control {
        let letter = key.key.as_bytes();
        if letter.len() == 1 {
            let c = letter[0].to_ascii_lowercase();
            if c.is_ascii_lowercase() {
                return Some(vec![c - b'a' + 1]);
            }
            if c == b'\\' {
                return Some(vec![0x1c]);
            }
        }
        return None;
    }
    let bytes = match key.key.as_str() {
        "enter" => b"\r".to_vec(),
        "backspace" => b"\x7f".to_vec(),
        "tab" => b"\t".to_vec(),
        "escape" => b"\x1b".to_vec(),
        "up" => b"\x1b[A".to_vec(),
        "down" => b"\x1b[B".to_vec(),
        "right" => b"\x1b[C".to_vec(),
        "left" => b"\x1b[D".to_vec(),
        "home" => b"\x1b[H".to_vec(),
        "end" => b"\x1b[F".to_vec(),
        "delete" => b"\x1b[3~".to_vec(),
        "pageup" => b"\x1b[5~".to_vec(),
        "pagedown" => b"\x1b[6~".to_vec(),
        _ => key.key_char.as_ref()?.as_bytes().to_vec(),
    };
    if key.modifiers.alt {
        let mut result = vec![0x1b];
        result.extend(bytes);
        Some(result)
    } else {
        Some(bytes)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn live_selection_preserves_wide_combining_cells_and_invalidates_on_change() {
        let term = super::Terminal::fixture(2, 12, "A한e\u{301}Z".as_bytes());
        let frame = term.frame();
        let mut selected = super::Selection { frame: frame.clone(), start: (0, 2), end: (0, 3), dragging: false, moved: true };
        assert_eq!(selected.text().as_deref(), Some("한e\u{301}"));
        assert!(selected.contains(0, 1) && selected.contains(0, 2));
        selected.start = (0, 4); selected.end = (0, 1);
        assert_eq!(selected.text().as_deref(), Some("한e\u{301}Z"));
        assert!(selected.valid_for(&frame));
        let changed = super::Terminal::fixture(2, 12, b"different").frame();
        assert!(!selected.valid_for(&changed));
        let resized = super::Terminal::fixture(2, 13, "A한e\u{301}Z".as_bytes()).frame();
        assert!(!selected.valid_for(&resized));
        let emoji = "A👨‍👩‍👧‍👦B";
        let frame = super::Terminal::fixture(2, 30, emoji.as_bytes()).frame();
        let whole = super::Selection { frame, start: (0, 0), end: (0, 29), dragging: false, moved: true };
        assert_eq!(whole.text().unwrap().trim_end(), emoji);
        let frame = super::Terminal::fixture(2, 4, b"ab\r\ncd").frame();
        let across = super::Selection { frame, start: (0, 1), end: (1, 0), dragging: false, moved: true };
        assert_eq!(across.text().as_deref(), Some("b  \nc"));
        assert_eq!(super::key_bytes(&gpui::Keystroke::parse("ctrl-c").unwrap()), Some(vec![3]));
    }
    #[test]
    fn live_selection_maximum_grid_comparison_cost() {
        let frame = super::Terminal::fixture(200, 100, b"sample").frame();
        let selection = super::Selection { frame: frame.clone(), start: (0, 0), end: (199, 99), dragging: false, moved: true };
        let started = std::time::Instant::now();
        for _ in 0..100 { assert!(std::hint::black_box(&selection).valid_for(std::hint::black_box(&frame))); }
        println!("100 max-grid cell equality checks: {:?}", started.elapsed());
    }
    #[test]
    fn paste_preserves_utf8_bounds_and_bracketed_mode_and_ctrl_c() {
        let text = "한글\nA";
        assert_eq!(super::paste_bytes(text, false).unwrap(), text.as_bytes());
        assert_eq!(super::paste_bytes(text, true).unwrap(), [b"\x1b[200~".as_slice(), text.as_bytes(), b"\x1b[201~"].concat());
        assert!(super::paste_bytes(&"x".repeat(remuda_native::strict_input::MAX_BATCH + 1), false).is_err());
        for text in ["hello\x1b[201~world", "\x1b[A", "a\0b", "a\u{7f}b", "a\u{85}b"] {
            assert!(super::paste_bytes(text, false).is_err());
            assert!(super::paste_bytes(text, true).is_err());
        }
        let whitespace = "\t한글\r\nnext";
        assert_eq!(super::paste_bytes(whitespace, false).unwrap(), whitespace.as_bytes());
        assert_eq!(super::paste_bytes(whitespace, true).unwrap(), [b"\x1b[200~".as_slice(), whitespace.as_bytes(), b"\x1b[201~"].concat());
        assert_eq!(super::key_bytes(&gpui::Keystroke::parse("ctrl-c").unwrap()), Some(vec![3]));
    }
    use super::*;

    fn parse(rows: u16, cols: u16, bytes: &[u8]) -> Frame {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(bytes);
        frame_of(parser.screen())
    }

    #[test]
    fn filtered_empty_cell_is_a_blank_not_a_wide_continuation() {
        use remuda_core::agent::StyledCell;
        let cells = vec![vec![
            StyledCell {
                text: String::new(),
                ..Default::default()
            },
            StyledCell {
                text: "X".into(),
                ..Default::default()
            },
        ]];
        let frame = frame_from_styled(cells);
        assert!(!frame.cells[0][0].continuation);
        assert_eq!(frame.cells[0][0].text, "");
        assert_eq!(frame.cells[0][1].text, "X");

        let cells = vec![vec![
            StyledCell {
                text: "🏇".into(),
                wide: true,
                ..Default::default()
            },
            StyledCell {
                text: String::new(),
                ..Default::default()
            },
            StyledCell {
                text: "X".into(),
                ..Default::default()
            },
        ]];
        let frame = frame_from_styled(cells);
        assert!(frame.cells[0][0].wide);
        assert!(frame.cells[0][1].continuation);
        assert_eq!(frame.cells[0][2].text, "X");
    }

    #[test]
    fn large_grids_are_capped_by_rows_without_losing_columns() {
        assert_eq!(clamp_grid(80, 24), (80, 24));
        let (cols, rows) = clamp_grid(500, 300);
        assert_eq!(cols, 500, "columns are the user-visible width and are kept");
        assert!(
            usize::from(cols) * usize::from(rows) <= MAX_RENDER_CELLS,
            "the rendered grid stays bounded: {cols}x{rows}"
        );
    }

    #[test]
    fn default_foreground_and_background_resolve_by_role_and_inverse_swaps() {
        let (fg, bg) = resolve_colors(AnsiColor::Default, AnsiColor::Default, false);
        assert_ne!(fg, bg, "default text must contrast with the default background");
        assert_eq!(fg, 0xe8eaf0);
        assert_eq!(bg, 0x101216);
        assert_eq!(
            resolve_colors(AnsiColor::Default, AnsiColor::Default, true),
            (bg, fg)
        );
        let (red, blue) = resolve_colors(AnsiColor::Indexed(1), AnsiColor::Indexed(4), false);
        assert_eq!(red, 0xcc6666);
        assert_eq!(blue, 0x6f9fd8);
        assert_eq!(
            resolve_colors(AnsiColor::Indexed(1), AnsiColor::Indexed(4), true),
            (blue, red)
        );
    }

    #[test]
    fn colors_attributes_and_reset_survive_the_frame() {
        let frame = parse(2, 20, b"\x1b[31mred\x1b[0m \x1b[1mbold\x1b[0m");
        assert_eq!(frame.cells[0][0].fg, AnsiColor::Indexed(1));
        assert!(frame.cells[0][4].bold);
        assert_eq!(frame.cells[0][4].text, "b");
        assert_eq!(frame.cells[0][9].fg, AnsiColor::Default);
        assert!(!frame.cells[0][9].bold);
    }

    #[test]
    fn background_inverse_and_256_color_are_carried() {
        let frame = parse(
            1,
            12,
            b"\x1b[44;7mX\x1b[0m\x1b[38;5;196mR\x1b[0m",
        );
        assert_eq!(frame.cells[0][0].bg, AnsiColor::Indexed(4));
        assert!(frame.cells[0][0].inverse);
        assert_eq!(frame.cells[0][1].fg, AnsiColor::Indexed(196));
    }

    #[test]
    fn wrapping_follows_the_pty_width_not_a_text_widget() {
        let frame = parse(2, 5, b"abcdef");
        assert_eq!(frame.cells[0][0].text, "a");
        assert_eq!(frame.cells[0][4].text, "e");
        assert_eq!(frame.cells[1][0].text, "f");
        assert_eq!((frame.rows, frame.cols), (2, 5));
    }

    #[test]
    fn wide_emoji_and_combining_marks_keep_their_cells() {
        let frame = parse(1, 8, "🏇e\u{301}x".as_bytes());
        assert!(frame.cells[0][0].wide, "the horse claims two columns");
        assert!(frame.cells[0][1].continuation);
        assert_eq!(frame.cells[0][2].text, "e\u{301}");
        assert_eq!(frame.cells[0][3].text, "x");
    }
}
