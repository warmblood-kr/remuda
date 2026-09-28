//! Read-only, single-node cluster tree. Kept separate from the herd TUI so its
//! navigation and rendering cannot change the existing session workflow.

use crate::client::{self, RawMode};
use remuda_core::agent::StyledCell;
use remuda_core::clock::Clock;
use remuda_core::protocol::{expand_runs, Request, Response};
use remuda_core::registry::SessionSummary;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AttentionSignals {
    pub stale: bool,
    pub ended: bool,
    pub pending_input: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Badge {
    Local,
    Reachable,
    Live,
    Ended,
}

pub fn matches_query(query: &str, name: &str) -> bool {
    name.to_lowercase().contains(&query.to_lowercase())
}

pub fn is_attention(signals: AttentionSignals) -> bool {
    signals.stale || signals.ended || signals.pending_input
}

pub fn render_badge(badge: Badge) -> &'static str {
    match badge {
        Badge::Local => "local",
        Badge::Reachable => "reachable",
        Badge::Live => "live",
        Badge::Ended => "ended",
    }
}

pub struct ClusterUi {
    node: String,
    sessions: Vec<SessionSummary>,
    selected: usize,
    active: usize,
    expanded: bool,
    synced_at: Duration,
    snapshot_at: Option<Duration>,
    query: Option<String>,
    attention_only: bool,
}

impl ClusterUi {
    pub fn new(node: &str, sessions: Vec<SessionSummary>, synced_at: Duration) -> Self {
        Self {
            node: node.into(),
            sessions,
            selected: 0,
            active: 0,
            expanded: true,
            synced_at,
            snapshot_at: None,
            query: None,
            attention_only: false,
        }
    }

    pub fn sync_completed(&mut self, at: Duration) {
        self.synced_at = at;
    }

    pub fn capture_completed(&mut self, at: Duration) {
        self.snapshot_at = Some(at);
    }

