//! Read-only, single-node cluster tree. Kept separate from the herd TUI so its
//! navigation and rendering cannot change the existing session workflow.

use crate::client::{self, RawMode};
use composer::{ComposerAction, LineComposer};
use queue::{InputQueue, PendingBatch, QueueEvent, QueueState};
use remuda_core::agent::StyledCell;
use remuda_core::clock::Clock;
use remuda_core::protocol::{expand_runs, Request, Response};
use remuda_core::registry::SessionSummary;
use sender::InputSender;
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

pub mod composer;
pub mod queue;
pub mod sender;

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
    composer: LineComposer,
    composer_focused: bool,
    composer_target: Option<(String, String)>,
    input_sender: InputSender,
    input_queue: InputQueue,
    notice: Option<(String, Duration)>,
    input_hint: Option<(String, Duration)>,
}

impl ClusterUi {
    pub fn new(node: &str, sessions: Vec<SessionSummary>, synced_at: Duration) -> Self {
        Self::with_sender(
            node,
            sessions,
            synced_at,
            InputSender::with_client_id([0; 16]),
        )
    }

    fn with_sender(
        node: &str,
        sessions: Vec<SessionSummary>,
        synced_at: Duration,
        input_sender: InputSender,
    ) -> Self {
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
            composer: LineComposer::default(),
            composer_focused: false,
            composer_target: None,
            input_sender,
            input_queue: InputQueue::default(),
            notice: None,
            input_hint: None,
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
                    session_status(
                        render_badge(if session.alive {
                            Badge::Live
                        } else {
                            Badge::Ended
                        }),
                        self.pending_count(session),
                    )
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
        let queue_rows: Vec<&PendingBatch> = self.input_queue.items().rev().take(3).collect();
        let hint = self
            .input_hint
            .as_ref()
            .filter(|(_, at)| now.saturating_sub(*at) < Duration::from_secs(3));
        let notice = self
            .notice
            .as_ref()
            .filter(|(_, at)| now.saturating_sub(*at) < Duration::from_secs(5));
        let footer_rows = if self.composer_focused { 2 } else { 1 };
        let reserved = frame.len()
            + queue_rows.len()
            + usize::from(hint.is_some())
            + usize::from(notice.is_some())
            + footer_rows;
        let screen_rows = height.saturating_sub(reserved);
        frame.extend(screen.lines().take(screen_rows).map(str::to_string));
        if let Some((hint, _)) = hint {
            frame.push(hint.clone());
        }
        if let Some((notice, _)) = notice {
            frame.push(notice.clone());
        }
        frame.extend(queue_rows.into_iter().rev().map(queue_line));
        if self.composer_focused {
            frame.push(self.composer_line());
            frame.push("Enter send · Ctrl-C clear · Ctrl-\\ list".into());
        } else {
            frame.push(self.footer());
        }
        frame
            .into_iter()
            .map(|line| truncate(&line, width))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[cfg(test)]
    fn key(&mut self, code: crossterm::event::KeyCode) -> bool {
        self.key_event(
            crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
            Duration::ZERO,
        )
    }

    fn key_event(&mut self, event: crossterm::event::KeyEvent, now: Duration) -> bool {
        if self.composer_focused {
            self.handle_composer_event(event, now);
            return false;
        }
        use crossterm::event::KeyModifiers;
        if self.query.is_some() && event.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        use crossterm::event::KeyCode::*;
        match event.code {
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
                let visible = self.visible_sessions();
                if visible.is_empty() || !visible.contains(&self.selected) {
                    return false;
                }
                self.query = None;
                self.expanded = true;
                self.active = self.selected;
                self.composer_focused = true;
                self.bind_composer_target();
            }
            _ => {}
        }
        false
    }

    fn handle_composer_event(&mut self, event: crossterm::event::KeyEvent, now: Duration) {
        if self.composer_target.is_none() {
            self.bind_composer_target();
        }
        match self.composer.handle_key(event) {
            ComposerAction::None => {}
            ComposerAction::Cleared => {
                self.composer_target = None;
                self.notice = Some(("draft cleared".into(), now));
            }
            ComposerAction::Detach => self.composer_focused = false,
            ComposerAction::Submit(bytes) => self.enqueue_draft(bytes, now),
        }
    }

    fn bind_composer_target(&mut self) {
        if self.composer.text().is_empty() {
            self.composer_target = self.sessions.get(self.active).and_then(|session| {
                session
                    .instance_id
                    .as_ref()
                    .map(|instance_id| (session.name.clone(), instance_id.clone()))
            });
        }
    }

