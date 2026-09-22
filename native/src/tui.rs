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
use crate::mcp;
use remuda_core::agent::{Color, Cursor, StyledCell};
use remuda_core::protocol::{expand_runs, Request, Response};
use remuda_core::registry::SessionSummary;
use remuda_core::Size;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

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
    /// `focus_session` succeeded: keyboard and mouse now point at this name.
    /// `run`'s `reconcile_hold` re-attaches if it differs from what's held.
    Focus(String),
    Scroll(i16),
    Copy(String),
    CopySelection(String),
    Paste,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct TextPoint {
    row: usize,
    col: usize,
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct TextSelection {
    session: String,
    start: TextPoint,
    end: TextPoint,
}

pub struct Ui {
    pub sessions: Vec<SessionSummary>,
    pub selected: usize,
    pub pan: u16,
    /// `None` follows the normal content-aware width; `Some` is a user drag.
    pub list_width: Option<u16>,
    list_visible: bool,
    dragging_divider: bool,
    selecting_text: bool,
    text_selection: Option<TextSelection>,
    last_resized: Option<(String, Size)>,
    /// Remuda's own kill ring. It deliberately does not require or alter the
    /// host OS clipboard.
    yank: String,
    scrollback: HashMap<String, usize>,
    pub mode: Mode,
    pub focus: Focus,
    visual: bool,
    preview_cursor: Cursor,
    /// What `n` prefills the prompt with. Held rather than read at the prompt,
    /// so the pure state machine still needs no environment.
    shell: String,
    /// One line of feedback under the list — a refusal, or how the last ride
    /// ended. Cleared by the next keypress that does anything.
    pub notice: Option<String>,
    /// The "*sessions*" buffer's rendered rows (`tools.lua`'s
    /// `remuda._refresh_sessions_buffer`).  Lua owns its presentation; Rust
    /// only adds the per-viewer cursor and maps rows back to sessions.
    sessions_text: Vec<String>,
    /// Rows per session, supplied by the Lua sessions-buffer contract.
    session_rows: usize,
}

impl Ui {
    pub fn new(sessions: Vec<SessionSummary>, shell: &str, notice: Option<String>) -> Self {
        Self {
            sessions,
            selected: 0,
            pan: 0,
            list_width: None,
            list_visible: true,
            dragging_divider: false,
            selecting_text: false,
            text_selection: None,
            last_resized: None,
            yank: String::new(),
            scrollback: HashMap::new(),
            // An empty herd asks rather than acting: it says what to press and
            // waits. It must never spawn a shell on its own — that silent spawn
            // is half of the incident `steps/012` is named after.
            mode: Mode::Browse,
            focus: Focus::List,
            visual: false,
            preview_cursor: Cursor {
                row: 0,
                col: 0,
                visible: false,
            },
            shell: shell.to_string(),
            notice,
            sessions_text: Vec::new(),
            session_rows: 1,
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

    /// A press in the list column switches to that row's session, even if
    /// another one already has focus. A press in the session pane forwards
    /// as a real click to the child instead. See steps/030.
    pub fn on_mouse(&mut self, event: MouseEvent, cols: u16, rows: u16) -> Action {
        if self.mode != Mode::Browse {
            return Action::Nothing;
        }
        let (list_w, preview_w) = ui_layout(self, cols);
        let body = rows.saturating_sub(1);
        // crossterm's column/row are 0-based; the frame's own rows are
        // 1-based (row 1 is the header, row `body+1` is the footer — see
        // `render`/`render_styled`), so both get +1 before comparing.
        let col = event.column + 1;
        let row = event.row + 1;

        if self.dragging_divider {
            match event.kind {
                MouseEventKind::Drag(MouseButton::Left) => self.set_list_width(col, cols),
                MouseEventKind::Up(MouseButton::Left) => self.dragging_divider = false,
                _ => {}
            }
            return Action::Nothing;
        }

        let preview_offset = if self.list_visible { list_w + 1 } else { 0 };
        if matches!(event.kind, MouseEventKind::ScrollUp)
            && event.modifiers.contains(KeyModifiers::SHIFT)
            && col > preview_offset
        {
            return self.wheel_session(
                "wheel-up",
                row.saturating_sub(1),
                col - preview_offset,
                body,
            );
        }
        if matches!(event.kind, MouseEventKind::ScrollDown)
            && event.modifiers.contains(KeyModifiers::SHIFT)
            && col > preview_offset
        {
            return self.wheel_session(
                "wheel-down",
                row.saturating_sub(1),
                col - preview_offset,
                body,
            );
        }
        if matches!(event.kind, MouseEventKind::ScrollUp) && col > preview_offset {
            return Action::Scroll(3);
        }
        if matches!(event.kind, MouseEventKind::ScrollDown) && col > preview_offset {
            return Action::Scroll(-3);
        }

        if self.list_visible && col <= list_w {
            return self.click_list_row(event.kind, row, body);
        }
        // `col == list_w + 1` is the divider itself — between the two panes,
        // part of neither. Anything past it is the session pane.
        if self.list_visible && col == list_w + 1 {
            match event.kind {
                MouseEventKind::Down(MouseButton::Left) => self.dragging_divider = true,
                _ => {}
            }
            return Action::Nothing;
        }
        if (self.visual || event.modifiers.contains(KeyModifiers::SHIFT))
            && matches!(
                event.kind,
                MouseEventKind::Down(MouseButton::Left)
                    | MouseEventKind::Drag(MouseButton::Left)
                    | MouseEventKind::Up(MouseButton::Left)
            )
        {
            return self.select_session_text(event.kind, row, col - preview_offset, body);
        }
        self.click_session_pane(event.kind, row, col - preview_offset, body, preview_w)
    }

    fn select_session_text(
        &mut self,
        kind: MouseEventKind,
        pane_row: u16,
        pane_col: u16,
        body: u16,
    ) -> Action {
        let Some(session) = self.selected() else {
            return Action::Nothing;
        };
        let session_name = session.name.clone();
        if pane_row < 1 || pane_row > body || pane_col < 1 {
            return Action::Nothing;
        }
        let point = TextPoint {
            row: (session.size.rows() as usize).saturating_sub(body as usize) + pane_row as usize
                - 1,
            col: self.pan as usize + pane_col as usize - 1,
        };
        match kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.selecting_text = true;
                self.text_selection = Some(TextSelection {
                    session: session_name,
                    start: point,
                    end: point,
                });
            }
            MouseEventKind::Drag(MouseButton::Left) if self.selecting_text => {
                if let Some(selection) = &mut self.text_selection {
                    selection.end = point;
                }
            }
            MouseEventKind::Up(MouseButton::Left) if self.selecting_text => {
                self.selecting_text = false;
                if let Some(selection) = &mut self.text_selection {
                    selection.end = point;
                }
            }
            _ => {}
        }
        Action::Nothing
    }

    fn click_list_row(&mut self, kind: MouseEventKind, row: u16, body: u16) -> Action {
        if !matches!(kind, MouseEventKind::Down(MouseButton::Left)) {
            return Action::Nothing;
        }
        if row < 2 || row > body {
            return Action::Nothing;
        }
        let index = ((row - 2) as usize / self.session_rows) + list_viewport(self, body);
        if index >= self.sessions.len() {
            return Action::Nothing;
        }
        self.selected = index;
        self.pan = 0;
        self.focus_session()
    }

    /// A left click on the session pane, forwarded as a real SGR mouse
    /// report in the child's own pty-cell coordinates — same encoder the
    /// Lua `click` verb uses. `pane_row`/`pane_col` are 1-based, pane-local.
    fn click_session_pane(
        &mut self,
        kind: MouseEventKind,
        pane_row: u16,
        pane_col: u16,
        body: u16,
        preview_w: u16,
    ) -> Action {
        if self.focus != Focus::Session {
            return Action::Nothing;
        }
        if !matches!(kind, MouseEventKind::Down(MouseButton::Left)) {
            return Action::Nothing;
        }
        if pane_row < 1 || pane_row > body || pane_col < 1 || pane_col > preview_w {
            return Action::Nothing;
        }
        let Some(session) = self.selected() else {
            return Action::Nothing;
        };
        // The pane is bottom-anchored (`Viewport::bottom_anchored`); the
        // current session dimensions from the herd place the click.
        let row_offset = (session.size.rows() as usize).saturating_sub(body as usize);
        let child_row = row_offset + pane_row as usize;
        let child_col = self.pan as usize + pane_col as usize;
        // Only reachable if the outer terminal grew taller/the pan scrolled
        // further than this session's pty extends.
        if child_row > session.size.rows() as usize || child_col > session.size.cols() as usize {
            return Action::Nothing;
        }
        match remuda_core::keys::mouse("left", child_col as u16, child_row as u16) {
            Some(bytes) => Action::Type(bytes),
            None => Action::Nothing,
        }
    }

    fn wheel_session(&self, button: &str, pane_row: u16, pane_col: u16, body: u16) -> Action {
        if self.focus != Focus::Session || pane_row < 1 || pane_row > body || pane_col == 0 {
            return Action::Nothing;
        }
        let Some(session) = self.selected() else {
            return Action::Nothing;
        };
        let child_row =
            (session.size.rows() as usize).saturating_sub(body as usize) + pane_row as usize;
        let child_col = self.pan as usize + pane_col as usize;
        remuda_core::keys::mouse(button, child_col as u16, child_row as u16)
            .map_or(Action::Nothing, Action::Type)
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
        if self.visual {
            match key.code {
                KeyCode::Esc => {
                    self.visual = false;
                    self.selecting_text = false;
                    self.text_selection = None;
                    return Action::Nothing;
                }
                KeyCode::Char('y') => {
                    self.visual = false;
                    self.selecting_text = false;
                    return self.copy_action();
                }
                KeyCode::Char('h') | KeyCode::Left => self.move_visual(-1, 0),
                KeyCode::Char('j') | KeyCode::Down => self.move_visual(0, 1),
                KeyCode::Char('k') | KeyCode::Up => self.move_visual(0, -1),
                KeyCode::Char('l') | KeyCode::Right => self.move_visual(1, 0),
                _ => {}
            }
            return Action::Nothing;
        }
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
                self.list_width = None;
                Action::Nothing
            }
            KeyCode::Char('l') => {
                self.list_visible = !self.list_visible;
                Action::Nothing
            }
            KeyCode::Char('n') => {
                self.mode = Mode::Prompt(self.shell.clone());
                self.notice = None;
                Action::Nothing
            }
            KeyCode::Char('x') => self.kill_selected(),
            KeyCode::Char('y') => self.copy_action(),
            KeyCode::Char('v') => {
                self.visual = true;
                self.selecting_text = false;
                if let Some(name) = self.selected().map(|s| s.name.clone()) {
                    let point = TextPoint {
                        row: self.preview_cursor.row as usize,
                        col: self.preview_cursor.col as usize,
                    };
                    self.text_selection = Some(TextSelection {
                        session: name,
                        start: point,
                        end: point,
                    });
                }
                Action::Nothing
            }
            KeyCode::Char('p') => Action::Paste,
            KeyCode::Esc if self.visual => {
                self.visual = false;
                self.selecting_text = false;
                self.text_selection = None;
                Action::Nothing
            }
            KeyCode::Enter | KeyCode::Char('i') => self.focus_session(),
            _ => Action::Nothing,
        }
    }

    fn copy_action(&self) -> Action {
        let Some(name) = self.selected().map(|s| s.name.clone()) else {
            return Action::Nothing;
        };
        if self
            .text_selection
            .as_ref()
            .is_some_and(|selection| selection.session == name)
        {
            Action::CopySelection(name)
        } else {
            Action::Copy(name)
        }
    }

    fn move_visual(&mut self, dc: i32, dr: i32) {
        let dimensions = self
            .text_selection
            .as_ref()
            .and_then(|selection| self.sessions.iter().find(|s| s.name == selection.session))
            .map(|session| (session.size.rows() as usize, session.size.cols() as usize));
        let Some(selection) = &mut self.text_selection else {
            return;
        };
        let Some((rows, cols)) = dimensions else {
            return;
        };
        let max_row = rows.saturating_sub(1);
        let max_col = cols.saturating_sub(1);
        selection.end.row = selection
            .end
            .row
            .saturating_add_signed(dr as isize)
            .min(max_row);
        selection.end.col = selection
            .end
            .col
            .saturating_add_signed(dc as isize)
            .min(max_col);
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
        let name = session.name.clone();
        self.notice = None;
        self.focus = Focus::Session;
        Action::Focus(name)
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

    fn set_list_width(&mut self, width: u16, cols: u16) {
        let usable = cols.saturating_sub(1);
        self.list_width = Some(width.clamp(16, usable.saturating_sub(16)));
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

fn ui_layout(ui: &Ui, term_cols: u16) -> (u16, u16) {
    if !ui.list_visible {
        return (0, term_cols);
    }
    let usable = term_cols.saturating_sub(1);
    let automatic = layout(term_cols, widest(ui)).0;
    let list = ui
        .list_width
        .unwrap_or(automatic)
        .clamp(16, usable.saturating_sub(16));
    (list, usable.saturating_sub(list))
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

    /// A session-space `(row, col)` cell in panel coordinates, or `None` when
    /// scrolled above the visible rows or panned past the visible columns.
    /// Column math mirrors `crop_row`'s own width-folding. See steps/027.
    pub fn map_cursor<C: Cell>(
        &self,
        source: &[Vec<C>],
        row: usize,
        col: usize,
    ) -> Option<(u16, u16)> {
        if row < self.row_offset {
            return None;
        }
        let panel_row = row - self.row_offset;
        if panel_row >= self.height as usize {
            return None;
        }
        let source_row = source.get(row)?;
        let target: u32 = source_row[..col.min(source_row.len())]
            .iter()
            .map(|c| u32::from(c.width()))
            .sum();
        let offset = u32::from(self.col_offset);
        let panel_col = target.checked_sub(offset)?;
        if panel_col >= u32::from(self.width) {
            return None;
        }
        Some((panel_row as u16, panel_col as u16))
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
        let mut chars = text.chars();
        let mut styled = false;
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                styled = true;
                out.push(c);
                for escape in chars.by_ref() {
                    out.push(escape);
                    if escape == 'm' {
                        break;
                    }
                }
                continue;
            }
            let w = c.width().unwrap_or(1);
            if used + w > width.saturating_sub(1) {
                break;
            }
            out.push(c);
            used += w;
        }
        if styled {
            out.push_str("\x1b[0m");
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
    let (list_w, preview_w) = ui_layout(ui, cols);
    let body = rows.saturating_sub(1);
    // The border, and the only thing on screen that is always saying where the
    // keyboard is pointing. A prefix key's state is invisible; this is not.
    let divider = if ui.list_visible {
        match ui.focus {
            Focus::List => "│",
            Focus::Session => "\x1b[7m┃\x1b[0m",
        }
    } else {
        ""
    };

    let (lines, cut) = crop(screen, preview_w, body, ui.pan);
    let mut out = String::from("\x1b[H\x1b[2J");
    for row in 0..body {
        out.push_str(&format!("\x1b[{};1H", row + 1));
        // The left column has a header; the preview deliberately has none, so
        // its first row is the session's own first row — the same thing a ride
        // shows, at the same place on the screen.
        if ui.list_visible {
            let left = if row == 0 {
                format!("remuda · {server}")
            } else {
                list_row(
                    ui,
                    row as usize - 1 + list_viewport(ui, body) * ui.session_rows,
                    list_w,
                )
            };
            out.push_str(&fit(&left, list_w));
        }
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

/// The session's own cursor on the real screen, 1-based `(row, col)` for
/// `\x1b[{row};{col}H`. `None` hides the caret: the child asked for that, or
/// it's scrolled/panned out of the pane's current crop. See steps/027.
fn locate_cursor(
    cells: &[Vec<StyledCell>],
    cursor: Cursor,
    pan: u16,
    preview_w: u16,
    body: u16,
    list_w: u16,
) -> Option<(u16, u16)> {
    if !cursor.visible {
        return None;
    }
    let viewport = Viewport::bottom_anchored(cells.len(), pan, preview_w, body);
    let (panel_row, panel_col) =
        viewport.map_cursor(cells, cursor.row as usize, cursor.col as usize)?;
    // +1 for the header row above row 0 of the pane; list_w + divider + 1 for
    // the preview pane's own left edge; both again for 1-based addressing.
    Some((
        panel_row + 1,
        if list_w == 0 {
            panel_col + 1
        } else {
            list_w + panel_col + 2
        },
    ))
}

/// Same frame as [`render`], but the preview column carries real colour, is
/// written with row-local clears plus synchronized output instead of a full
/// erase, and the terminal's own caret follows the session's cursor.
pub fn render_styled(
    ui: &Ui,
    cells: &[Vec<StyledCell>],
    cursor: Cursor,
    server: &str,
    cols: u16,
    rows: u16,
) -> String {
    let (list_w, preview_w) = ui_layout(ui, cols);
    let body = rows.saturating_sub(1);
    let divider = if ui.list_visible {
        match ui.focus {
            Focus::List => "│",
            Focus::Session => "\x1b[7m┃\x1b[0m",
        }
    } else {
        ""
    };

    let selected = cells_with_selection(ui, cells);
    let (lines, cut) = crop_styled(&selected, preview_w, body, ui.pan);
    let caret = locate_cursor(cells, cursor, ui.pan, preview_w, body, list_w);
    let mut out = String::from("\x1b[?2026h\x1b[H");
    for row in 0..body {
        out.push_str(&format!("\x1b[{};1H\x1b[K", row + 1));
        if ui.list_visible {
            let left = if row == 0 {
                format!("remuda · {server}")
            } else {
                list_row(
                    ui,
                    row as usize - 1 + list_viewport(ui, body) * ui.session_rows,
                    list_w,
                )
            };
            out.push_str(&fit(&left, list_w));
        }
        out.push_str(divider);
        let line = lines.get(row as usize).map_or("", String::as_str);
        out.push_str(&fit_styled(line, preview_w));
    }

    out.push_str(&format!("\x1b[{};1H\x1b[K", rows));
    out.push_str(&fit(&footer(ui, cut, preview_w), cols));
    // Erase anything a previous, taller frame left below this one — a resize
    // to fewer rows is the only way stale content can survive past here. Must
    // happen BEFORE the caret move below, or `\x1b[J` erases from the caret's
    // new position instead of the footer's.
    out.push_str("\x1b[J");
    match caret {
        Some((row, col)) => out.push_str(&format!("\x1b[{row};{col}H\x1b[?25h")),
        None => out.push_str("\x1b[?25l"),
    }
    out.push_str("\x1b[?2026l");
    out
}

fn cells_with_selection(ui: &Ui, cells: &[Vec<StyledCell>]) -> Vec<Vec<StyledCell>> {
    let mut selected = cells.to_vec();
    let Some(selection) = ui.text_selection.as_ref() else {
        return selected;
    };
    let Some(session) = ui.selected() else {
        return selected;
    };
    if selection.session != session.name {
        return selected;
    }
    let (start, end) =
        if (selection.start.row, selection.start.col) <= (selection.end.row, selection.end.col) {
            (selection.start, selection.end)
        } else {
            (selection.end, selection.start)
        };
    for (row_index, row) in selected.iter_mut().enumerate() {
        if row_index < start.row || row_index > end.row {
            continue;
        }
        let from = if row_index == start.row { start.col } else { 0 };
        let to = if row_index == end.row {
            end.col.saturating_add(1)
        } else {
            row.len()
        };
        let row_len = row.len();
        for cell in row
            .iter_mut()
            .skip(from.min(row_len))
            .take(to.saturating_sub(from).min(row_len.saturating_sub(from)))
        {
            cell.inverse = !cell.inverse;
        }
    }
    selected
}

/// The widest session in the herd, which is what the preview column claims —
/// from the herd rather than the cursor, so the divider does not jump. Zero
/// when there is no herd: nothing to preview, so nothing to reserve.
fn widest(ui: &Ui) -> u16 {
    ui.sessions.iter().map(|s| s.size.cols()).max().unwrap_or(0)
}

/// The first session shown in the list, based on Lua's rows-per-session
/// contract rather than a native presentation policy.
fn list_viewport(ui: &Ui, body: u16) -> usize {
    let visible = body.saturating_sub(1) as usize / ui.session_rows;
    if visible == 0 {
        return 0;
    }
    ui.selected
        .saturating_sub(visible - 1)
        .min(ui.sessions.len().saturating_sub(visible))
}

/// The current size requested for the selected session panel. `Size::new`
/// still floors it at 80×24 so agent TUIs retain a usable compositor.
pub fn pane_size(ui: &Ui, cols: u16, rows: u16) -> Size {
    let (_, preview_w) = ui_layout(ui, cols);
    Size::new(preview_w, rows.saturating_sub(1))
}

/// A row that degrades instead of being cut. When the preview claims most of
/// the terminal the list can floor at 16 columns, and a truncated row loses
/// `live`/`dead` — the one field the whole list exists to show.
// Lua owns the content, spacing, and colour of every list row.  The cursor is
// intentionally still per-viewer state, never buffer content.
fn list_row(ui: &Ui, row: usize, width: u16) -> String {
    if ui.sessions.is_empty() {
        // The empty herd says what it is and what to do about it. It does not
        // open a prompt on its own, and it never starts anything by itself.
        return match row {
            1 => format!("  {}", ui.sessions_text.first().map_or("", String::as_str)),
            3 => format!("  {}", ui.sessions_text.get(1).map_or("", String::as_str)),
            _ => String::new(),
        };
    }
    let session_index = row / ui.session_rows;
    let Some(session) = ui.sessions.get(session_index) else {
        return String::new();
    };
    // A failed or not-yet-completed Lua refresh must not turn a real session
    // into a blank selectable row. The buffer supplies the styled version in
    // normal operation; this fallback keeps the name readable until then.
    let content = ui.sessions_text.get(row).map_or_else(
        || {
            if row % ui.session_rows == 0 {
                session.name.as_str()
            } else {
                ""
            }
        },
        String::as_str,
    );
    if row % ui.session_rows != 0 {
        return fit(content, width);
    }
    let cursor = if session_index == ui.selected {
        "▸"
    } else {
        " "
    };
    format!("{cursor} {}", fit(content, width.saturating_sub(2)))
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
                "↑↓/jk select   ⏎ enter   n new   x kill   l list   showing {preview_w} cols   q quit"
            ),
            None => "↑↓/jk select   ⏎ enter   n new   x kill   l list   q quit".into(),
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
// ponytail: 8 plain params over clippy's 7 rather than a bundling struct
// only `refresh` would ever construct — split out if a 9th ever shows up.
#[allow(clippy::too_many_arguments)]
fn refresh(
    path: &Path,
    server: &str,
    ui: &mut Ui,
    held: &mut Option<(String, Hold)>,
    painted: &mut String,
    shown: &mut Option<ShownTarget>,
    skip_list: bool,
    selection_moved: bool,
) -> std::io::Result<(u16, u16)> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    if !skip_list {
        match list(path) {
            Ok(sessions) => ui.sessions = sessions,
            // Keep the last known herd rather than blanking it: a transport
            // failure is not a report that every session vanished. See steps/021.
            Err(e) => ui.notice = Some(e),
        }
        // Same skip as the relist above, and for the same reason: a
        // `Type`-forced wake (fast-typing tick) needs none of this, so
        // paying for it there would be the exact per-keystroke IPC cost
        // steps/017/022 exist to avoid.
        let (list_w, _) = ui_layout(ui, cols);
        match sessions_buffer_lines(path, list_w, ui.selected) {
            Ok((session_rows, lines)) => {
                ui.session_rows = session_rows;
                ui.sessions_text = lines;
            }
            // Same fallback as the relist: keep whatever was last drawn
            // rather than blanking the tail column on a transport hiccup.
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

    // Hidden-at-origin is the safe default: nothing selected, or a failed
    // capture, means there is no cursor to trust — showing one anyway would
    // paint a caret the daemon never reported. See steps/027.
    let hidden = Cursor {
        row: 0,
        col: 0,
        visible: false,
    };
    // A Type-forced wake can never change the selection (`session_key` never
    // touches `self.selected`, and the only actions that do are never
    // `Type` — see the RED test this fixes), so re-syncing the window there
    // would be a per-keystroke Eval for an answer that can't have changed.
    if !skip_list {
        let selected_name = ui.selected().map(|s| s.name.clone());
        // `selection_moved` is `true` only for a real Up/Down keypress that
        // actually moved `ui.selected` (see `run()`) — never a session
        // first appearing in the list, `clamp()`, or `follow_focus()`.
        match window_shown_session(path, selected_name.as_deref(), selection_moved) {
            Ok(target) => *shown = target,
            Err(e) => {
                ui.notice = Some(e);
                *shown = None;
            }
        }
    }
    if let Some(ShownTarget::Session(name)) = shown.as_ref() {
        let target = pane_size(ui, cols, rows);
        if ui.last_resized.as_ref() != Some(&(name.clone(), target)) {
            match resize(path, name, target) {
                Ok(()) => ui.last_resized = Some((name.clone(), target)),
                Err(e) => ui.notice = Some(format!("{name}: {e}")),
            }
        }
    } else {
        ui.last_resized = None;
    }
    let (cells, _wrapped, cursor) = match shown.as_ref() {
        Some(ShownTarget::Session(name)) => {
            match capture_styled(path, name, *ui.scrollback.get(name).unwrap_or(&0)) {
                Ok(result) => result,
                Err(e) => {
                    ui.notice = Some(format!("{name}: {e}"));
                    (Vec::new(), Vec::new(), hidden)
                }
            }
        }
        Some(ShownTarget::Buffer(name)) => match capture_buffer(path, name) {
            Ok((cells, cursor)) => (cells, Vec::new(), cursor),
            Err(e) => {
                ui.notice = Some(format!("{name}: {e}"));
                (Vec::new(), Vec::new(), hidden)
            }
        },
        None => (Vec::new(), Vec::new(), hidden),
    };
    ui.preview_cursor = cursor;
    let frame = render_styled(ui, &cells, cursor, server, cols, rows);
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

/// Enables SGR mouse reporting on construction, disables it on drop — for
/// the whole `run` now, not just while the list has focus (steps/028 toggled
/// this off on attach; steps/030 explains why that changed).
struct MouseCapture;

impl MouseCapture {
    fn enable() -> std::io::Result<Self> {
        crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)?;
        Ok(Self)
    }
}

impl Drop for MouseCapture {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    }
}

/// Draw the herd until the user quits. One screen for the whole run: focus
/// moves between the panes, and the terminal is never handed over, so the
/// alternate screen is entered exactly once. `notice` is what stderr cannot reach.
pub fn run(path: &Path, server: &str, notice: Option<String>) -> std::io::Result<()> {
    let _terminal = RawMode::enable()?;
    // Scoped to the herd screen, not `RawMode` itself: `attach` uses `RawMode`
    // too, forwards every raw byte it reads, and has no list to click — a
    // session reached directly by `remuda attach` has no list to switch to,
    // so there is nothing for a click there to do. See steps/028 and steps/030.
    let _mouse = MouseCapture::enable()?;
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
    // What the window last reported showing — refreshed only on a
    // non-skip_list wake, and reused as-is on a Type-forced one.
    let mut shown: Option<ShownTarget> = None;
    let mut last_refresh = Instant::now();
    let mut force_refresh = true;
    // Set only by an `Action::Type` below, consumed by the very next refresh,
    // then always cleared — never carried into a tick-driven refresh.
    let mut skip_list = false;
    // Set only by a real Up/Down keypress that moved `ui.selected`, consumed
    // by the very next refresh, then cleared — never by a session appearing
    // in the list, `clamp()`, `follow_focus()`, or a mouse click (out of scope).
    let mut selection_moved = false;
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
            (cols, rows) = refresh(
                path,
                server,
                &mut ui,
                &mut held,
                &mut painted,
                &mut shown,
                skip_list,
                selection_moved,
            )?;
            skip_list = false;
            selection_moved = false;
        }

        if !crossterm::event::poll(tick)? {
            continue;
        }
        // Reset before every read: a stale `true` from a prior iteration
        // must never leak into an unrelated tick.
        selection_moved = false;
        let action = match crossterm::event::read()? {
            // Windows delivers Release as well, and acting on both double-fires.
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let before = ui.selected;
                let action = ui.on_key(key);
                selection_moved = ui.selected != before;
                action
            }
            Event::Mouse(m) => ui.on_mouse(m, cols, rows),
            Event::Resize(_, _) => {
                force_refresh = true;
                continue;
            }
            _ => continue,
        };
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
            Action::Focus(name) => reconcile_hold(path, &mut ui, &mut held, &name),
            Action::Scroll(delta) => {
                if let Some(session) = ui.selected() {
                    let offset = ui.scrollback.entry(session.name.clone()).or_default();
                    *offset = if delta >= 0 {
                        offset.saturating_add(delta as usize)
                    } else {
                        offset.saturating_sub((-delta) as usize)
                    };
                }
            }
            Action::Copy(name) => match capture_styled(path, &name, 0) {
                Ok((cells, wrapped, _)) => {
                    ui.yank = all_screen_text(&cells, &wrapped);
                    ui.notice = Some(format!(
                        "copied {} bytes; p pastes into the selected session",
                        ui.yank.len()
                    ));
                }
                Err(e) => ui.notice = Some(format!("{name}: {e}")),
            },
            Action::CopySelection(name) => {
                let offset = *ui.scrollback.get(&name).unwrap_or(&0);
                match capture_styled(path, &name, offset) {
                    Ok((cells, wrapped, _)) => {
                        ui.yank = ui.text_selection.as_ref().map_or_else(
                            || all_screen_text(&cells, &wrapped),
                            |selection| selected_screen_text(&cells, &wrapped, selection),
                        );
                        ui.notice = Some(format!(
                            "copied {} bytes; p pastes into the selected session",
                            ui.yank.len()
                        ));
                    }
                    Err(e) => ui.notice = Some(format!("{name}: {e}")),
                }
            }
            Action::Paste => {
                if ui.yank.is_empty() {
                    ui.notice = Some("kill ring is empty".into());
                } else if let Some((name, hold)) = &held {
                    if let Err(e) = hold.keys(ui.yank.as_bytes()) {
                        ui.notice = Some(format!("{name}: {e}"));
                    }
                } else if let Some(name) = ui.selected().map(|session| session.name.clone()) {
                    match client::request(
                        path,
                        &Request::Send {
                            name: name.clone(),
                            bytes: ui.yank.as_bytes().to_vec(),
                        },
                    ) {
                        Ok(Response::Ok) => {}
                        Ok(Response::Error(reason)) => ui.notice = Some(reason),
                        other => ui.notice = Some(format!("{other:?}")),
                    }
                }
            }
        }
        // Not `painted.clear()`: the frame-vs-`painted` compare in `refresh`
        // already skips the write when a key changed nothing visible — see
        // steps/026. Only the immediate re-check is forced here.
        force_refresh = true;
    }
}

