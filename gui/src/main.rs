use gpui::{
    div, prelude::*, px, rgb, size, App, Application, Bounds, Context, SharedString, Window,
    WindowBounds, WindowOptions,
};
use serde::{Deserialize, Serialize};
use std::{sync::mpsc, time::Duration};

mod provider;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Session {
    name: String,
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
fn worker() -> i32 {
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
            alive: s.alive,
            unavailable: s.unavailable,
        })
        .collect();
    i32::from(serde_json::to_writer(std::io::stdout(), &sessions).is_err())
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

fn nodes_worker() -> i32 {
    worker_deadline();
    match discover_peers() {
        Ok(rows) => i32::from(serde_json::to_writer(std::io::stdout(), &rows).is_err()),
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

struct LocalGui {
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
}
impl LocalGui {
    fn new() -> Self {
        let (sender, results) = mpsc::channel();
        let (nodes_sender, nodes_results) = mpsc::channel();
        Self {
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
        }
    }
    fn refresh_nodes(&mut self) {
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
                    provider::output(&path, &["--internal-nodes-worker"], provider::TIMEOUT)
                })
                .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()));
            let _ = sender.send((generation, result));
        });
    }
    fn peer_current(&self, fingerprint: &str) -> bool {
        !self.nodes_loading
            && !self.nodes_error
            && !fingerprint.is_empty()
            && self
                .peers
                .iter()
                .filter(|peer| peer.fingerprint == fingerprint)
                .count()
                == 1
            && self
                .peers
                .iter()
                .any(|peer| peer.fingerprint == fingerprint && peer.admitted)
    }
    fn select_peer(&mut self, fingerprint: String) {
        if self.peer_current(&fingerprint) {
            self.selected_peer = Some(fingerprint);
        }
    }
    fn refresh(&mut self) {
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
            let result = std::env::current_exe()
                .map_err(|e| e.to_string())
                .and_then(|path| {
                    provider::output(&path, &["--internal-list-worker"], provider::TIMEOUT)
                })
                .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()));
            let _ = sender.send((generation, result));
        });
    }
    fn drain(&mut self) {
        while let Ok((generation, result)) = self.results.try_recv() {
            self.busy = false;
            if generation == self.generation {
                self.loading = false;
                match result {
                    Ok(rows) => {
                        self.sessions = rows;
                        self.error = false;
                    }
                    Err(_) => self.error = true,
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
                    Err(_) => self.nodes_error = true,
                }
                if self
                    .selected_peer
                    .as_ref()
                    .is_some_and(|fp| !self.peer_current(fp))
                {
                    self.selected_peer = None;
                }
            }
            self.launch_nodes();
        }
    }
}
impl Render for LocalGui {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = if self.selected_peer.is_some() {
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
                        self.sessions
                            .clone()
                            .into_iter()
                            .filter(|_| self.selected_peer.is_none())
                            .map(|session| {
                                let status = if session.unavailable {
                                    "unavailable"
                                } else if session.alive {
                                    "running"
                                } else {
                                    "exited"
                                };
                                div()
                                    .py_2()
                                    .child(div().child(label(&session.name)))
                                    .child(div().text_sm().text_color(rgb(0x8a95a5)).child(status))
                            }),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .p_4()
                    .child(div().child(if self.selected_peer.is_some() {
                        "Peer session listing is not available yet."
                    } else {
                        "Read-only local session list"
                    }))
                    .when(self.error, |d| {
                        d.child(
                            div()
                                .text_color(rgb(0xf28787))
                                .child("Local daemon unavailable; listed sessions may be stale."),
                        )
                    })
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
                    ),
            )
    }
}
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--internal-list-worker") {
        std::process::exit(worker());
    }
    if std::env::args().nth(1).as_deref() == Some("--internal-nodes-worker") {
        std::process::exit(nodes_worker());
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
    fn labels_escape_controls_and_literal_escapes() {
        assert_ne!(label("\u{1b}"), label("\\u{1b}"));
        assert!(label("a\u{202e}b").contains("\\u{202e}"));
        assert_eq!(label("한글"), "\"한글\"");
    }
    #[gpui::test]
    fn node_selection_navigates_locally_and_refuses_stale_peers(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
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