    fn enqueue_draft(&mut self, bytes: Vec<u8>, now: Duration) {
        let target = self.composer_target.clone();
        let Some((name, instance_id)) = target else {
            self.restore_draft(&bytes);
            self.notice = Some(("input unavailable: session identity is missing".into(), now));
            return;
        };
        let Some(session) = self.sessions.iter().find(|session| session.name == name) else {
            self.restore_draft(&bytes);
            self.notice = Some(("input unavailable: session is no longer listed".into(), now));
            return;
        };
        if !session.alive {
            self.restore_draft(&bytes);
            self.notice = Some(("input disabled: session ended".into(), now));
            return;
        }
        match self.input_sender.enqueue(
            &mut self.input_queue,
            &name,
            &instance_id,
            bytes.clone(),
            Instant::now(),
        ) {
            Ok(_) => {
                self.notice = None;
                self.composer_target = None;
            }
            Err(error) => {
                self.restore_draft(&bytes);
                self.notice = Some((format!("input not queued: {error}"), now));
            }
        }
    }

    fn restore_draft(&mut self, bytes: &[u8]) {
        let draft = bytes.strip_suffix(b"\r").unwrap_or(bytes);
        if let Ok(text) = std::str::from_utf8(draft) {
            self.composer.restore_draft(text);
        }
    }

    fn start_pending(&mut self, now: Duration) {
        self.input_sender
            .begin_due(&mut self.input_queue, Instant::now());
        if let Some(batch) = self.input_queue.sending_batch() {
            self.notice = Some((format!("sending input to {}", batch.name), now));
        }
    }