/// [`Action::Focus`]`(name)` just fired. Already held: no-op. A different
/// name: drop the stale hold (its `Drop` is the detach) and take the new one
/// immediately — `refresh` must never be the one to notice. See steps/030.
fn reconcile_hold(path: &Path, ui: &mut Ui, held: &mut Option<(String, Hold)>, name: &str) {
    if held.as_ref().is_some_and(|(held, _)| held == name) {
        return;
    }
    *held = None;
    *held = take(path, ui);
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

/// The "*sessions*" buffer's content, refreshed at WIDTH and fetched in one
/// `Eval` round trip (`tools.lua`'s `remuda._refresh_sessions_buffer`, then
/// `remuda.buffer.new("*sessions*"):get()`).
fn sessions_buffer_lines(
    path: &Path,
    width: u16,
    selected: usize,
) -> Result<(usize, Vec<String>), String> {
    let code = format!(
        "remuda._refresh_sessions_buffer({width}, {selected}); return remuda.buffer.new('*sessions*'):get()"
    );
    match client::request(path, &Request::Eval { code, name: None }) {
        Ok(Response::Value(text)) => parse_sessions_buffer(&text),
        Ok(Response::Error(reason)) => Err(reason),
        other => Err(format!("{other:?}")),
    }
}

/// Decode Lua's private sessions-buffer header. The body stays ordinary
/// newline-separated styled text; only the row grouping crosses the boundary.
fn parse_sessions_buffer(text: &str) -> Result<(usize, Vec<String>), String> {
    let mut lines = text.split('\n');
    let rows = lines
        .next()
        .and_then(|line| line.strip_prefix('\x1e'))
        .and_then(|rows| rows.parse::<usize>().ok())
        .filter(|rows| *rows > 0)
        .ok_or_else(|| "invalid sessions-buffer layout contract".to_string())?;
    Ok((rows, lines.map(str::to_string).collect()))
}

/// What the window shows — a real session, a script's own buffer, or
/// nothing — parsed from `remuda._sync_window_shown`'s return string.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ShownTarget {
    Session(String),
    Buffer(String),
}

