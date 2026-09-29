//! Cluster tree for local sessions and read-only remote snapshots. Kept
//! separate from the herd TUI so its navigation cannot change that workflow.

use crate::client::{self, RawMode};
use crate::cluster_remote::{
    ClusterRemoteInput, RemoteInputTransport, RemoteNodeSnapshot,
    RemoteSelection as RemotePollSelection, RemoteSessionSnapshot, RemoteSnapshot, RemoteSource,
    RemoteState,
};
use close_request::confirmed_close;
use composer::{ComposerAction, LineComposer};
use confirm::{Confirmation, Decision};
use ended::EndedState;
use queue::{InputQueue, PendingBatch, QueueEvent, QueueState};
use remuda_core::agent::StyledCell;
use remuda_core::clock::Clock;
use remuda_core::protocol::{expand_runs, Request, Response};
use remuda_core::registry::SessionSummary;
use sender::InputSender;
use std::collections::HashSet;
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

pub mod close_request;
const UI_REQUEST_TIMEOUT: Duration = Duration::from_millis(250);
pub mod composer;
pub mod confirm;
pub mod ended;
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
    pending_close: Option<(String, String)>,
    confirmation: Confirmation,
    ended: Option<EndedState>,
    last_frame: Option<EndedState>,
    remote_snapshot: RemoteSnapshot,
    remote_expanded: HashSet<String>,
    remote_selected: Option<RemoteSelection>,
    remote_active: Option<RemoteSelection>,
    remote_composer_target: Option<(String, String, String)>,
    remote_input_enabled: bool,
    remote_control_disabled: HashSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RemoteSelection {
    Node(String),
    Session {
        node: String,
        name: String,
        instance_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TreeSelection {
    LocalSession(usize),
    RemoteNode(String),
    RemoteSession {
        node: String,
        name: String,
        instance_id: String,
    },
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
            pending_close: None,
            confirmation: Confirmation::default(),
            ended: None,
            last_frame: None,
            remote_snapshot: RemoteSnapshot::default(),
            remote_expanded: HashSet::new(),
            remote_selected: None,
            remote_active: None,
            remote_composer_target: None,
            remote_input_enabled: false,
            remote_control_disabled: HashSet::new(),
        }
    }

    pub fn sync_completed(&mut self, at: Duration) {
        self.synced_at = at;
    }

    fn remote_synced(&mut self, source: &dyn RemoteSource) {
        let selected = self.remote_selected.clone();
        let active = self.remote_active.clone();
        self.remote_snapshot = source.snapshot();
        self.remote_selected = selected.and_then(|selection| match selection {
            RemoteSelection::Node(name) => self
                .remote_snapshot
                .nodes
                .iter()
                .any(|node| node.name == name)
                .then_some(RemoteSelection::Node(name)),
            RemoteSelection::Session {
                node,
                name,
                instance_id,
            } => {
                if self.has_remote_session(&node, &name, &instance_id) {
                    Some(RemoteSelection::Session {
                        node,
                        name,
                        instance_id,
                    })
                } else if self
                    .remote_snapshot
                    .nodes
                    .iter()
                    .any(|snapshot| snapshot.name == node)
                {
                    Some(RemoteSelection::Node(node))
                } else {
                    None
                }
            }
        });
        self.remote_active = active.filter(|selection| match selection {
            RemoteSelection::Session {
                node,
                name,
                instance_id,
            } => self.has_remote_session(node, name, instance_id),
            RemoteSelection::Node(_) => false,
        });
    }

    fn has_remote_session(&self, node: &str, name: &str, instance_id: &str) -> bool {
        self.remote_snapshot
            .nodes
            .iter()
            .find(|snapshot| snapshot.name == node)
            .is_some_and(|snapshot| {
                snapshot
                    .sessions
                    .iter()
                    .any(|session| session.name == name && session.instance_id == instance_id)
            })
    }

    fn remote_session<'a>(
        &'a self,
        selection: &RemoteSelection,
    ) -> Option<(&'a RemoteNodeSnapshot, &'a RemoteSessionSnapshot)> {
        let RemoteSelection::Session {
            node,
            name,
            instance_id,
        } = selection
        else {
            return None;
        };
        let snapshot = self
            .remote_snapshot
            .nodes
            .iter()
            .find(|snapshot| &snapshot.name == node)?;
        let session = snapshot
            .sessions
            .iter()
            .find(|session| &session.name == name && &session.instance_id == instance_id)?;
        Some((snapshot, session))
    }

    fn current_tree_selection(&self) -> TreeSelection {
        match &self.remote_selected {
            Some(RemoteSelection::Node(name)) => TreeSelection::RemoteNode(name.clone()),
            Some(RemoteSelection::Session {
                node,
                name,
                instance_id,
            }) => TreeSelection::RemoteSession {
                node: node.clone(),
                name: name.clone(),
                instance_id: instance_id.clone(),
            },
            None => TreeSelection::LocalSession(self.selected),
        }
    }

    fn set_tree_selection(&mut self, selection: TreeSelection) {
        match selection {
            TreeSelection::LocalSession(index) => {
                self.selected = index;
                self.remote_selected = None;
            }
            TreeSelection::RemoteNode(name) => {
                self.remote_selected = Some(RemoteSelection::Node(name));
            }
            TreeSelection::RemoteSession {
                node,
                name,
                instance_id,
            } => {
                self.remote_selected = Some(RemoteSelection::Session {
                    node,
                    name,
                    instance_id,
                });
            }
        }
    }

    fn visible_tree_rows(&self) -> Vec<TreeSelection> {
        let mut rows = Vec::new();
        if self.expanded {
            rows.extend(
                self.visible_sessions()
                    .into_iter()
                    .map(TreeSelection::LocalSession),
            );
        }
        let query = self.query.as_deref().unwrap_or_default();
        for node in &self.remote_snapshot.nodes {
            let node_matches = query.is_empty() || matches_query(query, &node.name);
            let visible_sessions = node
                .sessions
                .iter()
                .filter(|session| self.remote_session_visible(node, session, query))
                .collect::<Vec<_>>();
            let node_attention = node.state != RemoteState::Reachable;
            let node_visible = (!self.attention_only || node_attention)
                && (node_matches || !visible_sessions.is_empty());
            if !node_visible {
                continue;
            }
            rows.push(TreeSelection::RemoteNode(node.name.clone()));
            if self.remote_expanded.contains(&node.name) {
                rows.extend(visible_sessions.into_iter().map(|session| {
                    TreeSelection::RemoteSession {
                        node: node.name.clone(),
                        name: session.name.clone(),
                        instance_id: session.instance_id.clone(),
                    }
                }));
            }
        }
        rows
    }

    fn remote_session_visible(
        &self,
        node: &RemoteNodeSnapshot,
        session: &RemoteSessionSnapshot,
        query: &str,
    ) -> bool {
        let attention = node.state != RemoteState::Reachable
            || !session.alive
            || self.remote_pending_count(node, session) > 0;
        let name_matches = query.is_empty()
            || matches_query(query, &node.name)
            || matches_query(query, &session.name);
        (!self.attention_only || attention) && name_matches
    }

    fn sessions_synced(&mut self, sessions: Vec<SessionSummary>, at: Duration) {
        let previous = self.sessions.get(self.active).cloned();
        let selected = self
            .sessions
            .get(self.selected)
            .map(|session| (session.name.clone(), session.instance_id.clone()));
        if self.ended.is_none() {
            if let Some(previous) = previous {
                let same_start = sessions.iter().any(|session| {
                    session.name == previous.name && session.instance_id == previous.instance_id
                });
                let restarted = sessions.iter().any(|session| {
                    session.name == previous.name && session.instance_id != previous.instance_id
                });
                if !same_start && restarted {
                    self.notice = Some((
                        format!("{} restarted; the old session ended", previous.name),
                        at,
                    ));
                    if self
                        .composer_target
                        .as_ref()
                        .is_some_and(|(name, instance_id)| {
                            name == &previous.name
                                && Some(instance_id.as_str()) == previous.instance_id.as_deref()
                        })
                    {
                        self.composer_target = None;
                    }
                } else if !same_start {
                    let cached = self.last_frame.as_ref().filter(|frame| {
                        frame.summary.name == previous.name
                            && frame.summary.instance_id == previous.instance_id
                    });
                    if let Some(frame) = cached {
                        self.set_ended(frame.clone());
                    }
                }
            }
        }
        self.sessions = sessions;
        if let Some(ended) = &self.ended {
            let mut summary = ended.summary.clone();
            summary.alive = false;
            summary.attached = false;
            self.sessions.push(summary);
            let ended_index = self.sessions.len() - 1;
            self.selected = selected
                .and_then(|(name, instance_id)| {
                    self.sessions.iter().position(|session| {
                        session.name == name && session.instance_id == instance_id
                    })
                })
                .unwrap_or(ended_index);
            self.active = ended_index;
        } else {
            self.selected = self.selected.min(self.sessions.len().saturating_sub(1));
            self.active = self.active.min(self.sessions.len().saturating_sub(1));
        }
        self.sync_completed(at);
    }

    fn set_ended(&mut self, ended: EndedState) {
        let summary = ended.ended_summary();
        self.sessions.retain(|session| {
            session.name != summary.name || session.instance_id != summary.instance_id
        });
        self.sessions.push(summary);
        self.selected = self.sessions.len() - 1;
        self.active = self.selected;
        self.composer_target = None;
        self.composer_focused = false;
        self.ended = Some(ended);
    }

    fn clear_ended(&mut self) {
        if let Some(ended) = self.ended.take() {
            self.sessions.retain(|session| {
                session.name != ended.summary.name
                    || session.instance_id != ended.summary.instance_id
            });
        }
        self.selected = self.selected.min(self.sessions.len().saturating_sub(1));
        self.active = self.active.min(self.sessions.len().saturating_sub(1));
        self.last_frame = None;
    }

    fn send_close_pending(&mut self, path: &Path, now: Duration) {
        let Some((name, instance_id)) = self.pending_close.take() else {
            return;
        };
        let response = client::request_with_timeout(
            path,
            &confirmed_close(name.clone(), instance_id.clone()),
            UI_REQUEST_TIMEOUT,
        );
        self.close_completed(name, instance_id, response, now);
    }

    fn close_completed(
        &mut self,
        name: String,
        instance_id: String,
        response: io::Result<Response>,
        now: Duration,
    ) {
        match response {
            Ok(Response::Ok) => {
                if let Some(frame) = self.last_frame.as_ref().filter(|frame| {
                    frame.summary.name == name
                        && frame.summary.instance_id.as_deref() == Some(instance_id.as_str())
                }) {
                    self.set_ended(frame.clone());
                } else {
                    self.notice = Some(("session closed; final frame was unavailable".into(), now));
                }
            }
            Ok(Response::Error(reason)) => self.notice = Some((reason, now)),
            Ok(other) => self.notice = Some((format!("unexpected Close response: {other:?}"), now)),
            Err(error) => self.notice = Some((format!("Close failed: {error}"), now)),
        }
    }

    pub fn capture_completed(&mut self, at: Duration) {
        self.snapshot_at = Some(at);
    }

    fn record_capture(&mut self, summary: &SessionSummary, screen: String, at: Duration) -> bool {
        let Some(instance_id) = summary.instance_id.as_deref() else {
            return false;
        };
        if self
            .sessions
            .iter()
            .find(|session| session.name == summary.name)
            .and_then(|session| session.instance_id.as_deref())
            != Some(instance_id)
        {
            return false;
        }
        self.snapshot_at = Some(at);
        self.last_frame = Some(EndedState::from_capture(summary.clone(), screen, at));
        true
    }

    pub fn render(&self, cols: u16, rows: u16, screen: &str, clock: &dyn Clock) -> String {
        let width = usize::from(cols.max(1));
        let height = usize::from(rows.max(1));
        let now = clock.now();
        let mut frame = Vec::new();
        let reachable = 1 + self
            .remote_snapshot
            .nodes
            .iter()
            .filter(|node| node.state == RemoteState::Reachable)
            .count();
        frame.push(format!(
            "remuda · cluster ({reachable}/{} reachable)",
            self.remote_snapshot.nodes.len() + 1
        ));
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
        self.append_remote_tree(&mut frame);
        let (pane_header, pane_body) = self.remote_pane(self.local_pane_header(now), screen);
        let divider = "─".repeat(width);
        frame.push(divider);
        frame.push(pane_header);
        let queue_rows: Vec<&PendingBatch> = self.input_queue.items().rev().take(3).collect();
        let notice = self
            .notice
            .as_ref()
            .filter(|(_, at)| now.saturating_sub(*at) < Duration::from_secs(5));
        let footer_rows = if self.composer_focused { 2 } else { 1 };
        let reserved = frame.len() + queue_rows.len() + usize::from(notice.is_some()) + footer_rows;
        let screen_rows = height.saturating_sub(reserved);
        frame.extend(pane_body.lines().take(screen_rows).map(str::to_string));
        if let Some((notice, _)) = notice {
            frame.push(notice.clone());
        }
        frame.extend(queue_rows.into_iter().rev().map(queue_line));
        if let Some(prompt) = self.confirmation.prompt() {
            frame.push(prompt);
        } else if self.remote_active.is_some() && self.composer_focused {
            frame.push(self.composer_line());
            frame.push("Enter send · Ctrl-C clear · Ctrl-\\ list".into());
        } else if self.remote_active.is_some() {
            frame.push("Remote session is read-only · q detach".into());
        } else if self.ended.is_some() {
            frame.push("Input is disabled · x clears ended session · q detaches".into());
        } else if self.composer_focused {
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

    fn append_remote_tree(&self, frame: &mut Vec<String>) {
        let query = self.query.as_deref().unwrap_or_default();
        for node in &self.remote_snapshot.nodes {
            let node_matches = query.is_empty() || matches_query(query, &node.name);
            let visible_sessions = node
                .sessions
                .iter()
                .filter(|session| self.remote_session_visible(node, session, query))
                .collect::<Vec<_>>();
            let node_attention = node.state != RemoteState::Reachable;
            if (self.attention_only && !node_attention && visible_sessions.is_empty())
                || (!node_matches && visible_sessions.is_empty())
            {
                continue;
            }
            let is_expanded = self.remote_expanded.contains(&node.name);
            let is_selected =
                self.remote_selected == Some(RemoteSelection::Node(node.name.clone()));
            frame.push(format!(
                "{}{} {} · {}",
                if is_selected { ">" } else { " " },
                if is_expanded { "▼" } else { "▶" },
                node.name,
                remote_state_label(node.state, node.last_sync_age, node.last_error.as_deref())
            ));
            if is_expanded {
                for session in visible_sessions {
                    let is_selected = self.remote_selected
                        == Some(RemoteSelection::Session {
                            node: node.name.clone(),
                            name: session.name.clone(),
                            instance_id: session.instance_id.clone(),
                        });
                    frame.push(format!(
                        "{}    {:<12} {}{}",
                        if is_selected { ">" } else { " " },
                        session.name,
                        session_status(
                            if session.alive { "live" } else { "ended" },
                            self.remote_pending_count(node, session),
                        ),
                        session_error_suffix(session.last_error.as_deref())
                    ));
                }
            }
        }
    }

    fn remote_pane(&self, local_header: String, local_screen: &str) -> (String, String) {
        match &self.remote_active {
            Some(active) => match self.remote_session(active) {
                Some((node, session)) => (
                    format!(
                        "{} / {} · remote {} · {}",
                        node.name,
                        session.name,
                        if session.alive { "live" } else { "ended" },
                        remote_state_label(
                            node.state,
                            node.last_sync_age,
                            node.last_error.as_deref()
                        )
                    ) + &session_error_suffix(session.last_error.as_deref()),
                    session
                        .screen
                        .as_ref()
                        .map(remote_screen_text)
                        .unwrap_or_default(),
                ),
                None => (local_header, local_screen.into()),
            },
            None => (local_header, local_screen.into()),
        }
    }

    fn local_pane_header(&self, now: Duration) -> String {
        self.ended.as_ref().map_or_else(
            || {
                self.sessions.get(self.active).map_or_else(
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
                )
            },
            |ended| {
                format!(
                    "{} / {} · ended · final snapshot {}s ago",
                    self.node,
                    ended.summary.name,
                    age_seconds(now, ended.captured_at)
                )
            },
        )
    }

    #[cfg(test)]
    fn key(&mut self, code: crossterm::event::KeyCode) -> bool {
        self.key_event(
            crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
            Duration::ZERO,
        )
    }

    fn key_event(&mut self, event: crossterm::event::KeyEvent, now: Duration) -> bool {
        if let Some(decision) = self.confirmation.handle(event.code) {
            match decision {
                Decision::Confirmed { name, instance_id } => {
                    self.pending_close = Some((name.clone(), instance_id));
                    self.notice = Some((format!("closing {name}"), now));
                }
                Decision::Cancelled => self.notice = Some(("close cancelled".into(), now)),
            }
            return false;
        }
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
            Char('x') if self.query.is_none() => self.handle_close_key(now),
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
            Left => self.collapse_tree_selection(),
            Right => self.expand_tree_selection(),
            Enter => self.enter_selected(now),
            _ => {}
        }
        false
    }

    fn handle_close_key(&mut self, now: Duration) {
        if self.ended.is_some() {
            self.clear_ended();
        } else if self.remote_selected.is_some() {
            self.notice = Some(("remote sessions are read-only".into(), now));
        } else if let Some(session) = self.sessions.get(self.selected) {
            if session.alive {
                if let Some(instance_id) = session.instance_id.clone() {
                    self.active = self.selected;
                    self.confirmation.begin(session.name.clone(), instance_id);
                } else {
                    self.notice = Some(("cannot close: session identity is missing".into(), now));
                }
            } else {
                self.notice = Some((format!("{} has already ended.", session.name), now));
            }
        }
    }

    fn enter_selected(&mut self, now: Duration) {
        let selection = self.current_tree_selection();
        if !self.visible_tree_rows().contains(&selection) {
            return;
        }
        self.discard_remote_draft_if_switching(&selection, now);
        match selection {
            TreeSelection::RemoteNode(name) => {
                self.remote_expanded.insert(name);
            }
            TreeSelection::RemoteSession {
                node,
                name,
                instance_id,
            } => {
                self.query = None;
                self.composer_target = None;
                let can_send = self.remote_input_enabled
                    && self
                        .remote_session(&RemoteSelection::Session {
                            node: node.clone(),
                            name: name.clone(),
                            instance_id: instance_id.clone(),
                        })
                        .is_some_and(|(_, session)| session.alive);
                self.remote_active = Some(RemoteSelection::Session {
                    node: node.clone(),
                    name: name.clone(),
                    instance_id: instance_id.clone(),
                });
                self.composer_focused = can_send;
                self.remote_composer_target = can_send.then_some((node, name, instance_id));
            }
            TreeSelection::LocalSession(selected) => {
                if self
                    .sessions
                    .get(selected)
                    .is_some_and(|session| !session.alive)
                {
                    return;
                }
                self.query = None;
                self.expanded = true;
                self.active = selected;
                self.remote_active = None;
                self.remote_composer_target = None;
                self.composer_focused = true;
                self.bind_composer_target();
            }
        }
    }

    fn handle_composer_event(&mut self, event: crossterm::event::KeyEvent, now: Duration) {
        if self.composer_target.is_none() && self.remote_composer_target.is_none() {
            self.bind_composer_target();
        }
        match self.composer.handle_key(event) {
            ComposerAction::None => {}
            ComposerAction::Cleared => {
                if self.remote_composer_target.is_none() {
                    self.composer_target = None;
                }
                self.notice = Some(("draft cleared".into(), now));
            }
            ComposerAction::Detach => self.composer_focused = false,
            ComposerAction::Submit(bytes) => {
                if self.remote_composer_target.is_some() {
                    self.enqueue_remote_draft(bytes, now);
                } else {
                    self.enqueue_draft(bytes, now);
                }
            }
        }
    }

    fn discard_remote_draft_if_switching(&mut self, selection: &TreeSelection, now: Duration) {
        if matches!(selection, TreeSelection::RemoteNode(_)) {
            return;
        }
        let Some((node, name, instance_id)) = self.remote_composer_target.clone() else {
            return;
        };
        if self.composer.text().is_empty() {
            return;
        }
        let same_target = matches!(selection,
            TreeSelection::RemoteSession {
                node: selected_node,
                name: selected_name,
                instance_id: selected_instance,
            } if selected_node == &node && selected_name == &name && selected_instance == &instance_id
        );
        if !same_target {
            self.composer.clear();
            self.remote_composer_target = None;
            self.notice = Some((format!("draft for {node}/{name} discarded"), now));
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
        if session.instance_id.as_deref() != Some(instance_id.as_str()) {
            self.restore_draft(&bytes);
            self.notice = Some(("input disabled: session restarted".into(), now));
            return;
        }
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

    fn enqueue_remote_draft(&mut self, bytes: Vec<u8>, now: Duration) {
        let Some((node, name, instance_id)) = self.remote_composer_target.clone() else {
            return;
        };
        if self.remote_control_disabled.contains(&node) {
            self.restore_draft(&bytes);
            self.notice = Some((format!("remote control disabled on {node}"), now));
            return;
        }
        let selection = RemoteSelection::Session {
            node: node.clone(),
            name: name.clone(),
            instance_id: instance_id.clone(),
        };
        let Some((_, session)) = self.remote_session(&selection) else {
            self.restore_draft(&bytes);
            self.notice = Some((
                format!("remote input unavailable: {node}/{name} is no longer listed"),
                now,
            ));
            return;
        };
        let wire_name = session.wire_name.clone();
        if !session.alive {
            self.restore_draft(&bytes);
            self.notice = Some((
                format!("remote input disabled: {node}/{name} has ended"),
                now,
            ));
            return;
        }
        match self.input_sender.enqueue_remote(
            &mut self.input_queue,
            &node,
            &wire_name,
            &instance_id,
            bytes.clone(),
            Instant::now(),
        ) {
            Ok(_) => self.notice = None,
            Err(error) => {
                self.restore_draft(&bytes);
                self.notice = Some((format!("remote input not queued: {error}"), now));
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

    fn send_pending(
        &mut self,
        path: &Path,
        now: Duration,
        remote_input: Option<&dyn RemoteInputTransport>,
    ) {
        let remote_node = self
            .input_queue
            .sending_batch()
            .and_then(|batch| batch.remote_node);
        let event = if remote_node.is_some() {
            match remote_input {
                Some(remote_input) => self.input_sender.send_remote_started(
                    &mut self.input_queue,
                    Instant::now(),
                    |node, request| remote_input.send_input(node, request),
                ),
                None => self.input_sender.send_remote_started(
                    &mut self.input_queue,
                    Instant::now(),
                    |_, _| Err(io::Error::other("remote input transport is unavailable")),
                ),
            }
        } else {
            self.input_sender
                .send_started(&mut self.input_queue, Instant::now(), |request| {
                    crate::client::request_with_timeout(path, request, sender::INPUT_SEND_TIMEOUT)
                })
        };
        self.handle_send_event(event, now);
    }

    fn handle_send_event(&mut self, event: Option<QueueEvent>, now: Duration) {
        match event {
            Some(QueueEvent::Sent { .. } | QueueEvent::Uncertain { .. }) => {
                self.clear_sending_notice();
            }
            Some(QueueEvent::Dropped { reason, .. }) | Some(QueueEvent::Failed { reason, .. }) => {
                if reason.contains("session restarted") {
                    if self.remote_composer_target.is_some() {
                        self.remote_composer_target = None;
                        self.composer_focused = false;
                    } else {
                        self.composer_target = None;
                    }
                }
                self.notice = Some((reason, now));
            }
            Some(QueueEvent::RemoteControlDisabled { node, .. }) => {
                self.remote_control_disabled.insert(node.clone());
                self.notice = Some((format!("remote control disabled on {node}"), now));
            }
            Some(QueueEvent::RetryScheduled { .. }) | None => {}
        }
    }

    fn clear_sending_notice(&mut self) {
        if self
            .notice
            .as_ref()
            .is_some_and(|(notice, _)| notice.starts_with("sending input to "))
        {
            self.notice = None;
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
        let visible = self.visible_tree_rows();
        if !visible.contains(&self.current_tree_selection()) {
            if let Some(selection) = visible.first().cloned() {
                self.set_tree_selection(selection);
            }
        }
    }

    fn move_selection(&mut self, down: bool) {
        let visible = self.visible_tree_rows();
        let current = self.current_tree_selection();
        let Some(position) = visible.iter().position(|selection| *selection == current) else {
            self.select_first_visible();
            return;
        };
        let next = if down {
            (position + 1).min(visible.len().saturating_sub(1))
        } else {
            position.saturating_sub(1)
        };
        self.set_tree_selection(visible[next].clone());
    }

    fn collapse_tree_selection(&mut self) {
        match self.current_tree_selection() {
            TreeSelection::LocalSession(_) => self.expanded = false,
            TreeSelection::RemoteNode(node) => {
                self.remote_expanded.remove(&node);
            }
            TreeSelection::RemoteSession { node, .. } => {
                self.remote_expanded.remove(&node);
                self.set_tree_selection(TreeSelection::RemoteNode(node));
            }
        }
    }

    fn expand_tree_selection(&mut self) {
        match self.current_tree_selection() {
            TreeSelection::LocalSession(_) => self.expanded = true,
            TreeSelection::RemoteNode(node) => {
                self.remote_expanded.insert(node);
            }
            TreeSelection::RemoteSession { node, .. } => {
                self.remote_expanded.insert(node);
            }
        }
    }

    fn pending_count(&self, session: &SessionSummary) -> usize {
        self.input_queue
            .items()
            .filter(|batch| {
                batch.name == session.name
                    && matches!(batch.state, QueueState::Waiting | QueueState::Sending)
            })
            .count()
    }

    fn remote_pending_count(
        &self,
        node: &RemoteNodeSnapshot,
        session: &RemoteSessionSnapshot,
    ) -> usize {
        self.input_queue
            .items()
            .filter(|batch| {
                batch.remote_node.as_deref() == Some(node.name.as_str())
                    && batch.name == session.wire_name
                    && batch.instance_id == session.instance_id
                    && matches!(batch.state, QueueState::Waiting | QueueState::Sending)
            })
            .count()
    }

    fn footer(&self) -> String {
        let attention = if self.attention_only { "on" } else { "off" };
        match &self.query {
            Some(query) => format!("search: {query} · Esc clear · Enter select"),
            None => format!("↑/↓ move · ←/→ tree · Enter · x close · / search · ! attention: {attention} · q detach"),
        }
    }

    fn composer_line(&self) -> String {
        let cursor = self.composer.cursor();
        let before: String = self.composer.text().chars().take(cursor).collect();
        let after: String = self.composer.text().chars().skip(cursor).collect();
        let target = self.composer_target.as_ref().map_or_else(
            || {
                self.remote_composer_target
                    .as_ref()
                    .map_or_else(String::new, |(node, name, _)| format!("{node}/{name}> "))
            },
            |(name, _)| format!("{name}> "),
        );
        format!("$ {target}{before}▏{after}")
    }

    fn select_target(&mut self, target: Option<&str>) -> io::Result<()> {
        let Some(target) = target else { return Ok(()) };
        let Some((node, session)) = target.split_once('/') else {
            return Err(io::Error::other("target must be node/session"));
        };
        if node != self.node {
            let remote = self
                .remote_snapshot
                .nodes
                .iter()
                .find(|snapshot| snapshot.name == node)
                .ok_or_else(|| io::Error::other(format!("node {node} was not found")))?;
            let session = remote
                .sessions
                .iter()
                .find(|item| item.name == session)
                .ok_or_else(|| {
                    io::Error::other(format!("session {session} was not found on {node}"))
                })?;
            let selection = RemoteSelection::Session {
                node: node.into(),
                name: session.name.clone(),
                instance_id: session.instance_id.clone(),
            };
            self.remote_expanded.insert(node.into());
            self.remote_selected = Some(selection.clone());
            self.remote_active = Some(selection);
            return Ok(());
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

fn remote_state_label(
    state: RemoteState,
    last_sync_age: Option<Duration>,
    last_error: Option<&str>,
) -> String {
    let reason = last_error.map(|error| {
        if error.contains("supply --addr") {
            "no address known".to_owned()
        } else {
            crate::text::strip_terminal_controls(error)
                .chars()
                .take(80)
                .collect()
        }
    });
    match state {
        RemoteState::Reachable => "reachable".into(),
        RemoteState::Stale => last_sync_age.map_or_else(
            || {
                format!(
                    "stale · sync age unknown{}",
                    reason_suffix(reason.as_deref())
                )
            },
            |age| {
                format!(
                    "stale · sync {}s ago{}",
                    age.as_secs(),
                    reason_suffix(reason.as_deref())
                )
            },
        ),
        RemoteState::Reconnecting => last_sync_age.map_or_else(
            || format!("reconnecting{}", reason_suffix(reason.as_deref())),
            |age| {
                format!(
                    "reconnecting · last sync {}s ago{}",
                    age.as_secs(),
                    reason_suffix(reason.as_deref())
                )
            },
        ),
        RemoteState::Unreachable => format!("unreachable{}", reason_suffix(reason.as_deref())),
    }
}

fn reason_suffix(reason: Option<&str>) -> String {
    reason.map_or_else(String::new, |reason| format!(" · {reason}"))
}

fn session_error_suffix(error: Option<&str>) -> String {
    error.map_or_else(String::new, |error| {
        let safe: String = crate::text::strip_terminal_controls(error)
            .chars()
            .take(80)
            .collect();
        format!(" · {safe}")
    })
}

fn remote_screen_text(screen: &remuda_core::agent::ScreenSnapshot) -> String {
    screen
        .cells
        .iter()
        .map(|row| {
            row.iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct EmptyRemoteSource;

impl RemoteSource for EmptyRemoteSource {
    fn snapshot(&self) -> RemoteSnapshot {
        RemoteSnapshot::default()
    }
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
    match client::request_with_timeout(path, &Request::List, UI_REQUEST_TIMEOUT)
        .map_err(io::Error::other)?
    {
        Response::Sessions(sessions) => Ok(sessions),
        Response::Error(error) => Err(io::Error::other(error)),
        other => Err(io::Error::other(format!(
            "unexpected List response: {other:?}"
        ))),
    }
}

fn screen(
    path: &Path,
    name: &str,
    clock: &dyn Clock,
) -> io::Result<(String, Duration, Option<String>)> {
    let response = client::request_with_timeout(
        path,
        &Request::CaptureStyled {
            name: name.into(),
            scrollback: 0,
        },
        UI_REQUEST_TIMEOUT,
    )
    .map_err(io::Error::other)?;
    let captured_at = clock.now();
    match response {
        Response::StyledScreen {
            rows, instance_id, ..
        } => Ok((
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
            instance_id,
        )),
        Response::Error(error) => Err(io::Error::other(error)),
        other => Err(io::Error::other(format!(
            "unexpected CaptureStyled response: {other:?}"
        ))),
    }
}

/// Run the local tree. Call [`run_with_remote_source`] when a transport-backed
/// source is available to show remote nodes as well.
pub fn run(path: &Path, node: &str, target: Option<&str>) -> io::Result<()> {
    run_with_remote_source(path, node, target, &EmptyRemoteSource)
}

/// Run the cluster tree with a remote snapshot source and the cluster Input client.
/// Its snapshot method must return promptly; polling belongs outside the TUI loop.
pub fn run_with_remote_source(
    path: &Path,
    node: &str,
    target: Option<&str>,
    source: &dyn RemoteSource,
) -> io::Result<()> {
    let remote_input = ClusterRemoteInput::system().ok();
    let remote_input = remote_input
        .as_ref()
        .map(|input| input as &dyn RemoteInputTransport);
    run_with_remote_selection_and_input(
        path,
        node,
        target,
        source,
        &RemotePollSelection::default(),
        remote_input,
    )
}

/// Run the cluster tree with a read-only snapshot source and a separate
/// selection signal for transport-owned polling.
pub fn run_with_remote_selection(
    path: &Path,
    node: &str,
    target: Option<&str>,
    source: &dyn RemoteSource,
    selection: &RemotePollSelection,
) -> io::Result<()> {
    let remote_input = ClusterRemoteInput::system().ok();
    let remote_input = remote_input
        .as_ref()
        .map(|input| input as &dyn RemoteInputTransport);
    run_with_remote_selection_and_input(path, node, target, source, selection, remote_input)
}

/// Run the cluster tree with remote snapshots and optional remote input.
pub fn run_with_remote_input(
    path: &Path,
    node: &str,
    target: Option<&str>,
    source: &dyn RemoteSource,
    remote_input: Option<&dyn RemoteInputTransport>,
) -> io::Result<()> {
    run_with_remote_selection_and_input(
        path,
        node,
        target,
        source,
        &RemotePollSelection::default(),
        remote_input,
    )
}

/// Run the cluster tree with separate remote polling and input transports.
pub fn run_with_remote_selection_and_input(
    path: &Path,
    node: &str,
    target: Option<&str>,
    source: &dyn RemoteSource,
    selection: &RemotePollSelection,
    remote_input: Option<&dyn RemoteInputTransport>,
) -> io::Result<()> {
    let terminal_mode = RawMode::enable()?;
    let clock = crate::SystemClock::new();
    let result = run_loop(path, node, target, &clock, source, selection, remote_input);
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
            let (body, captured_at, _) = screen(path, &session.name, &clock)?;
            ui.capture_completed(captured_at);
            body
        }
        None => String::new(),
    };
    Ok(ui.render(cols, rows, &body, &clock))
}

fn run_loop(
    path: &Path,
    node: &str,
    target: Option<&str>,
    clock: &dyn Clock,
    remote_source: &dyn RemoteSource,
    remote_selection: &RemotePollSelection,
    remote_input: Option<&dyn RemoteInputTransport>,
) -> io::Result<()> {
    let mut ui = ClusterUi::with_sender(node, list(path)?, clock.now(), InputSender::random()?);
    ui.remote_input_enabled = remote_input.is_some();
    ui.remote_synced(remote_source);
    select_target_with_remote_wait(&mut ui, target, remote_source, Duration::from_secs(5))?;
    if ui.remote_input_enabled {
        if let Some(RemoteSelection::Session {
            node,
            name,
            instance_id,
        }) = ui.remote_active.clone()
        {
            ui.remote_composer_target = Some((node, name, instance_id));
            ui.composer_focused = true;
        }
    }
    loop {
        if let Ok(current) = list(path) {
            ui.sessions_synced(current, clock.now());
        }
        ui.remote_synced(remote_source);
        if let Some(RemoteSelection::Session { node, name, .. }) = &ui.remote_active {
            remote_selection.select(node.clone(), name.clone());
        } else {
            remote_selection.clear();
        }
        ui.selected = ui.selected.min(ui.sessions.len().saturating_sub(1));
        ui.active = ui.active.min(ui.sessions.len().saturating_sub(1));
        let captured = ui
            .ended
            .as_ref()
            .map(|ended| ended.screen.clone())
            .or_else(|| {
                if ui.remote_active.is_some() {
                    return None;
                }
                let session = ui.sessions.get(ui.active)?.clone();
                let (captured, captured_at, instance_id) =
                    screen(path, &session.name, clock).ok()?;
                if instance_id == session.instance_id
                    && ui.record_capture(&session, captured.clone(), captured_at)
                {
                    Some(captured)
                } else {
                    ui.notice = Some((
                        format!("{} restarted; screen was not refreshed", session.name),
                        clock.now(),
                    ));
                    None
                }
            });
        let body = captured.unwrap_or_default();
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let frame = ui.render(cols, rows, &body, clock);
        crossterm::execute!(
            io::stdout(),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
            crossterm::cursor::MoveTo(0, 0)
        )?;
        write!(io::stdout(), "{frame}")?;
        io::stdout().flush()?;
        ui.send_close_pending(path, clock.now());
        ui.send_pending(path, clock.now(), remote_input);
        ui.start_pending(clock.now());
        if crossterm::event::poll(Duration::from_millis(250))? {
            if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
                if key.kind != crossterm::event::KeyEventKind::Press {
                    continue;
                }
                if ui.key_event(key, clock.now()) {
                    return Ok(());
                }
                ui.send_close_pending(path, clock.now());
            }
        }
    }
}

fn select_target_with_remote_wait(
    ui: &mut ClusterUi,
    target: Option<&str>,
    source: &dyn RemoteSource,
    timeout: Duration,
) -> io::Result<()> {
    let Some(target) = target else { return Ok(()) };
    let Some((node, _)) = target.split_once('/') else {
        return ui.select_target(Some(target));
    };
    match ui.select_target(Some(target)) {
        Ok(()) => Ok(()),
        Err(error) if node != ui.node => {
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                ui.remote_synced(source);
                if ui.select_target(Some(target)).is_ok() {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::queue::{QueueEvent, QueueState};
    use super::{
        is_attention, remote_state_label, render_badge, select_target_with_remote_wait,
        AttentionSignals, Badge, ClusterUi, RemoteInputTransport, RemoteSelection, RemoteSource,
    };
    use crate::cluster_remote::{
        RemoteNodeSnapshot, RemoteSessionSnapshot, RemoteSnapshot, RemoteState,
    };
    use remuda_core::agent::{Color, Cursor, ScreenSnapshot, StyledCell};
    use remuda_core::clock::{Clock, ManualClock};
    use remuda_core::protocol::{Request, Response};
    use remuda_core::{SessionSummary, Size};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    struct FakeRemoteSource(Mutex<RemoteSnapshot>);

    struct FakeRemoteInput {
        response: Mutex<Option<Response>>,
        requests: Mutex<Vec<(String, Request)>>,
    }

    impl FakeRemoteInput {
        fn new(response: Response) -> Self {
            Self {
                response: Mutex::new(Some(response)),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl RemoteInputTransport for FakeRemoteInput {
        fn send_input(&self, node: &str, request: &Request) -> std::io::Result<Response> {
            self.requests
                .lock()
                .unwrap()
                .push((node.into(), request.clone()));
            Ok(self.response.lock().unwrap().take().unwrap())
        }
    }

    impl FakeRemoteSource {
        fn replace(&self, snapshot: RemoteSnapshot) {
            *self.0.lock().unwrap() = snapshot;
        }
    }

    impl RemoteSource for FakeRemoteSource {
        fn snapshot(&self) -> RemoteSnapshot {
            self.0.lock().unwrap().clone()
        }
    }

    fn remote_screen(text: &str) -> ScreenSnapshot {
        ScreenSnapshot {
            cells: vec![vec![StyledCell {
                text: text.into(),
                fg: Color::Default,
                bg: Color::Default,
                bold: false,
                dim: false,
                italic: false,
                underline: false,
                inverse: false,
                wide: false,
            }]],
            wrapped: vec![false],
            cursor: Cursor {
                row: 0,
                col: 0,
                visible: true,
            },
            scrollback_len: 0,
            scrollback_total: 0,
        }
    }

    fn remote_snapshot(
        state: RemoteState,
        age: Duration,
        screen: Option<ScreenSnapshot>,
    ) -> RemoteSnapshot {
        RemoteSnapshot {
            nodes: vec![RemoteNodeSnapshot {
                name: "laptop".into(),
                state,
                last_sync_age: Some(age),
                last_error: None,
                sessions: vec![RemoteSessionSnapshot {
                    name: "build".into(),
                    wire_name: "build".into(),
                    instance_id: "remote-instance".into(),
                    alive: true,
                    output_version: Some(7),
                    screen,
                    last_error: None,
                }],
            }],
        }
    }

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
    fn uncertain_input_is_visible_but_does_not_count_as_pending_attention() {
        let clock = ManualClock::new();
        let now = Instant::now();
        let mut ui = ClusterUi::new("studio", vec![session("dev", true)], clock.now());
        ui.input_sender
            .enqueue(
                &mut ui.input_queue,
                "dev",
                "instance-dev",
                b"line\r".to_vec(),
                now,
            )
            .unwrap();
        ui.input_sender.attempt_due(&mut ui.input_queue, now, |_| {
            Ok(remuda_core::protocol::Response::Uncertain)
        });
        let frame = ui.render(80, 24, "", &clock);
        assert!(frame.contains("Input delivery uncertain"));
        assert!(!frame.contains("pending input 1"));
        ui.key(crossterm::event::KeyCode::Char('!'));
        assert!(ui
            .render(80, 24, "", &clock)
            .contains("no sessions need attention"));
    }

    #[test]
    fn sent_input_clears_the_sending_notice() {
        let mut ui = ClusterUi::new("studio", vec![session("dev", true)], Duration::ZERO);
        ui.notice = Some(("sending input to dev".into(), Duration::ZERO));
        ui.handle_send_event(
            Some(QueueEvent::Sent {
                seq: 1,
                duplicate: false,
            }),
            Duration::ZERO,
        );
        assert!(ui.notice.is_none());
    }

    #[test]
    fn uncertain_input_clears_the_sending_notice() {
        let mut ui = ClusterUi::new("studio", vec![session("dev", true)], Duration::ZERO);
        ui.notice = Some(("sending input to dev".into(), Duration::ZERO));
        ui.handle_send_event(
            Some(QueueEvent::Uncertain {
                seq: 1,
                reason: "delivery uncertain".into(),
            }),
            Duration::ZERO,
        );
        assert!(ui.notice.is_none());
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
    fn remote_nodes_start_collapsed_and_render_state_badges() {
        let clock = ManualClock::new();
        let source = FakeRemoteSource(Mutex::new(remote_snapshot(
            RemoteState::Stale,
            Duration::from_secs(17),
            Some(remote_screen("kept output")),
        )));
        let mut ui = ClusterUi::new("studio", sessions(), clock.now());
        ui.remote_synced(&source);

        let frame = ui.render(100, 24, "", &clock);

        assert!(frame.contains("▶ laptop · stale · sync 17s ago"));
        assert!(!frame.contains("build"));
    }

    #[test]
    fn remote_session_keeps_its_last_screen_when_unreachable_and_is_read_only() {
        let clock = ManualClock::new();
        let source = FakeRemoteSource(Mutex::new(remote_snapshot(
            RemoteState::Reachable,
            Duration::ZERO,
            Some(remote_screen("last good screen")),
        )));
        let mut ui = ClusterUi::new("studio", sessions(), clock.now());
        ui.remote_synced(&source);
        ui.key(crossterm::event::KeyCode::Down);
        ui.key(crossterm::event::KeyCode::Enter);
        ui.key(crossterm::event::KeyCode::Down);
        ui.key(crossterm::event::KeyCode::Enter);
        assert!(!ui.composer_focused);
        assert!(ui.render(100, 24, "", &clock).contains("last good screen"));

        source.replace(remote_snapshot(
            RemoteState::Unreachable,
            Duration::from_secs(23),
            Some(remote_screen("last good screen")),
        ));
        ui.remote_synced(&source);
        ui.key(crossterm::event::KeyCode::Char('x'));

        let frame = ui.render(100, 24, "", &clock);
        assert!(frame.contains("laptop · unreachable"));
        assert!(frame.contains("last good screen"));
        assert!(frame.contains("Remote session is read-only"));
        assert!(!frame.contains("kill build?"));
    }

    #[test]
    fn remote_target_waits_for_first_session_list() {
        let source = std::sync::Arc::new(FakeRemoteSource(Mutex::new(RemoteSnapshot::default())));
        let producer = std::sync::Arc::clone(&source);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            producer.replace(remote_snapshot(
                RemoteState::Reachable,
                Duration::ZERO,
                Some(remote_screen("ready")),
            ));
        });
        let mut ui = ClusterUi::new("studio", Vec::new(), Duration::ZERO);
        ui.remote_synced(source.as_ref());
        select_target_with_remote_wait(
            &mut ui,
            Some("laptop/build"),
            source.as_ref(),
            Duration::from_millis(250),
        )
        .unwrap();
        assert!(matches!(
            ui.remote_active,
            Some(RemoteSelection::Session { .. })
        ));
    }

    #[test]
    fn unreachable_remote_label_explains_missing_registry_address() {
        assert_eq!(
            remote_state_label(
                RemoteState::Unreachable,
                None,
                Some("supply --addr HOST:PORT"),
            ),
            "unreachable · no address known"
        );
    }

    #[test]
    fn remote_composer_sends_a_chunk_through_the_remote_input_transport() {
        let clock = ManualClock::new();
        let mut ui = ClusterUi::new("studio", sessions(), clock.now());
        let mut snapshot = remote_snapshot(
            RemoteState::Reachable,
            Duration::ZERO,
            Some(remote_screen("remote")),
        );
        snapshot.nodes[0].sessions[0].wire_name = "wire-build".into();
        ui.remote_synced(&FakeRemoteSource(Mutex::new(snapshot)));
        ui.remote_input_enabled = true;
        ui.remote_expanded.insert("laptop".into());
        ui.remote_selected = Some(RemoteSelection::Session {
            node: "laptop".into(),
            name: "build".into(),
            instance_id: "remote-instance".into(),
        });
        ui.enter_selected(clock.now());
        assert!(ui.composer_focused);
        ui.enqueue_remote_draft(b"echo hi\r".to_vec(), clock.now());
        ui.start_pending(clock.now());

        let transport = FakeRemoteInput::new(Response::Ack { duplicate: false });
        ui.send_pending(
            std::path::Path::new("unused"),
            clock.now(),
            Some(&transport),
        );

        let sent = transport.requests.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "laptop");
        assert!(matches!(
            &sent[0].1,
            Request::Input { name, instance_id, bytes, .. }
                if name == "wire-build" && instance_id == "remote-instance" && bytes == b"echo hi\r"
        ));
        assert_eq!(
            ui.input_queue.items().next().unwrap().state,
            QueueState::Sent
        );
    }

    #[test]
    fn switching_remote_target_discards_unsaved_draft_with_original_target_notice() {
        let clock = ManualClock::new();
        let mut snapshot = remote_snapshot(
            RemoteState::Reachable,
            Duration::ZERO,
            Some(remote_screen("remote")),
        );
        let mut second = snapshot.nodes[0].sessions[0].clone();
        second.name = "shell".into();
        second.wire_name = "shell".into();
        second.instance_id = "shell-instance".into();
        snapshot.nodes[0].sessions.push(second);

        let mut ui = ClusterUi::new("studio", sessions(), clock.now());
        ui.remote_synced(&FakeRemoteSource(Mutex::new(snapshot)));
        ui.remote_input_enabled = true;
        ui.remote_expanded.insert("laptop".into());
        ui.remote_selected = Some(RemoteSelection::Session {
            node: "laptop".into(),
            name: "build".into(),
            instance_id: "remote-instance".into(),
        });
        ui.enter_selected(clock.now());
        ui.key(crossterm::event::KeyCode::Char('d'));
        ui.key(crossterm::event::KeyCode::Char('r'));
        ui.key_event(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('\\'),
                crossterm::event::KeyModifiers::CONTROL,
            ),
            clock.now(),
        );
        assert_eq!(ui.composer.text(), "dr");

        ui.set_tree_selection(super::TreeSelection::RemoteSession {
            node: "laptop".into(),
            name: "shell".into(),
            instance_id: "shell-instance".into(),
        });
        ui.enter_selected(clock.now());

        assert_eq!(ui.composer.text(), "");
        assert_eq!(
            ui.notice.as_ref().map(|(notice, _)| notice.as_str()),
            Some("draft for laptop/build discarded")
        );
        assert_eq!(ui.input_queue.items().count(), 0);
        assert_eq!(ui.remote_composer_target.as_ref().unwrap().1, "shell");
    }

    #[test]
    fn remote_pending_input_marks_rows_and_attention_filter() {
        let clock = ManualClock::new();
        let mut snapshot = remote_snapshot(
            RemoteState::Reachable,
            Duration::ZERO,
            Some(remote_screen("remote")),
        );
        snapshot.nodes[0].sessions[0].wire_name = "wire-build".into();
        let source = FakeRemoteSource(Mutex::new(snapshot));
        let mut ui = ClusterUi::new("studio", sessions(), clock.now());
        ui.remote_synced(&source);
        ui.remote_expanded.insert("laptop".into());
        ui.input_sender
            .enqueue_remote(
                &mut ui.input_queue,
                "laptop",
                "wire-build",
                "remote-instance",
                b"queued\r".to_vec(),
                Instant::now(),
            )
            .unwrap();

        let frame = ui.render(100, 24, "", &clock);
        assert!(
            frame.contains("live · pending input 1"),
            "remote pending marker missing from frame:\n{frame}"
        );

        ui.start_pending(clock.now());
        let sending_frame = ui.render(100, 24, "", &clock);
        assert!(sending_frame.contains("live · pending input 1"));

        ui.key(crossterm::event::KeyCode::Char('!'));
        let attention_frame = ui.render(100, 24, "", &clock);
        assert!(attention_frame.contains("build"));
        assert!(attention_frame.contains("pending input 1"));
    }

    #[test]
    fn remote_control_disabled_is_shown_and_stops_future_target_sends() {
        let clock = ManualClock::new();
        let mut ui = ClusterUi::new("studio", sessions(), clock.now());
        ui.remote_synced(&FakeRemoteSource(Mutex::new(remote_snapshot(
            RemoteState::Reachable,
            Duration::ZERO,
            Some(remote_screen("remote")),
        ))));
        ui.remote_input_enabled = true;
        ui.remote_expanded.insert("laptop".into());
        ui.remote_selected = Some(RemoteSelection::Session {
            node: "laptop".into(),
            name: "build".into(),
            instance_id: "remote-instance".into(),
        });
        ui.enter_selected(clock.now());
        ui.enqueue_remote_draft(b"first\r".to_vec(), clock.now());
        ui.start_pending(clock.now());
        let transport = FakeRemoteInput::new(Response::RemoteControlDisabled);
        ui.send_pending(
            std::path::Path::new("unused"),
            clock.now(),
            Some(&transport),
        );
        assert_eq!(
            ui.notice.as_ref().unwrap().0,
            "remote control disabled on laptop"
        );
        let count = ui.input_queue.items().count();

        ui.enqueue_remote_draft(b"second\r".to_vec(), clock.now());

        assert_eq!(ui.input_queue.items().count(), count);
        assert_eq!(
            ui.notice.as_ref().unwrap().0,
            "remote control disabled on laptop"
        );
        assert!(transport.requests.lock().unwrap().len() == 1);
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

    #[test]
    fn x_opens_close_confirmation_and_any_non_y_key_cancels() {
        let clock = ManualClock::new();
        let mut ui = ClusterUi::new("studio", vec![session("dev", true)], clock.now());
        ui.key(crossterm::event::KeyCode::Char('x'));
        assert!(ui
            .render(80, 24, "", &clock)
            .contains("kill dev? it is running — y / n"));
        ui.key(crossterm::event::KeyCode::Char('n'));
        assert!(!ui
            .render(80, 24, "", &clock)
            .contains("kill dev? it is running — y / n"));
    }

    #[test]
    fn x_then_y_closes_the_selected_instance_and_shows_its_ended_frame() {
        let clock = ManualClock::new();
        let mut ui = ClusterUi::new("studio", vec![session("dev", true)], clock.now());
        let summary = ui.sessions[0].clone();
        assert!(ui.record_capture(&summary, "final output".into(), clock.now()));
        ui.key(crossterm::event::KeyCode::Char('x'));
        ui.key(crossterm::event::KeyCode::Char('y'));
        assert!(ui.render(80, 24, "", &clock).contains("closing dev"));
        let (name, instance_id) = ui.pending_close.take().unwrap();
        assert_eq!(
            (name.clone(), instance_id.clone()),
            ("dev".into(), "instance-dev".into())
        );
        ui.close_completed(name, instance_id, Ok(Response::Ok), clock.now());
        let final_frame = ui.ended.as_ref().unwrap().screen.clone();
        let frame = ui.render(80, 24, &final_frame, &clock);
        assert!(frame.contains("studio / dev · ended · final snapshot"));
        assert!(frame.contains("final output"));
        assert!(frame.contains("Input is disabled"));
    }

    #[test]
    fn sync_keeps_tree_selection_while_showing_an_ended_frame() {
        let clock = ManualClock::new();
        let mut ui = ClusterUi::new(
            "studio",
            vec![session("dev", true), session("worker", true)],
            clock.now(),
        );
        let summary = ui.sessions[0].clone();
        assert!(ui.record_capture(&summary, "final output".into(), clock.now()));
        ui.sessions_synced(vec![session("worker", true)], Duration::from_secs(1));
        assert!(ui.ended.is_some());
        assert_eq!(ui.sessions[ui.selected].name, "dev");

        ui.key(crossterm::event::KeyCode::Up);
        assert_eq!(ui.sessions[ui.selected].name, "worker");
        ui.sessions_synced(vec![session("worker", true)], Duration::from_secs(2));

        assert_eq!(ui.sessions[ui.selected].name, "worker");
        assert_eq!(ui.sessions[ui.active].name, "dev");
        assert!(ui.ended.is_some());
    }

    #[test]
    fn x_on_a_dead_session_shows_an_already_ended_notice() {
        let clock = ManualClock::new();
        let mut ui = ClusterUi::new("studio", vec![session("dev", false)], clock.now());

        ui.key(crossterm::event::KeyCode::Char('x'));

        let frame = ui.render(80, 24, "", &clock);
        assert!(frame.contains("dev has already ended."));
        assert!(!frame.contains("kill dev?"));
    }

    #[test]
    fn vanished_instance_keeps_its_last_frame_as_ended_until_x_clears_it() {
        let clock = ManualClock::new();
        let mut ui = ClusterUi::new("studio", vec![session("dev", true)], clock.now());
        let summary = ui.sessions[0].clone();
        assert!(ui.record_capture(&summary, "final output".into(), clock.now()));
        ui.sessions_synced(vec![], Duration::from_secs(2));
        let frame = ui.render(80, 24, &ui.ended.as_ref().unwrap().screen, &clock);
        assert!(frame.contains("studio / dev · ended · final snapshot"));
        assert!(frame.contains("final output"));
        assert!(frame.contains("Input is disabled · x clears ended session"));
        ui.key(crossterm::event::KeyCode::Char('x'));
        let frame = ui.render(80, 24, "", &clock);
        assert!(!frame.contains("studio / dev · ended"));
        assert!(frame.contains("studio · no sessions"));
    }
}
