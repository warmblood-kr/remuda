use gpui::{
    div, prelude::*, px, rgb, size, App, AppContext, Application, Bounds, Context, Div, FocusHandle,
    SharedString, Stateful, Window, WindowBounds, WindowOptions,
};
use serde::{Deserialize, Serialize};
use std::{sync::mpsc, time::Duration};
use gpui_component::input::{Input, InputState};

mod document_workspace_view;
#[allow(dead_code)]
mod rich_selection;
#[allow(dead_code)]
mod input_helper;
use remuda_gui::documents;
mod preview;
mod provider;
mod terminal;

const WORKER_ABI: &str = "--remuda-gui-worker-abi-1";
const WORKER_ABI_2: &str = "--remuda-gui-worker-abi-2";
const MIN_DOCUMENT_FRACTION: f32 = 0.18;
const MAX_DOCUMENT_FRACTION: f32 = 0.62;
const DOCUMENT_KEY_STEP_PX: f32 = 16.0;
const TERMINAL_LINE_HEIGHT_PX: f32 = 16.0;
/// One hierarchy level, in logical pixels. The tree uses `text_sm`; a smaller
/// step collapses into the row text, and this stays visible at a 420 px window.
const TREE_INDENT_PX: f32 = 12.0;
/// Detail lines sit under the identity glyph, one step past the name.
const TREE_DETAIL_OFFSET_PX: f32 = 3.0;
/// Sidebar children before the local node: title and section label. The local
/// node is the first tree entry, so a reveal index is `2 + helper notice`.
const TREE_FIXED_HEADER_CHILDREN: usize = 2;
/// Pointer travel that turns a preview click into a drag selection.
const DRAG_THRESHOLD_PX: f32 = 3.0;
const WORKER_REPLY_HEADROOM_BYTES: usize = 16 * 1024;
const DEFAULT_DOCUMENT_PANEL_FRACTION: f32 = 0.32;
/// Structured preview is allowed to be smaller than the provider cap, leaving
/// room for the reply envelope; larger grids fall back to plain text.
const WIRE_BUDGET: usize = provider::STDOUT_LIMIT - WORKER_REPLY_HEADROOM_BYTES;
const STALE_HELPER: &str = "GUI helper version changed; restart the GUI";

#[derive(Serialize, Deserialize)]
struct WorkerReply<T> {
    abi: String,
    payload: T,
}

fn write_worker_reply_abi<T: Serialize>(payload: &T, abi: Option<&str>) -> i32 {
    let result = match abi {
        Some(abi) => serde_json::to_writer(
            std::io::stdout(),
            &WorkerReply {
                abi: abi.to_owned(),
                payload,
            },
        ),
        None => serde_json::to_writer(std::io::stdout(), payload),
    };
    i32::from(result.is_err())
}

fn write_worker_reply<T: Serialize>(payload: &T, tagged: bool) -> i32 {
    write_worker_reply_abi(payload, tagged.then_some(WORKER_ABI))
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
    #[serde(default)]
    attached: bool,
    /// Validated hierarchy depth; 0 when the daemon or peer has no sidecar.
    #[serde(default)]
    depth: u8,
    /// Advisory activity for display; `alive` remains the liveness authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state: Option<remuda_core::session_view::ViewState>,
    /// Validated detail line from the shared semantic row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    /// Whether the shared instance-bound sidecar was present for this row.
    #[serde(default)]
    structured: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Peer {
    fingerprint: String,
    label: String,
    admitted: bool,
}

fn session_marker(session: &Session) -> char {
    if session.unavailable {
        return '?';
    }
    if !session.alive {
        return remuda_native::cluster_tui::session_glyph(false);
    }
    match session.state {
        Some(remuda_core::session_view::ViewState::Working) => '🏇',
        Some(remuda_core::session_view::ViewState::Idle) => '○',
        Some(remuda_core::session_view::ViewState::Blocked) => '!',
        _ => remuda_native::cluster_tui::session_glyph(true),
    }
}

/// Peer text escaped by `label` without the surrounding quotes.
fn plain_label(value: &str) -> String {
    let escaped = label(value);
    escaped[1..escaped.len() - 1].to_owned()
}

fn worker_deadline() {
    std::thread::spawn(|| {
        std::thread::sleep(provider::TIMEOUT);
        std::process::exit(124);
    });
}

fn worker_executable() -> std::io::Result<std::path::PathBuf> {
    #[cfg(test)]
    if let Some(path) = std::env::var_os("REMUDA_GUI_ACCEPTANCE_WORKER") {
        return Ok(path.into());
    }
    std::env::current_exe()
}

// The view request falls back to a List request on an older daemon. Each
// request is bounded, and the worker's own deadline survives GUI exit.
const SESSION_VIEW_REQUEST_COLUMNS: u16 = 512;
/// Whole connect/send/read budget for one local request. Two of these stay
/// under the 8 s worker watchdog, so a silent older daemon still reaches the
/// flat `List` fallback instead of killing the worker first.
const VIEW_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
/// Whole peer fetch including the view and its flat fallback. Kept below the
/// 8 s worker watchdog so a slow old peer keeps the previous rows instead of
/// being killed mid-fallback.
const PEER_REQUEST_BUDGET: Duration = Duration::from_secs(6);

/// Run one blocking call under a whole-call budget. The worker is a
/// short-lived process, so a timed-out call cannot outlive its reply.
fn bounded_call<T: Send + 'static>(
    budget: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    rx.recv_timeout(budget).ok()
}

/// Bound the whole local request; a stuck connect must not outlive the worker.
fn bounded_request(
    path: &std::path::Path,
    request: &remuda_core::protocol::Request,
    timeout: Duration,
) -> Option<remuda_core::protocol::Response> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let path = path.to_path_buf();
    let request = request.clone();
    std::thread::spawn(move || {
        let _ = tx.send(remuda_native::client::request(&path, &request));
    });
    rx.recv_timeout(timeout).ok().and_then(Result::ok)
}

fn sessions_from_view(view: remuda_core::session_view::SessionView) -> Option<Vec<Session>> {
    let checked = view
        .validate_for_display(None, SESSION_VIEW_REQUEST_COLUMNS)
        .ok()?;
    let semantics = checked.semantics();
    checked
        .sessions()
        .iter()
        .enumerate()
        .map(|(index, session)| {
            // Binding and order were validated; an older peer has no rows.
            let semantic = semantics.get(index);
            Some(Session {
                name: session.name.clone(),
                instance_id: session.instance_id.clone(),
                alive: session.alive,
                unavailable: session.unavailable,
                attached: session.attached,
                depth: semantic.map_or(0, |row| row.depth),
                state: semantic.and_then(|row| row.state),
                detail: semantic.and_then(|row| row.detail.clone()),
                structured: semantic.is_some(),
            })
        })
        .collect()
}

fn plain_sessions(rows: Vec<remuda_core::SessionSummary>) -> Vec<Session> {
    rows.into_iter()
        .map(|s| Session {
            name: s.name,
            instance_id: s.instance_id,
            alive: s.alive,
            unavailable: s.unavailable,
            attached: s.attached,
            depth: 0,
            state: None,
            detail: None,
            structured: false,
        })
        .collect()
}

fn worker(tagged: bool) -> i32 {
    use remuda_core::protocol::{Request, Response};
    worker_deadline();
    let path = remuda_native::daemon::socket_path("default");
    let sessions = view_request(&path).or_else(|| {
        match bounded_request(&path, &Request::List, VIEW_REQUEST_TIMEOUT) {
            Some(Response::Sessions(rows)) => Some(plain_sessions(rows)),
            _ => None,
        }
    });
    match sessions {
        Some(rows) => write_worker_reply(&rows, tagged),
        None => 1,
    }
}

/// A daemon that predates the sidecar answers with an error; callers fall back
/// to the flat session list so the GUI stays usable.
fn view_request(path: &std::path::Path) -> Option<Vec<Session>> {
    use remuda_core::protocol::{Request, Response};
    match bounded_request(
        path,
        &Request::RenderedSessionList {
            width: SESSION_VIEW_REQUEST_COLUMNS,
        },
        VIEW_REQUEST_TIMEOUT,
    ) {
        Some(Response::RenderedSessionList(view)) => sessions_from_view(view),
        _ => None,
    }
}