/// Reconciles the window with NAME, the session Rust wants to auto-follow.
/// SELECTION_CHANGED is the only thing allowed to reclaim the window from a
/// buffer a script explicitly showed — a bare tick must leave one alone.
fn window_shown_session(
    path: &Path,
    name: Option<&str>,
    selection_changed: bool,
) -> Result<Option<ShownTarget>, String> {
    let target = name.map_or_else(|| "nil".to_string(), mcp::lua_string);
    let code = format!("return remuda._sync_window_shown({target}, {selection_changed})");
    match client::request(path, &Request::Eval { code, name: None }) {
        Ok(Response::Value(shown)) => Ok(parse_shown_target(&shown)),
        Ok(Response::Error(reason)) => Err(reason),
        other => Err(format!("{other:?}")),
    }
}

/// `remuda._sync_window_shown`'s string, unwrapped: `"nil"`, `"session:<name>"`,
/// or `"buffer:<name>"` — an unrecognized shape falls back to `None`.
fn parse_shown_target(text: &str) -> Option<ShownTarget> {
    text.strip_prefix("session:")
        .map(|name| ShownTarget::Session(name.to_string()))
        .or_else(|| {
            text.strip_prefix("buffer:")
                .map(|name| ShownTarget::Buffer(name.to_string()))
        })
}