    fn send_pending(&mut self, path: &Path, now: Duration) {
        let event =
            self.input_sender
                .send_started(&mut self.input_queue, Instant::now(), |request| {
                    crate::client::request(path, request)
                });
        match event {
            Some(QueueEvent::Sent { .. }) => {
                self.input_hint = Some((format!("input from {}", self.node), now));
            }
            Some(QueueEvent::Uncertain { reason, .. })
            | Some(QueueEvent::Dropped { reason, .. })
            | Some(QueueEvent::Failed { reason, .. }) => {
                if reason.contains("session restarted") {
                    self.composer_target = None;
                }
                self.notice = Some((reason, now));
            }
            Some(QueueEvent::RetryScheduled { .. }) | None => {}
        }
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
                    pending_input: self.pending_count(session) > 0,
                    ..AttentionSignals::default()
                });
                let name_matches = node_matches || matches_query(query, &session.name);
                (!self.attention_only || attention) && name_matches
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn select_first_visible(&mut self) {
        let visible = self.visible_sessions();
        if !visible.contains(&self.selected) {
            if let Some(index) = visible.first().copied() {
                self.selected = index;
            }
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

    fn pending_count(&self, session: &SessionSummary) -> usize {
        self.input_queue
            .items()
            .filter(|batch| {
                batch.name == session.name
                    && matches!(
                        batch.state,
                        QueueState::Waiting | QueueState::Sending | QueueState::Uncertain
                    )
            })
            .count()
    }

    fn footer(&self) -> String {
        let attention = if self.attention_only { "on" } else { "off" };
        match &self.query {
            Some(query) => format!("search: {query} · Esc clear · Enter select"),
            None => format!("↑/↓ move · ←/→ collapse/expand · Enter select · / search · ! attention: {attention} · q detach"),
        }
    }

    fn composer_line(&self) -> String {
        let cursor = self.composer.cursor();
        let before: String = self.composer.text().chars().take(cursor).collect();
        let after: String = self.composer.text().chars().skip(cursor).collect();
        let target = self
            .composer_target
            .as_ref()
            .map_or_else(String::new, |(name, _)| format!("{name}> "));
        format!("$ {target}{before}▏{after}")
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

fn queue_line(batch: &PendingBatch) -> String {
    let bytes = batch.bytes.strip_suffix(b"\r").unwrap_or(&batch.bytes);
    let text = String::from_utf8_lossy(bytes);
    format!("Input {} · {text}", batch.status)
}

fn session_status(status: &str, pending_count: usize) -> String {
    if pending_count == 0 {
        status.into()
    } else {
        format!("{status} · pending input {pending_count}")
    }
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
    let mut ui = ClusterUi::with_sender(node, list(path)?, clock.now(), InputSender::random()?);
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
        ui.send_pending(path, clock.now());
        ui.start_pending(clock.now());
        if crossterm::event::poll(Duration::from_millis(250))? {
            if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
                if key.kind != crossterm::event::KeyEventKind::Press {
                    continue;
                }
                if ui.key_event(key, clock.now()) {
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::queue::QueueState;
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
            instance_id: Some(format!("instance-{name}")),
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
        ui.key_event(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('\\'),
                crossterm::event::KeyModifiers::CONTROL,
            ),
            clock.now(),
        );
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

    #[test]
    fn search_q_does_not_detach_and_control_keys_are_ignored() {
        let clock = ManualClock::new();
        let mut ui = ClusterUi::new("studio", sessions(), clock.now());
        assert!(!ui.key(crossterm::event::KeyCode::Char('/')));
        assert!(!ui.key(crossterm::event::KeyCode::Char('q')));
        ui.key_event(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('c'),
                crossterm::event::KeyModifiers::CONTROL,
            ),
            Duration::ZERO,
        );
        assert_eq!(ui.query.as_deref(), Some("q"));
        let frame = ui.render(80, 24, "", &clock);
        assert!(frame.contains("search: q"));
        assert!(!frame.contains("attention:"));
    }

    #[test]
    fn enter_on_an_empty_search_result_does_nothing() {
        let clock = ManualClock::new();
        let mut ui = ClusterUi::new("studio", sessions(), clock.now());
        ui.key(crossterm::event::KeyCode::Char('/'));
        ui.key(crossterm::event::KeyCode::Char('z'));
        ui.key(crossterm::event::KeyCode::Enter);
        let frame = ui.render(80, 24, "", &clock);
        assert!(frame.contains("search: z"));
        assert!(frame.contains("no matching sessions"));
        assert_eq!(ui.query.as_deref(), Some("z"));
        assert_eq!(ui.active, 0);
    }

    #[test]
    fn enter_with_a_hidden_selection_does_nothing() {
        let clock = ManualClock::new();
        let mut ui = ClusterUi::new(
            "studio",
            vec![session("dev", true), session("shell", true)],
            clock.now(),
        );
        ui.query = Some("dev".into());
        ui.selected = 1;
        ui.active = 1;
        ui.key(crossterm::event::KeyCode::Enter);
        assert_eq!(ui.query.as_deref(), Some("dev"));
        assert_eq!(ui.active, 1);
    }

    #[test]
    fn enter_on_an_empty_attention_filter_does_nothing() {
        let clock = ManualClock::new();
        let mut ui = ClusterUi::new("studio", sessions(), clock.now());
        ui.key(crossterm::event::KeyCode::Char('!'));
        ui.key(crossterm::event::KeyCode::Enter);
        let frame = ui.render(80, 24, "", &clock);
        assert!(frame.contains("attention: on"));
        assert!(frame.contains("no sessions need attention"));
        assert_eq!(ui.active, 0);
    }

    #[test]
    fn toggling_attention_preserves_a_selection_when_it_remains_visible() {
        let clock = ManualClock::new();
        let mut ui = ClusterUi::new(
            "studio",
            vec![session("dev", true), session("shell", true)],
            clock.now(),
        );
        ui.key(crossterm::event::KeyCode::Down);
        ui.key(crossterm::event::KeyCode::Char('!'));
        ui.key(crossterm::event::KeyCode::Char('!'));
        assert_eq!(ui.selected, 1);
        assert!(ui.render(80, 24, "", &clock).contains(">    shell"));
    }

    #[test]
    fn enter_focuses_the_composer_and_submits_to_its_bound_instance() {
        let clock = ManualClock::new();
        let mut ui = ClusterUi::new("studio", vec![session("dev", true)], clock.now());
        ui.key(crossterm::event::KeyCode::Enter);
        assert!(ui.composer_focused);
        ui.key(crossterm::event::KeyCode::Char('q'));
        assert_eq!(ui.composer.text(), "q");
        ui.key_event(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('c'),
                crossterm::event::KeyModifiers::CONTROL,
            ),
            clock.now(),
        );
        assert_eq!(ui.composer.text(), "");
        for character in ['h', 'i'] {
            ui.key(crossterm::event::KeyCode::Char(character));
        }
        ui.key(crossterm::event::KeyCode::Enter);
        let batch = ui.input_queue.items().next().unwrap();
        assert_eq!(batch.name, "dev");
        assert_eq!(batch.instance_id, "instance-dev");
        assert_eq!(batch.seq, 1);
        assert_eq!(batch.bytes, b"hi\r");
        assert_eq!(batch.state, QueueState::Waiting);
        ui.key_event(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('\\'),
                crossterm::event::KeyModifiers::CONTROL,
            ),
            clock.now(),
        );
        ui.key(crossterm::event::KeyCode::Char('!'));
        let frame = ui.render(80, 24, "", &clock);
        assert!(frame.contains("dev          live · pending input 1"));
    }
}
