//! The picker: `remuda` with no arguments.
//!
//! Two modes, because the owner's sentence — "the right pane shows one session
//! whole, full-screen" — is only consistent as two. **browse** is a list beside
//! a read-only preview and the keys drive remuda; **ride** is the session
//! owning the whole terminal and every key going to the pty. The frame's
//! presence is what tells you which one you are in.
//!
//! Ride mode is `client::attach` unchanged — this module drops its own terminal
//! guard, hands over, and takes it back. There is no second raw-mode mechanism
//! and no privileged path: the TUI is a client speaking the same `Request` a
//! script speaks.
//!
//! [`render`] and [`Ui::on_key`] are pure and take no terminal, which is what
//! keeps the state machine testable on a machine with no tty. Everything that
//! needs one lives in [`run`].

use crate::client::{self, Left, RawMode};
use remuda_core::protocol::{Request, Response};
use remuda_core::registry::SessionSummary;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// How often the preview is re-captured, and the event loop's tick.
const TICK: Duration = Duration::from_millis(250);

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Mode {
    Browse,
    /// The one-line new-session field, holding what has been typed so far.
    Prompt(String),
    /// Kill confirmation for the selected session; only live ones ask.
    Confirm(String),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
    Nothing,
    Quit,
    Ride(String),
    Start(String),
    Kill(String),
}

pub struct Ui {
    pub sessions: Vec<SessionSummary>,
    pub selected: usize,
    pub pan: u16,
    pub mode: Mode,
    /// One line of feedback under the list — a refusal, or how the last ride
    /// ended. Cleared by the next keypress that does anything.
    pub notice: Option<String>,
}

impl Ui {
    pub fn new(sessions: Vec<SessionSummary>, shell: &str, notice: Option<String>) -> Self {
        // An empty herd opens the prompt prefilled rather than showing a blank
        // screen or silently spawning a shell — the latter is the first half of
        // the incident this whole change exists for.
        let mode = if sessions.is_empty() {
            Mode::Prompt(shell.to_string())
        } else {
            Mode::Browse
        };
        Self {
            sessions,
            selected: 0,
            pan: 0,
            mode,
            notice,
        }
    }

    pub fn selected(&self) -> Option<&SessionSummary> {
        self.sessions.get(self.selected)
    }

    /// Put the cursor on a named session, if it is still here. A session that
    /// died and was cleared while you rode it leaves the cursor alone.
    pub fn select_named(&mut self, name: Option<&str>) {
        let Some(name) = name else { return };
        if let Some(at) = self.sessions.iter().position(|s| s.name == name) {
            self.selected = at;
        }
    }

    /// Keep the cursor on a real row after the herd changes underneath it.
    pub fn clamp(&mut self) {
        if self.selected >= self.sessions.len() {
            self.selected = self.sessions.len().saturating_sub(1);
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        match self.mode.clone() {
            Mode::Prompt(buffer) => self.prompt_key(key, buffer),
            Mode::Confirm(name) => self.confirm_key(key, name),
            Mode::Browse => self.browse_key(key),
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
                self.mode = Mode::Prompt(String::new());
                self.notice = None;
                Action::Nothing
            }
            KeyCode::Char('x') => self.kill_selected(),
            KeyCode::Enter => self.ride_selected(),
            _ => Action::Nothing,
        }
    }