/// Styled counterpart of the (now unused) plain `capture` — see steps/020,
/// 021. The wire carries runs, expanded back to cells here — see steps/022.
/// The cursor rides the same round trip — see steps/027.
fn capture_styled(
    path: &Path,
    name: &str,
    scrollback: usize,
) -> Result<(Vec<Vec<StyledCell>>, Vec<bool>, Cursor), String> {
    match client::request(
        path,
        &Request::CaptureStyled {
            name: name.to_string(),
            scrollback,
        },
    ) {
        Ok(Response::StyledScreen {
            rows,
            wrapped,
            cursor,
        }) => Ok((
            rows.iter().map(|row| expand_runs(row)).collect(),
            wrapped,
            cursor,
        )),
        Ok(Response::Error(reason)) => Err(reason),
        other => Err(format!("{other:?}")),
    }
}

fn all_screen_text(cells: &[Vec<StyledCell>], wrapped: &[bool]) -> String {
    cells
        .iter()
        .enumerate()
        .map(|(row_index, row)| {
            let text = row
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>()
                .trim_end()
                .to_string();
            (row_index, text)
        })
        .fold(String::new(), |mut text, (row_index, row)| {
            text.push_str(&row);
            if !wrapped.get(row_index).copied().unwrap_or(false) {
                text.push('\n');
            }
            text
        })
        .trim_end_matches('\n')
        .to_string()
}

fn selected_screen_text(
    cells: &[Vec<StyledCell>],
    wrapped: &[bool],
    selection: &TextSelection,
) -> String {
    let (start, end) =
        if (selection.start.row, selection.start.col) <= (selection.end.row, selection.end.col) {
            (selection.start, selection.end)
        } else {
            (selection.end, selection.start)
        };
    (start.row..=end.row)
        .filter_map(|row_index| cells.get(row_index).map(|row| (row_index, row)))
        .map(|(row_index, row)| {
            let from = if row_index == start.row { start.col } else { 0 };
            let to = if row_index == end.row {
                end.col.saturating_add(1)
            } else {
                row.len()
            };
            let text = row
                .iter()
                .skip(from.min(row.len()))
                .take(to.saturating_sub(from).min(row.len().saturating_sub(from)))
                .map(|cell| cell.text.as_str())
                .collect::<String>()
                .trim_end()
                .to_string();
            (row_index, text)
        })
        .fold(String::new(), |mut text, (row_index, row)| {
            text.push_str(&row);
            if row_index != end.row && !wrapped.get(row_index).copied().unwrap_or(false) {
                text.push('\n');
            }
            text
        })
}

/// A shown buffer's counterpart to `capture_styled` — same `Eval`/`Value`
/// round trip `sessions_buffer_lines` already uses, not a new wire path.
/// Plain text, one default-styled cell per char; no cursor, always hidden.
fn capture_buffer(path: &Path, name: &str) -> Result<(Vec<Vec<StyledCell>>, Cursor), String> {
    let code = format!("return remuda.buffer.new({}):get()", mcp::lua_string(name));
    let text = match client::request(path, &Request::Eval { code, name: None }) {
        Ok(Response::Value(text)) => text,
        Ok(Response::Error(reason)) => return Err(reason),
        other => return Err(format!("{other:?}")),
    };
    let hidden = Cursor {
        row: 0,
        col: 0,
        visible: false,
    };
    let rows = text
        .split('\n')
        .map(|line| {
            line.chars()
                .map(|ch| StyledCell {
                    text: ch.to_string(),
                    ..Default::default()
                })
                .collect()
        })
        .collect();
    Ok((rows, hidden))
}

/// A session started here begins at its panel size; later panel changes resize
/// the PTY through the same path.
fn start(path: &Path, command: &str, size: Size) -> Result<(), String> {
    let request = Request::New {
        name: None,
        command: command.split_whitespace().map(str::to_string).collect(),
        size,
        cwd: None,
        env: None,
    };
    match client::request(path, &request) {
        Ok(Response::Value(_)) => Ok(()),
        Ok(Response::Error(reason)) => Err(reason),
        other => Err(format!("{other:?}")),
    }
}

