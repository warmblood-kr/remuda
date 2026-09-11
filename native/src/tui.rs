//! The herd: `remuda` with no arguments.
//!
//! One screen, not two. The list is always on the left and the selected session
//! is always on the right; what moves is **focus**. With focus on the list, keys
//! drive remuda; with focus on the session, every key — `x` and `q` included —
//! is typed into the pty, and `Ctrl-\` brings focus back.
//!
//! One key rather than a prefix, because a prefix exists to open a *namespace*
//! and there is exactly one command from inside a session. One keypress is also
//! one byte, so it carries no inter-key timing to be mangled by nested ttys.
//! `Ctrl-\` is [`client::DETACH`], the key `remuda attach` already leaves by.
//!
//! Focus is exclusive, not a peek: it takes the same `Attach` guard a ride does
//! (PRINCIPLES §6, invariant 3), so orchestrated input is refused while a person
//! is typing. [`render`] and [`Ui::on_key`] stay pure, so the state machine is
//! testable on a machine with no tty; everything needing one lives in [`run`].

use crate::client::{self, Hold, RawMode};
use remuda_core::agent::{Color, StyledCell};
use remuda_core::protocol::{expand_runs, Request, Response};
use remuda_core::registry::SessionSummary;
use remuda_core::Size;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// How often an idle herd is relisted, recaptured and redrawn — see
/// [`should_refresh`]. A key always forces an immediate refresh regardless.
const TICK: Duration = Duration::from_millis(250);

/// How often the keyboard is polled while a session has focus — shorter
/// than `TICK` so a keypress is never left waiting to be noticed. Used to
/// also be the redraw cadence; see steps/017 for why that was the bug.
const TICK_TYPING: Duration = Duration::from_millis(40);

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Mode {
    Browse,
    /// The one-line new-session field, holding what has been typed so far.
    Prompt(String),
    /// Kill confirmation for the selected session; only live ones ask.
    Confirm(String),
}

/// Which pane the keyboard is talking to. Always drawn, never remembered — the
/// improvement over a prefix key, whose state is invisible by construction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    List,
    Session,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
    Nothing,
    Quit,
    /// Bytes for the focused session's pty, already encoded.
    Type(Vec<u8>),
    Start(String),
    Kill(String),
}

pub struct Ui {
    pub sessions: Vec<SessionSummary>,
    pub selected: usize,
    pub pan: u16,
    pub mode: Mode,
    pub focus: Focus,
    /// What `n` prefills the prompt with. Held rather than read at the prompt,
    /// so the pure state machine still needs no environment.
    shell: String,
    /// One line of feedback under the list — a refusal, or how the last ride
    /// ended. Cleared by the next keypress that does anything.
    pub notice: Option<String>,
}

impl Ui {
    pub fn new(sessions: Vec<SessionSummary>, shell: &str, notice: Option<String>) -> Self {
        Self {
            sessions,
            selected: 0,
            pan: 0,
            // An empty herd asks rather than acting: it says what to press and
            // waits. It must never spawn a shell on its own — that silent spawn
            // is half of the incident `steps/012` is named after.
            mode: Mode::Browse,
            focus: Focus::List,
            shell: shell.to_string(),
            notice,
        }
    }

    pub fn selected(&self) -> Option<&SessionSummary> {
        self.sessions.get(self.selected)
    }

    /// Keep the cursor on a real row after the herd changes underneath it.
    pub fn clamp(&mut self) {
        if self.selected >= self.sessions.len() {
            self.selected = self.sessions.len().saturating_sub(1);
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        if self.focus == Focus::Session {
            return self.session_key(key);
        }
        match self.mode.clone() {
            Mode::Prompt(buffer) => self.prompt_key(key, buffer),
            Mode::Confirm(name) => self.confirm_key(key, name),
            Mode::Browse => self.browse_key(key),
        }
    }

    /// Everything reaches the pty except the one key that comes back. Checked
    /// first and unconditionally, so no remuda command can be typed by accident
    /// into a shell — the whole reason focus exists rather than modeless keys.
    fn session_key(&mut self, key: KeyEvent) -> Action {
        if is_detach(key) {
            self.focus = Focus::List;
            self.notice = None;
            return Action::Nothing;
        }
        to_bytes(key).map_or(Action::Nothing, Action::Type)
    }

    /// Follow the session the keyboard is talking to by NAME, and hand the
    /// keyboard back when it is gone. Rows move under the cursor when the herd
    /// changes, and focus landing on a neighbour would type into the wrong pty.
    pub fn follow_focus(&mut self, name: Option<&str>) {
        let Some(name) = name else { return };
        match self.sessions.iter().position(|s| s.name == name && s.alive) {
            Some(at) => self.selected = at,
            // With sessions closing themselves on exit, this is how a ride
            // ordinarily ends: you type `exit`, and you are on the list.
            None => self.focus = Focus::List,
        }
    }

    fn browse_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => Action::Quit,
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                self.pan = 0;
                Action::Nothing
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.sessions.len() {
                    self.selected += 1;
                }
                self.pan = 0;
                Action::Nothing
            }
            KeyCode::Char('h') => {
                self.pan = self.pan.saturating_sub(8);
                Action::Nothing
            }
            KeyCode::Char('l') => {
                self.pan = self.pan.saturating_add(8);
                Action::Nothing
            }
            KeyCode::Char('n') => {
                self.mode = Mode::Prompt(self.shell.clone());
                self.notice = None;
                Action::Nothing
            }
            KeyCode::Char('x') => self.kill_selected(),
            KeyCode::Enter => self.focus_session(),
            _ => Action::Nothing,
        }
    }

    /// Refuse before focus moves, not after: an occupied or dead session cannot
    /// be typed into, and finding that out with the keyboard already elsewhere
    /// is the shape of confusion this design keeps deleting.
    fn focus_session(&mut self) -> Action {
        let Some(session) = self.selected() else {
            return Action::Nothing;
        };
        if session.attached {
            self.notice = Some(format!("{} is attached by someone else", session.name));
            return Action::Nothing;
        }
        if !session.alive {
            self.notice = Some(format!(
                "{} has exited — its last screen is here; x clears it",
                session.name
            ));
            return Action::Nothing;
        }
        self.notice = None;
        self.focus = Focus::Session;
        Action::Nothing
    }

    fn kill_selected(&mut self) -> Action {
        let Some((name, alive)) = self.selected().map(|s| (s.name.clone(), s.alive)) else {
            return Action::Nothing;
        };
        self.notice = None;
        if alive {
            self.mode = Mode::Confirm(name);
            Action::Nothing
        } else {
            Action::Kill(name)
        }
    }

    fn confirm_key(&mut self, key: KeyEvent, name: String) -> Action {
        self.mode = Mode::Browse;
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Action::Kill(name),
            _ => Action::Nothing,
        }
    }

    fn prompt_key(&mut self, key: KeyEvent, mut buffer: String) -> Action {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Browse;
                Action::Nothing
            }
            KeyCode::Enter if buffer.trim().is_empty() => {
                self.mode = Mode::Browse;
                Action::Nothing
            }
            KeyCode::Enter => {
                self.mode = Mode::Browse;
                Action::Start(buffer.trim().to_string())
            }
            KeyCode::Backspace => {
                buffer.pop();
                self.mode = Mode::Prompt(buffer);
                Action::Nothing
            }
            KeyCode::Char(c) => {
                buffer.push(c);
                self.mode = Mode::Prompt(buffer);
                Action::Nothing
            }
            _ => Action::Nothing,
        }
    }
}

/// Ctrl-\, the key `remuda attach` already detaches by. ⚠ Also true of Ctrl-4:
/// a terminal sends 0x1C for both, and crossterm spells that byte `C-4`. The
/// conflation is the terminal's, and `client::attach` has always shared it.
fn is_detach(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('\\') | KeyCode::Char('4'))
}