    /// Refuse before the screen is handed over, not after: an occupied or dead
    /// session cannot be ridden, and finding that out mid-handover is the shape
    /// of confusion this design keeps deleting.
    fn ride_selected(&mut self) -> Action {
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
        Action::Ride(session.name.clone())
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

/// List width and preview width, divider excluded. The preview claims what the
/// **widest** session needs — from the herd, not the cursor, so the divider does
/// not jump as you move — and the list takes what is left, within 16..=40.
pub fn layout(term_cols: u16, widest: u16) -> (u16, u16) {
    let usable = term_cols.saturating_sub(1);
    let list = usable.saturating_sub(widest).clamp(16, 40).min(usable);
    (list, usable - list)
}

/// The visible rectangle of a screen: the last `rows` lines, each panned by
/// `pan` and cut to `cols`. Bottom-left, because agents left-align and put
/// their input line at the bottom. The flag says whether anything was cut.
pub fn crop(screen: &str, cols: u16, rows: u16, pan: u16) -> (Vec<String>, bool) {
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
            // The marker has to go on here rather than in `fit`: by the time
            // the row is padded there is nothing left to tell it was cut.
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

/// Exactly `width` columns: padded with spaces, or cut with a `→` in the last
/// cell. A crop that looks like absence is the failure mode this repo keeps
/// re-discovering, so a cut always shows.
fn fit(text: &str, width: u16) -> String {
    let width = width as usize;
    let chars: Vec<char> = text.chars().collect();
    if chars.len() > width {
        let mut out: String = chars.into_iter().take(width.saturating_sub(1)).collect();
        out.push('→');
        out
    } else {
        let mut out: String = chars.into_iter().collect();
        out.push_str(&" ".repeat(width - text.chars().count()));
        out
    }
}

/// The whole frame as one string of text and ANSI cursor moves. Pure on
/// purpose: this is the part a test can read without owning a terminal.
pub fn render(ui: &Ui, screen: &str, server: &str, cols: u16, rows: u16) -> String {
    // No herd means nothing to preview, so the list is not squeezed for a pane
    // that would be blank — and the empty-herd sentence fits.
    let widest = ui.sessions.iter().map(|s| s.size.cols()).max().unwrap_or(0);
    let (list_w, preview_w) = layout(cols, widest);
    let body = rows.saturating_sub(1);

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
        out.push('│');
        let line = lines.get(row as usize).map_or("", String::as_str);
        out.push_str(&fit(line, preview_w));
    }

    out.push_str(&format!("\x1b[{};1H", rows));
    out.push_str(&fit(&footer(ui, cut, preview_w), cols));
    out
}

/// A row that degrades instead of being cut. When the preview claims most of
/// the terminal the list can floor at 16 columns, and a truncated row loses
/// `live`/`dead` — the one field the whole list exists to show.
fn list_row(ui: &Ui, row: usize, width: u16) -> String {
    if ui.sessions.is_empty() {
        return if row == 1 {
            "  the herd is empty.".into()
        } else {
            String::new()
        };
    }
    let Some(session) = ui.sessions.get(row) else {
        return String::new();
    };
    let cursor = if row == ui.selected { "▸" } else { " " };
    let flag = if session.attached { "⚑" } else { " " };
    let state = if session.alive { "live" } else { "dead" };
    let tail = match width {
        34.. => format!("{state} {flag} {}s", session.idle.as_secs()),
        22.. => format!("{state} {flag}"),
        _ => flag.to_string(),
    };
    let room = (width as usize).saturating_sub(tail.chars().count() + 3);
    format!("{cursor} {} {tail}", fit(&session.name, room as u16))
}

/// The crop notice moved here when the preview lost its title band: a crop that
/// reads as absence is the failure this repo keeps re-discovering, and the
/// footer is the only band left that is not the session's own screen.
fn footer(ui: &Ui, cut: bool, preview_w: u16) -> String {
    match &ui.mode {
        Mode::Prompt(buffer) => format!("start: {buffer}▏   ⏎ run · esc cancel"),
        Mode::Confirm(name) => format!("kill {name}? it is running — y / n"),
        Mode::Browse => match &ui.notice {
            Some(notice) => format!("remuda: {notice}"),
            None if cut => format!(
                "↑↓ select   ⏎ ride   n new   x kill   showing {preview_w} cols — h/l pans   q quit"
            ),
            None => "↑↓ select   ⏎ ride   n new   x kill   q quit".into(),
        },
    }
}

/// Browse until the user quits or picks a session, then ride it and come back.
/// The guard is dropped inside [`browse`] before handing over, so one owner
/// enters the alternate screen at a time. `notice` is what stderr cannot reach.
pub fn run(path: &Path, server: &str, mut notice: Option<String>) -> std::io::Result<()> {
    let mut last = None;
    loop {
        match browse(path, server, notice.take(), last.as_deref())? {
            None => return Ok(()),
            Some(name) => {
                match client::attach(path, &name) {
                    Ok(Left::Detached) => {}
                    Ok(Left::Exited) => {
                        notice = Some(format!("{name} exited — its screen is kept; x clears it"));
                    }
                    Err(e) => notice = Some(format!("attach {name}: {e}")),
                }
                last = Some(name);
            }
        }
    }
}

fn browse(
    path: &Path,
    server: &str,
    notice: Option<String>,
    select: Option<&str>,
) -> std::io::Result<Option<String>> {
    let _terminal = RawMode::enable()?;
    let shell = crate::daemon::default_shell();
    let mut ui = Ui::new(list(path), &shell, notice);
    // Coming back from a ride lands where you left, not at the top.
    ui.select_named(select);
    let mut painted = String::new();

    loop {
        ui.sessions = list(path);
        ui.clamp();
        let screen = ui
            .selected()
            .map(|s| capture(path, &s.name))
            .unwrap_or_default();
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let frame = render(&ui, &screen, server, cols, rows);
        // Repaint only on change. A hand-rolled draw loop that writes the whole
        // frame four times a second flickers; this is the cheap half of what a
        // diffing renderer would buy, and the reason ratatui is not here yet.
        if frame != painted {
            let mut stdout = std::io::stdout();
            stdout.write_all(frame.as_bytes())?;
            stdout.flush()?;
            painted = frame;
        }

        if !crossterm::event::poll(TICK)? {
            continue;
        }
        let Event::Key(key) = crossterm::event::read()? else {
            continue;
        };
        // Windows delivers Release as well, and acting on both double-fires.
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match ui.on_key(key) {
            Action::Nothing => {}
            Action::Quit => return Ok(None),
            Action::Ride(name) => return Ok(Some(name)),
            Action::Start(command) => {
                ui.notice = start(path, &command).err();
                ui.sessions = list(path);
            }
            Action::Kill(name) => ui.notice = kill(path, &name).err(),
        }
        painted.clear();
    }
}

fn list(path: &Path) -> Vec<SessionSummary> {
    match client::request(path, &Request::List) {
        Ok(Response::Sessions(sessions)) => sessions,
        _ => Vec::new(),
    }
}

fn capture(path: &Path, name: &str) -> String {
    match client::request(
        path,
        &Request::Capture {
            name: name.to_string(),
        },
    ) {
        Ok(Response::Screen(text)) => text,
        _ => String::new(),
    }
}

/// A session's size is the size of the terminal that will ride it, never the
/// size of the pane previewing it — a preview is a transient layout decision
/// and the pty's size is permanent.
fn start(path: &Path, command: &str) -> Result<(), String> {
    let request = Request::New {
        name: None,
        command: command.split_whitespace().map(str::to_string).collect(),
        size: crate::terminal_size(),
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
    fn enter_rides_the_selected_session() {
        let mut ui = ui(vec![row("a", true, false), row("b", true, false)]);
        ui.on_key(press(KeyCode::Char('j')));
        assert_eq!(ui.on_key(press(KeyCode::Enter)), Action::Ride("b".into()));
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
    fn an_empty_herd_opens_the_prompt_prefilled_rather_than_spawning_silently() {
        let ui = ui(vec![]);
        assert_eq!(ui.mode, Mode::Prompt("/bin/sh".into()));
    }

    #[test]
    fn the_prompt_edits_and_starts_what_it_shows() {
        let mut ui = ui(vec![]);
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
        assert_eq!(ui.mode, Mode::Prompt(String::new()));
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
        assert_eq!(ui.mode, Mode::Prompt("q".into()));
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
        assert!(frame.contains("⏎ ride"), "the footer teaches the keys");
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

    #[test]
    fn coming_back_from_a_ride_lands_where_you_left() {
        let mut ui = ui(vec![row("a", true, false), row("b", true, false)]);
        ui.select_named(Some("b"));
        assert_eq!(ui.selected, 1);
        ui.select_named(Some("gone-while-you-were-away"));
        assert_eq!(
            ui.selected, 1,
            "a vanished session does not move the cursor"
        );
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
        assert!(
            list_row(&ui, 0, 40).contains("4s"),
            "idle survives at full width"
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
    fn an_empty_herd_renders_its_own_sentence() {
        let ui = ui(vec![]);
        let frame = render(&ui, "", "default", 80, 10);
        assert!(frame.contains("the herd is empty."));
        assert!(
            frame.contains("start: /bin/sh"),
            "prefilled, and it says what it will run"
        );
    }
}
