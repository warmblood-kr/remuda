use gpui::{
    div, prelude::*, px, rgb, size, App, Application, Bounds, Context, FocusHandle, SharedString,
    Window, WindowBounds, WindowOptions,
};
use serde::{Deserialize, Serialize};
use std::{sync::mpsc, time::Duration};

#[allow(dead_code)]
mod input_helper;
use remuda_gui::documents;
mod preview;
mod provider;
mod terminal;

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

fn worker_executable() -> std::io::Result<std::path::PathBuf> {
    #[cfg(test)]
    if let Some(path) = std::env::var_os("REMUDA_GUI_ACCEPTANCE_WORKER") {
        return Ok(path.into());
    }
    std::env::current_exe()
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
type PreviewResult = (u64, Selection, Result<String, String>);
type SyncResult = (u64, Selection, Result<preview::SyncFrame, String>);

type RemoteResult = (u64, u64, String, Result<Vec<Session>, String>);

struct LocalGui {
    documents: Option<documents::Pane>,
    documents_focus: Option<FocusHandle>,
    terminal: Option<terminal::Terminal>,
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
            documents: None,
            documents_focus: None,
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
            remote_error: false,
            remote_loading: false,
            remote_generation: 0,
            remote_busy: false,
            remote_pending: None,
            remote_sender,
            remote_results,
        }
    }
    fn clear_preview(&mut self) {
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
        self.preview_pending = None;
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
            if !self.peer_current(fp) || self.remote_loading || self.remote_error {
                return false;
            }
            &self.remote_sessions
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
    fn attach_selected(&mut self) {
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
        self.terminal = None;
        match worker_executable()
            .map_err(|e| e.to_string())
            .and_then(|path| {
                terminal::Terminal::start(&path, &selection.name, &selection.instance, WORKER_ABI)
            }) {
            Ok(terminal) => {
                self.terminal = Some(terminal);
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
            let peer = selection.peer.as_deref().unwrap_or("");
            let kind = if selection.peer.is_some() {
                "peer"
            } else {
                "local"
            };
            let result = worker_executable()
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
            let kind = if selection.peer.is_some() {
                "peer"
            } else {
                "local"
            };
            let peer = selection.peer.as_deref().unwrap_or("");
            let result = worker_executable()
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
                            "--sync",
                            &since.to_string(),
                            WORKER_ABI,
                        ],
                        provider::TIMEOUT,
                    )
                })
                .and_then(|bytes| read_worker_reply::<preview::SyncFrame>(&bytes));
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
            if self.selected_peer.as_ref() != Some(&fingerprint) {
                self.remote_sessions.clear();
            }
            self.selected_peer = Some(fingerprint);
            self.refresh_peer();
        }
    }
    fn invalidate_remote(&mut self) {
        self.clear_preview();
        self.remote_generation = self
            .remote_generation
            .checked_add(1)
            .expect("generation exhausted");
        self.remote_pending = None;
        self.remote_loading = false;
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
    fn drain(&mut self) {
        if let Some(pane) = &mut self.documents {
            pane.poll();
        }
        if self
            .terminal
            .as_ref()
            .is_some_and(|terminal| terminal.ended())
        {
            self.terminal_status = "Terminal ended. Inspect its output or attach again.".into();
        }
        let input_event = self
            .input_helper
            .as_ref()
            .and_then(|helper| helper.recv_event_timeout(Duration::ZERO).ok());
        if let Some(event) = input_event {
            self.input_helper = None;
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
                        self.sync_version = frame.version;
                        self.preview = Some(frame.text);
                        self.sync_error = false;
                    }
                    _ => {
                        self.preview = None;
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
                self.preview = Some(result.unwrap_or_else(|error| {
                    if error == STALE_HELPER {
                        self.helper_changed = true;
                    }
                    "Preview unavailable; if the GUI was updated, restart it".into()
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
                if self
                    .selected_peer
                    .as_ref()
                    .is_some_and(|fp| !self.peer_current(fp))
                {
                    self.selected_peer = None;
                }
                if !self.nodes_error && self.selected_peer.is_some() {
                    self.refresh_peer();
                }
            }
            self.launch_nodes();
        }
        while let Ok((generation, directory, fp, result)) = self.remote_results.try_recv() {
            self.remote_busy = false;
            if generation == self.remote_generation
                && directory == self.nodes_generation
                && self.selected_peer.as_ref() == Some(&fp)
                && self.peer_current(&fp)
            {
                self.remote_loading = false;
                match result {
                    Ok(rows) => {
                        self.remote_sessions = rows;
                        self.remote_error = false;
                    }
                    Err(error) => {
                        self.helper_changed |= error == STALE_HELPER;
                        self.remote_error = true;
                    }
                }
            }
            self.launch_peer();
        }
    }
}
impl Render for LocalGui {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let input_focus = self
            .input_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        let documents_open = self.documents.is_some();
        let documents_focus = self
            .documents_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        let title = if documents_open {
            "DOCUMENTS"
        } else if self.selected_peer.is_some() && self.remote_loading {
            "PEER SESSIONS · LOADING"
        } else if self.selected_peer.is_some() && self.remote_error {
            "PEER SESSIONS · STALE"
        } else if self.selected_peer.is_some() {
            "PEER SESSIONS"
        } else if self.loading {
            "LOCAL SESSIONS · LOADING"
        } else if self.error {
            "LOCAL SESSIONS · STALE"
        } else {
            "LOCAL SESSIONS"
        };
        div()
            .size_full()
            .flex()
            .bg(rgb(0x101216))
            .text_color(rgb(0xe8eaf0))
            .font_family("Menlo")
            .child(
                div()
                    .w(px(300.))
                    .h_full()
                    .flex()
                    .flex_col()
                    .bg(rgb(0x191d24))
                    .p_4()
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
                            .child("This device")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.invalidate_remote();
                                this.selected_peer = None;
                                cx.notify();
                            })),
                    )
                    .children(self.peers.iter().cloned().enumerate().map(|(index, peer)| {
                        let enabled = self.peer_current(&peer.fingerprint);
                        let fingerprint = peer.fingerprint.clone();
                        let name = label(&peer.label);
                        let identity = label(&peer.fingerprint);
                        let id = SharedString::from(format!("node-{}-{index}", peer.fingerprint));
                        let debug = format!("node-peer-{index}");
                        div()
                            .id(id)
                            .debug_selector(move || debug.clone())
                            .py_2()
                            .cursor_pointer()
                            .child(name)
                            .child(div().text_sm().child(identity))
                            .child(div().text_sm().child(if !peer.admitted {
                                "revoked"
                            } else if enabled {
                                "admitted"
                            } else {
                                "unavailable"
                            }))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.select_peer(fingerprint.clone());
                                cx.notify();
                            }))
                    }))
                    .child(div().pt_4().text_sm().child(title))
                    .children(
                        (if self.selected_peer.is_some() {
                            &self.remote_sessions
                        } else {
                            &self.sessions
                        })
                        .clone()
                        .into_iter()
                        .enumerate()
                        .map(|(index, session)| {
                            let status = if session.unavailable {
                                "unavailable"
                            } else if session.alive {
                                "running"
                            } else {
                                "exited"
                            };
                            let selected = session.clone();
                            let is_selected = self.selection.as_ref().is_some_and(|s| {
                                self.selection_current(s) && s.peer == self.selected_peer && s.name == session.name
                                    && session.instance_id.as_deref() == Some(s.instance.as_str())
                            });
                            div()
                                .id(SharedString::from(format!("session-{index}")))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.select_session(selected.clone());
                                    cx.notify();
                                }))
                                .debug_selector(move || format!("session-{index}"))
                                .py_2()
                                .when(is_selected, |d| d.bg(rgb(0x34445a)).child(div().id("selected-session").debug_selector(|| "selected-session".into()).child("SELECTED")))
                                .child(div().child(label(&session.name)))
                                .child(div().text_sm().text_color(rgb(0x8a95a5)).child(status))
                        }),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .p_4()
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
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.documents = if this.documents.is_some() {
                                    None
                                } else {
                                    Some(documents::Pane::new(
                                        remuda_native::daemon::socket_path("default"),
                                    ))
                                };
                                cx.notify();
                            })),
                    )
                    .when_some(self.documents.as_ref(), |d, pane| {
                        d.child(
                            div()
                                .id("document-pane")
                                .debug_selector(|| "document-pane".into())
                                .child(
                                    div()
                                        .id("document-status")
                                        .debug_selector(|| "document-status".into())
                                        .text_color(rgb(0x8a95a5))
                                        .child(pane.status()),
                                )
                                .child(
                                    div()
                                        .id("document-frame")
                                        .debug_selector(|| "document-frame".into())
                                        .track_focus(&documents_focus)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            if let Some(focus) = &this.documents_focus {
                                                focus.focus(window);
                                            }
                                            cx.notify();
                                        }))
                                        .on_key_down(cx.listener(
                                            |this, event: &gpui::KeyDownEvent, _, cx| {
                                                let key = &event.keystroke;
                                                if let Some(pane) = &mut this.documents {
                                                    match key.key.as_str() {
                                                        "up" => pane.move_selection(false),
                                                        "down" => pane.move_selection(true),
                                                        "enter" => pane.open_selected(),
                                                        "escape" => {
                                                            pane.back();
                                                        }
                                                        "tab" => pane.select_link(true),
                                                        "backtab" => pane.select_link(false),
                                                        "r" => pane.refresh(),
                                                        _ => {}
                                                    }
                                                }
                                                cx.notify();
                                            },
                                        ))
                                        .font_family("monospace")
                                        .text_sm()
                                        .whitespace_nowrap()
                                        .overflow_hidden()
                                        .py_2()
                                        .children(
                                            pane.frame(120, 30)
                                                .into_iter()
                                                .map(|line| div().child(line)),
                                        ),
                                )
                                .children(
                                    [
                                        ("doc-up", "Up", 0u8),
                                        ("doc-down", "Down", 1),
                                        ("doc-open", "Open", 2),
                                        ("doc-back", "Back", 3),
                                        ("doc-prev-link", "Prev link", 4),
                                        ("doc-next-link", "Next link", 5),
                                        ("doc-refresh", "Refresh document", 6),
                                    ]
                                    .into_iter()
                                    .map(|(id, text, action)| {
                                        div()
                                            .id(id)
                                            .debug_selector(move || id.to_string())
                                            .py_2()
                                            .pr_4()
                                            .cursor_pointer()
                                            .child(text)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                if let Some(pane) = &mut this.documents {
                                                    match action {
                                                        0 => pane.move_selection(false),
                                                        1 => pane.move_selection(true),
                                                        2 => pane.open_selected(),
                                                        3 => {
                                                            pane.back();
                                                        }
                                                        4 => pane.select_link(false),
                                                        5 => pane.select_link(true),
                                                        _ => pane.refresh(),
                                                    }
                                                }
                                                cx.notify();
                                            }))
                                    }),
                                ),
                        )
                    })
                    .child(div().child(if documents_open {
                        "Document browser"
                    } else if self.selected_peer.is_some() {
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
                    .when(self.selection.as_ref().is_some_and(|s| s.peer.is_none() && self.selection_current(s)), |d| {
                        d.child(div().pt_4().child(format!("Terminal target: {}", selection_label(self.selection.as_ref().unwrap()))))
                            .child(div().child("Attach takes over any current terminal. If this name is recreated during attach, the new session may be selected."))
                            .child(div().id("attach-terminal").debug_selector(|| "attach-terminal".into()).py_2().px_2().bg(rgb(0x34445a)).child("Attach interactive terminal").on_click(cx.listener(|this, _, window, cx| {
                                this.attach_selected();
                                if let Some(focus) = &this.input_focus { focus.focus(window); }
                                cx.notify();
                            })))
                            .child(div().id("terminal-status").debug_selector(|| "terminal-status".into()).child(self.terminal_status.clone()))
                    })
                    .when(self.terminal.is_some() && !documents_open, |d| {
                        let screen = self.terminal.as_ref().map(|terminal| terminal.screen()).unwrap_or_default();
                        d.child(div().id("detach-terminal").debug_selector(|| "detach-terminal".into()).py_2().child("Detach terminal").on_click(cx.listener(|this, _, _, cx| { this.terminal = None; this.terminal_status = "Terminal detached.".into(); cx.notify(); })))
                        .child(div().id("terminal-screen").debug_selector(|| "terminal-screen".into()).track_focus(&input_focus)
                            .on_click(cx.listener(|this, _, window, cx| { if let Some(focus) = &this.input_focus { focus.focus(window); } cx.notify(); }))
                            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                                if let (Some(terminal), Some(bytes)) = (&this.terminal, terminal::key_bytes(&event.keystroke)) {
                                    if let Err(error) = terminal.send(bytes) { this.terminal_status = error.into(); }
                                }
                                cx.notify();
                            }))
                            .font_family("monospace").text_sm().whitespace_nowrap().overflow_hidden().py_2().child(screen))
                    })
                    .when(!documents_open && self.terminal.is_none() && self.selection.as_ref().is_some_and(|s| self.selection_current(s)), |d| {
                        d.child(div().id("input-target").debug_selector(|| "input-target".into()).pt_4().child(format!("Input target: {}", selection_label(self.selection.as_ref().unwrap()))))
                            .child(div().child(if self.selected_peer.is_some() { "Peer line input (ASCII; Enter sends to this admitted peer; may append to its current prompt)" } else { "Local line input (ASCII; Enter sends; no terminal attach)" }))
                            .child(div()
                                .id("line-input")
                                .debug_selector(|| "line-input".into())
                                .track_focus(&input_focus)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    if let Some(focus) = &this.input_focus { focus.focus(window); }
                                    cx.notify();
                                }))
                                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                                    let key = &event.keystroke;
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
                    .when_some(self.selection.as_ref().filter(|s| self.terminal.is_none() && self.selection_current(s)), |d, s| d.child(div().id("preview-target").debug_selector(|| "preview-target".into()).child(format!("Snapshot: {}", selection_label(s)))))
                    .when(self.terminal.is_none() && !documents_open, |d| d.child(
                        div()
                            .id("preview")
                            .debug_selector(|| "preview".into())
                            .pt_4()
                            .overflow_hidden()
                            .child(self.preview.clone().unwrap_or_else(|| {
                                "Select a session for a read-only snapshot.".into()
                            })),
                    )
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
                    )),
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
            && args.last().map(String::as_str) == Some(WORKER_ABI)
            && args[4] == "--sync"
        {
            args.pop();
            std::process::exit(preview::sync_worker(&args));
        }
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
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1000.), px(650.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|cx| {
                    let mut gui = LocalGui::new();
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
                }),
            ))
            .unwrap();
        gui.drain();
        assert_eq!(gui.preview.as_deref(), Some("new"));
        assert_eq!(gui.sync_version, 4);
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
        let field = cx
            .debug_bounds("line-input")
            .expect("peer input control not laid out");
        cx.simulate_click(field.center(), gpui::Modifiers::none());
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
            let field = cx.debug_bounds("line-input").expect("line input visible");
            cx.simulate_click(field.center(), gpui::Modifiers::none());
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
            let terminal = cx
                .debug_bounds("terminal-screen")
                .expect("terminal visible");
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
    fn peer_failure_retains_stale_rows_and_exact_instance_identity() {
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
        assert_eq!(gui.remote_sessions[0].name, "proof");
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
        gui.select_session(row("one"));
        let (old, selected) = gui.preview_pending.take().unwrap();
        gui.select_session(row("two"));
        gui.preview_pending = None;
        gui.preview_sender
            .send((old, selected, Ok("stale".into())))
            .unwrap();
        gui.drain();
        assert!(gui.preview.is_none());
        gui.preview_busy = true;
        gui.select_session(row("one"));
        let (generation, selection) = gui.preview_pending.take().unwrap();
        gui.sessions[0].instance_id = Some("replacement".into());
        gui.preview_sender
            .send((generation, selection, Ok("stale".into())))
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
