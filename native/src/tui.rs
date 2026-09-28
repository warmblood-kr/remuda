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
#[cfg(not(test))]
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

/// How often an idle herd is relisted, recaptured and redrawn — see
/// [`should_refresh`]. A key always forces an immediate refresh regardless.
const TICK: Duration = Duration::from_millis(250);
const DAEMON_FAILURES_BEFORE_GONE: u8 = 3;

/// How often the keyboard is polled while a session has focus — shorter
/// than `TICK` so a keypress is never left waiting to be noticed. Used to
/// also be the redraw cadence; see steps/017 for why that was the bug.
const TICK_TYPING: Duration = Duration::from_millis(40);
const SCROLL_DOWN_SETTLE: Duration = Duration::from_millis(1500);
const MAX_ANCHOR_CAPTURES: usize = 3;

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
    Restart,
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

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct TextPoint {
    row: usize,
    /// Index in the captured wire row. Wide glyphs occupy one cell here;
    /// convert to terminal display columns only when positioning the viewport.
    col: usize,
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct TextSelection {
    session: String,
    start: TextPoint,
    end: TextPoint,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WordClass {
    Blank,
    Keyword,
    Punctuation,
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
    scrollback: HashMap<String, ScrollState>,
    pub mode: Mode,
    pub focus: Focus,
    /// Visual mode (tmux copy-mode-vi): `visual_cursor` moves; the selection
    /// (`text_selection`) exists only once `v`/Space anchors it there.
    visual: bool,
    visual_cursor: TextPoint,
    visual_g_pending: bool,
    visual_screen: Vec<Vec<StyledCell>>,
    visual_wrapped: Vec<bool>,
    preview_width: u16,
    preview_cursor: Cursor,
    /// What `n` prefills the prompt with. Held rather than read at the prompt,
    /// so the pure state machine still needs no environment.
    shell: String,
    /// One line of feedback under the list — a refusal, or how the last ride
    /// ended. Cleared by the next keypress that does anything.
    pub notice: Option<String>,
    /// The daemon endpoint is gone; retain the cached session list for context.
    daemon_gone: Option<String>,
    consecutive_transport_failures: u8,
    /// The "*sessions*" buffer's rendered rows (`tools.lua`'s
    /// `remuda._refresh_sessions_buffer`).  Lua owns its presentation; Rust
    /// only adds the per-viewer cursor and maps rows back to sessions.
    sessions_text: Vec<String>,
    /// Rows per session, supplied by the Lua sessions-buffer contract.
    session_rows: usize,
    /// Visible preview rows, refreshed from the terminal dimensions each frame.
    preview_rows: u16,
}

#[derive(Clone, Copy, Default)]
struct ScrollState {
    offset: usize,
    history_rows: usize,
    history_total: usize,
    /// Total corresponding to `offset`; can lag `history_total` when output
    /// keeps arriving through the bounded capture retries.
    anchor_total: usize,
    last_scroll_down: Option<Instant>,
    scroll_direction: i8,
    recent_up_output: usize,
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
            visual_cursor: TextPoint { row: 0, col: 0 },
            visual_g_pending: false,
            visual_screen: Vec::new(),
            visual_wrapped: Vec::new(),
            preview_width: 80,
            preview_cursor: Cursor {
                row: 0,
                col: 0,
                visible: false,
            },
            shell: shell.to_string(),
            notice,
            daemon_gone: None,
            consecutive_transport_failures: 0,
            sessions_text: Vec::new(),
            session_rows: 1,
            preview_rows: 1,
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
        if self.daemon_gone.is_some() {
            return Action::Nothing;
        }
        if self.mode != Mode::Browse {
            return Action::Nothing;
        }
        let (list_w, preview_w) = ui_layout(self, cols);
        let body = rows.saturating_sub(1);
        // crossterm's column/row are 0-based; frame rows are 1-based, so both
        // get +1 before comparing.
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
            if let MouseEventKind::Down(MouseButton::Left) = event.kind {
                self.dragging_divider = true;
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
            col: cell_index_at_display_col(
                self.visual_screen
                    .get(
                        (session.size.rows() as usize).saturating_sub(body as usize)
                            + pane_row as usize
                            - 1,
                    )
                    .map_or(&[], Vec::as_slice),
                usize::from(self.pan) + pane_col as usize - 1,
            ),
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
        if row < 1 || row > body {
            return Action::Nothing;
        }
        let index = ((row - 1) as usize / self.session_rows) + list_viewport(self, body);
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
        // Click coordinates follow the same content anchor as the visible crop.
        let row_offset = if self.visual_screen.is_empty() {
            (session.size.rows() as usize).saturating_sub(body as usize)
        } else {
            ui_preview_row_offset(self, body)
        };
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
        if !session.mouse_tracking {
            return Action::Nothing;
        }
        let row_offset = if self.visual_screen.is_empty() {
            (session.size.rows() as usize).saturating_sub(body as usize)
        } else {
            ui_preview_row_offset(self, body)
        };
        let child_row = row_offset + pane_row as usize;
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
        let bytes = to_bytes(key);
        if bytes.is_some() {
            if let Some(name) = self.selected().map(|session| session.name.clone()) {
                self.scrollback.entry(name).or_default().offset = 0;
            }
        }
        bytes.map_or(Action::Nothing, Action::Type)
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
        if self.daemon_gone.is_some() {
            return match key.code {
                KeyCode::Char('r') | KeyCode::Char('R') => Action::Restart,
                KeyCode::Char('q') | KeyCode::Char('Q') => Action::Quit,
                _ => Action::Nothing,
            };
        }
        if self.visual {
            if self.visual_g_pending {
                self.visual_g_pending = false;
                if key.code == KeyCode::Char('g') {
                    self.move_visual_to(TextPoint { row: 0, col: 0 });
                    return Action::Nothing;
                }
            }
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.leave_visual(),
                // Yank and leave in one key; with no anchor there is nothing
                // to copy, so it only leaves (tmux's copy-selection-and-cancel).
                KeyCode::Char('y') => {
                    let anchored = self.anchored();
                    self.visual = false;
                    self.selecting_text = false;
                    if anchored {
                        return self.copy_action();
                    }
                    self.text_selection = None;
                }
                KeyCode::Char('v') if self.anchored() => self.text_selection = None,
                KeyCode::Char('v') | KeyCode::Char(' ') => self.anchor_visual(),
                KeyCode::Char('h') | KeyCode::Left => self.move_visual(-1, 0),
                KeyCode::Char('j') | KeyCode::Down => self.move_visual(0, 1),
                KeyCode::Char('k') | KeyCode::Up => self.move_visual(0, -1),
                KeyCode::Char('l') | KeyCode::Right => self.move_visual(1, 0),
                KeyCode::Char('0') => self.move_visual_to(TextPoint {
                    row: self.visual_line_start_row(self.visual_cursor.row),
                    col: 0,
                }),
                KeyCode::Char('$') => self.visual_line_end(),
                KeyCode::Char('^') => self.visual_first_nonblank(),
                KeyCode::Char('w') => self.visual_word_forward(),
                KeyCode::Char('b') => self.visual_word_backward(),
                KeyCode::Char('e') => self.visual_word_end(),
                KeyCode::Char('g') => self.visual_g_pending = true,
                KeyCode::Char('G') => self.visual_bottom(),
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
            KeyCode::PageUp => Action::Scroll(self.preview_page_delta()),
            KeyCode::PageDown => Action::Scroll(-self.preview_page_delta()),
            KeyCode::End => Action::Scroll(i16::MIN),
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
                self.text_selection = None;
                self.visual_cursor = TextPoint {
                    row: self.preview_cursor.row as usize,
                    col: cell_index_at_display_col(
                        self.visual_screen
                            .get(self.preview_cursor.row as usize)
                            .map_or(&[], Vec::as_slice),
                        self.preview_cursor.col as usize,
                    ),
                };
                Action::Nothing
            }
            KeyCode::Char('p') => Action::Paste,
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

    fn preview_page_delta(&self) -> i16 {
        self.preview_rows
            .saturating_sub(1)
            .max(1)
            .min(i16::MAX as u16) as i16
    }

    fn leave_visual(&mut self) {
        self.visual = false;
        self.selecting_text = false;
        self.text_selection = None;
    }

    /// A selection on the shown session exists (anchored by `v`/Space or a drag).
    fn anchored(&self) -> bool {
        let shown = self.selected().map(|s| s.name.as_str());
        self.text_selection
            .as_ref()
            .is_some_and(|selection| Some(selection.session.as_str()) == shown)
    }

    /// Anchor at the cursor — again from the cursor if already anchored, as
    /// tmux's begin-selection does on Space.
    fn anchor_visual(&mut self) {
        if let Some(name) = self.selected().map(|s| s.name.clone()) {
            self.text_selection = Some(TextSelection {
                session: name,
                start: self.visual_cursor,
                end: self.visual_cursor,
            });
        }
    }

    fn move_visual(&mut self, dc: i32, dr: i32) {
        let Some(size) = self.selected().map(|s| s.size) else {
            return;
        };
        let max_row = (size.rows() as usize).saturating_sub(1);
        self.visual_cursor.row = self
            .visual_cursor
            .row
            .saturating_add_signed(dr as isize)
            .min(max_row);
        let max_col = self
            .visual_screen
            .get(self.visual_cursor.row)
            .map_or((size.cols() as usize).saturating_sub(1), |row| {
                row.len().saturating_sub(1)
            });
        self.visual_cursor.col = self
            .visual_cursor
            .col
            .saturating_add_signed(dc as isize)
            .min(max_col);
        self.move_visual_to(self.visual_cursor);
    }

    fn move_visual_to(&mut self, point: TextPoint) {
        let Some(size) = self.selected().map(|s| s.size) else {
            return;
        };
        self.visual_cursor.row = point.row.min((size.rows() as usize).saturating_sub(1));
        let max_col = self
            .visual_screen
            .get(self.visual_cursor.row)
            .map_or((size.cols() as usize).saturating_sub(1), |row| {
                row.len().saturating_sub(1)
            });
        self.visual_cursor.col = point.col.min(max_col);
        self.keep_visual_cursor_visible();
        self.extend_visual(self.visual_cursor);
    }

    fn keep_visual_cursor_visible(&mut self) {
        let Some(row) = self.visual_screen.get(self.visual_cursor.row) else {
            return;
        };
        let col = display_col_for_cell_index(row, self.visual_cursor.col);
        let cursor_width = row
            .get(self.visual_cursor.col)
            .map(|cell| usize::from(cell.width()).max(1))
            .unwrap_or(1);
        let cursor_end = col.saturating_add(cursor_width);
        let total_width: usize = row.iter().map(|cell| usize::from(cell.width())).sum();
        let width = usize::from(self.preview_width.max(1));
        let pan = usize::from(self.pan);
        if col < pan {
            self.pan = col as u16;
        } else {
            let cropped = total_width > pan + width;
            let visible_width = width.saturating_sub(usize::from(cropped));
            if visible_width > 0 && cursor_end > pan + visible_width {
                let mut next_pan = cursor_end
                    .saturating_sub(visible_width)
                    .min(u16::MAX as usize);
                let right_edge = total_width.saturating_sub(width);
                if col >= right_edge {
                    next_pan = next_pan.max(right_edge);
                }
                self.pan = next_pan.min(u16::MAX as usize) as u16;
            }
        }
    }

    fn extend_visual(&mut self, cursor: TextPoint) {
        if let Some(selection) = &mut self.text_selection {
            selection.end = cursor;
        }
    }

    fn visual_line_start_row(&self, row: usize) -> usize {
        let mut start = row.min(self.visual_screen.len().saturating_sub(1));
        while start > 0 && self.visual_wrapped.get(start - 1).copied().unwrap_or(false) {
            start -= 1;
        }
        start
    }

    fn visual_line_end_row(&self, row: usize) -> usize {
        let mut end = row.min(self.visual_screen.len().saturating_sub(1));
        while self.visual_wrapped.get(end).copied().unwrap_or(false)
            && end + 1 < self.visual_screen.len()
        {
            end += 1;
        }
        end
    }

    fn visual_line_end(&mut self) {
        let row = self.visual_line_end_row(self.visual_cursor.row);
        let col = self.visual_screen.get(row).map_or(0, |cells| {
            cells
                .iter()
                .rposition(|cell| cell.width() > 0 && !cell.text.chars().all(char::is_whitespace))
                .unwrap_or(0)
        });
        self.move_visual_to(TextPoint { row, col });
    }

    fn visual_first_nonblank(&mut self) {
        let start = self.visual_line_start_row(self.visual_cursor.row);
        let end = self.visual_line_end_row(self.visual_cursor.row);
        let target = (start..=end).find_map(|row| {
            self.visual_screen.get(row).and_then(|cells| {
                cells
                    .iter()
                    .position(|cell| {
                        cell.width() > 0 && !cell.text.chars().all(char::is_whitespace)
                    })
                    .map(|col| TextPoint { row, col })
            })
        });
        let point = target.unwrap_or(TextPoint { row: start, col: 0 });
        self.move_visual_to(TextPoint {
            row: point.row,
            col: point.col,
        });
    }

    fn visual_words(&self) -> Vec<(TextPoint, WordClass)> {
        let mut words = Vec::new();
        let start = self.visual_line_start_row(self.visual_cursor.row);
        let end = self.visual_line_end_row(self.visual_cursor.row);
        for row in start..=end {
            if let Some(cells) = self.visual_screen.get(row) {
                for (col, cell) in cells.iter().enumerate() {
                    let class = cell.text.chars().next().map_or(WordClass::Blank, |ch| {
                        if ch.is_whitespace() {
                            WordClass::Blank
                        } else if ch.is_alphanumeric() || ch == '_' {
                            WordClass::Keyword
                        } else {
                            WordClass::Punctuation
                        }
                    });
                    words.push((TextPoint { row, col }, class));
                }
            }
        }
        words
    }

    fn visual_word_forward(&mut self) {
        let words = self.visual_words();
        let current = words.iter().rposition(|(at, _)| *at <= self.visual_cursor);
        let mut next = current.unwrap_or(0);
        if let Some(index) = current.filter(|index| words[*index].1 != WordClass::Blank) {
            let class = words[index].1;
            while next < words.len() && words[next].1 == class {
                next += 1;
            }
        }
        while next < words.len() && words[next].1 == WordClass::Blank {
            next += 1;
        }
        let target = words
            .get(next)
            .filter(|(_, class)| *class != WordClass::Blank)
            .map(|(at, _)| *at)
            .or_else(|| {
                words
                    .iter()
                    .rfind(|(_, class)| *class != WordClass::Blank)
                    .map(|(at, _)| *at)
            });
        if let Some(target) = target {
            self.move_visual_to(target);
        }
    }

    fn visual_word_backward(&mut self) {
        let words = self.visual_words();
        let Some(mut index) = words.iter().rposition(|(at, _)| *at < self.visual_cursor) else {
            if let Some((at, _)) = words.iter().find(|(_, class)| *class != WordClass::Blank) {
                self.move_visual_to(*at);
            }
            return;
        };
        while index > 0 && words[index].1 == WordClass::Blank {
            index -= 1;
        }
        let class = words[index].1;
        while index > 0 && words[index - 1].1 == class {
            index -= 1;
        }
        self.move_visual_to(words[index].0);
    }

    fn visual_word_end(&mut self) {
        let words = self.visual_words();
        let here = self.visual_cursor;
        let current = words.iter().rposition(|(at, _)| *at <= here);
        let mut start = current.unwrap_or(0);
        if let Some(index) = current.filter(|index| words[*index].1 != WordClass::Blank) {
            let class = words[index].1;
            let end = (index..words.len())
                .take_while(|i| words[*i].1 == class)
                .last()
                .unwrap_or(index);
            if words[end].0 > here {
                self.move_visual_to(words[end].0);
                return;
            }
            start = end + 1;
        }
        while start < words.len() && words[start].1 == WordClass::Blank {
            start += 1;
        }
        if start < words.len() {
            let class = words[start].1;
            let end = (start..words.len())
                .take_while(|i| words[*i].1 == class)
                .last()
                .unwrap_or(start);
            self.move_visual_to(words[end].0);
        }
    }

    fn visual_bottom(&mut self) {
        let row = self.visual_screen.len().saturating_sub(1);
        self.move_visual_to(TextPoint {
            row,
            col: self.visual_cursor.col,
        });
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
        let max = usable.saturating_sub(16).max(16).min(usable);
        self.list_width = Some(width.clamp(16.min(max), max));
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
    let automatic = content_list_width(ui, usable);
    let max = usable.saturating_sub(16).max(16).min(usable);
    let list = ui.list_width.unwrap_or(automatic).clamp(16.min(max), max);
    (list, usable.saturating_sub(list))
}

/// Fit the automatic list pane to its longest rendered row, leaving a little
/// room at the right edge. `sessions_text` is the Lua-owned rendered content;
/// before its first refresh, session names give us a useful estimate instead.
fn content_list_width(ui: &Ui, usable: u16) -> u16 {
    let longest = ui
        .sessions_text
        .iter()
        .map(|line| visible_width(line))
        .max()
        .unwrap_or_else(|| {
            ui.sessions
                .iter()
                .map(|session| visible_width(&session.name) + 2)
                .max()
                .unwrap_or(0)
        });
    let wanted = longest.saturating_add(2).min(u16::MAX as usize) as u16;
    wanted.clamp(16, 40).min(usable)
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

/// The display column of a wire-cell index. The protocol omits a wide
/// character's continuation, so the index is shorter than the terminal row.
fn display_col_for_cell_index(row: &[StyledCell], index: usize) -> usize {
    row.iter()
        .take(index)
        .map(|cell| usize::from(cell.width()))
        .sum()
}

/// Map a terminal display column back to the one wire cell for its glyph.
/// A column inside a wide glyph resolves to its lead cell.
fn cell_index_at_display_col(row: &[StyledCell], display_col: usize) -> usize {
    let mut col = 0usize;
    for (index, cell) in row.iter().enumerate() {
        let width = usize::from(cell.width());
        if width > 0 && display_col < col + width {
            return index;
        }
        col += width;
    }
    row.len().saturating_sub(1)
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

fn anchored_row_offset(source_rows: usize, height: u16, bottom_row: usize) -> usize {
    let max_start = source_rows.saturating_sub(height as usize);
    bottom_row
        .min(source_rows.saturating_sub(1))
        .saturating_add(1)
        .saturating_sub(height as usize)
        .min(max_start)
}

impl Viewport {
    /// Anchor at the bottom: the source's last `height` rows are visible,
    /// the rest scrolled off above — what the preview pane has always done,
    /// since an agent's own input line sits at the bottom.
    pub fn bottom_anchored(source_rows: usize, col_offset: u16, width: u16, height: u16) -> Self {
        Self::bottom_anchored_at(
            source_rows,
            col_offset,
            width,
            height,
            source_rows.saturating_sub(1),
        )
    }

    /// Anchor the visible window so `bottom_row` is its last row when
    /// possible. Short outer terminals use the active content row here,
    /// avoiding a crop made mostly of blank rows below a top-line session.
    pub fn bottom_anchored_at(
        source_rows: usize,
        col_offset: u16,
        width: u16,
        height: u16,
        bottom_row: usize,
    ) -> Self {
        let bottom_row = bottom_row.min(source_rows.saturating_sub(1));
        Self {
            row_offset: anchored_row_offset(source_rows, height, bottom_row),
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
        // `Cursor::col` is already a terminal display column: the PTY
        // emulator indexes its grid by columns, including the continuation
        // cell of a wide character. Do not sum cell widths here. Doing so
        // counts a wide lead cell as two before the cursor has crossed its
        // continuation cell, putting the caret one column too far right for
        // CJK input. The row crop still uses `Cell::width` because it emits
        // the lead cell and omits zero-width continuation cells.
        // The clamp, though, is by the row's display width, not its cell
        // count: the wire drops each wide cell's continuation, so a row of k
        // wide glyphs arrives k cells short and a cell-count clamp pinned the
        // caret left of typed Hangul.
        let row_width: usize = source_row.iter().map(|c| usize::from(c.width())).sum();
        let target = col.min(row_width) as u32;
        let offset = u32::from(self.col_offset);
        let panel_col = target.checked_sub(offset)?;
        if panel_col >= u32::from(self.width) {
            return None;
        }
        Some((panel_row as u16, panel_col as u16))
    }
}

#[cfg(test)]
mod visual_mode_tests {
    //! Visual mode on the tmux copy-mode-vi model: `v` gives a movable
    //! cursor and no selection; `v`/Space anchors; motions extend; `y` yanks
    //! and leaves; Esc/`q` cancel; `v` while anchored clears the anchor.
    use super::{capture_styled, selected_screen_text, Action, TextPoint, Ui};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use remuda_core::agent::{Cursor, StyledCell};
    use remuda_core::{SessionSummary, Size};
    use std::time::Duration;

    fn ui() -> Ui {
        let mut ui = Ui::new(
            vec![SessionSummary {
                id: String::new(),
                name: "agent".into(),
                instance_id: Some("test-agent".into()),
                output_version: Some(0),
                alive: true,
                idle: Duration::ZERO,
                output_idle: Some(Duration::ZERO),
                size: Size::new(120, 24),
                attached: false,
                human_idle: None,
                mouse_tracking: false,
            }],
            "sh",
            None,
        );
        ui.preview_cursor = Cursor {
            row: 0,
            col: 0,
            visible: true,
        };
        ui
    }

    fn press(ui: &mut Ui, keys: &str) -> Action {
        let mut last = Action::Nothing;
        for ch in keys.chars() {
            let code = match ch {
                '\u{1b}' => KeyCode::Esc,
                c => KeyCode::Char(c),
            };
            last = ui.on_key(KeyEvent::new(code, KeyModifiers::NONE));
        }
        last
    }

    fn copied(ui: &Ui) -> String {
        let cells: Vec<Vec<StyledCell>> = ["hello world"]
            .iter()
            .map(|line| {
                line.chars()
                    .map(|c| StyledCell {
                        text: c.to_string(),
                        ..StyledCell::default()
                    })
                    .collect()
            })
            .collect();
        let selection = ui.text_selection.as_ref().expect("an anchored selection");
        selected_screen_text(&cells, &[false], selection)
    }

    fn screen_row(ui: &mut Ui, text: &str) {
        let mut row = Vec::new();
        for ch in text.chars() {
            let wide = ch.len_utf8() > 1 && (ch.is_alphanumeric() || !ch.is_ascii());
            row.push(StyledCell {
                text: ch.to_string(),
                wide,
                ..StyledCell::default()
            });
        }
        ui.visual_screen = vec![row; 24];
        ui.visual_wrapped = vec![false; 24];
    }

    fn at(ui: &Ui) -> TextPoint {
        ui.visual_cursor
    }

    #[test]
    fn zero_moves_to_line_start() {
        let mut ui = ui();
        screen_row(&mut ui, "0123456789");
        press(&mut ui, "v");
        press(&mut ui, "0");
        assert_eq!(at(&ui), TextPoint { row: 0, col: 0 });
    }

    #[test]
    fn dollar_moves_to_true_end_past_pane_and_wide_cells() {
        let mut ui = ui();
        screen_row(&mut ui, &format!("ab한{}", "x".repeat(90)));
        press(&mut ui, "v$");
        assert_eq!(at(&ui), TextPoint { row: 0, col: 92 });
    }

    #[test]
    fn caret_moves_to_first_nonblank_without_landing_inside_wide_cell() {
        let mut ui = ui();
        screen_row(&mut ui, "  한x");
        press(&mut ui, "v^");
        assert_eq!(at(&ui), TextPoint { row: 0, col: 2 });
    }

    #[test]
    fn w_moves_to_next_word_start() {
        let mut ui = ui();
        screen_row(&mut ui, "one two! 한글 ok");
        press(&mut ui, "vw");
        assert_eq!(at(&ui), TextPoint { row: 0, col: 4 });
    }

    #[test]
    fn w_from_inside_the_last_word_clamps_to_the_row_end() {
        let mut ui = ui();
        screen_row(&mut ui, "longword");
        press(&mut ui, "v");
        ui.visual_cursor.col = 3;
        press(&mut ui, "w");
        assert_eq!(at(&ui), TextPoint { row: 0, col: 7 });
    }

    #[test]
    fn w_on_a_single_word_row_terminates_at_its_last_cell() {
        let mut ui = ui();
        screen_row(&mut ui, "singleword");
        press(&mut ui, "vw");
        assert_eq!(at(&ui), TextPoint { row: 0, col: 9 });
    }

    #[test]
    fn b_moves_to_previous_word_start() {
        let mut ui = ui();
        screen_row(&mut ui, "one two three");
        press(&mut ui, "v");
        ui.visual_cursor.col = 8;
        press(&mut ui, "b");
        assert_eq!(at(&ui), TextPoint { row: 0, col: 4 });
    }

    #[test]
    fn e_moves_to_word_end_without_splitting_wide_character() {
        let mut ui = ui();
        screen_row(&mut ui, "한글 ok");
        press(&mut ui, "ve");
        assert_eq!(at(&ui), TextPoint { row: 0, col: 1 });
    }

    #[test]
    fn b_and_e_at_the_row_end_stay_on_the_last_word() {
        let mut ui = ui();
        screen_row(&mut ui, "one two");
        press(&mut ui, "v");
        ui.visual_cursor.col = 6;
        press(&mut ui, "b");
        assert_eq!(at(&ui), TextPoint { row: 0, col: 4 });
        ui.visual_cursor.col = 6;
        press(&mut ui, "e");
        assert_eq!(at(&ui), TextPoint { row: 0, col: 6 });
    }

    #[test]
    fn every_vim_motion_handles_an_empty_line() {
        for motion in ["0", "$", "^", "w", "b", "e", "gg", "G"] {
            let mut ui = ui();
            screen_row(&mut ui, "");
            press(&mut ui, "v");
            press(&mut ui, motion);
            assert_eq!(at(&ui).col, 0, "motion {motion}");
            assert_eq!(
                at(&ui).row,
                if motion == "G" { 23 } else { 0 },
                "motion {motion}"
            );
        }
    }

    #[test]
    fn visual_motion_pans_to_keep_the_cursor_visible() {
        let mut ui = ui();
        ui.preview_width = 10;
        screen_row(&mut ui, &"x".repeat(94));
        press(&mut ui, "v$");
        assert_eq!(at(&ui).col, 93);
        assert_eq!(ui.pan, 85);
        let display_col = super::display_col_for_cell_index(&ui.visual_screen[0], at(&ui).col);
        assert!(display_col < usize::from(ui.pan) + usize::from(ui.preview_width));
    }

    #[test]
    fn visual_cursor_on_wide_glyph_stays_visible_at_crop_boundary() {
        let mut ui = ui();
        ui.preview_width = 10;
        screen_row(&mut ui, &format!("{}한{}", "a".repeat(8), "b".repeat(20)));
        ui.visual_cursor = TextPoint { row: 0, col: 8 };

        ui.keep_visual_cursor_visible();

        assert_eq!(ui.pan, 1);
        let (cropped, _) = super::crop_styled(&ui.visual_screen[..1], 10, 1, ui.pan, 0);
        assert!(
            cropped[0].contains('한'),
            "cropped row was: {:?}",
            cropped[0]
        );
    }

    #[test]
    fn visual_dollar_pans_past_the_crop_marker_on_a_wide_session() {
        let mut ui = ui();
        ui.preview_width = 103;
        let mut row: Vec<_> = "x"
            .repeat(180)
            .chars()
            .map(|ch| StyledCell {
                text: ch.to_string(),
                ..StyledCell::default()
            })
            .collect();
        row.resize_with(200, || StyledCell {
            text: " ".into(),
            ..StyledCell::default()
        });
        ui.visual_screen = vec![row; 24];
        ui.visual_wrapped = vec![false; 24];
        press(&mut ui, "v$");
        assert_eq!(at(&ui).col, 179);
        assert_eq!(ui.pan, 97);
        assert_eq!(ui.pan + ui.preview_width, 200);
    }

    #[test]
    fn line_motions_follow_soft_wrap_metadata() {
        let mut ui = ui();
        screen_row(&mut ui, "");
        ui.visual_screen[0] = "first"
            .chars()
            .map(|ch| StyledCell {
                text: ch.to_string(),
                ..StyledCell::default()
            })
            .collect();
        ui.visual_screen[1] = "second"
            .chars()
            .map(|ch| StyledCell {
                text: ch.to_string(),
                ..StyledCell::default()
            })
            .collect();
        ui.visual_wrapped[0] = true;
        press(&mut ui, "vv$");
        assert_eq!(at(&ui), TextPoint { row: 1, col: 5 });
        let cells = ui.visual_screen.clone();
        assert_eq!(
            selected_screen_text(
                &cells,
                &ui.visual_wrapped,
                ui.text_selection.as_ref().unwrap()
            ),
            "firstsecond"
        );
        press(&mut ui, "0");
        assert_eq!(at(&ui), TextPoint { row: 0, col: 0 });
    }

    #[test]
    fn motions_and_yank_use_real_daemon_capture_cells_for_wide_text() {
        use remuda_core::protocol::{Request, Response};
        use std::time::{Duration, Instant};

        let text = "한글 텍스트 emoji 🚀 end";
        let dir = std::env::temp_dir().join(format!("remuda-visual-cells-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch directory");
        let path = crate::daemon::socket_path_in(&dir, "visual-cells");
        let serving = path.clone();
        std::thread::spawn(move || {
            let _ = crate::daemon::serve(&serving);
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while crate::ipc::connect(&path).is_err() {
            assert!(
                Instant::now() < deadline,
                "private test daemon did not start"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let name = "visual-cells";
        let command = vec![
            "sh".into(),
            "-c".into(),
            "printf '%s' \"$1\"; sleep 30".into(),
            "fixture".into(),
            text.into(),
        ];
        assert!(matches!(
            crate::client::request(
                &path,
                &Request::New {
                    name: Some(name.into()),
                    command,
                    size: Size::new(120, 24),
                    cwd: None,
                    env: None,
                },
            ),
            Ok(Response::Value(_))
        ));
        // ConPTY can take several seconds to launch the shell and flush its
        // first output on a loaded Windows CI worker.
        let capture_deadline = Instant::now() + Duration::from_secs(30);
        let (cells, wrapped) = loop {
            let (cells, wrapped, _, _, _) = capture_styled(&path, name, 0).expect("real capture");
            let captured: String = cells[0].iter().map(|cell| cell.text.as_str()).collect();
            if captured.starts_with(text) {
                break (cells, wrapped);
            }
            assert!(
                Instant::now() < capture_deadline,
                "fixture text not captured: {captured:?}"
            );
            std::thread::sleep(Duration::from_millis(25));
        };
        assert_eq!(cells[0].iter().filter(|cell| cell.wide).count(), 6);

        let make_ui = || {
            let mut ui = ui();
            ui.sessions[0].name = name.into();
            ui.visual_screen = cells.clone();
            ui.visual_wrapped = wrapped.clone();
            ui
        };

        let mut ui = make_ui();
        press(&mut ui, "vvw");
        let selected = selected_screen_text(&cells, &wrapped, ui.text_selection.as_ref().unwrap());
        assert_eq!(selected, "한글 텍");
        assert_eq!(at(&ui), TextPoint { row: 0, col: 3 });

        let mut ui = make_ui();
        press(&mut ui, "v");
        ui.visual_cursor.col = 11;
        press(&mut ui, "ve");
        assert_eq!(at(&ui), TextPoint { row: 0, col: 13 });

        let mut ui = make_ui();
        press(&mut ui, "v");
        ui.visual_cursor.col = 17;
        press(&mut ui, "vb");
        assert_eq!(at(&ui), TextPoint { row: 0, col: 15 });

        let _ = crate::client::request(&path, &Request::Close { name: name.into() });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gg_and_capital_g_move_to_top_and_bottom_rows() {
        let mut ui = ui();
        screen_row(&mut ui, "line");
        press(&mut ui, "vG");
        assert_eq!(at(&ui).row, 23);
        press(&mut ui, "gg");
        assert_eq!(at(&ui).row, 0);
    }

    #[test]
    fn v_moves_then_v_anchors_then_y_yanks_and_leaves() {
        let mut ui = ui();
        assert_eq!(press(&mut ui, "vll"), Action::Nothing);
        assert!(ui.text_selection.is_none(), "v must not select yet");
        press(&mut ui, "vllll");
        assert_eq!(copied(&ui), "llo w");
        assert_eq!(press(&mut ui, "y"), Action::CopySelection("agent".into()));
        assert!(!ui.visual, "y must leave visual mode");
    }

    #[test]
    fn space_anchors_like_v() {
        let mut ui = ui();
        press(&mut ui, "vl ll");
        assert_eq!(copied(&ui), "ell");
    }

    #[test]
    fn y_without_an_anchor_leaves_and_copies_nothing() {
        let mut ui = ui();
        assert_eq!(press(&mut ui, "vy"), Action::Nothing);
        assert!(!ui.visual);
    }

    #[test]
    fn esc_and_q_cancel_without_copying() {
        for cancel in ["\u{1b}", "q"] {
            let mut ui = ui();
            assert_eq!(
                press(&mut ui, &format!("vvl{cancel}")),
                Action::Nothing,
                "{cancel:?}"
            );
            assert!(!ui.visual, "{cancel:?}");
            assert!(ui.text_selection.is_none(), "{cancel:?}");
        }
    }

    #[test]
    fn v_while_anchored_clears_the_anchor_but_stays() {
        let mut ui = ui();
        press(&mut ui, "vvlv");
        assert!(ui.visual);
        assert!(ui.text_selection.is_none());
    }

    #[test]
    fn vim_motions_extend_an_anchored_selection() {
        let mut ui = ui();
        screen_row(&mut ui, "one two");
        press(&mut ui, "vv$");
        assert_eq!(ui.text_selection.as_ref().unwrap().end, at(&ui));
        assert_eq!(at(&ui).col, 6);
    }

    #[test]
    fn the_footer_names_the_visual_state() {
        let mut ui = ui();
        press(&mut ui, "v");
        assert!(super::render(&ui, "hello world", "s", 120, 30).contains("visual: move"));
        press(&mut ui, "v");
        assert!(super::render(&ui, "hello world", "s", 120, 30).contains("visual: selecting"));
    }
}

#[cfg(test)]
mod cursor_width_tests {
    use super::Viewport;
    use remuda_core::agent::StyledCell;

    fn cell(text: &str, wide: bool) -> StyledCell {
        StyledCell {
            text: text.into(),
            wide,
            ..StyledCell::default()
        }
    }

    fn cursor_col(row: &[StyledCell], col: usize) -> u16 {
        Viewport::bottom_anchored(1, 0, 20, 1)
            .map_cursor(&[row.to_vec()], 0, col)
            .expect("cursor is visible")
            .1
    }

    /// Bytes -> vt100 -> the wire -> caret, a session pane's real path. The wire
    /// drops wide continuation cells (k Hangul arrive k cells short); the caret
    /// must still land where the text ends, as in iTerm or tmux.
    fn caret_after_typing(typed: &str) -> (u16, u16) {
        use remuda_core::protocol::{collapse_runs, expand_runs};
        let size = remuda_core::Size::new(80, 24);
        let mut parser = vt100::Parser::new(size.rows(), size.cols(), 0);
        // A claude/codex-style input box: a right border, then "│ > " + input.
        parser.process(format!("\x1b[1;80H│\x1b[1;1H│ > {typed}").as_bytes());
        let (_, col) = crate::pty::display_cursor(parser.screen());
        let wire: Vec<Vec<StyledCell>> = crate::pty::styled_cells(parser.screen(), size)
            .iter()
            .map(|row| expand_runs(&collapse_runs(row)))
            .collect();
        let caret = Viewport::bottom_anchored(wire.len(), 0, 80, 24)
            .map_cursor(&wire, 0, col as usize)
            .expect("cursor is visible")
            .1;
        (caret, col)
    }

    /// DECAWM pending wrap: after exactly filling a row the emulator's
    /// cursor sits one past the last column; a terminal draws (and reports)
    /// it ON the last column. Clamped, the caret stays visible at the edge.
    #[test]
    fn caret_stays_on_the_last_column_in_pending_wrap() {
        for typed in ["a".repeat(80), "한".repeat(40)] {
            let size = remuda_core::Size::new(80, 24);
            let mut parser = vt100::Parser::new(size.rows(), size.cols(), 0);
            parser.process(typed.as_bytes());
            let (row, col) = crate::pty::display_cursor(parser.screen());
            assert_eq!((row, col), (0, 79), "{typed:?}");
            let cells = crate::pty::styled_cells(parser.screen(), size);
            let caret = Viewport::bottom_anchored(cells.len(), 0, 80, 24).map_cursor(
                &cells,
                0,
                col as usize,
            );
            assert_eq!(caret.map(|c| c.1), Some(79), "caret hidden at the edge");
        }
    }

    #[test]
    fn caret_follows_ascii_typed_across_a_bordered_row() {
        let (caret, col) = caret_after_typing(&"a".repeat(60));
        assert_eq!((caret, col), (64, 64));
    }

    #[test]
    fn caret_follows_hangul_typed_across_a_bordered_row() {
        let (caret, col) = caret_after_typing(&"한".repeat(30));
        assert_eq!(col, 64, "the emulator's own cursor");
        assert_eq!(caret, col, "the caret lags left of the typed text");
    }

    #[test]
    fn latin_cursor_uses_terminal_column() {
        assert_eq!(cursor_col(&[cell("a", false), cell("b", false)], 2), 2);
    }

    #[test]
    fn korean_cursor_does_not_double_count_wide_lead_cell() {
        let row = [cell("안", true), cell("", false), cell("b", false)];
        assert_eq!(cursor_col(&row, 2), 2);
        assert_eq!(cursor_col(&row, 3), 3);
    }

    #[test]
    fn combining_text_stays_in_one_terminal_column() {
        let row = [cell("e\u{301}", false), cell("x", false)];
        assert_eq!(cursor_col(&row, 1), 1);
    }

    #[test]
    fn mixed_korean_and_ascii_tracks_the_terminal_grid() {
        let row = [
            cell("A", false),
            cell("한", true),
            cell("", false),
            cell("B", false),
        ];
        assert_eq!(cursor_col(&row, 4), 4);
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
            if row_cut && cols > 0 {
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
    let width = width as usize;
    if width == 0 {
        return String::new();
    }
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
            let w = char_width(c);
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
    let divider = if ui.list_visible && list_w < cols {
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
        // Start list rows at the top now that the pane header moved to the footer.
        if ui.list_visible {
            let left = list_row(
                ui,
                row as usize + list_viewport(ui, body) * ui.session_rows,
                list_w,
            );
            out.push_str(&fit(&left, list_w));
        }
        out.push_str(divider);
        let line = lines.get(row as usize).map_or("", String::as_str);
        out.push_str(&fit(line, preview_w));
    }

    out.push_str(&format!("\x1b[{};1H", rows));
    out.push_str(&footer(ui, server, cut, preview_w, cols));
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
        Color::Idx(n) if n < 16 => {
            out.push_str(&format!("\x1b[{}{}m", if fg { 9 } else { 10 }, n - 8))
        }
        Color::Idx(n) => out.push_str(&format!("\x1b[{};5;{n}m", if fg { 38 } else { 48 })),
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
#[cfg(test)]
fn crop_styled(
    cells: &[Vec<StyledCell>],
    cols: u16,
    rows: u16,
    pan: u16,
    bottom_row: usize,
) -> (Vec<String>, bool) {
    let row_offset = anchored_row_offset(cells.len(), rows, bottom_row);
    crop_styled_at_offset(cells, cols, rows, pan, row_offset)
}

fn crop_styled_at_offset(
    cells: &[Vec<StyledCell>],
    cols: u16,
    rows: u16,
    pan: u16,
    row_offset: usize,
) -> (Vec<String>, bool) {
    let viewport = Viewport {
        row_offset: row_offset.min(cells.len().saturating_sub(rows as usize)),
        col_offset: pan,
        width: cols,
        height: rows,
    };
    let (cropped, cut) = viewport.crop(cells);
    let out = cropped
        .into_iter()
        .map(|(mut visible, row_cut)| {
            if row_cut && cols > 0 {
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
            if row_cut && cols > 0 {
                s.push('→');
            }
            s
        })
        .collect();
    (out, cut)
}

/// The crop should end at the most relevant row: the child's cursor, or the
/// last row containing visible text when content extends below the cursor.
fn preview_anchor_row(cells: &[Vec<StyledCell>], cursor_row: u16) -> usize {
    let last_nonempty = cells
        .iter()
        .rposition(|row| row.iter().any(|cell| !cell.text.trim().is_empty()));
    usize::from(cursor_row)
        .max(last_nonempty.unwrap_or(0))
        .min(cells.len().saturating_sub(1))
}

fn preview_row_offset(
    cells: &[Vec<StyledCell>],
    cursor: Cursor,
    height: u16,
    preserve_history: bool,
) -> usize {
    if preserve_history || !cursor.visible {
        return cells.len().saturating_sub(height as usize);
    }
    anchored_row_offset(cells.len(), height, preview_anchor_row(cells, cursor.row))
        .min(cursor.row as usize)
}

fn ui_preview_row_offset(ui: &Ui, height: u16) -> usize {
    let preserve_history = ui
        .selected()
        .and_then(|session| ui.scrollback.get(&session.name))
        .is_some_and(|state| state.offset > 0);
    preview_row_offset(
        &ui.visual_screen,
        ui.preview_cursor,
        height,
        preserve_history,
    )
}

/// The one text-width rule the list renderer uses. Ambiguous-width
/// characters (East Asian Width A, e.g. the status dot) count as narrow.
fn char_width(c: char) -> usize {
    // Ambiguous-width is treated as narrow by owner choice.
    unicode_width::UnicodeWidthChar::width(c).unwrap_or(1)
}

/// A styled row's display width, ignoring the SGR bytes riding along with
/// it, and counting a wide (CJK) character as the 2 columns it actually
/// draws — `fit`'s plain char count would under-count it by 1. See steps/023.
fn visible_width(s: &str) -> usize {
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
            width += char_width(c);
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
    preserve_history: bool,
) -> Option<(u16, u16)> {
    if !cursor.visible {
        return None;
    }
    let mut viewport = Viewport::bottom_anchored_at(
        cells.len(),
        pan,
        preview_w,
        body,
        preview_anchor_row(cells, cursor.row),
    );
    viewport.row_offset = preview_row_offset(cells, cursor, body, preserve_history);
    let (panel_row, panel_col) =
        viewport.map_cursor(cells, cursor.row as usize, cursor.col as usize)?;
    // Convert pane-local coordinates to 1-based terminal coordinates, adding
    // the list and divider widths when the preview shares the screen.
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
    let divider = if ui.list_visible && list_w < cols {
        match ui.focus {
            Focus::List => "│",
            Focus::Session => "\x1b[7m┃\x1b[0m",
        }
    } else {
        ""
    };

    let selected = cells_with_selection(ui, cells);
    let preserve_history = ui
        .selected()
        .and_then(|session| ui.scrollback.get(&session.name))
        .is_some_and(|state| state.offset > 0);
    let row_offset = preview_row_offset(cells, cursor, body, preserve_history);
    let (lines, cut) = crop_styled_at_offset(&selected, preview_w, body, ui.pan, row_offset);
    let caret = locate_cursor(
        cells,
        cursor,
        ui.pan,
        preview_w,
        body,
        list_w,
        preserve_history,
    );
    // Hide before moving the terminal cursor around the frame. The final
    // caret state below is the only place that makes it visible again.
    let mut out = String::from("\x1b[?2026h\x1b[?25l\x1b[H");
    for row in 0..body {
        out.push_str(&format!("\x1b[{};1H\x1b[K", row + 1));
        if ui.list_visible {
            let left = list_row(
                ui,
                row as usize + list_viewport(ui, body) * ui.session_rows,
                list_w,
            );
            out.push_str(&fit(&left, list_w));
        }
        out.push_str(divider);
        let line = lines.get(row as usize).map_or("", String::as_str);
        out.push_str(&fit_styled(line, preview_w));
    }

    out.push_str(&format!("\x1b[{};1H\x1b[K", rows));
    out.push_str(&footer(ui, server, cut, preview_w, cols));
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
    // Visual mode before an anchor: the movable cursor, drawn as one cell.
    let cursor_only = (ui.visual && !ui.anchored()).then(|| TextSelection {
        session: ui.selected().map_or_else(String::new, |s| s.name.clone()),
        start: ui.visual_cursor,
        end: ui.visual_cursor,
    });
    let Some(selection) = cursor_only.as_ref().or(ui.text_selection.as_ref()) else {
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
#[cfg(test)]
fn widest(ui: &Ui) -> u16 {
    ui.sessions.iter().map(|s| s.size.cols()).max().unwrap_or(0)
}

/// The first session shown in the list, based on Lua's rows-per-session
/// contract rather than a native presentation policy.
fn list_viewport(ui: &Ui, body: u16) -> usize {
    let visible = body as usize / ui.session_rows;
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
    // No caret column: selection is Lua's zero-width reverse-video name, and
    // this fallback marks it the same way.
    let content = match ui.sessions_text.get(row) {
        Some(text) => text.clone(),
        None if !row.is_multiple_of(ui.session_rows) => String::new(),
        None if session_index == ui.selected => format!("\x1b[7m{}\x1b[0m", session.name),
        None => session.name.clone(),
    };
    fit_session_row(&content, width)
}

/// Keep the status dot in one shared column on every row. Attached rows use
/// the horse in the final three cells; other rows reserve the same cells.
/// Only the styled name is shortened, one Unicode scalar at a time, by `fit`.
fn fit_session_row(content: &str, width: u16) -> String {
    const MARKER: &str = " 🏇";
    const PLAIN_PAD: &str = "   ";
    for state in [" \x1b[32m●\x1b[0m", " \x1b[31m●\x1b[0m"] {
        if let Some(name) = content.strip_suffix(state) {
            let suffix = format!("{state}{PLAIN_PAD}");
            let name_width = width.saturating_sub(visible_width(&suffix) as u16);
            return format!("{}{}", fit(name, name_width), suffix);
        }
        let attached = format!("{state}{MARKER}");
        if let Some(name) = content.strip_suffix(&attached) {
            let name_width = width.saturating_sub(visible_width(&attached) as u16);
            return format!("{}{}", fit(name, name_width), attached);
        }
    }
    if let Some(name) = content.strip_suffix(MARKER) {
        let name_width = width.saturating_sub(visible_width(MARKER) as u16);
        return format!("{}{}", fit(name, name_width), MARKER);
    }
    fit(content, width)
}

/// The crop notice moved here when the preview lost its title band: a crop that
/// reads as absence is the failure this repo keeps re-discovering, and the
/// footer is the only band left that is not the session's own screen.
fn footer(ui: &Ui, server: &str, cut: bool, preview_w: u16, cols: u16) -> String {
    let status = match &ui.mode {
        Mode::Prompt(buffer) => format!("start: {buffer}▏   ⏎ run · esc cancel"),
        Mode::Confirm(name) => format!("kill {name}? it is running — y / n"),
        // Focus is named in words as well as drawn, because the one thing a
        // person must never wonder is where their next keystroke lands.
        Mode::Browse if ui.focus == Focus::Session => format!(
            "▶ {} — every key goes to the session{}   ctrl-\\ back to the list",
            ui.selected().map_or("", |s| s.name.as_str()),
            if cut {
                format!("   showing {preview_w} cols")
            } else {
                String::new()
            },
        ),
        Mode::Browse if ui.visual => {
            if ui.anchored() {
                "visual: selecting — hjkl 0$^ wbe ggG   y yank   v clear   esc/q cancel".into()
            } else {
                "visual: move — hjkl 0$^ wbe ggG   v/space anchor   y/esc/q leave".into()
            }
        }
        Mode::Browse if let Some(socket) = &ui.daemon_gone => {
            let prefix = "daemon gone (stale): ";
            let controls = " · r restart · q quit";
            let path_width = (cols as usize)
                .saturating_sub(visible_width(prefix) + visible_width(controls))
                .min(u16::MAX as usize) as u16;
            format!("{prefix}{}{controls}", fit(socket, path_width))
        }
        Mode::Browse => match &ui.notice {
            Some(notice) => format!("remuda: {notice}"),
            None if ui.sessions.is_empty() => "n new   q quit".into(),
            None if cut => format!("↑↓/jk ⏎ enter n new l list q quit · showing {preview_w}"),
            None => "↑↓/jk select   ⏎ enter   n new   x kill   l list   q quit".into(),
        },
    };
    let brand = format!("remuda · {server} 🏇");
    let status_width = visible_width(&status);
    let brand_width = visible_width(&brand);
    if status_width + 3 + brand_width <= cols as usize {
        format!(
            "{status}{}{}",
            " ".repeat(cols as usize - status_width - brand_width),
            brand
        )
    } else {
        fit(&status, cols)
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
    ui.preview_rows = rows.saturating_sub(1).max(1);
    ui.preview_width = ui_layout(ui, cols).1;
    if !skip_list {
        // The buffer is allowed to reorder the herd. Keep the identity, not
        // the old numeric position, so a refresh cannot move a cursor (or an
        // attached pane) onto its new neighbour.
        let selected_name = ui.selected().map(|session| session.name.clone());
        match list(path) {
            Ok(sessions) => {
                if ui.daemon_gone.take().is_some() {
                    ui.notice = None;
                }
                ui.consecutive_transport_failures = 0;
                ui.sessions = sessions;
            }
            // Keep the last known herd rather than blanking it: a transport
            // failure is not a report that every session vanished. See steps/021.
            Err(e) => match crate::ipc::connect(path) {
                Ok(_) => {
                    ui.consecutive_transport_failures = 0;
                    ui.notice = Some(e);
                }
                Err(probe_error) => record_daemon_probe_failure(path, ui, e, probe_error),
            },
        }
        // Same skip as the relist above, and for the same reason: a
        // `Type`-forced wake (fast-typing tick) needs none of this, so
        // paying for it there would be the exact per-keystroke IPC cost
        // steps/017/022 exist to avoid.
        if ui.daemon_gone.is_none() {
            let (list_w, _) = ui_layout(ui, cols);
            match sessions_buffer_lines(path, list_w, ui.selected, selected_name.as_deref()) {
                Ok((session_rows, lines, order)) => {
                    ui.session_rows = session_rows;
                    ui.sessions_text = lines;
                    apply_session_order(ui, selected_name.as_deref(), &order);
                }
                // Same fallback as the relist: keep whatever was last drawn
                // rather than blanking the tail column on a transport hiccup.
                Err(e) => ui.notice = Some(e),
            }
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
    if !skip_list && ui.daemon_gone.is_none() {
        sync_shown_session(path, ui, shown, selection_moved);
    }
    resize_shown_session(path, ui, shown, cols, rows);
    let (cells, wrapped, cursor) = if ui.daemon_gone.is_some() {
        (Vec::new(), Vec::new(), hidden)
    } else {
        match shown.as_ref() {
            Some(ShownTarget::Session(name)) => match capture_preview(path, ui, name) {
                Ok(result) => result,
                Err(e) => {
                    ui.notice = Some(format!("{name}: {e}"));
                    (Vec::new(), Vec::new(), hidden)
                }
            },
            Some(ShownTarget::Buffer(name)) => match capture_buffer(path, name) {
                Ok((cells, cursor)) => (cells, Vec::new(), cursor),
                Err(e) => {
                    ui.notice = Some(format!("{name}: {e}"));
                    (Vec::new(), Vec::new(), hidden)
                }
            },
            None => (Vec::new(), Vec::new(), hidden),
        }
    };
    ui.preview_cursor = cursor;
    ui.visual_screen = cells.clone();
    ui.visual_wrapped = wrapped;
    let frame = render_styled(ui, &cells, cursor, server, cols, rows);
    // The write is gated on change, as it always was — but before
    // `should_refresh` existed, everything ABOVE this line (a relist, an IPC
    // round-trip for a full screen snapshot, a rebuilt frame) ran on every
    // wake of the input poll too, key or not. That is what a session with
    // focus cost ~25 times a second while sitting idle, and every keystroke
    // on top of that.
    if frame != *painted {
        #[cfg(not(test))]
        {
            let mut stdout = std::io::stdout();
            let previous_content = painted
                .rsplit_once("\x1b[J")
                .map_or(painted.as_str(), |(content, _)| content);
            let current_content = frame
                .rsplit_once("\x1b[J")
                .map_or(frame.as_str(), |(content, _)| content);
            if previous_content == current_content {
                let cursor = frame.rsplit_once("\x1b[J").map_or("", |(_, cursor)| cursor);
                // A cursor-only update still moves a visible terminal caret.
                // Hide it for that move, then let the frame's suffix restore
                // the child-requested visibility state.
                stdout.write_all(b"\x1b[?25l")?;
                stdout.write_all(cursor.as_bytes())?;
            } else {
                stdout.write_all(frame.as_bytes())?;
            }
            stdout.flush()?;
        }
        *painted = frame;
    }
    Ok((cols, rows))
}

fn sync_shown_session(
    path: &Path,
    ui: &mut Ui,
    shown: &mut Option<ShownTarget>,
    selection_moved: bool,
) {
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

fn resize_shown_session(
    path: &Path,
    ui: &mut Ui,
    shown: &Option<ShownTarget>,
    cols: u16,
    rows: u16,
) {
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
            Action::Restart => match restart_daemon(path, server) {
                Ok(()) => {
                    ui.daemon_gone = None;
                    ui.consecutive_transport_failures = 0;
                    ui.notice = None;
                }
                Err(error) => ui.notice = Some(error),
            },
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
                scroll_selected(&mut ui, delta);
            }
            Action::Copy(name) => copy_screen(path, &mut ui, &name),
            Action::CopySelection(name) => copy_selection(path, &mut ui, &name),
            Action::Paste => paste(path, &mut ui, &held),
        }
        // Not `painted.clear()`: the frame-vs-`painted` compare in `refresh`
        // already skips the write when a key changed nothing visible — see
        // steps/026. Only the immediate re-check is forced here.
        force_refresh = true;
    }
}

fn copy_screen(path: &Path, ui: &mut Ui, name: &str) {
    match capture_styled(path, name, 0) {
        Ok((cells, wrapped, _, _, _)) => {
            ui.yank = all_screen_text(&cells, &wrapped);
            ui.notice = Some(format!(
                "copied {} bytes; p pastes into the selected session",
                ui.yank.len()
            ));
        }
        Err(e) => ui.notice = Some(format!("{name}: {e}")),
    }
}

fn copy_selection(path: &Path, ui: &mut Ui, name: &str) {
    let offset = ui.scrollback.get(name).map_or(0, |state| state.offset);
    match capture_styled(path, name, offset) {
        Ok((cells, wrapped, _, _, _)) => {
            // Copy-and-cancel, as tmux: the selection has done its job.
            ui.yank = ui.text_selection.take().map_or_else(
                || all_screen_text(&cells, &wrapped),
                |selection| selected_screen_text(&cells, &wrapped, &selection),
            );
            ui.notice = Some(format!(
                "copied {} bytes; p pastes into the selected session",
                ui.yank.len()
            ));
        }
        Err(e) => ui.notice = Some(format!("{name}: {e}")),
    }
}

/// Start a replacement only after the user presses `r` in the gone-daemon
/// state. The child owns the daemon socket and is detached from this terminal.
fn restart_daemon(path: &Path, server: &str) -> Result<(), String> {
    if crate::ipc::connect(path).is_ok() {
        return Ok(());
    }
    if path.exists() && !socket_lock_is_free(path) {
        return Err(format!(
            "daemon lock is still held for {}; wait or quit",
            path.display()
        ));
    }
    let exe =
        std::env::current_exe().map_err(|error| format!("cannot find own binary: {error}"))?;
    let mut command = std::process::Command::new(exe);
    command
        .args(["-s", server, "daemon"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid() is async-signal-safe and this is a fresh child.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot restart daemon: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if crate::ipc::connect(path).is_ok() {
            return Ok(());
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("cannot check restarted daemon: {error}"))?
        {
            return Err(format!("restarted daemon exited with {status}"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    Err(format!(
        "restarted daemon did not answer at {} within five seconds",
        path.display()
    ))
}

fn scroll_state(state: &mut ScrollState, delta: i16) {
    if delta > 0 {
        if state.scroll_direction != 1 {
            state.recent_up_output = 0;
        }
        state.scroll_direction = 1;
        state.offset = state
            .offset
            .saturating_add(delta as usize)
            .min(state.history_rows);
    } else if delta < 0 {
        if state.scroll_direction != -1 {
            state.offset = state.offset.saturating_sub(state.recent_up_output);
            state.recent_up_output = 0;
        }
        state.scroll_direction = -1;
        state.last_scroll_down = Some(Instant::now());
        state.offset = state.offset.saturating_sub(delta.unsigned_abs() as usize);
    }
}

fn scroll_selected(ui: &mut Ui, delta: i16) {
    if let Some(name) = ui.selected().map(|session| session.name.clone()) {
        scroll_state(ui.scrollback.entry(name).or_default(), delta);
    }
}

fn anchor_offset_to_new_history(
    offset: usize,
    previous_total: usize,
    current_total: usize,
    current_rows: usize,
) -> usize {
    if offset == 0 {
        0
    } else {
        offset
            .saturating_add(current_total.saturating_sub(previous_total))
            .min(current_rows)
    }
}

/// Capture at the offset computed from the history total in that capture.
/// Output can arrive between a probe and its corrective capture; retry until
/// the offset and the captured total describe the same screen state.
fn capture_anchored<T>(
    base_offset: usize,
    previous_total: usize,
    mut capture: impl FnMut(usize) -> Result<(T, usize, usize), String>,
) -> Result<(T, usize, usize, usize, usize), String> {
    let mut requested = base_offset;
    for attempt in 0..MAX_ANCHOR_CAPTURES {
        let (value, rows, total) = capture(requested)?;
        let anchored = anchor_offset_to_new_history(base_offset, previous_total, total, rows);
        if anchored == requested {
            return Ok((value, rows, total, anchored, total));
        }
        if attempt + 1 == MAX_ANCHOR_CAPTURES {
            // The captured cells use `requested`, while `total` may already
            // include output that arrived after that offset was chosen. Store
            // the total that corresponds to the captured offset so the next
            // frame accounts for that missed drift exactly once.
            let anchor_total = total.saturating_sub(requested.saturating_sub(base_offset));
            return Ok((value, rows, total, requested, anchor_total));
        }
        requested = anchored;
    }
    unreachable!("the bounded anchor loop always returns a capture")
}

/// Keep a scrolled preview on the same history rows as output pushes new rows.
fn capture_preview(path: &Path, ui: &mut Ui, name: &str) -> Result<PreviewCapture, String> {
    let state = ui.scrollback.entry(name.to_string()).or_default();
    let scrolling_down = state
        .last_scroll_down
        .is_some_and(|last| last.elapsed() < SCROLL_DOWN_SETTLE);
    let (cells, wrapped, cursor, history_rows, history_total, anchored, anchor_total) =
        if scrolling_down {
            let (cells, wrapped, cursor, rows, total) = capture_styled(path, name, state.offset)?;
            (cells, wrapped, cursor, rows, total, state.offset, total)
        } else {
            let (capture, rows, total, anchored, anchor_total) =
                capture_anchored(state.offset, state.anchor_total, |offset| {
                    let (cells, wrapped, cursor, rows, total) = capture_styled(path, name, offset)?;
                    Ok(((cells, wrapped, cursor), rows, total))
                })?;
            let (cells, wrapped, cursor) = capture;
            (cells, wrapped, cursor, rows, total, anchored, anchor_total)
        };
    state.offset = anchored;
    if state.scroll_direction == 1 {
        state.recent_up_output = state
            .recent_up_output
            .saturating_add(history_total.saturating_sub(state.history_total));
    }
    state.history_rows = history_rows;
    state.history_total = history_total;
    state.anchor_total = anchor_total;
    Ok((cells, wrapped, cursor))
}

fn paste(path: &Path, ui: &mut Ui, held: &Option<(String, Hold)>) {
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

fn daemon_is_definitively_gone(path: &Path, error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::NotFound {
        return true;
    }
    #[cfg(unix)]
    {
        if !path.exists() {
            return true;
        }
        if error.kind() == std::io::ErrorKind::ConnectionRefused {
            return socket_lock_is_free(path);
        }
    }
    false
}

fn daemon_failure_marks_gone(consecutive: u8, definitive: bool) -> bool {
    definitive || consecutive >= DAEMON_FAILURES_BEFORE_GONE
}

fn record_daemon_probe_failure(
    path: &Path,
    ui: &mut Ui,
    list_error: String,
    probe_error: std::io::Error,
) {
    let definitive = daemon_is_definitively_gone(path, &probe_error);
    let busy_refusal = probe_error.kind() == std::io::ErrorKind::ConnectionRefused
        && path.exists()
        && !socket_lock_is_free(path);
    if !definitive {
        if busy_refusal {
            ui.consecutive_transport_failures = 0;
        } else {
            ui.consecutive_transport_failures = ui.consecutive_transport_failures.saturating_add(1);
        }
    }
    if daemon_failure_marks_gone(ui.consecutive_transport_failures, definitive) {
        ui.daemon_gone = Some(path.display().to_string());
        ui.notice = None;
        ui.focus = Focus::List;
        ui.mode = Mode::Browse;
        ui.visual = false;
        ui.text_selection = None;
    } else {
        ui.notice = Some(list_error);
    }
}

fn socket_lock_is_free(socket: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let mut lock_name = socket.as_os_str().to_os_string();
        lock_name.push(".lock");
        let lock_path = Path::new(&lock_name);
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(lock_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
            Err(_) => return false,
        };
        // Probe the exact exclusive lock the daemon holds for its lifetime.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            return false;
        }
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) == 0 }
    }
    #[cfg(windows)]
    {
        let _ = socket;
        false
    }
}

/// The "*sessions*" buffer's content, refreshed at WIDTH and fetched in one
/// `Eval` round trip (`tools.lua`'s `remuda._refresh_sessions_buffer`, then
/// `remuda.buffer.new("*sessions*"):get()`).
fn sessions_buffer_lines(
    path: &Path,
    width: u16,
    selected: usize,
    selected_name: Option<&str>,
) -> Result<(usize, Vec<String>, Vec<String>), String> {
    let selected_name = selected_name.map_or_else(|| "nil".to_string(), mcp::lua_string);
    let code = format!(
        "remuda._refresh_sessions_buffer({width}, {selected}, {selected_name}); return remuda.buffer.new('*sessions*'):get()"
    );
    match client::request(path, &Request::Eval { code, name: None }) {
        Ok(Response::Value(text)) => parse_sessions_buffer(&text),
        Ok(Response::Error(reason)) => Err(reason),
        other => Err(format!("{other:?}")),
    }
}

/// Decode Lua's private sessions-buffer header. The body stays ordinary
/// newline-separated styled text; only the row grouping crosses the boundary.
fn parse_sessions_buffer(text: &str) -> Result<(usize, Vec<String>, Vec<String>), String> {
    let mut lines = text.split('\n');
    let rows = lines
        .next()
        .and_then(|line| line.strip_prefix('\x1e'))
        .and_then(|rows| rows.parse::<usize>().ok())
        .filter(|rows| *rows > 0)
        .ok_or_else(|| "invalid sessions-buffer layout contract".to_string())?;
    let body: Vec<_> = lines.collect();
    let (order, body) = match body.first().and_then(|line| line.strip_prefix('\x1f')) {
        Some(names) => (names.split('\t').map(str::to_string).collect(), &body[1..]),
        None => (Vec::new(), body.as_slice()),
    };
    Ok((
        rows,
        body.iter().map(|line| (*line).to_string()).collect(),
        order,
    ))
}

/// Apply Lua's optional ordering header while retaining every live session
/// exactly once. Names absent from the header deliberately keep their daemon
/// order at the end, which makes a partial hook safe.
fn apply_session_order(ui: &mut Ui, selected_name: Option<&str>, order: &[String]) {
    if order.is_empty() {
        return;
    }
    let mut sessions = std::mem::take(&mut ui.sessions);
    let mut ordered = Vec::with_capacity(sessions.len());
    for name in order {
        if let Some(index) = sessions.iter().position(|session| session.name == *name) {
            ordered.push(sessions.remove(index));
        }
    }
    ordered.append(&mut sessions);
    ui.sessions = ordered;
    ui.selected = selected_name
        .and_then(|name| ui.sessions.iter().position(|session| session.name == name))
        .unwrap_or(ui.selected);
    ui.clamp();
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

/// Rows of cells, each row's soft-wrap flag, and the cursor.
type StyledCapture = (Vec<Vec<StyledCell>>, Vec<bool>, Cursor, usize, usize);
type PreviewCapture = (Vec<Vec<StyledCell>>, Vec<bool>, Cursor);

/// Styled counterpart of the (now unused) plain `capture` — see steps/020,
/// 021. The wire carries runs, expanded back to cells here — see steps/022.
/// The cursor rides the same round trip — see steps/027.
fn capture_styled(path: &Path, name: &str, scrollback: usize) -> Result<StyledCapture, String> {
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
            scrollback_len,
            scrollback_total,
            ..
        }) => Ok((
            rows.iter().map(|row| expand_runs(row)).collect(),
            wrapped,
            cursor,
            scrollback_len,
            scrollback_total,
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
#[path = "../tests/tui_support/tui_unit.rs"]
mod tui_unit;