    pub fn render(&self, cols: u16, rows: u16, screen: &str, clock: &dyn Clock) -> String {
        let width = usize::from(cols.max(1));
        let height = usize::from(rows.max(1));
        let now = clock.now();
        let mut frame = Vec::new();
        frame.push("remuda · cluster (1/1 reachable)".into());
        frame.push(format!(
            "{} {}       {} · {} · sync {}s",
            if self.expanded { "▼" } else { "▶" },
            self.node,
            render_badge(Badge::Local),
            render_badge(Badge::Reachable),
            age_seconds(now, self.synced_at)
        ));
        if self.expanded {
            let visible = self.visible_sessions();
            for index in visible.iter().copied() {
                let session = &self.sessions[index];
                frame.push(format!(
                    "{}{} {:<12} {}",
                    if index == self.selected { ">" } else { " " },
                    "   ",
                    session.name,
                    render_badge(if session.alive {
                        Badge::Live
                    } else {
                        Badge::Ended
                    })
                ));
            }
            if visible.is_empty() {
                frame.push(if self.attention_only {
                    "    no sessions need attention".into()
                } else {
                    "    no matching sessions".into()
                });
            }
        }
        let pane_header = self.sessions.get(self.active).map_or_else(
            || format!("{} · no sessions", self.node),
            |session| {
                format!(
                    "{} / {} · {} · snapshot {}s ago",
                    self.node,
                    session.name,
                    if session.alive { "live" } else { "ended" },
                    age_seconds(now, self.snapshot_at.unwrap_or(now))
                )
            },
        );
        let divider = "─".repeat(width);
        frame.push(divider);
        frame.push(pane_header);
        let screen_rows = height.saturating_sub(frame.len() + 1);
        frame.extend(screen.lines().take(screen_rows).map(str::to_string));
        frame.push(self.footer());
        frame
            .into_iter()
            .map(|line| truncate(&line, width))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn key(&mut self, code: crossterm::event::KeyCode) -> bool {
        use crossterm::event::KeyCode::*;
        match code {
            Char('q') if self.query.is_none() => return true,
            Char('/') if self.query.is_none() => self.query = Some(String::new()),
            Char('!') if self.query.is_none() => {
                self.attention_only = !self.attention_only;
                self.select_first_visible();
            }
            Char(ch) if self.query.is_some() => {
                if let Some(query) = self.query.as_mut() {
                    query.push(ch);
                }
                self.select_first_visible();
            }
            Esc if self.query.is_some() => {
                self.query = None;
                self.select_first_visible();
            }
            Backspace if self.query.is_some() => {
                if let Some(query) = self.query.as_mut() {
                    query.pop();
                }
                self.select_first_visible();
            }
            Up => self.move_selection(false),
            Down => self.move_selection(true),
            Left => self.expanded = false,
            Right => self.expanded = true,
            Enter => {
                self.query = None;
                self.expanded = true;
                self.active = self.selected;
            }
            _ => {}
        }
        false
    }

    fn visible_sessions(&self) -> Vec<usize> {
        let query = self.query.as_deref().unwrap_or_default();
        let node_matches = query.is_empty() || matches_query(query, &self.node);
        self.sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| {
                let attention = is_attention(AttentionSignals {
                    ended: !session.alive,
                    ..AttentionSignals::default()
                });
                let name_matches = node_matches || matches_query(query, &session.name);
                (!self.attention_only || attention) && name_matches
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn select_first_visible(&mut self) {
        if let Some(index) = self.visible_sessions().first().copied() {
            self.selected = index;
        }
    }

    fn move_selection(&mut self, down: bool) {
        let visible = self.visible_sessions();
        let Some(position) = visible.iter().position(|index| *index == self.selected) else {
            self.select_first_visible();
            return;
        };
        let next = if down {
            (position + 1).min(visible.len().saturating_sub(1))
        } else {
            position.saturating_sub(1)
        };
        self.selected = visible[next];
    }

    fn footer(&self) -> String {
        let attention = if self.attention_only { "on" } else { "off" };
        match &self.query {
            Some(query) => format!("search: {query} · Esc clear · Enter select · ! attention: {attention}"),
            None => format!("↑/↓ move · ←/→ collapse/expand · Enter select · / search · ! attention: {attention} · q detach"),
        }
    }

    fn select_target(&mut self, target: Option<&str>) -> io::Result<()> {
        let Some(target) = target else { return Ok(()) };
        let Some((node, session)) = target.split_once('/') else {
            return Err(io::Error::other("target must be node/session"));
        };
        if node != self.node {
            return Err(io::Error::other(format!(
                "node {node} is not available in the local-only tree"
            )));
        }
        self.selected = self
            .sessions
            .iter()
            .position(|item| item.name == session)
            .ok_or_else(|| {
                io::Error::other(format!("session {session} was not found on {node}"))
            })?;
        self.active = self.selected;
        Ok(())
    }
}

fn age_seconds(now: Duration, since: Duration) -> u64 {
    now.saturating_sub(since).as_secs()
}

fn truncate(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

fn list(path: &Path) -> io::Result<Vec<SessionSummary>> {
    match client::request(path, &Request::List).map_err(io::Error::other)? {
        Response::Sessions(sessions) => Ok(sessions),
        Response::Error(error) => Err(io::Error::other(error)),
        other => Err(io::Error::other(format!(
            "unexpected List response: {other:?}"
        ))),
    }
}

fn screen(path: &Path, name: &str, clock: &dyn Clock) -> io::Result<(String, Duration)> {
    let response = client::request(
        path,
        &Request::CaptureStyled {
            name: name.into(),
            scrollback: 0,
        },
    )
    .map_err(io::Error::other)?;
    let captured_at = clock.now();
    match response {
        Response::StyledScreen { rows, .. } => Ok((
            rows.iter()
                .map(|row| {
                    let cells = expand_runs(row);
                    cells
                        .iter()
                        .map(|cell: &StyledCell| cell.text.as_str())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n"),
            captured_at,
        )),
        Response::Error(error) => Err(io::Error::other(error)),
        other => Err(io::Error::other(format!(
            "unexpected CaptureStyled response: {other:?}"
        ))),
    }
}

pub fn run(path: &Path, node: &str, target: Option<&str>) -> io::Result<()> {
    let terminal_mode = RawMode::enable()?;
    let clock = crate::SystemClock::new();
    let result = run_loop(path, node, target, &clock);
    drop(terminal_mode);
    result
}

/// Read one local-only frame through the existing List and CaptureStyled IPC.
pub fn read_frame(
    path: &Path,
    node: &str,
    target: Option<&str>,
    cols: u16,
    rows: u16,
) -> io::Result<String> {
    let clock = crate::SystemClock::new();
    let mut ui = ClusterUi::new(node, list(path)?, clock.now());
    ui.select_target(target)?;
    let body = match ui.sessions.get(ui.active) {
        Some(session) => {
            let (body, captured_at) = screen(path, &session.name, &clock)?;
            ui.capture_completed(captured_at);
            body
        }
        None => String::new(),
    };
    Ok(ui.render(cols, rows, &body, &clock))
}

fn run_loop(path: &Path, node: &str, target: Option<&str>, clock: &dyn Clock) -> io::Result<()> {
    let mut ui = ClusterUi::new(node, list(path)?, clock.now());
    ui.select_target(target)?;
    loop {
        if let Ok(current) = list(path) {
            ui.sessions = current;
            ui.sync_completed(clock.now());
        }
        ui.selected = ui.selected.min(ui.sessions.len().saturating_sub(1));
        ui.active = ui.active.min(ui.sessions.len().saturating_sub(1));
        let captured = ui
            .sessions
            .get(ui.active)
            .and_then(|session| screen(path, &session.name, clock).ok());
        let body = if let Some((captured, captured_at)) = captured {
            ui.capture_completed(captured_at);
            captured
        } else {
            String::new()
        };
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let frame = ui.render(cols, rows, &body, clock);
        crossterm::execute!(
            io::stdout(),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
            crossterm::cursor::MoveTo(0, 0)
        )?;
        write!(io::stdout(), "{frame}")?;
        io::stdout().flush()?;
        if crossterm::event::poll(Duration::from_millis(250))? {
            if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
                if key.kind != crossterm::event::KeyEventKind::Press {
                    continue;
                }
                if ui.key(key.code) {
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{is_attention, render_badge, AttentionSignals, Badge, ClusterUi};
    use remuda_core::clock::{Clock, ManualClock};
    use remuda_core::{SessionSummary, Size};
    use std::time::Duration;

    fn sessions() -> Vec<SessionSummary> {
        vec![SessionSummary {
            id: "session-dev".into(),
            name: "dev".into(),
            alive: true,
            idle: Duration::ZERO,
            output_idle: Some(Duration::ZERO),
            size: Size::new(80, 24),
            attached: false,
            human_idle: None,
            mouse_tracking: false,
            instance_id: None,
            output_version: None,
        }]
    }

    fn session(name: &str, alive: bool) -> SessionSummary {
        SessionSummary {
            id: format!("session-{name}"),
            name: name.into(),
            alive,
            idle: Duration::ZERO,
            output_idle: Some(Duration::ZERO),
            size: Size::new(80, 24),
            attached: false,
            human_idle: None,
            mouse_tracking: false,
            instance_id: None,
            output_version: None,
        }
    }

    #[test]
    fn local_node_is_expanded_and_tree_header_names_selected_target() {
        let clock = ManualClock::new();
        let mut ui = ClusterUi::new("studio", sessions(), clock.now());
        ui.capture_completed(clock.now());
        let frame = ui.render(80, 24, "snapshot", &clock);
        assert!(frame.contains("▼ studio       local"));
        assert!(frame.contains("    dev          live"));
        assert!(frame.contains("studio / dev · live · snapshot 0s ago"));
    }

    #[test]
    fn arrows_move_the_tree_cursor_and_enter_selects_the_session() {
        let mut sessions = sessions();
        sessions.push(SessionSummary {
            id: "session-shell".into(),
            name: "shell".into(),
            alive: true,
            idle: Duration::ZERO,
            output_idle: Some(Duration::ZERO),
            size: Size::new(80, 24),
            attached: false,
            human_idle: None,
            mouse_tracking: false,
            instance_id: None,
            output_version: None,
        });
        let clock = ManualClock::new();
        let mut ui = ClusterUi::new("studio", sessions, clock.now());
        ui.key(crossterm::event::KeyCode::Down);
        assert!(ui
            .render(80, 24, "", &clock)
            .contains("studio / dev · live"));
        ui.key(crossterm::event::KeyCode::Enter);
        assert!(ui
            .render(80, 24, "", &clock)
            .contains("studio / shell · live"));
    }

    #[test]
    fn snapshot_and_sync_ages_follow_the_injected_clock() {
        let clock = ManualClock::new();
        let mut ui = ClusterUi::new("studio", sessions(), clock.now());
        ui.capture_completed(clock.now());
        ui.sync_completed(clock.now());
        clock.advance(Duration::from_secs(5));
        let frame = ui.render(80, 24, "snapshot", &clock);
        assert!(frame.contains("local · reachable · sync 5s"));
        assert!(frame.contains("studio / dev · live · snapshot 5s ago"));
    }

    #[test]
    fn search_render_shows_matching_node_and_sessions() {
        let clock = ManualClock::new();
        let mut ui = ClusterUi::new(
            "studio",
            vec![session("dev", true), session("shell", true)],
            clock.now(),
        );
        ui.key(crossterm::event::KeyCode::Char('/'));
        ui.key(crossterm::event::KeyCode::Char('s'));
        ui.key(crossterm::event::KeyCode::Char('h'));
        let frame = ui.render(80, 24, "", &clock);
        assert!(frame.contains("shell"));
        assert!(!frame.contains("    dev"));
        assert!(frame.contains("search: sh"));

        let mut node_search = ClusterUi::new(
            "studio",
            vec![session("dev", true), session("shell", true)],
            clock.now(),
        );
        node_search.key(crossterm::event::KeyCode::Char('/'));
        node_search.key(crossterm::event::KeyCode::Char('S'));
        let node_frame = node_search.render(80, 24, "", &clock);
        assert!(node_frame.contains("    dev"));
        assert!(node_frame.contains("    shell"));
    }

    #[test]
    fn search_escape_clears_and_enter_selects_match() {
        let clock = ManualClock::new();
        let mut ui = ClusterUi::new(
            "studio",
            vec![session("dev", true), session("shell", true)],
            clock.now(),
        );
        ui.key(crossterm::event::KeyCode::Char('/'));
        ui.key(crossterm::event::KeyCode::Char('s'));
        ui.key(crossterm::event::KeyCode::Char('h'));
        ui.key(crossterm::event::KeyCode::Enter);
        assert!(ui
            .render(80, 24, "", &clock)
            .contains("studio / shell · live"));
        ui.key(crossterm::event::KeyCode::Char('/'));
        ui.key(crossterm::event::KeyCode::Char('d'));
        ui.key(crossterm::event::KeyCode::Backspace);
        let edited = ui.render(80, 24, "", &clock);
        assert!(edited.contains("    dev"));
        assert!(edited.contains("    shell"));
        assert!(edited.contains("search:  ·"));
        ui.key(crossterm::event::KeyCode::Char('z'));
        ui.key(crossterm::event::KeyCode::Esc);
        let frame = ui.render(80, 24, "", &clock);
        assert!(frame.contains("    dev"));
        assert!(frame.contains("    shell"));
        assert!(!frame.contains("search:"));
    }

    #[test]
    fn attention_filter_shows_only_attention_sessions() {
        let clock = ManualClock::new();
        let mut ui = ClusterUi::new(
            "studio",
            vec![session("dev", true), session("ended", false)],
            clock.now(),
        );
        ui.key(crossterm::event::KeyCode::Char('!'));
        let frame = ui.render(80, 24, "", &clock);
        assert!(frame.contains("ended"));
        assert!(!frame.contains("    dev"));
        assert!(frame.contains("attention: on"));
    }

    #[test]
    fn empty_attention_filter_has_clear_message_and_badges_remain_visible() {
        let clock = ManualClock::new();
        let mut ui = ClusterUi::new("studio", sessions(), clock.now());
        ui.key(crossterm::event::KeyCode::Char('!'));
        let frame = ui.render(80, 24, "", &clock);
        assert!(frame.contains("no sessions need attention"));
        assert!(frame.contains("local · reachable"));
        assert!(frame.contains("attention: on"));
    }

    #[test]
    fn badges_show_local_node_and_session_status() {
        let clock = ManualClock::new();
        let ui = ClusterUi::new(
            "studio",
            vec![session("dev", true), session("ended", false)],
            clock.now(),
        );
        let frame = ui.render(80, 24, "", &clock);
        assert!(frame.contains("local · reachable"));
        assert!(frame.contains("dev          live"));
        assert!(frame.contains("ended        ended"));
        assert_eq!(render_badge(Badge::Local), "local");
        assert_eq!(render_badge(Badge::Live), "live");
    }

    #[test]
    fn attention_predicate_accepts_each_documented_signal() {
        assert!(is_attention(AttentionSignals {
            stale: true,
            ..AttentionSignals::default()
        }));
        assert!(is_attention(AttentionSignals {
            ended: true,
            ..AttentionSignals::default()
        }));
        assert!(is_attention(AttentionSignals {
            pending_input: true,
            ..AttentionSignals::default()
        }));
        assert!(!is_attention(AttentionSignals::default()));
    }
}