/// A keypress as the bytes a terminal would have sent, or `None` for a key we
/// cannot spell — refused rather than sent as an empty burst, the rule
/// `remuda_core::keys` already states. The spelling is that module's, reused.
fn to_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    let base = match key.code {
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "RET".into(),
        KeyCode::Tab => "TAB".into(),
        KeyCode::BackTab => "<backtab>".into(),
        KeyCode::Backspace => "DEL".into(),
        KeyCode::Esc => "ESC".into(),
        KeyCode::Up => "<up>".into(),
        KeyCode::Down => "<down>".into(),
        KeyCode::Right => "<right>".into(),
        KeyCode::Left => "<left>".into(),
        KeyCode::Home => "<home>".into(),
        KeyCode::End => "<end>".into(),
        KeyCode::Insert => "<insert>".into(),
        KeyCode::Delete => "<delete>".into(),
        KeyCode::PageUp => "<prior>".into(),
        KeyCode::PageDown => "<next>".into(),
        KeyCode::F(n) => format!("<f{n}>"),
        _ => return None,
    };
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    // Shift is deliberately absent: crossterm has already applied it to the
    // character, and naming it again would ask for `C-S-a`, which xterm spells
    // with a CSI parameter that a plain letter has no room for.
    let spec = format!(
        "{}{}{base}",
        if ctrl { "C-" } else { "" },
        if alt { "M-" } else { "" }
    );
    remuda_core::keys::key(&spec)
}

/// List width and preview width, divider excluded. The preview claims what the
/// **widest** session needs — from the herd, not the cursor, so the divider does
/// not jump as you move — and the list takes what is left, within 16..=40.
pub fn layout(term_cols: u16, widest: u16) -> (u16, u16) {
    let usable = term_cols.saturating_sub(1);
    let list = usable.saturating_sub(widest).clamp(16, 40).min(usable);
    (list, usable - list)
}

/// Display columns a unit of session content claims. `char`'s answer of 1
/// for everything is deliberately not correct for a wide (CJK) character —
/// see steps/018 for why that's kept for now.
pub trait Cell {
    fn width(&self) -> u16;
}

impl Cell for char {
    fn width(&self) -> u16 {
        1
    }
}

/// A wide (CJK) cell claims 2 columns; its now-empty continuation claims 0.
/// `impl Cell for char` (the plain, pane-dead path) stays wrong on purpose —
/// see steps/023.
impl Cell for StyledCell {
    fn width(&self) -> u16 {
        if self.wide {
            2
        } else if self.text.is_empty() {
            0
        } else {
            1
        }
    }
}

/// A window from a larger coordinate space onto a smaller one — today, the
/// panel's rectangle onto a session's own screen. Named per 정수님's
/// instruction; see steps/018 for the quote and the cascading design.
pub struct Viewport {
    row_offset: usize,
    col_offset: u16,
    width: u16,
    height: u16,
}

impl Viewport {
    /// Anchor at the bottom: the source's last `height` rows are visible,
    /// the rest scrolled off above — what the preview pane has always done,
    /// since an agent's own input line sits at the bottom.
    pub fn bottom_anchored(source_rows: usize, col_offset: u16, width: u16, height: u16) -> Self {
        Self {
            row_offset: source_rows.saturating_sub(height as usize),
            col_offset,
            width,
            height,
        }
    }

    /// Crop `source` (session coordinates) into panel coordinates: visible
    /// rows, panned and clipped by display column via [`Cell::width`]. Each
    /// row says whether IT was cut; the caller decides how to mark that.
    pub fn crop<C: Cell + Clone>(&self, source: &[Vec<C>]) -> (Vec<(Vec<C>, bool)>, bool) {
        let start = self.row_offset.min(source.len());
        let mut any_cut = false;
        let rows = source[start..]
            .iter()
            .take(self.height as usize)
            .map(|row| {
                let (visible, cut) = self.crop_row(row);
                any_cut |= cut;
                (visible, cut)
            })
            .collect();
        (rows, any_cut)
    }

    fn crop_row<C: Cell + Clone>(&self, row: &[C]) -> (Vec<C>, bool) {
        // No `.max(1)` here on purpose: a wide cell's continuation is a real
        // zero-width entry now (steps/023), and clamping it back up to 1
        // would double-count the column its own wide cell already claimed.
        let total: u32 = row.iter().map(|c| u32::from(c.width())).sum();
        let (offset, width) = (u32::from(self.col_offset), u32::from(self.width));
        let mut out = Vec::new();
        let (mut col, mut taken) = (0u32, 0u32);
        for cell in row {
            let w = u32::from(cell.width());
            if col < offset {
                col += w;
                continue;
            }
            if taken + w > width {
                break;
            }
            taken += w;
            col += w;
            out.push(cell.clone());
        }
        let cut = total > offset + width;
        (out, cut)
    }
}

/// The visible rectangle of a screen: the last `rows` lines, each panned by
/// `pan` and cut to `cols`. Bottom-left, because agents left-align and put
/// their input line at the bottom. The flag says whether anything was cut.
pub fn crop(screen: &str, cols: u16, rows: u16, pan: u16) -> (Vec<String>, bool) {
    let source: Vec<Vec<char>> = screen.lines().map(|line| line.chars().collect()).collect();
    let viewport = Viewport::bottom_anchored(source.len(), pan, cols, rows);
    let (cropped, cut) = viewport.crop(&source);
    let out = cropped
        .into_iter()
        .map(|(cells, row_cut)| {
            let mut visible: String = cells.into_iter().collect();
            // The marker has to go on here rather than in `fit`: by the time
            // the row is padded there is nothing left to tell it was cut.
            if row_cut {
                visible.pop();
                visible.push('→');
            }
            visible
        })
        .collect();
    (out, cut)
}

/// Exactly `width` columns: padded with spaces, or cut with a `→` in the
/// last cell. Counts real display width via `visible_width`, not `char`s —
/// a wide (CJK) name used to overflow this budget. See steps/025.
fn fit(text: &str, width: u16) -> String {
    use unicode_width::UnicodeWidthChar;
    let width = width as usize;
    if visible_width(text) > width {
        let mut out = String::new();
        let mut used = 0usize;
        for c in text.chars() {
            let w = c.width().unwrap_or(1);
            if used + w > width.saturating_sub(1) {
                break;
            }
            out.push(c);
            used += w;
        }
        out.push('→');
        out.push_str(&" ".repeat(width.saturating_sub(used + 1)));
        out
    } else {
        let mut out = text.to_string();
        out.push_str(&" ".repeat(width - visible_width(text)));
        out
    }
}

/// The whole frame as one string of text and ANSI cursor moves. Pure on
/// purpose: this is the part a test can read without owning a terminal.
pub fn render(ui: &Ui, screen: &str, server: &str, cols: u16, rows: u16) -> String {
    let (list_w, preview_w) = layout(cols, widest(ui));
    let body = rows.saturating_sub(1);
    // The border, and the only thing on screen that is always saying where the
    // keyboard is pointing. A prefix key's state is invisible; this is not.
    let divider = match ui.focus {
        Focus::List => "│",
        Focus::Session => "\x1b[7m┃\x1b[0m",
    };

    let (lines, cut) = crop(screen, preview_w, body, ui.pan);
    let mut out = String::from("\x1b[H\x1b[2J");
    for row in 0..body {
        out.push_str(&format!("\x1b[{};1H", row + 1));
        // The left column has a header; the preview deliberately has none, so
        // its first row is the session's own first row — the same thing a ride
        // shows, at the same place on the screen.
        let left = if row == 0 {
            format!("remuda · {server}")
        } else {
            list_row(ui, row as usize - 1, list_w)
        };
        out.push_str(&fit(&left, list_w));
        out.push_str(divider);
        let line = lines.get(row as usize).map_or("", String::as_str);
        out.push_str(&fit(line, preview_w));
    }

    out.push_str(&format!("\x1b[{};1H", rows));
    out.push_str(&fit(&footer(ui, cut, preview_w), cols));
    out
}

/// True when a cell carries no colour or attribute at all — the case that
/// must emit no SGR, so an all-default grid renders byte-identical to `crop`.
fn is_plain(c: &StyledCell) -> bool {
    c.fg == Color::Default
        && c.bg == Color::Default
        && !c.bold
        && !c.dim
        && !c.italic
        && !c.underline
        && !c.inverse
}

