use super::*;
use cluster_status::Reachability;
use gpui::{AnyElement, MouseButton};

enum Entry<'a> {
    Node(Option<(usize, &'a Peer)>),
    Session(Option<(usize, &'a Peer)>, usize, &'a Session, bool),
    Message(&'static str),
}

impl Entry<'_> {
    fn target(&self) -> Option<TreeRow> {
        match self {
            Self::Node(peer) => Some(TreeRow::Node(
                peer.map(|(_, peer)| peer.fingerprint.clone()),
            )),
            Self::Session(peer, _, row, _) => Some(TreeRow::session(
                peer.map(|(_, peer)| peer.fingerprint.clone()),
                row,
            )),
            Self::Message(_) => None,
        }
    }
}

impl LocalGui {
    fn entries(&self) -> Vec<Entry<'_>> {
        let query = self.search.as_deref().unwrap_or("").to_lowercase();
        let filtering = !query.is_empty() || self.attention_only;
        let mut entries = Vec::new();
        let groups = std::iter::once(None).chain(self.peers.iter().enumerate().map(Some));
        for peer in groups {
            let (name, expanded, rows, loading, unavailable, flagged) =
                if let Some((_, peer)) = peer {
                    let remote = self.remote_sessions.get(&peer.fingerprint);
                    let unreachable =
                        self.peer_status
                            .get(&peer.fingerprint)
                            .is_some_and(|status| {
                                matches!(
                                    status.state,
                                    Reachability::Connecting | Reachability::Offline
                                )
                            });
                    let unavailable = !self.peer_current(&peer.fingerprint)
                        || unreachable
                        || remote.is_some_and(|r| r.error);
                    (
                        peer.label.as_str(),
                        self.expanded_peers.contains(&peer.fingerprint),
                        remote.map(|r| r.rows.as_slice()).unwrap_or_default(),
                        remote.is_some_and(|r| r.loading),
                        unavailable,
                        !peer.admitted || unreachable || remote.is_some_and(|r| r.error),
                    )
                } else {
                    (
                        "This device",
                        self.local_expanded,
                        self.sessions.as_slice(),
                        self.loading,
                        self.error,
                        self.error,
                    )
                };
            let node_matches = name.to_lowercase().contains(&query);
            let matches = |row: &Session| {
                (node_matches || row.name.to_lowercase().contains(&query))
                    && (!self.attention_only
                        || flagged
                        || !row.alive
                        || row.unavailable
                        || row
                            .decoration
                            .as_ref()
                            .is_some_and(|status| status.glyph == "!"))
            };
            if filtering
                && !(node_matches && (!self.attention_only || flagged))
                && !rows.iter().any(matches)
            {
                continue;
            }
            entries.push(Entry::Node(peer));
            if !expanded && !filtering {
                continue;
            }
            if loading {
                entries.push(Entry::Message("Loading sessions…"));
            } else if unavailable && rows.is_empty() {
                entries.push(Entry::Message("Sessions unavailable"));
            } else if unavailable {
                entries.push(Entry::Message("Last known sessions"));
            } else if rows.is_empty() && !filtering {
                entries.push(Entry::Message("No sessions"));
            }
            entries.extend(
                rows.iter()
                    .enumerate()
                    .filter(|(_, row)| matches(row))
                    .map(|(index, row)| Entry::Session(peer, index, row, unavailable)),
            );
        }
        entries
    }

    pub(super) fn tree_rows(&self) -> Vec<TreeRow> {
        self.entries().iter().filter_map(Entry::target).collect()
    }

    fn refresh_filter_data(&mut self) {
        let missing: Vec<_> = self
            .peers
            .iter()
            .filter(|p| {
                self.peer_current(&p.fingerprint)
                    && !self.remote_sessions.contains_key(&p.fingerprint)
                    && !self
                        .peer_status
                        .get(&p.fingerprint)
                        .is_some_and(|s| s.state == Reachability::Offline)
            })
            .map(|p| p.fingerprint.clone())
            .collect();
        for fp in missing {
            self.refresh_peer(fp);
        }
    }

    fn focus_search(&mut self, window: &mut Window) {
        self.search.get_or_insert_with(String::new);
        self.refresh_filter_data();
        if let Some(focus) = &self.tree_focus {
            focus.focus(window);
        }
    }

    fn ensure_tree_cursor_visible(&mut self) {
        let rows = self.tree_rows();
        if let Some(first) = rows.first().filter(|_| !rows.contains(&self.focused_row)) {
            self.focused_row = first.clone();
        }
        if let Some(index) = self
            .entries()
            .iter()
            .position(|entry| entry.target().as_ref() == Some(&self.focused_row))
        {
            self.tree_scroll.scroll_to_item(index);
        }
    }

    fn tree_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = &event.keystroke;
        let printable = key.key_char.as_deref().or_else(|| {
            (key.key.chars().count() == 1 && !key.modifiers.control && !key.modifiers.platform)
                .then_some(key.key.as_str())
        });
        match (key.key.as_str(), printable) {
            ("escape", _) if self.show_help => self.show_help = false,
            ("escape", _) if self.search.is_some() => self.search = None,
            (_, Some("/")) if self.search.is_none() => self.focus_search(window),
            (_, Some("?")) if self.search.is_none() => self.show_help = !self.show_help,
            (_, Some("!")) if self.search.is_none() => {
                self.attention_only = !self.attention_only;
                if self.attention_only {
                    self.refresh_filter_data();
                }
            }
            ("backspace", _) if self.search.is_some() => {
                self.search.as_mut().unwrap().pop();
            }
            ("enter" | "space", _) if self.show_help => self.show_help = false,
            ("enter" | "up" | "down" | "left" | "right" | "home" | "end", _) => {
                self.move_tree(&key.key);
                if key.key == "enter"
                    && self.search.is_none()
                    && matches!((&self.focused_row, &self.selection),
                        (TreeRow::Session(peer, name, Some(instance)), Some(selection))
                            if peer == &selection.peer && name == &selection.name
                                && instance == &selection.instance)
                {
                    self.attach_input(window, cx, true);
                }
            }
            ("k", _) if self.search.is_none() => {
                if self.live.is_none() {
                    self.attach_input(window, cx, false);
                }
            }
            (_, Some(text))
                if self.search.is_some() && !key.modifiers.control && !key.modifiers.platform =>
            {
                let query = self.search.as_mut().unwrap();
                if query.len() < 160 && !text.chars().any(char::is_control) {
                    query.push_str(text);
                }
            }
            _ => return,
        }
        self.ensure_tree_cursor_visible();
        cx.stop_propagation();
        cx.notify();
        window.refresh();
    }

    fn move_tree(&mut self, key: &str) {
        let rows = self.tree_rows();
        let Some(first) = rows.first() else {
            return;
        };
        let position = rows
            .iter()
            .position(|row| row == &self.focused_row)
            .unwrap_or_default();
        let current = rows[position].clone();
        match (key, current) {
            ("home", _) => self.focused_row = first.clone(),
            ("end", _) => self.focused_row = rows[rows.len() - 1].clone(),
            ("up", _) => self.focused_row = rows[position.saturating_sub(1)].clone(),
            ("down", _) => self.focused_row = rows[(position + 1).min(rows.len() - 1)].clone(),
            ("left", TreeRow::Node(peer) | TreeRow::Session(peer, _, _)) => {
                self.focused_row = TreeRow::Node(peer.clone());
                self.set_expanded(peer, false);
            }
            ("right", TreeRow::Node(peer)) => self.set_expanded(peer, true),
            ("enter", TreeRow::Node(peer)) => {
                let expanded = self.is_expanded(peer.as_deref());
                self.set_expanded(peer, !expanded);
            }
            ("enter", TreeRow::Session(peer, name, instance)) => {
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
                    .find(|row| row.name == name && row.instance_id == instance)
                    .cloned()
                {
                    if row.alive && !row.unavailable {
                        self.select_session(peer, row);
                    }
                }
            }
            _ => {}
        }
    }

    fn node_status_label(&self, peer: Option<&Peer>) -> (String, u32) {
        let Some(peer) = peer else {
            return if self.error {
                ("Offline".into(), 0xffad9f)
            } else if self.loading {
                ("Connecting".into(), 0xe7c279)
            } else {
                ("Local".into(), 0x9bb0c6)
            };
        };
        if !peer.admitted {
            return ("Revoked".into(), 0xffad9f);
        }
        let state = self.peer_status.get(&peer.fingerprint);
        let (status, color) = match state.map(|s| s.state) {
            Some(Reachability::Reachable) => ("Online", 0x7bdda8),
            Some(Reachability::Connecting) => ("Connecting", 0xe7c279),
            Some(Reachability::Offline) => ("Offline", 0xffad9f),
            _ if self
                .remote_sessions
                .get(&peer.fingerprint)
                .is_some_and(|r| r.error) =>
            {
                ("Unavailable", 0xffad9f)
            }
            _ => ("Unknown", 0xb5beca),
        };
        let age = state
            .and_then(|s| s.last_updated_at)
            .and_then(|time| SystemTime::now().duration_since(time).ok());
        (
            age.map_or_else(
                || status.into(),
                |age| format!("{status} · {}", compact_age(age)),
            ),
            color,
        )
    }

    fn render_entry(&self, entry: Entry<'_>, cx: &mut Context<Self>) -> AnyElement {
        match entry {
            Entry::Message(notice) => div()
                .pl_8()
                .py_1()
                .text_xs()
                .text_color(rgb(0xb5beca))
                .child(notice)
                .into_any_element(),
            Entry::Node(peer) => {
                let (name, key) = peer.map_or_else(
                    || ("This device".into(), "node-local".into()),
                    |(index, peer)| (display_label(&peer.label), format!("node-peer-{index}")),
                );
                let (status, color) = self.node_status_label(peer.map(|(_, p)| p));
                let fp = peer.map(|(_, peer)| peer.fingerprint.clone());
                let expanded = self.is_expanded(fp.as_deref());
                let filtering = self.attention_only
                    || self.search.as_ref().is_some_and(|query| !query.is_empty());
                let focused = self.focused_row == TreeRow::Node(fp.clone());
                let debug = key.clone();
                let full_name = format!("{name} · {status}");
                let glyph = match status.split(" · ").next() {
                    Some("Online" | "Local") => "●",
                    Some("Connecting" | "Unknown") => "○",
                    _ => "!",
                };
                div()
                    .id(SharedString::from(key))
                    .debug_selector(move || debug.clone())
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .px_2()
                    .py_1()
                    .text_sm()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|d| d.bg(rgb(0x2a3340)))
                    .when(focused, |d| d.bg(rgb(0x303a49)))
                    .child(
                        div()
                            .flex_none()
                            .text_color(rgb(0x9bb0c6))
                            .child(if filtering {
                                "·"
                            } else if expanded {
                                "▾"
                            } else {
                                "▸"
                            }),
                    )
                    .child(div().flex_none().text_color(rgb(color)).child(glyph))
                    .child(div().flex_1().min_w_0().truncate().child(name))
                    .tooltip(move |_, app| app.new(|_| FullName(full_name.clone())).into())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(focus) = &this.tree_focus {
                            focus.focus(window);
                        }
                        this.focused_row = TreeRow::Node(fp.clone());
                        if !filtering {
                            this.set_expanded(fp.clone(), !this.is_expanded(fp.as_deref()));
                        }
                        cx.notify();
                    }))
                    .into_any_element()
            }
            Entry::Session(peer, index, session, unavailable) => {
                let fp = peer.map(|(_, peer)| peer.fingerprint.clone());
                let key = peer.map_or_else(
                    || format!("session-{index}"),
                    |(node, _)| format!("peer-session-{node}-{index}"),
                );
                let row = TreeRow::session(fp.clone(), session);
                let focused = self.focused_row == row;
                let selected = self.selection.as_ref().is_some_and(|s| {
                    s.peer == fp
                        && s.name == session.name
                        && session.instance_id.as_deref() == Some(&s.instance)
                });
                let (status, dot) = if unavailable || session.unavailable {
                    ("Unavailable", 0xd1a163)
                } else if session.alive {
                    ("Running", 0x65d89b)
                } else {
                    ("Exited", 0xffad9f)
                };
                let openable = session.alive && !session.unavailable && !unavailable;
                let name = display_label(&session.name);
                let (glyph, detail) = match session.decoration.as_ref().filter(|_| openable) {
                    Some(decoration) => (decoration.glyph.clone(), decoration.detail.clone()),
                    None if unavailable || session.unavailable => ("!".into(), String::new()),
                    None if session.alive => ("●".into(), String::new()),
                    None => ("●".into(), String::new()),
                };
                let (status, dot) = match glyph.as_str() {
                    "🏇" => ("Working", dot),
                    "○" => ("Idle", dot),
                    "!" if openable => ("Needs attention", 0xffad9f),
                    _ => (status, dot),
                };
                let full_name = if detail.is_empty() {
                    format!("{name} · {status}")
                } else {
                    format!("{name} · {status} · {detail}")
                };
                let session = session.clone();
                let debug = key.clone();
                let mut item = div()
                    .id(SharedString::from(key))
                    .debug_selector(move || debug.clone())
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .px_2()
                    .py_1()
                    .text_sm()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(if focused { 0x799ebe } else { 0x191d24 }))
                    .when(selected, |d| d.bg(rgb(0x233e57)))
                    .child(
                        div()
                            .flex_none()
                            .w(px(18.))
                            .text_color(rgb(dot))
                            .child(glyph),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(div().truncate().child(name))
                            .when(!detail.is_empty(), |d| {
                                d.child(
                                    div()
                                        .truncate()
                                        .text_xs()
                                        .text_color(rgb(0xb5beca))
                                        .child(detail),
                                )
                            }),
                    )
                    .tooltip(move |_, app| app.new(|_| FullName(full_name.clone())).into());
                if openable {
                    item = item
                        .cursor_pointer()
                        .hover(|d| d.bg(rgb(0x2a3340)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if let Some(focus) = &this.tree_focus {
                                focus.focus(window);
                            }
                            this.focused_row = row.clone();
                            this.select_session(fp.clone(), session.clone());
                            cx.notify();
                        }));
                }
                div().pl_5().child(item).into_any_element()
            }
        }
    }
}

