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
            if self.focus == Focus::Session {
                return self.wheel_session(
                    "wheel-up",
                    row.saturating_sub(1),
                    col - preview_offset,
                    body,
                );
            }
            return Action::Scroll(3);
        }
        if matches!(event.kind, MouseEventKind::ScrollDown) && col > preview_offset {
            if self.focus == Focus::Session {
                return self.wheel_session(
                    "wheel-down",
                    row.saturating_sub(1),
                    col - preview_offset,
                    body,
                );
            }
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
#[path = "../tests/tui_unit.rs"]
mod tui_unit;
