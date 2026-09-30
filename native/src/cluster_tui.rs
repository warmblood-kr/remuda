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
use std::collections::{HashSet, VecDeque};
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

pub mod close_request;
const UI_REQUEST_TIMEOUT: Duration = Duration::from_millis(250);
const REMOTE_KEYS_COALESCE_INTERVAL: Duration = Duration::from_millis(40);
const REMOTE_PASTE_FAILURE_NOTICE: &str =
    "remote input stopped after a failed chunk; remaining lines dropped";
const REMOTE_PARTIAL_INPUT_NOTICE: &str =
    "partial input may remain at the remote prompt; your new line would be appended to the leftover fragment and run with it — press Enter again to send anyway";
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

pub fn cluster_tree_has_sessions(
    local_sessions: &[SessionSummary],
    remote_snapshot: &RemoteSnapshot,
) -> bool {
    !local_sessions.is_empty()
        || remote_snapshot
            .nodes
            .iter()
            .any(|node| !node.sessions.is_empty())
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
    pending_close: Option<PendingClose>,
    remote_close_confirmation: Option<RemoteCloseTarget>,
    confirmation: Confirmation,
    ended: Option<EndedState>,
    last_frame: Option<EndedState>,
    remote_snapshot: RemoteSnapshot,
    remote_expanded: HashSet<String>,
    remote_selected: Option<RemoteSelection>,
    remote_active: Option<RemoteSelection>,
    remote_keys_mode: Option<RemoteSelection>,
    remote_key_buffers: VecDeque<RemoteKeyBuffer>,
    // (registry fingerprint, display label, session name, instance id)
    remote_composer_target: Option<(String, String, String, String)>,
    partial_input_confirm_target: Option<(String, String, String, String)>,
    remote_input_enabled: bool,
    remote_control_disabled: HashSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingClose {
    remote_target: Option<(String, String)>,
    name: String,
    display_name: String,
    instance_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RemoteCloseTarget {
    registry_key: String,
    label: String,
    wire_name: String,
}

struct RemoteKeyBuffer {
    target: RemoteSelection,
    bytes: Vec<u8>,
    last_input_at: Instant,
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
            remote_close_confirmation: None,
            confirmation: Confirmation::default(),
            ended: None,
            last_frame: None,
            remote_snapshot: RemoteSnapshot::default(),
            remote_expanded: HashSet::new(),
            remote_selected: None,
            remote_active: None,
            remote_keys_mode: None,
            remote_key_buffers: VecDeque::new(),
            remote_composer_target: None,
            partial_input_confirm_target: None,
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
            RemoteSelection::Node(registry_key) => self
                .remote_snapshot
                .nodes
                .iter()
                .any(|node| node.registry_key == registry_key)
                .then_some(RemoteSelection::Node(registry_key)),
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
                    .any(|snapshot| snapshot.registry_key == node)
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
            .find(|snapshot| snapshot.registry_key == node)
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
            .find(|snapshot| &snapshot.registry_key == node)?;
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
            rows.push(TreeSelection::RemoteNode(node.registry_key.clone()));
            if self.remote_expanded.contains(&node.registry_key) {
                rows.extend(visible_sessions.into_iter().map(|session| {
                    TreeSelection::RemoteSession {
                        node: node.registry_key.clone(),
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

    fn send_close_pending(
        &mut self,
        path: &Path,
        remote_input: Option<&dyn RemoteInputTransport>,
        now: Duration,
    ) {
        let Some(pending) = self.pending_close.take() else {
            return;
        };
        let request = confirmed_close(pending.name.clone(), pending.instance_id.clone());
        let response = if let Some((registry_key, _)) = &pending.remote_target {
            match remote_input {
                Some(transport) => transport.send_close(registry_key, &request),
                None => Err(io::Error::other("remote Close transport is unavailable")),
            }
        } else {
            client::request_with_timeout(path, &request, UI_REQUEST_TIMEOUT)
        };
        self.close_completed(
            pending
                .remote_target
                .as_ref()
                .map(|(_, label)| label.as_str()),
            pending.display_name,
            pending.name,
            pending.instance_id,
            response,
            now,
        );
    }

    fn close_completed(
        &mut self,
        remote_label: Option<&str>,
        display_name: String,
        name: String,
        instance_id: String,
        response: io::Result<Response>,
        now: Duration,
    ) {
        match response {
            Ok(Response::Ok) => {
                if remote_label.is_some() {
                    self.notice = Some((format!("Close sent to {display_name}"), now));
                    return;
                }
                if let Some(frame) = self.last_frame.as_ref().filter(|frame| {
                    frame.summary.name == name
                        && frame.summary.instance_id.as_deref() == Some(instance_id.as_str())
                }) {
                    self.set_ended(frame.clone());
                } else {
                    self.notice = Some(("session closed; final frame was unavailable".into(), now));
                }
            }
            Ok(Response::RemoteControlDisabled) => {
                let notice = remote_label.map_or_else(
                    || "remote control disabled".to_owned(),
                    |label| format!("remote control disabled on {label}"),
                );
                self.notice = Some((notice, now));
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
        if self.remote_keys_mode.is_some() {
            return self.render_remote_keys_mode(width, height);
        }
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
                if !cluster_tree_has_sessions(&self.sessions, &self.remote_snapshot) {
                    frame.push(
                        "    No sessions yet. Start one on any machine: remuda run -n NAME COMMAND"
                            .into(),
                    );
                }
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
        frame.extend(
            visible_remote_pane_lines(&pane_body, self.remote_active.is_some(), screen_rows)
                .into_iter()
                .map(str::to_string),
        );
        if let Some((notice, _)) = notice {
            frame.push(notice.clone());
        }
        frame.extend(queue_rows.into_iter().rev().map(queue_line));
        if let Some(prompt) = self.confirmation.prompt() {
            frame.push(prompt);
        } else if self.remote_active.is_some() && self.composer_focused {
            frame.push(self.composer_line());
            frame.push("Enter send · Ctrl-C clear · Esc list".into());
        } else if self.remote_active.is_some() {
            frame.push("Remote session is read-only · q detach".into());
        } else if self.ended.is_some() {
            frame.push("Input is disabled · x clears ended session · q detaches".into());
        } else if self.composer_focused {
            frame.push(self.composer_line());
            frame.push("Enter send · Ctrl-C clear · Esc list".into());
        } else {
            frame.push(self.footer());
        }
        frame
            .into_iter()
            .map(|line| truncate(&line, width))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render_remote_keys_mode(&self, width: usize, height: usize) -> String {
        let Some(target @ RemoteSelection::Session { .. }) = &self.remote_keys_mode else {
            return String::new();
        };
        let (node_label, session_name, body) = self.remote_session(target).map_or_else(
            || {
                let RemoteSelection::Session { node, name, .. } = target else {
                    unreachable!()
                };
                (node.clone(), name.clone(), String::new())
            },
            |(node, session)| {
                (
                    node.name.clone(),
                    session.name.clone(),
                    session
                        .screen
                        .as_ref()
                        .map(remote_screen_text)
                        .unwrap_or_default(),
                )
            },
        );
        let mut frame = vec![format!("KEYS {node_label}/{session_name} · Ctrl-\\ back")];
        frame.extend(
            visible_remote_pane_lines(&body, true, height.saturating_sub(1))
                .into_iter()
                .map(str::to_string),
        );
        frame.resize(height, String::new());
        frame
            .into_iter()
            .take(height)
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
            let is_expanded = self.remote_expanded.contains(&node.registry_key);
            let is_selected =
                self.remote_selected == Some(RemoteSelection::Node(node.registry_key.clone()));
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
                            node: node.registry_key.clone(),
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

    fn handle_event(&mut self, event: crossterm::event::Event, now: Duration) -> bool {
        match event {
            crossterm::event::Event::Key(key)
                if key.kind == crossterm::event::KeyEventKind::Press =>
            {
                self.key_event(key, now)
            }
            crossterm::event::Event::Paste(text) => {
                if let Some(target) = self.remote_keys_mode.clone() {
                    let text = text.replace("\x1b[200~", "").replace("\x1b[201~", "");
                    let mut bytes = Vec::with_capacity(text.len() + 12);
                    bytes.extend_from_slice(b"\x1b[200~");
                    bytes.extend_from_slice(text.as_bytes());
                    bytes.extend_from_slice(b"\x1b[201~");
                    self.enqueue_remote_key_bytes(&target, bytes, now);
                }
                false
            }
            _ => false,
        }
    }

    fn key_event(&mut self, event: crossterm::event::KeyEvent, now: Duration) -> bool {
        if self.partial_input_confirm_target.is_some()
            && !self
                .notice
                .as_ref()
                .is_some_and(|(notice, _)| notice == REMOTE_PARTIAL_INPUT_NOTICE)
        {
            self.partial_input_confirm_target = None;
        }
        if let Some(decision) = self.confirmation.handle(event.code) {
            match decision {
                Decision::Confirmed { name, instance_id } => {
                    let remote = self.remote_close_confirmation.take();
                    self.pending_close = Some(PendingClose {
                        remote_target: remote
                            .as_ref()
                            .map(|target| (target.registry_key.clone(), target.label.clone())),
                        name: remote
                            .as_ref()
                            .map_or_else(|| name.clone(), |target| target.wire_name.clone()),
                        display_name: name.clone(),
                        instance_id,
                    });
                    self.notice = Some((format!("closing {name}"), now));
                }
                Decision::Cancelled => {
                    self.remote_close_confirmation = None;
                    self.notice = Some(("close cancelled".into(), now));
                }
            }
            return false;
        }
        if let Some(target) = self.remote_keys_mode.clone() {
            if event
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
                && matches!(event.code, crossterm::event::KeyCode::Char('\\' | '4'))
            {
                self.remote_keys_mode = None;
                self.notice = None;
                return false;
            }
            if let Some(bytes) = crate::tui::to_bytes(event) {
                self.enqueue_remote_key_bytes(&target, bytes, now);
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
            Char('k') if self.query.is_none() => self.enter_remote_keys_mode(now),
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

    fn enter_remote_keys_mode(&mut self, now: Duration) {
        let TreeSelection::RemoteSession {
            node,
            name,
            instance_id,
        } = self.current_tree_selection()
        else {
            return;
        };
        let target = RemoteSelection::Session {
            node,
            name,
            instance_id,
        };
        let Some((snapshot, session)) = self.remote_session(&target) else {
            self.notice = Some(("remote session is no longer listed".into(), now));
            return;
        };
        if !self.remote_input_enabled || !session.alive {
            self.notice = Some((
                format!(
                    "remote input disabled: {}/{} is unavailable",
                    snapshot.name, session.name
                ),
                now,
            ));
            return;
        }
        self.remote_active = Some(target.clone());
        self.remote_keys_mode = Some(target);
        self.composer_focused = false;
        self.set_remote_composer_target(None, now);
        self.notice = None;
    }

    fn enqueue_remote_key_bytes(
        &mut self,
        target: &RemoteSelection,
        bytes: Vec<u8>,
        now: Duration,
    ) {
        let RemoteSelection::Session {
            node,
            name: _,
            instance_id: _,
        } = target
        else {
            return;
        };
        if self.remote_control_disabled.contains(node) {
            self.notice = Some(("remote control disabled on this node".into(), now));
            return;
        }
        let Some((node_label, session_name, alive)) =
            self.remote_session(target).map(|(snapshot, session)| {
                (snapshot.name.clone(), session.name.clone(), session.alive)
            })
        else {
            self.remote_keys_mode = None;
            self.notice = Some(("remote session is no longer listed".into(), now));
            return;
        };
        if !alive {
            self.remote_keys_mode = None;
            self.notice = Some((format!("{node_label}/{session_name} has ended"), now));
            return;
        }
        let at = Instant::now();
        if let Some(buffer) = self
            .remote_key_buffers
            .back_mut()
            .filter(|buffer| buffer.target == *target)
        {
            buffer.bytes.extend(bytes);
            buffer.last_input_at = at;
        } else {
            self.remote_key_buffers.push_back(RemoteKeyBuffer {
                target: target.clone(),
                bytes,
                last_input_at: at,
            });
        }
    }

    fn flush_remote_key_buffer(&mut self, now: Instant, ui_now: Duration) {
        if self.input_queue.sending_batch().is_some() {
            return;
        }
        let Some(buffer) = self.remote_key_buffers.front() else {
            return;
        };
        if now.saturating_duration_since(buffer.last_input_at) < REMOTE_KEYS_COALESCE_INTERVAL {
            return;
        }
        let buffer = self
            .remote_key_buffers
            .pop_front()
            .expect("remote key buffer exists");
        let RemoteSelection::Session {
            node,
            name,
            instance_id,
        } = buffer.target
        else {
            return;
        };
        let selection = RemoteSelection::Session {
            node: node.clone(),
            name,
            instance_id: instance_id.clone(),
        };
        let Some((node_label, session_name, wire_name, alive)) =
            self.remote_session(&selection).map(|(snapshot, session)| {
                (
                    snapshot.name.clone(),
                    session.name.clone(),
                    session.wire_name.clone(),
                    session.alive,
                )
            })
        else {
            self.notice = Some(("remote session is no longer listed".into(), ui_now));
            return;
        };
        if !alive {
            self.notice = Some((format!("{node_label}/{session_name} has ended"), ui_now));
            return;
        }
        if let Err(error) = self.input_sender.enqueue_remote(
            &mut self.input_queue,
            &node,
            &wire_name,
            &instance_id,
            buffer.bytes,
            now,
        ) {
            self.notice = Some((format!("remote key not queued: {error}"), ui_now));
        }
    }

    fn handle_close_key(&mut self, now: Duration) {
        if self.ended.is_some() {
            self.clear_ended();
        } else if let Some(RemoteSelection::Session {
            node,
            name,
            instance_id,
        }) = self.remote_selected.clone()
        {
            let selected = self.remote_session(&RemoteSelection::Session {
                node: node.clone(),
                name: name.clone(),
                instance_id: instance_id.clone(),
            });
            if let Some((node_snapshot, session)) = selected {
                if !session.alive {
                    self.notice = Some((
                        format!("{}/{} has already ended", node_snapshot.name, name),
                        now,
                    ));
                } else {
                    let label = node_snapshot.name.clone();
                    let fingerprint = node.strip_prefix("SHA256:").unwrap_or(&node);
                    let short_fingerprint = fingerprint.chars().take(6).collect::<String>();
                    let wire_name = session.wire_name.clone();
                    self.remote_close_confirmation = Some(RemoteCloseTarget {
                        registry_key: node,
                        label: label.clone(),
                        wire_name,
                    });
                    self.confirmation
                        .begin(format!("{label}({short_fingerprint})/{name}"), instance_id);
                }
            } else {
                self.notice = Some(("remote session is no longer listed".into(), now));
            }
        } else if self.remote_selected.is_some() {
            self.remote_close_confirmation = None;
            self.notice = Some(("select a remote session to close".into(), now));
        } else if let Some(session) = self.sessions.get(self.selected) {
            if session.alive {
                if let Some(instance_id) = session.instance_id.clone() {
                    self.active = self.selected;
                    self.remote_close_confirmation = None;
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
                let display_label = self
                    .remote_session(&RemoteSelection::Session {
                        node: node.clone(),
                        name: name.clone(),
                        instance_id: instance_id.clone(),
                    })
                    .map(|(snapshot, _)| snapshot.name.clone())
                    .unwrap_or_else(|| node.clone());
                self.set_remote_composer_target(
                    can_send.then_some((node, display_label, name, instance_id)),
                    now,
                );
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
                self.set_remote_composer_target(None, now);
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
            ComposerAction::None => {
                if self
                    .partial_input_confirm_target
                    .as_ref()
                    .is_some_and(|(_, _, _, confirmed_text)| self.composer.text() != confirmed_text)
                {
                    self.partial_input_confirm_target = None;
                }
            }
            ComposerAction::Cleared => {
                self.partial_input_confirm_target = None;
                if self.remote_composer_target.is_none() {
                    self.composer_target = None;
                }
                self.notice = Some(("draft cleared".into(), now));
            }
            ComposerAction::Detach => {
                self.partial_input_confirm_target = None;
                self.composer_focused = false;
            }
            ComposerAction::Submit(bytes) => {
                if let Some((node, _, name, instance_id)) = self.remote_composer_target.clone() {
                    let draft = bytes.strip_suffix(b"\r").unwrap_or(&bytes);
                    let target = (
                        node.clone(),
                        name.clone(),
                        instance_id.clone(),
                        String::from_utf8_lossy(draft).into_owned(),
                    );
                    if self.partial_input_confirm_target.as_ref() == Some(&target) {
                        self.partial_input_confirm_target = None;
                        self.enqueue_remote_draft(bytes, now);
                    } else if self.remote_target_may_be_partial(&node, &name, &instance_id) {
                        self.restore_draft(&bytes);
                        self.partial_input_confirm_target = Some(target);
                        self.notice = Some((REMOTE_PARTIAL_INPUT_NOTICE.into(), now));
                    } else {
                        self.enqueue_remote_draft(bytes, now);
                    }
                } else {
                    self.enqueue_draft(bytes, now);
                }
            }
        }
    }

    fn set_remote_composer_target(
        &mut self,
        target: Option<(String, String, String, String)>,
        now: Duration,
    ) -> bool {
        if self.remote_composer_target == target {
            return false;
        }
        self.partial_input_confirm_target = None;
        let discarded = if let Some((_, node_label, name, _)) = &self.remote_composer_target {
            if self.composer.text().is_empty() {
                false
            } else {
                self.composer.clear();
                self.notice = Some((format!("draft for {node_label}/{name} discarded"), now));
                true
            }
        } else {
            false
        };
        self.remote_composer_target = target;
        discarded
    }

    fn remote_target_may_be_partial(&self, node: &str, name: &str, instance_id: &str) -> bool {
        let selection = RemoteSelection::Session {
            node: node.into(),
            name: name.into(),
            instance_id: instance_id.into(),
        };
        self.remote_session(&selection).is_some_and(|(_, session)| {
            self.input_sender
                .has_partial_input_warning_for(node, &session.wire_name, instance_id)
        })
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
        let Some((node_key, node_label, name, instance_id)) = self.remote_composer_target.clone()
        else {
            return;
        };
        if self.remote_control_disabled.contains(&node_key) {
            self.restore_draft(&bytes);
            self.notice = Some((format!("remote control disabled on {node_label}"), now));
            return;
        }
        let selection = RemoteSelection::Session {
            node: node_key.clone(),
            name: name.clone(),
            instance_id: instance_id.clone(),
        };
        let Some((_, session)) = self.remote_session(&selection) else {
            self.restore_draft(&bytes);
            self.notice = Some((
                format!("remote input unavailable: {node_label}/{name} is no longer listed"),
                now,
            ));
            return;
        };
        let wire_name = session.wire_name.clone();
        if !session.alive {
            self.restore_draft(&bytes);
            self.notice = Some((
                format!("remote input disabled: {node_label}/{name} has ended"),
                now,
            ));
            return;
        }
        match self.input_sender.enqueue_remote(
            &mut self.input_queue,
            &node_key,
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
        self.flush_remote_key_buffer(Instant::now(), now);
        self.input_sender
            .begin_due(&mut self.input_queue, Instant::now());
        if let Some(batch) = self.input_queue.sending_batch() {
            if self.partial_input_confirm_target.is_none()
                && !self
                    .notice
                    .as_ref()
                    .is_some_and(|(notice, _)| notice == REMOTE_PASTE_FAILURE_NOTICE)
            {
                self.notice = Some((format!("sending input to {}", batch.name), now));
            }
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
            Some(QueueEvent::PasteAborted { .. }) => {
                self.notice = Some((REMOTE_PASTE_FAILURE_NOTICE.into(), now));
            }
            Some(QueueEvent::RemoteTargetFailed { .. }) => {
                self.notice = Some((REMOTE_PASTE_FAILURE_NOTICE.into(), now));
            }
            Some(QueueEvent::Dropped { reason, .. }) | Some(QueueEvent::Failed { reason, .. }) => {
                let mut draft_discarded = false;
                if reason.contains("session restarted") {
                    if self.remote_composer_target.is_some() {
                        draft_discarded = self.set_remote_composer_target(None, now);
                        self.composer_focused = false;
                    } else {
                        self.composer_target = None;
                    }
                }
                if !draft_discarded {
                    self.notice = Some((reason, now));
                }
            }
            Some(QueueEvent::RemoteControlDisabled { node, .. }) => {
                self.remote_control_disabled.insert(node.clone());
                let label = self
                    .remote_snapshot
                    .nodes
                    .iter()
                    .find(|snapshot| snapshot.registry_key == node)
                    .map(|snapshot| snapshot.name.as_str())
                    .unwrap_or(&node);
                self.notice = Some((format!("remote control disabled on {label}"), now));
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
                batch.remote_node.as_deref() == Some(node.registry_key.as_str())
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
                    .map_or_else(String::new, |(_, node_label, name, _)| {
                        format!("{node_label}/{name}> ")
                    })
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
            let exact = self
                .remote_snapshot
                .nodes
                .iter()
                .find(|snapshot| snapshot.registry_key == node);
            let remote = if let Some(exact) = exact {
                exact
            } else {
                let mut matches = self
                    .remote_snapshot
                    .nodes
                    .iter()
                    .filter(|snapshot| snapshot.name == node);
                let remote = matches
                    .next()
                    .ok_or_else(|| io::Error::other(format!("node {node} was not found")))?;
                if matches.next().is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("node label {node} is ambiguous; use the fingerprint"),
                    ));
                }
                remote
            };
            let session = remote
                .sessions
                .iter()
                .find(|item| item.name == session)
                .ok_or_else(|| {
                    io::Error::other(format!("session {session} was not found on {node}"))
                })?;
            let selection = RemoteSelection::Session {
                node: remote.registry_key.clone(),
                name: session.name.clone(),
                instance_id: session.instance_id.clone(),
            };
            self.remote_expanded.insert(remote.registry_key.clone());
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

fn visible_remote_pane_lines(
    pane_body: &str,
    remote_active: bool,
    screen_rows: usize,
) -> Vec<&str> {
    let mut pane_lines = pane_body.lines().collect::<Vec<_>>();
    if remote_active {
        while pane_lines.last().is_some_and(|line| line.trim().is_empty()) {
            pane_lines.pop();
        }
    }
    let first_visible = if remote_active {
        pane_lines.len().saturating_sub(screen_rows)
    } else {
        0
    };
    pane_lines
        .iter()
        .skip(first_visible)
        .take(screen_rows)
        .copied()
        .collect()
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
            let display_label = ui
                .remote_session(&RemoteSelection::Session {
                    node: node.clone(),
                    name: name.clone(),
                    instance_id: instance_id.clone(),
                })
                .map(|(snapshot, _)| snapshot.name.clone())
                .unwrap_or_else(|| node.clone());
            ui.set_remote_composer_target(
                Some((node, display_label, name, instance_id)),
                clock.now(),
            );
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
        ui.send_close_pending(path, remote_input, clock.now());
        ui.send_pending(path, clock.now(), remote_input);
        ui.start_pending(clock.now());
        if crossterm::event::poll(Duration::from_millis(250))? {
            if ui.handle_event(crossterm::event::read()?, clock.now()) {
                return Ok(());
            }
            ui.send_close_pending(path, remote_input, clock.now());
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
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => Err(error),
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
    use super::queue::{QueueEvent, QueueState, MAX_IO_RETRIES};
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
                registry_key: "fp-laptop".into(),
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

    fn remote_screen_rows(rows: usize) -> ScreenSnapshot {
        ScreenSnapshot {
            cells: (0..rows)
                .map(|row| {
                    vec![StyledCell {
                        text: format!("body-row-{row}"),
                        fg: Color::Default,
                        bg: Color::Default,
                        bold: false,
                        dim: false,
                        italic: false,
                        underline: false,
                        inverse: false,
                        wide: false,
                    }]
                })
                .collect(),
            wrapped: vec![false; rows],
            cursor: Cursor {
                row: rows.saturating_sub(1) as u16,
                col: 0,
                visible: true,
            },
            scrollback_len: 0,
            scrollback_total: 0,
        }
    }

    fn selected_remote_ui(clock: &ManualClock, screen: ScreenSnapshot) -> ClusterUi {
        let mut ui = ClusterUi::new("studio", sessions(), clock.now());
        ui.remote_synced(&FakeRemoteSource(Mutex::new(remote_snapshot(
            RemoteState::Reachable,
            Duration::ZERO,
            Some(screen),
        ))));
        ui.remote_input_enabled = true;
        ui.select_target(Some("fp-laptop/build")).unwrap();
        ui
    }

    fn key_event(
        ui: &mut ClusterUi,
        code: crossterm::event::KeyCode,
        modifiers: crossterm::event::KeyModifiers,
        now: Duration,
    ) {
        ui.key_event(crossterm::event::KeyEvent::new(code, modifiers), now);
    }

    #[test]
    fn remote_keys_mode_uses_terminal_bytes_and_forwards_escape_control_and_navigation_keys() {
        use crossterm::event::{KeyCode, KeyModifiers};

        let clock = ManualClock::new();
        let mut ui = selected_remote_ui(&clock, remote_screen("remote"));
        let transport = FakeRemoteInput::new(Response::Ack { duplicate: false });
        key_event(&mut ui, KeyCode::Char('k'), KeyModifiers::NONE, clock.now());
        for (code, modifiers) in [
            (KeyCode::Esc, KeyModifiers::NONE),
            (KeyCode::Char('c'), KeyModifiers::CONTROL),
            (KeyCode::Up, KeyModifiers::NONE),
            (KeyCode::Tab, KeyModifiers::NONE),
            (KeyCode::Enter, KeyModifiers::NONE),
            (KeyCode::Char('y'), KeyModifiers::NONE),
        ] {
            key_event(&mut ui, code, modifiers, clock.now());
        }
        std::thread::sleep(Duration::from_millis(50));
        ui.start_pending(clock.now());
        let queued = ui
            .input_queue
            .items()
            .map(|batch| batch.bytes.clone())
            .collect::<Vec<_>>();
        assert_eq!(queued, [b"\x1b\x03\x1b[A\t\ry".to_vec()]);
        ui.send_pending(
            std::path::Path::new("unused"),
            clock.now(),
            Some(&transport),
        );

        let sent = transport.requests.lock().unwrap();
        assert_eq!(sent.len(), 1, "a burst of keys should be one Input");
        assert!(matches!(
            &sent[0].1,
            Request::Input { bytes, .. }
                if bytes == b"\x1b\x03\x1b[A\t\ry"
        ));
    }

    #[test]
    fn ctrl_backslash_leaves_remote_keys_mode_without_sending_the_exit_key() {
        use crossterm::event::{KeyCode, KeyModifiers};

        let clock = ManualClock::new();
        let mut ui = selected_remote_ui(&clock, remote_screen("remote"));
        key_event(&mut ui, KeyCode::Char('k'), KeyModifiers::NONE, clock.now());
        assert!(ui
            .render(80, 24, "", &clock)
            .contains("KEYS laptop/build · Ctrl-\\ back"));

        key_event(
            &mut ui,
            KeyCode::Char('\\'),
            KeyModifiers::CONTROL,
            clock.now(),
        );

        assert!(!ui.render(80, 24, "", &clock).contains("KEYS laptop/build"));
        assert_eq!(ui.input_queue.items().count(), 0);
    }

    #[test]
    fn remote_keys_mode_uses_one_status_row_and_all_other_rows_for_the_pane() {
        use crossterm::event::{KeyCode, KeyModifiers};

        let clock = ManualClock::new();
        for (cols, rows) in [(80, 24), (40, 12)] {
            let mut ui = selected_remote_ui(&clock, remote_screen_rows(40));
            key_event(&mut ui, KeyCode::Char('k'), KeyModifiers::NONE, clock.now());
            let frame = ui.render(cols, rows, "", &clock);
            let lines = frame.lines().collect::<Vec<_>>();

            assert_eq!(lines.len(), usize::from(rows), "{cols}x{rows}: {frame}");
            assert!(lines[0].contains("KEYS laptop/build · Ctrl-\\ back"));
            assert_eq!(lines[1..].len(), usize::from(rows - 1));
            assert!(lines[1].starts_with("body-row-"));
        }
    }

    #[test]
    fn keys_typed_while_a_remote_send_is_in_flight_coalesce_into_one_next_input() {
        use crossterm::event::{KeyCode, KeyModifiers};

        let clock = ManualClock::new();
        let mut ui = selected_remote_ui(&clock, remote_screen("remote"));
        let transport = FakeRemoteInput::new(Response::Ack { duplicate: false });
        key_event(&mut ui, KeyCode::Char('k'), KeyModifiers::NONE, clock.now());
        key_event(&mut ui, KeyCode::Char('a'), KeyModifiers::NONE, clock.now());
        std::thread::sleep(Duration::from_millis(50));
        ui.start_pending(clock.now());
        key_event(&mut ui, KeyCode::Char('b'), KeyModifiers::NONE, clock.now());
        key_event(&mut ui, KeyCode::Char('c'), KeyModifiers::NONE, clock.now());

        ui.send_pending(
            std::path::Path::new("unused"),
            clock.now(),
            Some(&transport),
        );
        *transport.response.lock().unwrap() = Some(Response::Ack { duplicate: false });
        std::thread::sleep(Duration::from_millis(50));
        ui.start_pending(clock.now());
        ui.send_pending(
            std::path::Path::new("unused"),
            clock.now(),
            Some(&transport),
        );

        let sent = transport.requests.lock().unwrap();
        assert_eq!(sent.len(), 2, "the in-flight key plus one coalesced burst");
        assert!(matches!(
            &sent[1].1,
            Request::Input { bytes, .. } if bytes == b"bc"
        ));
    }

    #[test]
    fn bracketed_paste_in_remote_keys_mode_is_forwarded_with_its_markers() {
        use crossterm::event::{Event, KeyCode, KeyModifiers};

        let clock = ManualClock::new();
        let mut ui = selected_remote_ui(&clock, remote_screen("remote"));
        let transport = FakeRemoteInput::new(Response::Ack { duplicate: false });
        key_event(&mut ui, KeyCode::Char('k'), KeyModifiers::NONE, clock.now());
        ui.handle_event(Event::Paste("approval text".into()), clock.now());
        std::thread::sleep(Duration::from_millis(50));
        ui.start_pending(clock.now());
        ui.send_pending(
            std::path::Path::new("unused"),
            clock.now(),
            Some(&transport),
        );

        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "paste should produce an Input");
        assert!(matches!(
            &requests[0].1,
            Request::Input { bytes, .. }
                if bytes == b"\x1b[200~approval text\x1b[201~"
        ));
    }

    #[test]
    fn bracketed_paste_markers_inside_remote_paste_are_stripped() {
        use crossterm::event::{Event, KeyCode, KeyModifiers};

        let clock = ManualClock::new();
        let mut ui = selected_remote_ui(&clock, remote_screen("remote"));
        let transport = FakeRemoteInput::new(Response::Ack { duplicate: false });
        key_event(&mut ui, KeyCode::Char('k'), KeyModifiers::NONE, clock.now());
        ui.handle_event(
            Event::Paste("left\x1b[201~middle\x1b[200~right".into()),
            clock.now(),
        );
        std::thread::sleep(Duration::from_millis(50));
        ui.start_pending(clock.now());
        ui.send_pending(
            std::path::Path::new("unused"),
            clock.now(),
            Some(&transport),
        );

        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "paste should produce an Input");
        assert!(matches!(
            &requests[0].1,
            Request::Input { bytes, .. }
                if bytes == b"\x1b[200~leftmiddleright\x1b[201~"
        ));
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

    fn ui_with_uncertain_remote_target() -> (ManualClock, ClusterUi, FakeRemoteInput) {
        let clock = ManualClock::new();
        let limit = crate::remote_front::MAX_REMOTE_INPUT_BATCH_BYTES;
        let mut snapshot = remote_snapshot(
            RemoteState::Reachable,
            Duration::ZERO,
            Some(remote_screen("remote")),
        );
        let mut other = snapshot.nodes[0].clone();
        other.registry_key = "fp-tablet".into();
        other.name = "tablet".into();
        other.sessions[0].instance_id = "tablet-instance".into();
        snapshot.nodes.push(other);

        let mut ui = ClusterUi::new("studio", sessions(), clock.now());
        ui.remote_synced(&FakeRemoteSource(Mutex::new(snapshot)));
        ui.remote_input_enabled = true;
        ui.select_target(Some("fp-laptop/build")).unwrap();
        ui.enter_selected(clock.now());
        ui.enqueue_remote_draft(vec![b'x'; limit + 1], clock.now());
        ui.start_pending(clock.now());
        let transport = FakeRemoteInput::new(Response::Uncertain);
        ui.send_pending(
            std::path::Path::new("unused"),
            clock.now(),
            Some(&transport),
        );
        (clock, ui, transport)
    }

    fn arm_partial_input_confirmation(ui: &mut ClusterUi) {
        for ch in "draft".chars() {
            ui.key(crossterm::event::KeyCode::Char(ch));
        }
        ui.key(crossterm::event::KeyCode::Enter);
        assert!(ui.notice.as_ref().is_some_and(|(notice, _)| {
            notice.contains("appended to the leftover fragment and run with it")
                && notice.contains("press Enter again to send anyway")
        }));
    }

    #[test]
    fn uncertain_remote_target_warns_on_its_next_send_once() {
        let (clock, mut ui, transport) = ui_with_uncertain_remote_target();
        let failure_notice = ui.notice.as_ref().map(|(notice, _)| notice.as_str());
        assert!(failure_notice.is_some());
        assert_eq!(
            ui.render(100, 24, "", &clock)
                .matches(failure_notice.unwrap())
                .count(),
            1
        );

        ui.select_target(Some("fp-tablet/build")).unwrap();
        ui.enter_selected(clock.now());
        ui.enqueue_remote_draft(b"tablet line\r".to_vec(), clock.now());
        *transport.response.lock().unwrap() = Some(Response::Ack { duplicate: false });
        ui.start_pending(clock.now());
        ui.send_pending(
            std::path::Path::new("unused"),
            clock.now(),
            Some(&transport),
        );
        assert!(!ui
            .notice
            .as_ref()
            .is_some_and(|(notice, _)| notice.contains("partial input may remain")));

        ui.select_target(Some("fp-laptop/build")).unwrap();
        ui.enter_selected(clock.now());
        for ch in "next line".chars() {
            ui.key(crossterm::event::KeyCode::Char(ch));
        }
        *transport.response.lock().unwrap() = Some(Response::Ack { duplicate: false });
        let request_count = transport.requests.lock().unwrap().len();
        ui.key(crossterm::event::KeyCode::Enter);
        ui.start_pending(clock.now());
        ui.send_pending(
            std::path::Path::new("unused"),
            clock.now(),
            Some(&transport),
        );
        assert_eq!(transport.requests.lock().unwrap().len(), request_count);
        assert!(ui
            .notice
            .as_ref()
            .is_some_and(|(notice, _)| notice.contains("press Enter again to send anyway")));

        ui.key(crossterm::event::KeyCode::Enter);
        *transport.response.lock().unwrap() = Some(Response::Uncertain);
        ui.start_pending(clock.now());
        ui.send_pending(
            std::path::Path::new("unused"),
            clock.now(),
            Some(&transport),
        );
        assert_eq!(transport.requests.lock().unwrap().len(), request_count + 1);

        for ch in "retry line".chars() {
            ui.key(crossterm::event::KeyCode::Char(ch));
        }
        ui.key(crossterm::event::KeyCode::Enter);
        assert!(ui
            .notice
            .as_ref()
            .is_some_and(|(notice, _)| notice.contains("press Enter again to send anyway")));
        ui.start_pending(clock.now());
        ui.send_pending(
            std::path::Path::new("unused"),
            clock.now(),
            Some(&transport),
        );
        assert_eq!(transport.requests.lock().unwrap().len(), request_count + 1);

        ui.key(crossterm::event::KeyCode::Enter);
        *transport.response.lock().unwrap() = Some(Response::Ack { duplicate: false });
        ui.start_pending(clock.now());
        ui.send_pending(
            std::path::Path::new("unused"),
            clock.now(),
            Some(&transport),
        );
        assert_eq!(transport.requests.lock().unwrap().len(), request_count + 2);
        assert!(!ui
            .notice
            .as_ref()
            .is_some_and(|(notice, _)| notice.contains("partial input may remain")));

        for ch in "after success".chars() {
            ui.key(crossterm::event::KeyCode::Char(ch));
        }
        *transport.response.lock().unwrap() = Some(Response::Ack { duplicate: false });
        ui.key(crossterm::event::KeyCode::Enter);
        ui.start_pending(clock.now());
        ui.send_pending(
            std::path::Path::new("unused"),
            clock.now(),
            Some(&transport),
        );
        assert_eq!(transport.requests.lock().unwrap().len(), request_count + 3);
    }

    #[test]
    fn changing_remote_target_cancels_partial_input_confirmation() {
        let (clock, mut ui, transport) = ui_with_uncertain_remote_target();
        let request_count = transport.requests.lock().unwrap().len();

        for ch in "draft".chars() {
            ui.key(crossterm::event::KeyCode::Char(ch));
        }
        ui.key(crossterm::event::KeyCode::Enter);
        assert!(ui
            .notice
            .as_ref()
            .is_some_and(|(notice, _)| notice.contains("press Enter again to send anyway")));

        ui.select_target(Some("fp-tablet/build")).unwrap();
        ui.enter_selected(clock.now());
        ui.select_target(Some("fp-laptop/build")).unwrap();
        ui.enter_selected(clock.now());
        for ch in "fresh draft".chars() {
            ui.key(crossterm::event::KeyCode::Char(ch));
        }
        ui.key(crossterm::event::KeyCode::Enter);
        assert!(ui
            .notice
            .as_ref()
            .is_some_and(|(notice, _)| notice.contains("press Enter again to send anyway")));
        ui.start_pending(clock.now());
        ui.send_pending(
            std::path::Path::new("unused"),
            clock.now(),
            Some(&transport),
        );
        assert_eq!(transport.requests.lock().unwrap().len(), request_count);
    }

    #[test]
    fn editing_draft_disarms_partial_input_confirmation() {
        let (clock, mut ui, transport) = ui_with_uncertain_remote_target();
        let request_count = transport.requests.lock().unwrap().len();
        arm_partial_input_confirmation(&mut ui);

        ui.key(crossterm::event::KeyCode::Backspace);
        *transport.response.lock().unwrap() = Some(Response::Ack { duplicate: false });
        ui.key(crossterm::event::KeyCode::Enter);
        ui.start_pending(clock.now());
        ui.send_pending(
            std::path::Path::new("unused"),
            clock.now(),
            Some(&transport),
        );

        assert_eq!(transport.requests.lock().unwrap().len(), request_count);
        assert!(ui
            .notice
            .as_ref()
            .is_some_and(|(notice, _)| notice.contains("press Enter again to send anyway")));
    }

    #[test]
    fn detach_disarms_partial_input_confirmation() {
        let (clock, mut ui, transport) = ui_with_uncertain_remote_target();
        let request_count = transport.requests.lock().unwrap().len();
        arm_partial_input_confirmation(&mut ui);

        ui.key_event(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('\\'),
                crossterm::event::KeyModifiers::CONTROL,
            ),
            clock.now(),
        );
        ui.key(crossterm::event::KeyCode::Enter);
        *transport.response.lock().unwrap() = Some(Response::Ack { duplicate: false });
        ui.key(crossterm::event::KeyCode::Enter);
        ui.start_pending(clock.now());
        ui.send_pending(
            std::path::Path::new("unused"),
            clock.now(),
            Some(&transport),
        );

        assert_eq!(transport.requests.lock().unwrap().len(), request_count);
        assert!(ui
            .notice
            .as_ref()
            .is_some_and(|(notice, _)| notice.contains("press Enter again to send anyway")));
    }

    #[test]
    fn replacing_partial_warning_disarms_confirmation() {
        let (clock, mut ui, transport) = ui_with_uncertain_remote_target();
        let request_count = transport.requests.lock().unwrap().len();
        arm_partial_input_confirmation(&mut ui);
        ui.notice = Some(("another notice replaced the warning".into(), clock.now()));

        *transport.response.lock().unwrap() = Some(Response::Ack { duplicate: false });
        ui.key(crossterm::event::KeyCode::Enter);
        ui.start_pending(clock.now());
        ui.send_pending(
            std::path::Path::new("unused"),
            clock.now(),
            Some(&transport),
        );

        assert_eq!(transport.requests.lock().unwrap().len(), request_count);
        assert!(ui
            .notice
            .as_ref()
            .is_some_and(|(notice, _)| notice.contains("press Enter again to send anyway")));
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
    fn empty_cluster_tree_shows_start_hint_only_when_all_nodes_have_no_sessions() {
        let clock = ManualClock::new();
        let mut empty_ui = ClusterUi::new("studio", vec![], clock.now());
        let empty_remote = FakeRemoteSource(Mutex::new(remote_snapshot(
            RemoteState::Unreachable,
            Duration::from_secs(12),
            None,
        )));
        empty_remote.0.lock().unwrap().nodes[0].sessions.clear();
        empty_ui.remote_synced(&empty_remote);

        let frame = empty_ui.render(100, 24, "", &clock);

        assert!(
            frame.contains("No sessions yet. Start one on any machine: remuda run -n NAME COMMAND")
        );

        let mut stale_ui = ClusterUi::new("studio", vec![], clock.now());
        let stale_remote = FakeRemoteSource(Mutex::new(remote_snapshot(
            RemoteState::Unreachable,
            Duration::from_secs(12),
            None,
        )));
        stale_ui.remote_synced(&stale_remote);
        stale_ui.remote_expanded.insert("fp-laptop".into());

        let frame = stale_ui.render(100, 24, "", &clock);

        assert!(frame.contains("build"));
        assert!(!frame.contains("No sessions yet."));
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
    fn escape_returns_from_remote_composer_to_the_read_only_view() {
        let clock = ManualClock::new();
        let source = FakeRemoteSource(Mutex::new(remote_snapshot(
            RemoteState::Reachable,
            Duration::ZERO,
            Some(remote_screen("remote")),
        )));
        let mut ui = ClusterUi::new("studio", sessions(), clock.now());
        ui.remote_synced(&source);
        ui.remote_input_enabled = true;
        ui.select_target(Some("fp-laptop/build")).unwrap();
        ui.enter_selected(clock.now());
        assert!(ui.composer_focused);

        let focused = ui.render(100, 24, "", &clock);
        assert!(focused.contains("Enter send · Ctrl-C clear · Esc list"));

        ui.key_event(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            ),
            clock.now(),
        );

        assert!(!ui.composer_focused);
        assert!(ui
            .render(100, 24, "", &clock)
            .contains("Remote session is read-only · q detach"));
    }

    #[test]
    fn remote_session_keeps_its_last_screen_when_unreachable_and_close_can_be_cancelled() {
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
        assert!(frame.contains("kill laptop(fp-lap)/build? it is running — y / n"));
        ui.key(crossterm::event::KeyCode::Char('n'));
        assert!(!ui
            .render(100, 24, "", &clock)
            .contains("kill laptop(fp-lap)/build?"));
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
    fn user_target_with_an_ambiguous_remote_label_is_refused() {
        let mut ui = ClusterUi::new("studio", Vec::new(), Duration::ZERO);
        let mut snapshot = remote_snapshot(
            RemoteState::Reachable,
            Duration::ZERO,
            Some(remote_screen("ready")),
        );
        let mut colliding = snapshot.nodes[0].clone();
        colliding.registry_key = "fp-other".into();
        snapshot.nodes.push(colliding);
        ui.remote_snapshot = snapshot;

        let error = ui.select_target(Some("laptop/build")).unwrap_err();
        assert!(error.to_string().contains("ambiguous"));
    }

    #[test]
    fn x_on_highlighted_remote_session_opens_the_close_confirmation() {
        let clock = ManualClock::new();
        let mut ui = ClusterUi::new("studio", sessions(), clock.now());
        let mut snapshot = remote_snapshot(
            RemoteState::Reachable,
            Duration::ZERO,
            Some(remote_screen("remote")),
        );
        snapshot.nodes[0].registry_key = "SHA256:RL+gtaJ8...".into();
        snapshot.nodes[0].sessions[0].wire_name = "wire-build".into();
        ui.remote_synced(&FakeRemoteSource(Mutex::new(snapshot)));
        ui.remote_selected = Some(RemoteSelection::Session {
            node: "SHA256:RL+gtaJ8...".into(),
            name: "build".into(),
            instance_id: "remote-instance".into(),
        });

        ui.key(crossterm::event::KeyCode::Char('x'));

        assert!(ui
            .render(100, 24, "", &clock)
            .contains("kill laptop(RL+gta)/build? it is running — y / n"));

        ui.key(crossterm::event::KeyCode::Char('y'));
        let transport = FakeRemoteInput::new(Response::RemoteControlDisabled);
        ui.send_close_pending(
            std::path::Path::new("unused"),
            Some(&transport),
            clock.now(),
        );

        assert_eq!(
            transport.requests.lock().unwrap().as_slice(),
            &[(
                "SHA256:RL+gtaJ8...".into(),
                Request::Close {
                    name: "wire-build".into(),
                    instance_id: Some("remote-instance".into()),
                    confirm: Some(true),
                }
            )]
        );
        assert_eq!(
            ui.notice.as_ref().unwrap().0,
            "remote control disabled on laptop"
        );
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
    fn fingerprint_target_sends_input_when_remote_labels_collide() {
        let clock = ManualClock::new();
        let mut ui = ClusterUi::new("studio", sessions(), clock.now());
        let mut snapshot = remote_snapshot(
            RemoteState::Reachable,
            Duration::ZERO,
            Some(remote_screen("remote")),
        );
        let mut colliding = snapshot.nodes[0].clone();
        colliding.registry_key = "fp-other".into();
        colliding.sessions[0].instance_id = "other-instance".into();
        snapshot.nodes.push(colliding);
        snapshot.nodes[0].sessions[0].wire_name = "wire-build".into();
        ui.remote_synced(&FakeRemoteSource(Mutex::new(snapshot)));
        ui.remote_input_enabled = true;
        ui.select_target(Some("fp-laptop/build")).unwrap();
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
        assert_eq!(sent[0].0, "fp-laptop");
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
        ui.remote_expanded.insert("fp-laptop".into());
        ui.remote_selected = Some(RemoteSelection::Session {
            node: "fp-laptop".into(),
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
            node: "fp-laptop".into(),
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
        assert_eq!(ui.remote_composer_target.as_ref().unwrap().2, "shell");
    }

    #[test]
    fn reentering_unavailable_remote_target_discards_draft_before_target_clear() {
        let clock = ManualClock::new();
        let source = FakeRemoteSource(Mutex::new(remote_snapshot(
            RemoteState::Reachable,
            Duration::ZERO,
            Some(remote_screen("remote")),
        )));
        let mut ui = ClusterUi::new("studio", sessions(), clock.now());
        ui.remote_synced(&source);
        ui.remote_input_enabled = true;
        ui.remote_expanded.insert("fp-laptop".into());
        ui.remote_selected = Some(RemoteSelection::Session {
            node: "fp-laptop".into(),
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

        let mut unavailable = remote_snapshot(
            RemoteState::Reachable,
            Duration::ZERO,
            Some(remote_screen("remote")),
        );
        unavailable.nodes[0].sessions[0].alive = false;
        source.replace(unavailable);
        ui.remote_synced(&source);
        ui.enter_selected(clock.now());

        assert_eq!(ui.composer.text(), "");
        assert_eq!(ui.remote_composer_target, None);
        assert_eq!(
            ui.notice.as_ref().map(|(notice, _)| notice.as_str()),
            Some("draft for laptop/build discarded")
        );
        assert_eq!(ui.input_queue.items().count(), 0);
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
        ui.remote_expanded.insert("fp-laptop".into());
        ui.input_sender
            .enqueue_remote(
                &mut ui.input_queue,
                "fp-laptop",
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
        ui.remote_expanded.insert("fp-laptop".into());
        ui.remote_selected = Some(RemoteSelection::Session {
            node: "fp-laptop".into(),
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
    fn failed_remote_paste_chunk_drops_later_chunks_with_one_notice() {
        let clock = ManualClock::new();
        let limit = crate::remote_front::MAX_REMOTE_INPUT_BATCH_BYTES;
        let oversize = Response::Error(format!("remote Input batch exceeds {limit} bytes"));
        let mut ui = ClusterUi::new("studio", sessions(), clock.now());
        let mut snapshot = remote_snapshot(
            RemoteState::Reachable,
            Duration::ZERO,
            Some(remote_screen("remote")),
        );
        let mut other = snapshot.nodes[0].clone();
        other.registry_key = "fp-tablet".into();
        other.name = "tablet".into();
        other.sessions[0].instance_id = "tablet-instance".into();
        snapshot.nodes.push(other);
        ui.remote_synced(&FakeRemoteSource(Mutex::new(snapshot)));
        ui.remote_input_enabled = true;
        ui.select_target(Some("fp-laptop/build")).unwrap();
        ui.enter_selected(clock.now());
        ui.enqueue_remote_draft(vec![b'x'; limit * 2 + 7], clock.now());
        ui.enqueue_remote_draft(b"later\r".to_vec(), clock.now());
        ui.select_target(Some("fp-tablet/build")).unwrap();
        ui.enter_selected(clock.now());
        ui.enqueue_remote_draft(b"other\r".to_vec(), clock.now());
        let transport = FakeRemoteInput::new(oversize.clone());

        for attempt in 0..=MAX_IO_RETRIES {
            if attempt > 0 {
                std::thread::sleep(Duration::from_secs(1));
            }
            *transport.response.lock().unwrap() = Some(oversize.clone());
            ui.start_pending(clock.now());
            ui.send_pending(
                std::path::Path::new("unused"),
                clock.now(),
                Some(&transport),
            );
        }

        let requests = transport.requests.lock().unwrap();
        let first_request = requests
            .first()
            .expect("first paste chunk was sent")
            .1
            .clone();
        let Request::Input {
            client_id: first_client,
            bytes: first_bytes,
            ..
        } = &first_request
        else {
            panic!("remote composer sent a non-Input request");
        };
        assert!(
            requests.iter().all(|(_, request)| !matches!(
                request,
                Request::Input { client_id, bytes, .. }
                    if bytes.iter().all(|byte| *byte == b'x')
                        && bytes.len() <= first_bytes.len()
                        && client_id != first_client
            )),
            "a later paste chunk was sent after the first chunk failed"
        );
        drop(requests);

        for _ in 0..2 {
            *transport.response.lock().unwrap() = Some(Response::Ack { duplicate: false });
            ui.start_pending(clock.now());
            ui.send_pending(
                std::path::Path::new("unused"),
                clock.now(),
                Some(&transport),
            );
        }
        let notice = ui.notice.as_ref().map(|(notice, _)| notice.as_str());
        assert!(notice.is_some_and(|notice| notice.contains("remaining lines dropped")));
        let frame = ui.render(100, 24, "", &clock);
        assert_eq!(frame.matches(notice.unwrap()).count(), 1);
        let requests = transport.requests.lock().unwrap();
        assert!(!requests.iter().any(|(node, request)| {
            node == "fp-laptop"
                && matches!(request, Request::Input { bytes, .. } if bytes == b"later\r")
        }));
        assert!(requests.iter().any(|(node, request)| {
            node == "fp-tablet"
                && matches!(request, Request::Input { bytes, .. } if bytes == b"other\r")
        }));
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
        let pending = ui.pending_close.take().unwrap();
        assert_eq!(
            (pending.name.clone(), pending.instance_id.clone()),
            ("dev".into(), "instance-dev".into())
        );
        assert!(pending.remote_target.is_none());
        ui.close_completed(
            None,
            pending.display_name,
            pending.name,
            pending.instance_id,
            Ok(Response::Ok),
            clock.now(),
        );
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