/// Every non-default attribute of `cell`, regenerated from scratch. Simpler
/// and correct: a style change resets first, so no per-attribute "turn this
/// back off" code is ever needed.
fn sgr_codes(cell: &StyledCell) -> String {
    let mut out = String::new();
    if cell.bold {
        out.push_str("\x1b[1m");
    }
    if cell.dim {
        out.push_str("\x1b[2m");
    }
    if cell.italic {
        out.push_str("\x1b[3m");
    }
    if cell.underline {
        out.push_str("\x1b[4m");
    }
    if cell.inverse {
        out.push_str("\x1b[7m");
    }
    push_color(&mut out, cell.fg, true);
    push_color(&mut out, cell.bg, false);
    out
}

/// One half (fg or bg) of a cell's colour, in the convention this codebase
/// already writes raw escapes in — see steps/020 for why not `crossterm::style`.
fn push_color(out: &mut String, color: Color, fg: bool) {
    match color {
        Color::Default => {}
        Color::Idx(n) if n < 8 => out.push_str(&format!("\x1b[{}{n}m", if fg { 3 } else { 4 })),
        Color::Idx(n) => out.push_str(&format!("\x1b[{}{}m", if fg { 9 } else { 10 }, n - 8)),
        Color::Rgb(r, g, b) => {
            out.push_str(&format!("\x1b[{};2;{r};{g};{b}m", if fg { 38 } else { 48 }))
        }
    }
}

/// One row of styled cells as text plus minimal SGR — see steps/020 for the
/// shape this has to satisfy (byte-identity when plain; minimal when not).
fn render_styled_row(cells: &[StyledCell]) -> String {
    let mut out = String::new();
    let mut current: Option<&StyledCell> = None;
    let mut styled_at_all = false;
    for cell in cells {
        let changed = match current {
            None => !is_plain(cell),
            Some(prev) => !same_style(prev, cell),
        };
        if changed {
            out.push_str("\x1b[0m");
            out.push_str(&sgr_codes(cell));
            styled_at_all |= !is_plain(cell);
        }
        out.push_str(&cell.text);
        current = Some(cell);
    }
    if styled_at_all {
        out.push_str("\x1b[0m");
    }
    out
}

/// Whether two cells would emit the same SGR — everything but `text`.
fn same_style(a: &StyledCell, b: &StyledCell) -> bool {
    a.fg == b.fg
        && a.bg == b.bg
        && a.bold == b.bold
        && a.dim == b.dim
        && a.italic == b.italic
        && a.underline == b.underline
        && a.inverse == b.inverse
}

/// The styled counterpart of the free `crop`, byte-identical to it when
/// every cell is plain — see steps/020's oracle.
fn crop_styled(cells: &[Vec<StyledCell>], cols: u16, rows: u16, pan: u16) -> (Vec<String>, bool) {
    let viewport = Viewport::bottom_anchored(cells.len(), pan, cols, rows);
    let (cropped, cut) = viewport.crop(cells);
    let out = cropped
        .into_iter()
        .map(|(mut visible, row_cut)| {
            if row_cut {
                // Free at least 1 display column for `→`. A wide cell's
                // trailing continuation frees 0 on its own, so keep popping
                // until real width comes back — see steps/023.
                let mut freed = 0u16;
                while freed < 1 {
                    match visible.pop() {
                        Some(cell) => freed += cell.width(),
                        None => break,
                    }
                }
            }
            let mut s = render_styled_row(&visible);
            if row_cut {
                s.push('→');
            }
            s
        })
        .collect();
    (out, cut)
}

/// A styled row's display width, ignoring the SGR bytes riding along with
/// it, and counting a wide (CJK) character as the 2 columns it actually
/// draws — `fit`'s plain char count would under-count it by 1. See steps/023.
fn visible_width(s: &str) -> usize {
    use unicode_width::UnicodeWidthChar;
    let mut width = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for skip in chars.by_ref() {
                if skip == 'm' {
                    break;
                }
            }
        } else {
            width += c.width().unwrap_or(1);
        }
    }
    width
}

/// `fit`'s padding step for a styled row: pad to `width` visible columns
/// without counting escape bytes as columns. `crop_styled` never hands back
/// a row wider than asked, so there is nothing here to cut.
fn fit_styled(line: &str, width: u16) -> String {
    let pad = (width as usize).saturating_sub(visible_width(line));
    let mut out = line.to_string();
    out.push_str(&" ".repeat(pad));
    out
}

/// Same frame as [`render`], but the preview column carries real colour from
/// a styled capture, and is written with row-local clears plus synchronized
/// output instead of a full erase — see steps/020 and steps/026.
pub fn render_styled(
    ui: &Ui,
    cells: &[Vec<StyledCell>],
    server: &str,
    cols: u16,
    rows: u16,
) -> String {
    let (list_w, preview_w) = layout(cols, widest(ui));
    let body = rows.saturating_sub(1);
    let divider = match ui.focus {
        Focus::List => "│",
        Focus::Session => "\x1b[7m┃\x1b[0m",
    };

    let (lines, cut) = crop_styled(cells, preview_w, body, ui.pan);
    let mut out = String::from("\x1b[?2026h\x1b[H");
    for row in 0..body {
        out.push_str(&format!("\x1b[{};1H\x1b[K", row + 1));
        let left = if row == 0 {
            format!("remuda · {server}")
        } else {
            list_row(ui, row as usize - 1, list_w)
        };
        out.push_str(&fit(&left, list_w));
        out.push_str(divider);
        let line = lines.get(row as usize).map_or("", String::as_str);
        out.push_str(&fit_styled(line, preview_w));
    }

    out.push_str(&format!("\x1b[{};1H\x1b[K", rows));
    out.push_str(&fit(&footer(ui, cut, preview_w), cols));
    // Erase anything a previous, taller frame left below this one — a resize
    // to fewer rows is the only way stale content can survive past here.
    out.push_str("\x1b[J\x1b[?2026l");
    out
}

/// The widest session in the herd, which is what the preview column claims —
/// from the herd rather than the cursor, so the divider does not jump. Zero
/// when there is no herd: nothing to preview, so nothing to reserve.
fn widest(ui: &Ui) -> u16 {
    ui.sessions.iter().map(|s| s.size.cols()).max().unwrap_or(0)
}

/// The size a session started from here is given: the pane it will live in.
/// Nothing can resize a pty afterwards (PRINCIPLES §6), so this is the only
/// chance to make it fit — and `Size::new` still floors it at 80×24.
pub fn pane_size(ui: &Ui, cols: u16, rows: u16) -> Size {
    let (_, preview_w) = layout(cols, widest(ui));
    Size::new(preview_w, rows.saturating_sub(1))
}

/// A row that degrades instead of being cut. When the preview claims most of
/// the terminal the list can floor at 16 columns, and a truncated row loses
/// `live`/`dead` — the one field the whole list exists to show.
fn list_row(ui: &Ui, row: usize, width: u16) -> String {
    if ui.sessions.is_empty() {
        // The empty herd says what it is and what to do about it. It does not
        // open a prompt on its own, and it never starts anything by itself.
        return match row {
            1 => "  the herd is empty.".into(),
            3 => "  press n to start a session.".into(),
            _ => String::new(),
        };
    }
    let Some(session) = ui.sessions.get(row) else {
        return String::new();
    };
    let cursor = if row == ui.selected { "▸" } else { " " };
    let flag = if session.attached { "⚑" } else { " " };
    let state = if session.alive { "live" } else { "dead" };
    // No time-driven field: `idle` never resets on typing (only on `send`,
    // see session.rs), so it read as an uptime clock, not "liveness" — and it
    // was the only per-second repaint source in the whole TUI. See steps/026.
    let tail = if width >= 22 {
        format!("{state} {flag}")
    } else {
        flag.to_string()
    };
    let room = (width as usize).saturating_sub(visible_width(&tail) + 3);
    format!("{cursor} {} {tail}", fit(&session.name, room as u16))
}