fn resize(path: &Path, name: &str, size: Size) -> Result<(), String> {
    match client::request(
        path,
        &Request::Resize {
            name: name.to_string(),
            size,
        },
    ) {
        Ok(Response::Ok) => Ok(()),
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
        let mut ui = Ui::new(rows, "/bin/sh", None);
        // Unit fixtures use the same row contract the Lua renderer publishes.
        ui.session_rows = 3;
        ui
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
    fn selected_session_stays_visible_when_the_list_exceeds_a_short_terminal() {
        let mut ui = ui((0..12)
            .map(|index| row(&format!("session-{index}"), true, false))
            .collect());
        for _ in 0..11 {
            ui.on_key(press(KeyCode::Down));
        }

        let frame = render(&ui, "", "test", 80, 24);
        assert!(
            frame.contains("\x1b[20;1H▸ session-11"),
            "the selected final session must be rendered in the 24-row viewport: {frame:?}"
        );
        assert!(
            !frame.contains("session-0"),
            "the list must have advanced rather than rendering only its initial rows"
        );
    }

    #[test]
    fn enter_points_the_keyboard_at_the_selected_session() {
        let mut ui = ui(vec![row("a", true, false), row("b", true, false)]);
        ui.on_key(press(KeyCode::Char('j')));
        assert_eq!(ui.on_key(press(KeyCode::Enter)), Action::Focus("b".into()));
        assert_eq!(ui.focus, Focus::Session);
        assert_eq!(ui.selected().unwrap().name, "b");
        // The list stays on screen: nothing about entering removes a session.
        assert_eq!(ui.sessions.len(), 2);
    }

    fn click(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// steps/028: a click is the same `Action` `↓ ↓ ⏎` already produces.
    #[test]
    fn a_click_on_a_list_row_selects_and_enters_it_like_arrow_plus_enter() {
        let mut ui = ui(vec![row("a", true, false), row("b", true, false)]);
        assert_eq!(ui.on_mouse(click(5, 4), 80, 24), Action::Focus("b".into()));
        assert_eq!(ui.selected, 1, "clicked the second row");
        assert_eq!(ui.focus, Focus::Session, "a click enters, same as Enter");
    }

    /// The header (row 0) and anything below the herd's actual rows are both
    /// outside the list — a click there must not panic or move selection.
    #[test]
    fn a_click_outside_any_list_row_is_a_no_op() {
        let mut ui = ui(vec![row("a", true, false), row("b", true, false)]);
        assert_eq!(
            ui.on_mouse(click(5, 0), 80, 24),
            Action::Nothing,
            "the header row"
        );
        assert_eq!(ui.selected, 0, "unmoved by the header click");
        assert_eq!(ui.focus, Focus::List, "unmoved by the header click");
        // Screen row 8 (0-based 7) is past "b" — the herd has 6 rows.
        assert_eq!(
            ui.on_mouse(click(5, 7), 80, 24),
            Action::Nothing,
            "past the last row"
        );
        assert_eq!(ui.selected, 0, "unmoved by the out-of-range click");
    }

    /// Only a press selects. A release or a drag reaching here (e.g. the drag
    /// tail of a click that started off the list) must be inert.
    #[test]
    fn a_release_or_drag_alone_never_selects() {
        let mut ui = ui(vec![row("a", true, false), row("b", true, false)]);
        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 5,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };
        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 5,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(ui.on_mouse(up, 80, 24), Action::Nothing);
        assert_eq!(ui.selected, 0, "a release did not select");
        assert_eq!(ui.on_mouse(drag, 80, 24), Action::Nothing);
        assert_eq!(ui.selected, 0, "a drag did not select");
    }

    /// steps/030: a click on a *different* list row switches straight to it
    /// even while another session is already attached — the one-way door
    /// #028 left. See steps/030 for the full before/after.
    #[test]
    fn a_click_on_a_different_list_row_switches_focus_even_while_attached() {
        let mut ui = ui(vec![row("a", true, false), row("b", true, false)]);
        assert_eq!(ui.on_key(press(KeyCode::Enter)), Action::Focus("a".into()));
        assert_eq!(ui.focus, Focus::Session);
        assert_eq!(
            ui.on_mouse(click(5, 4), 80, 24),
            Action::Focus("b".into()),
            "row 2 (0-based) is \"b\" — clicking it must switch, not be ignored"
        );
        assert_eq!(ui.focus, Focus::Session, "still attached, now to \"b\"");
        assert_eq!(ui.selected, 1, "the click's row did override it");
    }

    /// The ⒝ half of steps/030: a click inside the session pane forwards as
    /// a real SGR mouse report at the child's OWN coordinates — screen (20,
    /// 5) lands at child (3, 6), not (20, 5). See steps/030 for the math.
    #[test]
    fn a_click_inside_the_session_pane_is_forwarded_to_the_child_as_a_click() {
        let mut ui = ui(vec![row("a", true, false)]);
        assert_eq!(ui.on_key(press(KeyCode::Enter)), Action::Focus("a".into()));
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 19, // screen col 20, 1-based
            row: 4,     // screen row 5, 1-based
            modifiers: KeyModifiers::NONE,
        };
        let expected = remuda_core::keys::mouse("left", 3, 6).unwrap();
        assert_eq!(
            ui.on_mouse(event, 80, 24),
            Action::Type(expected),
            "the click must reach the child at its OWN (3, 6), not remuda's screen (20, 5)"
        );
    }

    /// The divider column is part of neither pane — a click there selects
    /// nothing and forwards nothing.
    #[test]
    fn a_click_on_the_divider_is_a_no_op() {
        let mut ui = ui(vec![row("a", true, false)]);
        ui.on_key(press(KeyCode::Enter));
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 16, // screen col 17 = list_w(16) + 1: the divider
            row: 4,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(ui.on_mouse(event, 80, 24), Action::Nothing);
    }

    #[test]
    fn dragging_the_divider_sets_a_clamped_list_width() {
        let mut ui = ui(vec![row("a", true, false)]);
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 39,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };
        ui.on_mouse(down, 120, 24);
        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 54,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };
        ui.on_mouse(drag, 120, 24);
        assert_eq!(ui.list_width, Some(55));
        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 54,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };
        ui.on_mouse(up, 120, 24);
        assert!(!ui.dragging_divider);
    }

    /// Nothing is attached, so a click landing where the pane *would* be has
    /// nothing to forward to — this is what keeps a stray click from ever
    /// reaching a child that was never asked to receive it.
    #[test]
    fn a_click_in_the_pane_region_with_nothing_attached_is_a_no_op() {
        let mut ui = ui(vec![row("a", true, false)]);
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 19,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(ui.on_mouse(event, 80, 24), Action::Nothing);
        assert_eq!(ui.focus, Focus::List, "unmoved — nothing was attached");
    }

    /// The wheel remains out of the child's input stream; it controls
    /// Remuda's retained terminal history instead.
    #[test]
    fn a_scroll_wheel_while_attached_moves_remuda_scrollback_not_the_child() {
        let mut ui = ui(vec![row("a", true, false)]);
        ui.on_key(press(KeyCode::Enter));
        let wheel = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 19,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(ui.on_mouse(wheel, 80, 24), Action::Scroll(-3));
    }

    #[test]
    fn shift_wheel_is_forwarded_to_the_child_tui() {
        let mut ui = ui(vec![row("a", true, false)]);
        ui.on_key(press(KeyCode::Enter));
        let wheel = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 19,
            row: 4,
            modifiers: KeyModifiers::SHIFT,
        };
        assert_eq!(
            ui.on_mouse(wheel, 80, 24),
            Action::Type(remuda_core::keys::mouse("wheel-down", 3, 5).unwrap())
        );
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

    #[test]
    fn a_session_uses_spaced_name_and_state_rows() {
        let mut ui = ui(vec![row("monocle", true, false)]);
        ui.sessions_text = vec![
            "\x1b[1;36mmonocle\x1b[0m".into(),
            "  \x1b[32mlive\x1b[0m  \x1b[2mclaude · opus · CTX 12k/200k 6%\x1b[0m".into(),
            String::new(),
        ];
        assert!(list_row(&ui, 0, 40).contains("monocle"));
        assert!(list_row(&ui, 0, 40).contains("\x1b[1;36mmonocle\x1b[0m"));
        assert!(list_row(&ui, 1, 40).contains("CTX 12k/200k 6%"));
        assert!(list_row(&ui, 1, 40).contains("\x1b[32mlive\x1b[0m"));
        assert_eq!(list_row(&ui, 2, 40).trim(), "");
        assert_eq!(
            ui.click_list_row(MouseEventKind::Down(MouseButton::Left), 3, 23),
            Action::Focus("monocle".into())
        );
    }

    #[test]
    fn sessions_buffer_contract_leaves_entry_height_to_lua() {
        let (rows, lines) = parse_sessions_buffer("\x1e3\nname\nstate\n\nnext\nstate\n").unwrap();
        assert_eq!(rows, 3);
        assert_eq!(lines, vec!["name", "state", "", "next", "state", ""]);
        assert!(parse_sessions_buffer("name\nstate").is_err());
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
        let out = render_styled(&ui, &cells, hidden_cursor(), "default", 80, 10);
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
        let out = render_styled(&ui, &cells, hidden_cursor(), "default", 80, 10);
        assert!(out.starts_with("\x1b[?2026h"), "begin sync: {out:?}");
        assert!(out.ends_with("\x1b[?2026l"), "end sync: {out:?}");
    }

    /// The regression oracle for the "*sessions*"-buffer migration, captured
    /// before it touched anything (two names of different lengths, one
    /// attached, to exercise `list_row`'s column alignment).
    // If this ever needs editing to pass, the migration changed
    // `render_styled`'s own output, not just its internals — stop and
    // report rather than updating the literal.
    #[test]
    fn render_styled_of_the_session_list_is_byte_identical_before_and_after_the_buffer_migration() {
        let mut ui = ui(vec![row("alpha", true, false), row("bravo", true, true)]);
        // What a real refresh() would have fetched from the "*sessions*"
        // buffer at this scenario's list width (16, per `layout(80, 80)`):
        // unattached is a bare space, attached is the flag — width 16 is
        // under the 22-column threshold `tools.lua` uses for the live/dead
        // word, so neither row shows it. This is `render_styled`'s only
        // input that no longer comes from `ui.sessions` directly.
        ui.sessions_text = vec![
            "claude".into(),
            "live".into(),
            String::new(),
            "busy".into(),
            "live  ⚑".into(),
            String::new(),
        ];
        let cells = vec![text_row(10); 23];
        let out = render_styled(&ui, &cells, hidden_cursor(), "default", 80, 24);
        assert_eq!(
            out,
            "\x1b[?2026h\x1b[H\
             \x1b[1;1H\x1b[Kremuda · default│xxxxxxxxxx                                                     \
             \x1b[2;1H\x1b[K▸ alpha         │xxxxxxxxxx                                                     \
             \x1b[3;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[4;1H\x1b[K  bravo         │xxxxxxxxxx                                                     \
             \x1b[5;1H\x1b[K  ⚑             │xxxxxxxxxx                                                     \
             \x1b[6;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[7;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[8;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[9;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[10;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[11;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[12;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[13;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[14;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[15;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[16;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[17;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[18;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[19;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[20;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[21;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[22;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[23;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[24;1H\x1b[K↑↓ select   ⏎ enter   n new   x kill   q quit                                   \
             \x1b[J\x1b[?25l\x1b[?2026l",
            "byte-identical oracle for the non-empty session list, captured \
             before the buffer migration"
        );
    }

    /// Same oracle, empty herd — a distinct code path in `list_row` (the two
    /// fixed help lines), so it needs its own captured literal.
    #[test]
    fn render_styled_of_the_empty_session_list_is_byte_identical_before_and_after_the_buffer_migration(
    ) {
        let mut ui = ui(vec![]);
        // What a real refresh() would have fetched for an empty herd: the
        // two fixed lines `tools.lua`'s `remuda._refresh_sessions_buffer`
        // returns, now content rather than a Rust string literal.
        ui.sessions_text = vec![
            "the herd is empty.".into(),
            "press n to start a session.".into(),
        ];
        let cells: Vec<Vec<StyledCell>> = vec![];
        let out = render_styled(&ui, &cells, hidden_cursor(), "default", 80, 24);
        assert_eq!(
            out,
            "\x1b[?2026h\x1b[H\
             \x1b[1;1H\x1b[Kremuda · default                        │                                       \
             \x1b[2;1H\x1b[K                                        │                                       \
             \x1b[3;1H\x1b[K  the herd is empty.                    │                                       \
             \x1b[4;1H\x1b[K                                        │                                       \
             \x1b[5;1H\x1b[K  press n to start a session.           │                                       \
             \x1b[6;1H\x1b[K                                        │                                       \
             \x1b[7;1H\x1b[K                                        │                                       \
             \x1b[8;1H\x1b[K                                        │                                       \
             \x1b[9;1H\x1b[K                                        │                                       \
             \x1b[10;1H\x1b[K                                        │                                       \
             \x1b[11;1H\x1b[K                                        │                                       \
             \x1b[12;1H\x1b[K                                        │                                       \
             \x1b[13;1H\x1b[K                                        │                                       \
             \x1b[14;1H\x1b[K                                        │                                       \
             \x1b[15;1H\x1b[K                                        │                                       \
             \x1b[16;1H\x1b[K                                        │                                       \
             \x1b[17;1H\x1b[K                                        │                                       \
             \x1b[18;1H\x1b[K                                        │                                       \
             \x1b[19;1H\x1b[K                                        │                                       \
             \x1b[20;1H\x1b[K                                        │                                       \
             \x1b[21;1H\x1b[K                                        │                                       \
             \x1b[22;1H\x1b[K                                        │                                       \
             \x1b[23;1H\x1b[K                                        │                                       \
             \x1b[24;1H\x1b[Kn new   q quit                                                                  \
             \x1b[J\x1b[?25l\x1b[?2026l",
            "byte-identical oracle for the empty session list, captured \
             before the buffer migration"
        );
    }

    /// Creates a real session the same way `native/tests/daemon.rs` does —
    /// `sh`, no pty features exercised, just something alive to list.
    fn new_session(path: &std::path::Path, name: &str) {
        let response = client::request(
            path,
            &Request::New {
                name: Some(name.to_string()),
                command: vec!["sh".into()],
                size: Size::new(80, 24),
                cwd: None,
                env: None,
            },
        )
        .expect("new");
        assert_eq!(
            response,
            Response::Value(name.to_string()),
            "New answers with the name it gave the session"
        );
    }

    /// The e2e counterpart to the two literal oracles above: same expected
    /// bytes, but fed from a real daemon instead of hand-built fixtures.
    // Proves the half of the migration the hand-fed oracle can't: that
    // Lua's flag glyph and 22-column threshold actually produce " " / "⚑"
    // for this scenario. Measured ~13ms: one daemon thread, two `sh`
    // children, one held Attach connection, one Eval round trip.
    #[test]
    fn render_styled_of_the_session_list_is_byte_identical_when_fed_by_a_real_daemon() {
        let path = scratch_socket("e2e-sessions-oracle");
        daemon_at(&path);

        new_session(&path, "alpha");
        new_session(&path, "bravo");
        let _held = client::hold(&path, "bravo").expect("attach bravo for real");

        let sessions = match client::request(&path, &Request::List).expect("list") {
            Response::Sessions(s) => s,
            other => panic!("unexpected: {other:?}"),
        };
        let mut ui = Ui::new(sessions, "/bin/sh", None);
        let (list_w, _) = layout(80, widest(&ui));
        let lines = sessions_buffer_lines(&path, list_w, 0)
            .expect("refresh sessions buffer")
            .1;
        assert_eq!(
            lines,
            vec![" ".to_string(), "⚑".to_string()],
            "real Lua output for this scenario must match the hand-fed oracle's input"
        );
        ui.sessions_text = lines;

        let cells = vec![text_row(10); 23];
        let out = render_styled(&ui, &cells, hidden_cursor(), "default", 80, 24);
        assert_eq!(
            out,
            "\x1b[?2026h\x1b[H\
             \x1b[1;1H\x1b[Kremuda · default│xxxxxxxxxx                                                     \
             \x1b[2;1H\x1b[K▸ alpha         │xxxxxxxxxx                                                     \
             \x1b[3;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[4;1H\x1b[K  bravo         │xxxxxxxxxx                                                     \
             \x1b[5;1H\x1b[K  ⚑             │xxxxxxxxxx                                                     \
             \x1b[6;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[7;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[8;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[9;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[10;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[11;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[12;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[13;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[14;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[15;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[16;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[17;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[18;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[19;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[20;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[21;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[22;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[23;1H\x1b[K                │xxxxxxxxxx                                                     \
             \x1b[24;1H\x1b[K↑↓ select   ⏎ enter   n new   x kill   q quit                                   \
             \x1b[J\x1b[?25l\x1b[?2026l",
            "byte-identical oracle for the non-empty session list, fed for real"
        );
    }

    /// Empty-herd counterpart, fed by a real (empty) daemon registry.
    // No sessions created at all, so `remuda.ls()` itself sees a real empty
    // herd rather than an empty `Vec` constructed by hand. Measured ~13ms:
    // one daemon thread, one Eval round trip, no children.
    #[test]
    fn render_styled_of_the_empty_session_list_is_byte_identical_when_fed_by_a_real_daemon() {
        let path = scratch_socket("e2e-empty-oracle");
        daemon_at(&path);

        let lines = sessions_buffer_lines(&path, 16, 0)
            .expect("refresh sessions buffer")
            .1;
        assert_eq!(
            lines,
            vec![
                "the herd is empty.".to_string(),
                "press n to start a session.".to_string()
            ],
            "real Lua output for the empty herd must match the hand-fed oracle's input"
        );

        let mut ui = ui(vec![]);
        ui.sessions_text = lines;
        let cells: Vec<Vec<StyledCell>> = vec![];
        let out = render_styled(&ui, &cells, hidden_cursor(), "default", 80, 24);
        assert_eq!(
            out,
            "\x1b[?2026h\x1b[H\
             \x1b[1;1H\x1b[Kremuda · default                        │                                       \
             \x1b[2;1H\x1b[K                                        │                                       \
             \x1b[3;1H\x1b[K  the herd is empty.                    │                                       \
             \x1b[4;1H\x1b[K                                        │                                       \
             \x1b[5;1H\x1b[K  press n to start a session.           │                                       \
             \x1b[6;1H\x1b[K                                        │                                       \
             \x1b[7;1H\x1b[K                                        │                                       \
             \x1b[8;1H\x1b[K                                        │                                       \
             \x1b[9;1H\x1b[K                                        │                                       \
             \x1b[10;1H\x1b[K                                        │                                       \
             \x1b[11;1H\x1b[K                                        │                                       \
             \x1b[12;1H\x1b[K                                        │                                       \
             \x1b[13;1H\x1b[K                                        │                                       \
             \x1b[14;1H\x1b[K                                        │                                       \
             \x1b[15;1H\x1b[K                                        │                                       \
             \x1b[16;1H\x1b[K                                        │                                       \
             \x1b[17;1H\x1b[K                                        │                                       \
             \x1b[18;1H\x1b[K                                        │                                       \
             \x1b[19;1H\x1b[K                                        │                                       \
             \x1b[20;1H\x1b[K                                        │                                       \
             \x1b[21;1H\x1b[K                                        │                                       \
             \x1b[22;1H\x1b[K                                        │                                       \
             \x1b[23;1H\x1b[K                                        │                                       \
             \x1b[24;1H\x1b[Kn new   q quit                                                                  \
             \x1b[J\x1b[?25l\x1b[?2026l",
            "byte-identical oracle for the empty session list, fed for real"
        );
    }

    /// Pins the live/dead word's 22-column threshold against real Lua, not
    /// just the flag glyph the two oracles above already exercise.
    // `alpha` unattached, `bravo` attached, both alive. Measured ~13ms: one
    // daemon thread, two `sh` children, one held Attach, two Eval round
    // trips (one per width).
    #[test]
    fn the_live_dead_word_is_pinned_against_real_lua_at_the_22_column_threshold() {
        let path = scratch_socket("e2e-live-dead-threshold");
        daemon_at(&path);

        new_session(&path, "alpha");
        new_session(&path, "bravo");
        let _held = client::hold(&path, "bravo").expect("attach bravo for real");

        let wide = sessions_buffer_lines(&path, 22, 0)
            .expect("refresh at width 22")
            .1;
        assert_eq!(
            wide,
            vec!["live  ".to_string(), "live ⚑".to_string()],
            "at width >= 22, tools.lua shows the live/dead word before the flag"
        );

        let narrow = sessions_buffer_lines(&path, 21, 0)
            .expect("refresh at width 21")
            .1;
        assert_eq!(
            narrow,
            vec![" ".to_string(), "⚑".to_string()],
            "below width 22, tools.lua drops the word and shows only the flag"
        );
    }

    fn hidden_cursor() -> Cursor {
        Cursor {
            row: 0,
            col: 0,
            visible: false,
        }
    }

    /// A row of real cells (not `StyledCell::default`, whose empty text
    /// claims 0 columns), so cumulative width advances one column per cell.
    fn text_row(width: usize) -> Vec<StyledCell> {
        (0..width)
            .map(|_| StyledCell {
                text: "x".into(),
                ..Default::default()
            })
            .collect()
    }

    /// With a real 16-wide list column and header row in front of it, a
    /// cursor at session (2, 3) must land at absolute (3, 21) — not at
    /// (2, 3) itself. See steps/027 and the PR's negative control.
    #[test]
    fn a_focused_cursor_lands_at_its_absolute_screen_position_not_at_origin() {
        let ui = ui(vec![row("claude", true, false)]);
        let cells = vec![text_row(10); 5];
        let cursor = Cursor {
            row: 2,
            col: 3,
            visible: true,
        };
        let out = render_styled(&ui, &cells, cursor, "default", 80, 10);
        assert!(
            out.ends_with("\x1b[3;21H\x1b[?25h\x1b[?2026l"),
            "row 2 col 3 in session space, with a 16-col list + divider in \
             front and a header row above, must land at screen (3, 21), \
             positioned before the end-sync marker: {out:?}"
        );
    }

    /// The child's own DECTCEM hide must win: steps/027's axis 4. A visible
    /// caret painted over an app that deliberately hid its own (a spinner, a
    /// full-screen TUI) is worse than none.
    #[test]
    fn a_hidden_cursor_never_gets_a_show_sequence() {
        let ui = ui(vec![row("claude", true, false)]);
        let cells = vec![text_row(10); 5];
        let cursor = Cursor {
            row: 2,
            col: 3,
            visible: false,
        };
        let out = render_styled(&ui, &cells, cursor, "default", 80, 10);
        assert!(
            !out.contains("\x1b[?25h"),
            "the child asked to hide: {out:?}"
        );
        assert!(
            out.ends_with("\x1b[?25l\x1b[?2026l"),
            "an explicit hide, not a silently-omitted one: {out:?}"
        );
    }

    /// A cursor scrolled above the bottom-anchored viewport (more session
    /// rows than the pane has height for) must not paint a caret at some
    /// clamped, wrong row — steps/027's axis 3.
    #[test]
    fn a_cursor_scrolled_out_of_the_viewport_is_hidden_not_clamped() {
        let ui = ui(vec![row("claude", true, false)]);
        // 20 session rows into a 9-row body (rows=10): row_offset = 11, so
        // session row 0 is 11 rows above the visible window.
        let cells = vec![text_row(10); 20];
        let cursor = Cursor {
            row: 0,
            col: 0,
            visible: true,
        };
        let out = render_styled(&ui, &cells, cursor, "default", 80, 10);
        assert!(
            out.ends_with("\x1b[?25l\x1b[?2026l"),
            "row 0 is scrolled off above the visible window: {out:?}"
        );
    }

    /// A cursor panned past the visible columns must not paint a caret
    /// inside the list column or the divider — steps/027's axis 3 and 5.
    #[test]
    fn a_cursor_panned_out_of_view_is_hidden() {
        let mut ui = ui(vec![row("claude", true, false)]);
        ui.pan = 5;
        let cells = vec![text_row(3); 5];
        let cursor = Cursor {
            row: 2,
            col: 1,
            visible: true,
        };
        let out = render_styled(&ui, &cells, cursor, "default", 80, 10);
        assert!(
            out.ends_with("\x1b[?25l\x1b[?2026l"),
            "column 1 is behind the pan of 5, so nothing of it is visible: {out:?}"
        );
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
        ui.sessions_text = vec![
            "claude".into(),
            "live".into(),
            String::new(),
            "busy".into(),
            "live  ⚑".into(),
            String::new(),
        ];
        let frame = render(&ui, "hello", "default", 120, 10);
        assert!(frame.contains("remuda · default"));
        assert!(frame.contains("▸ claude"), "the cursor is on the first row");
        assert!(frame.contains('⚑'), "and the busy one is flagged");
        assert!(frame.contains("⏎ enter"), "the footer teaches the keys");
        assert!(frame.contains('│'), "and the border is the quiet one");
    }

    /// The preview column of each row — where a ride puts the same content.
    fn preview_rows(frame: &str, _list_w: usize) -> Vec<String> {
        fn without_sgr(text: &str) -> String {
            let mut out = String::new();
            let mut chars = text.chars();
            while let Some(c) = chars.next() {
                if c == '\x1b' {
                    for escape in chars.by_ref() {
                        if escape == 'm' {
                            break;
                        }
                    }
                } else {
                    out.push(c);
                }
            }
            out
        }

        frame
            .split('│')
            .skip(1)
            .filter_map(|row| row.split_once("\x1b["))
            .map(|(preview, _)| without_sgr(preview))
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
        // 80-wide terminal, 80-wide sessions: the list floors at 16. At that
        // width `tools.lua`'s own `remuda._refresh_sessions_buffer` omits
        // the live/dead word (width < 22) — simulated here as the tail it
        // would have supplied at each width, since deciding that is no
        // longer list_row's job (see the "*sessions*" buffer migration
        // above list_row's own doc comment). What list_row still owns is
        // degrading the NAME rather than ever truncating the tail it's given.
        let mut ui = ui(vec![row("claude", true, false)]);
        let (list_w, _) = layout(80, 80);
        assert_eq!(list_w, 16);
        ui.sessions_text = vec!["claude".into(), "".into(), String::new()];
        assert!(
            !list_row(&ui, 0, list_w).contains('→'),
            "it fits, by dropping"
        );
        ui.sessions_text = vec!["claude".into(), "live".into(), String::new()];
        assert!(
            list_row(&ui, 1, 40).contains("live"),
            "and keeps it when there is room"
        );
    }

    /// `idle` never resets on typing, so it read as uptime, not liveness —
    /// and was the list's only per-second repaint source. See steps/026.
    #[test]
    fn the_list_row_carries_no_seconds_counter() {
        let mut ui = ui(vec![row("claude", true, false)]);
        ui.sessions_text = vec!["claude".into(), "live".into(), String::new()];
        let line = list_row(&ui, 1, 40);
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
        assert!(frame.contains("l list"));
    }

    #[test]
    fn an_empty_herd_says_so_and_says_what_to_do_about_it() {
        let mut ui = ui(vec![]);
        ui.sessions_text = vec![
            "the herd is empty.".into(),
            "press n to start a session.".into(),
        ];
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

        let result = capture_styled(&path, "no-such-session", 0);
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

    /// The window is real state, not a local variable dressed up: syncing it
    /// to a name and reading it back returns that name.
    // Clearing it (`None`) reads back as `None`, not the literal string
    // "nil" — `Eval`'s own rendering of Lua's `nil` that this function has
    // to unwrap. Measured ~13ms: one daemon thread, two Eval round trips.
    #[test]
    fn window_shown_session_round_trips_a_real_name_and_clears_to_none() {
        let path = scratch_socket("window-shown-round-trip");
        daemon_at(&path);

        let shown = window_shown_session(&path, Some("alpha"), false).expect("show alpha");
        assert_eq!(shown, Some(ShownTarget::Session("alpha".to_string())));

        let cleared = window_shown_session(&path, None, false).expect("clear");
        assert_eq!(
            cleared, None,
            "a nil target must read back as None, not the string \"nil\""
        );
    }

    /// The e2e proof for step ③: the right pane's `cells` come from a real
    /// session, reached through a real window that was actually asked to
    /// show it — not `text_row`, and not `ui.selected()` read directly.
    // `printf` (no shell) makes the screen's content exactly "hello" with no
    // prompt noise. Polls before capturing, so this doesn't race the child.
    // Measured ~20ms: one daemon thread, one `printf` child, up to a few
    // 10ms poll ticks, two Eval/capture round trips.
    #[test]
    fn render_styled_of_the_right_pane_is_fed_by_a_real_window_showing_a_real_session() {
        let path = scratch_socket("window-preview-oracle");
        daemon_at(&path);

        let response = client::request(
            &path,
            &Request::New {
                name: Some("alpha".to_string()),
                command: vec!["printf".into(), "hello".into()],
                size: Size::new(80, 24),
                cwd: None,
                env: None,
            },
        )
        .expect("new");
        assert_eq!(response, Response::Value("alpha".to_string()));

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok((cells, _, _)) = capture_styled(&path, "alpha", 0) {
                let first_five: String = cells
                    .first()
                    .map(|row| row.iter().take(5).map(|c| c.text.as_str()).collect())
                    .unwrap_or_default();
                if first_five == "hello" {
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "\"hello\" never appeared on alpha's real screen"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let shown = window_shown_session(&path, Some("alpha"), false).expect("show alpha");
        assert_eq!(
            shown,
            Some(ShownTarget::Session("alpha".to_string())),
            "the window must show what it was just asked to show"
        );
        let ShownTarget::Session(name) = shown.unwrap() else {
            unreachable!("just asserted it above");
        };
        let (cells, _, cursor) =
            capture_styled(&path, &name, 0).expect("capture through the window's target");

        let mut ui = Ui::new(vec![row("alpha", true, false)], "/bin/sh", None);
        ui.sessions_text = vec![" ".into()];
        // rows=25, not 24: the pty is a real 24-row screen (`Size::MIN_ROWS`
        // floors it there), and `body = rows - 1` is what the bottom-anchored
        // crop keeps — 24 would drop row 0, exactly where "hello" printed.
        let out = render_styled(&ui, &cells, cursor, "default", 80, 25);
        assert_eq!(
            out,
            "\x1b[?2026h\x1b[H\
             \x1b[1;1H\x1b[Kremuda · default│hello                                                         →\
             \x1b[2;1H\x1b[K▸ alpha         │                                                              →\
             \x1b[3;1H\x1b[K                │                                                              →\
             \x1b[4;1H\x1b[K                │                                                              →\
             \x1b[5;1H\x1b[K                │                                                              →\
             \x1b[6;1H\x1b[K                │                                                              →\
             \x1b[7;1H\x1b[K                │                                                              →\
             \x1b[8;1H\x1b[K                │                                                              →\
             \x1b[9;1H\x1b[K                │                                                              →\
             \x1b[10;1H\x1b[K                │                                                              →\
             \x1b[11;1H\x1b[K                │                                                              →\
             \x1b[12;1H\x1b[K                │                                                              →\
             \x1b[13;1H\x1b[K                │                                                              →\
             \x1b[14;1H\x1b[K                │                                                              →\
             \x1b[15;1H\x1b[K                │                                                              →\
             \x1b[16;1H\x1b[K                │                                                              →\
             \x1b[17;1H\x1b[K                │                                                              →\
             \x1b[18;1H\x1b[K                │                                                              →\
             \x1b[19;1H\x1b[K                │                                                              →\
             \x1b[20;1H\x1b[K                │                                                              →\
             \x1b[21;1H\x1b[K                │                                                              →\
             \x1b[22;1H\x1b[K                │                                                              →\
             \x1b[23;1H\x1b[K                │                                                              →\
             \x1b[24;1H\x1b[K                │                                                              →\
             \x1b[25;1H\x1b[K↑↓ select   ⏎ enter   n new   x kill   showing 63 cols — h/l pans   q quit      \
             \x1b[J\x1b[1;23H\x1b[?25h\x1b[?2026l",
            "byte-identical oracle for the right pane, fed through a real window"
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
        let mut shown = None;
        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            false,
            false,
        )
        .unwrap();
        assert_eq!(ui.sessions.len(), 1, "the first session must be seen");

        // A herd change from elsewhere — exactly what a `Kill` from the list
        // (which sets no `skip_list`) or a second client would produce.
        start(&path, "sh", Size::new(80, 24)).unwrap();

        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            true,
            false,
        )
        .unwrap();
        assert_eq!(
            ui.sessions.len(),
            1,
            "skip_list=true must not relist — this is the optimisation"
        );

        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            ui.sessions.len(),
            2,
            "skip_list=false must still see the herd change — a Start/Kill \
             wake never sets skip_list, so it can never lose one"
        );
    }

    /// `request_counts()`'s three fields read back through one `Eval` — the
    /// same shape `native/tests/script.rs`'s sibling helper reads.
    fn request_counts(path: &std::path::Path) -> (u64, u64, u64) {
        let code = "local c = remuda.request_counts(); \
                     return c.list .. ',' .. c.eval .. ',' .. c.capture_styled";
        match client::request(
            path,
            &Request::Eval {
                code: code.to_string(),
                name: None,
            },
        ) {
            Ok(Response::Value(text)) => {
                let parts: Vec<u64> = text.split(',').map(|n| n.parse().unwrap()).collect();
                (parts[0], parts[1], parts[2])
            }
            other => panic!("request_counts failed: {other:?}"),
        }
    }

    /// [MEASURED, RED as of PR #46 / 121a64e9] A `Type`-forced refresh
    /// (`skip_list=true`) must cost exactly one daemon request once the
    /// window is already synced to the selected session. See steps/022.
    // Today it costs two: `window_shown_session`'s `Eval` (tui.rs:952) runs
    // unconditionally, outside the `skip_list` guard, even when nothing changed.
    #[test]
    fn a_type_forced_refresh_with_unchanged_selection_costs_one_daemon_request() {
        let path = scratch_socket("type-forced-request-count");
        daemon_at(&path);
        start(&path, "sh", Size::new(80, 24)).unwrap();

        let mut ui = Ui::new(Vec::new(), "/bin/sh", None);
        let mut held = None;
        let mut painted = String::new();
        let mut shown = None;
        // Steady state first: one relist, one window sync, one capture. The
        // Type-forced refresh below has no selection change to react to.
        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            false,
            false,
        )
        .unwrap();
        assert!(
            ui.selected().is_some(),
            "one session must be selected before measuring"
        );

        let before = request_counts(&path);
        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            true,
            false,
        )
        .unwrap();
        let after = request_counts(&path);

        // `after`'s own read is itself one Eval, counted in `after` but not
        // caused by `refresh()` — subtract it to isolate what refresh() did.
        let list = after.0 - before.0;
        let eval = after.1 - before.1 - 1;
        let capture_styled = after.2 - before.2;
        let total = list + eval + capture_styled;
        assert_eq!(
            total, 1,
            "a Type-forced refresh with an unchanged selection must cost exactly \
             one daemon request (just the capture) — got {total} (list={list}, \
             eval={eval}, capture_styled={capture_styled})"
        );
    }

    /// [MEASURED, RED as of PR #46 / 121a64e9] A tick refresh
    /// (`skip_list=false`) must send exactly one `Request::List`. See steps/031.
    // Today it sends two: the direct `list(path)` call here, plus
    // `remuda.ls()`'s own loopback socket connection inside
    // `_refresh_sessions_buffer`'s `Eval` (`script.rs:110-114`).
    #[test]
    fn a_tick_refresh_sends_exactly_one_request_list() {
        let path = scratch_socket("tick-refresh-list-count");
        daemon_at(&path);
        start(&path, "sh", Size::new(80, 24)).unwrap();

        let mut ui = Ui::new(Vec::new(), "/bin/sh", None);
        let mut held = None;
        let mut painted = String::new();
        let mut shown = None;

        let before = request_counts(&path);
        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            false,
            false,
        )
        .unwrap();
        let after = request_counts(&path);

        let list = after.0 - before.0;
        assert_eq!(
            list, 1,
            "a tick refresh must send exactly one Request::List — got {list}"
        );
    }

    /// [MEASURED] The real bug behind steps/030: which session's pty a real
    /// `Hold` is attached to, not just `Ui` state — proven against a real
    /// daemon and two real held sessions, not a mock. See steps/030.
    #[test]
    fn reconcile_hold_switches_the_real_attach_not_just_ui_state() {
        let path = scratch_socket("reconcile-hold-switch");
        daemon_at(&path);
        start(&path, "sh", Size::new(80, 24)).unwrap();
        start(&path, "sh", Size::new(80, 24)).unwrap();

        let mut ui = Ui::new(Vec::new(), "/bin/sh", None);
        let mut held = None;
        let mut painted = String::new();
        let mut shown = None;
        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            false,
            false,
        )
        .unwrap();
        assert_eq!(ui.sessions.len(), 2, "both sessions must be seen");
        let first = ui.sessions[0].name.clone();
        let second = ui.sessions[1].name.clone();

        ui.selected = 0;
        reconcile_hold(&path, &mut ui, &mut held, &first);
        assert_eq!(
            held.as_ref().map(|(name, _)| name.as_str()),
            Some(first.as_str()),
            "attached to the first session"
        );

        // The regression: selecting a DIFFERENT session while one is already
        // held must drop the stale hold and take the new one.
        ui.selected = 1;
        reconcile_hold(&path, &mut ui, &mut held, &second);
        assert_eq!(
            held.as_ref().map(|(name, _)| name.as_str()),
            Some(second.as_str()),
            "must have switched — a stale hold on the first is the exact bug"
        );

        // Idempotence: calling it again with the SAME name must not
        // needlessly drop and re-take a hold that already matches.
        let before = held.as_ref().map(|(name, _)| name.clone());
        reconcile_hold(&path, &mut ui, &mut held, &second);
        assert_eq!(
            held.as_ref().map(|(name, _)| name.clone()),
            before,
            "already held — must be a no-op, not a needless re-attach"
        );
    }

    /// A small `Eval` helper for the buffer tests below — same wire shape as
    /// `request_counts()`, but for arbitrary Lua rather than the fixed
    /// counter read.
    fn eval(path: &std::path::Path, code: &str) -> String {
        match client::request(
            path,
            &Request::Eval {
                code: code.to_string(),
                name: None,
            },
        ) {
            Ok(Response::Value(text)) => text,
            other => panic!("eval failed: {other:?}"),
        }
    }

    /// Shows a fresh Lua buffer named `scratch`, with TEXT as its content —
    /// the real `Window:show(buf)` path a plugin uses, not a stand-in.
    fn show_scratch_buffer(path: &std::path::Path, text: &str) {
        eval(
            path,
            &format!(
                "remuda.buffer.new('scratch'):set({}); \
                 remuda.window.current():show(remuda.buffer.new('scratch'))",
                mcp::lua_string(text)
            ),
        );
    }

    /// `show(buf)` now survives a refresh: `capture_buffer` renders it,
    /// not `capture_styled` mistaking a buffer's name for a session's.
    #[test]
    fn a_buffer_shown_via_window_renders_its_text_not_a_no_such_session_error() {
        let path = scratch_socket("buffer-shown-renders-text");
        daemon_at(&path);

        show_scratch_buffer(&path, "hello from buffer");

        let mut ui = Ui::new(Vec::new(), "/bin/sh", None);
        let mut held = None;
        let mut painted = String::new();
        let mut shown = None;

        // Nothing selected before or after (no sessions at all), so this is
        // an ordinary tick — no selection change to report.
        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            false,
            false,
        )
        .unwrap();

        assert_eq!(shown, Some(ShownTarget::Buffer("scratch".to_string())));
        assert!(
            painted.contains("hello from buffer"),
            "a shown buffer's own text must reach the screen: {painted:?}"
        );
        assert!(
            !painted.contains("no such session"),
            "a buffer is not a session — capture_styled must not be asked \
             to treat one as one: {painted:?}"
        );
    }

    /// Unchanged buffer content between two refreshes must not repaint —
    /// same real text captured twice, not both calls erroring identically.
    #[test]
    fn a_shown_buffers_unchanged_update_does_not_repaint() {
        let path = scratch_socket("buffer-unchanged-no-repaint");
        daemon_at(&path);

        show_scratch_buffer(&path, "hello from buffer");

        let mut ui = Ui::new(Vec::new(), "/bin/sh", None);
        let mut held = None;
        let mut painted = String::new();
        let mut shown = None;

        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            false,
            false,
        )
        .unwrap();
        assert!(
            painted.contains("hello from buffer"),
            "sanity: the first refresh must have actually captured the \
             buffer's real text, or an unchanged second refresh would prove \
             nothing: {painted:?}"
        );
        let after_first = painted.clone();

        // Content unchanged.
        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            false,
            false,
        )
        .unwrap();

        assert_eq!(
            painted, after_first,
            "no content change happened, so no repaint should have either"
        );
    }

    /// A real content change must repaint, keeping the same no-full-erase /
    /// synchronized-output shape `render_styled_*` requires of every frame.
    #[test]
    fn a_shown_buffers_changed_update_does_repaint_without_full_erase() {
        let path = scratch_socket("buffer-changed-repaints");
        daemon_at(&path);

        show_scratch_buffer(&path, "before");

        let mut ui = Ui::new(Vec::new(), "/bin/sh", None);
        let mut held = None;
        let mut painted = String::new();
        let mut shown = None;

        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            false,
            false,
        )
        .unwrap();
        let before_frame = painted.clone();
        assert!(before_frame.contains("before"), "{before_frame:?}");

        eval(&path, "remuda.buffer.new('scratch'):set('after')");
        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            false,
            false,
        )
        .unwrap();

        assert_ne!(
            painted, before_frame,
            "the buffer's text changed, so the painted frame must too: {painted:?}"
        );
        assert!(
            painted.contains("after") && !painted.contains("before"),
            "the new frame must show the buffer's NEW text: {painted:?}"
        );
        assert!(
            !painted.contains("\x1b[2J"),
            "a repaint must still never be a full erase: {painted:?}"
        );
        assert!(
            painted.starts_with("\x1b[?2026h"),
            "begin sync: {painted:?}"
        );
        assert!(painted.ends_with("\x1b[?2026l"), "end sync: {painted:?}");
    }

    /// Mirrors `a_type_forced_refresh_with_unchanged_selection_costs_one_daemon_request`:
    /// a buffer shown, unchanged, must also cost exactly one request.
    #[test]
    fn a_type_forced_refresh_while_a_buffer_is_shown_costs_one_daemon_request() {
        let path = scratch_socket("buffer-type-forced-request-count");
        daemon_at(&path);

        show_scratch_buffer(&path, "hello from buffer");

        let mut ui = Ui::new(Vec::new(), "/bin/sh", None);
        let mut held = None;
        let mut painted = String::new();
        let mut shown = None;
        // Steady state first, exactly like the session-based test: one
        // ordinary refresh syncs the window to the buffer.
        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            shown,
            Some(ShownTarget::Buffer("scratch".to_string())),
            "steady state must have the buffer shown before measuring"
        );

        let before = request_counts(&path);
        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            true,
            false,
        )
        .unwrap();
        let after = request_counts(&path);

        let list = after.0 - before.0;
        let eval_calls = after.1 - before.1 - 1; // `after`'s own read is one Eval
        let capture_styled = after.2 - before.2;
        let total = list + eval_calls + capture_styled;
        assert_eq!(
            total, 1,
            "a Type-forced refresh with an unchanged buffer target must \
             cost exactly one daemon request — got {total} (list={list}, \
             eval={eval_calls}, capture_styled={capture_styled})"
        );
    }

    /// Arrowing the list must reclaim the window from a shown buffer —
    /// the next refresh shows the newly selected session, not the buffer.
    #[test]
    fn a_selection_change_reclaims_the_window_from_a_shown_buffer() {
        let path = scratch_socket("buffer-selection-change-reclaims");
        daemon_at(&path);
        start(&path, "sh", Size::new(80, 24)).unwrap();
        start(&path, "sh", Size::new(80, 24)).unwrap();

        let mut ui = Ui::new(Vec::new(), "/bin/sh", None);
        let mut held = None;
        let mut painted = String::new();
        let mut shown = None;
        // Steady state: both real sessions seen, the first selected and shown.
        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            false,
            false,
        )
        .unwrap();
        assert_eq!(ui.sessions.len(), 2, "both sessions must be seen");
        let second = ui.sessions[1].name.clone();
        assert_eq!(
            shown,
            Some(ShownTarget::Session(ui.sessions[0].name.clone())),
            "steady state must have the first session shown"
        );

        // A script takes the window over.
        show_scratch_buffer(&path, "hello from buffer");

        // The explicit user act: arrowing down to the other session.
        ui.on_key(press(KeyCode::Down));
        assert_eq!(ui.selected, 1, "the selection must have moved");

        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            false,
            true,
        )
        .unwrap();

        assert_eq!(
            shown,
            Some(ShownTarget::Session(second)),
            "an explicit selection change must reclaim the window from the \
             buffer, showing the newly selected session"
        );
    }

    /// The defect the fix closes: a tick with no user action must NOT
    /// clear a shown buffer — only an explicit selection change may.
    #[test]
    fn a_tick_refresh_keeps_a_shown_buffer() {
        let path = scratch_socket("buffer-tick-refresh-keeps-it");
        daemon_at(&path);

        // No sessions at all, so there is nothing to compete for the window
        // via a default selection either — isolates "a tick fired" from
        // "something was already selected".
        let mut ui = Ui::new(Vec::new(), "/bin/sh", None);
        let mut held = None;
        let mut painted = String::new();
        let mut shown = None;

        // Steady state, nothing shown yet.
        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            false,
            false,
        )
        .unwrap();
        assert_eq!(shown, None, "nothing selected, nothing shown yet");

        // A script takes the window over, out of band from any refresh.
        show_scratch_buffer(&path, "hello from buffer");

        // A tick fires with no user action at all: `run`'s own loop only
        // ever sets `skip_list=true` right after an `Action::Type`, and
        // always clears it straight after — a schedule- or TICK-driven wake
        // is always this shape, and the selection here never moved.
        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            false,
            false,
        )
        .unwrap();

        assert_eq!(
            shown,
            Some(ShownTarget::Buffer("scratch".to_string())),
            "no user action fired — a tick alone must not clear a shown buffer"
        );
    }

    /// A default selection being assigned to a real session (no keypress at
    /// all) must not reclaim a shown buffer — unlike
    /// `a_tick_refresh_keeps_a_shown_buffer`, this uses a REAL session.
    #[test]
    fn a_shown_buffer_survives_the_first_refresh_when_a_default_selection_is_assigned() {
        let path = scratch_socket("buffer-first-refresh-default-selection");
        daemon_at(&path);
        start(&path, "sh", Size::new(80, 24)).unwrap();

        // Show the buffer BEFORE ever calling refresh() — nothing has
        // synced yet, and no `selection_moved` is passed below.
        show_scratch_buffer(&path, "hello from buffer");

        let mut ui = Ui::new(Vec::new(), "/bin/sh", None);
        let mut held = None;
        let mut painted = String::new();
        let mut shown = None;

        // The very first refresh ever: `ui.clamp()` assigns a default
        // selection (None -> Some), with no key event involved at all.
        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            false,
            false,
        )
        .unwrap();

        assert_eq!(
            shown,
            Some(ShownTarget::Buffer("scratch".to_string())),
            "a default selection being assigned is not a user action — the \
             shown buffer must survive it"
        );
    }

    /// Positive control: a real Down keypress that moves `ui.selected`
    /// (NOT the mouse-driven `click_list_row`, out of scope) must reclaim
    /// the window — passed explicitly as `selection_moved` below.
    #[test]
    fn a_key_driven_selection_move_still_reclaims_the_window_from_a_shown_buffer() {
        let path = scratch_socket("buffer-key-driven-selection-reclaims");
        daemon_at(&path);
        start(&path, "sh", Size::new(80, 24)).unwrap();
        start(&path, "sh", Size::new(80, 24)).unwrap();

        let mut ui = Ui::new(Vec::new(), "/bin/sh", None);
        let mut held = None;
        let mut painted = String::new();
        let mut shown = None;
        // Steady state: both real sessions seen, the first selected and shown.
        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            false,
            false,
        )
        .unwrap();
        assert_eq!(ui.sessions.len(), 2, "both sessions must be seen");
        let second = ui.sessions[1].name.clone();

        // A script takes the window over.
        show_scratch_buffer(&path, "hello from buffer");

        // The explicit user act: a real Down keypress, genuinely moving
        // `ui.selected` from 0 to 1.
        ui.on_key(press(KeyCode::Down));
        assert_eq!(ui.selected, 1, "the selection must have moved");

        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            false,
            true,
        )
        .unwrap();

        assert_eq!(
            shown,
            Some(ShownTarget::Session(second)),
            "a real key-driven selection move must reclaim the window from \
             the buffer, showing the newly selected session"
        );
    }

    /// Steady state (already synced once), then a second tick with no key
    /// event: distinct from the "buffer set before the first-ever refresh"
    /// case above — here it's set after a session was already selected.
    #[test]
    fn a_shown_buffer_survives_a_tick_while_a_real_session_is_selected() {
        let path = scratch_socket("buffer-tick-with-real-session-selected");
        daemon_at(&path);
        start(&path, "sh", Size::new(80, 24)).unwrap();

        let mut ui = Ui::new(Vec::new(), "/bin/sh", None);
        let mut held = None;
        let mut painted = String::new();
        let mut shown = None;

        // First, ordinary refresh: a default selection is assigned and
        // synced. Under today's buggy code this may already have reclaimed
        // the window — irrelevant here, since no buffer is shown yet.
        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            false,
            false,
        )
        .unwrap();

        // Now a script takes the window over, out of band from any refresh.
        show_scratch_buffer(&path, "hello from buffer");

        // A second, ordinary tick — no key event, no selection change.
        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            false,
            false,
        )
        .unwrap();

        assert_eq!(
            shown,
            Some(ShownTarget::Buffer("scratch".to_string())),
            "no user action fired while a real session was already \
             selected — the shown buffer must survive the tick"
        );
    }

    /// Sanity companion only — `skip_list=true` never enters the
    /// selection-sync block at all, so this can't isolate the selection
    /// bug; it just confirms the one-request invariant with a session too.
    #[test]
    fn a_type_forced_refresh_with_a_real_session_and_a_shown_buffer_costs_one_daemon_request() {
        let path = scratch_socket("buffer-type-forced-request-count-with-session");
        daemon_at(&path);
        start(&path, "sh", Size::new(80, 24)).unwrap();

        let mut ui = Ui::new(Vec::new(), "/bin/sh", None);
        let mut held = None;
        let mut painted = String::new();
        let mut shown = None;
        // Steady state: the session is synced first...
        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            false,
            false,
        )
        .unwrap();
        // ...then a script takes the window over.
        show_scratch_buffer(&path, "hello from buffer");
        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            shown,
            Some(ShownTarget::Buffer("scratch".to_string())),
            "steady state must have the buffer shown before measuring"
        );

        let before = request_counts(&path);
        refresh(
            &path,
            "default",
            &mut ui,
            &mut held,
            &mut painted,
            &mut shown,
            true,
            false,
        )
        .unwrap();
        let after = request_counts(&path);

        let list = after.0 - before.0;
        let eval_calls = after.1 - before.1 - 1; // `after`'s own read is one Eval
        let capture_styled = after.2 - before.2;
        let total = list + eval_calls + capture_styled;
        assert_eq!(
            total, 1,
            "a Type-forced refresh with an unchanged buffer target and a \
             real session present must still cost exactly one daemon \
             request — got {total} (list={list}, eval={eval_calls}, \
             capture_styled={capture_styled})"
        );
    }
}
