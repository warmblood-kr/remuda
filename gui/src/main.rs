use gpui::{
    div, prelude::*, px, rgb, size, App, Application, Bounds, Context, SharedString, Window,
    WindowBounds, WindowOptions,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    sync::mpsc,
    time::{Duration, Instant},
};

mod browser;
#[allow(dead_code)]
mod input_helper;
mod preview;
mod provider;
#[cfg(any(target_os = "macos", windows))]
mod tray;

const WORKER_ABI: &str = "--remuda-gui-worker-abi-1";
const STALE_HELPER: &str = "GUI helper version changed; restart the GUI";

#[derive(Serialize, Deserialize)]
struct WorkerReply<T> {
    abi: String,
    payload: T,
}

fn write_worker_reply<T: Serialize>(payload: &T, tagged: bool) -> i32 {
    let result = if tagged {
        serde_json::to_writer(
            std::io::stdout(),
            &WorkerReply {
                abi: WORKER_ABI.to_owned(),
                payload,
            },
        )
    } else {
        serde_json::to_writer(std::io::stdout(), payload)
    };
    i32::from(result.is_err())
}

fn read_worker_reply<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    let reply: WorkerReply<T> =
        serde_json::from_slice(bytes).map_err(|_| STALE_HELPER.to_string())?;
    if reply.abi != WORKER_ABI {
        return Err(STALE_HELPER.to_string());
    }
    Ok(reply.payload)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Session {
    name: String,
    instance_id: Option<String>,
    alive: bool,
    unavailable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Peer {
    fingerprint: String,
    label: String,
    admitted: bool,
}

fn worker_deadline() {
    std::thread::spawn(|| {
        std::thread::sleep(provider::TIMEOUT);
        std::process::exit(124);
    });
}

// This worker makes only a List request. Its own deadline survives GUI exit,
// including when native IPC setup stalls before the client's read timeout.
fn worker(tagged: bool) -> i32 {
    worker_deadline();
    let path = remuda_native::daemon::socket_path("default");
    let rows = match remuda_native::client::request(&path, &remuda_core::protocol::Request::List) {
        Ok(remuda_core::protocol::Response::Sessions(rows)) => rows,
        _ => return 1,
    };
    let sessions: Vec<_> = rows
        .into_iter()
        .map(|s| Session {
            name: s.name,
            instance_id: s.instance_id,
            alive: s.alive,
            unavailable: s.unavailable,
        })
        .collect();
    write_worker_reply(&sessions, tagged)
}

fn peer_is_unique(peers: &[Peer], fingerprint: &str) -> bool {
    !fingerprint.is_empty()
        && peers
            .iter()
            .filter(|p| p.fingerprint == fingerprint)
            .count()
            == 1
        && peers
            .iter()
            .any(|p| p.fingerprint == fingerprint && p.admitted)
}

// A fingerprint is resolved again immediately before the authenticated request.
fn peer_worker(fingerprint: &str, tagged: bool) -> i32 {
    use remuda_core::protocol::{Request, Response};
    use remuda_native::{
        cluster,
        cluster_api::{resolve_call, ResolvedCall},
        net::cluster_client::ClusterClient,
    };
    worker_deadline();
    let list = || -> Result<Vec<Session>, ()> {
        if !peer_is_unique(&discover_peers().map_err(|_| ())?, fingerprint) {
            return Err(());
        }
        let ResolvedCall::Remote {
            fingerprint: resolved,
            address,
            pinned_static_key,
        } = resolve_call(fingerprint, None).map_err(|_| ())?
        else {
            return Err(());
        };
        if resolved != fingerprint {
            return Err(());
        }
        let private = cluster::identity::load_static_private_key().map_err(|_| ())?;
        let Response::Sessions(rows) = ClusterClient::system()
            .request(address, &pinned_static_key, &private, &Request::List)
            .map_err(|_| ())?
        else {
            return Err(());
        };
        if !peer_is_unique(&discover_peers().map_err(|_| ())?, fingerprint) {
            return Err(());
        }
        Ok(rows
            .into_iter()
            .map(|s| Session {
                name: s.name,
                instance_id: s.instance_id,
                alive: s.alive,
                unavailable: s.unavailable,
            })
            .collect())
    };
    match list() {
        Ok(rows) => write_worker_reply(&rows, tagged),
        Err(()) => 1,
    }
}

// A missing cluster is the normal local-only case; other errors are not.
fn discover_peers() -> std::io::Result<Vec<Peer>> {
    remuda_native::cluster::snapshot().map(peers_from_snapshot)
}

fn peers_from_snapshot(snapshot: Option<remuda_native::cluster::ClusterSnapshot>) -> Vec<Peer> {
    use remuda_native::cluster::{self, NodeState};
    let Some(snapshot) = snapshot else {
        return vec![];
    };
    snapshot
        .registry
        .authorized_nodes
        .iter()
        .filter(|node| !node.node_fp.is_empty() && node.node_fp != snapshot.identity.node_fp)
        .map(|node| Peer {
            fingerprint: node.node_fp.clone(),
            admitted: node.state == NodeState::Admitted,
            label: snapshot
                .aliases
                .get(&node.node_fp)
                .cloned()
                .unwrap_or_else(|| cluster::node_label(&node.node_fp)),
        })
        .collect()
}

fn nodes_worker(tagged: bool) -> i32 {
    worker_deadline();
    match discover_peers() {
        Ok(rows) => write_worker_reply(&rows, tagged),
        Err(_) => 1,
    }
}

fn label(value: &str) -> String {
    let mut text = String::from("\"");
    for ch in value.chars() {
        if ch == '\\' || ch == '"' {
            text.push('\\');
            text.push(ch);
        } else if ch.is_control() || remuda_core::protocol::is_secret_prompt_format_or_separator(ch)
        {
            text.push_str(&format!("\\u{{{:x}}}", ch as u32));
        } else {
            text.push(ch);
        }
    }
    text.push('"');
    text
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Selection {
    peer: Option<String>,
    name: String,
    instance: String,
}
type PreviewResult = (u64, Selection, Result<String, String>);

type RemoteResult = (u64, u64, String, Result<Vec<Session>, String>);

#[derive(Clone, Debug, PartialEq, Eq)]
enum TreeRow {
    Node(Option<String>),
    Session(Option<String>, String, Option<String>),
}

#[derive(Default)]
struct PeerSessions {
    rows: Vec<Session>,
    loading: bool,
    error: bool,
    generation: u64,
}

struct LocalGui {
    tree_focus: Option<gpui::FocusHandle>,
    focused_row: TreeRow,
    local_expanded: bool,
    expanded_peers: std::collections::HashSet<String>,
    preview_received: Option<Instant>,
    helper_changed: bool,
    selection: Option<Selection>,
    preview: Option<String>,
    preview_generation: u64,
    preview_busy: bool,
    preview_pending: Option<(u64, Selection)>,
    preview_sender: mpsc::Sender<PreviewResult>,
    preview_results: mpsc::Receiver<PreviewResult>,
    sessions: Vec<Session>,
    error: bool,
    loading: bool,
    generation: u64,
    busy: bool,
    pending: bool,
    sender: mpsc::Sender<(u64, Result<Vec<Session>, String>)>,
    results: mpsc::Receiver<(u64, Result<Vec<Session>, String>)>,
    peers: Vec<Peer>,
    nodes_loading: bool,
    nodes_error: bool,
    nodes_generation: u64,
    nodes_busy: bool,
    nodes_pending: bool,
    nodes_sender: mpsc::Sender<(u64, Result<Vec<Peer>, String>)>,
    nodes_results: mpsc::Receiver<(u64, Result<Vec<Peer>, String>)>,
    remote_sessions: HashMap<String, PeerSessions>,
    remote_generation: u64,
    remote_busy: bool,
    remote_pending: VecDeque<(u64, u64, String)>,
    remote_sender: mpsc::Sender<RemoteResult>,
    remote_results: mpsc::Receiver<RemoteResult>,
}
impl LocalGui {
    fn new() -> Self {
        let (preview_sender, preview_results) = mpsc::channel();
        let (sender, results) = mpsc::channel();
        let (nodes_sender, nodes_results) = mpsc::channel();
        let (remote_sender, remote_results) = mpsc::channel();
        Self {
            tree_focus: None,
            focused_row: TreeRow::Node(None),
            local_expanded: true,
            expanded_peers: Default::default(),
            preview_received: None,
            helper_changed: false,
            selection: None,
            preview: None,
            preview_generation: 0,
            preview_busy: false,
            preview_pending: None,
            preview_sender,
            preview_results,
            sessions: vec![],
            error: false,
            loading: false,
            generation: 0,
            busy: false,
            pending: false,
            sender,
            results,
            peers: vec![],
            nodes_loading: false,
            nodes_error: false,
            nodes_generation: 0,
            nodes_busy: false,
            nodes_pending: false,
            nodes_sender,
            nodes_results,
            remote_sessions: HashMap::new(),
            remote_generation: 0,
            remote_busy: false,
            remote_pending: VecDeque::new(),
            remote_sender,
            remote_results,
        }
    }
    fn clear_preview(&mut self) {
        self.preview_generation = self
            .preview_generation
            .checked_add(1)
            .expect("generation exhausted");
        self.selection = None;
        self.preview = None;
        self.preview_received = None;
        self.preview_pending = None;
    }
    fn selection_current(&self, s: &Selection) -> bool {
        let rows = if let Some(fp) = &s.peer {
            let Some(sessions) = self.remote_sessions.get(fp) else {
                return false;
            };
            if !self.peer_current(fp) || sessions.loading || sessions.error {
                return false;
            }
            &sessions.rows
        } else {
            if self.loading || self.error {
                return false;
            }
            &self.sessions
        };
        let matching: Vec<_> = rows.iter().filter(|r| r.name == s.name).collect();
        matching.len() == 1
            && matching[0].alive
            && !matching[0].unavailable
            && !s.instance.is_empty()
            && matching[0].instance_id.as_deref() == Some(s.instance.as_str())
    }
    fn select_session(&mut self, peer: Option<String>, row: Session) {
        self.clear_preview();
        let Some(instance) = row.instance_id else {
            return;
        };
        let s = Selection {
            peer,
            name: row.name,
            instance,
        };
        if !self.selection_current(&s) {
            return;
        }
        self.selection = Some(s.clone());
        self.preview_pending = Some((self.preview_generation, s));
        self.launch_preview();
    }
    fn launch_preview(&mut self) {
        if self.preview_busy {
            return;
        }
        let Some((generation, selection)) = self.preview_pending.take() else {
            return;
        };
        if self.selection.as_ref() != Some(&selection) || !self.selection_current(&selection) {
            return;
        }
        self.preview_busy = true;
        let sender = self.preview_sender.clone();
        std::thread::spawn(move || {
            let peer = selection.peer.as_deref().unwrap_or("");
            let kind = if selection.peer.is_some() {
                "peer"
            } else {
                "local"
            };
            let result = std::env::current_exe()
                .map_err(|e| e.to_string())
                .and_then(|path| {
                    provider::output(
                        &path,
                        &[
                            "--internal-preview-worker",
                            kind,
                            peer,
                            &selection.name,
                            &selection.instance,
                            WORKER_ABI,
                        ],
                        provider::TIMEOUT,
                    )
                })
                .and_then(|bytes| read_worker_reply::<String>(&bytes))
                .and_then(|s| {
                    if s.len() <= 65_536 {
                        Ok(s)
                    } else {
                        Err("too large".into())
                    }
                });
            let _ = sender.send((generation, selection, result));
        });
    }
    fn refresh_nodes(&mut self) {
        self.clear_preview();
        self.remote_pending.clear();
        for sessions in self.remote_sessions.values_mut() {
            sessions.loading = false;
            sessions.error = true;
        }
        self.nodes_generation = self
            .nodes_generation
            .checked_add(1)
            .expect("generation exhausted");
        self.nodes_loading = true;
        self.nodes_pending = true;
        self.launch_nodes();
    }
    fn launch_nodes(&mut self) {
        if self.nodes_busy || !self.nodes_pending {
            return;
        }
        self.nodes_busy = true;
        self.nodes_pending = false;
        let generation = self.nodes_generation;
        let sender = self.nodes_sender.clone();
        std::thread::spawn(move || {
            let result = std::env::current_exe()
                .map_err(|e| e.to_string())
                .and_then(|path| {
                    provider::output(
                        &path,
                        &["--internal-nodes-worker", WORKER_ABI],
                        provider::TIMEOUT,
                    )
                })
                .and_then(|bytes| read_worker_reply(&bytes));
            let _ = sender.send((generation, result));
        });
    }
    fn peer_current(&self, fingerprint: &str) -> bool {
        !self.nodes_loading && !self.nodes_error && peer_is_unique(&self.peers, fingerprint)
    }
    fn set_expanded(&mut self, peer: Option<String>, expanded: bool) {
        match peer {
            None => self.local_expanded = expanded,
            Some(fp) if expanded && self.peer_current(&fp) => {
                if self.expanded_peers.insert(fp.clone()) {
                    self.refresh_peer(fp);
                }
            }
            Some(fp) if !expanded => {
                self.expanded_peers.remove(&fp);
            }
            Some(_) => {}
        }
    }
    fn is_expanded(&self, peer: Option<&str>) -> bool {
        peer.map_or(self.local_expanded, |fp| self.expanded_peers.contains(fp))
    }
    fn refresh_peer(&mut self, fp: String) {
        if !self.peer_current(&fp) {
            return;
        }
        self.remote_generation = self
            .remote_generation
            .checked_add(1)
            .expect("generation exhausted");
        let sessions = self.remote_sessions.entry(fp.clone()).or_default();
        sessions.loading = true;
        sessions.generation = self.remote_generation;
        self.remote_pending.retain(|(_, _, queued)| queued != &fp);
        self.remote_pending
            .push_back((self.remote_generation, self.nodes_generation, fp));
        self.launch_peer();
    }
    fn launch_peer(&mut self) {
        if self.remote_busy {
            return;
        }
        let (generation, directory, fp) = loop {
            let Some((generation, directory, fp)) = self.remote_pending.pop_front() else {
                return;
            };
            if directory == self.nodes_generation && self.peer_current(&fp) {
                break (generation, directory, fp);
            }
        };
        self.remote_busy = true;
        let sender = self.remote_sender.clone();
        std::thread::spawn(move || {
            let result = std::env::current_exe()
                .map_err(|e| e.to_string())
                .and_then(|path| {
                    provider::output(
                        &path,
                        &["--internal-peer-worker", &fp, WORKER_ABI],
                        provider::TIMEOUT,
                    )
                })
                .and_then(|bytes| read_worker_reply(&bytes));
            let _ = sender.send((generation, directory, fp, result));
        });
    }
    fn refresh(&mut self) {
        self.clear_preview();
        self.generation = self
            .generation
            .checked_add(1)
            .expect("generation exhausted");
        self.loading = true;
        self.pending = true;
        self.launch();
    }
    fn launch(&mut self) {
        if self.busy || !self.pending {
            return;
        }
        self.busy = true;
        self.pending = false;
        let generation = self.generation;
        let sender = self.sender.clone();
        std::thread::spawn(move || {
            #[cfg(test)]
            let executable = std::env::var_os("REMUDA_GUI_ACCEPTANCE_WORKER")
                .map(std::path::PathBuf::from)
                .map(Ok)
                .unwrap_or_else(std::env::current_exe);
            #[cfg(not(test))]
            let executable = std::env::current_exe();
            let result = executable
                .map_err(|e| e.to_string())
                .and_then(|path| {
                    provider::output(
                        &path,
                        &["--internal-list-worker", WORKER_ABI],
                        provider::TIMEOUT,
                    )
                })
                .and_then(|bytes| read_worker_reply(&bytes));
            let _ = sender.send((generation, result));
        });
    }
    fn drain(&mut self) {
        while let Ok((generation, selection, result)) = self.preview_results.try_recv() {
            self.preview_busy = false;
            if generation == self.preview_generation
                && self.selection.as_ref() == Some(&selection)
                && self.selection_current(&selection)
            {
                self.preview_received = result.as_ref().ok().map(|_| Instant::now());
                self.preview = Some(result.unwrap_or_else(|error| {
                    if error == STALE_HELPER {
                        self.helper_changed = true;
                        return STALE_HELPER.into();
                    }
                    "Snapshot unavailable. The session or node may have disconnected.".into()
                }));
            }
            self.launch_preview();
        }
        while let Ok((generation, result)) = self.results.try_recv() {
            self.busy = false;
            if generation == self.generation {
                self.loading = false;
                match result {
                    Ok(rows) => {
                        self.sessions = rows;
                        self.error = false;
                    }
                    Err(error) => {
                        self.helper_changed |= error == STALE_HELPER;
                        self.error = true;
                    }
                }
            }
            self.launch();
        }
        while let Ok((generation, result)) = self.nodes_results.try_recv() {
            self.nodes_busy = false;
            if generation == self.nodes_generation {
                self.nodes_loading = false;
                match result {
                    Ok(peers) => {
                        self.peers = peers;
                        self.nodes_error = false;
                    }
                    Err(error) => {
                        self.helper_changed |= error == STALE_HELPER;
                        self.nodes_error = true;
                    }
                }
                if !self.nodes_error {
                    let peers = &self.peers;
                    self.expanded_peers.retain(|fp| peer_is_unique(peers, fp));
                    self.remote_sessions
                        .retain(|fp, _| peer_is_unique(peers, fp));
                    let expanded = self
                        .peers
                        .iter()
                        .filter(|p| self.expanded_peers.contains(&p.fingerprint))
                        .map(|p| p.fingerprint.clone())
                        .collect::<Vec<_>>();
                    for fp in expanded {
                        self.refresh_peer(fp);
                    }
                }
            }
            self.launch_nodes();
        }
        while let Ok((generation, directory, fp, result)) = self.remote_results.try_recv() {
            self.remote_busy = false;
            if directory == self.nodes_generation && self.peer_current(&fp) {
                if let Some(sessions) = self
                    .remote_sessions
                    .get_mut(&fp)
                    .filter(|state| state.generation == generation)
                {
                    sessions.loading = false;
                    match result {
                        Ok(rows) => {
                            sessions.rows = rows;
                            sessions.error = false;
                        }
                        Err(error) => {
                            self.helper_changed |= error == STALE_HELPER;
                            sessions.error = true;
                        }
                    }
                }
            }
            self.launch_peer();
        }
    }
}
impl LocalGui {
    fn tree_rows(&self) -> Vec<TreeRow> {
        let mut rows = vec![TreeRow::Node(None)];
        if self.local_expanded {
            rows.extend(self.sessions.iter().map(|row| TreeRow::session(None, row)));
        }
        for peer in &self.peers {
            let fp = Some(peer.fingerprint.clone());
            rows.push(TreeRow::Node(fp.clone()));
            if self.expanded_peers.contains(&peer.fingerprint) {
                if let Some(sessions) = self.remote_sessions.get(&peer.fingerprint) {
                    rows.extend(
                        sessions
                            .rows
                            .iter()
                            .map(|row| TreeRow::session(fp.clone(), row)),
                    );
                }
            }
        }
        rows
    }

    fn open_focused_session(&mut self, peer: Option<String>, name: &str, instance: Option<&str>) {
        let rows = match peer.as_deref() {
            Some(fp) => self
                .remote_sessions
                .get(fp)
                .map(|state| state.rows.as_slice())
                .unwrap_or_default(),
            None => &self.sessions,
        };
        if let Some(row) = rows
            .iter()
            .find(|row| row.name == name && row.instance_id.as_deref() == instance)
            .cloned()
        {
            self.select_session(peer, row);
        }
    }

    fn tree_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rows = self.tree_rows();
        let position = rows
            .iter()
            .position(|row| row == &self.focused_row)
            .unwrap_or_default();
        let current = rows[position].clone();
        match (event.keystroke.key.as_str(), current) {
            ("up", _) => self.focused_row = rows[position.saturating_sub(1)].clone(),
            ("down", _) => self.focused_row = rows[(position + 1).min(rows.len() - 1)].clone(),
            ("left", TreeRow::Node(peer) | TreeRow::Session(peer, _, _)) => {
                self.focused_row = TreeRow::Node(peer.clone());
                self.set_expanded(peer, false);
            }
            ("right", TreeRow::Node(peer)) => self.set_expanded(peer, true),
            ("enter" | "space", TreeRow::Node(peer)) => {
                let expanded = self.is_expanded(peer.as_deref());
                self.set_expanded(peer, !expanded);
            }
            ("enter" | "space", TreeRow::Session(peer, name, instance)) => {
                self.open_focused_session(peer, &name, instance.as_deref());
            }
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
        window.refresh();
    }

    fn render_node(
        &self,
        peer: Option<String>,
        text: String,
        status: &'static str,
        key: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let expanded = self.is_expanded(peer.as_deref());
        let can_open = expanded || peer.as_deref().map_or(true, |fp| self.peer_current(fp));
        let row = TreeRow::Node(peer.clone());
        let focused = self.focused_row == row;
        let debug = key.clone();
        div()
            .id(SharedString::from(key))
            .debug_selector(move || debug.clone())
            .flex()
            .items_center()
            .gap_2()
            .min_w_0()
            .px_2()
            .py_1()
            .rounded_sm()
            .when(focused, |d| d.bg(rgb(0x303a49)))
            .when(can_open, |d| {
                d.cursor_pointer().hover(|d| d.bg(rgb(0x2a3340)))
            })
            .child(
                div()
                    .flex_none()
                    .text_color(rgb(0x9bb0c6))
                    .child(if expanded { "▾" } else { "▸" }),
            )
            .child(div().flex_1().min_w_0().truncate().child(text))
            .when(!status.is_empty(), |d| {
                d.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(rgb(0xb5beca))
                        .child(status),
                )
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                if let Some(focus) = &this.tree_focus {
                    focus.focus(window);
                }
                this.focused_row = row.clone();
                let expanded = this.is_expanded(peer.as_deref());
                this.set_expanded(peer.clone(), !expanded);
                cx.notify();
            }))
    }

    fn render_node_sessions(
        &self,
        peer: Option<String>,
        key: &str,
        rows: &[Session],
        loading: bool,
        unavailable: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let focus = self.tree_focus.clone();
        div()
            .pl_5()
            .when(loading || unavailable || rows.is_empty(), |d| {
                d.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(rgb(0xb5beca))
                        .child(if loading {
                            "Loading sessions…"
                        } else if unavailable && !rows.is_empty() {
                            "Session list may be stale"
                        } else if unavailable {
                            "Sessions unavailable"
                        } else {
                            "No sessions"
                        }),
                )
            })
            .children(rows.iter().cloned().enumerate().map(|(index, session)| {
                let (status, dot) = if unavailable || session.unavailable {
                    ("Unavailable", 0xd1a163)
                } else if session.alive {
                    ("Running", 0x65d89b)
                } else {
                    ("Exited", 0x9ca8b6)
                };
                let active = self.selection.as_ref().is_some_and(|selection| {
                    selection.peer == peer
                        && selection.name == session.name
                        && session.instance_id.as_deref() == Some(&selection.instance)
                });
                let row = TreeRow::session(peer.clone(), &session);
                let focused = self.focused_row == row;
                let name = display_label(&session.name);
                let peer = peer.clone();
                let focus = focus.clone();
                let debug = format!("{key}-{index}");
                div()
                    .id(SharedString::from(debug.clone()))
                    .debug_selector(move || debug.clone())
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|d| d.bg(rgb(0x2a3340)))
                    .when(active, |d| d.bg(rgb(0x233e57)))
                    .when(focused, |d| d.border_1().border_color(rgb(0x799ebe)))
                    .child(div().flex_none().text_xs().text_color(rgb(dot)).child("●"))
                    .child(div().flex_1().min_w_0().truncate().child(name))
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(rgb(0xb5beca))
                            .child(status),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(focus) = &focus {
                            focus.focus(window);
                        }
                        this.focused_row = row.clone();
                        this.select_session(peer.clone(), session.clone());
                        cx.notify();
                    }))
            }))
    }
}