/// The crop notice moved here when the preview lost its title band: a crop that
/// reads as absence is the failure this repo keeps re-discovering, and the
/// footer is the only band left that is not the session's own screen.
fn footer(ui: &Ui, cut: bool, preview_w: u16) -> String {
    match &ui.mode {
        Mode::Prompt(buffer) => format!("start: {buffer}▏   ⏎ run · esc cancel"),
        Mode::Confirm(name) => format!("kill {name}? it is running — y / n"),
        // Focus is named in words as well as drawn, because the one thing a
        // person must never wonder is where their next keystroke lands.
        Mode::Browse if ui.focus == Focus::Session => format!(
            "▶ {} — every key goes to the session{}   ctrl-\\ back to the list",
            ui.selected().map_or("", |s| s.name.as_str()),
            if cut { format!("   showing {preview_w} cols") } else { String::new() },
        ),
        Mode::Browse => match &ui.notice {
            Some(notice) => format!("remuda: {notice}"),
            None if ui.sessions.is_empty() => "n new   q quit".into(),
            None if cut => format!(
                "↑↓ select   ⏎ enter   n new   x kill   showing {preview_w} cols — h/l pans   q quit"
            ),
            None => "↑↓ select   ⏎ enter   n new   x kill   q quit".into(),
        },
    }
}

/// Whether to relist, recapture and rebuild the frame: forced right after a
/// key, or because `tick` elapsed. See steps/017 and steps/026 for why the
/// caller picks `tick` rather than this always using `TICK`.
fn should_refresh(forced: bool, since_last: Duration, tick: Duration) -> bool {
    forced || since_last >= tick
}

/// The one expensive act of a frame: relist the herd, take or release the
/// focus hold, capture the focused screen and repaint if that changed
/// anything. `skip_list` skips the relist for a `Type`-forced wake. See steps/022.
fn refresh(
    path: &Path,
    server: &str,
    ui: &mut Ui,
    held: &mut Option<(String, Hold)>,
    painted: &mut String,
    skip_list: bool,
) -> std::io::Result<(u16, u16)> {
    if !skip_list {
        match list(path) {
            Ok(sessions) => ui.sessions = sessions,
            // Keep the last known herd rather than blanking it: a transport
            // failure is not a report that every session vanished. See steps/021.
            Err(e) => ui.notice = Some(e),
        }
    }
    ui.clamp();
    ui.follow_focus(held.as_ref().map(|(name, _)| name.as_str()));
    // Dropping the hold is the detach, and this is the only place it
    // happens: focus went back to the list, or the session ended under it.
    if ui.focus == Focus::List {
        *held = None;
    } else if held.is_none() {
        *held = take(path, ui);
    }

    let cells = match ui.selected().map(|s| s.name.clone()) {
        Some(name) => match capture_styled(path, &name) {
            Ok(cells) => cells,
            Err(e) => {
                ui.notice = Some(format!("{name}: {e}"));
                Vec::new()
            }
        },
        None => Vec::new(),
    };
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let frame = render_styled(ui, &cells, server, cols, rows);
    // The write is gated on change, as it always was — but before
    // `should_refresh` existed, everything ABOVE this line (a relist, an IPC
    // round-trip for a full screen snapshot, a rebuilt frame) ran on every
    // wake of the input poll too, key or not. That is what a session with
    // focus cost ~25 times a second while sitting idle, and every keystroke
    // on top of that.
    if frame != *painted {
        let mut stdout = std::io::stdout();
        stdout.write_all(frame.as_bytes())?;
        stdout.flush()?;
        *painted = frame;
    }
    Ok((cols, rows))
}

/// Draw the herd until the user quits. One screen for the whole run: focus
/// moves between the panes, and the terminal is never handed over, so the
/// alternate screen is entered exactly once. `notice` is what stderr cannot reach.
pub fn run(path: &Path, server: &str, notice: Option<String>) -> std::io::Result<()> {
    let _terminal = RawMode::enable()?;
    let shell = crate::daemon::default_shell();
    let (sessions, list_err) = match list(path) {
        Ok(sessions) => (sessions, None),
        Err(e) => (Vec::new(), Some(e)),
    };
    // The caller's own notice (e.g. a version-skew warning) is the more
    // specific diagnosis when both exist; only fall back to the raw list
    // failure if nothing else already explains why nothing works.
    let mut ui = Ui::new(sessions, &shell, notice.or(list_err));
    let mut painted = String::new();
    // The exclusive hold on the focused session, and the name it was taken on.
    // Its `Drop` is the detach, so letting it fall out of scope is the release.
    let mut held: Option<(String, Hold)> = None;
    let mut last_refresh = Instant::now();
    let mut force_refresh = true;
    // Set only by an `Action::Type` below, consumed by the very next refresh,
    // then always cleared — never carried into a tick-driven refresh.
    let mut skip_list = false;
    let (mut cols, mut rows) = (80u16, 24u16);

    loop {
        let tick = if ui.focus == Focus::Session {
            TICK_TYPING
        } else {
            TICK
        };
        if should_refresh(force_refresh, last_refresh.elapsed(), tick) {
            force_refresh = false;
            last_refresh = Instant::now();
            (cols, rows) = refresh(path, server, &mut ui, &mut held, &mut painted, skip_list)?;
            skip_list = false;
        }

        if !crossterm::event::poll(tick)? {
            continue;
        }
        let Event::Key(key) = crossterm::event::read()? else {
            continue;
        };
        // Windows delivers Release as well, and acting on both double-fires.
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let action = ui.on_key(key);
        skip_list = matches!(action, Action::Type(_));
        match action {
            Action::Nothing => {}
            Action::Quit => return Ok(()),
            Action::Type(bytes) => {
                if let Some((name, hold)) = &held {
                    if let Err(e) = hold.keys(&bytes) {
                        ui.notice = Some(format!("{name}: {e}"));
                        ui.focus = Focus::List;
                    }
                }
            }
            Action::Start(command) => {
                ui.notice = start(path, &command, pane_size(&ui, cols, rows)).err();
                match list(path) {
                    Ok(sessions) => ui.sessions = sessions,
                    Err(e) if ui.notice.is_none() => ui.notice = Some(e),
                    Err(_) => {}
                }
            }
            Action::Kill(name) => ui.notice = kill(path, &name).err(),
        }
        // Not `painted.clear()`: the frame-vs-`painted` compare in `refresh`
        // already skips the write when a key changed nothing visible — see
        // steps/026. Only the immediate re-check is forced here.
        force_refresh = true;
    }
}

/// Take the exclusive hold focus needs, or say why not and stay on the list.
/// The refusal is the daemon's — a session someone else attached is refused
/// there, not here, so a second viewer over any transport is refused too.
fn take(path: &Path, ui: &mut Ui) -> Option<(String, Hold)> {
    let Some(name) = ui.selected().map(|s| s.name.clone()) else {
        ui.focus = Focus::List;
        return None;
    };
    match client::hold(path, &name) {
        Ok(hold) => Some((name, hold)),
        Err(e) => {
            ui.notice = Some(format!("{name}: {e}"));
            ui.focus = Focus::List;
            None
        }
    }
}

/// A transport failure and a daemon `Response::Error` are both real answers —
/// `start`/`kill` already say so this way; see steps/021 for why the herd list
/// and the styled capture used to say it a different, worse way instead.
fn list(path: &Path) -> Result<Vec<SessionSummary>, String> {
    match client::request(path, &Request::List) {
        Ok(Response::Sessions(sessions)) => Ok(sessions),
        Ok(Response::Error(reason)) => Err(reason),
        other => Err(format!("{other:?}")),
    }
}

/// Styled counterpart of the (now unused) plain `capture` — see steps/020,
/// 021. The wire carries runs, expanded back to cells here — see steps/022.
fn capture_styled(path: &Path, name: &str) -> Result<Vec<Vec<StyledCell>>, String> {
    match client::request(
        path,
        &Request::CaptureStyled {
            name: name.to_string(),
        },
    ) {
        Ok(Response::StyledScreen(runs)) => Ok(runs.iter().map(|row| expand_runs(row)).collect()),
        Ok(Response::Error(reason)) => Err(reason),
        other => Err(format!("{other:?}")),
    }
}