fn terminal_worker(name: &str, instance: &str) -> i32 {
    use remuda_core::protocol::{Request, Response};
    let path = remuda_native::daemon::socket_path("default");
    let current = match remuda_native::client::request_with_timeout(
        &path,
        &Request::List,
        Duration::from_secs(3),
    ) {
        Ok(Response::Sessions(rows)) => {
            let matching: Vec<_> = rows.iter().filter(|row| row.name == name).collect();
            matching.len() == 1
                && matching[0].alive
                && !matching[0].unavailable
                && matching[0].instance_id.as_deref() == Some(instance)
        }
        _ => false,
    };
    if !current {
        eprintln!("Session changed or is unavailable; attach refused.");
        return 1;
    }
    match remuda_native::client::attach_with_mouse(&path, name, false) {
        Ok(_) => {
            eprintln!("\r\nTerminal detached.");
            0
        }
        Err(error) => {
            eprintln!("\r\nAttach failed: {error}");
            1
        }
    }
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
    let fingerprint = fingerprint.to_owned();
    let list = move || -> Result<Vec<Session>, ()> {
        let fingerprint = fingerprint.as_str();
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
        let rendered = match ClusterClient::system().request(
            address,
            &pinned_static_key,
            &private,
            &Request::RenderedSessionList {
                width: SESSION_VIEW_REQUEST_COLUMNS,
            },
        ) {
            Ok(Response::RenderedSessionList(view)) => sessions_from_view(view),
            _ => None,
        };
        let rows = match rendered {
            Some(rows) => rows,
            None => {
                let Response::Sessions(rows) = ClusterClient::system()
                    .request(address, &pinned_static_key, &private, &Request::List)
                    .map_err(|_| ())?
                else {
                    return Err(());
                };
                plain_sessions(rows)
            }
        };
        if !peer_is_unique(&discover_peers().map_err(|_| ())?, fingerprint) {
            return Err(());
        }
        Ok(rows)
    };
    match bounded_call(PEER_REQUEST_BUDGET, list) {
        Some(Ok(rows)) => write_worker_reply(&rows, tagged),
        _ => 1,
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
#[derive(Clone, Copy, PartialEq, Eq)]
enum NarrowView { Nodes, Session, Documents }
fn selection_label(selection: &Selection) -> String {
    let node = selection.peer.as_ref().map_or_else(
        || "This device".to_string(),
        |peer| format!("Peer {}", label(peer)),
    );
    format!(
        "{} / {} · instance {}",
        node,
        label(&selection.name),
        label(&selection.instance)
    )
}
fn compact_selection_label(selection: &Selection) -> String {
    let node = selection.peer.as_ref().map_or_else(|| "This device".to_string(), |peer| format!("Peer {}…", label(&peer.chars().take(8).collect::<String>())));
    let name = if selection.name.chars().count() > 20 { format!("{}…", selection.name.chars().take(20).collect::<String>()) } else { selection.name.clone() };
    format!("{node} / {} · {}…", label(&name), label(&selection.instance.chars().take(8).collect::<String>()))
}
type PreviewResult = (u64, Selection, Result<preview::PreviewPayload, String>);
type SyncResult = (u64, Selection, Result<preview::SyncFrame, String>);

type RemoteResult = (u64, u64, String, Result<Vec<Session>, String>);

enum TreeEntry {
    Node(Option<String>),
    Session(Option<String>, usize),
}

#[derive(Clone)]
struct SelectionSnapshot {
    input: gpui::Entity<InputState>,
    from_document: bool,
    label: String,
    identity: Option<String>,
    show_identity: bool,
}

struct PreviewSelection {
    grid: terminal::Selection,
    identity: Selection,
    version: u64,
    revision: u64,
}

const MAX_SELECTION_SNAPSHOT: usize = 1024 * 1024;
const MAX_PREVIEW_DRAG_RUNS: usize = 256;

fn is_terminal_snapshot_shortcut(key: &gpui::Keystroke) -> bool {
    let m = &key.modifiers;
    key.key == "c" && m.shift && !m.alt && !m.function &&
        if cfg!(target_os = "macos") { m.platform && !m.control } else { m.control && !m.platform }
}

fn terminal_snapshot_text(frame: terminal::Frame) -> Result<String, &'static str> {
    let mut result = String::new();
    for row in frame.cells {
        for cell in row {
            if cell.continuation { continue; }
            let value = if cell.text.is_empty() { " " } else { &cell.text };
            if result.len().saturating_add(value.len()) > MAX_SELECTION_SNAPSHOT { return Err("Selection snapshot exceeds 1 MiB"); }
            result.push_str(value);
        }
        if result.len() == MAX_SELECTION_SNAPSHOT { return Err("Selection snapshot exceeds 1 MiB"); }
        result.push('\n');
    }
    Ok(result)
}

struct LocalGui {
    rich_selections: document_workspace_view::SelectionMap,
    preview_selection: Option<PreviewSelection>,
    preview_revision: u64,
    preview_drag_start: Option<gpui::Point<gpui::Pixels>>,
    preview_dragged: bool,
    /// Identity at mouse-down; release must match it before focusing input.
    preview_press: Option<Selection>,
    preview_bounds: std::rc::Rc<std::cell::Cell<Option<gpui::Bounds<gpui::Pixels>>>>,
    preview_focus: Option<FocusHandle>,
    show_session_identity: bool,
    terminal_selection: Option<terminal::Selection>,
    terminal_drag_start: Option<gpui::Point<gpui::Pixels>>,
    terminal_bounds: std::rc::Rc<std::cell::Cell<Option<gpui::Bounds<gpui::Pixels>>>>,
    selection_snapshot: Option<SelectionSnapshot>,
    selection_error: Option<&'static str>,
    documents: Option<documents::Workspace>,
    documents_focus: Option<FocusHandle>,
    document_panel_fraction: f32,
    document_collapsed: bool,
    document_dragging: bool,
    narrow_view: NarrowView,
    was_narrow: bool,
    divider_focus: Option<FocusHandle>,
    collapse_focus: Option<FocusHandle>,
    toggle_focus: Option<FocusHandle>,
    initial_focus_set: bool,
    attach_confirm: Option<Selection>,
    #[cfg(test)]
    documents_socket: Option<std::path::PathBuf>,
    terminal: Option<std::rc::Rc<terminal::Terminal>>,
    terminal_status: String,
    input_focus: Option<FocusHandle>,
    input_text: String,
    input_status: String,
    input_warnings: Vec<Selection>,
    input_helper: Option<input_helper::InputHelper>,
    input_target: Option<Selection>,
    input_stopped: bool,
    sync_tick: u8,
    sync_busy: bool,
    sync_version: u64,
    sync_error: bool,
    sync_sender: mpsc::Sender<SyncResult>,
    sync_results: mpsc::Receiver<SyncResult>,
    helper_changed: bool,
    selection: Option<Selection>,
    preview: Option<String>,
    preview_cells: Option<Vec<Vec<remuda_core::agent::StyledCell>>>,
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
    selected_peer: Option<String>,
    nodes_loading: bool,
    nodes_error: bool,
    nodes_generation: u64,
    nodes_busy: bool,
    nodes_pending: bool,
    nodes_sender: mpsc::Sender<(u64, Result<Vec<Peer>, String>)>,
    nodes_results: mpsc::Receiver<(u64, Result<Vec<Peer>, String>)>,
    remote_sessions: Vec<Session>,
    /// Sessions fetched per peer, so expanding one node does not hide another.
    peer_sessions: std::collections::HashMap<String, Vec<Session>>,
    /// Node rows that are open. The empty key is this device.
    expanded_nodes: std::collections::HashSet<String>,
    tree_focus: Option<FocusHandle>,
    tree_cursor: usize,
    sidebar_scroll: gpui::ScrollHandle,
    remote_error: bool,
    remote_loading: bool,
    remote_generation: u64,
    remote_busy: bool,
    remote_pending: Option<(u64, u64, String)>,
    remote_sender: mpsc::Sender<RemoteResult>,
    remote_results: mpsc::Receiver<RemoteResult>,
}
impl LocalGui {
    fn new() -> Self {
        let (preview_sender, preview_results) = mpsc::channel();
        let (sync_sender, sync_results) = mpsc::channel();
        let (sender, results) = mpsc::channel();
        let (nodes_sender, nodes_results) = mpsc::channel();
        let (remote_sender, remote_results) = mpsc::channel();
        Self {
            rich_selections: Default::default(),
            selection_snapshot: None,
            terminal_selection: None,
            preview_selection: None,
            preview_revision: 0,
            preview_drag_start: None,
            preview_dragged: false,
            preview_press: None,
            preview_bounds: std::rc::Rc::new(std::cell::Cell::new(None)),
            preview_focus: None,
            show_session_identity: false,
            terminal_drag_start: None,
            terminal_bounds: std::rc::Rc::new(std::cell::Cell::new(None)),
            selection_error: None,
            documents: None,
            documents_focus: None,
            document_panel_fraction: DEFAULT_DOCUMENT_PANEL_FRACTION,
            document_collapsed: false,
            document_dragging: false,
            narrow_view: NarrowView::Nodes,
            was_narrow: false,
            divider_focus: None,
            collapse_focus: None,
            toggle_focus: None,
            initial_focus_set: false,
            attach_confirm: None,
            #[cfg(test)]
            documents_socket: None,
            terminal: None,
            terminal_status: String::new(),
            input_focus: None,
            input_text: String::new(),
            input_status: String::new(),
            input_warnings: Vec::new(),
            input_helper: None,
            input_target: None,
            input_stopped: false,
            sync_tick: 0,
            sync_busy: false,
            sync_version: 0,
            sync_error: false,
            sync_sender,
            sync_results,
            helper_changed: false,
            selection: None,
            preview: None,
            preview_cells: None,
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
            selected_peer: None,
            nodes_loading: false,
            nodes_error: false,
            nodes_generation: 0,
            nodes_busy: false,
            nodes_pending: false,
            nodes_sender,
            nodes_results,
            remote_sessions: vec![],
            peer_sessions: std::collections::HashMap::new(),
            expanded_nodes: ["".to_string()].into_iter().collect(),
            tree_focus: None,
            tree_cursor: 0,
            sidebar_scroll: gpui::ScrollHandle::new(),
            remote_error: false,
            remote_loading: false,
            remote_generation: 0,
            remote_busy: false,
            remote_pending: None,
            remote_sender,
            remote_results,
        }
    }
    fn open_selection_snapshot(&mut self, source: &str, from_document: bool, label: String, identity: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if source.len() > MAX_SELECTION_SNAPSHOT {
            self.selection_error = Some("Selection snapshot exceeds 1 MiB");
            return;
        }
        self.selection_error = None;
        let value = source.to_owned();
        let input = cx.new(|cx| InputState::new(window, cx).multi_line(true).default_value(value));
        self.selection_snapshot = Some(SelectionSnapshot { input: input.clone(), from_document, label, identity, show_identity: false });
        input.update(cx, |state, cx| state.focus(window, cx));
    }

    fn close_selection_snapshot(&mut self, window: &mut Window) {
        let was_document = self.selection_snapshot.take().is_some_and(|s| s.from_document);
        let focus = if was_document && self.documents.is_some() { &self.documents_focus } else if self.session_input_visible() { &self.input_focus } else { &self.tree_focus };
        if let Some(focus) = focus { focus.focus(window); }
    }

    fn open_terminal_snapshot(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = if let Some(terminal) = &self.terminal {
            match terminal_snapshot_text(terminal.frame()) {
                Ok(text) => Some(text),
                Err(error) => { self.selection_error = Some(error); None },
            }
        } else if self.selection.as_ref().is_some_and(|s| self.selection_current(s)) {
            if let Some(frame) = self.preview_full_frame() {
                match terminal_snapshot_text(frame) {
                    Ok(text) => Some(text),
                    Err(error) => { self.selection_error = Some(error); None },
                }
            } else { self.preview.clone() }
        } else {
            self.selection_error = Some("Preview session is no longer current; select it again.");
            None
        };
        let target = self.selection.as_ref().map(selection_label).unwrap_or_else(|| "Terminal".into());
        let label = if self.terminal.is_some() { format!("Terminal grid: {target} (row padding and visual wraps; not raw output)") } else { format!("Terminal preview: {target} · v{} (bounded screen rows/spaces; not raw output)", self.sync_version) };
        if let Some(text) = text { self.open_selection_snapshot(&text, false, label, None, window, cx); }
    }
    fn clear_preview(&mut self) {
        self.show_session_identity = false;
        self.invalidate_preview_selection();
        self.preview_drag_start = None;
        self.preview_dragged = false;
        self.preview_press = None;
        self.terminal_selection = None;
        self.attach_confirm = None;
        self.terminal = None;
        self.terminal_status.clear();
        self.input_text.clear();
        self.sync_tick = 0;
        self.sync_version = 0;
        self.sync_error = false;
        self.preview_generation = self
            .preview_generation
            .checked_add(1)
            .expect("generation exhausted");
        self.selection = None;
        self.preview = None;
        self.preview_cells = None;
        self.preview_pending = None;
    }
    fn invalidate_preview_selection(&mut self) {
        self.preview_revision = self.preview_revision.wrapping_add(1);
        self.preview_selection = None;
    }
    fn warn_input_target(&mut self) {
        if let Some(selection) = self.input_target.clone() {
            if !self.input_warnings.contains(&selection) {
                self.input_warnings.push(selection);
            }
        }
    }
    fn selection_current(&self, s: &Selection) -> bool {
        if s.peer != self.selected_peer {
            return false;
        }
        let rows = if let Some(fp) = &s.peer {
            if !self.peer_current(fp) {
                return false;
            }
            if self.selected_peer.as_ref() == Some(fp)
                && (self.remote_loading || self.remote_error)
            {
                return false;
            }
            let Some(rows) = self.peer_sessions.get(fp) else {
                return false;
            };
            rows
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
    fn select_session(&mut self, row: Session) {
        self.clear_preview();
        let Some(instance) = row.instance_id else {
            return;
        };
        let s = Selection {
            peer: self.selected_peer.clone(),
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
    fn submit_local_line(&mut self) {
        self.submit_local_line_with(|selection| {
            worker_executable().and_then(|path| {
                let kind = if selection.peer.is_some() {
                    "peer"
                } else {
                    "local"
                };
                let peer = selection.peer.as_deref().unwrap_or("");
                input_helper::InputHelper::start(
                    &path,
                    &[
                        "--internal-input-worker",
                        kind,
                        peer,
                        &selection.name,
                        &selection.instance,
                    ],
                )
            })
        });
    }
    fn session_input_visible(&self) -> bool {
        self.terminal.is_some() || self.selection.as_ref().is_some_and(|s| self.selection_current(s))
    }

    fn toggle_documents(&mut self, window: &mut Window) {
        if self.documents.is_some() {
            self.documents = None;
            self.document_collapsed = false;
            self.document_dragging = false;
            let focus = if self.session_input_visible() {
                &self.input_focus
            } else {
                &self.tree_focus
            };
            if let Some(focus) = focus {
                focus.focus(window);
            }
        } else {
            #[cfg(test)]
            let socket = self
                .documents_socket
                .clone()
                .unwrap_or_else(|| remuda_native::daemon::socket_path("default"));
            #[cfg(not(test))]
            let socket = remuda_native::daemon::socket_path("default");
            self.documents = Some(documents::Workspace::new(socket));
            self.document_collapsed = false;
            if let Some(focus) = &self.documents_focus {
                focus.focus(window);
            }
        }
    }

    fn show_narrow_view(&mut self, view: NarrowView, window: &mut Window) {
        let view = if view == NarrowView::Session && !self.session_input_visible() { NarrowView::Nodes } else { view };
        if view == NarrowView::Documents && self.documents.is_none() { self.toggle_documents(window); }
        if view == NarrowView::Documents { self.document_collapsed = false; }
        self.narrow_view = view;
        let focus = match view {
            NarrowView::Nodes => &self.tree_focus,
            NarrowView::Session if self.session_input_visible() => &self.input_focus,
            NarrowView::Documents => &self.documents_focus,
            _ => &self.tree_focus,
        };
        if let Some(focus) = focus { focus.focus(window); }
    }

    fn request_attach(&mut self) {
        let Some(selection) = self.selection.clone() else {
            return;
        };
        if selection.peer.is_some()
            || !self.selection_current(&selection)
            || self.input_helper.is_some()
            || self.input_stopped
        {
            self.terminal_status =
                "Select a current local session and wait for pending input.".into();
            return;
        }
        self.terminal_status = format!(
            "Confirm attach for {}; this takes over any current terminal.",
            selection_label(&selection)
        );
        self.attach_confirm = Some(selection);
    }
    fn cancel_attach(&mut self) {
        self.attach_confirm = None;
        self.terminal_status = "Attach cancelled.".into();
    }
    fn attach_selected(&mut self) {
        let Some(selection) = self.selection.clone() else {
            return;
        };
        if self.attach_confirm.as_ref() != Some(&selection) {
            self.terminal_status = "Confirm attach for the selected session first.".into();
            return;
        }
        self.attach_confirm = None;
        if selection.peer.is_some()
            || !self.selection_current(&selection)
            || self.input_helper.is_some()
            || self.input_stopped
        {
            self.terminal_status =
                "Select a current local session and wait for pending input.".into();
            return;
        }
        self.terminal = None;
        match worker_executable()
            .map_err(|e| e.to_string())
            .and_then(|path| {
                terminal::Terminal::start(&path, &selection.name, &selection.instance, WORKER_ABI)
            }) {
            Ok(terminal) => {
                self.terminal = Some(std::rc::Rc::new(terminal));
                self.terminal_status =
                    "Opening terminal. Click it to type; Ctrl-\\ detaches.".into();
            }
            Err(error) => self.terminal_status = format!("Could not open terminal: {error}"),
        }
    }
    fn submit_local_line_with(
        &mut self,
        start: impl FnOnce(&Selection) -> std::io::Result<input_helper::InputHelper>,
    ) {
        let Some(selection) = self.selection.clone() else {
            return;
        };
        if !self.selection_current(&selection) || self.input_stopped {
            return;
        }
        if self.input_helper.is_some() {
            self.input_status = "Input pending; wait for the result".into();
            return;
        }
        if self.input_text.is_empty() {
            return;
        }
        let mut bytes = self.input_text.as_bytes().to_vec();
        bytes.push(b'\r');
        self.input_target = Some(selection.clone());
        match start(&selection) {
            Ok(helper) => match helper.submit(&bytes) {
                input_helper::Submit::Queued => {
                    self.input_text.clear();
                    self.input_status = "Sending…".into();
                    self.input_helper = Some(helper);
                }
                _ => self.input_status = "Input could not be queued".into(),
            },
            Err(_) => self.input_status = "Input helper unavailable".into(),
        }
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
            let result = worker_executable()
                .map_err(|e| e.to_string())
                .and_then(|path| preview::request_preview(&path, &selection))
                .and_then(|payload| {
                    if payload.text.len() <= 65_536 {
                        Ok(payload)
                    } else {
                        Err("too large".into())
                    }
                });
            let _ = sender.send((generation, selection, result));
        });
    }
    fn launch_sync(&mut self, selection: Selection) {
        if self.sync_busy
            || self.preview_busy
            || self.preview_pending.is_some()
            || self.selection.as_ref() != Some(&selection)
            || !self.selection_current(&selection)
        {
            return;
        }
        self.sync_busy = true;
        let generation = self.preview_generation;
        let since = self.sync_version;
        let sender = self.sync_sender.clone();
        std::thread::spawn(move || {
            let result = worker_executable()
                .map_err(|e| e.to_string())
                .and_then(|path| preview::request_sync(&path, &selection, since));
            let _ = sender.send((generation, selection, result));
        });
    }
    fn refresh_nodes(&mut self) {
        self.clear_preview();
        self.invalidate_remote();
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
            let result = worker_executable()
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
    fn select_peer(&mut self, fingerprint: String) {
        if self.peer_current(&fingerprint) {
            self.expanded_nodes.insert(fingerprint.clone());
            if self.selected_peer.as_ref() != Some(&fingerprint) {
                self.remote_sessions = self
                    .peer_sessions
                    .get(&fingerprint)
                    .cloned()
                    .unwrap_or_default();
            }
            self.selected_peer = Some(fingerprint);
            self.refresh_peer();
        }
    }

    /// Select a node and make sure its children are open. Clicking the node
    /// label never hides already-visible sessions.
    fn select_node(&mut self, key: String) {
        self.expanded_nodes.insert(key.clone());
        if key.is_empty() {
            if self.selected_peer.take().is_some() {
                self.clear_preview();
                self.remote_generation = self
                    .remote_generation
                    .checked_add(1)
                    .expect("generation exhausted");
                self.remote_pending = None;
                self.remote_loading = false;
                self.remote_error = false;
                self.remote_sessions.clear();
            }
        } else {
            self.select_peer(key);
        }
    }

    /// Collapse or reopen a node from its chevron. Loaded peer sessions stay
    /// cached so reopening does not blank another node's children.
    fn toggle_node(&mut self, key: String) {
        if self.expanded_nodes.remove(&key) {
            return;
        }
        self.expanded_nodes.insert(key.clone());
        if !key.is_empty() {
            // Opening a fresh peer must start its own fetch, not wait for a
            // second click on the node label.
            self.select_peer(key);
        }
    }

    /// Select a session under any visible node, not only the last selected one.
    fn activate_session(&mut self, peer: Option<String>, row: Session) {
        if peer != self.selected_peer {
            self.selected_peer = peer.clone();
            self.remote_generation = self
                .remote_generation
                .checked_add(1)
                .expect("generation exhausted");
            self.remote_pending = None;
            self.remote_loading = false;
            self.remote_error = false;
            self.remote_sessions = peer
                .as_ref()
                .and_then(|fp| self.peer_sessions.get(fp).cloned())
                .unwrap_or_default();
        }
        self.select_session(row);
    }

    /// The same bounded grid that is displayed, including its original cell widths.
    fn preview_full_frame(&self) -> Option<terminal::Frame> {
        let cells = self.preview_cells.as_ref().filter(|cells| !cells.is_empty())?;
        let cols = cells.iter().map(Vec::len).max().unwrap_or(0);
        if cols == 0 { return None; }
        let (_, rows) = terminal::clamp_grid(cols.min(usize::from(u16::MAX)) as u16, cells.len().min(usize::from(u16::MAX)) as u16);
        Some(terminal::frame_from_styled(cells[..usize::from(rows).min(cells.len())].to_vec()))
    }
    fn preview_visible_rows(&self, total: usize) -> usize {
        let visible = self.preview_bounds.get().map(|bounds| (f32::from(bounds.size.height) / TERMINAL_LINE_HEIGHT_PX).ceil().max(1.) as usize).unwrap_or(40);
        total.min(visible)
    }
    fn preview_frame(&self) -> Option<terminal::Frame> {
        let cells = self.preview_cells.as_ref().filter(|cells| !cells.is_empty())?;
        let cols = cells.iter().map(Vec::len).max().unwrap_or(0);
        if cols == 0 { return None; }
        let (_, rows) = terminal::clamp_grid(cols.min(usize::from(u16::MAX)) as u16, cells.len().min(usize::from(u16::MAX)) as u16);
        let rows = self.preview_visible_rows(usize::from(rows).min(cells.len()));
        Some(terminal::frame_from_styled(cells[..rows].to_vec()))
    }

    fn preview_selection_current(&self) -> bool {
        self.preview_selection.as_ref().is_some_and(|s| {
            self.terminal.is_none() && self.selection.as_ref() == Some(&s.identity)
                && self.selection_current(&s.identity) && self.sync_version == s.version
                && self.preview_revision == s.revision
                && self.preview_cells.as_ref().is_some_and(|cells| {
                    let max_rows = (terminal::MAX_RENDER_CELLS / usize::from(s.grid.frame.cols).max(1)).max(1);
                    self.preview_visible_rows(cells.len().min(max_rows)) == usize::from(s.grid.frame.rows)
                })
        })
    }

    /// Read-only captured screen. Mouse selection never sends terminal input.
    fn preview_panel(&self, window: &Window, cx: &mut Context<Self>, focus: &FocusHandle, short: bool) -> Stateful<Div> {
        let (cell_w, line_h) = terminal_metrics(window);
        let fresh_frame = if self.preview_selection.is_none() { self.preview_frame() } else { None };
        let frame = self.preview_selection.as_ref().map(|s| &s.grid.frame).or(fresh_frame.as_ref());
        let drag_available = frame.is_some_and(preview_drag_available);
        let hidden_rows = frame.map_or(0, |f| self.preview_cells.as_ref().map_or(0, |c| c.len()).saturating_sub(f.cells.len()));
        let rows = frame.map(|f| preview_rows(f, cell_w, line_h, self.preview_selection.as_ref().map(|s| &s.grid)));
        let has_rows = rows.is_some();
        let bounds = self.preview_bounds.clone();
        let entity = cx.entity();
        div().id("preview").debug_selector(|| "preview".into()).pt_4().flex_1().min_h(px(if short { 120. } else { 0. })).overflow_hidden().flex().flex_col()
            .when(has_rows, |d| d.child(div().h(px(23.)).flex_none().overflow_hidden().text_sm().text_color(rgb(0xf2c14e))
                .when(!drag_available, |d| d.child(div().id("preview-drag-unavailable").debug_selector(|| "preview-drag-unavailable".into()).whitespace_nowrap().child(if hidden_rows > 0 { format!("Use snapshot · {hidden_rows} hidden") } else { "Use snapshot to copy".to_string() })))
                .when(drag_available, |d| {
                    let key = if cfg!(target_os = "macos") { "⌘C" } else { "Ctrl+C" };
                    if hidden_rows > 0 { d.child(div().id("preview-truncated").debug_selector(|| "preview-truncated".into()).whitespace_nowrap().child(format!("Drag · {key} · row/space · {hidden_rows} hidden"))) }
                    else { d.child(div().whitespace_nowrap().child(format!("Drag · {key} · screen rows/spaces"))) }
                })))
            .when_some(rows, |d, rows| d.child(
                div().on_children_prepainted(move |child, _, cx| {
                    if let Some(first) = child.first() {
                        let rows = |b: gpui::Bounds<gpui::Pixels>| (f32::from(b.size.height) / TERMINAL_LINE_HEIGHT_PX).ceil().max(1.) as usize;
                        let before = bounds.get().map(rows).unwrap_or(40);
                        let after = rows(*first);
                        bounds.set(Some(*first));
                        if before != after { entity.update(cx, |_, cx| cx.notify()); }
                    }
                })
                    .id("preview-grid").debug_selector(|| "preview-grid".into()).w_full().flex_1().min_h(px(0.)).track_focus(focus)
                    .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                        this.preview_selection = None;
                        this.preview_drag_start = Some(event.position);
                        this.preview_dragged = false;
                        this.preview_press = this.selection.clone().filter(|s| this.selection_current(s));
                        if let (Some(frame), Some(identity), Some(bounds)) = (this.preview_frame(), this.selection.clone(), this.preview_bounds.get()) {
                            if this.selection_current(&identity) && preview_drag_available(&frame) {
                                let (w, h) = terminal_metrics(window);
                                if let Some(pos) = terminal_cell_at(event.position, bounds, &frame, w, h) {
                                    this.preview_selection = Some(PreviewSelection { grid: terminal::Selection { frame, start: pos, end: pos, dragging: true, moved: false }, identity, version: this.sync_version, revision: this.preview_revision });
                                    this.preview_drag_start = Some(event.position);
                                }
                            }
                        }
                        if let Some(focus) = &this.preview_focus { focus.focus(window); }
                        cx.notify();
                    }))
                    .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, window, cx| {
                        if event.pressed_button == Some(gpui::MouseButton::Left) {
                            if let Some(start) = this.preview_drag_start {
                                let dx = f32::from(event.position.x - start.x); let dy = f32::from(event.position.y - start.y);
                                this.preview_dragged |= dx.abs() + dy.abs() >= DRAG_THRESHOLD_PX;
                            }
                        }
                        if !this.preview_selection_current() { this.preview_selection = None; return; }
                        if let (Some(selection), Some(bounds)) = (&mut this.preview_selection, this.preview_bounds.get()) {
                            if selection.grid.dragging && event.pressed_button == Some(gpui::MouseButton::Left) {
                                let (w, h) = terminal_metrics(window);
                                if let Some(pos) = terminal_cell_at(event.position, bounds, &selection.grid.frame, w, h) {
                                    selection.grid.end = pos;
                                    if let Some(start) = this.preview_drag_start {
                                        let dx = f32::from(event.position.x - start.x); let dy = f32::from(event.position.y - start.y);
                                        selection.grid.moved |= dx.abs() + dy.abs() >= DRAG_THRESHOLD_PX;
                                    }
                                    cx.notify();
                                }
                            }
                        }
                    }))
                    .on_click(cx.listener(|this, _, window, cx| {
                        let dragged = this.preview_dragged || this.preview_selection.as_ref().is_some_and(|selection| selection.grid.moved);
                        this.preview_dragged = false;
                        let press = this.preview_press.take();
                        let current = this.selection.as_ref();
                        let current_ok = current.is_some_and(|s| this.selection_current(s));
                        if preview_click_focus_allowed(press.as_ref(), current, dragged, current_ok) {
                            this.preview_selection = None;
                            if let Some(focus) = &this.input_focus { focus.focus(window); }
                            cx.notify();
                        }
                    }))
                    .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                        let key = &event.keystroke;
                        let copy_modifier = if cfg!(target_os = "macos") { key.modifiers.platform && !key.modifiers.control } else { key.modifiers.control && !key.modifiers.platform };
                        if key.key == "c" && copy_modifier && !key.modifiers.alt && !key.modifiers.shift && !key.modifiers.function {
                            if this.preview_selection_current() {
                                if let Some(text) = this.preview_selection.as_ref().and_then(|s| s.grid.text()) { cx.write_to_clipboard(gpui::ClipboardItem::new_string(text)); }
                            } else { this.preview_selection = None; }
                            cx.stop_propagation(); cx.notify();
                        }
                    }))
                    .child(div().size_full().flex().flex_col().children(rows))
            ))
            .when(!has_rows, |d| d.child(div().id("preview-text").debug_selector(|| "preview-text".into()).track_focus(focus)
                .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, event: &gpui::MouseDownEvent, window, _| {
                    this.preview_drag_start = Some(event.position);
                    this.preview_dragged = false;
                    this.preview_press = this.selection.clone().filter(|s| this.selection_current(s));
                    if let Some(focus) = &this.preview_focus { focus.focus(window); }
                }))
                .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, _| {
                    if event.pressed_button == Some(gpui::MouseButton::Left) {
                        if let Some(start) = this.preview_drag_start {
                            let dx = f32::from(event.position.x - start.x); let dy = f32::from(event.position.y - start.y);
                            this.preview_dragged |= dx.abs() + dy.abs() >= DRAG_THRESHOLD_PX;
                        }
                    }
                }))
                .child(self.preview.clone().unwrap_or_else(|| "Select a session for a read-only snapshot.".into()))
                .on_click(cx.listener(|this, _, window, cx| {
                    let dragged = this.preview_dragged;
                    this.preview_dragged = false;
                    let press = this.preview_press.take();
                    let current = this.selection.as_ref();
                    let current_ok = current.is_some_and(|s| this.selection_current(s));
                    if preview_click_focus_allowed(press.as_ref(), current, dragged, current_ok) {
                        if let Some(focus) = &this.input_focus { focus.focus(window); }
                        cx.notify();
                    }
                }))))
    }

    /// The keyboard's flat view of the tree, in display order.
    /// Cached rows for this exact fingerprint; the selected peer may still be
    /// in `remote_sessions` before the cache insert. Never another node's rows.
    fn peer_rows(&self, fingerprint: &str) -> Option<&[Session]> {
        self.peer_sessions
            .get(fingerprint)
            .map(Vec::as_slice)
            .or_else(|| {
                (self.selected_peer.as_deref() == Some(fingerprint))
                    .then_some(self.remote_sessions.as_slice())
            })
    }

    /// The row a tree key at `index` names. A peer miss refuses instead of
    /// indexing the local list with a remote index.
    fn tree_session_at(&self, peer: Option<&str>, index: usize) -> Option<Session> {
        let rows = match peer {
            None => self.sessions.as_slice(),
            Some(fingerprint) if self.peer_current(fingerprint) => {
                self.peer_rows(fingerprint)?
            }
            Some(_) => return None,
        };
        rows.get(index).cloned()
    }

    fn tree_entries(&self) -> Vec<TreeEntry> {
        let mut entries = vec![TreeEntry::Node(None)];
        if self.expanded_nodes.contains("") {
            entries.extend((0..self.sessions.len()).map(|i| TreeEntry::Session(None, i)));
        }
        for peer in &self.peers {
            entries.push(TreeEntry::Node(Some(peer.fingerprint.clone())));
            if self.expanded_nodes.contains(&peer.fingerprint) {
                let count = self.peer_rows(&peer.fingerprint).map_or(0, <[Session]>::len);
                entries.extend(
                    (0..count).map(|i| TreeEntry::Session(Some(peer.fingerprint.clone()), i)),
                );
            }
        }
        entries
    }

    /// Direct sidebar children in the same order as `tree_entries`, including
    /// the conditional notice/empty rows that are not keyboard targets.
    fn sidebar_child_indices(&self) -> Vec<usize> {
        let mut child = TREE_FIXED_HEADER_CHILDREN + usize::from(self.helper_changed);
        let mut indices = vec![child];
        child += 1;
        if self.expanded_nodes.contains("") {
            if self.sessions.iter().any(|row| !row.structured) {
                child += 1;
            }
            for _ in &self.sessions {
                indices.push(child);
                child += 1;
            }
        }
        for peer in &self.peers {
            indices.push(child);
            child += 1;
            if !self.expanded_nodes.contains(&peer.fingerprint) {
                continue;
            }
            let rows = self.peer_rows(&peer.fingerprint);
            if rows.is_some_and(|rows| rows.iter().any(|row| !row.structured)) {
                child += 1;
            }
            if rows.is_none_or(<[Session]>::is_empty) {
                child += 1;
            }
            if let Some(rows) = rows {
                for _ in rows {
                    indices.push(child);
                    child += 1;
                }
            }
        }
        indices
    }

    fn reveal_tree_cursor(&self) {
        if let Some(index) = self.sidebar_child_indices().get(self.tree_cursor) {
            self.sidebar_scroll.scroll_to_item(*index);
        }
    }

    fn tree_key(&mut self, key: &gpui::Keystroke) {
        if key.modifiers.control || key.modifiers.platform || key.modifiers.alt || key.modifiers.function {
            return;
        }
        let entries = self.tree_entries();
        if entries.is_empty() {
            return;
        }
        self.tree_cursor = self.tree_cursor.min(entries.len() - 1);
        match key.key.as_str() {
            "down" => {
                self.tree_cursor = (self.tree_cursor + 1).min(entries.len() - 1);
                self.reveal_tree_cursor();
            }
            "up" => {
                self.tree_cursor = self.tree_cursor.saturating_sub(1);
                self.reveal_tree_cursor();
            }
            "left" => {
                if let Some(key) = entries[..=self.tree_cursor].iter().rev().find_map(|entry| {
                    if let TreeEntry::Node(key) = entry {
                        Some(key.clone())
                    } else {
                        None
                    }
                }) {
                    let key = key.unwrap_or_default();
                    self.expanded_nodes.remove(&key);
                }
            }
            "right" => {
                if let Some(TreeEntry::Node(key)) = entries.get(self.tree_cursor) {
                    let key = key.clone().unwrap_or_default();
                    self.expanded_nodes.insert(key.clone());
                    if !key.is_empty() {
                        self.select_peer(key);
                    }
                }
            }
            "enter" => match entries.get(self.tree_cursor) {
                Some(TreeEntry::Node(key)) => {
                    let key = key.clone().unwrap_or_default();
                    self.select_node(key);
                }
                Some(TreeEntry::Session(peer, index)) => {
                    let row = self.tree_session_at(peer.as_deref(), *index);
                    if let Some(row) = row {
                        self.activate_session(peer.clone(), row);
                    }
                }
                None => {}
            },
            _ => {}
        }
    }

    /// One node's session rows. The active node keeps the historical
    /// `session-N` ids; other nodes get a node-specific prefix so a tree with
    /// several expanded nodes has no duplicate selectors.
    fn session_rows(
        &self,
        peer: Option<String>,
        rows: &[Session],
        active_node: bool,
        cx: &mut Context<Self>,
        prefix: &str,
    ) -> Vec<Stateful<Div>> {
        let entries = self.tree_entries();
        rows.iter()
            .cloned()
            .enumerate()
            .map(|(index, session)| {
                let mut status = if session.unavailable {
                    "unavailable".to_string()
                } else if session.alive {
                    "running".to_string()
                } else {
                    "exited".to_string()
                };
                if session.attached && !session.unavailable {
                    status.push_str(" ·A");
                }
                let selected = session.clone();
                let is_selected = active_node
                    && self.selection.as_ref().is_some_and(|s| {
                        self.selection_current(s)
                            && s.peer == peer
                            && s.name == session.name
                            && session.instance_id.as_deref() == Some(s.instance.as_str())
                    });
                let click_peer = peer.clone();
                let id = if active_node {
                    format!("session-{index}")
                } else {
                    format!("{prefix}-session-{index}")
                };
                let marker = session_marker(&session).to_string();
                let indent = f32::from(session.depth.min(20)) * TREE_INDENT_PX;
                let detail = session.detail.clone();
                let name_id = format!("{id}-name");
                let text_id = format!("{id}-text");
                let status_id = format!("{id}-status");
                let detail_id = format!("{id}-detail");
                let cursor_id = format!("{id}-cursor");
                let cursor = matches!(entries.get(self.tree_cursor), Some(TreeEntry::Session(p, i)) if *i == index && p == &peer);
                div()
                    .id(SharedString::from(id.clone()))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.activate_session(click_peer.clone(), selected.clone());
                        if this.session_input_visible() {
                            if f32::from(window.viewport_size().width) < 700. { this.narrow_view = NarrowView::Session; }
                            if let Some(focus) = &this.input_focus { focus.focus(window); }
                            cx.stop_propagation();
                        }
                        cx.notify();
                    }))
                    .debug_selector(move || id.clone())
                    .pl(px(16. + indent))
                    .py_2()
                    .when(is_selected, |d| d.bg(rgb(0x34445a)))
                    .when(cursor, |d| d.border_1().border_color(rgb(0x91b9ef)))
                    .child(div().id(SharedString::from(name_id.clone())).debug_selector(move || name_id.clone())
                        .w_full().flex().items_center().min_w(px(0.))
                        .when(cursor, |d| d.child(
                            div().id(SharedString::from(cursor_id.clone()))
                                .debug_selector(move || cursor_id.clone())
                                .w(px(2.)).h(px(2.)).bg(rgb(0x91b9ef))))
                        .when(is_selected, |d| d.child(
                            div().id("selected-session")
                                .debug_selector(|| "selected-session".into())
                                .w(px(2.)).h(px(2.)).bg(rgb(0xc9d4e4))))
                        .child(div().id(SharedString::from(text_id.clone()))
                            .debug_selector(move || text_id.clone())
                            .flex_1().min_w(px(56.)).overflow_hidden()
                            .whitespace_nowrap().text_ellipsis()
                            .child(format!("{}  {}", marker, plain_label(&session.name))))
                        .child(div().id(SharedString::from(status_id.clone()))
                            .debug_selector(move || status_id.clone())
                            .flex_shrink().min_w(px(0.)).max_w(px(96.)).pl_2().text_sm()
                            .text_color(rgb(0x8a95a5)).whitespace_nowrap().text_ellipsis()
                            .child(status)))
                    .when_some(detail, |d, detail| d.child(
                        div().id(SharedString::from(detail_id.clone())).debug_selector(move || detail_id.clone())
                            .pl(px(TREE_DETAIL_OFFSET_PX)).text_sm().text_color(rgb(0x8a95a5))
                            .child(format!("↳ {}", plain_label(&detail)))))
            })
            .collect()
    }
    fn invalidate_remote(&mut self) {
        self.clear_preview();
        self.remote_generation = self
            .remote_generation
            .checked_add(1)
            .expect("generation exhausted");
        self.remote_pending = None;
        self.remote_loading = false;
        if let Some(fp) = self.selected_peer.as_ref() {
            self.peer_sessions.remove(fp);
            self.remote_sessions.clear();
        }
        if self.selected_peer.is_some() {
            self.remote_error = true;
        }
    }
    fn refresh_peer(&mut self) {
        self.clear_preview();
        let Some(fp) = self
            .selected_peer
            .clone()
            .filter(|fp| self.peer_current(fp))
        else {
            return;
        };
        self.remote_generation = self
            .remote_generation
            .checked_add(1)
            .expect("generation exhausted");
        self.remote_loading = true;
        self.remote_pending = Some((self.remote_generation, self.nodes_generation, fp));
        self.launch_peer();
    }
    fn launch_peer(&mut self) {
        if self.remote_busy {
            return;
        }
        let Some((generation, directory, fp)) = self.remote_pending.take() else {
            return;
        };
        if directory != self.nodes_generation
            || self.selected_peer.as_ref() != Some(&fp)
            || !self.peer_current(&fp)
        {
            return;
        }
        self.remote_busy = true;
        let sender = self.remote_sender.clone();
        std::thread::spawn(move || {
            let result = worker_executable()
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
    /// Drain background workers and report whether anything visible changed.
    /// The 150ms timer uses this to skip rebuilding a large preview when the
    /// session output and frame are unchanged.
    fn drain(&mut self) -> bool {
        let mut changed = false;
        if let Some(pane) = &mut self.documents {
            changed |= pane.poll_changed();
        }
        if self.selection_error == Some("Historical source unavailable. Refresh loads the current version.")
            && self.documents.as_ref().is_some_and(|w| w.active().source_text().is_some())
        {
            self.selection_error = None;
            changed = true;
        }
        if self
            .terminal
            .as_ref()
            .is_some_and(|terminal| terminal.ended())
            && self.terminal_status != "Terminal ended. Inspect its output or attach again."
        {
            self.terminal_status = "Terminal ended. Inspect its output or attach again.".into();
            changed = true;
        }
        let input_event = self
            .input_helper
            .as_ref()
            .and_then(|helper| helper.recv_event_timeout(Duration::ZERO).ok());
        if let Some(event) = input_event {
            self.input_helper = None;
            changed = true;
            match event {
                input_helper::Event::Applied => self.input_status = "Sent".into(),
                input_helper::Event::NotAcknowledged => {
                    self.warn_input_target();
                    self.input_stopped = true;
                    self.input_status = "Delivery uncertain; input disabled".into();
                }
            }
        }
        while let Ok((generation, selection, result)) = self.sync_results.try_recv() {
            self.sync_busy = false;
            if generation == self.preview_generation
                && self.selection.as_ref() == Some(&selection)
                && self.selection_current(&selection)
            {
                match result {
                    Ok(frame)
                        if frame.instance == selection.instance
                            && frame.version >= self.sync_version
                            && frame.text.len() <= 65_536 =>
                    {
                        if frame.version != self.sync_version && self.preview_selection.is_some() { self.invalidate_preview_selection(); changed = true; }
                        let cells = (!frame.cells.is_empty()).then_some(frame.cells);
                        let text = Some(frame.text);
                        if self.preview != text || self.preview_cells != cells {
                            self.invalidate_preview_selection();
                            self.preview = text;
                            // A text-only v1/fallback frame must stay on the
                            // plain-text path, not a zero-dimension grid.
                            self.preview_cells = cells;
                            changed = true;
                        }
                        self.sync_version = frame.version;
                        if self.sync_error {
                            self.sync_error = false;
                            changed = true;
                        }
                    }
                    _ => {
                        self.invalidate_preview_selection();
                        if self.preview.is_some() || !self.sync_error {
                            changed = true;
                        }
                        self.preview = None;
                        self.preview_cells = None;
                        self.sync_error = true;
                    }
                }
            }
        }
        self.sync_tick = self.sync_tick.saturating_add(1);
        if self.sync_tick >= if self.sync_error { 14 } else { 1 } {
            self.sync_tick = 0;
            if let Some(selection) = self.selection.clone().filter(|s| self.selection_current(s)) {
                self.launch_sync(selection);
            }
        }
        while let Ok((generation, selection, result)) = self.preview_results.try_recv() {
            self.preview_busy = false;
            if generation == self.preview_generation
                && self.selection.as_ref() == Some(&selection)
                && self.selection_current(&selection)
            {
                changed = true;
                match result {
                    Ok(payload) => {
                        self.invalidate_preview_selection();
                        self.preview = Some(payload.text);
                        self.preview_cells =
                            (!payload.cells.is_empty()).then_some(payload.cells);
                    }
                    Err(error) => {
                        self.invalidate_preview_selection();
                        if error == STALE_HELPER {
                            self.helper_changed = true;
                        }
                        self.preview = Some(
                            "Preview unavailable; if the GUI was updated, restart it".into(),
                        );
                        self.preview_cells = None;
                    }
                }
            }
            self.launch_preview();
        }
        while let Ok((generation, result)) = self.results.try_recv() {
            changed = true;
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
            changed = true;
            self.nodes_busy = false;
            if generation == self.nodes_generation {
                self.nodes_loading = false;
                match result {
                    Ok(peers) => {
                        self.peers = peers;
                        self.nodes_error = false;
                        let valid: std::collections::HashSet<String> = self
                            .peers
                            .iter()
                            .filter(|peer| self.peer_current(&peer.fingerprint))
                            .map(|peer| peer.fingerprint.clone())
                            .collect();
                        self.peer_sessions.retain(|fp, _| valid.contains(fp));
                    }
                    Err(error) => {
                        self.helper_changed |= error == STALE_HELPER;
                        self.nodes_error = true;
                        // A failed node refresh cannot vouch for any cached
                        // child rows; hide them instead of showing an old
                        // running/exited state beside "unavailable".
                        self.peer_sessions.clear();
                        self.remote_sessions.clear();
                        self.clear_preview();
                        self.remote_error = self.selected_peer.is_some();
                    }
                }
                if self
                    .selected_peer
                    .as_ref()
                    .is_some_and(|fp| !self.peer_current(fp))
                {
                    self.selected_peer = None;
                    self.remote_sessions.clear();
                }
                if !self.nodes_error && self.selected_peer.is_some() {
                    self.refresh_peer();
                }
            }
            self.launch_nodes();
        }
        while let Ok((generation, directory, fp, result)) = self.remote_results.try_recv() {
            changed = true;
            self.remote_busy = false;
            if generation == self.remote_generation
                && directory == self.nodes_generation
                && self.selected_peer.as_ref() == Some(&fp)
                && self.peer_current(&fp)
            {
                self.remote_loading = false;
                match result {
                    Ok(rows) => {
                        self.peer_sessions.insert(fp.clone(), rows.clone());
                        self.remote_sessions = rows;
                        self.remote_error = false;
                    }
                    Err(error) => {
                        self.helper_changed |= error == STALE_HELPER;
                        self.peer_sessions.remove(&fp);
                        self.remote_sessions.clear();
                        self.remote_error = true;
                    }
                }
            }
            self.launch_peer();
        }
        changed
    }
}
/// Monospace cell metrics. The column count is measured from the same font the
/// rows use, so a wider window asks the PTY for exactly the columns it draws.
fn terminal_metrics(window: &Window) -> (f32, f32) {
    let mut style = window.text_style();
    style.font_family = "Menlo".into();
    style.font_size = px(13.).into();
    let run = style.to_run(1);
    let width: f32 = window
        .text_system()
        .layout_line("M", px(13.), std::slice::from_ref(&run), None)
        .width
        .into();
    (width.max(1.), TERMINAL_LINE_HEIGHT_PX)
}

fn rgba(value: u32) -> gpui::Rgba {
    rgb(value)
}

fn terminal_cell_at(point: gpui::Point<gpui::Pixels>, bounds: gpui::Bounds<gpui::Pixels>, frame: &terminal::Frame, cell_w: f32, line_h: f32) -> Option<(usize, usize)> {
    if frame.rows == 0 || frame.cols == 0 { return None; }
    let x = f32::from(point.x - bounds.left()).max(0.);
    let y = f32::from(point.y - bounds.top()).max(0.);
    Some(((y / line_h).floor().min(f32::from(frame.rows - 1)) as usize,
          (x / cell_w).floor().min(f32::from(frame.cols - 1)) as usize))
}

fn preview_runs<'a>(row: &'a [terminal::Cell], row_index: usize, selection: Option<&terminal::Selection>) -> Vec<(&'a terminal::Cell, bool, String, usize)> {
    let mut runs: Vec<(&terminal::Cell, bool, String, usize)> = Vec::new();
    for (col, cell) in row.iter().enumerate() {
        if cell.continuation { continue; }
        let selected = selection.is_some_and(|s| s.contains(row_index, col));
        let simple = !cell.wide && (cell.text.is_empty() || (cell.text.len() == 1 && cell.text.is_ascii()));
        let value = if cell.text.is_empty() { " " } else { &cell.text };
        if simple {
            if let Some((previous, prev_selected, text, width)) = runs.last_mut() {
                let same_style = previous.fg == cell.fg && previous.bg == cell.bg && previous.bold == cell.bold && previous.dim == cell.dim && previous.italic == cell.italic && previous.underline == cell.underline && previous.inverse == cell.inverse;
                let previous_simple = !previous.wide && (previous.text.is_empty() || (previous.text.len() == 1 && previous.text.is_ascii()));
                if previous_simple && same_style && *prev_selected == selected {
                    text.push_str(value); *width += 1; continue;
                }
            }
        }
        runs.push((cell, selected, value.to_owned(), if cell.wide { 2 } else { 1 }));
    }
    runs
}

/// A plain preview click focuses input only when the release is the exact
/// selection pressed and that selection is still current.
fn preview_click_focus_allowed(
    press: Option<&Selection>,
    current: Option<&Selection>,
    dragged: bool,
    current_ok: bool,
) -> bool {
    !dragged && press.is_some() && press == current && current_ok
}

fn preview_drag_available(frame: &terminal::Frame) -> bool {
    let mut count = 0;
    for (index, row) in frame.cells.iter().enumerate() {
        count += preview_runs(row, index, None).len();
        if count > MAX_PREVIEW_DRAG_RUNS { return false; }
    }
    true
}

fn preview_rows(frame: &terminal::Frame, cell_w: f32, line_h: f32, selection: Option<&terminal::Selection>) -> Vec<Stateful<Div>> {
    frame.cells.iter().enumerate().map(|(row_index, row)| {
        let id = format!("preview-row-{row_index}");
        div().id(SharedString::from(id.clone())).debug_selector(move || id.clone()).flex().h(px(line_h))
            .children(preview_runs(row, row_index, selection).into_iter().map(|(cell, selected, text, width)| {
                let (fg, bg) = terminal::resolve_colors(cell.fg, cell.bg, cell.inverse);
                div().w(px(cell_w * width as f32)).h(px(line_h)).flex_none().bg(rgba(bg)).text_color(rgba(fg))
                    .when(selected, |d| d.bg(rgb(0x48658c)).text_color(rgb(0xffffff)))
                    .font_family("Menlo").text_size(px(13.)).line_height(px(line_h)).whitespace_nowrap()
                    .when(cell.bold, |d| d.font_weight(gpui::FontWeight::BOLD)).when(cell.italic, |d| d.italic())
                    .when(cell.underline, |d| d.underline()).when(cell.dim, |d| d.opacity(0.7)).child(text)
            }))
    }).collect()
}

fn terminal_rows(
    frame: &terminal::Frame,
    cell_w: f32,
    line_h: f32,
    prefix: &str,
    selection: Option<&terminal::Selection>,
) -> Vec<Stateful<Div>> {
    let cursor = if frame.cursor_visible {
        let (row, col) = frame.cursor;
        // A terminal cursor may land on the continuation cell of a wide glyph;
        // highlight the glyph's first cell, which is where it is drawn.
        let col = frame
            .cells
            .get(usize::from(row))
            .and_then(|line| line.get(usize::from(col)))
            .filter(|cell| cell.continuation && col > 0)
            .map_or(col, |_| col - 1);
        Some((row, col))
    } else {
        None
    };
    frame
        .cells
        .iter()
        .enumerate()
        .map(|(row_index, row)| {
            let row_id = format!("{prefix}-row-{row_index}");
            div()
                .id(SharedString::from(row_id.clone()))
                .debug_selector(move || row_id.clone())
                .flex()
                .h(px(line_h))
                .children(row.iter().enumerate().filter_map(|(col_index, cell)| {
                    if cell.continuation {
                        return None;
                    }
                    let (fg, bg) = terminal::resolve_colors(cell.fg, cell.bg, cell.inverse);
                    let width = if cell.wide { cell_w * 2. } else { cell_w };
                    let cursor_here = cursor == Some((row_index as u16, col_index as u16));
                    let cursor_id = format!("{prefix}-cursor");
                    Some(
                        div()
                            .w(px(width))
                            .h(px(line_h))
                            .flex_none()
                            .bg(rgba(bg))
                            .text_color(rgba(fg))
                            .when(selection.is_some_and(|s| s.contains(row_index, col_index)), |d| d.bg(rgb(0x48658c)).text_color(rgb(0xffffff)))
                            .font_family("Menlo")
                            .text_size(px(13.))
                            .line_height(px(line_h))
                            .when(cell.bold, |d| d.font_weight(gpui::FontWeight::BOLD))
                            .when(cell.italic, |d| d.italic())
                            .when(cell.underline, |d| d.underline())
                            .when(cell.dim, |d| d.opacity(0.7))
                            .when(cursor_here, |d| {
                                d.bg(rgb(0xd0d0d0))
                                    .text_color(rgb(0x101216))
                                    .child(
                                        div()
                                            .id(SharedString::from(cursor_id.clone()))
                                            .debug_selector(move || cursor_id.clone()),
                                    )
                            })
                            .child(if cell.text.is_empty() {
                                SharedString::from(" ")
                            } else {
                                SharedString::from(cell.text.clone())
                            }),
                    )
                }))
        })
        .collect()
}

impl document_workspace_view::DocumentHost for LocalGui {
    fn rich_selections(&self) -> Option<document_workspace_view::SelectionMap> { Some(self.rich_selections.clone()) }
    fn documents(&self) -> Option<&documents::Workspace> {
        self.documents.as_ref()
    }

    fn documents_mut(&mut self) -> Option<&mut documents::Workspace> {
        self.documents.as_mut()
    }

    fn select_document_source(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.documents.as_ref().is_some_and(|w| w.active().is_document() && !w.active().is_image()) {
            return;
        }
        let text = self.documents.as_ref().and_then(|w| w.active().source_text().map(|text| (text.to_owned(), w.active().location().to_owned(), w.active().source_provenance(), w.active().source_identity())));
        if let Some((text, path, provenance, identity)) = text {
            let shown = label(&path);
            let compact = if shown.chars().count() > 36 { format!("{}…", shown.chars().take(33).collect::<String>()) } else { shown.clone() };
            let detail = identity.map(|identity| format!("Document: {shown} · {identity}"));
            self.open_selection_snapshot(&text, true, format!("Document: {compact} · {provenance}"), detail, window, cx);
        } else {
            self.selection_error = Some("Historical source unavailable. Refresh loads the current version.");
        }
    }
}

impl Render for LocalGui {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        let live_frame = self.terminal.as_ref().map(|t| t.frame());
        if self.preview_selection.is_some() && !self.preview_selection_current() { self.preview_selection = None; }
        if self.terminal_selection.as_ref().is_some_and(|s| live_frame.as_ref().is_none_or(|frame| !s.valid_for(frame))) {
            self.terminal_selection = None;
        }
        let viewport_width: f32 = viewport.width.into();
        let short = f32::from(viewport.height) < 500.;
        let narrow = viewport_width < 700.;
        let entering_narrow = narrow && !self.was_narrow;
        if narrow != self.was_narrow {
            self.was_narrow = narrow;
            if narrow {
                self.narrow_view = if self.session_input_visible() { NarrowView::Session } else { NarrowView::Nodes };
            }
        }
        let sidebar_width = (viewport_width * 0.20).clamp(110.0, 260.0);
        let max_document_width = (viewport_width - sidebar_width - 126.0).max(160.0);
        let document_width =
            (self.document_panel_fraction * viewport_width).clamp(160.0, max_document_width);
        let input_focus = self
            .input_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        let preview_focus = self.preview_focus.get_or_insert_with(|| cx.focus_handle()).clone();
        let documents_open = self.documents.is_some();
        let tree_focus = self.tree_focus.get_or_insert_with(|| cx.focus_handle()).clone();
        if !self.initial_focus_set {
            self.initial_focus_set = true;
            tree_focus.focus(window);
        }
        let documents_focus = self
            .documents_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        let divider_focus = self
            .divider_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        let collapse_focus = self
            .collapse_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        let toggle_focus = self.toggle_focus.get_or_insert_with(|| cx.focus_handle()).clone();
        if entering_narrow {
            match self.narrow_view { NarrowView::Nodes => tree_focus.focus(window), NarrowView::Session => input_focus.focus(window), NarrowView::Documents => documents_focus.focus(window) }
        }
        let title = if self.nodes_loading || self.loading {
            "NODES · SESSIONS · LOADING"
        } else if self.nodes_error || self.error {
            "NODES · SESSIONS · STALE"
        } else {
            "NODES · SESSIONS"
        };
        let preview_panel = self.terminal.is_none().then(|| self.preview_panel(window, cx, &preview_focus, short));
        let snapshot = self.selection_snapshot.clone();
        div()
            .size_full()
            .relative()
            .flex()
            .bg(rgb(0x101216))
            .text_color(rgb(0xe8eaf0))
            .font_family("Menlo")
            .when(narrow, |root| root.child(
                div().id("narrow-navigation").debug_selector(|| "narrow-navigation".into()).w(px(84.)).flex_none().flex().flex_col().gap_2().p_2().bg(rgb(0x191d24))
                    .children([(NarrowView::Nodes, "⌘1 Nodes"), (NarrowView::Session, "⌘2 Term"), (NarrowView::Documents, "⌘3 Docs")].into_iter().enumerate().map(|(i, (view, title))| {
                        div().id(SharedString::from(format!("narrow-nav-{i}"))).debug_selector(move || format!("narrow-nav-{i}"))
                            .py_2().text_sm().whitespace_nowrap().cursor_pointer().when(self.narrow_view == view, |d| d.bg(rgb(0x34445a))).child(title)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.show_narrow_view(view, window);
                                cx.notify();
                            }))
                    }))
            ))
            .when(!narrow || self.narrow_view == NarrowView::Nodes, |root| root.child(
                div()
                    .id("sidebar")
                    .debug_selector(|| "sidebar".into())
                    .w(px(if narrow { viewport_width - 84. } else { sidebar_width }))
                    .min_w(px(if narrow { viewport_width - 84. } else { sidebar_width }))
                    .h_full()
                    .flex()
                    .flex_col()
                    .bg(rgb(0x191d24))
                    .p_4()
                    .overflow_y_scroll()
                    .track_scroll(&self.sidebar_scroll)
                    .track_focus(&tree_focus)
                    .on_click(cx.listener(|this, _, window, cx| {
                        if let Some(focus) = &this.tree_focus {
                            focus.focus(window);
                        }
                        cx.notify();
                    }))
                    .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                        this.tree_key(&event.keystroke);
                        cx.notify();
                    }))
                    .child(div().text_lg().child("REMUDA"))
                    .when(self.helper_changed, |d| {
                        d.child(
                            div()
                                .id("helper-changed")
                                .debug_selector(|| "helper-changed".into())
                                .child(STALE_HELPER),
                        )
                    })
                    .child(div().pt_4().text_sm().child(if self.nodes_loading {
                        "NODES · LOADING"
                    } else if self.nodes_error {
                        "NODES · STALE"
                    } else {
                        "NODES"
                    }))
                    .child(
                        div()
                            .id("node-local")
                            .debug_selector(|| "node-local".into())
                            .py_2()
                            .cursor_pointer()
                            .flex()
                            .child(
                                div()
                                    .id("node-toggle-local")
                                    .debug_selector(|| "node-toggle-local".into())
                                    .pr_1()
                                    .child(if self.expanded_nodes.contains("") { "▾" } else { "▸" })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.toggle_node(String::new());
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .id("node-local-label")
                                    .flex_1()
                                    .child("This device")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.select_node(String::new());
                                        cx.notify();
                                    })),
                            ),
                    )
                    .when(self.expanded_nodes.contains("") && self.sessions.iter().any(|row| !row.structured), |d| d.child(
                        div().id("local-hierarchy-fallback").debug_selector(|| "local-hierarchy-fallback".into())
                            .pl_4().text_xs().child("Hierarchy unavailable · flat session list")))
                    .children(if self.expanded_nodes.contains("") {
                        self.session_rows(None, &self.sessions, self.selected_peer.is_none(), cx, "local")
                    } else {
                        Vec::new()
                    })
                    .children(self.peers.iter().cloned().enumerate().flat_map(|(index, peer)| {
                        let fingerprint = peer.fingerprint.clone();
                        let enabled = self.peer_current(&fingerprint);
                        let expanded = self.expanded_nodes.contains(&fingerprint);
                        let name = label(&peer.label);
                        let state = if !peer.admitted {
                            "revoked"
                        } else if self.nodes_loading {
                            "loading"
                        } else if enabled {
                            "admitted"
                        } else {
                            "unavailable"
                        };
                        let state_id = format!("node-peer-state-{index}");
                        let id = SharedString::from(format!("node-{}-{index}", peer.fingerprint));
                        let debug = format!("node-peer-{index}");
                        let click_peer = fingerprint.clone();
                        let rows = self
                            .peer_rows(&fingerprint)
                            .map(<[Session]>::to_vec)
                            .unwrap_or_default();
                        let selected_node = self.selected_peer.as_ref() == Some(&fingerprint);
                        let children = if expanded {
                            self.session_rows(
                                Some(fingerprint.clone()),
                                &rows,
                                selected_node,
                                cx,
                                &format!("peer-{index}"),
                            )
                        } else {
                            Vec::new()
                        };
                        let mut nodes: Vec<gpui::AnyElement> = vec![
                            div()
                                .id(id)
                                .debug_selector(move || debug.clone())
                                .py_2()
                                .cursor_pointer()
                                .flex()
                                .child(
                                    div()
                                        .id(SharedString::from(format!("node-toggle-peer-{index}")))
                                        .debug_selector(move || format!("node-toggle-peer-{index}"))
                                        .pr_1()
                                        .child(if expanded { "▾" } else { "▸" })
                                        .on_click(cx.listener({
                                            let toggle_peer = click_peer.clone();
                                            move |this, _, _, cx| {
                                                this.toggle_node(toggle_peer.clone());
                                                cx.notify();
                                            }
                                        })),
                                )
                                .child(
                                    div()
                                        .id(SharedString::from(format!("node-peer-label-{index}")))
                                        .debug_selector(move || format!("node-peer-label-{index}"))
                                        .flex_1()
                                        .min_w(px(0.))
                                        .flex()
                                        .items_center()
                                        .child(div().flex_1().min_w(px(56.)).overflow_hidden()
                                            .whitespace_nowrap().text_ellipsis().child(name))
                                        .child(div().id(SharedString::from(state_id.clone()))
                                            .debug_selector(move || state_id.clone())
                                            .pl_2().text_xs().max_w(px(96.))
                                            .text_color(if peer.admitted { rgb(0x7f9f7f) } else { rgb(0xd08b8b) })
                                            .whitespace_nowrap().text_ellipsis().child(state))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.select_node(click_peer.clone());
                                            cx.notify();
                                        })),
                                )
                                .into_any_element(),
                        ];
                        if expanded && rows.iter().any(|row| !row.structured) {
                            let fallback_id = format!("peer-{index}-hierarchy-fallback");
                            nodes.push(div().id(SharedString::from(fallback_id.clone()))
                                .debug_selector(move || fallback_id.clone())
                                .pl_4().text_xs().child("Hierarchy unavailable · flat session list")
                                .into_any_element());
                        }
                        if expanded && rows.is_empty() {
                            nodes.push(div().pl_4().text_sm().text_color(rgb(0x8a95a5)).child(
                                if selected_node && self.remote_loading {
                                    "loading…"
                                } else if self.nodes_error {
                                    "sessions hidden while nodes are unavailable"
                                } else if selected_node && self.remote_error {
                                    "sessions unavailable"
                                } else {
                                    "no sessions loaded"
                                },
                            ).into_any_element());
                        }
                        nodes.extend(children.into_iter().map(IntoElement::into_any_element));
                        nodes
                    }))
                    .child(div().pt_4().text_sm().child(title))
            ))
            .when(!narrow || self.narrow_view == NarrowView::Session, |root| root.child(
                div().id("session-column").debug_selector(|| "session-column".into())
                    .flex_1()
                    .min_w(px(0.))
                    .min_h(px(0.))
                    .h_full()
                    .when(short, |d| d.overflow_y_scroll())
                    .p_4()
                    .flex()
                    .flex_col()
                    .when(short, |d| d.child(div().text_sm().child("Scroll for preview and session controls")))
                    .child(
                        div()
                            .id("documents-toggle")
                            .debug_selector(|| "documents-toggle".into())
                            .py_2()
                            .cursor_pointer()
                            .child(if documents_open {
                                "Close documents"
                            } else {
                                "Documents"
                            })
                            .track_focus(&toggle_focus)
                            .when(toggle_focus.is_focused(window), |d| d.bg(rgb(0x34445a)))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_documents(window);
                                cx.notify();
                            }))
                            .on_key_down(cx.listener(
                                |this, event: &gpui::KeyDownEvent, window, cx| {
                                    let modifiers = &event.keystroke.modifiers;
                                    if modifiers.control
                                        || modifiers.platform
                                        || modifiers.alt
                                        || modifiers.function
                                        || modifiers.shift
                                    {
                                        return;
                                    }
                                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                        this.toggle_documents(window);
                                        cx.notify();
                                    }
                                },
                            )),
                    )
                    .when(self.terminal.is_some() || self.preview.is_some(), |d| {
                        d.child(div().id("select-terminal-text").debug_selector(|| "select-terminal-text".into()).py_2().cursor_pointer().when(input_focus.is_focused(window), |d| d.bg(rgb(0x34445a))).child(if self.terminal.is_none() && self.preview_cells.is_none() { if cfg!(target_os = "macos") { "Select text-only preview (⌘⇧C)" } else { "Select text-only preview (Ctrl+Shift+C)" } } else if cfg!(target_os = "macos") { "Select terminal snapshot (⌘⇧C)" } else { "Select terminal snapshot (Ctrl+Shift+C)" })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_terminal_snapshot(window, cx);
                                cx.notify();
                            })))
                    })
                    .when(documents_open, |d| {
                        let key_collapse = collapse_focus.clone();
                        let key_divider = divider_focus.clone();
                        let click_collapse = collapse_focus.clone();
                        let click_divider = divider_focus.clone();
                        d.child(
                            div()
                                .id("document-collapse")
                                .debug_selector(|| "document-collapse".into())
                                .py_2()
                                .cursor_pointer()
                                .track_focus(&collapse_focus)
                                .when(collapse_focus.is_focused(window), |d| d.bg(rgb(0x34445a)))
                                .child(if self.document_collapsed {
                                    "Restore documents"
                                } else {
                                    "Collapse documents"
                                })
                                .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, window, cx| {
                                    let m = &event.keystroke.modifiers;
                                    if m.control || m.platform || m.alt || m.function || m.shift || !matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                        return;
                                    }
                                    this.document_collapsed = !this.document_collapsed;
                                    if this.document_collapsed {
                                        key_collapse.focus(window);
                                    } else {
                                        key_divider.focus(window);
                                    }
                                    cx.notify();
                                }))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.document_collapsed = !this.document_collapsed;
                                    if this.document_collapsed {
                                        click_collapse.focus(window);
                                    } else {
                                        click_divider.focus(window);
                                    }
                                    cx.notify();
                                })),
                        )
                    })
                    .child(div().child(if self.selected_peer.is_some() {
                        "Peer session · live screen"
                    } else {
                        "Local session"
                    }))
                    .children(self.input_warnings.iter().map(|s| div().id("input-warning").debug_selector(|| "input-warning".into()).text_color(rgb(0xf28787)).child(format!("Delivery uncertain for {}. Check this session before restarting the GUI; input is disabled.", selection_label(s)))))
                    .when_some(self.input_target.as_ref(), |d, target| d.child(div().id("input-result-target").debug_selector(|| "input-result-target".into()).child(format!("{}: {}", self.input_status, selection_label(target)))))
                    .when(self.selected_peer.is_some() && self.remote_error, |d| {
                        d.child(
                            div()
                                .text_color(rgb(0xf28787))
                                .child("Peer unavailable; listed sessions may be stale."),
                        )
                    })
                    .when(self.selected_peer.is_none() && self.error, |d| {
                        d.child(
                            div()
                                .text_color(rgb(0xf28787))
                                .child("Local daemon unavailable; listed sessions may be stale."),
                        )
                    })
                    .when(self.sync_error, |d| d.child(div().id("sync-error").debug_selector(|| "sync-error".into()).text_color(rgb(0xf28787)).child("Live output unavailable; retrying.")))
                    .when(self.terminal.is_none() && self.selection.as_ref().is_some_and(|s| s.peer.is_none() && self.selection_current(s)), |d| {
                        let confirming = self.attach_confirm.as_ref() == self.selection.as_ref();
                        d
                            .when(confirming, |d| {
                                d.child(div().id("attach-confirm").debug_selector(|| "attach-confirm".into()).child(format!("Confirm attach: {}", selection_label(self.selection.as_ref().unwrap()))))
                                    .child(div().child("Attach takes over any current terminal. If this name is recreated during attach, the new session may be selected."))
                                    .child(div().id("confirm-attach").debug_selector(|| "confirm-attach".into()).py_2().px_2().bg(rgb(0x34445a)).cursor_pointer().child("Confirm attach").on_click(cx.listener(|this, _, window, cx| {
                                        this.attach_selected();
                                        if let Some(focus) = &this.input_focus { focus.focus(window); }
                                        cx.notify();
                                    })))
                                    .child(div().id("cancel-attach").debug_selector(|| "cancel-attach".into()).py_2().cursor_pointer().child("Cancel").on_click(cx.listener(|this, _, _, cx| {
                                        this.cancel_attach();
                                        cx.notify();
                                    })))
                            })
                            .when(!confirming, |d| {
                                d.child(div().id("attach-terminal").debug_selector(|| "attach-terminal".into()).py_2().px_2().bg(rgb(0x34445a)).cursor_pointer().child("Attach interactive terminal").on_click(cx.listener(|this, _, window, cx| {
                                    this.request_attach();
                                    if let Some(focus) = &this.input_focus { focus.focus(window); }
                                    cx.notify();
                                })))
                            })
                            .child(div().id("terminal-status").debug_selector(|| "terminal-status".into()).child(self.terminal_status.clone()))
                    })
                    .when(self.terminal.is_some(), |d| {
                        let terminal = self.terminal.clone().expect("terminal checked above");
                        let frame = live_frame.as_ref().expect("terminal frame");
                        let (cell_w, line_h) = terminal_metrics(window);
                        let rows = terminal_rows(frame, cell_w, line_h, "terminal", self.terminal_selection.as_ref());
                        let entity = cx.entity();
                        let terminal_bounds = self.terminal_bounds.clone();
                        d.child(div().flex().items_center().gap_2().child(div().max_w(px(350.)).overflow_hidden().child(format!("Terminal · {}", label(&self.selection.as_ref().map(|s| s.name.as_str()).unwrap_or("")))))
                            .child(div().id("detach-terminal").debug_selector(|| "detach-terminal".into()).py_1().cursor_pointer().child("Detach").on_click(cx.listener(|this, _, _, cx| { this.terminal = None; this.terminal_status = "Terminal detached.".into(); cx.notify(); }))))
                        .child(div().id("terminal-input-hint").debug_selector(|| "terminal-input-hint".into()).text_sm().text_color(rgb(0x8a95a5)).child(if input_focus.is_focused(window) { "Drag to select · ⌘C copy · ⌘V paste (screen rows and spaces)" } else { "Click to type · drag to select · ⌘C copy" }))
                        .when(!self.terminal_status.starts_with("Opening terminal") && !self.terminal_status.is_empty(), |d| d.child(div().text_color(rgb(0xf28787)).child(self.terminal_status.clone())))
                        .child(div()
                            .on_children_prepainted(move |bounds, window, cx| {
                                let Some(bounds) = bounds.first() else { return };
                                terminal_bounds.set(Some(*bounds));
                                let (cell_w, line_h) = terminal_metrics(window);
                                let cols = (f32::from(bounds.size.width) / cell_w).floor().clamp(1., 500.) as u16;
                                let rows = (f32::from(bounds.size.height) / line_h).floor().clamp(1., 300.) as u16;
                                let (cols, rows) = terminal::clamp_grid(cols, rows);
                                if terminal.resize(rows, cols) {
                                    entity.update(cx, |_, cx| cx.notify());
                                }
                            })
                            .id("terminal-screen").debug_selector(|| "terminal-screen".into()).track_focus(&input_focus)
                            .flex_1().min_h(px(if short { 120. } else { 0. })).overflow_hidden()
                            .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                                if let (Some(terminal), Some(bounds)) = (&this.terminal, this.terminal_bounds.get()) {
                                    let frame = terminal.frame();
                                    let (w, h) = terminal_metrics(window);
                                    if let Some(pos) = terminal_cell_at(event.position, bounds, &frame, w, h) {
                                        this.terminal_selection = Some(terminal::Selection { frame, start: pos, end: pos, dragging: true, moved: false });
                                        this.terminal_drag_start = Some(event.position);
                                    }
                                }
                                if let Some(focus) = &this.input_focus { focus.focus(window); }
                                cx.notify();
                            }))
                            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, window, cx| {
                                if let (Some(selection), Some(bounds)) = (&mut this.terminal_selection, this.terminal_bounds.get()) {
                                    if selection.dragging && event.pressed_button == Some(gpui::MouseButton::Left) {
                                        let (w, h) = terminal_metrics(window);
                                        if let Some(pos) = terminal_cell_at(event.position, bounds, &selection.frame, w, h) {
                                            selection.end = pos;
                                            if let Some(start) = this.terminal_drag_start {
                                                let dx = f32::from(event.position.x - start.x);
                                                let dy = f32::from(event.position.y - start.y);
                                                selection.moved |= dx.abs() + dy.abs() >= 3.;
                                            }
                                            cx.notify();
                                        }
                                    }
                                }
                            }))
                            .on_click(cx.listener(|this, _, window, cx| { if let Some(focus) = &this.input_focus { focus.focus(window); } cx.notify(); }))
                            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                                let key = &event.keystroke;
                                if key.key == "c" && key.modifiers.platform && !key.modifiers.control && !key.modifiers.alt && !key.modifiers.shift && !key.modifiers.function {
                                    if let (Some(selection), Some(terminal)) = (&this.terminal_selection, &this.terminal) {
                                        if selection.valid_for(&terminal.frame()) {
                                            if let Some(text) = selection.text() { cx.write_to_clipboard(gpui::ClipboardItem::new_string(text)); }
                                        } else { this.terminal_selection = None; }
                                    }
                                    cx.stop_propagation(); cx.notify(); return;
                                }
                                this.terminal_selection = None;
                                if is_terminal_snapshot_shortcut(key) {
                                    this.open_terminal_snapshot(window, cx);
                                    cx.stop_propagation(); cx.notify(); return;
                                }
                                if key.key == "v" && key.modifiers.platform && !key.modifiers.control && !key.modifiers.alt && !key.modifiers.shift && !key.modifiers.function {
                                    if let (Some(terminal), Some(text)) = (&this.terminal, cx.read_from_clipboard().and_then(|item| item.text())) {
                                        match terminal.paste(&text) {
                                            Ok(()) => this.terminal_status.clear(),
                                            Err(error) => this.terminal_status = error.into(),
                                        }
                                    }
                                    cx.stop_propagation();
                                    cx.notify();
                                    return;
                                }
                                if let (Some(terminal), Some(bytes)) = (&this.terminal, terminal::key_bytes(&event.keystroke)) {
                                    if let Err(error) = terminal.send(bytes) { this.terminal_status = error.into(); }
                                }
                                cx.notify();
                            }))
                            .child(div().size_full().flex().flex_col().children(rows)))
                    })
                    .when(self.terminal.is_none() && self.selection.as_ref().is_some_and(|s| self.selection_current(s)), |d| {
                        d.child(div().id("input-target").debug_selector(|| "input-target".into()).pt_2().child(format!("Input: {}", compact_selection_label(self.selection.as_ref().unwrap()))))
                            .child(div().id("session-identity-toggle").debug_selector(|| "session-identity-toggle".into()).cursor_pointer().child(if self.show_session_identity { "Hide identity" } else { "Full identity" }).on_click(cx.listener(|this, _, _, cx| { this.show_session_identity = !this.show_session_identity; cx.notify(); })))
                            .when(self.show_session_identity, |d| d.child(div().id("session-identity-full").debug_selector(|| "session-identity-full".into()).child(selection_label(self.selection.as_ref().unwrap()))))
                            .child(div().child(if self.selected_peer.is_some() { "Peer line input · ASCII; Enter sends to this admitted peer and may append to its current prompt" } else { "Session line input · ASCII; Enter sends · attach above for interactive terminal" }))
                            .child(div()
                                .id("line-input")
                                .debug_selector(|| "line-input".into())
                                .track_focus(&input_focus)
                                .when(input_focus.is_focused(window), |d| d.bg(rgb(0x34445a)))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    if let Some(focus) = &this.input_focus { focus.focus(window); }
                                    cx.notify();
                                }))
                                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                                    let key = &event.keystroke;
                                    if is_terminal_snapshot_shortcut(key) {
                                        this.open_terminal_snapshot(window, cx);
                                        cx.stop_propagation(); cx.notify(); return;
                                    }
                                    if key.modifiers.control || key.modifiers.platform || key.modifiers.alt || key.modifiers.function { return; }
                                    match key.key.as_str() {
                                        "enter" => this.submit_local_line(),
                                        "backspace" => { this.input_text.pop(); },
                                        _ => if let Some(text) = &key.key_char {
                                            if text.bytes().all(|b| (b' '..=b'~').contains(&b)) && this.input_text.len() + text.len() < remuda_native::strict_input::MAX_BATCH {
                                                this.input_text.push_str(text);
                                            }
                                        }
                                    }
                                    cx.notify();
                                }))
                                .py_2()
                                .child(format!("> {}", self.input_text)))
                    })
                    .when_some(self.selection.as_ref().filter(|s| self.terminal.is_none() && self.selection_current(s)), |d, _| d.child(div().id("preview-target").debug_selector(|| "preview-target".into()).child("Read-only preview")))
                    .when_some(preview_panel, |d, panel| d.child(panel))
                    .child(
                        div()
                            .id("refresh")
                            .debug_selector(|| "refresh".into())
                            .pt_4()
                            .cursor_pointer()
                            .child("Refresh")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.refresh();
                                this.refresh_nodes();
                                cx.notify();
                            })),
                    )))
            .when(self.documents.is_some() && !self.document_collapsed && (!narrow || self.narrow_view == NarrowView::Documents), |d| {
                d.when(!narrow, |d| d.child(
                    div()
                        .id("document-divider")
                        .debug_selector(|| "document-divider".into())
                        .w(px(6.))
                        .h_full()
                        .bg(rgb(0x2a3140))
                        .cursor_col_resize()
                        .track_focus(&divider_focus)
                        .when(divider_focus.is_focused(window), |d| d.bg(rgb(0x46587a)))
                        .on_key_down(cx.listener({
                            let collapse_focus = collapse_focus.clone();
                            let pane_focus = documents_focus.clone();
                            move |this, event: &gpui::KeyDownEvent, window, cx| {
                                let modifiers = &event.keystroke.modifiers;
                                if modifiers.control
                                    || modifiers.platform
                                    || modifiers.alt
                                    || modifiers.function
                                    || modifiers.shift
                                {
                                    return;
                                }
                                let width: f32 = window.viewport_size().width.into();
                                if width <= 0.0 {
                                    return;
                                }
                                let step = DOCUMENT_KEY_STEP_PX / width;
                                match event.keystroke.key.as_str() {
                                    "left" => {
                                        this.document_panel_fraction =
                                            (this.document_panel_fraction + step)
                                                .min(MAX_DOCUMENT_FRACTION);
                                    }
                                    "right" => {
                                        this.document_panel_fraction =
                                            (this.document_panel_fraction - step)
                                                .max(MIN_DOCUMENT_FRACTION);
                                    }
                                    "home" => this.document_panel_fraction = MIN_DOCUMENT_FRACTION,
                                    "end" => this.document_panel_fraction = MAX_DOCUMENT_FRACTION,
                                    "c" => {
                                        this.document_collapsed = true;
                                        collapse_focus.focus(window);
                                    }
                                    "d" | "escape" => pane_focus.focus(window),
                                    "q" => this.toggle_documents(window),
                                    _ => return,
                                }
                                cx.notify();
                            }
                        }))
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener({
                                let divider_focus = divider_focus.clone();
                                move |this, _: &gpui::MouseDownEvent, window, cx| {
                                    this.document_dragging = true;
                                    divider_focus.focus(window);
                                    cx.stop_propagation();
                                    cx.notify();
                                }
                            }),
                        ),
                ))
                .child(document_workspace_view::render(
                    self,
                    &documents_focus,
                    &divider_focus,
                    if narrow { viewport_width - 84. } else { document_width },
                    viewport,
                    narrow,
                    documents_focus.is_focused(window),
                    cx,
                ))
            })
            .when(self.documents.is_some() && self.document_collapsed && !narrow, |d| {
                d.child(
                    div()
                        .id("document-rail")
                        .debug_selector(|| "document-rail".into())
                        .w(px(40.))
                        .h_full()
                        .flex()
                        .flex_col()
                        .items_center()
                        .bg(rgb(0x151920))
                        .p_2()
                        .child(
                            div()
                                .id("document-restore")
                                .debug_selector(|| "document-restore".into())
                                .cursor_pointer()
                                .child("Docs")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.document_collapsed = false;
                                    if let Some(focus) = &this.documents_focus {
                                        focus.focus(window);
                                    }
                                    cx.notify();
                                })),
                        ),
                )
            })
            .when_some(self.selection_error, |root, error| root.child(
                div().id("selection-error").debug_selector(|| "selection-error".into())
                    .absolute().top_0().right_0().text_sm().bg(rgb(0x301b20)).text_color(rgb(0xf28787)).child(error)
            ))
            .when_some(snapshot, |root, snapshot| root.child(
                div().id("selection-snapshot").debug_selector(|| "selection-snapshot".into())
                    .absolute().inset_0().occlude().bg(rgb(0x151920)).p_4().flex().flex_col().gap_2()
                    .child(div().text_sm().child(format!("{} · frozen", snapshot.label)))
                    .child(div().flex().flex_wrap().gap_2()
                        .child(div().id("selection-close").debug_selector(|| "selection-close".into()).cursor_pointer().child("Return (Esc)").on_click(cx.listener(|this, _, window, cx| { this.close_selection_snapshot(window); cx.notify(); })))
                        .when(snapshot.identity.is_some(), |d| d.child(div().id("selection-identity").debug_selector(|| "selection-identity".into()).cursor_pointer().child("Identity (F2)").on_click(cx.listener(|this, _, _, cx| { if let Some(snapshot) = &mut this.selection_snapshot { snapshot.show_identity = !snapshot.show_identity; } cx.notify(); }))))
                        .child("Read-only · select + copy"))
                    .when(snapshot.show_identity, |d| d.child(div().text_sm().child(snapshot.identity.clone().unwrap_or_default())))
                    .child(div().id("selection-input").debug_selector(|| "selection-input".into()).flex_1().min_h(px(0.)).child(Input::new(&snapshot.input).disabled(true).h_full()))
                    .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                        if event.keystroke.key == "escape" && event.keystroke.modifiers == gpui::Modifiers::none() {
                            this.close_selection_snapshot(window); cx.stop_propagation(); cx.notify();
                        } else if event.keystroke.key == "f2" && event.keystroke.modifiers == gpui::Modifiers::none() {
                            if let Some(snapshot) = &mut this.selection_snapshot { if snapshot.identity.is_some() { snapshot.show_identity = !snapshot.show_identity; } }
                            cx.stop_propagation(); cx.notify();
                        }
                    }))
            ))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                if this.selection_snapshot.is_some() { return; }
                let m = &event.keystroke.modifiers;
                if f32::from(window.viewport_size().width) < 700. && m.platform && !m.control && !m.alt && !m.shift && !m.function {
                    let view = match event.keystroke.key.as_str() { "1" => Some(NarrowView::Nodes), "2" => Some(NarrowView::Session), "3" => Some(NarrowView::Documents), _ => None };
                    if let Some(view) = view { this.show_narrow_view(view, window); cx.stop_propagation(); cx.notify(); return; }
                }
                if event.keystroke.key != "tab" || m.control || m.platform || m.alt || m.function {
                    return;
                }
                let focused = window.focused(cx);
                let owned = focused.as_ref().is_some_and(|handle| {
                    this.toggle_focus.as_ref() == Some(handle)
                        || this.divider_focus.as_ref() == Some(handle)
                        || this.collapse_focus.as_ref() == Some(handle)
                });
                if focused.is_none() || focused.as_ref() == this.tree_focus.as_ref() {
                    if m.shift && this.session_input_visible() {
                        if let Some(focus) = &this.input_focus { focus.focus(window); }
                    } else if let Some(focus) = &this.toggle_focus {
                        focus.focus(window);
                    }
                } else if this.terminal.is_none() && focused.as_ref() == this.input_focus.as_ref() {
                    if m.shift {
                        if let Some(focus) = &this.toggle_focus { focus.focus(window); }
                    } else if let Some(focus) = &this.tree_focus { focus.focus(window); }
                } else if owned {
                    if m.shift {
                        if let Some(focus) = &this.tree_focus { focus.focus(window); }
                    } else if this.session_input_visible() {
                        if let Some(focus) = &this.input_focus { focus.focus(window); }
                    } else if let Some(focus) = &this.tree_focus { focus.focus(window); }
                } else {
                    return;
                }
                cx.stop_propagation();
                cx.notify();
            }))
            .on_mouse_move(cx.listener(
                |this, event: &gpui::MouseMoveEvent, window, cx| {
                    if !this.document_dragging
                        || this.documents.is_none()
                        || this.document_collapsed
                    {
                        return;
                    }
                    let width: f32 = window.viewport_size().width.into();
                    let x: f32 = event.position.x.into();
                    if width > 0.0 {
                        this.document_panel_fraction = ((width - x) / width)
                            .clamp(MIN_DOCUMENT_FRACTION, MAX_DOCUMENT_FRACTION);
                        cx.notify();
                    }
                },
            ))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if let Some(selection) = &mut this.terminal_selection { selection.dragging = false; }
                    this.terminal_drag_start = None;
                    if let Some(selection) = &mut this.preview_selection { selection.grid.dragging = false; }
                    this.preview_drag_start = None;
                    if this.document_dragging {
                        this.document_dragging = false;
                        cx.notify();
                    }
                }),
            )
    }
}
fn main() {
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
        if args.len() == 7
            && args[0] == "local"
            && args[1].is_empty()
            && !args[2].is_empty()
            && !args[3].is_empty()
            && args[4] == "--terminal"
            && args[5] == "--attach-v1"
            && args[6] == WORKER_ABI
        {
            std::process::exit(terminal_worker(&args[2], &args[3]));
        }
        if args.len() == 7
            && (args.last().map(String::as_str) == Some(WORKER_ABI)
                || args.last().map(String::as_str) == Some(WORKER_ABI_2))
            && args[4] == "--sync"
        {
            let abi2 = args.last().map(String::as_str) == Some(WORKER_ABI_2);
            args.pop();
            std::process::exit(preview::sync_worker(&args, abi2));
        }
        let abi = if args.len() == 5 {
            match args.last().map(String::as_str) {
                Some(WORKER_ABI) => Some(1),
                Some(WORKER_ABI_2) => Some(2),
                _ => None,
            }
        } else {
            None
        };
        if abi.is_some() {
            args.pop();
        }
        std::process::exit(preview::worker(&args, abi));
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
    Application::new().run(|cx: &mut App| {
        gpui_component::init(cx);
        let bounds = Bounds::centered(None, size(px(1000.), px(650.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| {
                    let mut gui = LocalGui::new();
                    gui.refresh();
                    gui.refresh_nodes();
                    cx.spawn(async move |weak, cx| loop {
                        gpui::Timer::after(Duration::from_millis(150)).await;
                        if weak
                            .update(cx, |this: &mut LocalGui, cx| {
                                let changed = this.drain();
                                // An attached terminal repaints from its live
                                // parser; a static preview repaints only on a
                                // real frame change.
                                if changed || this.terminal.is_some() {
                                    cx.notify();
                                }
                            })
                            .is_err()
                        {
                            break;
                        }
                    })
                    .detach();
                    gui
                });
                cx.new(|cx| gpui_component::Root::new(view, window, cx))
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
    fn terminal_snapshot_rejects_oversized_cells_without_truncating() {
        let terminal = terminal::Terminal::fixture(1, 2, b"x");
        let mut frame = terminal.frame();
        frame.cells[0][0].text = "x".repeat(MAX_SELECTION_SNAPSHOT + 1);
        assert!(terminal_snapshot_text(frame).is_err());
    }

    #[gpui::test]
    fn selection_snapshot_keeps_exact_text_blocks_edits_and_restores_focus(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let slot = captured.clone();
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|_| LocalGui::new());
            *slot.borrow_mut() = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let view = captured.borrow().clone().unwrap();
        let original = "  한글\n    code  \n";
        cx.update(|window, app| view.update(app, |gui, cx| gui.open_selection_snapshot(original, true, "Document source: one.md".into(), Some("Version 1 · SHA-256 abcdef".into()), window, cx)));
        cx.run_until_parked();
        cx.simulate_keystrokes("f2");
        assert!(cx.read(|app| view.read(app).selection_snapshot.as_ref().unwrap().show_identity));
        cx.simulate_keystrokes("f2");
        assert!(!cx.read(|app| view.read(app).selection_snapshot.as_ref().unwrap().show_identity));
        cx.update(|_, app| app.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into())));
        assert_eq!(cx.read(|app| app.read_from_clipboard().unwrap().text()).as_deref(), Some("sentinel"));
        cx.simulate_keystrokes("cmd-a cmd-c");
        assert_eq!(cx.read(|app| app.read_from_clipboard().unwrap().text()).as_deref(), Some(original));
        cx.simulate_keystrokes("cmd-up shift-right shift-right cmd-c");
        assert_eq!(cx.read(|app| app.read_from_clipboard().unwrap().text()).as_deref(), Some("  "));
        let input_bounds = cx.debug_bounds("selection-input").unwrap();
        let start = gpui::point(input_bounds.left() + px(15.), input_bounds.top() + px(10.));
        let end = gpui::point(input_bounds.left() + px(65.), input_bounds.top() + px(10.));
        cx.simulate_mouse_down(start, gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_move(end, Some(gpui::MouseButton::Left), gpui::Modifiers::none());
        cx.simulate_mouse_up(end, gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_keystrokes("cmd-c");
        let mouse = cx.read(|app| app.read_from_clipboard().unwrap().text()).unwrap();
        assert!(!mouse.is_empty() && mouse != original && original.contains(&mouse));
        for size in [gpui::size(px(1000.), px(650.)), gpui::size(px(420.), px(320.))] {
            cx.simulate_resize(size);
            cx.run_until_parked();
            let bounds = cx.debug_bounds("selection-input").unwrap();
            assert!(bounds.size.height >= px(180.), "snapshot must provide multi-line height: {bounds:?}");
            let start = gpui::point(bounds.left() + px(6.), bounds.top() + px(9.));
            let end = gpui::point(bounds.left() + px(100.), bounds.top() + px(52.));
            cx.simulate_mouse_down(start, gpui::MouseButton::Left, gpui::Modifiers::none());
            cx.simulate_mouse_move(end, Some(gpui::MouseButton::Left), gpui::Modifiers::none());
            cx.simulate_mouse_up(end, gpui::MouseButton::Left, gpui::Modifiers::none());
            cx.simulate_keystrokes("cmd-c");
            let copied = cx.read(|app| app.read_from_clipboard().unwrap().text()).unwrap();
            assert!(copied.contains("한글") && copied.contains("code"), "drag must select across visible lines: {copied:?}");
        }
        cx.update(|_, app| app.write_to_clipboard(gpui::ClipboardItem::new_string("replacement".into())));
        cx.simulate_keystrokes("cmd-v cmd-x x backspace");
        assert_eq!(cx.read(|app| view.read(app).selection_snapshot.as_ref().unwrap().input.read(app).value()).as_ref(), original);
        cx.simulate_keystrokes("escape");
        assert!(cx.read(|app| view.read(app).selection_snapshot.is_none()));
        assert!(cx.update(|window, app| view.read(app).tree_focus.as_ref().unwrap().is_focused(window)));
        let huge = "x".repeat(1024 * 1024 + 1);
        cx.update(|window, app| view.update(app, |gui, cx| { gui.open_selection_snapshot(&huge, true, "Document source: huge.md".into(), None, window, cx); cx.notify(); }));
        assert!(cx.read(|app| view.read(app).selection_snapshot.is_none()));
        assert!(cx.read(|app| view.read(app).selection_error.is_some_and(|e| e.contains("exceeds"))));
        let error = cx.debug_bounds("selection-error").expect("document overflow must be visible");
        assert!(error.left() >= px(0.) && error.right() <= px(420.) && error.bottom() <= px(320.));
        cx.update(|window, app| view.update(app, |gui, cx| { gui.selection_error = None; gui.open_selection_snapshot(&huge, false, "Terminal".into(), None, window, cx); cx.notify(); }));
        assert!(cx.read(|app| view.read(app).selection_snapshot.is_none()));
        let error = cx.debug_bounds("selection-error").expect("terminal overflow must be visible");
        assert!(error.left() >= px(0.) && error.right() <= px(420.) && error.bottom() <= px(320.));
    }

    #[test]
    fn preview_grid_conversion_measurement() {
        let cell = remuda_core::agent::StyledCell { text: "A".into(), ..Default::default() };
        let grid = vec![vec![cell; 100]; 200];
        let started = std::time::Instant::now();
        for _ in 0..100 { std::hint::black_box(terminal::frame_from_styled(std::hint::black_box(grid.clone()))); }
        println!("100 conversions of 20k cells: {:?}", started.elapsed());
        let mut gui = LocalGui::new();
        gui.preview_busy = true;
        gui.sessions = vec![row("one")];
        gui.select_session(row("one"));
        gui.preview_cells = Some(grid);
        let frame = gui.preview_frame().unwrap();
        gui.preview_selection = Some(PreviewSelection { grid: terminal::Selection { frame, start: (0, 0), end: (0, 1), dragging: true, moved: true }, identity: gui.selection.clone().unwrap(), version: gui.sync_version, revision: gui.preview_revision });
        let started = std::time::Instant::now();
        for _ in 0..100 { assert!(std::hint::black_box(&gui).preview_selection_current()); }
        println!("100 cached selection event validity checks: {:?}", started.elapsed());
    }

    #[test]
    fn preview_runs_keep_colors_widths_and_selection_boundaries() {
        let mut frame = terminal::Terminal::fixture(1, 8, "AB한é".as_bytes()).frame();
        frame.cells[0][1].fg = terminal::AnsiColor::Indexed(1);
        let selection = terminal::Selection { frame: frame.clone(), start: (0, 1), end: (0, 2), dragging: false, moved: true };
        let runs = preview_runs(&frame.cells[0], 0, Some(&selection));
        assert_eq!(runs[0].2, "A");
        assert_eq!((runs[1].2.as_str(), runs[1].1, runs[1].0.fg), ("B", true, terminal::AnsiColor::Indexed(1)));
        assert_eq!((runs[2].2.as_str(), runs[2].1, runs[2].3), ("한", true, 2));
        assert_eq!((runs[3].2.as_str(), runs[3].3), ("é", 1));
    }
    #[test]
    fn preview_drag_run_limit_allows_boundary_and_blocks_next() {
        let mut frame = terminal::Terminal::fixture(1, 257, b"").frame();
        for (i, cell) in frame.cells[0].iter_mut().enumerate() { cell.fg = terminal::AnsiColor::Indexed(if i % 2 == 0 { 1 } else { 2 }); }
        frame.cells[0].truncate(MAX_PREVIEW_DRAG_RUNS);
        frame.cols = MAX_PREVIEW_DRAG_RUNS as u16;
        assert!(preview_drag_available(&frame));
        let mut extra = frame.cells[0].last().unwrap().clone(); extra.fg = terminal::AnsiColor::Indexed(1);
        frame.cells[0].push(extra); frame.cols += 1;
        assert!(!preview_drag_available(&frame));
    }
    #[gpui::test]
    fn preview_run_gate_does_not_oscillate_at_viewport_boundary(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new(); gui.preview_busy = true; gui.sessions = vec![row("one")]; gui.select_session(row("one"));
            gui.preview = Some("A".into()); gui.preview_cells = Some(vec![vec![remuda_core::agent::StyledCell { text: "A".into(), ..Default::default() }; 100]; 200]); gui
        });
        cx.simulate_resize(gpui::size(px(1000.), px(650.))); cx.run_until_parked();
        let visible = cx.read(|app| view.read(app).preview_visible_rows(200));
        let mut heights = Vec::new();
        for wanted in [256usize, 257, 256] {
            view.update(cx, |gui, cx| {
                gui.invalidate_preview_selection();
                gui.preview_cells = Some((0..200).map(|row| {
                    let runs = wanted / visible + usize::from(row < wanted % visible);
                    (0..100).map(|i| remuda_core::agent::StyledCell { text: "A".into(), fg: remuda_core::agent::Color::Idx(1 + (i.min(runs - 1) % 2) as u8), ..Default::default() }).collect()
                }).collect());
                cx.notify();
            });
            cx.run_until_parked();
            let (count, allowed, height) = cx.read(|app| {
                let gui = view.read(app); let frame = gui.preview_frame().unwrap();
                (frame.cells.iter().enumerate().map(|(i, row)| preview_runs(row, i, None).len()).sum::<usize>(), preview_drag_available(&frame), gui.preview_bounds.get().unwrap().size.height)
            });
            assert_eq!(count, wanted); assert_eq!(allowed, wanted <= MAX_PREVIEW_DRAG_RUNS);
            heights.push(height);
        }
        assert_eq!(heights[0], heights[1]); assert_eq!(heights[1], heights[2]);
    }

    #[gpui::test]
    fn preview_ascii_runs_preserve_each_cell_position(cx: &mut gpui::TestAppContext) {
        let (_, cx) = cx.add_window_view(|_, _| LocalGui::new());
        cx.update(|window, _| {
            let text = "fi AV.,!?->=()[]";
            let (cell_w, _) = terminal_metrics(window);
            for (weight, style) in [(gpui::FontWeight::NORMAL, gpui::FontStyle::Normal), (gpui::FontWeight::BOLD, gpui::FontStyle::Normal), (gpui::FontWeight::NORMAL, gpui::FontStyle::Italic), (gpui::FontWeight::BOLD, gpui::FontStyle::Italic)] {
                let mut text_style = window.text_style();
                text_style.font_family = "Menlo".into(); text_style.font_size = px(13.).into(); text_style.font_weight = weight; text_style.font_style = style;
                let run = text_style.to_run(text.len());
                let layout = window.text_system().layout_line(text, px(13.), &[run], None);
                for index in 0..text.len() {
                    let actual = f32::from(layout.x_for_index(index));
                    assert!((actual - index as f32 * cell_w).abs() < 0.1, "index {index}: {actual} vs {}", index as f32 * cell_w);
                }
            }
        });
    }

    #[gpui::test]
    fn preview_idle_resize_updates_rows_and_shows_truncation(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new(); gui.preview_busy = true; gui.sessions = vec![row("one")]; gui.select_session(row("one"));
            gui.preview = Some("A".into());
            gui.preview_cells = Some(vec![vec![remuda_core::agent::StyledCell { text: "A".into(), ..Default::default() }; 20]; 100]);
            gui
        });
        let mut counts = Vec::new();
        for height in [500., 800., 500.] {
            cx.simulate_resize(gpui::size(px(1000.), px(height))); cx.run_until_parked();
            let (rows, expected) = cx.read(|app| { let gui = view.read(app); (gui.preview_frame().unwrap().rows as usize, gui.preview_visible_rows(100)) });
            assert_eq!(rows, expected);
            let cue = cx.debug_bounds("preview-truncated").expect("truncation cue");
            let preview = cx.debug_bounds("preview").unwrap();
            assert!(cue.top() >= preview.top() && cue.bottom() <= preview.bottom(), "truncation cue must remain visible: cue={cue:?} preview={preview:?}");
            counts.push(rows);
        }
        assert!(counts[1] > counts[0] && counts[2] == counts[0], "idle resizing did not track viewport: {counts:?}");
        cx.simulate_resize(gpui::size(px(420.), px(320.))); cx.run_until_parked();
        let column = cx.debug_bounds("session-column").unwrap();
        cx.simulate_event(gpui::ScrollWheelEvent { position: column.center(), delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-1000.))), ..Default::default() });
        cx.run_until_parked();
        let grid = cx.debug_bounds("preview-grid").unwrap();
        let cue = cx.debug_bounds("preview-truncated").unwrap();
        assert!(grid.size.height >= px(48.) && grid.top() < column.bottom() && grid.bottom() > column.top(), "short viewport must scroll to preview: {grid:?}");
        assert!(cue.top() >= column.top() && cue.bottom() <= column.bottom(), "short viewport truncation cue must be visible after scroll: {cue:?}");
    }

    #[gpui::test]
    fn complex_preview_uses_snapshot_instead_of_slow_drag(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new(); gui.preview_busy = true; gui.sessions = vec![row("one")]; gui.select_session(row("one"));
            gui.preview = Some("safe snapshot".into());
            gui.preview_cells = Some(vec![(0..100).map(|i| remuda_core::agent::StyledCell { text: "A".into(), fg: if i % 2 == 0 { remuda_core::agent::Color::Idx(1) } else { remuda_core::agent::Color::Idx(2) }, ..Default::default() }).collect(); 20]);
            gui
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("preview-drag-unavailable").is_some());
        assert!(cx.debug_bounds("select-terminal-text").is_some());
        let bounds = cx.debug_bounds("preview-grid").unwrap();
        cx.simulate_mouse_down(bounds.center(), gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_move(gpui::point(bounds.center().x + px(20.), bounds.center().y), Some(gpui::MouseButton::Left), gpui::Modifiers::none());
        cx.simulate_mouse_up(gpui::point(bounds.center().x + px(20.), bounds.center().y), gpui::MouseButton::Left, gpui::Modifiers::none());
        assert!(cx.read(|app| { let gui = view.read(app); gui.preview_selection.is_none() && gui.terminal.is_none() && gui.input_helper.is_none() }));
        cx.simulate_keystrokes("x->x");
        assert_eq!(cx.read(|app| view.read(app).input_text.clone()), "", "a blocked drag must not focus input");
        view.update(cx, |gui, cx| {
            gui.invalidate_preview_selection();
            gui.preview_cells = Some(vec![vec![remuda_core::agent::StyledCell { text: "A".into(), ..Default::default() }; 10]]);
            cx.notify();
        });
        cx.run_until_parked();
        let bounds = cx.debug_bounds("preview-grid").unwrap();
        let p = gpui::point(bounds.left() + px(3.), bounds.top() + px(8.));
        cx.simulate_mouse_down(p, gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_move(gpui::point(p.x + px(12.), p.y), Some(gpui::MouseButton::Left), gpui::Modifiers::none());
        cx.simulate_mouse_up(gpui::point(p.x + px(12.), p.y), gpui::MouseButton::Left, gpui::Modifiers::none());
        assert!(cx.read(|app| view.read(app).preview_selection.as_ref().is_some_and(|s| s.grid.moved)));
        view.update(cx, |gui, cx| {
            gui.invalidate_preview_selection();
            gui.preview_cells = Some(vec![(0..100).map(|i| remuda_core::agent::StyledCell { text: "A".into(), fg: if i % 2 == 0 { remuda_core::agent::Color::Idx(1) } else { remuda_core::agent::Color::Idx(2) }, ..Default::default() }).collect(); 20]);
            cx.notify();
        });
        assert!(cx.read(|app| view.read(app).preview_selection.is_none()));
    }

    #[gpui::test]
    fn complex_preview_snapshot_copies_bounded_grid_and_checks_identity(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None)); let slot = captured.clone();
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|_| {
                let mut gui = LocalGui::new(); gui.preview_busy = true; gui.sessions = vec![row("one")]; gui.select_session(row("one")); gui.sync_version = 7;
                gui.preview = Some("different text must not be copied".into());
                gui.preview_cells = Some(vec![(0..100).map(|i| remuda_core::agent::StyledCell { text: "A".into(), fg: if i % 2 == 0 { remuda_core::agent::Color::Idx(1) } else { remuda_core::agent::Color::Idx(2) }, ..Default::default() }).collect(); 3]);
                gui
            });
            *slot.borrow_mut() = Some(view.clone()); gpui_component::Root::new(view, window, cx)
        });
        let view = captured.borrow().clone().unwrap();
        assert!(cx.debug_bounds("preview-drag-unavailable").is_some());
        let button = cx.debug_bounds("select-terminal-text").unwrap(); cx.simulate_click(button.center(), gpui::Modifiers::none());
        assert!(cx.read(|app| view.read(app).selection_snapshot.as_ref().unwrap().label.contains("v7")));
        cx.simulate_keystrokes("cmd-a cmd-c");
        assert_eq!(cx.read(|app| app.read_from_clipboard().and_then(|item| item.text())), Some(format!("{}\n", "A".repeat(100)).repeat(3)));
        assert!(cx.read(|app| { let gui = view.read(app); gui.terminal.is_none() && gui.input_helper.is_none() }));
        cx.simulate_keystrokes("escape");
        view.update(cx, |gui, cx| { gui.sessions[0].instance_id = Some("replacement".into()); cx.notify(); });
        cx.update(|window, app| view.update(app, |gui, cx| gui.open_terminal_snapshot(window, cx)));
        assert!(cx.read(|app| { let gui = view.read(app); gui.selection_snapshot.is_none() && gui.selection_error.is_some() && gui.terminal.is_none() && gui.input_helper.is_none() }));
    }

    #[gpui::test]
    #[ignore = "manual maximum-grid interaction measurement"]
    fn preview_max_grid_drag_measurement(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new(); gui.preview_busy = true; gui.sessions = vec![row("one")]; gui.select_session(row("one"));
            gui.preview = Some("A".into());
            gui.preview_cells = Some(vec![vec![remuda_core::agent::StyledCell { text: "A".into(), ..Default::default() }; 100]; 200]);
            gui
        });
        let bounds = cx.debug_bounds("preview-grid").unwrap();
        println!("preview grid viewport: {:?}", bounds.size);
        let w = cx.update(|window, _| terminal_metrics(window).0);
        let point = |c: f32| gpui::point(bounds.left() + px(w * c), bounds.top() + px(8.));
        cx.simulate_mouse_down(point(1.1), gpui::MouseButton::Left, gpui::Modifiers::none());
        let started = std::time::Instant::now();
        for col in [2.1, 3.1, 4.1] { cx.simulate_mouse_move(point(col), Some(gpui::MouseButton::Left), gpui::Modifiers::none()); }
        println!("3 maximum-grid drag events and GUI repaints: {:?}", started.elapsed());
        cx.simulate_mouse_up(point(4.1), gpui::MouseButton::Left, gpui::Modifiers::none());
        view.update(cx, |gui, cx| {
            gui.invalidate_preview_selection();
            let visible = gui.preview_visible_rows(200);
            gui.preview_cells = Some((0..200).map(|row| {
                let runs = MAX_PREVIEW_DRAG_RUNS / visible + usize::from(row < MAX_PREVIEW_DRAG_RUNS % visible);
                (0..100).map(|i| remuda_core::agent::StyledCell { text: "A".into(), fg: remuda_core::agent::Color::Idx(1 + (i.min(runs - 1) % 2) as u8), ..Default::default() }).collect()
            }).collect());
            cx.notify();
        });
        cx.run_until_parked();
        let count = cx.read(|app| { let frame = view.read(app).preview_frame().unwrap(); frame.cells.iter().enumerate().map(|(i, row)| preview_runs(row, i, None).len()).sum::<usize>() });
        assert_eq!(count, MAX_PREVIEW_DRAG_RUNS);
        cx.simulate_mouse_down(point(1.1), gpui::MouseButton::Left, gpui::Modifiers::none());
        let started = std::time::Instant::now();
        for col in [2.1, 3.1, 4.1] { cx.simulate_mouse_move(point(col), Some(gpui::MouseButton::Left), gpui::Modifiers::none()); }
        println!("3 at-limit drag events and repaints ({count} runs): {:?}", started.elapsed());
        cx.simulate_mouse_up(point(4.1), gpui::MouseButton::Left, gpui::Modifiers::none());
    }

    #[gpui::test]
    fn preview_click_allows_typing_in_selected_peer_line_input(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new(); gui.preview_busy = true;
            let session = row("one");
            gui.peers = vec![peer("peer-fingerprint", "Peer")];
            gui.selected_peer = Some("peer-fingerprint".into());
            gui.peer_sessions.insert("peer-fingerprint".into(), vec![session.clone()]);
            gui.remote_sessions = vec![session.clone()]; gui.select_session(session);
            gui.preview = Some("HELLO".into());
            gui.preview_cells = Some(vec![vec![remuda_core::agent::StyledCell { text: "A".into(), ..Default::default() }; 10]]);
            gui
        });
        let preview = cx.debug_bounds("preview-grid").unwrap();
        cx.simulate_click(preview.center(), gpui::Modifiers::none());
        cx.simulate_keystrokes("p->p");
        assert_eq!(cx.read(|app| view.read(app).input_text.clone()), "p");
        assert!(cx.read(|app| { let gui = view.read(app); gui.terminal.is_none() && gui.input_helper.is_none() }));
        view.update(cx, |gui, cx| { gui.peer_sessions.get_mut("peer-fingerprint").unwrap()[0].instance_id = Some("replacement".into()); cx.notify(); });
        let preview = cx.debug_bounds("preview-grid").unwrap();
        cx.simulate_click(preview.center(), gpui::Modifiers::none());
        cx.simulate_keystrokes("x->x enter");
        assert!(cx.read(|app| { let gui = view.read(app); gui.input_text == "p" && gui.terminal.is_none() && gui.input_helper.is_none() }), "stale peer must not receive focus or input");
    }

    #[gpui::test]
    fn text_only_preview_click_focuses_line_input(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new(); gui.preview_busy = true; gui.sessions = vec![row("one")]; gui.select_session(row("one")); gui.preview = Some("legacy text".into()); gui
        });
        cx.update(|window, app| view.read(app).tree_focus.as_ref().unwrap().focus(window));
        let preview = cx.debug_bounds("preview-text").unwrap(); cx.simulate_click(preview.center(), gpui::Modifiers::none());
        cx.simulate_keystrokes("p->p");
        assert!(cx.read(|app| { let gui = view.read(app); gui.input_text == "p" && gui.terminal.is_none() && gui.input_helper.is_none() }));
    }

    #[test]
    fn preview_click_requires_the_same_press_and_release_selection() {
        let one = Selection { peer: None, name: "one".into(), instance: "i1".into() };
        let two = Selection { peer: None, name: "two".into(), instance: "i2".into() };
        assert!(preview_click_focus_allowed(Some(&one), Some(&one), false, true));
        assert!(!preview_click_focus_allowed(Some(&one), Some(&two), false, true), "release on a new target must not focus");
        assert!(!preview_click_focus_allowed(Some(&one), Some(&one), false, false), "a stale selection must not focus");
        assert!(!preview_click_focus_allowed(Some(&one), Some(&one), true, true), "a drag must not focus");
        assert!(!preview_click_focus_allowed(None, Some(&one), false, true), "a release without a press must not focus");
    }

    #[gpui::test]
    fn preview_release_after_replacement_does_not_focus_the_new_target(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new(); gui.preview_busy = true;
            let session = row("one");
            gui.peers = vec![peer("peer-fingerprint", "Peer")];
            gui.selected_peer = Some("peer-fingerprint".into());
            gui.peer_sessions.insert("peer-fingerprint".into(), vec![session.clone()]);
            gui.remote_sessions = vec![session.clone()];
            gui.select_session(session);
            gui.preview = Some("HELLO".into());
            gui.preview_cells = Some(vec![vec![remuda_core::agent::StyledCell { text: "A".into(), ..Default::default() }; 10]]);
            gui
        });
        let preview = cx.debug_bounds("preview-grid").unwrap();
        cx.simulate_mouse_down(preview.center(), gpui::MouseButton::Left, gpui::Modifiers::none());
        view.update(cx, |gui, cx| {
            gui.peer_sessions.get_mut("peer-fingerprint").unwrap()[0].instance_id = Some("replacement".into());
            cx.notify();
        });
        cx.simulate_mouse_up(preview.center(), gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_keystrokes("x->x");
        assert!(cx.read(|app| view.read(app).input_text.is_empty()), "a replaced peer must not receive the release focus");
    }

    #[gpui::test]
    fn preview_release_after_peer_revocation_does_not_focus_input(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new(); gui.preview_busy = true;
            let session = row("one");
            gui.peers = vec![peer("peer-fingerprint", "Peer")];
            gui.selected_peer = Some("peer-fingerprint".into());
            gui.peer_sessions.insert("peer-fingerprint".into(), vec![session.clone()]);
            gui.remote_sessions = vec![session.clone()];
            gui.select_session(session);
            gui.preview = Some("HELLO".into());
            gui.preview_cells = Some(vec![vec![remuda_core::agent::StyledCell { text: "A".into(), ..Default::default() }; 10]]);
            gui
        });
        let preview = cx.debug_bounds("preview-grid").unwrap();
        cx.simulate_mouse_down(preview.center(), gpui::MouseButton::Left, gpui::Modifiers::none());
        view.update(cx, |gui, cx| { gui.peers[0].admitted = false; cx.notify(); });
        cx.simulate_mouse_up(preview.center(), gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_keystrokes("x->x");
        assert!(cx.read(|app| view.read(app).input_text.is_empty()), "a revoked peer must not receive the release focus");
    }

    #[gpui::test]
    fn text_preview_release_after_replacement_does_not_focus_the_new_target(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new(); gui.preview_busy = true; gui.sessions = vec![row("one")]; gui.select_session(row("one")); gui.preview = Some("legacy text".into()); gui
        });
        cx.update(|window, app| view.read(app).tree_focus.as_ref().unwrap().focus(window));
        let preview = cx.debug_bounds("preview-text").unwrap();
        cx.simulate_mouse_down(preview.center(), gpui::MouseButton::Left, gpui::Modifiers::none());
        view.update(cx, |gui, cx| { gui.sessions[0].instance_id = Some("replacement".into()); cx.notify(); });
        cx.simulate_mouse_up(preview.center(), gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_keystrokes("x->x");
        assert!(cx.read(|app| view.read(app).input_text.is_empty()), "a replaced local session must not receive the release focus");
    }

    #[gpui::test]
    fn text_preview_drag_does_not_focus_input(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new(); gui.preview_busy = true; gui.sessions = vec![row("one")]; gui.select_session(row("one")); gui.preview = Some("legacy text".into()); gui
        });
        cx.update(|window, app| view.read(app).tree_focus.as_ref().unwrap().focus(window));
        let preview = cx.debug_bounds("preview-text").unwrap();
        cx.simulate_mouse_down(preview.center(), gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_move(gpui::point(preview.center().x + px(20.), preview.center().y), Some(gpui::MouseButton::Left), gpui::Modifiers::none());
        cx.simulate_mouse_up(gpui::point(preview.center().x + px(20.), preview.center().y), gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_keystrokes("x->x");
        assert!(cx.read(|app| view.read(app).input_text.is_empty()), "a text-preview drag must not focus input");
    }

    #[gpui::test]
    fn text_preview_drag_takes_focus_from_line_input_and_does_not_type(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new(); gui.preview_busy = true; gui.sessions = vec![row("one")]; gui.select_session(row("one")); gui.preview = Some("legacy text".into()); gui
        });
        cx.update(|window, app| view.read(app).input_focus.as_ref().unwrap().focus(window));
        assert!(cx.update(|window, app| view.read(app).input_focus.as_ref().is_some_and(|f| f.is_focused(window))));
        let preview = cx.debug_bounds("preview-text").unwrap();
        cx.simulate_mouse_down(preview.center(), gpui::MouseButton::Left, gpui::Modifiers::none());
        assert!(cx.update(|window, app| view.read(app).preview_focus.as_ref().is_some_and(|f| f.is_focused(window))), "text preview mouse-down must focus its preview handle");
        cx.simulate_mouse_move(gpui::point(preview.center().x + px(20.), preview.center().y), Some(gpui::MouseButton::Left), gpui::Modifiers::none());
        cx.simulate_mouse_up(gpui::point(preview.center().x + px(20.), preview.center().y), gpui::MouseButton::Left, gpui::Modifiers::none());
        assert!(cx.update(|window, app| !view.read(app).input_focus.as_ref().is_some_and(|f| f.is_focused(window))), "a text drag must move focus off the input");
        cx.simulate_keystrokes("x->x");
        assert!(cx.read(|app| view.read(app).input_text.is_empty()), "a text drag must not type into the previously focused input");
    }

    #[gpui::test]
    fn read_only_preview_mouse_copy_never_sends_and_invalidates_on_change(cx: &mut gpui::TestAppContext) {
        for remote in [false, true] {
            let (view, cx) = cx.add_window_view(|_, _| {
                let mut gui = LocalGui::new();
                gui.preview_busy = true;
                let session = row("one");
                if remote {
                    gui.peers = vec![peer("peer-fingerprint", "Peer")];
                    gui.selected_peer = Some("peer-fingerprint".into());
                    gui.peer_sessions.insert("peer-fingerprint".into(), vec![session.clone()]);
                    gui.remote_sessions = vec![session.clone()];
                } else { gui.sessions = vec![session.clone()]; }
                gui.select_session(session);
                gui.input_text = "draft".into();
                gui.preview = Some("A한éZ".into());
                gui.preview_cells = Some(vec![vec![
                    remuda_core::agent::StyledCell { text: "A".into(), ..Default::default() },
                    remuda_core::agent::StyledCell { text: "한".into(), wide: true, ..Default::default() },
                    remuda_core::agent::StyledCell { text: "".into(), ..Default::default() },
                    remuda_core::agent::StyledCell { text: "é".into(), ..Default::default() },
                    remuda_core::agent::StyledCell { text: "Z".into(), ..Default::default() },
                ]]);
                gui
            });
            let identity = cx.debug_bounds("session-identity-toggle").unwrap();
            cx.simulate_click(identity.center(), gpui::Modifiers::none());
            assert!(cx.read(|app| view.read(app).show_session_identity));
            assert!(cx.debug_bounds("session-identity-full").is_some());
            assert!(cx.read(|app| { let gui = view.read(app); gui.terminal.is_none() && gui.input_helper.is_none() }));
            let identity = cx.debug_bounds("session-identity-toggle").unwrap();
            cx.simulate_click(identity.center(), gpui::Modifiers::none());
            let bounds = cx.debug_bounds("preview-grid").unwrap();
            let w = cx.update(|window, _| terminal_metrics(window).0);
            let point = |col: f32| gpui::point(bounds.left() + px(w * col), bounds.top() + px(8.));
            cx.simulate_mouse_down(point(2.2), gpui::MouseButton::Left, gpui::Modifiers::none());
            cx.simulate_mouse_move(point(3.2), Some(gpui::MouseButton::Left), gpui::Modifiers::none());
            cx.simulate_mouse_up(point(3.2), gpui::MouseButton::Left, gpui::Modifiers::none());
            assert!(cx.update(|window, app| view.read(app).preview_focus.as_ref().unwrap().is_focused(window)));
            cx.simulate_keystrokes("cmd-c");
            assert_eq!(cx.read(|app| app.read_from_clipboard().and_then(|item| item.text())).as_deref(), Some("한é"));
            cx.simulate_keystrokes("x enter");
            assert!(cx.read(|app| { let gui = view.read(app); gui.terminal.is_none() && gui.input_helper.is_none() && gui.input_text == "draft" }));
            cx.update(|_, app| app.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into())));
            view.update(cx, |gui, cx| { gui.sync_version += 1; cx.notify(); });
            cx.simulate_keystrokes("cmd-c");
            assert_eq!(cx.read(|app| app.read_from_clipboard().and_then(|item| item.text())).as_deref(), Some("sentinel"));
            cx.simulate_mouse_down(point(1.2), gpui::MouseButton::Left, gpui::Modifiers::none());
            cx.simulate_mouse_move(point(3.2), Some(gpui::MouseButton::Left), gpui::Modifiers::none());
            cx.simulate_mouse_up(point(3.2), gpui::MouseButton::Left, gpui::Modifiers::none());
            view.update(cx, |gui, cx| {
                let mut cells = gui.preview_cells.clone().unwrap();
                cells[0][0].text = "changed".into();
                let identity = gui.selection.clone().unwrap();
                gui.sync_sender.send((gui.preview_generation, identity.clone(), Ok(preview::SyncFrame {
                    text: "changed".into(), version: gui.sync_version, instance: identity.instance, cells,
                }))).unwrap();
                gui.drain();
                cx.notify();
            });
            cx.simulate_keystrokes("cmd-c");
            assert_eq!(cx.read(|app| app.read_from_clipboard().and_then(|item| item.text())).as_deref(), Some("sentinel"));
            cx.simulate_mouse_down(point(1.2), gpui::MouseButton::Left, gpui::Modifiers::none());
            cx.simulate_mouse_move(point(3.2), Some(gpui::MouseButton::Left), gpui::Modifiers::none());
            cx.simulate_mouse_up(point(3.2), gpui::MouseButton::Left, gpui::Modifiers::none());
            view.update(cx, |gui, cx| { gui.select_session(row("other")); cx.notify(); });
            assert!(cx.read(|app| view.read(app).preview_selection.is_none()));
        }
    }

    #[gpui::test]
    fn text_only_preview_snapshot_fallback_does_not_attach(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let slot = captured.clone();
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|_| {
                let mut gui = LocalGui::new();
                gui.preview_busy = true;
                gui.sessions = vec![row("one")];
                gui.select_session(row("one"));
                gui.preview = Some("legacy 한글\nline".into());
                gui
            });
            *slot.borrow_mut() = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let view = captured.borrow().clone().unwrap();
        let button = cx.debug_bounds("select-terminal-text").unwrap();
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        cx.simulate_keystrokes("cmd-a cmd-c");
        assert_eq!(cx.read(|app| app.read_from_clipboard().and_then(|item| item.text())).as_deref(), Some("legacy 한글\nline"));
        assert!(cx.read(|app| { let gui = view.read(app); gui.terminal.is_none() && gui.input_helper.is_none() }));
        cx.simulate_keystrokes("escape");
        view.update(cx, |gui, cx| { gui.preview = Some("x".repeat(MAX_SELECTION_SNAPSHOT + 1)); cx.notify(); });
        let button = cx.debug_bounds("select-terminal-text").unwrap();
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        assert!(cx.read(|app| { let gui = view.read(app); gui.selection_snapshot.is_none() && gui.selection_error.is_some() && gui.terminal.is_none() && gui.input_helper.is_none() }));
    }

    #[gpui::test]
    fn live_terminal_mouse_selection_copies_and_resize_invalidates(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let slot = captured.clone();
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|_| {
                let mut gui = LocalGui::new();
                gui.preview_busy = true;
                gui.sessions = vec![row("one")];
                gui.select_session(row("one"));
                gui.terminal = Some(std::rc::Rc::new(terminal::Terminal::fixture(24, 80, "A한e\u{301}Z".as_bytes())));
                gui
            });
            *slot.borrow_mut() = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let view = captured.borrow().clone().unwrap();
        cx.simulate_resize(gpui::size(px(1000.), px(650.)));
        cx.run_until_parked();
        let bounds = cx.debug_bounds("terminal-screen").unwrap();
        let cell_width = cx.update(|window, _| terminal_metrics(window).0);
        let p = |col: f32| gpui::point(bounds.left() + px(cell_width * col), bounds.top() + px(8.));
        cx.simulate_click(p(1.25), gpui::Modifiers::none());
        cx.update(|_, app| app.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into())));
        cx.simulate_keystrokes("cmd-c");
        assert_eq!(cx.read(|app| app.read_from_clipboard().and_then(|item| item.text())).as_deref(), Some("sentinel"));
        cx.simulate_mouse_down(p(0.10), gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_move(p(0.85), Some(gpui::MouseButton::Left), gpui::Modifiers::none());
        cx.simulate_mouse_up(p(0.85), gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_keystrokes("cmd-c");
        assert_eq!(cx.read(|app| app.read_from_clipboard().and_then(|item| item.text())).as_deref(), Some("A"));
        cx.simulate_mouse_down(p(1.25), gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_move(p(3.25), Some(gpui::MouseButton::Left), gpui::Modifiers::none());
        cx.simulate_mouse_up(p(3.25), gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_keystrokes("cmd-c");
        assert_eq!(cx.read(|app| app.read_from_clipboard().and_then(|item| item.text())).as_deref(), Some("한e\u{301}"));
        cx.simulate_mouse_down(p(3.25), gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_move(p(2.25), Some(gpui::MouseButton::Left), gpui::Modifiers::none());
        cx.simulate_mouse_up(p(2.25), gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_keystrokes("cmd-c");
        assert_eq!(cx.read(|app| app.read_from_clipboard().and_then(|item| item.text())).as_deref(), Some("한e\u{301}"));
        cx.simulate_mouse_down(p(1.25), gpui::MouseButton::Left, gpui::Modifiers::none());
        view.update(cx, |gui, cx| {
            gui.terminal = Some(std::rc::Rc::new(terminal::Terminal::fixture(24, 80, b"changed")));
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_mouse_move(p(3.25), Some(gpui::MouseButton::Left), gpui::Modifiers::none());
        cx.simulate_mouse_up(p(3.25), gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.update(|_, app| app.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into())));
        cx.simulate_keystrokes("cmd-c");
        assert_eq!(cx.read(|app| app.read_from_clipboard().and_then(|item| item.text())).as_deref(), Some("sentinel"));
        cx.simulate_mouse_down(p(1.25), gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_move(p(3.25), Some(gpui::MouseButton::Left), gpui::Modifiers::none());
        cx.simulate_mouse_up(p(3.25), gpui::MouseButton::Left, gpui::Modifiers::none());
        assert!(cx.read(|app| view.read(app).terminal_selection.as_ref().is_some_and(|s| s.moved)));
        cx.update(|_, app| app.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into())));
        cx.simulate_resize(gpui::size(px(700.), px(650.)));
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-c");
        assert_eq!(cx.read(|app| app.read_from_clipboard().and_then(|item| item.text())).as_deref(), Some("sentinel"));
        cx.simulate_keystrokes("ctrl-c");
        assert_eq!(cx.read(|app| view.read(app).terminal_status.as_str().to_string()), "Terminal has no input");
    }

    #[test]
    fn terminal_selection_hit_testing_clamps_to_grid() {
        let frame = terminal::Terminal::fixture(2, 4, b"a").frame();
        let bounds = gpui::Bounds { origin: gpui::point(px(10.), px(20.)), size: gpui::size(px(40.), px(32.)) };
        assert_eq!(terminal_cell_at(gpui::point(px(-5.), px(-5.)), bounds, &frame, 10., 16.), Some((0, 0)));
        assert_eq!(terminal_cell_at(gpui::point(px(500.), px(500.)), bounds, &frame, 10., 16.), Some((1, 3)));
    }

    #[gpui::test]
    fn terminal_snapshot_button_captures_a_non_mutating_screen(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let slot = captured.clone();
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|_| {
                let mut gui = LocalGui::new();
                gui.terminal = Some(std::rc::Rc::new(terminal::Terminal::fixture(2, 10, b" a  ")));
                gui
            });
            *slot.borrow_mut() = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let view = captured.borrow().clone().unwrap();
        cx.run_until_parked();
        let button = cx.debug_bounds("select-terminal-text").unwrap();
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        let before = cx.read(|app| view.read(app).selection_snapshot.as_ref().unwrap().input.read(app).value());
        assert!(before.starts_with(" a  "));
        cx.simulate_keystrokes("cmd-a cmd-c");
        assert_eq!(cx.read(|app| app.read_from_clipboard().unwrap().text()).as_deref(), Some(before.as_ref()));
        cx.simulate_keystrokes("escape");
        assert!(cx.read(|app| view.read(app).selection_snapshot.is_none()));
        assert!(cx.read(|app| view.read(app).terminal.is_some()));
        let ctrl_c = gpui::Keystroke::parse("ctrl-c").unwrap();
        assert!(!is_terminal_snapshot_shortcut(&ctrl_c));
        assert_eq!(terminal::key_bytes(&ctrl_c), Some(vec![3]));
        cx.simulate_keystrokes("cmd-shift-c");
        assert!(cx.read(|app| view.read(app).selection_snapshot.is_some()), "keyboard shortcut must open snapshot");
    }

    #[test]
    fn sync_results_are_bound_to_selection_generation_identity_and_version() {
        let mut gui = LocalGui::new();
        gui.sessions = vec![row("one"), row("two")];
        gui.preview_busy = true;
        gui.select_session(row("one"));
        let original = gui.selection.clone().unwrap();
        let generation = gui.preview_generation;
        gui.sync_busy = true;
        gui.sync_sender
            .send((
                generation,
                original.clone(),
                Ok(preview::SyncFrame {
                    text: "new".into(),
                    version: 4,
                    instance: original.instance.clone(),
                    cells: Vec::new(),
                }),
            ))
            .unwrap();
        gui.drain();
        assert_eq!(gui.preview.as_deref(), Some("new"));
        assert_eq!(gui.sync_version, 4);
        assert!(
            gui.preview_cells.is_none(),
            "an empty v1/fallback frame stays on the text path"
        );
        gui.select_session(row("two"));
        gui.select_session(row("one"));
        gui.sync_sender
            .send((
                generation,
                original.clone(),
                Ok(preview::SyncFrame {
                    text: "late".into(),
                    version: 5,
                    instance: original.instance.clone(),
                    cells: Vec::new(),
                }),
            ))
            .unwrap();
        gui.drain();
        assert!(gui.preview.is_none());
        let current = gui.preview_generation;
        gui.sync_sender
            .send((
                current,
                original.clone(),
                Ok(preview::SyncFrame {
                    text: "wrong".into(),
                    version: 6,
                    instance: "replacement".into(),
                    cells: Vec::new(),
                }),
            ))
            .unwrap();
        gui.drain();
        assert!(gui.preview.is_none() && gui.sync_error);
        let selection = gui.selection.clone().unwrap();
        gui.sync_sender
            .send((
                current,
                selection.clone(),
                Ok(preview::SyncFrame {
                    text: "recovered".into(),
                    version: 9,
                    instance: selection.instance,
                    cells: Vec::new(),
                }),
            ))
            .unwrap();
        gui.drain();
        assert_eq!(gui.preview.as_deref(), Some("recovered"));
        assert!(!gui.sync_error);
        gui.sync_version = 8;
        gui.sync_sender
            .send((
                current,
                original.clone(),
                Ok(preview::SyncFrame {
                    text: "old".into(),
                    version: 7,
                    instance: original.instance,
                    cells: Vec::new(),
                }),
            ))
            .unwrap();
        gui.drain();
        assert!(gui.preview.is_none() && gui.sync_error);
    }
    #[gpui::test]
    fn peer_selection_shows_instance_bound_sync_and_fences_late_update(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.peers = vec![peer("peer-full-fingerprint", "Peer")];
            gui.remote_busy = true;
            gui.preview_busy = true;
            gui
        });
        let node = cx.debug_bounds("node-peer-0").unwrap();
        cx.simulate_click(node.center(), gpui::Modifiers::none());
        view.update(cx, |gui, cx| {
            gui.remote_loading = false;
            gui.remote_error = false;
            gui.remote_sessions = vec![row("remote")];
            gui.peer_sessions
                .insert("peer-full-fingerprint".into(), vec![row("remote")]);
            cx.notify();
        });
        let session = cx.debug_bounds("session-0").unwrap();
        cx.simulate_click(session.center(), gpui::Modifiers::none());
        view.update(cx, |gui, cx| {
            let selection = gui.selection.clone().unwrap();
            assert_eq!(selection.peer.as_deref(), Some("peer-full-fingerprint"));
            let generation = gui.preview_generation;
            gui.sync_busy = true;
            gui.sync_sender
                .send((
                    generation,
                    selection.clone(),
                    Ok(preview::SyncFrame {
                        text: "peer frame".into(),
                        version: 2,
                        instance: selection.instance.clone(),
                    cells: Vec::new(),
                }),
                ))
                .unwrap();
            gui.drain();
            assert_eq!(gui.preview.as_deref(), Some("peer frame"));
            gui.clear_preview();
            gui.sync_sender
                .send((
                    generation,
                    selection,
                    Ok(preview::SyncFrame {
                        text: "late frame".into(),
                        version: 3,
                        instance: "instance".into(),
                    cells: Vec::new(),
                }),
                ))
                .unwrap();
            gui.drain();
            assert!(gui.preview.is_none());
            cx.notify();
        });
        assert!(cx.debug_bounds("preview").is_some());
    }
    #[gpui::test]
    #[ignore = "run with gui/tests/peer_live_smoke.py and its isolated peers"]
    fn real_peer_live_view_reaches_gui(cx: &mut gpui::TestAppContext) {
        let fingerprint = std::env::var("REMUDA_GUI_TEST_PEER_FP").unwrap();
        let expected_instance = std::env::var("REMUDA_GUI_TEST_PEER_INSTANCE").unwrap();
        assert!(std::env::var_os("REMUDA_GUI_ACCEPTANCE_WORKER").is_some());
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.refresh_nodes();
            gui
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(12);
        loop {
            view.update(cx, |gui, cx| {
                gui.drain();
                cx.notify();
            });
            cx.run_until_parked();
            if cx.read(|app| {
                view.read(app)
                    .peers
                    .iter()
                    .any(|p| p.fingerprint == fingerprint && p.admitted)
            }) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "peer did not reach GUI"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let node = cx.debug_bounds("node-peer-0").expect("peer row laid out");
        cx.simulate_click(node.center(), gpui::Modifiers::none());
        let deadline = std::time::Instant::now() + Duration::from_secs(12);
        loop {
            view.update(cx, |gui, cx| {
                gui.drain();
                cx.notify();
            });
            cx.run_until_parked();
            if cx.read(|app| {
                view.read(app).remote_sessions.iter().any(|row| {
                    row.name == "proof"
                        && row.instance_id.as_deref() == Some(expected_instance.as_str())
                })
            }) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "peer session did not reach GUI"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let session = cx
            .debug_bounds("session-0")
            .expect("peer session row laid out");
        cx.simulate_click(session.center(), gpui::Modifiers::none());
        let deadline = std::time::Instant::now() + Duration::from_secs(24);
        let mut saw_initial = false;
        loop {
            view.update(cx, |gui, cx| {
                gui.drain();
                cx.notify();
            });
            cx.run_until_parked();
            let frame = cx.read(|app| view.read(app).preview.clone().unwrap_or_default());
            if frame.contains("REMOTE_START") && !frame.contains("REMOTE_LATER") {
                saw_initial = true;
            }
            if saw_initial && frame.contains("REMOTE_LATER") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "peer output did not reach GUI; first={saw_initial}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(cx.debug_bounds("preview").is_some());
        assert!(
            cx.read(|app| view
                .read(app)
                .selection
                .as_ref()
                .is_some_and(|s| s.peer.as_deref() == Some(fingerprint.as_str())
                    && s.instance == expected_instance))
        );
        assert!(cx.debug_bounds("line-input").is_some(), "peer input control not laid out");
        assert!(cx.update(|window, app| view.read(app).input_focus.as_ref().is_some_and(|f| f.is_focused(window))), "peer session click must focus its input");
        let (row, col) = cx.read(|app| {
            view.read(app).preview_frame().unwrap().cells.iter().enumerate().find_map(|(row, cells)| {
                let line: String = cells.iter().map(|cell| cell.text.as_str()).collect();
                line.find("REMOTE_START").map(|col| (row, col))
            }).expect("structured peer preview marker")
        });
        let bounds = cx.debug_bounds("preview-grid").unwrap();
        let (cell_w, line_h) = cx.update(|window, _| terminal_metrics(window));
        let point = |c: f32| gpui::point(bounds.left() + px(cell_w * c), bounds.top() + px(line_h * (row as f32 + 0.5)));
        cx.simulate_mouse_down(point(col as f32 + 0.2), gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_move(point(col as f32 + 5.2), Some(gpui::MouseButton::Left), gpui::Modifiers::none());
        cx.simulate_mouse_up(point(col as f32 + 5.2), gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_keystrokes("cmd-c");
        assert_eq!(cx.read(|app| app.read_from_clipboard().and_then(|item| item.text())).as_deref(), Some("REMOTE"));
        assert!(cx.read(|app| { let gui = view.read(app); gui.terminal.is_none() && gui.input_helper.is_none() }));
        let screen = cx.debug_bounds("preview-grid").unwrap();
        cx.simulate_click(screen.center(), gpui::Modifiers::none());
        assert!(cx.update(|window, app| view.read(app).input_focus.as_ref().is_some_and(|f| f.is_focused(window))), "clicking preview must focus current peer input");
        cx.simulate_keystrokes("p->p r->r o->o o->o f->f enter");
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            view.update(cx, |gui, cx| {
                gui.drain();
                cx.notify();
            });
            cx.run_until_parked();
            if cx.read(|app| {
                view.read(app).input_status == "Sent"
                    && view
                        .read(app)
                        .preview
                        .as_deref()
                        .is_some_and(|text| text.contains("INPUT:proof"))
            }) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "guarded peer input not observed: {}",
                cx.read(|app| view.read(app).input_status.clone())
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            cx.read(|app| view
                .read(app)
                .input_target
                .as_ref()
                .is_some_and(|s| s.peer.as_deref() == Some(fingerprint.as_str())
                    && s.instance == expected_instance))
        );
    }
    #[test]
    fn input_requires_current_local_instance() {
        let mut gui = LocalGui::new();
        gui.sessions = vec![row("proof")];
        gui.preview_busy = true;
        gui.select_session(row("proof"));
        gui.input_text = "do-not-send".into();
        gui.sessions[0].instance_id = Some("replacement".into());
        gui.submit_local_line();
        assert!(gui.input_helper.is_none());
        assert_eq!(gui.input_text, "do-not-send");
    }
    #[test]
    fn peer_input_refuses_stale_selection_before_starting_helper() {
        for invalid in ["revoked", "replaced", "offline"] {
            let mut gui = LocalGui::new();
            gui.peers = vec![peer("fingerprint", "peer")];
            gui.selected_peer = Some("fingerprint".into());
            gui.remote_sessions = vec![row("peer-session")];
            gui.preview_busy = true;
            gui.select_session(row("peer-session"));
            gui.input_text = "preserve draft".into();
            match invalid {
                "revoked" => gui.peers[0].admitted = false,
                "replaced" => gui.remote_sessions[0].instance_id = Some("replacement".into()),
                _ => gui.remote_error = true,
            }
            let started = std::cell::Cell::new(false);
            gui.submit_local_line_with(|_| {
                started.set(true);
                Err(std::io::Error::other("must not start"))
            });
            assert!(!started.get(), "{invalid} peer started input helper");
            assert_eq!(gui.input_text, "preserve draft");
        }
    }
    #[gpui::test]
    fn immediate_input_failure_is_shown_against_the_attempted_target(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.sessions = vec![row("one"), row("two")];
            gui.preview_busy = true;
            gui
        });
        for name in ["one", "two"] {
            view.update(cx, |gui, cx| {
                gui.select_session(row(name));
                gui.input_text = "test".into();
                gui.submit_local_line_with(|_| Err(std::io::Error::other("fixture spawn failure")));
                assert_eq!(gui.input_target.as_ref().unwrap().name, name);
                assert_eq!(gui.input_status, "Input helper unavailable");
                assert_eq!(gui.input_text, "test");
                cx.notify();
            });
            assert!(cx.debug_bounds("input-result-target").is_some());
        }
    }
    #[test]
    fn immediate_queue_rejection_is_attributed_to_the_attempted_target() {
        let mut gui = LocalGui::new();
        gui.sessions = vec![row("one"), row("two")];
        gui.preview_busy = true;
        gui.select_session(row("one"));
        gui.input_target = gui.selection.clone();
        gui.input_status = "Sent".into();
        gui.select_session(row("two"));
        gui.input_text = "x".repeat(remuda_native::strict_input::MAX_BATCH);
        gui.submit_local_line_with(|_| {
            input_helper::InputHelper::start(std::path::Path::new("/bin/sleep"), &["30"])
        });
        assert_eq!(gui.input_target.as_ref().unwrap().name, "two");
        assert_eq!(gui.input_status, "Input could not be queued");
        assert!(gui.input_helper.is_none());
    }
    #[test]
    fn selection_changes_do_not_allow_overlapping_input_and_uncertainty_stays_blocked() {
        let mut gui = LocalGui::new();
        gui.sessions = vec![row("one"), row("two")];
        gui.preview_busy = true;
        gui.select_session(row("one"));
        let original = gui.selection.clone().unwrap();
        let helper =
            input_helper::InputHelper::start(std::path::Path::new("/bin/sleep"), &["30"]).unwrap();
        assert_eq!(helper.submit(b"test\r"), input_helper::Submit::Queued);
        gui.input_target = Some(original.clone());
        gui.input_helper = Some(helper);
        for next in ["one", "two", "one"] {
            gui.select_session(row(next));
            gui.input_text = "second".into();
            gui.submit_local_line();
            assert_eq!(gui.input_target.as_ref(), Some(&original));
            assert!(gui.input_helper.is_some());
            assert_eq!(gui.input_text, "second");
        }
        gui.input_helper.as_ref().unwrap().stop();
        gui.drain();
        assert!(gui.input_helper.is_none());
        assert!(gui.input_stopped);
        assert_eq!(gui.input_warnings, vec![original]);
        gui.select_session(row("two"));
        gui.input_text = "third".into();
        gui.submit_local_line();
        assert!(gui.input_helper.is_none());
        assert_eq!(gui.input_text, "third");
    }
    #[test]
    fn delayed_ack_is_attributed_to_original_target_before_new_input_is_allowed() {
        let mut gui = LocalGui::new();
        gui.sessions = vec![row("one"), row("two")];
        gui.preview_busy = true;
        gui.select_session(row("one"));
        let original = gui.selection.clone().unwrap();
        let helper = input_helper::InputHelper::start(
            std::path::Path::new("/bin/sh"),
            &["-c", "printf 'remuda-input-v2\\n'; dd bs=1 count=5 of=/dev/null 2>/dev/null; sleep 0.3; printf '\\001'"],
        ).unwrap();
        assert_eq!(helper.submit(b"test\r"), input_helper::Submit::Queued);
        gui.input_target = Some(original.clone());
        gui.input_helper = Some(helper);
        gui.select_session(row("two"));
        gui.input_text = "second".into();
        gui.submit_local_line();
        assert_eq!(gui.input_target.as_ref(), Some(&original));
        assert!(gui.input_helper.is_some());
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while gui.input_helper.is_some() {
            gui.drain();
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(gui.input_target.as_ref(), Some(&original));
        assert_eq!(gui.input_status, "Sent");
        assert!(!gui.input_stopped);
        assert!(gui.input_warnings.is_empty());
    }
    #[gpui::test]
    fn selected_session_and_exact_input_target_are_laid_out(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.preview_busy = true;
            gui.sessions = vec![row("one"), row("two")];
            gui
        });
        let bounds = cx.debug_bounds("session-1").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        assert!(cx.debug_bounds("selected-session").is_some());
        assert!(cx.debug_bounds("input-target").is_some());
        assert!(cx.debug_bounds("preview-target").is_some());
        assert_eq!(
            cx.read(|app| selection_label(view.read(app).selection.as_ref().unwrap())),
            "This device / \"two\" · instance \"instance\""
        );
    }
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
    #[test]
    fn attach_requires_confirmation_for_the_exact_target() {
        let mut gui = LocalGui::new();
        gui.sessions = vec![row("one"), row("two")];
        gui.preview_busy = true;
        gui.select_session(row("one"));
        gui.attach_selected();
        assert!(
            gui.terminal.is_none(),
            "attach started without confirmation"
        );
        assert!(
            gui.terminal_status.contains("Confirm attach"),
            "{}",
            gui.terminal_status
        );
        gui.request_attach();
        assert_eq!(gui.attach_confirm, gui.selection);
        assert!(gui.terminal.is_none());
        gui.cancel_attach();
        assert!(gui.attach_confirm.is_none());
        gui.request_attach();
        gui.select_session(row("one"));
        assert!(
            gui.attach_confirm.is_none(),
            "a selection change must clear the attach confirmation"
        );
    }

    #[gpui::test]
    fn initial_window_opens_documents_without_pointer(cx: &mut gpui::TestAppContext) {
        let socket = std::env::temp_dir().join(format!("gui-documents-initial-{}", std::process::id()));
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.preview_busy = true;
            gui.documents_socket = Some(socket.clone());
            gui
        });
        cx.simulate_keystrokes("shift-tab");
        assert!(cx.update(|window, app| view.read(app).toggle_focus.as_ref().unwrap().is_focused(window)));
        cx.simulate_keystrokes("tab");
        assert!(cx.update(|window, app| view.read(app).tree_focus.as_ref().unwrap().is_focused(window)));
        cx.simulate_keystrokes("tab enter");
        assert!(cx.update(|window, app| {
            let gui = view.read(app);
            gui.documents.is_some() && gui.documents_focus.as_ref().is_some_and(|f| f.is_focused(window))
        }));
        cx.simulate_keystrokes("d q");
        assert!(cx.update(|window, app| {
            let gui = view.read(app);
            gui.documents.is_none() && gui.tree_focus.as_ref().is_some_and(|f| f.is_focused(window))
        }), "closing without a session must return focus to the tree");
        cx.simulate_keystrokes("tab enter");
        assert!(cx.update(|window, app| {
            let gui = view.read(app);
            gui.documents.is_some() && gui.documents_focus.as_ref().is_some_and(|f| f.is_focused(window))
        }), "the tree must allow reopening documents");
    }

    #[gpui::test]
    fn keyboard_opens_documents_reaches_the_divider_and_returns_to_the_pane(
        cx: &mut gpui::TestAppContext,
    ) {
        let socket = std::env::temp_dir().join(format!(
            "gui-documents-keyboard-{}",
            std::process::id()
        ));
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.preview_busy = true;
            gui.sessions = vec![row("one")];
            gui.documents_socket = Some(socket.clone());
            gui
        });
        let cursor = cx.read(|app| view.read(app).tree_cursor);
        cx.simulate_keystrokes("ctrl-down alt-enter cmd-enter");
        assert_eq!(cx.read(|app| view.read(app).tree_cursor), cursor);
        assert!(cx.read(|app| view.read(app).selection.is_none()));
        let session = cx.debug_bounds("session-0").unwrap();
        cx.simulate_click(session.center(), gpui::Modifiers::none());
        let input = cx.debug_bounds("line-input").unwrap();
        cx.simulate_click(input.center(), gpui::Modifiers::none());
        cx.simulate_keystrokes("a");
        cx.update(|window, app| view.read(app).tree_focus.as_ref().unwrap().focus(window));
        cx.simulate_keystrokes("shift-tab");
        assert!(cx.update(|window, app| view.read(app).input_focus.as_ref().unwrap().is_focused(window)));
        cx.update(|window, app| view.read(app).tree_focus.as_ref().unwrap().focus(window));
        // From the initial state, Tab reaches the documents toggle and Enter
        // opens the workspace without a pointer.
        cx.simulate_keystrokes("tab");
        let toggle_focused = cx.update(|window, app| {
            view.read(app)
                .toggle_focus
                .as_ref()
                .is_some_and(|focus| focus.is_focused(window))
        });
        assert!(toggle_focused, "Tab must reach documents-toggle");
        cx.simulate_keystrokes("tab");
        assert!(cx.update(|window, app| view.read(app).input_focus.as_ref().unwrap().is_focused(window)), "Tab from toggle must reach session input");
        cx.update(|window, app| view.read(app).tree_focus.as_ref().unwrap().focus(window));
        cx.simulate_keystrokes("tab");
        cx.simulate_keystrokes("enter");
        let (open, pane_focused) = cx.update(|window, app| {
            let gui = view.read(app);
            (
                gui.documents.is_some(),
                gui.documents_focus
                    .as_ref()
                    .is_some_and(|focus| focus.is_focused(window)),
            )
        });
        assert!(open, "Enter on documents-toggle must open the workspace");
        assert!(pane_focused, "opening must focus the document pane");

        cx.simulate_keystrokes("d");
        let divider_focused = cx.update(|window, app| {
            view.read(app)
                .divider_focus
                .as_ref()
                .is_some_and(|focus| focus.is_focused(window))
        });
        assert!(divider_focused, "d must reach the divider");
        let before = cx.read(|app| view.read(app).document_panel_fraction);
        cx.simulate_keystrokes("left");
        let after = cx.read(|app| view.read(app).document_panel_fraction);
        assert!(after > before);

        // d/escape returns to the same pane and the panel still owns keys.
        cx.simulate_keystrokes("d");
        let pane_focused = cx.update(|window, app| {
            view.read(app)
                .documents_focus
                .as_ref()
                .is_some_and(|focus| focus.is_focused(window))
        });
        assert!(pane_focused, "d must return to the document pane");
        let before_keys = cx.read(|app| {
            view.read(app)
                .documents
                .as_ref()
                .map_or(0, |workspace| workspace.key_events())
        });
        cx.simulate_keystrokes("down");
        let after_keys = cx.read(|app| {
            view.read(app)
                .documents
                .as_ref()
                .map_or(0, |workspace| workspace.key_events())
        });
        assert_eq!(after_keys, before_keys + 1);

        // Modified shortcuts are not consumed by the pane/divider.
        cx.simulate_keystrokes("ctrl-d");
        let (pane_focused, keys_after_ctrl) = cx.update(|window, app| {
            let gui = view.read(app);
            (
                gui.documents_focus
                    .as_ref()
                    .is_some_and(|focus| focus.is_focused(window)),
                gui.documents
                    .as_ref()
                    .map_or(0, |workspace| workspace.key_events()),
            )
        });
        assert!(pane_focused);
        assert_eq!(keys_after_ctrl, after_keys);
        cx.simulate_keystrokes("d");
        cx.simulate_keystrokes("ctrl-c");
        let collapsed = cx.read(|app| view.read(app).document_collapsed);
        assert!(!collapsed, "ctrl-c must not collapse the panel");
        cx.simulate_keystrokes("escape");
        let pane_focused = cx.update(|window, app| {
            view.read(app)
                .documents_focus
                .as_ref()
                .is_some_and(|focus| focus.is_focused(window))
        });
        assert!(pane_focused);
        cx.simulate_keystrokes("d");
        cx.simulate_keystrokes("ctrl-q");
        assert!(cx.read(|app| view.read(app).documents.is_some()), "modified q must not close documents");
        cx.simulate_keystrokes("q");
        let (closed, input_focused, draft) = cx.update(|window, app| {
            let gui = view.read(app);
            (gui.documents.is_none(), gui.input_focus.as_ref().is_some_and(|f| f.is_focused(window)), gui.input_text.clone())
        });
        assert!(closed && input_focused, "q must close documents and focus session input");
        assert_eq!(draft, "a");
        cx.simulate_keystrokes("b");
        assert_eq!(cx.read(|app| view.read(app).input_text.clone()), "ab");
        cx.simulate_keystrokes("ctrl-tab");
        assert!(cx.update(|window, app| view.read(app).input_focus.as_ref().unwrap().is_focused(window)), "modified Tab must leave focus unchanged");
        cx.simulate_keystrokes("tab tab enter");
        assert!(cx.update(|window, app| {
            let gui = view.read(app);
            gui.documents.is_some() && gui.documents_focus.as_ref().is_some_and(|f| f.is_focused(window)) && gui.input_text == "ab"
        }), "Tab from session input must allow reopening documents without losing the draft");
    }

    #[gpui::test]
    fn default_widths_keep_terminal_and_documents_visible(cx: &mut gpui::TestAppContext) {
        let socket = std::env::temp_dir().join(format!("gui-widths-{}", std::process::id()));
        let (_view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.preview_busy = true;
            gui.sessions = vec![row("one")];
            gui.select_session(row("one"));
            gui.documents = Some(documents::Workspace::new(socket.clone()));
            gui.terminal = Some(std::rc::Rc::new(terminal::Terminal::fixture(24, 80, b"hello")));
            gui
        });
        for (width, minimum_terminal, minimum_documents) in [(1000., 400., 300.)] {
            cx.simulate_resize(gpui::size(px(width), px(650.)));
            cx.run_until_parked();
            let sidebar = cx.debug_bounds("sidebar").unwrap();
            let terminal = cx.debug_bounds("terminal-screen").unwrap();
            let documents = cx.debug_bounds("document-workspace").unwrap();
            println!("window={width} sidebar={} terminal={} documents={}", f32::from(sidebar.size.width), f32::from(terminal.size.width), f32::from(documents.size.width));
            assert!(terminal.size.width >= px(minimum_terminal), "{width}: terminal {terminal:?}");
            assert!(documents.size.width >= px(minimum_documents), "{width}: documents {documents:?}");
            assert!(sidebar.right() <= terminal.left() && terminal.right() <= documents.left(), "{width}: panes overlap");
            assert!(documents.right() <= px(width + 1.), "{width}: documents outside window: {documents:?}");
        }
    }

    #[gpui::test]
    fn narrow_switching_preserves_state_and_focus_across_resize(cx: &mut gpui::TestAppContext) {
        let socket = std::env::temp_dir().join(format!("gui-narrow-{}", std::process::id()));
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.preview_busy = true;
            gui.sessions = vec![row("one")];
            gui.select_session(row("one"));
            gui.input_text = "draft".into();
            gui.documents = Some(documents::Workspace::new(socket.clone()));
            gui.documents.as_mut().unwrap().open_tab();
            gui.terminal = Some(std::rc::Rc::new(terminal::Terminal::fixture(24, 80, b"hello")));
            gui
        });
        cx.simulate_resize(gpui::size(px(420.), px(650.)));
        cx.run_until_parked();
        assert!(cx.read(|app| view.read(app).narrow_view == NarrowView::Session));
        let terminal = cx.debug_bounds("terminal-screen").unwrap();
        assert!(terminal.size.width >= px(300.));
        cx.simulate_keystrokes("cmd-3");
        assert!(cx.read(|app| view.read(app).narrow_view == NarrowView::Documents));
        assert!(cx.update(|window, app| view.read(app).documents_focus.as_ref().unwrap().is_focused(window)));
        let body = cx.debug_bounds("document-frame-left").unwrap();
        assert!(body.size.width >= px(140.) && body.size.height >= px(150.), "document body must be usable: {body:?}");
        cx.simulate_keystrokes("x cmd-v");
        assert!(cx.read(|app| view.read(app).terminal_status.is_empty()));
        cx.simulate_keystrokes("cmd-1");
        assert!(cx.read(|app| view.read(app).narrow_view == NarrowView::Nodes));
        assert!(cx.update(|window, app| view.read(app).tree_focus.as_ref().unwrap().is_focused(window)));
        let docs_button = cx.debug_bounds("narrow-nav-2").unwrap();
        cx.simulate_click(docs_button.center(), gpui::Modifiers::none());
        assert!(cx.read(|app| view.read(app).narrow_view == NarrowView::Documents));
        cx.simulate_keystrokes("cmd-2");
        assert!(cx.update(|window, app| view.read(app).input_focus.as_ref().unwrap().is_focused(window)));
        cx.simulate_resize(gpui::size(px(1000.), px(650.)));
        cx.simulate_resize(gpui::size(px(420.), px(650.)));
        assert!(cx.read(|app| { let gui = view.read(app); gui.documents.as_ref().unwrap().len() == 2 && gui.input_text == "draft" && gui.terminal.is_some() && gui.selection.as_ref().is_some_and(|s| s.name == "one") }));
    }

    #[gpui::test]
    fn documents_pane_is_laid_out_and_operable_at_default_and_small_sizes(
        cx: &mut gpui::TestAppContext,
    ) {
        let socket =
            std::env::temp_dir().join(format!("gui-documents-layout-{}", std::process::id()));
        // The pane is present from the first frame: gpui's debug-bounds map
        // accumulates across frames, so absence is only meaningful for an
        // element that was never painted in this window.
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.preview_busy = true;
            gui.sessions = vec![row("one")];
            gui.select_session(row("one"));
            gui.documents = Some(documents::Workspace::new(socket.clone()));
            gui
        });
        assert!(cx.debug_bounds("document-pane-left").is_some());
        assert!(
            cx.debug_bounds("attach-terminal").is_some(),
            "the terminal target stays reachable beside an open document"
        );
        assert!(
            cx.debug_bounds("preview").is_some(),
            "the read-only screen stays visible beside an open document"
        );
        assert!(cx.debug_bounds("attach-confirm").is_none());
        // Dragging the divider changes the document width without replacing
        // the adjacent session pane.
        let divider = cx.debug_bounds("document-divider").unwrap();
        let before = cx.debug_bounds("document-workspace").unwrap().size.width;
        let start = divider.center();
        let target = gpui::point(start.x - px(80.), start.y);
        cx.simulate_mouse_down(start, gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_move(target, Some(gpui::MouseButton::Left), gpui::Modifiers::none());
        cx.simulate_mouse_up(target, gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.run_until_parked();
        let after = cx.debug_bounds("document-workspace").unwrap().size.width;
        assert!(after > before + px(40.), "document panel width: {before} -> {after}");
        let collapse = cx.debug_bounds("document-collapse").unwrap();
        cx.simulate_click(collapse.center(), gpui::Modifiers::none());
        let (collapsed, tabs) = cx.read(|app| {
            let gui = view.read(app);
            (gui.document_collapsed, gui.documents.as_ref().map_or(0, |workspace| workspace.len()))
        });
        assert!(collapsed && tabs >= 1);
        assert!(cx.debug_bounds("document-rail").is_some());
        let restore = cx.debug_bounds("document-restore").unwrap();
        cx.simulate_click(restore.center(), gpui::Modifiers::none());
        let (collapsed, tabs_after) = cx.read(|app| {
            let gui = view.read(app);
            (gui.document_collapsed, gui.documents.as_ref().map_or(0, |workspace| workspace.len()))
        });
        assert!(!collapsed);
        assert_eq!(tabs_after, tabs, "collapse must preserve open tabs");
        let frame = cx.debug_bounds("document-frame-left").unwrap();
        cx.simulate_click(frame.center(), gpui::Modifiers::none());
        cx.simulate_keystrokes("d");
        let divider_focused = cx.update(|window, app| {
            view.read(app).divider_focus.as_ref().is_some_and(|focus| focus.is_focused(window))
        });
        assert!(divider_focused, "d must focus the divider");
        let before = cx.debug_bounds("document-workspace").unwrap().size.width;
        cx.simulate_keystrokes("left");
        cx.run_until_parked();
        let after = cx.debug_bounds("document-workspace").unwrap().size.width;
        assert!(after > before, "left must widen the panel: {before} -> {after}");
        cx.simulate_keystrokes("home");
        cx.run_until_parked();
        let min_width = cx.debug_bounds("document-workspace").unwrap().size.width;
        let min_fraction = cx.read(|app| view.read(app).document_panel_fraction);
        assert_eq!(min_fraction, MIN_DOCUMENT_FRACTION);
        assert!(min_width >= px(180.));
        cx.simulate_keystrokes("end");
        cx.run_until_parked();
        let max_width = cx.debug_bounds("document-workspace").unwrap().size.width;
        let max_fraction = cx.read(|app| view.read(app).document_panel_fraction);
        assert_eq!(max_fraction, MAX_DOCUMENT_FRACTION);
        assert!(max_width > min_width);
        cx.simulate_keystrokes("c");
        assert!(cx.read(|app| view.read(app).document_collapsed));
        let collapse_focused = cx.update(|window, app| {
            view.read(app).collapse_focus.as_ref().is_some_and(|focus| focus.is_focused(window))
        });
        assert!(collapse_focused, "collapse must move focus to the control");
        cx.simulate_keystrokes("enter");
        let (collapsed, tabs_after) = cx.read(|app| {
            let gui = view.read(app);
            (gui.document_collapsed, gui.documents.as_ref().map_or(0, |workspace| workspace.len()))
        });
        assert!(!collapsed && tabs_after >= 1);
        let divider_focused = cx.update(|window, app| {
            view.read(app).divider_focus.as_ref().is_some_and(|focus| focus.is_focused(window))
        });
        assert!(divider_focused, "restore must return focus to the divider");
        for size in [None, Some(gpui::size(px(420.), px(320.)))] {
            if let Some(size) = size {
                cx.simulate_resize(size);
                let docs = cx.debug_bounds("narrow-nav-2").unwrap();
                cx.simulate_click(docs.center(), gpui::Modifiers::none());
            }
            assert!(cx.debug_bounds("document-frame-left").is_some(), "frame missing");
            assert!(
                cx.debug_bounds("document-controls").is_some(),
                "controls missing"
            );
            for id in [
                "doc-up",
                "doc-down",
                "doc-open",
                "doc-back",
                "doc-prev-link",
                "doc-next-link",
                "doc-refresh",
                "doc-new-tab",
                "doc-split",
                "doc-close-tab",
            ] {
                if size.is_some() && matches!(id, "doc-new-tab" | "doc-split" | "doc-close-tab") { continue; }
                let bounds = cx
                    .debug_bounds(id)
                    .unwrap_or_else(|| panic!("{id} missing"));
                assert!(bounds.size.width > px(0.), "{id} has no clickable width");
                cx.simulate_click(bounds.center(), gpui::Modifiers::none());
            }
        }
        // At the small size the document pane still owns keyboard focus.
        let frame = cx.debug_bounds("document-frame-left").unwrap();
        cx.simulate_click(frame.center(), gpui::Modifiers::none());
        let before_keys = cx.read(|app| {
            view.read(app)
                .documents
                .as_ref()
                .map_or(0, |workspace| workspace.key_events())
        });
        cx.simulate_keystrokes("down");
        let after_keys = cx.read(|app| {
            view.read(app)
                .documents
                .as_ref()
                .map_or(0, |workspace| workspace.key_events())
        });
        assert_eq!(after_keys, before_keys + 1, "pane must keep keyboard focus");

        cx.simulate_resize(gpui::size(px(1000.), px(650.)));
        let toggle = cx.debug_bounds("documents-toggle").unwrap();
        cx.simulate_click(toggle.center(), gpui::Modifiers::none());
        view.update(cx, |gui, _| assert!(gui.documents.is_none()));
        // The session area remains painted after closing the panel.
        assert!(cx.debug_bounds("attach-terminal").is_some());
        assert!(cx.debug_bounds("input-target").is_some());
        assert!(cx.debug_bounds("preview-target").is_some());
    }

    #[gpui::test]
    #[ignore = "run with gui/tests/unified_window_smoke.py and its isolated daemon"]
    fn real_terminal_and_two_documents_share_workspace(cx: &mut gpui::TestAppContext) {
        assert!(std::env::var_os("REMUDA_GUI_ACCEPTANCE_WORKER").is_some());
        cx.update(gpui_component::init);
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let slot = captured.clone();
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|_| { let mut gui = LocalGui::new(); gui.refresh(); gui });
            *slot.borrow_mut() = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let view = captured.borrow().clone().unwrap();
        cx.simulate_resize(gpui::size(px(1600.), px(900.)));
        let deadline = std::time::Instant::now() + Duration::from_secs(12);
        loop {
            view.update(cx, |gui, cx| { gui.drain(); cx.notify(); });
            cx.run_until_parked();
            if cx.read(|app| view.read(app).sessions.iter().any(|s| s.name == "document-proof")) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "isolated session not listed");
            std::thread::sleep(Duration::from_millis(50));
        }
        let row = cx.debug_bounds("session-0").unwrap();
        cx.simulate_click(row.center(), gpui::Modifiers::none());
        let attach = cx.debug_bounds("attach-terminal").unwrap();
        cx.simulate_click(attach.center(), gpui::Modifiers::none());
        let confirm = cx.debug_bounds("confirm-attach").unwrap();
        cx.simulate_click(confirm.center(), gpui::Modifiers::none());
        assert!(cx.read(|app| view.read(app).terminal.is_some()), "isolated terminal did not start");
        view.update(cx, |gui, _| {
            gui.terminal.as_ref().unwrap().send(b"printf '\\nUNIFIED_PROOF\\n'\r".to_vec()).unwrap();
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(12);
        loop {
            view.update(cx, |gui, cx| { gui.drain(); cx.notify(); });
            cx.run_until_parked();
            let found = cx.read(|app| view.read(app).terminal.as_ref().is_some_and(|terminal| {
                terminal.frame().cells.iter().any(|row| {
                    row.iter().map(|cell| cell.text.as_str()).collect::<String>().trim() == "UNIFIED_PROOF"
                })
            }));
            if found { break; }
            assert!(std::time::Instant::now() < deadline, "isolated terminal output not shown");
            std::thread::sleep(Duration::from_millis(50));
        }
        let toggle = cx.debug_bounds("documents-toggle").unwrap();
        cx.simulate_click(toggle.center(), gpui::Modifiers::none());
        let deadline = std::time::Instant::now() + Duration::from_secs(12);
        loop {
            view.update(cx, |gui, cx| { gui.drain(); cx.notify(); });
            cx.run_until_parked();
            let listed = cx.read(|app| view.read(app).documents.as_ref().is_some_and(|w| {
                w.frame(w.active_index(), 80, 15).join("\\n").contains("guide.md")
            }));
            if listed { break; }
            assert!(std::time::Instant::now() < deadline, "private VFS listing not shown");
            std::thread::sleep(Duration::from_millis(50));
        }
        let open = cx.debug_bounds("doc-open").unwrap();
        cx.simulate_click(open.center(), gpui::Modifiers::none());
        let deadline = std::time::Instant::now() + Duration::from_secs(12);
        loop {
            view.update(cx, |gui, cx| { gui.drain(); cx.notify(); });
            cx.run_until_parked();
            if cx.read(|app| view.read(app).documents.as_ref().is_some_and(|w| w.active().location() == "guide.md" && w.active().status().starts_with("ready"))) { break; }
            assert!(std::time::Instant::now() < deadline, "guide document not shown");
            std::thread::sleep(Duration::from_millis(50));
        }
        // A document-focused Ctrl-C must never be routed to the live terminal.
        view.update(cx, |gui, _| gui.terminal.as_ref().unwrap().send(b"printf 'DOC_WAIT_START\\n'; sleep 2; printf 'DOC_CTRL_SAFE\\n'\r".to_vec()).unwrap());
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            view.update(cx, |gui, cx| { gui.drain(); cx.notify(); }); cx.run_until_parked();
            if cx.read(|app| view.read(app).terminal.as_ref().unwrap().frame().cells.iter().any(|row| row.iter().map(|c| c.text.as_str()).collect::<String>().trim() == "DOC_WAIT_START")) { break; }
            assert!(std::time::Instant::now() < deadline, "terminal wait did not start"); std::thread::sleep(Duration::from_millis(30));
        }
        let doc=cx.debug_bounds("md-h1-left-0").unwrap(); cx.simulate_click(doc.center(),gpui::Modifiers::none());
        cx.simulate_keystrokes("x ctrl-c");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            view.update(cx, |gui, cx| { gui.drain(); cx.notify(); }); cx.run_until_parked();
            if cx.read(|app| view.read(app).terminal.as_ref().unwrap().frame().cells.iter().any(|row| row.iter().map(|c| c.text.as_str()).collect::<String>().trim() == "DOC_CTRL_SAFE")) { break; }
            assert!(std::time::Instant::now() < deadline, "document input interrupted terminal"); std::thread::sleep(Duration::from_millis(30));
        }
        // In narrow mode the session pane is hidden; selection must remain
        // reachable inside the actual Documents view.
        cx.simulate_resize(gpui::size(px(420.), px(320.)));
        cx.run_until_parked();
        let docs = cx.debug_bounds("narrow-nav-2").unwrap();
        cx.simulate_click(docs.center(), gpui::Modifiers::none());
        cx.simulate_keystrokes("s");
        assert!(cx.read(|app| view.read(app).selection_snapshot.as_ref().is_some_and(|snapshot| snapshot.label.contains("guide.md"))), "S opens source selection in narrow Docs view");
        cx.simulate_keystrokes("escape");
        let select = cx.debug_bounds("select-document-text").unwrap();
        assert!(select.left() >= px(0.) && select.right() <= px(420.) && select.top() >= px(0.) && select.bottom() <= px(320.), "selection control outside narrow viewport: {select:?}");
        cx.simulate_click(select.center(), gpui::Modifiers::none());
        assert!(cx.read(|app| view.read(app).selection_snapshot.as_ref().is_some_and(|snapshot| snapshot.label.contains("guide.md") && snapshot.label.contains("last loaded v") && snapshot.label.contains("sha256:") && snapshot.label.contains("last load succeeded"))));
        cx.simulate_keystrokes("cmd-a cmd-c");
        assert_eq!(cx.read(|app| app.read_from_clipboard().unwrap().text()).as_deref(), Some("# Guide\n\nRead **this** [next](next.md).\n\n![diagram](zz-art.png)\n"));
        cx.simulate_keystrokes("escape");
        assert!(cx.read(|app| view.read(app).selection_snapshot.is_none()));
        cx.simulate_resize(gpui::size(px(1600.), px(900.)));
        cx.run_until_parked();
        cx.simulate_keystrokes("s");
        assert!(cx.read(|app| view.read(app).selection_snapshot.is_some()), "S opens source selection in wide Docs view");
        cx.simulate_keystrokes("escape");
        let select = cx.debug_bounds("select-document-text").unwrap();
        cx.simulate_click(select.center(), gpui::Modifiers::none());
        assert!(cx.read(|app| view.read(app).selection_snapshot.as_ref().unwrap().label.contains("guide.md")));
        cx.simulate_keystrokes("cmd-a cmd-c");
        assert_eq!(cx.read(|app| app.read_from_clipboard().unwrap().text()).as_deref(), Some("# Guide\n\nRead **this** [next](next.md).\n\n![diagram](zz-art.png)\n"));
        cx.simulate_keystrokes("escape");
        assert!(cx.read(|app| view.read(app).selection_snapshot.is_none()));
        assert!(cx.read(|app| view.read(app).terminal.is_some()));
        let deadline = std::time::Instant::now() + Duration::from_secs(12);
        loop {
            view.update(cx, |gui, cx| { gui.drain(); cx.notify(); });
            cx.run_until_parked();
            let ready = cx.read(|app| view.read(app).documents.as_ref().is_some_and(|w| {
                matches!(w.active().image("zz-art.png"), Some(documents::ImageState::Ready { .. }))
            }));
            if ready && cx.debug_bounds("md-image-left-2-0").is_some() { break; }
            if std::time::Instant::now() >= deadline {
                let state = cx.read(|app| format!("{:?}", view.read(app).documents.as_ref().and_then(|w| w.active().image("zz-art.png"))));
                panic!("private VFS image not displayed in guide: {state}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // Small histories retain rich Markdown across two Back steps.
        for path in ["next.md", "guide.md"] {
            view.update(cx, |gui, cx| { gui.documents.as_mut().unwrap().active_mut().open_document(path); cx.notify(); });
            let deadline = std::time::Instant::now() + Duration::from_secs(12);
            loop {
                view.update(cx, |gui, cx| { gui.drain(); cx.notify(); });
                cx.run_until_parked();
                if cx.read(|app| view.read(app).documents.as_ref().is_some_and(|w| w.active().location() == path && w.active().status().starts_with("ready"))) { break; }
                assert!(std::time::Instant::now() < deadline, "private document {path} not ready");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let back = cx.debug_bounds("doc-back").unwrap();
        cx.simulate_click(back.center(), gpui::Modifiers::none());
        cx.simulate_click(back.center(), gpui::Modifiers::none());
        assert!(cx.read(|app| view.read(app).documents.as_ref().is_some_and(|w| w.active().location() == "guide.md" && w.active().markdown_source().is_some())));
        assert!(cx.debug_bounds("md-image-left-2-0").is_some());
        let link = cx.debug_bounds("md-link-left-1-3").unwrap();
        cx.simulate_click(link.center(), gpui::Modifiers::none());
        let deadline = std::time::Instant::now() + Duration::from_secs(12);
        loop {
            view.update(cx, |gui, cx| { gui.drain(); cx.notify(); });
            cx.run_until_parked();
            if cx.read(|app| view.read(app).documents.as_ref().is_some_and(|w| w.active().location() == "next.md" && w.active().status().starts_with("ready"))) { break; }
            assert!(std::time::Instant::now() < deadline, "restored Markdown link did not open");
            std::thread::sleep(Duration::from_millis(50));
        }
        cx.simulate_click(back.center(), gpui::Modifiers::none());
        assert!(cx.read(|app| view.read(app).documents.as_ref().is_some_and(|w| w.active().location() == "guide.md" && w.active().markdown_source().is_some())));
        let new_tab = cx.debug_bounds("document-new-tab").unwrap();
        cx.simulate_click(new_tab.center(), gpui::Modifiers::none());
        let deadline = std::time::Instant::now() + Duration::from_secs(12);
        loop {
            view.update(cx, |gui, cx| { gui.drain(); cx.notify(); });
            cx.run_until_parked();
            if cx.read(|app| view.read(app).documents.as_ref().is_some_and(|w| w.frame(w.active_index(), 80, 15).join("\\n").contains("next.md"))) { break; }
            assert!(std::time::Instant::now() < deadline, "second VFS listing not shown");
            std::thread::sleep(Duration::from_millis(50));
        }
        let down = cx.debug_bounds("doc-down").unwrap();
        cx.simulate_click(down.center(), gpui::Modifiers::none());
        let open = cx.debug_bounds("doc-open").unwrap();
        cx.simulate_click(open.center(), gpui::Modifiers::none());
        let deadline = std::time::Instant::now() + Duration::from_secs(12);
        loop {
            view.update(cx, |gui, cx| { gui.drain(); cx.notify(); });
            cx.run_until_parked();
            if cx.read(|app| view.read(app).documents.as_ref().is_some_and(|w| w.active().location() == "next.md" && w.active().status().starts_with("ready"))) { break; }
            assert!(std::time::Instant::now() < deadline, "second document not shown");
            std::thread::sleep(Duration::from_millis(50));
        }
        let select = cx.debug_bounds("select-document-text").unwrap();
        cx.simulate_click(select.center(), gpui::Modifiers::none());
        assert!(cx.read(|app| view.read(app).selection_snapshot.as_ref().unwrap().label.contains("next.md")));
        cx.simulate_keystrokes("cmd-a cmd-c");
        assert_eq!(cx.read(|app| app.read_from_clipboard().unwrap().text()).as_deref(), Some("# Next\n\nSecond page prose.\n"));
        cx.simulate_keystrokes("escape");
        // Evict next.md's raw history by exceeding the bounded source budget.
        for path in ["zz-big-a.txt", "zz-big-b.txt", "guide.md"] {
            view.update(cx, |gui, cx| { gui.documents.as_mut().unwrap().active_mut().open_document(path); cx.notify(); });
            let deadline = std::time::Instant::now() + Duration::from_secs(12);
            loop {
                view.update(cx, |gui, cx| { gui.drain(); cx.notify(); });
                cx.run_until_parked();
                if cx.read(|app| view.read(app).documents.as_ref().is_some_and(|w| w.active().location() == path && w.active().status().starts_with("ready"))) { break; }
                assert!(std::time::Instant::now() < deadline, "private document {path} not ready");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        // Change the private VFS while the historical next.md remains in memory.
        {
            use std::io::Write;
            let mut child = std::process::Command::new(std::env::var_os("REMUDA_GUI_ACCEPTANCE_REMUDA").unwrap())
                .args(["vfs", "put", "-", "remuda:///next.md"])
                .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).spawn().unwrap();
            child.stdin.take().unwrap().write_all(b"# Updated\n\nChanged content.\n").unwrap();
            assert!(child.wait().unwrap().success());
        }
        let back = cx.debug_bounds("doc-back").unwrap();
        cx.simulate_click(back.center(), gpui::Modifiers::none());
        cx.simulate_click(back.center(), gpui::Modifiers::none());
        cx.simulate_click(back.center(), gpui::Modifiers::none());
        assert!(cx.read(|app| view.read(app).documents.as_ref().is_some_and(|w| {
            w.active().location() == "next.md" && w.active().source_text().is_none() &&
            w.active().status().contains("Historical source evicted") &&
            w.frame(w.active_index(), 80, 15).join(" ").contains("Second page prose")
        })));
        let select = cx.debug_bounds("select-document-text").unwrap();
        cx.simulate_click(select.center(), gpui::Modifiers::none());
        assert!(cx.read(|app| view.read(app).selection_snapshot.is_none()));
        assert!(cx.read(|app| view.read(app).selection_error.is_some_and(|e| e.contains("Refresh loads"))));
        // Only the user's explicit refresh may fetch the changed version.
        let refresh = cx.debug_bounds("doc-refresh").unwrap();
        cx.simulate_click(refresh.center(), gpui::Modifiers::none());
        let deadline = std::time::Instant::now() + Duration::from_secs(12);
        loop {
            view.update(cx, |gui, cx| { gui.drain(); cx.notify(); });
            cx.run_until_parked();
            if cx.read(|app| view.read(app).documents.as_ref().is_some_and(|w| w.active().source_text() == Some("# Updated\n\nChanged content.\n"))) { break; }
            assert!(std::time::Instant::now() < deadline, "explicit refresh did not load updated version");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(cx.read(|app| view.read(app).selection_error.is_none()));
        let split = cx.debug_bounds("document-split").unwrap();
        cx.simulate_click(split.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        let terminal = cx.debug_bounds("terminal-screen").expect("terminal screen present");
        let left = cx.debug_bounds("document-pane-left").expect("first document pane");
        let right = cx.debug_bounds("document-pane-right").expect("second document pane");
        assert!(terminal.right() <= left.left(), "terminal and documents overlap");
        assert!(left.right() <= right.left(), "document panes overlap");
        let locations = cx.read(|app| {
            let gui = view.read(app);
            assert!(gui.terminal.is_some(), "terminal closed when documents opened");
            let w = gui.documents.as_ref().unwrap();
            (w.active().location().to_owned(), w.pane(w.split_index().unwrap()).unwrap().location().to_owned())
        });
        assert_eq!(locations, ("next.md".to_string(), "guide.md".to_string()));
        for id in ["document-tab-0", "document-new-tab", "document-split", "document-tab-close-0"] {
            let terminal = cx.debug_bounds("terminal-screen").unwrap();
            cx.simulate_click(terminal.center(), gpui::Modifiers::none());
            assert!(cx.update(|window, app| view.read(app).input_focus.as_ref().unwrap().is_focused(window)));
            let before = cx.read(|app| view.read(app).terminal.as_ref().unwrap().screen());
            let control = cx.debug_bounds(id).unwrap();
            cx.simulate_click(control.center(), gpui::Modifiers::none());
            assert!(cx.update(|window, app| view.read(app).documents_focus.as_ref().unwrap().is_focused(window)), "{id} must move focus away from terminal");
            cx.update(|_, app| app.write_to_clipboard(gpui::ClipboardItem::new_string("SHELL_MUST_NOT_RECEIVE".into())));
            cx.simulate_keystrokes("x cmd-v");
            std::thread::sleep(Duration::from_millis(100));
            assert_eq!(cx.read(|app| view.read(app).terminal.as_ref().unwrap().screen()), before, "{id} allowed typing or paste into terminal");
        }
        cx.simulate_resize(gpui::size(px(420.), px(650.)));
        cx.simulate_keystrokes("cmd-3");
        assert!(cx.update(|window, app| view.read(app).documents_focus.as_ref().unwrap().is_focused(window)));
        let before = cx.read(|app| view.read(app).terminal.as_ref().unwrap().screen());
        cx.simulate_keystrokes("x cmd-v");
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(cx.read(|app| view.read(app).terminal.as_ref().unwrap().screen()), before, "narrow document view leaked input to terminal");
        cx.simulate_keystrokes("cmd-2");
        assert!(cx.update(|window, app| view.read(app).input_focus.as_ref().unwrap().is_focused(window)));
    }

    #[gpui::test]
    fn opening_documents_focuses_the_pane_and_closing_restores_session_input(
        cx: &mut gpui::TestAppContext,
    ) {
        let socket =
            std::env::temp_dir().join(format!("gui-documents-focus-{}", std::process::id()));
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.preview_busy = true;
            gui.sessions = vec![row("one")];
            gui.documents_socket = Some(socket.clone());
            gui
        });
        let session = cx.debug_bounds("session-0").unwrap();
        cx.simulate_click(session.center(), gpui::Modifiers::none());
        let toggle = cx.debug_bounds("documents-toggle").unwrap();
        cx.simulate_click(toggle.center(), gpui::Modifiers::none());
        let focused = cx.update(|window, app| {
            view.read(app)
                .documents_focus
                .as_ref()
                .is_some_and(|focus| focus.is_focused(window))
        });
        assert!(focused, "opening documents must focus the pane");
        cx.simulate_keystrokes("down");
        let keys = cx.read(|app| {
            view.read(app)
                .documents
                .as_ref()
                .map_or(0, |workspace| workspace.key_events())
        });
        assert_eq!(keys, 1, "a keystroke must reach the focused pane");
        let toggle = cx.debug_bounds("documents-toggle").unwrap();
        cx.simulate_click(toggle.center(), gpui::Modifiers::none());
        let (open, input_focused) = cx.update(|window, app| {
            let gui = view.read(app);
            (
                gui.documents.is_some(),
                gui.input_focus
                    .as_ref()
                    .is_some_and(|focus| focus.is_focused(window)),
            )
        });
        assert!(!open);
        assert!(input_focused, "closing must return focus to session input");
        let line = cx.debug_bounds("line-input").unwrap();
        cx.simulate_click(line.center(), gpui::Modifiers::none());
        cx.simulate_keystrokes("x");
        assert_eq!(cx.read(|app| view.read(app).input_text.clone()), "x");
    }

    #[gpui::test]
    fn attach_confirmation_targets_the_selected_session(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.preview_busy = true;
            gui.sessions = vec![row("one")];
            gui
        });
        let bounds = cx.debug_bounds("session-0").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        let attach = cx.debug_bounds("attach-terminal").unwrap();
        cx.simulate_click(attach.center(), gpui::Modifiers::none());
        assert!(cx.debug_bounds("attach-confirm").is_some());
        assert!(cx.debug_bounds("confirm-attach").is_some());
        let state = cx.read(|app| {
            let gui = view.read(app);
            (
                gui.attach_confirm.clone().map(|s| s.name),
                gui.selection.clone().map(|s| s.name),
            )
        });
        assert_eq!(state, (Some("one".into()), Some("one".into())));
        let cancel = cx.debug_bounds("cancel-attach").unwrap();
        cx.simulate_click(cancel.center(), gpui::Modifiers::none());
        let (confirm, status) = cx.read(|app| {
            let gui = view.read(app);
            (gui.attach_confirm.is_some(), gui.terminal_status.clone())
        });
        assert!(!confirm);
        assert_eq!(status, "Attach cancelled.");
        let terminal_open = cx.read(|app| view.read(app).terminal.is_some());
        assert!(!terminal_open);
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
        let root = std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap())
            .parent()
            .unwrap()
            .to_owned();
        assert_eq!(
            std::env::var_os("XDG_STATE_HOME").map(std::path::PathBuf::from),
            Some(root.join("state")),
            "test state must be isolated"
        );
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
        assert!(cx.read(|app| view.read(app).selected_peer.is_none()));
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
        if std::env::var_os("REMUDA_GUI_ACCEPTANCE_INPUT").is_some() {
            let row = cx.debug_bounds("session-0").unwrap();
            cx.simulate_click(row.center(), gpui::Modifiers::none());
            cx.run_until_parked();
            assert!(cx.debug_bounds("line-input").is_some(), "line input visible");
            assert!(cx.update(|window, app| view.read(app).input_focus.as_ref().is_some_and(|f| f.is_focused(window))), "clicking a current session must focus its input");
            cx.simulate_keystrokes("i->i d->d enter");
            let deadline = std::time::Instant::now() + Duration::from_secs(12);
            loop {
                view.update(cx, |gui, cx| {
                    gui.drain();
                    cx.notify();
                });
                cx.run_until_parked();
                if cx.read(|app| {
                    view.read(app).preview.as_deref().is_some_and(|s| {
                        s.lines().any(|line| line.trim_start().starts_with("uid="))
                    })
                }) {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "input was not observed in session output"
                );
                std::thread::sleep(Duration::from_millis(150));
            }
            assert_eq!(cx.read(|app| view.read(app).input_status.clone()), "Sent");
            let selected = cx.read(|app| view.read(app).selection.clone().unwrap());
            let before = cx.read(|app| view.read(app).sync_version);
            let mut external = remuda_native::strict_input::StrictInput::new(
                remuda_native::strict_input::LocalInput(remuda_native::daemon::socket_path(
                    "default",
                )),
                selected.name.clone(),
                selected.instance.clone(),
            )
            .unwrap();
            assert_eq!(
                external.send(b"printf 'external-proof\\n'\r"),
                remuda_native::strict_input::InputResult::Applied
            );
            let deadline = std::time::Instant::now() + Duration::from_secs(12);
            loop {
                view.update(cx, |gui, cx| {
                    gui.drain();
                    cx.notify();
                });
                cx.run_until_parked();
                if cx.read(|app| {
                    let gui = view.read(app);
                    gui.sync_version > before
                        && gui
                            .preview
                            .as_deref()
                            .is_some_and(|s| s.lines().any(|line| line.trim() == "external-proof"))
                }) {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "external output did not reach the GUI through Sync"
                );
                std::thread::sleep(Duration::from_millis(150));
            }
            // Drive the GUI's attach button and PTY with the isolated shell.
            let button = cx
                .debug_bounds("attach-terminal")
                .expect("attach button visible");
            cx.simulate_click(button.center(), gpui::Modifiers::none());
            let confirm = cx
                .debug_bounds("confirm-attach")
                .expect("attach confirmation visible");
            cx.simulate_click(confirm.center(), gpui::Modifiers::none());
            let deadline = std::time::Instant::now() + Duration::from_secs(12);
            loop {
                view.update(cx, |gui, cx| {
                    gui.drain();
                    cx.notify();
                });
                cx.run_until_parked();
                let attached = matches!(remuda_native::client::request(&remuda_native::daemon::socket_path("default"), &remuda_core::protocol::Request::List), Ok(remuda_core::protocol::Response::Sessions(rows)) if rows.iter().any(|row| row.name == "local-gui-proof" && row.attached));
                if attached {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "terminal did not attach"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            cx.simulate_resize(gpui::size(px(1000.), px(650.)));
            cx.run_until_parked();
            let terminal = cx.debug_bounds("terminal-screen").expect("terminal visible after resize");
            assert!(f32::from(terminal.size.height) >= 300., "terminal must retain usable height at 1000×650: {:?}", terminal.size);
            cx.simulate_click(terminal.center(), gpui::Modifiers::none());
            cx.simulate_keystrokes("h->h o->o s->s t->t n->n a->a m->m e->e enter");
            let hostname = String::from_utf8(
                std::process::Command::new("hostname")
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(12);
            loop {
                view.update(cx, |gui, cx| {
                    gui.drain();
                    cx.notify();
                });
                cx.run_until_parked();
                if cx.read(|app| {
                    view.read(app).terminal.as_ref().is_some_and(|terminal| {
                        terminal
                            .screen()
                            .lines()
                            .any(|line| line.trim() == hostname.trim())
                    })
                }) {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "typed terminal command output not visible"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            cx.update(|_, app| app.write_to_clipboard(gpui::ClipboardItem::new_string("unsafe\x1b[201~".into())));
            cx.simulate_keystrokes("cmd-v");
            assert!(cx.read(|app| view.read(app).terminal_status.contains("unsupported control characters")));
            cx.update(|_, app| app.write_to_clipboard(gpui::ClipboardItem::new_string("printf 'PASTE_PROOF\\n'".into())));
            cx.simulate_keystrokes("cmd-v enter");
            assert!(cx.read(|app| view.read(app).terminal_status.is_empty()));
            let deadline = std::time::Instant::now() + Duration::from_secs(12);
            loop {
                cx.run_until_parked();
                if cx.read(|app| view.read(app).terminal.as_ref().is_some_and(|t| t.screen().lines().any(|line| line.trim() == "PASTE_PROOF"))) { break; }
                assert!(std::time::Instant::now() < deadline, "pasted command output not visible");
                std::thread::sleep(Duration::from_millis(50));
            }
            let (row, col) = cx.read(|app| {
                let frame = view.read(app).terminal.as_ref().unwrap().frame();
                frame.cells.iter().enumerate().find_map(|(row, cells)| {
                    let line: String = cells.iter().map(|cell| cell.text.as_str()).collect();
                    line.find("PASTE_PROOF").map(|col| (row, col))
                }).expect("marker row")
            });
            let bounds = cx.debug_bounds("terminal-screen").unwrap();
            let (cell_w, line_h) = cx.update(|window, _| terminal_metrics(window));
            let point = |c: f32| gpui::point(bounds.left() + px(cell_w * c), bounds.top() + px(line_h * (row as f32 + 0.5)));
            cx.simulate_mouse_down(point(col as f32 + 0.25), gpui::MouseButton::Left, gpui::Modifiers::none());
            cx.simulate_mouse_move(point(col as f32 + 10.25), Some(gpui::MouseButton::Left), gpui::Modifiers::none());
            cx.simulate_mouse_up(point(col as f32 + 10.25), gpui::MouseButton::Left, gpui::Modifiers::none());
            cx.simulate_keystrokes("cmd-c");
            assert_eq!(cx.read(|app| app.read_from_clipboard().and_then(|item| item.text())).as_deref(), Some("PASTE_PROOF"));
            cx.update(|_, app| app.write_to_clipboard(gpui::ClipboardItem::new_string("sleep 30".into())));
            cx.simulate_keystrokes("cmd-v enter");
            std::thread::sleep(Duration::from_millis(500));
            cx.simulate_keystrokes("ctrl-c");
            std::thread::sleep(Duration::from_millis(300));
            cx.update(|_, app| app.write_to_clipboard(gpui::ClipboardItem::new_string("printf 'INTERRUPT_OK\\n'".into())));
            cx.simulate_keystrokes("cmd-v enter");
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                cx.run_until_parked();
                if cx.read(|app| view.read(app).terminal.as_ref().is_some_and(|t| t.screen().lines().any(|line| line.trim() == "INTERRUPT_OK"))) { break; }
                assert!(std::time::Instant::now() < deadline, "Ctrl-C did not interrupt private sleep");
                std::thread::sleep(Duration::from_millis(50));
            }
            // Closing the terminal pane detaches the client, not the session.
            let detach = cx
                .debug_bounds("detach-terminal")
                .expect("detach control visible");
            cx.simulate_click(detach.center(), gpui::Modifiers::none());
            let detach_deadline = std::time::Instant::now() + Duration::from_secs(8);
            loop {
                let rows = match remuda_native::client::request(
                    &remuda_native::daemon::socket_path("default"),
                    &remuda_core::protocol::Request::List,
                ) {
                    Ok(remuda_core::protocol::Response::Sessions(rows)) => rows,
                    _ => panic!("daemon list failed"),
                };
                if let Some(row) = rows.iter().find(|row| row.name == "local-gui-proof") {
                    if !row.attached {
                        assert!(row.alive, "detaching must not end the session process");
                        break;
                    }
                }
                assert!(
                    std::time::Instant::now() < detach_deadline,
                    "detach did not release the terminal"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            assert!(cx.read(|app| view.read(app).terminal.is_none()));
            // The daemon must reject a stale row with the wrong instance.
            view.update(cx, |gui, cx| {
                let mut stale = gui.sessions[0].clone();
                stale.instance_id = Some("wrong-instance".into());
                gui.sessions = vec![stale.clone()];
                gui.select_session(stale);
                gui.input_text = "id".into();
                gui.submit_local_line();
                cx.notify();
            });
            let detach_deadline = std::time::Instant::now() + Duration::from_secs(8);
            loop {
                let attached = matches!(remuda_native::client::request(&remuda_native::daemon::socket_path("default"), &remuda_core::protocol::Request::List), Ok(remuda_core::protocol::Response::Sessions(rows)) if rows.iter().any(|row| row.name == "local-gui-proof" && row.attached));
                if !attached {
                    break;
                }
                assert!(
                    std::time::Instant::now() < detach_deadline,
                    "selection change did not release terminal"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                view.update(cx, |gui, cx| {
                    gui.drain();
                    cx.notify();
                });
                if cx.read(|app| view.read(app).input_stopped) {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "wrong instance was not rejected"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            view.update(cx, |gui, _| {
                assert_eq!(gui.input_warnings.len(), 1);
                gui.input_text = "id".into();
                gui.submit_local_line();
                assert!(gui.input_helper.is_none());
            });
        }
    }
    fn payload(text: &str) -> preview::PreviewPayload {
        preview::PreviewPayload {
            text: text.into(),
            ..Default::default()
        }
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
    fn expanding_a_peer_starts_its_fetch_without_a_label_click() {
        let mut gui = LocalGui::new();
        gui.peers = vec![peer("a", "alpha")];
        gui.remote_busy = true;
        assert!(!gui.expanded_nodes.contains("a"));
        gui.toggle_node("a".into());
        assert!(gui.expanded_nodes.contains("a"));
        assert_eq!(gui.selected_peer.as_deref(), Some("a"));
        assert!(gui.remote_pending.is_some(), "opening a node requests its rows");
    }

    #[test]
    fn selecting_local_node_invalidates_remote_selection_and_pending_work() {
        let mut gui = LocalGui::new();
        gui.peers = vec![peer("a", "alpha")];
        gui.selected_peer = Some("a".into());
        gui.peer_sessions.insert("a".into(), vec![row("a-one")]);
        gui.remote_sessions = vec![row("a-one")];
        gui.remote_pending = Some((1, 0, "a".into()));
        gui.selection = Some(Selection {
            peer: Some("a".into()),
            name: "a-one".into(),
            instance: "instance".into(),
        });
        gui.preview = Some("stale".into());
        gui.select_node(String::new());
        assert!(gui.selected_peer.is_none());
        assert!(gui.selection.is_none());
        assert!(gui.preview.is_none());
        assert!(gui.remote_pending.is_none());
        assert!(gui.remote_sessions.is_empty());
    }

    #[test]
    fn peer_selection_never_falls_back_to_another_peers_rows() {
        let mut gui = LocalGui::new();
        gui.peers = vec![peer("a", "same"), peer("b", "same")];
        gui.selected_peer = Some("b".into());
        gui.remote_sessions = vec![row("same")];
        gui.peer_sessions.insert("a".into(), vec![row("same")]);
        let selection = Selection {
            peer: Some("b".into()),
            name: "same".into(),
            instance: "instance".into(),
        };
        assert!(
            !gui.selection_current(&selection),
            "missing b cache must fail closed even while a has the same row"
        );
        gui.peer_sessions.insert("b".into(), vec![row("same")]);
        assert!(gui.selection_current(&selection));
        gui.peers[1].admitted = false;
        assert!(
            !gui.selection_current(&selection),
            "a revoked peer's cache is not current"
        );
        gui.peers[1].admitted = true;
        gui.selected_peer = Some("b".into());
        gui.remote_error = true;
        assert!(
            !gui.selection_current(&selection),
            "a failed selected peer is not current"
        );
    }

    #[test]
    fn peer_selection_requires_unique_current_full_fingerprint() {
        let mut gui = LocalGui::new();
        gui.remote_busy = true;
        gui.peers = vec![peer("full-a", "same"), peer("full-b", "same")];
        gui.select_peer("full-a".into());
        assert_eq!(gui.selected_peer.as_deref(), Some("full-a"));
        gui.nodes_loading = true;
        gui.select_peer("full-b".into());
        assert_eq!(gui.selected_peer.as_deref(), Some("full-a"));
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
    fn late_peer_reply_is_discarded_when_directory_or_selection_changes() {
        let mut gui = LocalGui::new();
        gui.peers = vec![peer("a", "a"), peer("b", "b")];
        gui.remote_busy = true;
        gui.select_peer("a".into());
        let (old, directory, fp) = gui.remote_pending.take().unwrap();
        gui.select_peer("b".into());
        assert!(gui.remote_sessions.is_empty());
        gui.remote_pending = None;
        gui.remote_sender
            .send((old, directory, fp, Ok(vec![row("wrong")])))
            .unwrap();
        gui.drain();
        assert!(gui.remote_sessions.is_empty());
        gui.remote_busy = true;
        gui.select_peer("b".into());
        let (old, directory, fp) = gui.remote_pending.take().unwrap();
        gui.nodes_busy = true;
        gui.refresh_nodes();
        gui.nodes_pending = false;
        gui.remote_sender
            .send((old, directory, fp, Ok(vec![row("wrong")])))
            .unwrap();
        gui.drain();
        assert!(gui.remote_sessions.is_empty());
        assert!(gui.remote_error);
    }
    #[test]
    fn peer_failure_invalidates_cached_rows_and_exact_instance_identity() {
        let mut gui = LocalGui::new();
        gui.peers = vec![peer("a", "a")];
        gui.remote_busy = true;
        gui.select_peer("a".into());
        let (generation, directory, fp) = gui.remote_pending.take().unwrap();
        gui.remote_sender
            .send((generation, directory, fp, Ok(vec![row("proof")])))
            .unwrap();
        gui.drain();
        assert_eq!(
            gui.remote_sessions[0].instance_id.as_deref(),
            Some("instance")
        );
        gui.remote_busy = true;
        gui.refresh_peer();
        let (generation, directory, fp) = gui.remote_pending.take().unwrap();
        gui.remote_sender
            .send((generation, directory, fp, Err("offline".into())))
            .unwrap();
        gui.drain();
        assert!(gui.remote_error && !gui.remote_loading);
        assert!(
            gui.remote_sessions.is_empty(),
            "a failed refresh invalidates that peer's cache instead of serving stale rows"
        );
        assert!(!gui.peer_sessions.contains_key("a"));
    }
    #[test]
    fn nodes_refresh_failure_hides_cached_peer_children() {
        let mut gui = LocalGui::new();
        gui.peers = vec![peer("a", "a")];
        gui.expanded_nodes.insert("a".into());
        gui.peer_sessions.insert("a".into(), vec![row("old")]);
        gui.selected_peer = Some("a".into());
        gui.remote_sessions = vec![row("old")];
        gui.preview = Some("old".into());
        gui.preview_cells = Some(vec![vec![Default::default()]]);
        gui.nodes_busy = true;
        gui.nodes_generation = 3;
        gui.nodes_sender
            .send((3, Err("offline".into())))
            .unwrap();
        gui.drain();
        assert!(gui.nodes_error);
        assert!(
            gui.peer_sessions.is_empty(),
            "a failed node refresh must not leave old peer children visible"
        );
        assert!(gui.remote_sessions.is_empty());
        assert!(gui.preview.is_none());
        assert!(gui.selected_peer.is_none());
        assert!(
            !gui.tree_entries()
                .iter()
                .any(|entry| matches!(entry, TreeEntry::Session(Some(fp), _) if fp == "a")),
            "the stale children are hidden from tree navigation too"
        );
    }

    #[test]
    fn idle_drain_with_documents_open_reports_no_change() {
        let mut gui = LocalGui::new();
        let mut workspace = documents::Workspace::new(std::path::PathBuf::from("/nonexistent-vfs.sock"));
        workspace.active_mut().cancel();
        gui.documents = Some(workspace);
        assert!(
            !gui.drain(),
            "an idle document pane must not repaint the preview every tick"
        );
    }

    #[gpui::test]
    fn empty_or_zero_width_preview_cells_render_the_text_fallback(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.preview = Some("plain fallback".into());
            gui.preview_cells = Some(Vec::new());
            gui
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("preview-text").is_some());
        view.update(cx, |gui, cx| {
            gui.preview_cells = Some(vec![Vec::new()]);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("preview-text").is_some(),
            "a zero-column grid is a text fallback, not a slice panic"
        );
    }

    #[test]
    fn queued_peer_work_is_not_dispatched_after_revocation_or_duplicate() {
        for duplicate in [false, true] {
            let mut gui = LocalGui::new();
            gui.peers = vec![peer("a", "a")];
            gui.remote_busy = true;
            gui.select_peer("a".into());
            if duplicate {
                gui.peers.push(peer("a", "duplicate"));
            } else {
                gui.peers[0].admitted = false;
            }
            gui.remote_sender
                .send((0, 0, "old".into(), Ok(vec![])))
                .unwrap();
            gui.drain();
            assert!(!gui.remote_busy && gui.remote_pending.is_none());
            assert!(gui.remote_sessions.is_empty());
        }
    }
    #[gpui::test]
    fn preview_render_is_bounded_and_reports_hidden_rows(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.preview_cells = Some(
                (0usize..201)
                    .map(|row| {
                        (0..200)
                            .map(|col| remuda_core::agent::StyledCell {
                                text: if (row + col).is_multiple_of(2) {
                                    "x".into()
                                } else {
                                    " ".into()
                                },
                                ..Default::default()
                            })
                            .collect()
                    })
                    .collect(),
            );
            gui
        });
        cx.run_until_parked();
        let (rows, expected) = cx.read(|app| { let gui = view.read(app); (gui.preview_frame().unwrap().rows as usize, gui.preview_visible_rows(100)) });
        assert!(rows > 0 && rows <= 100 && rows == expected, "visible rows must respect both viewport and 20,000-cell bound");
        assert!(cx.debug_bounds("preview-row-0").is_some(), "at least one visible row is rendered");
        assert!(
            cx.debug_bounds("preview-row-100").is_none(),
            "rows beyond the render bound are not created"
        );
        assert!(
            cx.debug_bounds("preview-truncated").is_some(),
            "the hidden rows are disclosed, not silently dropped"
        );
        let _ = view;
    }

    #[gpui::test]
    fn peer_rows_render_and_local_navigation_restores_local_rows(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.remote_busy = true;
            gui.peers = vec![peer("a", "a")];
            gui.sessions = vec![row("local")];
            gui
        });
        let peer = cx.debug_bounds("node-peer-0").unwrap();
        cx.simulate_click(peer.center(), gpui::Modifiers::none());
        view.update(cx, |gui, cx| {
            gui.remote_sessions = vec![row("remote-1"), row("remote-2")];
            gui.remote_loading = false;
            cx.notify();
        });
        assert!(
            cx.debug_bounds("session-1").is_some(),
            "both remote rows rendered"
        );
        let local = cx.debug_bounds("node-local").unwrap();
        cx.simulate_click(local.center(), gpui::Modifiers::none());
        assert!(cx.debug_bounds("session-0").is_some());
        assert!(cx.read(|app| view.read(app).selected_peer.is_none()));
        assert_eq!(
            cx.read(|app| view.read(app).sessions[0].name.clone()),
            "local"
        );
    }
    #[test]
    fn node_replies_reject_old_generations_and_mark_errors_stale() {
        let mut gui = LocalGui::new();
        gui.peers = vec![peer("full-a", "first")];
        gui.selected_peer = Some("full-a".into());
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
        assert!(gui.selected_peer.is_none());
        assert!(!gui.peer_current("full-a"));
    }
    fn row(name: &str) -> Session {
        Session {
            name: name.into(),
            instance_id: Some("instance".into()),
            alive: true,
            unavailable: false,
            attached: false,
            depth: 0,
            state: None,
            detail: None,
            structured: true,
        }
    }
    fn summary_for_view(name: &str, instance: &str) -> remuda_core::SessionSummary {
        remuda_core::SessionSummary {
            id: format!("id-{name}"), name: name.into(), instance_id: Some(instance.into()), output_version: None,
            alive: true, idle: Duration::ZERO, output_idle: Some(Duration::ZERO), size: remuda_core::agent::Size::new(80,24),
            attached: false, human_idle: None, mouse_tracking: false, unavailable: false, program_status: None,
            program_status_age_ms: None, remote_input_disabled: false, team: None,
        }
    }
    fn sample_semantic_view() -> remuda_core::session_view::SessionView {
        use remuda_core::session_view::{SessionView,ViewEntry,ViewSemantics,ViewState};
        let a=summary_for_view("parent","p-1"); let b=summary_for_view("child","c-1");
        SessionView::new(vec![b,a],1,vec![
            ViewEntry { name:"parent".into(), instance_id:"p-1".into(), lines:vec!["parent".into()] },
            ViewEntry { name:"child".into(), instance_id:"c-1".into(), lines:vec!["  child".into()] },
        ]).with_semantics(vec![
            ViewSemantics { name:"parent".into(), instance_id:"p-1".into(), depth:0, state:Some(ViewState::Idle), detail:None },
            ViewSemantics { name:"child".into(), instance_id:"c-1".into(), depth:1, state:Some(ViewState::Working), detail:Some("codex · 42%".into()) },
        ])
    }
    #[test]
    fn semantic_rows_bind_depth_state_and_detail() {
        use remuda_core::session_view::ViewState;
        let rows = sessions_from_view(sample_semantic_view()).unwrap();
        assert_eq!((rows[0].name.as_str(),rows[0].instance_id.as_deref()),("parent",Some("p-1")));
        assert_eq!((rows[1].name.as_str(),rows[1].instance_id.as_deref()),("child",Some("c-1")));
        assert_eq!((rows[0].depth, rows[0].state), (0, Some(ViewState::Idle)));
        assert_eq!((rows[1].depth, rows[1].state), (1, Some(ViewState::Working)));
        assert_eq!(rows[1].detail.as_deref(), Some("codex · 42%"));
        assert!(rows.iter().all(|row| row.structured));
    }
    #[test]
    fn flat_session_list_has_no_hierarchy_or_detail() {
        let rows = plain_sessions(vec![summary_for_view("child","c-1")]);
        assert_eq!((rows[0].depth, rows[0].state, rows[0].structured), (0, None, false));
        assert!(rows[0].detail.is_none());
    }
    #[test]
    fn semantic_markers_follow_state_with_alive_authority() {
        use remuda_core::session_view::ViewState;
        let mut s = row("a");
        assert_eq!(session_marker(&s), remuda_native::cluster_tui::session_glyph(true));
        s.state = Some(ViewState::Working); assert_eq!(session_marker(&s), '🏇');
        s.state = Some(ViewState::Idle); assert_eq!(session_marker(&s), '○');
        s.state = Some(ViewState::Blocked); assert_eq!(session_marker(&s), '!');
        s.alive = false; s.state = Some(ViewState::Working);
        assert_eq!(session_marker(&s), remuda_native::cluster_tui::session_glyph(false));
        s.unavailable = true; assert_eq!(session_marker(&s), '?');
    }
    #[gpui::test]
    fn semantic_hierarchy_and_detail_render_without_parsing_ansi(cx: &mut gpui::TestAppContext) {
        let (view,cx)=cx.add_window_view(|_,_| {let mut gui=LocalGui::new();gui.preview_busy=true;gui.sessions=sessions_from_view(sample_semantic_view()).unwrap();gui});
        cx.simulate_resize(gpui::size(px(1000.),px(650.)));cx.run_until_parked();
        assert!(cx.debug_bounds("local-hierarchy-fallback").is_none(), "structured rows need no fallback notice");
        let parent=cx.debug_bounds("session-0-name").unwrap();
        let child=cx.debug_bounds("session-1-name").unwrap();
        assert!(child.left() > parent.left(), "depth must indent the child: {child:?} {parent:?}");
        assert!(cx.debug_bounds("session-0-detail").is_none());
        assert!(cx.debug_bounds("session-1-detail").is_some(), "validated detail must render");
        let row=cx.debug_bounds("session-1").unwrap();
        cx.simulate_click(row.center(),gpui::Modifiers::none());
        assert!(view.read_with(cx,|gui,_| gui.selection.as_ref().is_some_and(|s|s.name=="child" && s.instance=="c-1")));
    }
    #[test]
    fn tree_session_at_never_uses_local_rows_for_a_peer() {
        let mut gui = LocalGui::new();
        gui.sessions = vec![row("local")];
        gui.peers = vec![peer("p", "Peer")];
        gui.expanded_nodes.insert("p".into());
        assert!(gui.tree_session_at(Some("p"), 0).is_none(), "cache miss must refuse");
        gui.selected_peer = Some("p".into());
        gui.remote_sessions = vec![row("remote")];
        assert_eq!(gui.tree_session_at(Some("p"), 0).unwrap().name, "remote");
        gui.peer_sessions.insert("p".into(), vec![row("cached")]);
        assert_eq!(gui.tree_session_at(Some("p"), 0).unwrap().name, "cached");
        assert_eq!(gui.tree_session_at(None, 0).unwrap().name, "local");
    }
    #[test]
    fn sidebar_child_indices_match_conditional_rows() {
        let mut gui = LocalGui::new();
        gui.sessions = plain_sessions(vec![summary_for_view("flat","f-1")]);
        gui.peers = vec![peer("p","Peer")];
        gui.expanded_nodes.insert("p".into());
        gui.peer_sessions.insert("p".into(), plain_sessions(vec![summary_for_view("r","r-1")]));
        assert_eq!(gui.tree_entries().len(), gui.sidebar_child_indices().len());
        // title+label 0-1, local node 2, local note+row, peer node, peer note+row
        assert_eq!(gui.sidebar_child_indices(), vec![2, 4, 5, 7]);
    }
    #[cfg(unix)]
    #[test]
    fn peer_budget_gives_up_before_the_worker_watchdog() {
        assert!(PEER_REQUEST_BUDGET < provider::TIMEOUT);
        let started = std::time::Instant::now();
        let result = bounded_call(Duration::from_millis(150), || {
            std::thread::sleep(Duration::from_secs(5));
            7
        });
        assert!(result.is_none(), "a slow peer must not hold the worker");
        assert!(started.elapsed() < Duration::from_secs(2));
    }
    #[test]
    fn bounded_request_gives_up_before_the_worker_watchdog() {
        use std::os::unix::net::UnixListener;
        let root = if std::path::Path::new("/private/tmp").is_dir() {
            std::path::PathBuf::from("/private/tmp")
        } else {
            std::env::temp_dir()
        };
        let dir = root.join(format!("gui-bound-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("silent.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            if let Ok((_stream, _)) = listener.accept() {
                std::thread::sleep(Duration::from_secs(5));
            }
        });
        let started = std::time::Instant::now();
        assert!(bounded_request(&path, &remuda_core::protocol::Request::List, Duration::from_millis(200)).is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[gpui::test]
    fn keyboard_cursor_is_visible_and_scrolls_into_view(cx: &mut gpui::TestAppContext) {
        let (view,cx)=cx.add_window_view(|_,_| {let mut gui=LocalGui::new();gui.preview_busy=true;gui.sessions=(0..40).map(|i| row(&format!("s{i:02}"))).collect();gui});
        cx.simulate_resize(gpui::size(px(900.),px(300.)));cx.run_until_parked();
        // The reveal index must name the same direct child the tree paints.
        let node_index=cx.read(|app| view.read(app).sidebar_child_indices()[0]);
        let node=cx.debug_bounds("node-local").unwrap();
        let node_item=cx.read(|app| view.read(app).sidebar_scroll.bounds_for_item(node_index)).unwrap();
        assert_eq!(node_item.top(), node.top(), "local node is sidebar child {node_index}");
        cx.update(|window, app| view.read(app).tree_focus.as_ref().unwrap().focus(window));
        let keys=(0..30).map(|_| "down").collect::<Vec<_>>().join(" ");
        cx.simulate_keystrokes(&keys);cx.run_until_parked();
        let cursor=cx.read(|app| view.read(app).tree_cursor);
        assert_eq!(cursor, 30);
        assert!(cx.debug_bounds("session-29-cursor").is_some(), "cursor row needs a visible indicator");
        let row=cx.debug_bounds("session-29").unwrap();
        let sidebar=cx.debug_bounds("sidebar").unwrap();
        assert!(row.top() >= sidebar.top() && row.bottom() <= sidebar.bottom(), "keyboard cursor must be revealed: {row:?} {sidebar:?}");
        // The cursor's reveal index names the cursor row's direct child slot.
        let cursor_index=cx.read(|app| view.read(app).sidebar_child_indices()[cursor]);
        let cursor_item=cx.read(|app| view.read(app).sidebar_scroll.bounds_for_item(cursor_index)).unwrap();
        let cursor_row=cx.debug_bounds("session-29").unwrap();
        let offset=cx.read(|app| view.read(app).sidebar_scroll.offset()).y;
        assert_eq!(cursor_item.top(), cursor_row.top()-offset, "cursor row is sidebar child {cursor_index}");
        let before=cx.read(|app| view.read(app).sidebar_scroll.offset());
        cx.simulate_event(gpui::ScrollWheelEvent { position: sidebar.center(), delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.),px(-200.))), ..Default::default() });
        cx.run_until_parked();
        let after=cx.read(|app| view.read(app).sidebar_scroll.offset());
        assert_ne!(before, after, "wheel must scroll the sidebar");
    }
    #[gpui::test]
    fn session_and_node_rows_stay_on_one_line(cx: &mut gpui::TestAppContext) {
        let (view,cx)=cx.add_window_view(|_,_| { let mut gui=LocalGui::new(); gui.preview_busy=true;
            gui.sessions=vec![row("a-session-name-long-enough-to-compete-with-status"), row("short")];
            gui.peers=vec![peer("peer-fingerprint","Peer Label")];
            gui.expanded_nodes.insert("peer-fingerprint".into());
            gui.peer_sessions.insert("peer-fingerprint".into(), vec![row("remote")]);
            gui.select_session(gui.sessions[0].clone()); gui });
        cx.simulate_resize(gpui::size(px(1000.),px(650.)));cx.run_until_parked();
        let name=cx.debug_bounds("session-0-text").unwrap();
        let status=cx.debug_bounds("session-0-status").unwrap();
        assert!(name.top() < status.bottom() && status.top() < name.bottom(), "name and status share a line: {name:?} {status:?}");
        assert!(name.size.width >= px(56.), "the name keeps a readable width: {name:?}");
        let selected_row=cx.debug_bounds("session-0").unwrap();
        let plain_row=cx.debug_bounds("session-1").unwrap();
        assert!(selected_row.size.height < px(48.), "one text line per session row: row={selected_row:?} name={:?} text={:?} status={:?} plain={plain_row:?}", cx.debug_bounds("session-0-name"), cx.debug_bounds("session-0-text"), cx.debug_bounds("session-0-status"));
        assert_eq!(selected_row.size.height, plain_row.size.height, "selection must not add a banner line");
        assert!(cx.debug_bounds("selected-session").is_some());
        let node=cx.debug_bounds("node-peer-0").unwrap();
        assert!(node.size.height < px(48.), "peer node must be one line: {node:?}");
        let label=cx.debug_bounds("node-peer-label-0").unwrap();
        let state=cx.debug_bounds("node-peer-state-0").unwrap();
        assert!(label.top() < state.bottom() && state.top() < label.bottom(), "node name and state share a line");
        let node_name=cx.debug_bounds("node-peer-label-0").unwrap();
        assert!(node_name.size.width >= px(56.), "peer label keeps a readable width: {node_name:?}");
    }
    #[gpui::test]
    fn narrow_long_session_name_keeps_status_inside_the_sidebar(cx: &mut gpui::TestAppContext) {
        let long="긴이름".repeat(30);
        let (view,cx)=cx.add_window_view(|_,_| { let mut gui=LocalGui::new(); gui.preview_busy=true;
            gui.sessions=vec![row(&long)]; gui.narrow_view=NarrowView::Nodes; gui });
        cx.simulate_resize(gpui::size(px(420.),px(600.)));cx.run_until_parked();
        let sidebar=cx.debug_bounds("sidebar").unwrap();
        let name=cx.debug_bounds("session-0-text").unwrap();
        let status=cx.debug_bounds("session-0-status").unwrap();
        assert!(name.size.width > px(0.), "long name keeps a visible slot");
        assert!(name.right() <= sidebar.right() + px(1.) && status.right() <= sidebar.right() + px(1.), "name/status must stay in the sidebar: {name:?} {status:?} {sidebar:?}");
        assert!(status.left() >= name.left());
    }

    #[gpui::test]
    fn narrow_tree_indent_keeps_levels_visible(cx: &mut gpui::TestAppContext) {
        let (view,cx)=cx.add_window_view(|_,_| {let mut gui=LocalGui::new();gui.preview_busy=true;gui.sessions=sessions_from_view(nested_view()).unwrap();gui.narrow_view=NarrowView::Nodes;gui});
        cx.simulate_resize(gpui::size(px(420.),px(600.)));cx.run_until_parked();
        let parent=cx.debug_bounds("session-0-name").unwrap();
        let child=cx.debug_bounds("session-1-name").unwrap();
        let grand=cx.debug_bounds("session-2-name").unwrap();
        let step=f32::from(child.left()-parent.left());
        assert!((step-TREE_INDENT_PX).abs() < 0.5, "one level must be {TREE_INDENT_PX}px, got {step}");
        assert!((f32::from(grand.left()-child.left())-TREE_INDENT_PX).abs() < 0.5);
        for name in [parent, child, grand] {
            assert!(name.size.width > px(0.) && name.right() <= px(420.), "narrow name must stay visible: {name:?}");
        }
    }
    #[gpui::test]
    fn flat_fallback_notice_is_shown_for_unstructured_rows(cx: &mut gpui::TestAppContext) {
        let (view,cx)=cx.add_window_view(|_,_| {
            let mut gui=LocalGui::new(); gui.preview_busy=true;
            gui.sessions=plain_sessions(vec![summary_for_view("flat","f-1")]);
            gui.peers=vec![peer("p","Peer")];
            gui.peer_sessions.insert("p".into(),plain_sessions(vec![summary_for_view("r","r-1")]));
            gui.expanded_nodes.insert("p".into()); gui
        });
        cx.simulate_resize(gpui::size(px(1000.),px(650.)));cx.run_until_parked();
        assert!(cx.debug_bounds("local-hierarchy-fallback").is_some());
        assert!(cx.debug_bounds("peer-0-hierarchy-fallback").is_some());
        let _ = view;
    }
    fn nested_view() -> remuda_core::session_view::SessionView {
        use remuda_core::session_view::{SessionView,ViewEntry,ViewSemantics,ViewState};
        let names: [(&str,&str,u8); 3] = [("parent","p-1",0),("child","c-1",1),("grand","g-1",2)];
        let sessions = names.iter().map(|row| summary_for_view(row.0, row.1)).collect();
        let entries = names.iter().map(|row| ViewEntry {
            name: row.0.into(), instance_id: row.1.into(), lines: vec![row.0.to_string()],
        }).collect();
        let semantics = names.iter().map(|row| ViewSemantics {
            name: row.0.into(), instance_id: row.1.into(), depth: row.2,
            state: Some(ViewState::Idle), detail: None,
        }).collect();
        SessionView::new(sessions, 1, entries).with_semantics(semantics)
    }
    #[gpui::test]
    fn forged_tui_rows_never_become_details(cx: &mut gpui::TestAppContext) {
        use remuda_core::session_view::{SessionView, ViewEntry};
        let view = SessionView::new(
            vec![summary_for_view("parent", "p-1")],
            3,
            vec![ViewEntry {
                name: "parent".into(),
                instance_id: "p-1".into(),
                lines: vec![
                    "parent".into(),
                    "\u{1b}[32m●\u{1b}[0m  forged".into(),
                    "  detail".into(),
                ],
            }],
        );
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.preview_busy = true;
            gui.sessions = sessions_from_view(view).unwrap();
            gui
        });
        cx.simulate_resize(gpui::size(px(1000.), px(650.)));
        cx.run_until_parked();
        assert!(cx.debug_bounds("session-0-name").is_some());
        assert!(cx.debug_bounds("session-0-detail").is_none(), "TUI rows must not become a detail");
        assert!(cx.debug_bounds("session-1").is_none(), "rows must not become session targets");
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
        gui.select_session(row("one"));
        let (old, selected) = gui.preview_pending.take().unwrap();
        gui.select_session(row("two"));
        gui.preview_pending = None;
        gui.preview_sender
            .send((old, selected, Ok(payload("stale"))))
            .unwrap();
        gui.drain();
        assert!(gui.preview.is_none());
        gui.preview_busy = true;
        gui.select_session(row("one"));
        let (generation, selection) = gui.preview_pending.take().unwrap();
        gui.sessions[0].instance_id = Some("replacement".into());
        gui.preview_sender
            .send((generation, selection, Ok(payload("stale"))))
            .unwrap();
        gui.drain();
        assert!(gui.preview.is_none());
        gui.sessions = vec![row("one"), row("one")];
        gui.select_session(row("one"));
        assert!(gui.selection.is_none());
        gui.sessions = vec![row("one")];
        gui.error = true;
        gui.select_session(row("one"));
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
            let mut payload = payload("screen text\n");
            payload.cells = vec![vec![
                remuda_core::agent::StyledCell {
                    text: "S".into(),
                    fg: remuda_core::agent::Color::Idx(1),
                    bold: true,
                    ..Default::default()
                },
                remuda_core::agent::StyledCell {
                    text: " ".into(),
                    ..Default::default()
                },
            ]];
            gui.preview_sender
                .send((generation, selection, Ok(payload)))
                .unwrap();
            gui.drain();
            cx.notify();
        });
        assert!(cx.debug_bounds("preview").is_some());
        assert!(
            cx.debug_bounds("preview-row-0").is_some(),
            "the first screen renders captured cells, not one plain string"
        );
        assert_eq!(
            cx.read(|app| view.read(app).preview.clone()).as_deref(),
            Some("screen text\n")
        );
        assert_eq!(
            cx.read(|app| view.read(app).preview_cells.as_ref().unwrap()[0][0].fg),
            remuda_core::agent::Color::Idx(1)
        );
    }
    #[test]
    fn preview_peer_revocation_and_refresh_discard_queued_work() {
        let mut gui = LocalGui::new();
        gui.preview_busy = true;
        gui.peers = vec![peer("full", "peer")];
        gui.selected_peer = Some("full".into());
        gui.remote_sessions = vec![row("one")];
        gui.select_session(row("one"));
        gui.peers[0].admitted = false;
        gui.preview_sender
            .send((
                0,
                Selection {
                    peer: None,
                    name: "old".into(),
                    instance: "old".into(),
                },
                Ok(payload("old")),
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
    fn terminal_grid_resizes_with_the_window_and_keeps_cell_frames(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.terminal = Some(std::rc::Rc::new(terminal::Terminal::fixture(
                4,
                12,
                b"\x1b[31mred \x1b[1mbold\x1b[0m\r\n\xf0\x9f\x8f\x87 wide",
            )));
            gui
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("terminal-row-0").is_some());
        assert!(
            cx.read(|app| view.read(app).terminal.as_ref().unwrap().frame().cells[0][0].text == "r"),
            "the frame carries the fixture's first cell"
        );
        cx.simulate_resize(gpui::size(px(640.), px(480.)));
        cx.run_until_parked();
        let small = view.read_with(cx, |gui, _| gui.terminal.as_ref().unwrap().size());
        cx.simulate_resize(gpui::size(px(1400.), px(900.)));
        cx.run_until_parked();
        let wide = view.read_with(cx, |gui, _| gui.terminal.as_ref().unwrap().size());
        assert!(
            wide.1 > small.1,
            "a wider window must give the child more columns: {small:?} -> {wide:?}"
        );
        cx.simulate_resize(gpui::size(px(640.), px(480.)));
        cx.run_until_parked();
        let small_again = view.read_with(cx, |gui, _| gui.terminal.as_ref().unwrap().size());
        assert!(
            small_again.1 < wide.1,
            "a narrower window must give the child fewer columns: {wide:?} -> {small_again:?}"
        );
        let frame = view.read_with(cx, |gui, _| gui.terminal.as_ref().unwrap().frame());
        assert_eq!(frame.cols, small_again.1);
        assert_eq!(frame.rows, small_again.0);
    }

    #[gpui::test]
    fn terminal_cursor_overlays_the_wide_glyph_cell_and_hides_with_the_screen(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.terminal = Some(std::rc::Rc::new(terminal::Terminal::fixture(
                2,
                8,
                "🏇\x1b[1;2H".as_bytes(),
            )));
            gui
        });
        cx.run_until_parked();
        let row = cx.debug_bounds("terminal-row-0").expect("row laid out");
        let cursor = cx
            .debug_bounds("terminal-cursor")
            .expect("cursor overlay present");
        assert_eq!(
            cursor.left(),
            row.left(),
            "a continuation-cell cursor highlights the wide glyph's first cell"
        );
        view.update(cx, |gui, cx| {
            gui.terminal = Some(std::rc::Rc::new(terminal::Terminal::fixture(
                2,
                8,
                b"\x1b[?25lab",
            )));
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            !cx.read(|app| view.read(app).terminal.as_ref().unwrap().frame().cursor_visible),
            "the hidden-cursor screen state is preserved"
        );
    }

    #[gpui::test]
    fn tree_is_keyboard_reachable_and_collapses_with_left(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.peers = vec![peer("a", "alpha")];
            gui.sessions = vec![row("local")];
            gui.peer_sessions.insert("a".into(), vec![row("a-one")]);
            gui
        });
        cx.run_until_parked();
        let sidebar = cx.debug_bounds("sidebar").expect("sidebar laid out");
        cx.simulate_click(sidebar.center(), gpui::Modifiers::none());
        cx.simulate_keystrokes("down down right");
        view.update(cx, |gui, cx| {
            gui.remote_loading = false;
            cx.notify();
        });
        cx.simulate_keystrokes("down enter");
        assert_eq!(
            cx.read(|app| view.read(app).selected_peer.clone()),
            Some("a".into())
        );
        assert_eq!(
            cx.read(|app| view.read(app).selection.as_ref().unwrap().name.clone()),
            "a-one"
        );
        cx.simulate_keystrokes("up left");
        assert!(
            !cx.read(|app| view.read(app).expanded_nodes.contains("a")),
            "left collapses the node under the keyboard cursor"
        );
    }

    #[gpui::test]
    fn tree_expands_several_nodes_and_collapses_one_without_hiding_others(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.peers = vec![peer("a", "alpha"), peer("b", "beta")];
            gui.sessions = vec![row("local")];
            gui.peer_sessions.insert("a".into(), vec![row("a-one")]);
            gui.peer_sessions.insert("b".into(), vec![row("b-one")]);
            gui.expanded_nodes.insert("a".into());
            gui.expanded_nodes.insert("b".into());
            gui
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("peer-0-session-0").is_some());
        assert!(cx.debug_bounds("peer-1-session-0").is_some());
        let local = cx.debug_bounds("session-0").expect("local child stays visible");
        let a = cx.debug_bounds("peer-0-session-0").unwrap();
        let b = cx.debug_bounds("peer-1-session-0").unwrap();
        assert!(local.top() < a.top() && a.top() < b.top(), "tree order");
        let before = cx.debug_bounds("peer-1-session-0").unwrap().top();
        let collapse = cx.debug_bounds("node-toggle-peer-0").expect("peer chevron");
        cx.simulate_click(collapse.center(), gpui::Modifiers::none());
        cx.simulate_resize(gpui::size(px(1200.), px(900.)));
        cx.run_until_parked();
        assert!(
            !cx.read(|app| view.read(app).expanded_nodes.contains("a")),
            "the chevron collapsed peer a"
        );
        let collapsed_top = cx.debug_bounds("peer-1-session-0").unwrap().top();
        assert!(
            collapsed_top < before,
            "the remaining peer moves up when a's children collapse"
        );
        assert!(cx.debug_bounds("session-0").is_some());
        let open = cx.debug_bounds("node-toggle-peer-0").unwrap();
        cx.simulate_click(open.center(), gpui::Modifiers::none());
        cx.simulate_resize(gpui::size(px(1200.), px(900.)));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("peer-1-session-0").unwrap().top() > collapsed_top,
            "reopening a pushes the other peer back down"
        );
        let session = cx.debug_bounds("peer-1-session-0").unwrap();
        cx.simulate_click(session.center(), gpui::Modifiers::none());
        assert_eq!(
            cx.read(|app| view.read(app).selected_peer.clone()),
            Some("b".into())
        );
        assert_eq!(
            cx.read(|app| view.read(app).selection.as_ref().unwrap().name.clone()),
            "b-one"
        );
    }

    #[gpui::test]
    fn node_selection_navigates_locally_and_refuses_stale_peers(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.remote_busy = true;
            gui.peers = vec![peer("full-a", "peer")];
            gui.sessions = vec![row("local")];
            gui
        });
        let button = cx.debug_bounds("node-peer-0").expect("peer rendered");
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        assert_eq!(
            cx.read(|app| view.read(app).selected_peer.clone()),
            Some("full-a".into())
        );
        let local = cx.debug_bounds("node-local").expect("local rendered");
        cx.simulate_click(local.center(), gpui::Modifiers::none());
        assert!(cx.read(|app| view.read(app).selected_peer.is_none()));
        view.update(cx, |gui, cx| {
            gui.nodes_loading = true;
            cx.notify();
        });
        let button = cx.debug_bounds("node-peer-0").unwrap();
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        assert!(cx.read(|app| view.read(app).selected_peer.is_none()));
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
