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
use remuda_core::protocol::{Request, Response};
use remuda_core::registry::SessionSummary;
use remuda_core::Size;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// How often an idle herd is relisted, its focused screen recaptured and the
/// frame rebuilt. A key always forces an immediate refresh regardless of this
/// (see [`should_refresh`]), so this cadence only bounds how quickly the
/// child's own, unprompted output becomes visible.
const TICK: Duration = Duration::from_millis(250);

/// How often the keyboard is polled while a session has focus — shorter than
/// `TICK` so a keypress is never left waiting to be noticed. This used to
/// also be the redraw cadence: every wake of this poll, key or not, re-ran
/// the full (list, capture, render) cycle — an IPC round-trip for a whole
/// screen snapshot among them — so an idle attached session cost ~25 of
/// those a second, forever, and actually typing drove it higher still, one
/// full cycle per keystroke on top of the timer. [`should_refresh`] is the
/// fix: only a key or the slower `TICK` may trigger that cycle now.
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

/// Whether the herd should be relisted, the focused screen recaptured and the
/// frame rebuilt: forced right after a key changed state, or because `TICK`
/// has elapsed since the last time — never on every wake of the input poll.
/// Pure so the rate this bounds can be measured without a terminal or a
/// daemon: see the `tests` module below.
fn should_refresh(forced: bool, since_last: Duration) -> bool {
    forced || since_last >= TICK
}

/// The one expensive act of a frame: relist the herd, take or release the
/// focus hold, capture the focused screen and repaint if that changed
/// anything on screen. Called on `should_refresh`, not on every poll wake.
fn refresh(
    path: &Path,
    server: &str,
    ui: &mut Ui,
    held: &mut Option<(String, Hold)>,
    painted: &mut String,
) -> std::io::Result<(u16, u16)> {
    ui.sessions = list(path);
    ui.clamp();
    ui.follow_focus(held.as_ref().map(|(name, _)| name.as_str()));
    // Dropping the hold is the detach, and this is the only place it
    // happens: focus went back to the list, or the session ended under it.
    if ui.focus == Focus::List {
        *held = None;
    } else if held.is_none() {
        *held = take(path, ui);
    }

    let screen = ui
        .selected()
        .map(|s| capture(path, &s.name))
        .unwrap_or_default();
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let frame = render(ui, &screen, server, cols, rows);
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
    let mut ui = Ui::new(list(path), &shell, notice);
    let mut painted = String::new();
    // The exclusive hold on the focused session, and the name it was taken on.
    // Its `Drop` is the detach, so letting it fall out of scope is the release.
    let mut held: Option<(String, Hold)> = None;
    let mut last_refresh = Instant::now();
    let mut force_refresh = true;
    let (mut cols, mut rows) = (80u16, 24u16);

    loop {
        if should_refresh(force_refresh, last_refresh.elapsed()) {
            force_refresh = false;
            last_refresh = Instant::now();
            (cols, rows) = refresh(path, server, &mut ui, &mut held, &mut painted)?;
        }

        let tick = if ui.focus == Focus::Session {
            TICK_TYPING
        } else {
            TICK
        };
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
        match ui.on_key(key) {
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
                ui.sessions = list(path);
            }
            Action::Kill(name) => ui.notice = kill(path, &name).err(),
        }
        painted.clear();
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

    /// The regression this whole change exists for: before `should_refresh`,
    /// `run` redid the full (list, capture, render) cycle — an IPC round-trip
    /// for a whole screen snapshot among them — on every wake of the input
    /// poll, key or not. With a session focused that poll wakes every
    /// `TICK_TYPING` (40ms), so one idle second was 25 of those cycles.
    /// Simulate that second here, with no key ever pressed, and assert the
    /// count stays down at `TICK`'s cadence (4/s) instead.
    #[test]
    fn an_idle_focused_session_refreshes_on_the_slow_tick_not_every_poll_wake() {
        let mut refreshes = 0;
        let mut since_last = Duration::ZERO;
        for _ in 0..25 {
            since_last += TICK_TYPING;
            if should_refresh(false, since_last) {
                refreshes += 1;
                since_last = Duration::ZERO;
            }
        }
        assert!(
            refreshes <= 5,
            "an idle session must not redo the full IPC cycle on every \
             {TICK_TYPING:?} poll wake — got {refreshes} refreshes in one \
             simulated second; the pre-fix behavior gives 25"
        );
    }

    /// The other half of the report: typing does not wait for `TICK` either.
    /// Echoing a keystroke has always meant an immediate refresh, and that is
    /// still true — it is proportional to typing speed, not the bug.
    #[test]
    fn a_key_forces_an_immediate_refresh_regardless_of_the_slow_tick() {
        assert!(
            should_refresh(true, Duration::ZERO),
            "a keystroke must not wait for TICK to be echoed"
        );
    }

    /// Sanity on the boundary itself, so the two tests above cannot both pass
    /// by accident of a threshold that admits everything or nothing.
    #[test]
    fn refresh_waits_for_the_slow_tick_when_nothing_forced_it() {
        assert!(!should_refresh(false, TICK - Duration::from_millis(1)));
        assert!(should_refresh(false, TICK));
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
}