impl TreeRow {
    pub(super) fn session(peer: Option<String>, row: &Session) -> Self {
        Self::Session(peer, row.name.clone(), row.instance_id.clone())
    }
}

fn compact_age(age: Duration) -> String {
    let seconds = age.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h", seconds / 3600)
    }
}

fn display_label(value: &str) -> String {
    let escaped = label(value);
    escaped[1..escaped.len() - 1].to_owned()
}

impl Render for LocalGui {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self
            .tree_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        let input_focus = self
            .live_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        let viewport = window.viewport_size();
        let width = f32::from(viewport.width);
        let narrow = width < 700.;
        let sidebar = self.sidebar_width.clamp(208., (width - 385.).max(208.));
        let live = self.live.is_some();
        let peer = self
            .selection
            .as_ref()
            .and_then(|s| s.peer.as_deref())
            .and_then(|fp| self.peers.iter().find(|p| p.fingerprint == fp));
        let node = peer.map_or_else(|| "This device".into(), |peer| display_label(&peer.label));
        let title = self.selection.as_ref().map_or_else(
            || "Session".into(),
            |s| format!("{node} / {}", display_label(&s.name)),
        );
        let detail = self.selection.as_ref().and_then(|selection| {
            let rows = match &selection.peer {
                Some(peer) => &self.remote_sessions.get(peer)?.rows,
                None => &self.sessions,
            };
            let row = rows.iter().find(|row| {
                row.name == selection.name
                    && row.instance_id.as_deref() == Some(selection.instance.as_str())
            })?;
            let detail = &row.decoration.as_ref()?.detail;
            (!detail.is_empty()).then(|| detail.clone())
        });
        let current = self
            .selection
            .as_ref()
            .is_some_and(|s| self.selection_current(s));
        let current = current && !self.selected_peer_offline();
        let snapshot = if let Some(at) = self.preview_received {
            if live {
                let mode = if self.line_input.is_some() {
                    "Line input"
                } else {
                    "Direct input"
                };
                format!("{mode} · screen {} ago", compact_age(at.elapsed()))
            } else if self.preview_failed || !current {
                format!("Last known screen · {} ago", compact_age(at.elapsed()))
            } else {
                format!("Read-only snapshot · {} ago", compact_age(at.elapsed()))
            }
        } else if self.selection.is_some() && !self.preview_failed {
            "Loading screen…".into()
        } else if self.preview_failed {
            "Screen unavailable".into()
        } else {
            "Select a session in the tree".into()
        };
        let connected = usize::from(!self.error && !self.loading)
            + self
                .peers
                .iter()
                .filter(|p| {
                    p.admitted
                        && self
                            .peer_status
                            .get(&p.fingerprint)
                            .is_some_and(|s| s.state == Reachability::Reachable)
                })
                .count();
        let total = 1 + self.peers.iter().filter(|p| p.admitted).count();
        let mut tree = div()
            .id("session-sidebar")
            .debug_selector(|| "session-sidebar".into())
            .flex_none()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(rgb(0x191d24))
            .p_3()
            .track_focus(&focus)
            .on_key_down(cx.listener(Self::tree_key));
        tree = if narrow {
            tree.w_full()
                .h(px((f32::from(viewport.height) * 0.40).clamp(100., 245.)))
        } else {
            tree.w(px(sidebar)).h_full()
        };
        let entries = self.entries();
        let empty = entries.is_empty();
        let show_help = self.show_help;
        let filtered = self.attention_only;
        div()
            .size_full()
            .flex()
            .when(narrow, |root| root.flex_col())
            .bg(rgb(0x101216))
            .text_color(rgb(0xe8eaf0))
            .when(cfg!(target_os = "macos"), |root| root.font_family("Menlo"))
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                if let Some((origin, width)) = this.resize_start {
                    if event.pressed_button == Some(MouseButton::Left) {
                        this.sidebar_width = width + f32::from(event.position.x) - origin;
                    } else {
                        this.resize_start = None;
                    }
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.resize_start.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .child(
                tree.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .pb_2()
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .flex()
                                .gap_2()
                                .items_center()
                                .child(div().text_sm().child(if self.peers.is_empty() {
                                    "Sessions"
                                } else {
                                    "Cluster"
                                }))
                                .when(!self.peers.is_empty(), |d| {
                                    d.child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(0x9bb0c6))
                                            .whitespace_nowrap()
                                            .child(format!("{connected}/{total} online")),
                                    )
                                }),
                        )
                        .child(
                            button("refresh", "Refresh", false)
                                .debug_selector(|| "refresh".into())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.refresh();
                                    this.refresh_nodes();
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_1()
                        .pb_2()
                        .child(
                            button("tree-find", "Find /", self.search.is_some()).on_click(
                                cx.listener(|this, _, window, cx| {
                                    if this.search.is_some() {
                                        this.search = None;
                                    } else {
                                        this.focus_search(window);
                                    }
                                    cx.notify();
                                }),
                            ),
                        )
                        .child(button("tree-filter", "Attention !", filtered).on_click(
                            cx.listener(|this, _, window, cx| {
                                this.attention_only = !this.attention_only;
                                if this.attention_only {
                                    this.refresh_filter_data();
                                }
                                if let Some(focus) = &this.tree_focus {
                                    focus.focus(window);
                                }
                                cx.notify();
                            }),
                        ))
                        .child(
                            button("tree-help", "Keys ?", show_help).on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.show_help = !this.show_help;
                                    if let Some(focus) = &this.tree_focus {
                                        focus.focus(window);
                                    }
                                    cx.notify();
                                },
                            )),
                        ),
                )
                .when_some(self.search.as_ref(), |d, query| {
                    d.child(
                        div()
                            .id("tree-query")
                            .debug_selector(|| "tree-query".into())
                            .pb_2()
                            .text_sm()
                            .text_color(rgb(0xadcceb))
                            .child(if query.is_empty() {
                                "Type a node or session name…".into()
                            } else {
                                format!("Find: {query}")
                            }),
                    )
                })
                .when(self.helper_changed, |d| {
                    d.child(
                        div()
                            .id("helper-changed")
                            .debug_selector(|| "helper-changed".into())
                            .child(STALE_HELPER),
                    )
                })
                .when(self.nodes_loading || self.nodes_error, |d| {
                    d.child(div().text_xs().pb_1().child(if self.nodes_loading {
                        "Loading nodes…"
                    } else {
                        "Node list may be stale"
                    }))
                })
                .when(show_help, |d| {
                    d.child(
                        div()
                            .id("tree-shortcuts")
                            .debug_selector(|| "tree-shortcuts".into())
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .text_sm()
                            .text_color(rgb(0xb5beca))
                            .children(
                                [
                                    "↑ ↓   Move",
                                    "← →   Fold / unfold",
                                    "Enter   Compose line / Fold node",
                                    "K   Direct input",
                                    "Ctrl+\\   Detach",
                                    "Esc   Close line input, search, or help",
                                    "/   Find",
                                    "!   Attention",
                                    "?   Keys",
                                ]
                                .map(|text| div().py_1().child(text)),
                            ),
                    )
                })
                .when(!show_help, |d| {
                    d.child(
                        div()
                            .id("node-tree")
                            .debug_selector(|| "node-tree".into())
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .track_scroll(&self.tree_scroll)
                            .children(
                                entries
                                    .into_iter()
                                    .map(|entry| self.render_entry(entry, cx)),
                            )
                            .when(empty, |d| {
                                d.child(
                                    div().p_2().text_sm().child("No matching sessions or nodes"),
                                )
                            }),
                    )
                }),
            )
            .when(!narrow, |root| {
                root.child(
                    div()
                        .id("sidebar-divider")
                        .debug_selector(|| "sidebar-divider".into())
                        .w(px(5.))
                        .flex_none()
                        .h_full()
                        .bg(rgb(0x262e3a))
                        .cursor(gpui::CursorStyle::ResizeLeftRight)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                                this.resize_start = Some((f32::from(event.position.x), sidebar));
                                cx.notify();
                            }),
                        ),
                )
            })
            .child(
                div()
                    .id("session-pane")
                    .debug_selector(|| "session-pane".into())
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .p_4()
                    .child(
                        div()
                            .id("preview-title")
                            .debug_selector(|| "preview-title".into())
                            .text_lg()
                            .child(title),
                    )
                    .when_some(detail, |d, detail| {
                        d.child(div().text_sm().text_color(rgb(0xb5beca)).child(detail))
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .py_1()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(
                                        if self.preview_failed
                                            || (!current && self.preview_received.is_some())
                                        {
                                            0xe7c279
                                        } else {
                                            0xb5beca
                                        },
                                    ))
                                    .child(snapshot),
                            )
                            .when(live, |d| {
                                d.child(button("detach-session", "Detach", true).on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.detach();
                                        if let Some(focus) = &this.tree_focus {
                                            focus.focus(window);
                                        }
                                        cx.notify();
                                    }),
                                ))
                            })
                            .when(!live && current, |d| {
                                d.child(button("attach-session", "Compose", true).on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.attach_input(window, cx, true)
                                    }),
                                ))
                            }),
                    )
                    .when_some(self.live_notice, |d, notice| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(rgb(0xadcceb))
                                .pb_1()
                                .child(notice),
                        )
                    })
                    .child(
                        div()
                            .id("preview")
                            .debug_selector(|| "preview".into())
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .border_t_1()
                            .border_color(rgb(if live { 0x799ebe } else { 0x262e3a }))
                            .track_focus(&input_focus)
                            .on_key_down(cx.listener(Self::live_key))
                            .child(
                                div()
                                    .flex_1()
                                    .min_h_0()
                                    .relative()
                                    .overflow_hidden()
                                    .child(crate::terminal_view::measure(self.viewport.clone()))
                                    .child(
                                        div()
                                            .id("terminal-cells")
                                            .size_full()
                                            .overflow_scroll()
                                            .when_some(self.preview.as_ref(), |d, screen| {
                                                d.child(screen.view())
                                            }),
                                    ),
                            )
                            .when_some(self.line_input.as_ref(), |d, editor| {
                                d.child(editor.view())
                            }),
                    ),
            )
    }
}

fn button(id: &'static str, text: &'static str, active: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .debug_selector(move || id.into())
        .flex_none()
        .px_2()
        .py_1()
        .rounded_sm()
        .text_xs()
        .bg(rgb(if active { 0x233e57 } else { 0x252d38 }))
        .text_color(rgb(0xe8eaf0))
        .cursor_pointer()
        .hover(|d| d.bg(rgb(0x34445a)))
        .child(text)
}

struct FullName(String);

impl Render for FullName {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .max_w(px(420.))
            .px_2()
            .py_1()
            .rounded_sm()
            .text_sm()
            .bg(rgb(0x303a49))
            .text_color(rgb(0xe8eaf0))
            .child(self.0.clone())
    }
}