impl TreeRow {
    fn session(peer: Option<String>, row: &Session) -> Self {
        Self::Session(peer, row.name.clone(), row.instance_id.clone())
    }
}

fn display_label(value: &str) -> String {
    let escaped = label(value);
    escaped[1..escaped.len() - 1].to_owned()
}

impl Render for LocalGui {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self
            .tree_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        let peer = self
            .selection
            .as_ref()
            .and_then(|selection| selection.peer.as_deref())
            .and_then(|fp| self.peers.iter().find(|peer| peer.fingerprint == fp));
        let node = peer.map_or_else(|| "This device".into(), |peer| display_label(&peer.label));
        let title = self.selection.as_ref().map_or_else(
            || "Session preview".into(),
            |selection| format!("{node} / {}", display_label(&selection.name)),
        );
        let detail = if let Some(at) = self.preview_received {
            let status = if self
                .selection
                .as_ref()
                .is_some_and(|selection| !self.selection_current(selection))
            {
                "Last read-only snapshot · session not currently verified"
            } else {
                "Read-only snapshot"
            };
            format!("{status} · received {}s ago", at.elapsed().as_secs())
        } else if self.selection.is_some() && self.preview.is_none() {
            "Loading read-only snapshot…".into()
        } else {
            "Read-only snapshot".into()
        };
        div()
            .size_full()
            .flex()
            .bg(rgb(0x101216))
            .text_color(rgb(0xe8eaf0))
            .when(cfg!(target_os = "macos"), |root| root.font_family("Menlo"))
            .child(
                div()
                    .id("session-sidebar")
                    .w(px(300.))
                    .flex_none()
                    .h_full()
                    .flex()
                    .flex_col()
                    .bg(rgb(0x191d24))
                    .p_3()
                    .track_focus(&focus)
                    .on_key_down(cx.listener(Self::tree_key))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .pb_2()
                            .child(div().text_lg().child(if self.peers.is_empty() {
                                "Nodes"
                            } else {
                                "Cluster"
                            }))
                            .child(
                                div()
                                    .id("refresh")
                                    .debug_selector(|| "refresh".into())
                                    .px_2()
                                    .py_1()
                                    .cursor_pointer()
                                    .hover(|d| d.bg(rgb(0x2a3340)))
                                    .child("Refresh")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.refresh();
                                        this.refresh_nodes();
                                        cx.notify();
                                    })),
                            ),
                    )
                    .when(self.helper_changed, |d| {
                        d.child(
                            div()
                                .id("helper-changed")
                                .debug_selector(|| "helper-changed".into())
                                .child(STALE_HELPER),
                        )
                    })
                    .when(self.nodes_loading || self.nodes_error, |d| {
                        d.child(div().text_xs().pb_2().child(if self.nodes_loading {
                            "Loading nodes…"
                        } else {
                            "Node list may be stale"
                        }))
                    })
                    .child(
                        div()
                            .id("node-tree")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .child(self.render_node(
                                None,
                                "This device".into(),
                                "",
                                "node-local".into(),
                                cx,
                            ))
                            .when(self.local_expanded, |d| {
                                d.child(self.render_node_sessions(
                                    None,
                                    "session",
                                    &self.sessions,
                                    self.loading,
                                    self.error,
                                    cx,
                                ))
                            })
                            .children(self.peers.iter().enumerate().map(|(index, peer)| {
                                let fp = &peer.fingerprint;
                                let sessions = self.remote_sessions.get(fp);
                                let status = if !peer.admitted {
                                    "Revoked"
                                } else if sessions.is_some_and(|s| s.error) {
                                    "Unavailable"
                                } else {
                                    ""
                                };
                                div()
                                    .child(self.render_node(
                                        Some(fp.clone()),
                                        display_label(&peer.label),
                                        status,
                                        format!("node-peer-{index}"),
                                        cx,
                                    ))
                                    .when(self.expanded_peers.contains(fp), |d| {
                                        d.child(self.render_node_sessions(
                                            Some(fp.clone()),
                                            &format!("peer-session-{index}"),
                                            sessions.map(|s| s.rows.as_slice()).unwrap_or_default(),
                                            sessions.is_some_and(|s| s.loading),
                                            !self.peer_current(fp)
                                                || sessions.is_some_and(|s| s.error),
                                            cx,
                                        ))
                                    })
                            })),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .p_4()
                    .child(
                        div()
                            .id("preview-title")
                            .debug_selector(|| "preview-title".into())
                            .text_lg()
                            .truncate()
                            .child(title),
                    )
                    .child(div().text_sm().text_color(rgb(0xb5beca)).child(detail))
                    .when_some(peer, |d, peer| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(rgb(0xb5beca))
                                .truncate()
                                .child(display_label(&peer.fingerprint)),
                        )
                    })
                    .child(
                        div()
                            .id("preview")
                            .debug_selector(|| "preview".into())
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .pt_4()
                            .font_family(if cfg!(windows) { "Consolas" } else { "Menlo" })
                            .child(self.preview.clone().unwrap_or_else(|| {
                                if self.selection.is_some() {
                                    String::new()
                                } else {
                                    "Select a session to view its screen.".into()
                                }
                            })),
                    ),
            )
    }
}
fn main() {
    #[cfg(any(target_os = "macos", windows))]
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--browser-picker")) {
        tray::pick_browser();
        return;
    }
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--tray")) {
        #[cfg(any(target_os = "macos", windows))]
        std::process::exit(tray::main());
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            eprintln!("The Remuda tray is available on macOS and Windows");
            std::process::exit(2);
        }
    }
    if std::env::args().nth(1).as_deref() == Some("--internal-build-id") {
        use std::io::Write;
        if std::env::args().len() != 2 {
            std::process::exit(2);
        }
        let status = writeln!(
            std::io::stdout().lock(),
            "{}",
            remuda_native::dist::BUILD_VERSION
        );
        std::process::exit(i32::from(status.is_err()));
    }
    if std::env::args().nth(1).as_deref() == Some("--internal-input-worker") {
        std::process::exit(input_helper::worker(
            &std::env::args().skip(2).collect::<Vec<_>>(),
        ));
    }
    if std::env::args().nth(1).as_deref() == Some("--internal-preview-worker") {
        let mut args = std::env::args().skip(2).collect::<Vec<_>>();
        let tagged = args.len() == 5 && args.last().map(String::as_str) == Some(WORKER_ABI);
        if tagged {
            args.pop();
        }
        std::process::exit(preview::worker(&args, tagged));
    }
    if std::env::args().nth(1).as_deref() == Some("--internal-list-worker") {
        std::process::exit(worker(
            std::env::args().nth(2).as_deref() == Some(WORKER_ABI),
        ));
    }
    if std::env::args().nth(1).as_deref() == Some("--internal-nodes-worker") {
        std::process::exit(nodes_worker(
            std::env::args().nth(2).as_deref() == Some(WORKER_ABI),
        ));
    }
    if std::env::args().nth(1).as_deref() == Some("--internal-peer-worker") {
        let tagged = std::env::args().nth(3).as_deref() == Some(WORKER_ABI);
        std::process::exit(
            std::env::args()
                .nth(2)
                .map_or(1, |fp| peer_worker(&fp, tagged)),
        );
    }
    let destination = if std::env::args().nth(1).as_deref() == Some("--browser") {
        match std::env::args().nth(2) {
            Some(host) if remuda_native::net::browser::valid_ssh_destination(&host) => Some(host),
            _ => {
                eprintln!("Expected a Browser SSH hostname, IP address or config alias");
                std::process::exit(2);
            }
        }
    } else {
        None
    };
    let browser = match destination
        .as_ref()
        .map(|_| remuda_native::net::browser::Client::from_env())
        .transpose()
    {
        Ok(client) => client,
        Err(error) => {
            eprintln!("Browser credentials: {error}");
            std::process::exit(1);
        }
    };
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1000.), px(650.)), cx);
        if let Some(destination) = destination {
            browser::open(cx, bounds, destination, browser.flatten());
            return;
        }
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| {
                let focus = cx.focus_handle();
                focus.focus(window);
                cx.new(|cx| {
                    let mut gui = LocalGui::new();
                    gui.tree_focus = Some(focus);
                    gui.refresh();
                    gui.refresh_nodes();
                    cx.spawn(async move |weak, cx| loop {
                        gpui::Timer::after(Duration::from_millis(150)).await;
                        if weak
                            .update(cx, |this: &mut LocalGui, cx| {
                                this.drain();
                                cx.notify();
                            })
                            .is_err()
                        {
                            break;
                        }
                    })
                    .detach();
                    gui
                })
            },
        )
        .unwrap();
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worker_reply_rejects_legacy_and_wrong_abi() {
        assert_eq!(
            read_worker_reply::<Vec<String>>(br#"["old"]"#).unwrap_err(),
            STALE_HELPER
        );
        assert_eq!(
            read_worker_reply::<Vec<String>>(br#"{"abi":"wrong","payload":[]}"#).unwrap_err(),
            STALE_HELPER
        );
        let bytes = serde_json::to_vec(&WorkerReply {
            abi: WORKER_ABI.to_owned(),
            payload: vec!["ok"],
        })
        .unwrap();
        assert_eq!(
            read_worker_reply::<Vec<String>>(&bytes).unwrap(),
            vec!["ok"]
        );
    }
    #[gpui::test]
    fn helper_abi_error_shows_restart_notice(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| LocalGui::new());
        view.update(cx, |gui, cx| {
            gui.sender
                .send((gui.generation, Err(STALE_HELPER.into())))
                .unwrap();
            gui.drain();
            cx.notify();
        });
        assert!(cx.read(|app| view.read(app).helper_changed));
        assert!(cx.debug_bounds("helper-changed").is_some());
    }
    #[gpui::test]
    #[ignore = "run the isolated real-daemon acceptance supervisor"]
    fn real_local_daemon_reaches_session_row(cx: &mut gpui::TestAppContext) {
        assert!(std::env::var_os("REMUDA_GUI_ACCEPTANCE_WORKER").is_some());
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.refresh();
            gui
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let expect_stale = std::env::var_os("REMUDA_GUI_EXPECT_STALE").is_some();
        loop {
            view.update(cx, |gui, cx| {
                gui.drain();
                cx.notify();
            });
            cx.run_until_parked();
            let found = cx.read(|app| {
                let gui = view.read(app);
                if expect_stale {
                    gui.helper_changed
                } else {
                    gui.sessions
                        .iter()
                        .any(|row| row.name == "local-gui-proof" && row.alive)
                }
            });
            if found {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "real session did not reach the GUI model"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        if expect_stale {
            assert!(cx.debug_bounds("helper-changed").is_some());
            assert!(cx.read(|app| view.read(app).sessions.is_empty()));
            return;
        }
        assert!(cx.read(|app| view.read(app).expanded_peers.is_empty()));
        assert_eq!(
            cx.read(|app| view.read(app).sessions.len()),
            1,
            "fixture must have exactly one local session"
        );
        assert_eq!(
            cx.read(|app| view.read(app).sessions[0].name.clone()),
            "local-gui-proof"
        );
        assert!(
            cx.debug_bounds("session-0").is_some(),
            "exact local session row was not laid out"
        );
    }
    fn peer(fp: &str, name: &str) -> Peer {
        Peer {
            fingerprint: fp.into(),
            label: name.into(),
            admitted: true,
        }
    }
    #[test]
    fn node_snapshot_keeps_membership_states_and_local_only_is_empty() {
        use remuda_native::cluster::{ClusterSnapshot, NodeIdentity, Registry};
        assert!(peers_from_snapshot(None).is_empty());
        let entries = serde_json::json!([
            {"node_fp":"self","static_pubkey":"x","state":"admitted","version":1,"by":"self"},
            {"node_fp":"peer1","static_pubkey":"x","state":"admitted","version":1,"by":"self"},
            {"node_fp":"peer2","static_pubkey":"x","state":"revoked","version":1,"by":"self"}
        ]);
        let snapshot = ClusterSnapshot {
            identity: NodeIdentity {
                node_name: "local".into(),
                node_fp: "self".into(),
                static_pubkey: vec![],
            },
            registry: Registry {
                authorized_nodes: serde_json::from_value(entries).unwrap(),
            },
            aliases: [("peer1".into(), "alpha".into())].into_iter().collect(),
        };
        let peers = peers_from_snapshot(Some(snapshot));
        assert_eq!(peers[0], peer("peer1", "alpha"));
        assert_eq!(peers[1].fingerprint, "peer2");
        assert!(!peers[1].admitted);
    }
    #[test]
    fn peer_selection_requires_unique_current_full_fingerprint() {
        let mut gui = LocalGui::new();
        gui.remote_busy = true;
        gui.peers = vec![peer("full-a", "same"), peer("full-b", "same")];
        gui.set_expanded(Some("full-a".into()), true);
        assert!(gui.expanded_peers.contains("full-a"));
        gui.nodes_loading = true;
        gui.set_expanded(Some("full-b".into()), true);
        assert!(!gui.expanded_peers.contains("full-b"));
        gui.nodes_loading = false;
        gui.nodes_error = true;
        assert!(!gui.peer_current("full-a"));
        gui.nodes_error = false;
        gui.peers.push(peer("full-a", "duplicate"));
        assert!(!gui.peer_current("full-a"));
        assert!(gui.peer_current("full-b"));
        assert!(!gui.peer_current("same"));
        gui.peers.push(Peer {
            admitted: false,
            ..peer("full-b", "revoked duplicate")
        });
        assert!(!gui.peer_current("full-b"));
    }
    #[test]
    fn peers_load_independently_and_directory_changes_discard_late_replies() {
        let mut gui = LocalGui::new();
        gui.peers = vec![peer("a", "alpha"), peer("b", "beta")];
        gui.remote_busy = true;
        gui.set_expanded(Some("a".into()), true);
        gui.set_expanded(Some("b".into()), true);
        let (a, directory, a_fp) = gui.remote_pending.pop_front().unwrap();
        let (b, _, b_fp) = gui.remote_pending.pop_front().unwrap();
        gui.remote_sender
            .send((a, directory, a_fp, Ok(vec![row("session-a")])))
            .unwrap();
        gui.drain();
        assert_eq!(gui.remote_sessions["a"].rows[0].name, "session-a");
        assert!(gui.remote_sessions["b"].loading);

        gui.nodes_busy = true;
        gui.refresh_nodes();
        gui.remote_sender
            .send((b, directory, b_fp, Ok(vec![row("stale")])))
            .unwrap();
        gui.drain();
        assert!(gui.remote_sessions["b"].rows.is_empty());
        assert!(gui.remote_sessions["a"].error);
    }
    #[test]
    fn peer_failure_retains_stale_rows_and_exact_instance_identity() {
        let mut gui = LocalGui::new();
        gui.peers = vec![peer("a", "a")];
        gui.remote_busy = true;
        gui.set_expanded(Some("a".into()), true);
        let (generation, directory, fp) = gui.remote_pending.pop_front().unwrap();
        gui.remote_sender
            .send((generation, directory, fp, Ok(vec![row("proof")])))
            .unwrap();
        gui.drain();
        assert_eq!(
            gui.remote_sessions["a"].rows[0].instance_id.as_deref(),
            Some("instance")
        );
        gui.remote_busy = true;
        gui.refresh_peer("a".into());
        let (generation, directory, fp) = gui.remote_pending.pop_front().unwrap();
        gui.remote_sender
            .send((generation, directory, fp, Err("offline".into())))
            .unwrap();
        gui.drain();
        let peer = &gui.remote_sessions["a"];
        assert!(peer.error && !peer.loading);
        assert_eq!(peer.rows[0].name, "proof");
    }
    #[test]
    fn queued_peer_work_is_not_dispatched_after_revocation_or_duplicate() {
        for duplicate in [false, true] {
            let mut gui = LocalGui::new();
            gui.peers = vec![peer("a", "a")];
            gui.remote_busy = true;
            gui.set_expanded(Some("a".into()), true);
            if duplicate {
                gui.peers.push(peer("a", "duplicate"));
            } else {
                gui.peers[0].admitted = false;
            }
            gui.remote_sender
                .send((0, 0, "old".into(), Ok(vec![])))
                .unwrap();
            gui.drain();
            assert!(!gui.remote_busy && gui.remote_pending.is_empty());
            assert!(gui.remote_sessions["a"].rows.is_empty());
        }
    }
    #[gpui::test]
    fn peer_branches_fold_independently_and_preserve_the_preview(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.preview_busy = true;
            gui.remote_busy = true;
            gui.peers = vec![peer("a", "alpha"), peer("b", "beta")];
            gui.sessions = vec![row("local")];
            gui
        });
        for key in ["node-peer-0", "node-peer-1"] {
            let node = cx.debug_bounds(key).unwrap();
            cx.simulate_click(node.center(), gpui::Modifiers::none());
        }
        view.update(cx, |gui, cx| {
            for (id, row_name) in [("a", "remote-a"), ("b", "remote-b")] {
                let state = gui.remote_sessions.get_mut(id).unwrap();
                state.rows = vec![row(row_name)];
                state.loading = false;
            }
            cx.notify();
        });
        let alpha = cx.debug_bounds("node-peer-0").unwrap();
        let child = cx.debug_bounds("peer-session-0-0").unwrap();
        assert!(child.origin.x > alpha.origin.x && child.origin.y > alpha.origin.y);
        assert!(cx.debug_bounds("session-0").is_some());
        assert!(cx.debug_bounds("peer-session-1-0").is_some());
        cx.simulate_click(child.center(), gpui::Modifiers::none());
        view.update(cx, |gui, cx| {
            let (generation, target) = gui.preview_pending.take().unwrap();
            gui.preview_sender
                .send((generation, target, Ok("test snapshot".into())))
                .unwrap();
            gui.drain();
            cx.notify();
        });
        cx.simulate_keystrokes("left");
        let alpha_session = TreeRow::session(Some("a".into()), &row("remote-a"));
        let beta_session = TreeRow::session(Some("b".into()), &row("remote-b"));
        assert!(cx.read(|app| {
            let visible = view.read(app).tree_rows();
            !visible.contains(&alpha_session) && visible.contains(&beta_session)
        }));
        assert!(cx.read(|app| view.read(app).preview_received.is_some()));
        assert_eq!(
            cx.read(|app| view.read(app).preview.clone()).as_deref(),
            Some("test snapshot")
        );
        cx.simulate_keystrokes("right");
        assert!(cx.read(|app| view.read(app).tree_rows().contains(&alpha_session)));
        cx.simulate_keystrokes("down");
        assert!(cx.read(|app| view.read(app).focused_row == alpha_session));
        let local = cx.debug_bounds("node-local").unwrap();
        cx.simulate_click(local.center(), gpui::Modifiers::none());
        assert!(cx.read(|app| {
            let gui = view.read(app);
            !gui.local_expanded && gui.tree_rows().contains(&beta_session)
        }));
    }
    #[test]
    fn node_replies_reject_old_generations_and_mark_errors_stale() {
        let mut gui = LocalGui::new();
        gui.peers = vec![peer("full-a", "first")];
        gui.expanded_peers.insert("full-a".into());
        gui.nodes_busy = true;
        gui.refresh_nodes();
        let old = gui.nodes_generation;
        gui.refresh_nodes();
        let current = gui.nodes_generation;
        gui.nodes_pending = false;
        gui.nodes_sender
            .send((old, Ok(vec![peer("wrong", "stale")])))
            .unwrap();
        gui.drain();
        assert!(gui.nodes_loading);
        assert_eq!(gui.peers[0].fingerprint, "full-a");
        gui.nodes_busy = true;
        gui.nodes_sender
            .send((current, Err("unavailable".into())))
            .unwrap();
        gui.drain();
        assert!(gui.nodes_error);
        assert_eq!(gui.peers[0].fingerprint, "full-a");
        assert!(gui.expanded_peers.contains("full-a"));
        assert!(!gui.peer_current("full-a"));
    }
    fn row(name: &str) -> Session {
        Session {
            name: name.into(),
            instance_id: Some("instance".into()),
            alive: true,
            unavailable: false,
        }
    }
    #[test]
    fn stale_reply_is_ignored_and_failure_preserves_old_rows() {
        let mut gui = LocalGui::new();
        gui.sessions = vec![row("old")];
        gui.busy = true;
        gui.refresh();
        let old = gui.generation;
        gui.refresh();
        let latest = gui.generation;
        gui.pending = false;
        gui.sender.send((old, Ok(vec![row("wrong")]))).unwrap();
        gui.drain();
        assert_eq!(gui.sessions, vec![row("old")]);
        assert!(gui.loading);
        gui.busy = true;
        gui.sender.send((latest, Err("offline".into()))).unwrap();
        gui.drain();
        assert!(gui.error);
        assert!(!gui.loading);
        assert_eq!(gui.sessions, vec![row("old")]);
        gui.busy = true;
        gui.refresh();
        let current = gui.generation;
        gui.pending = false;
        gui.sender.send((current, Ok(vec![]))).unwrap();
        gui.drain();
        assert!(!gui.error);
        assert!(gui.sessions.is_empty());
    }
    #[test]
    fn preview_requires_current_unique_instance_and_fences_late_replies() {
        let mut gui = LocalGui::new();
        gui.preview_busy = true;
        gui.sessions = vec![row("one"), row("two")];
        gui.select_session(None, row("one"));
        let (old, selected) = gui.preview_pending.take().unwrap();
        gui.select_session(None, row("two"));
        gui.preview_pending = None;
        gui.preview_sender
            .send((old, selected, Ok("stale".into())))
            .unwrap();
        gui.drain();
        assert!(gui.preview.is_none());
        gui.preview_busy = true;
        gui.select_session(None, row("one"));
        let (generation, selection) = gui.preview_pending.take().unwrap();
        gui.sessions[0].instance_id = Some("replacement".into());
        gui.preview_sender
            .send((generation, selection, Ok("stale".into())))
            .unwrap();
        gui.drain();
        assert!(gui.preview.is_none());
        gui.sessions = vec![row("one"), row("one")];
        gui.select_session(None, row("one"));
        assert!(gui.selection.is_none());
        gui.sessions = vec![row("one")];
        gui.error = true;
        gui.select_session(None, row("one"));
        assert!(gui.selection.is_none());
    }
    #[gpui::test]
    fn clicking_session_displays_current_snapshot(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.preview_busy = true;
            gui.sessions = vec![row("local")];
            gui
        });
        let session = cx.debug_bounds("session-0").unwrap();
        cx.simulate_click(session.center(), gpui::Modifiers::none());
        view.update(cx, |gui, cx| {
            let (generation, selection) = gui.preview_pending.take().unwrap();
            gui.preview_sender
                .send((generation, selection, Ok("screen text".into())))
                .unwrap();
            gui.drain();
            cx.notify();
        });
        assert!(cx.debug_bounds("preview").is_some());
        assert_eq!(
            cx.read(|app| view.read(app).preview.clone()).as_deref(),
            Some("screen text")
        );
    }
    #[test]
    fn preview_peer_revocation_and_refresh_discard_queued_work() {
        let mut gui = LocalGui::new();
        gui.preview_busy = true;
        gui.peers = vec![peer("full", "peer")];
        gui.remote_sessions.insert(
            "full".into(),
            PeerSessions {
                rows: vec![row("one")],
                ..Default::default()
            },
        );
        gui.select_session(Some("full".into()), row("one"));
        gui.peers[0].admitted = false;
        gui.preview_sender
            .send((
                0,
                Selection {
                    peer: None,
                    name: "old".into(),
                    instance: "old".into(),
                },
                Ok("old".into()),
            ))
            .unwrap();
        gui.drain();
        assert!(!gui.preview_busy && gui.preview_pending.is_none());
        assert!(gui.preview.is_none());
        gui.nodes_busy = true;
        gui.refresh_nodes();
        assert!(gui.selection.is_none());
    }
    #[test]
    fn labels_escape_controls_and_literal_escapes() {
        assert_ne!(label("\u{1b}"), label("\\u{1b}"));
        assert!(label("a\u{202e}b").contains("\\u{202e}"));
        assert_eq!(label("한글"), "\"한글\"");
    }
    #[gpui::test]
    fn stale_node_cannot_reopen_after_it_is_collapsed(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.remote_busy = true;
            gui.peers = vec![peer("full-a", "peer")];
            gui
        });
        let button = cx.debug_bounds("node-peer-0").unwrap();
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        assert!(cx.read(|app| view.read(app).expanded_peers.contains("full-a")));
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        view.update(cx, |gui, cx| {
            gui.nodes_loading = true;
            cx.notify();
        });
        let button = cx.debug_bounds("node-peer-0").unwrap();
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        assert!(cx.read(|app| view.read(app).expanded_peers.is_empty()));
    }
    #[gpui::test]
    fn refresh_click_starts_new_generation(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.busy = true;
            gui.sessions = vec![row("local")];
            gui
        });
        let button = cx.debug_bounds("refresh").expect("refresh rendered");
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        assert!(cx.read(|app| view.read(app).loading));
        assert_eq!(cx.read(|app| view.read(app).generation), 1);
        assert_eq!(
            cx.read(|app| view.read(app).sessions[0].name.clone()),
            "local"
        );
    }
}