/// A session started here is sized to the pane it will live in, and keeps that
/// size for life — nothing can resize a pty (PRINCIPLES §6). Sizing it to the
/// whole terminal instead would guarantee a crop the list can never give back.
fn start(path: &Path, command: &str, size: Size) -> Result<(), String> {
    let request = Request::New {
        name: None,
        command: command.split_whitespace().map(str::to_string).collect(),
        size,
    };
    match client::request(path, &request) {
        Ok(Response::Value(_)) => Ok(()),
        Ok(Response::Error(reason)) => Err(reason),
        other => Err(format!("{other:?}")),
    }
}

fn kill(path: &Path, name: &str) -> Result<(), String> {
    let request = Request::Close {
        name: name.to_string(),
    };
    match client::request(path, &request) {
        Ok(Response::Ok) => Ok(()),
        Ok(Response::Error(reason)) => Err(reason),
        other => Err(format!("{other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_core::Size;

    /// The regression steps/017 guards: an idle *list*-focused herd must not
    /// redo the full IPC cycle on every poll wake — `run` only uses the
    /// faster `TICK_TYPING` gate for a focused session, see the next test.
    #[test]
    fn an_idle_list_refreshes_on_the_slow_tick_not_every_poll_wake() {
        let mut refreshes = 0;
        let mut since_last = Duration::ZERO;
        for _ in 0..25 {
            since_last += TICK_TYPING;
            if should_refresh(false, since_last, TICK) {
                refreshes += 1;
                since_last = Duration::ZERO;
            }
        }
        assert!(
            refreshes <= 5,
            "an idle list must not redo the full IPC cycle on every \
             {TICK_TYPING:?} poll wake — got {refreshes} refreshes in one \
             simulated second; gating on TICK_TYPING gives 25"
        );
    }

    /// The lag steps/026 fixes: a keystroke's echo can land just after the
    /// capture that missed it, and `run` gates the retry on `TICK_TYPING`
    /// while focused, so the corrected screen is ~40ms behind, not 250ms.
    #[test]
    fn a_focused_sessions_late_echo_is_retried_within_the_typing_tick() {
        assert!(
            !should_refresh(false, TICK_TYPING - Duration::from_millis(1), TICK_TYPING),
            "sanity: not yet at the boundary"
        );
        assert!(
            should_refresh(false, TICK_TYPING, TICK_TYPING),
            "a focused session's unforced refresh must fire within TICK_TYPING, \
             not wait out the rest of TICK"
        );
    }

    /// The other half of the report: typing does not wait for a tick at all.
    /// Echoing a keystroke has always meant an immediate refresh, and that is
    /// still true — it is proportional to typing speed, not the bug.
    #[test]
    fn a_key_forces_an_immediate_refresh_regardless_of_the_tick() {
        assert!(
            should_refresh(true, Duration::ZERO, TICK),
            "a keystroke must not wait for a tick to be echoed"
        );
    }

    /// Sanity on the list-tick boundary itself, so the tests above cannot
    /// both pass by accident of a threshold that admits everything or nothing.
    #[test]
    fn refresh_waits_for_the_slow_tick_when_nothing_forced_it() {
        assert!(!should_refresh(
            false,
            TICK - Duration::from_millis(1),
            TICK
        ));
        assert!(should_refresh(false, TICK, TICK));
    }

    fn row(name: &str, alive: bool, attached: bool) -> SessionSummary {
        SessionSummary {
            name: name.into(),
            alive,
            idle: Duration::from_secs(4),
            size: Size::new(80, 24),
            attached,
        }
    }

    fn ui(rows: Vec<SessionSummary>) -> Ui {
        Ui::new(rows, "/bin/sh", None)
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn the_selection_stops_at_both_ends() {
        let mut ui = ui(vec![row("a", true, false), row("b", true, false)]);
        ui.on_key(press(KeyCode::Up));
        assert_eq!(ui.selected, 0, "up at the top stays");
        ui.on_key(press(KeyCode::Down));
        ui.on_key(press(KeyCode::Down));
        assert_eq!(ui.selected, 1, "down at the bottom stays");
    }

    #[test]
    fn enter_points_the_keyboard_at_the_selected_session() {
        let mut ui = ui(vec![row("a", true, false), row("b", true, false)]);
        ui.on_key(press(KeyCode::Char('j')));
        assert_eq!(ui.on_key(press(KeyCode::Enter)), Action::Nothing);
        assert_eq!(ui.focus, Focus::Session);
        assert_eq!(ui.selected().unwrap().name, "b");
        // The list stays on screen: nothing about entering removes a session.
        assert_eq!(ui.sessions.len(), 2);
    }

    /// The bug this whole mode exists to make impossible: in the list `x` kills
    /// and `q` quits, and inside a session both must be plain text.
    #[test]
    fn with_focus_on_the_session_the_command_keys_are_just_text() {
        let mut ui = ui(vec![row("sh", true, false)]);
        ui.on_key(press(KeyCode::Enter));
        for (code, byte) in [('x', b'x'), ('q', b'q'), ('n', b'n'), ('y', b'y')] {
            assert_eq!(
                ui.on_key(press(KeyCode::Char(code))),
                Action::Type(vec![byte]),
                "{code} must reach the pty, not remuda"
            );
        }
        assert_eq!(ui.mode, Mode::Browse, "no prompt, no kill confirmation");
        assert_eq!(ui.focus, Focus::Session, "and the keyboard has not moved");
    }

    #[test]
    fn ctrl_backslash_is_the_only_key_that_comes_back() {
        let mut ui = ui(vec![row("sh", true, false)]);
        ui.on_key(press(KeyCode::Enter));
        assert_eq!(ui.focus, Focus::Session);
        let detach = KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL);
        assert_eq!(ui.on_key(detach), Action::Nothing);
        assert_eq!(ui.focus, Focus::List);
        // And now the same keys are commands again.
        assert_eq!(ui.on_key(press(KeyCode::Char('q'))), Action::Quit);
    }

    /// crossterm reports 0x1C as `C-4`, because a terminal sends that byte for
    /// Ctrl-\ and Ctrl-4 alike. Accepting only `C-\` would leave the key dead
    /// on unix — measured against crossterm 0.29's own parser.
    #[test]
    fn the_detach_byte_is_recognised_however_crossterm_spells_it() {
        for code in [KeyCode::Char('\\'), KeyCode::Char('4')] {
            let mut ui = ui(vec![row("sh", true, false)]);
            ui.on_key(press(KeyCode::Enter));
            ui.on_key(KeyEvent::new(code, KeyModifiers::CONTROL));
            assert_eq!(ui.focus, Focus::List, "{code:?} did not come back");
        }
    }

    #[test]
    fn a_key_reaches_the_pty_as_the_bytes_a_terminal_would_have_sent() {
        let mut ui = ui(vec![row("sh", true, false)]);
        ui.on_key(press(KeyCode::Enter));
        // Enter is CR, not LF: canonical mode takes CR as submit.
        assert_eq!(
            ui.on_key(press(KeyCode::Enter)),
            Action::Type(b"\r".to_vec())
        );
        assert_eq!(
            ui.on_key(press(KeyCode::Up)),
            Action::Type(b"\x1b[A".to_vec())
        );
        assert_eq!(
            ui.on_key(press(KeyCode::Backspace)),
            Action::Type(vec![0x7f]),
            "backspace sends DEL, not BS"
        );
        assert_eq!(
            ui.on_key(press(KeyCode::BackTab)),
            Action::Type(b"\x1b[Z".to_vec()),
            "shift-tab is its own sequence, and agents cycle modes on it"
        );
        assert_eq!(
            ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Type(vec![0x03]),
            "Ctrl-C interrupts the program, it does not quit remuda"
        );
    }

    #[test]
    fn a_session_that_ends_hands_the_keyboard_back() {
        let mut ui = ui(vec![row("sh", true, false)]);
        ui.on_key(press(KeyCode::Enter));
        assert_eq!(ui.focus, Focus::Session);
        // What the next `List` returns once the shell has exited and closed.
        ui.sessions.clear();
        ui.clamp();
        ui.follow_focus(Some("sh"));
        assert_eq!(ui.focus, Focus::List);
    }

    /// The keyboard is pointed at a name, not at a row. A session vanishing
    /// above the focused one shifts every row below it, and focus following the
    /// row would start typing into a neighbour.
    #[test]
    fn focus_follows_the_session_when_the_herd_shifts_under_it() {
        let mut ui = ui(vec![
            row("a", true, false),
            row("b", true, false),
            row("c", true, false),
        ]);
        ui.on_key(press(KeyCode::Char('j')));
        ui.on_key(press(KeyCode::Enter));
        assert_eq!(ui.selected().unwrap().name, "b");

        ui.sessions.remove(0);
        ui.clamp();
        ui.follow_focus(Some("b"));
        assert_eq!(ui.focus, Focus::Session);
        assert_eq!(ui.selected().unwrap().name, "b", "not c");
    }

    #[test]
    fn a_session_someone_else_holds_is_refused_before_the_handover() {
        let mut ui = ui(vec![row("busy", true, true)]);
        assert_eq!(ui.on_key(press(KeyCode::Enter)), Action::Nothing);
        assert!(ui.notice.unwrap().contains("attached by someone else"));
    }

    #[test]
    fn a_dead_session_is_not_ridden_it_is_explained() {
        let mut ui = ui(vec![row("gone", false, false)]);
        assert_eq!(ui.on_key(press(KeyCode::Enter)), Action::Nothing);
        assert!(ui.notice.unwrap().contains("x clears it"));
    }

    #[test]
    fn killing_a_live_session_asks_and_killing_a_dead_one_does_not() {
        let mut ui = ui(vec![row("live", true, false)]);
        assert_eq!(ui.on_key(press(KeyCode::Char('x'))), Action::Nothing);
        assert_eq!(ui.mode, Mode::Confirm("live".into()));
        assert_eq!(
            ui.on_key(press(KeyCode::Char('y'))),
            Action::Kill("live".into())
        );

        let mut dead = super::tests::ui(vec![row("dead", false, false)]);
        assert_eq!(
            dead.on_key(press(KeyCode::Char('x'))),
            Action::Kill("dead".into())
        );
    }

    #[test]
    fn anything_but_y_cancels_the_kill() {
        let mut ui = ui(vec![row("live", true, false)]);
        ui.on_key(press(KeyCode::Char('x')));
        assert_eq!(ui.on_key(press(KeyCode::Char('n'))), Action::Nothing);
        assert_eq!(ui.mode, Mode::Browse, "and it does not open the new prompt");
    }

    #[test]
    fn an_empty_herd_invites_and_does_not_open_a_prompt_by_itself() {
        let ui = ui(vec![]);
        assert_eq!(ui.mode, Mode::Browse, "nothing was opened for the user");
        assert_eq!(ui.focus, Focus::List);
    }

    #[test]
    fn the_prompt_edits_and_starts_what_it_shows() {
        let mut ui = ui(vec![]);
        ui.on_key(press(KeyCode::Char('n')));
        ui.on_key(press(KeyCode::Backspace));
        for c in "!".chars() {
            ui.on_key(press(KeyCode::Char(c)));
        }
        assert_eq!(ui.mode, Mode::Prompt("/bin/s!".into()));
        assert_eq!(
            ui.on_key(press(KeyCode::Enter)),
            Action::Start("/bin/s!".into())
        );
        assert_eq!(ui.mode, Mode::Browse);
    }

    #[test]
    fn esc_leaves_the_prompt_without_starting_anything() {
        let mut ui = ui(vec![row("a", true, false)]);
        ui.on_key(press(KeyCode::Char('n')));
        assert_eq!(
            ui.mode,
            Mode::Prompt("/bin/sh".into()),
            "n prefills the shell — the prompt is the same wherever it came from"
        );
        assert_eq!(ui.on_key(press(KeyCode::Esc)), Action::Nothing);
        assert_eq!(ui.mode, Mode::Browse);
    }

    #[test]
    fn q_and_ctrl_c_leave_but_only_in_browse() {
        let mut ui = ui(vec![row("a", true, false)]);
        assert_eq!(ui.on_key(press(KeyCode::Char('q'))), Action::Quit);
        assert_eq!(
            ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        );
        // In the prompt they are text, not commands — otherwise typing the name
        // of a program with a `q` in it quits.
        ui.on_key(press(KeyCode::Char('n')));
        assert_eq!(ui.on_key(press(KeyCode::Char('q'))), Action::Nothing);
        assert_eq!(ui.mode, Mode::Prompt("/bin/shq".into()));
    }

    #[test]
    fn the_preview_claims_what_the_widest_session_needs() {
        // 120 wide, sessions 80×24: the preview fits exactly and nothing crops.
        assert_eq!(layout(120, 80), (39, 80));
        // Sessions as wide as the terminal: the list floors at 16 and the
        // preview takes the rest, cropping the difference.
        assert_eq!(layout(120, 120), (16, 103));
        // Wide terminal, narrow sessions: the list ceilings at 40.
        assert_eq!(layout(400, 80), (40, 359));
    }

    #[test]
    fn a_terminal_too_small_to_split_still_produces_a_frame() {
        let (list, preview) = layout(10, 80);
        assert_eq!(list + preview, 9, "the divider, and no underflow");
    }

    /// `crop`'s behaviour before the `Viewport` migration, kept only as the
    /// reference the byte-identity oracle below migrates against. See
    /// steps/018.
    fn crop_reference(screen: &str, cols: u16, rows: u16, pan: u16) -> (Vec<String>, bool) {
        let lines: Vec<&str> = screen.lines().collect();
        let start = lines.len().saturating_sub(rows as usize);
        let mut cut = false;
        let out = lines[start..]
            .iter()
            .map(|line| {
                let chars: Vec<char> = line.chars().collect();
                let mut visible: String = chars
                    .iter()
                    .skip(pan as usize)
                    .take(cols as usize)
                    .collect();
                if chars.len() > (pan as usize) + (cols as usize) {
                    cut = true;
                    visible.pop();
                    visible.push('→');
                }
                visible
            })
            .collect();
        (out, cut)
    }

    #[test]
    fn the_viewport_migration_reproduces_crop_byte_for_byte() {
        let screens = [
            "",
            "one line, no newline",
            "short\nlines",
            "a line that is definitely longer than the pane\nsecond, shorter",
            "exact\nfit!!",
            "one\ntwo\nthree\nfour\nfive\nsix",
            "унікод and 漢字 mixed with ascii",
        ];
        for screen in screens {
            // Exhaustive through and just past the two boundaries `crop`
            // actually branches on — a line exactly full (cols/pan) and the
            // screen exactly full (rows) — rather than a handful of picked
            // geometries, which a first pass here missed: a deliberately
            // injected off-by-one in the cut threshold (`total > offset +
            // width + 1`) passed a hand-picked geometry list undetected and
            // was only caught once the sweep below was made exhaustive near
            // the boundary. Kept exhaustive so that class of gap cannot
            // recur silently.
            let max_len = screen.lines().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
            let max_rows = screen.lines().count() as u16;
            for cols in 0..=max_len + 2 {
                for pan in 0..=max_len + 2 {
                    for rows in 0..=max_rows + 2 {
                        let got = crop(screen, cols, rows, pan);
                        let want = crop_reference(screen, cols, rows, pan);
                        assert_eq!(
                            got, want,
                            "screen={screen:?} cols={cols} rows={rows} pan={pan}"
                        );
                    }
                }
            }
        }
    }

    /// Every cell plain, so the styled path must degrade to exactly what the
    /// plain `crop` produces — same screens, same exhaustive sweep as the
    /// migration oracle above. See steps/020.
    #[test]
    fn the_styled_crop_matches_plain_crop_when_every_cell_is_default() {
        let screens = [
            "",
            "one line, no newline",
            "short\nlines",
            "a line that is definitely longer than the pane\nsecond, shorter",
            "exact\nfit!!",
            "one\ntwo\nthree\nfour\nfive\nsix",
            "унікод and 漢字 mixed with ascii",
        ];
        for screen in screens {
            let cells: Vec<Vec<StyledCell>> = screen
                .lines()
                .map(|l| {
                    l.chars()
                        .map(|c| StyledCell {
                            text: c.to_string(),
                            ..Default::default()
                        })
                        .collect()
                })
                .collect();
            let max_len = screen.lines().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
            let max_rows = screen.lines().count() as u16;
            for cols in 0..=max_len + 2 {
                for pan in 0..=max_len + 2 {
                    for rows in 0..=max_rows + 2 {
                        let want = crop(screen, cols, rows, pan);
                        let (got_lines, got_cut) = crop_styled(&cells, cols, rows, pan);
                        // Defensive: a default-only row should never actually
                        // carry a trailing reset, but strip one if present
                        // rather than assume it.
                        let got_lines: Vec<String> = got_lines
                            .into_iter()
                            .map(|l| l.strip_suffix("\x1b[0m").unwrap_or(&l).to_string())
                            .collect();
                        assert_eq!(
                            (got_lines, got_cut),
                            want,
                            "screen={screen:?} cols={cols} rows={rows} pan={pan}"
                        );
                    }
                }
            }
        }
    }

    /// [MEASURED] `StyledCell::width` — the trait `crop_styled`/`render_styled`
    /// actually call through `Viewport::crop` — answers 2 for a wide cell, 0
    /// for its empty continuation, 1 otherwise. See steps/023.
    #[test]
    fn styled_cell_width_reflects_wide_and_continuation() {
        let normal = StyledCell {
            text: "a".into(),
            ..Default::default()
        };
        let wide = StyledCell {
            text: "안".into(),
            wide: true,
            ..Default::default()
        };
        let continuation = StyledCell {
            text: String::new(),
            ..Default::default()
        };
        assert_eq!(normal.width(), 1);
        assert_eq!(wide.width(), 2);
        assert_eq!(continuation.width(), 0);
    }

    /// [MEASURED] A cut right after a wide cell used to pop only its
    /// zero-width continuation, pushing `→` one column past the pane's
    /// width. Exercises `crop_styled`, what the live pane calls. See steps/023.
    #[test]
    fn a_cut_after_a_wide_cell_never_overflows_the_pane_width() {
        fn plain(text: &str) -> StyledCell {
            StyledCell {
                text: text.into(),
                ..Default::default()
            }
        }
        let wide = StyledCell {
            text: "안".into(),
            wide: true,
            ..Default::default()
        };
        let continuation = plain("");
        // "ab안" is exactly 4 columns (1+1+2); "x" after it forces the cut
        // right where the wide cell's continuation is the naive last entry.
        let row = vec![plain("a"), plain("b"), wide, continuation, plain("x")];
        let cols = 4;

        let (lines, cut) = crop_styled(&[row], cols, 1, 0);
        assert!(
            cut,
            "there is more content than fits — this must be marked cut"
        );
        assert!(
            visible_width(&lines[0]) <= cols as usize,
            "row {:?} claims {} columns, more than the pane's {cols}",
            lines[0],
            visible_width(&lines[0])
        );
        assert!(
            lines[0].ends_with('→'),
            "a cut row must show it was cut: {:?}",
            lines[0]
        );
    }

    /// [MEASURED] A wide (CJK) session name used to overflow `list_row`'s own
    /// column budget — `fit` counted `char`s, not display width. Exercises
    /// `list_row`, what `render_styled` actually calls. See steps/025.
    #[test]
    fn list_row_with_a_wide_session_name_still_fits_its_column_budget() {
        let ui = ui(vec![row("안녕하세요", true, false)]);
        let width = 20;
        let line = list_row(&ui, 0, width);
        assert_eq!(
            visible_width(&line),
            width as usize,
            "a wide-named session must still occupy exactly {width} columns: {line:?}"
        );
    }

    /// Two style groups must emit exactly two style-change points, not one
    /// per cell — and a trailing reset only because the row used colour.
    #[test]
    fn render_styled_row_only_emits_sgr_on_a_style_change() {
        let red = StyledCell {
            text: "r".into(),
            fg: Color::Idx(1),
            ..Default::default()
        };
        let blue = StyledCell {
            text: "b".into(),
            fg: Color::Idx(4),
            ..Default::default()
        };
        let row = [
            StyledCell {
                text: "a".into(),
                ..red.clone()
            },
            StyledCell {
                text: "b".into(),
                ..red.clone()
            },
            StyledCell {
                text: "c".into(),
                ..red.clone()
            },
            StyledCell {
                text: "d".into(),
                ..blue.clone()
            },
            StyledCell {
                text: "e".into(),
                ..blue.clone()
            },
            StyledCell {
                text: "f".into(),
                ..blue.clone()
            },
        ];
        let out = render_styled_row(&row);
        assert_eq!(
            out.matches("\x1b[0m").count(),
            3,
            "one reset before each of the 2 style changes, plus the trailing \
             reset — not one per cell: {out:?}"
        );
        assert!(
            out.ends_with("\x1b[0m"),
            "row used colour, so it must reset at the end: {out:?}"
        );

        let plain_row = [
            StyledCell {
                text: "x".into(),
                ..Default::default()
            },
            StyledCell {
                text: "y".into(),
                ..Default::default()
            },
        ];
        let out = render_styled_row(&plain_row);
        assert_eq!(out, "xy", "no style used, no SGR at all");
    }

    /// The flicker steps/026 fixes: a full erase repaints every cell on every
    /// frame, which is what a real terminal shows as flashing. Row-local
    /// clears (`\x1b[K`) replace it; nothing here erases the whole screen.
    #[test]
    fn render_styled_never_erases_the_whole_screen() {
        let ui = ui(vec![row("claude", true, false)]);
        let cells = vec![vec![StyledCell::default(); 10]; 5];
        let out = render_styled(&ui, &cells, "default", 80, 10);
        assert!(
            !out.contains("\x1b[2J"),
            "a full erase is exactly the flicker being fixed: {out:?}"
        );
        assert!(out.contains("\x1b[K"), "rows are cleared locally instead");
    }

    /// A terminal that supports synchronized output never paints a
    /// half-written frame — see steps/026.
    #[test]
    fn render_styled_wraps_the_frame_in_synchronized_output() {
        let ui = ui(vec![row("claude", true, false)]);
        let cells = vec![vec![StyledCell::default(); 10]; 5];
        let out = render_styled(&ui, &cells, "default", 80, 10);
        assert!(out.starts_with("\x1b[?2026h"), "begin sync: {out:?}");
        assert!(out.ends_with("\x1b[?2026l"), "end sync: {out:?}");
    }

    #[test]
    fn the_crop_anchors_bottom_left_and_admits_what_it_cut() {
        let screen = "one\ntwo\nthree\nfourfourfour";
        let (lines, cut) = crop(screen, 4, 2, 0);
        assert_eq!(lines, vec!["thr→", "fou→"], "every cut row says it was cut");
        assert!(cut, "and the title band is told too");

        let (lines, _) = crop("exactfit\nshort", 8, 2, 0);
        assert_eq!(
            lines,
            vec!["exactfit", "short"],
            "a row that fits is untouched"
        );

        let (lines, cut) = crop("abcdefgh", 4, 1, 4);
        assert_eq!(lines, vec!["efgh"], "h/l pans into what was cut");
        assert!(!cut, "nothing is beyond the panned window");
    }

    #[test]
    fn a_cut_row_is_never_silent() {
        assert_eq!(fit("abcdef", 4), "abc→");
        assert_eq!(fit("ab", 4), "ab  ");
    }

    #[test]
    fn the_frame_says_what_it_is_showing() {
        let mut ui = ui(vec![row("claude", true, false), row("busy", true, true)]);
        ui.notice = None;
        let frame = render(&ui, "hello", "default", 120, 10);
        assert!(frame.contains("remuda · default"));
        assert!(frame.contains("▸ claude"), "the cursor is on the first row");
        assert!(frame.contains('⚑'), "and the busy one is flagged");
        assert!(frame.contains("⏎ enter"), "the footer teaches the keys");
        assert!(frame.contains('│'), "and the border is the quiet one");
    }

    /// The preview column of each row — where a ride puts the same content.
    fn preview_rows(frame: &str, list_w: usize) -> Vec<String> {
        frame
            .split("\x1b[")
            .filter_map(|chunk| chunk.split_once(";1H"))
            .map(|(_, row)| row.chars().skip(list_w + 1).collect())
            .collect()
    }

    #[test]
    fn the_preview_starts_at_the_sessions_own_first_row() {
        let ui = ui(vec![row("claude", true, false)]);
        let (list_w, _) = layout(120, 80);
        let frame = render(&ui, "first line\nsecond line", "default", 120, 10);
        let rows = preview_rows(&frame, list_w as usize);
        assert_eq!(
            rows[0].trim_end(),
            "first line",
            "nothing sits above the screen, so entering does not shift it"
        );
        assert_eq!(rows[1].trim_end(), "second line");
        assert!(
            !frame.contains("80×24"),
            "and no size: it is the size at creation, false the moment you resize"
        );
    }

    /// The pane is where a session will live for its whole life, and nothing
    /// can resize a pty afterwards — so sizing it to the whole terminal would
    /// crop it permanently against a list that is now never going away.
    #[test]
    fn a_session_started_here_is_sized_to_the_pane_not_the_terminal() {
        let empty = ui(vec![]);
        let first = pane_size(&empty, 160, 40);
        assert_eq!((first.cols(), first.rows()), (119, 39));

        // And once it exists, the layout it caused fits it exactly.
        let mut herd = ui(vec![row("sh", true, false)]);
        herd.sessions[0].size = first;
        let (_, preview_w) = layout(160, widest(&herd));
        assert_eq!(preview_w, first.cols(), "the second frame must not crop it");
    }

    #[test]
    fn a_pane_below_the_floor_is_raised_rather_than_dropping_keystrokes() {
        let size = pane_size(&ui(vec![]), 80, 24);
        assert_eq!((size.cols(), size.rows()), (80, 24), "Size::new's floor");
    }

    #[test]
    fn a_squeezed_list_drops_fields_rather_than_being_cut() {
        // 80-wide terminal, 80-wide sessions: the list floors at 16, and a row
        // that simply truncated would lose live/dead — the one field it is for.
        let ui = ui(vec![row("claude", true, false)]);
        let (list_w, _) = layout(80, 80);
        assert_eq!(list_w, 16);
        assert!(
            !list_row(&ui, 0, list_w).contains('→'),
            "it fits, by dropping"
        );
        assert!(
            list_row(&ui, 0, 40).contains("live"),
            "and keeps it when there is room"
        );
    }

    /// `idle` never resets on typing, so it read as uptime, not liveness —
    /// and was the list's only per-second repaint source. See steps/026.
    #[test]
    fn the_list_row_carries_no_seconds_counter() {
        let ui = ui(vec![row("claude", true, false)]);
        let line = list_row(&ui, 0, 40);
        assert!(
            !line.contains('s'),
            "no seconds suffix even at full width: {line:?}"
        );
    }

    #[test]
    fn a_cropped_preview_says_so_in_the_footer() {
        let mut ui = ui(vec![row("wide", true, false)]);
        ui.sessions[0].size = Size::new(200, 50);
        let frame = render(&ui, &"x".repeat(300), "default", 100, 10);
        assert!(
            frame.contains("showing"),
            "a crop that reads as absence is the bug"
        );
        assert!(frame.contains("h/l pans"));
    }

    #[test]
    fn an_empty_herd_says_so_and_says_what_to_do_about_it() {
        let ui = ui(vec![]);
        let frame = render(&ui, "", "default", 80, 10);
        assert!(frame.contains("the herd is empty."));
        assert!(
            frame.contains("press n to start a session."),
            "an empty screen that only states a fact leaves a new user stuck"
        );
        assert!(
            !frame.contains("start: "),
            "and no prompt was opened on the user's behalf"
        );
        assert!(
            frame.contains("n new   q quit"),
            "the footer drops what a herdless screen cannot do"
        );
    }

    /// The focused pane is drawn, not remembered — the improvement over a
    /// prefix key, whose state exists only in the user's head.
    #[test]
    fn which_pane_has_the_keyboard_is_on_screen_either_way() {
        let mut ui = ui(vec![row("sh", true, false)]);
        let list = render(&ui, "hello", "default", 120, 10);
        ui.on_key(press(KeyCode::Enter));
        let session = render(&ui, "hello", "default", 120, 10);
        assert_ne!(list, session, "the two states must not look alike");
        assert!(session.contains('┃'), "the focused border is heavier");
        assert!(!list.contains('┃'));
        assert!(session.contains("every key goes to the session"));
        assert!(session.contains("ctrl-\\ back to the list"));
    }

    fn scratch_socket(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("remuda-tuitest-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        crate::daemon::socket_path_in(&dir, "s")
    }

    /// Starts a real daemon and returns once it actually answers — the same
    /// shape as `native/tests/daemon.rs`'s helper, kept local since a unit
    /// test needs `list`/`capture_styled` themselves, which are private.
    fn daemon_at(path: &std::path::Path) {
        let serving = path.to_path_buf();
        std::thread::spawn(move || {
            let _ = crate::daemon::serve(&serving);
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while crate::ipc::connect(path).is_err() {
            assert!(Instant::now() < deadline, "daemon never bound {path:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// [MEASURED] `list` (the pane's own function, not a stand-in) must say a
    /// transport failure happened, not report an empty herd. See steps/021.
    #[test]
    fn list_reports_a_transport_failure_instead_of_an_empty_herd() {
        let path = scratch_socket("list-no-daemon");
        // Nothing is listening at this address on purpose.
        let result = list(&path);
        assert!(
            result.is_err(),
            "no daemon answered — an empty Ok(vec![]) would read as \
             'no sessions exist', which is not what happened: {result:?}"
        );
    }

    /// [MEASURED] `capture_styled` (the pane's own function) against a real
    /// daemon's real "no such session" error — must surface it, not swallow
    /// it into an empty grid. See steps/021.
    #[test]
    fn capture_styled_reports_the_daemons_own_words_instead_of_an_empty_grid() {
        let path = scratch_socket("capture-no-session");
        daemon_at(&path);

        let result = capture_styled(&path, "no-such-session");
        let err = result.expect_err(
            "the daemon has no session by this name — Ok(vec![]) here is the \
             exact defect: a blank pane where a real error was available",
        );
        assert!(
            err.contains("no such session"),
            "the fix must surface what the daemon actually said, not a made-up \
             message: {err:?}"
        );
    }

    /// [MEASURED] `skip_list=true` really skips the relist, and
    /// `skip_list=false` — all a `Start`/`Kill` wake ever uses — still sees
    /// a herd change. See steps/022.
    #[test]
    fn refresh_skips_the_herd_relist_only_when_asked() {
        let path = scratch_socket("refresh-skip-list");
        daemon_at(&path);
        start(&path, "sh", Size::new(80, 24)).unwrap();

        let mut ui = Ui::new(Vec::new(), "/bin/sh", None);
        let mut held = None;
        let mut painted = String::new();
        refresh(&path, "default", &mut ui, &mut held, &mut painted, false).unwrap();
        assert_eq!(ui.sessions.len(), 1, "the first session must be seen");

        // A herd change from elsewhere — exactly what a `Kill` from the list
        // (which sets no `skip_list`) or a second client would produce.
        start(&path, "sh", Size::new(80, 24)).unwrap();

        refresh(&path, "default", &mut ui, &mut held, &mut painted, true).unwrap();
        assert_eq!(
            ui.sessions.len(),
            1,
            "skip_list=true must not relist — this is the optimisation"
        );

        refresh(&path, "default", &mut ui, &mut held, &mut painted, false).unwrap();
        assert_eq!(
            ui.sessions.len(),
            2,
            "skip_list=false must still see the herd change — a Start/Kill \
             wake never sets skip_list, so it can never lose one"
        );
    }
}
