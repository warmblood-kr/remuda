use super::*;
use cluster_status::Reachability;
use gpui::{AnyElement, MouseButton};
use gpui_kit::base::{Button, Tab, Tabs};

fn render_node_metric(
    metric: remuda_native::node_status::Metric,
    stale: bool,
    compact: bool,
) -> impl IntoElement {
    let value = if let Some(usage) = metric.capacity {
        const GIB: f64 = 1_073_741_824.0;
        format!(
            "{:.1} / {:.1} GiB",
            usage.used_bytes as f64 / GIB,
            usage.total_bytes as f64 / GIB
        )
    } else {
        metric
            .percent
            .map_or_else(|| "Unavailable".into(), |value| format!("{value}%"))
    };
    let color = if stale { 0xe7c279 } else { 0x7bdda8 };
    let bar = div().h(px(6.)).rounded_full().bg(rgb(0x282f38)).when_some(
        metric.percent,
        |el, percent| {
            el.child(
                div()
                    .w(gpui::relative(f32::from(percent) / 100.))
                    .h_full()
                    .rounded_full()
                    .bg(rgb(color)),
            )
        },
    );
    let label = div().flex_none().child(metric.label);
    let value = div()
        .flex_none()
        .whitespace_nowrap()
        .text_color(rgb(0xb5beca))
        .debug_selector(|| format!("node-metric-value-{}", metric.label))
        .child(value);
    let row = div().flex().py_1().text_sm();
    if compact {
        row.flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_x_3()
                    .child(label)
                    .child(value),
            )
            .child(bar.w_full())
    } else {
        row.items_center()
            .gap_3()
            .child(label.w(px(94.)))
            .child(bar.flex_1().min_w_0())
            .child(value.min_w(px(135.)).text_right())
    }
}

enum Entry<'a> {
    Node(Option<(usize, &'a Peer)>),
    Session(Option<(usize, &'a Peer)>, usize, &'a Session, bool, usize),
    Message(&'static str),
}

impl Entry<'_> {
    fn target(&self) -> Option<TreeRow> {
        match self {
            Self::Node(peer) => Some(TreeRow::Node(
                peer.map(|(_, peer)| peer.fingerprint.clone()),
            )),
            Self::Session(peer, _, row, _, _) => Some(TreeRow::session(
                peer.map(|(_, peer)| peer.fingerprint.clone()),
                row,
            )),
            Self::Message(_) => None,
        }
    }
}

fn team_order(rows: &[Session]) -> Vec<(usize, usize)> {
    let by_identity: std::collections::HashMap<_, _> = rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| Some(((row.name.as_str(), row.instance_id.as_deref()?), index)))
        .collect();
    let mut children = vec![Vec::new(); rows.len()];
    let mut roots = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let parent = row.leader.as_ref().and_then(|leader| {
            by_identity.get(&(leader.name.as_str(), leader.instance_id.as_str()))
        });
        match parent {
            Some(&parent) if parent != index => children[parent].push(index),
            _ => roots.push(index),
        }
    }
    let mut result = Vec::with_capacity(rows.len());
    let mut stack: Vec<_> = roots.into_iter().rev().map(|index| (index, 0)).collect();
    let mut listed = vec![false; rows.len()];
    while let Some((index, depth)) = stack.pop() {
        listed[index] = true;
        result.push((index, depth));
        stack.extend(
            children[index]
                .iter()
                .rev()
                .map(|&child| (child, depth + 1)),
        );
    }
    // Cyclic claims have no root. Keep every affected session selectable.
    result.extend(
        listed
            .iter()
            .enumerate()
            .filter_map(|(index, &listed)| (!listed).then_some((index, 0))),
    );
    result
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
                team_order(rows)
                    .into_iter()
                    .filter(|&(index, _)| matches(&rows[index]))
                    .map(|(index, depth)| {
                        Entry::Session(peer, index, &rows[index], unavailable, depth)
                    }),
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

    fn focus_search(&mut self, window: &mut Window, cx: &mut App) {
        self.search.get_or_insert_with(String::new);
        self.refresh_filter_data();
        if let Some(focus) = &self.tree_focus {
            focus.focus(window, cx);
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
        if self.drive_selected
            || self
                .sidebar_tabs_focus
                .as_ref()
                .is_some_and(|focus| focus.contains_focused(window, cx))
            || self
                .refresh_focus
                .as_ref()
                .is_some_and(|focus| focus.is_focused(window))
        {
            return;
        }
        let key = &event.keystroke;
        let printable = key.key_char.as_deref().or_else(|| {
            (key.key.chars().count() == 1 && !key.modifiers.control && !key.modifiers.platform)
                .then_some(key.key.as_str())
        });
        match (key.key.as_str(), printable) {
            ("escape", _) if self.show_help => self.show_help = false,
            ("escape", _) if self.search.is_some() => self.search = None,
            (_, Some("/")) if self.search.is_none() => self.focus_search(window, cx),
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
                if matches!(self.focused_row, TreeRow::Node(_)) {
                    self.open_node_details(cx);
                } else {
                    self.node_details_requested = false;
                }
                if key.key == "enter"
                    && self.search.is_none()
                    && matches!((&self.focused_row, &self.selection),
                        (TreeRow::Session(peer, name, Some(instance)), Some(selection))
                            if peer == &selection.peer && name == &selection.name
                                && instance == &selection.instance)
                {
                    self.open_session_composer(window, cx);
                }
            }
            ("k", _) if self.search.is_none() && !self.showing_node_details() => {
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

    fn open_node_details(&mut self, cx: &mut Context<Self>) {
        self.node_details_requested = true;
        if let Some(documents) = &self.documents {
            documents.update(cx, |docs, _| docs.select_terminal());
        }
    }

    fn selected_node_panel(
        &self,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        if !self.showing_node_details() {
            return None;
        }
        let TreeRow::Node(peer) = &self.focused_row else {
            return None;
        };
        let name = peer.as_ref().map_or_else(
            || "This device".into(),
            |fp| {
                self.peers
                    .iter()
                    .find(|peer| peer.fingerprint == *fp)
                    .map_or_else(|| display_label(fp), |peer| display_label(&peer.label))
            },
        );
        let peer_entry = peer
            .as_ref()
            .and_then(|fp| self.peers.iter().find(|p| &p.fingerprint == fp));
        let details = self
            .selected_status_target()
            .map_or_else(remuda_native::node_status::Status::unavailable, |node| {
                self.selected_node_status.details_for(&node)
            });
        let (reachability, reachability_color) = self.node_status_label(peer_entry);
        let caption = if (details.health == "Status unavailable" || details.stale)
            && matches!(
                reachability.split(" · ").next(),
                Some("Offline" | "Revoked")
            ) {
            reachability.split(" · ").next().unwrap_or(&reachability)
        } else {
            details.health
        };
        let status = details.age.map_or_else(
            || caption.to_owned(),
            |age| format!("{caption} · updated {} ago", compact_age(age)),
        );
        let (rows, rows_unavailable, rows_loading) = match peer {
            None => (self.sessions.as_slice(), self.error, self.loading),
            Some(fp) => self.remote_sessions.get(fp).map_or(
                (&[][..], !self.peer_current(fp), false),
                |state| {
                    (
                        state.rows.as_slice(),
                        state.error || !self.peer_current(fp),
                        state.loading,
                    )
                },
            ),
        };
        let rows = rows.iter().cloned().enumerate().map(|(index, row)| {
            let state = if rows_unavailable || row.unavailable {
                "Unavailable"
            } else if row.alive {
                "Running"
            } else {
                "Exited"
            };
            let enabled = state == "Running";
            let fp = peer.clone();
            div()
                .id(SharedString::from(format!("node-detail-session-{index}")))
                .flex()
                .justify_between()
                .gap_2()
                .py_1()
                .child(div().truncate().child(display_label(&row.name)))
                .child(div().flex_none().text_color(rgb(0xb5beca)).child(state))
                .when(enabled, |el| {
                    el.cursor_pointer()
                        .hover(|el| el.bg(rgb(0x2a3340)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.focused_row = TreeRow::session(fp.clone(), &row);
                            this.select_session(fp.clone(), row.clone());
                            cx.notify();
                        }))
                })
        });
        Some(
            div()
                .id("selected-node-status")
                .debug_selector(|| "selected-node-status".into())
                .flex_1()
                .min_h_0()
                .min_w_0()
                .overflow_y_scroll()
                .p_4()
                .child(div().text_lg().child(name))
                .child(
                    div()
                        .text_sm()
                        .py_2()
                        .text_color(rgb(if details.stale {
                            0xe7c279
                        } else {
                            reachability_color
                        }))
                        .child(status),
                )
                .child(
                    div()
                        .border_t_1()
                        .border_b_1()
                        .border_color(rgb(0x354454))
                        .py_2()
                        .children(
                            details
                                .metrics
                                .map(|metric| render_node_metric(metric, details.stale, compact)),
                        ),
                )
                .child(div().pt_3().pb_1().child("Sessions"))
                .when(rows_loading, |el| el.child("Loading sessions…"))
                .when(rows_unavailable, |el| {
                    el.child("Session list unavailable or stale")
                })
                .when(
                    rows.len() == 0 && !rows_loading && !rows_unavailable,
                    |el| el.child("No sessions"),
                )
                .children(rows),
        )
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
                            focus.focus(window, cx);
                        }
                        this.focused_row = TreeRow::Node(fp.clone());
                        this.open_node_details(cx);
                        if !filtering {
                            this.set_expanded(fp.clone(), !this.is_expanded(fp.as_deref()));
                        }
                        cx.notify();
                    }))
                    .into_any_element()
            }
            Entry::Session(peer, index, session, unavailable, depth) => {
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
                                focus.focus(window, cx);
                            }
                            this.focused_row = row.clone();
                            this.select_session(fp.clone(), session.clone());
                            if matches!((&this.focused_row, &this.selection),
                                (TreeRow::Session(peer, name, Some(instance)), Some(selected))
                                    if peer == &selected.peer && name == &selected.name && instance == &selected.instance)
                            {
                                this.open_session_composer(window, cx);
                            }
                            cx.notify();
                        }));
                }
                div()
                    .pl(px(20. + depth.min(8) as f32 * 12.))
                    .child(item)
                    .into_any_element()
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
        let refresh_focus = self
            .refresh_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        let session_action_focus = self
            .session_action_focus
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
        let docs = self.documents.clone();
        let open_terminal_link = docs
            .as_ref()
            .map(|docs| browser::Browser::terminal_link_handler(docs.downgrade()));
        let workspace_wide = browser::Browser::wide(window);
        let document_selected = docs
            .as_ref()
            .is_some_and(|docs| docs.read(cx).showing_document());
        let split = workspace_wide
            && self.selection.is_some()
            && docs.as_ref().is_some_and(|docs| {
                let docs = docs.read(cx);
                docs.has_document() && docs.split_requested()
            });
        let drive = if self.drive_selected {
            docs.as_ref()
                .map(|docs| docs.update(cx, |docs, cx| docs.drive_listing(cx)))
        } else {
            None
        };
        let pane_width = if narrow { width } else { width - sidebar - 5. };
        let document_width = if split { pane_width / 2. } else { pane_width };
        let node_panel = (!self.drive_selected && !document_selected)
            .then(|| self.selected_node_panel(width < 360., cx))
            .flatten();
        let showing_node = node_panel.is_some();
        let document = if split || document_selected {
            docs.as_ref()
                .map(|docs| docs.update(cx, |docs, cx| docs.document_panel(document_width, cx)))
        } else {
            None
        };
        let sidebar_tabs_focus = self
            .sidebar_tabs_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        let content_tabs_focus = self
            .content_tabs_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        let content_tabs = docs.as_ref().map(|docs| {
            let unread = docs.read(cx).unseen_document();
            div()
                .flex()
                .gap_2()
                .px_3()
                .py_1()
                .child(
                    Tabs::new("local-content-tabs")
                        .track_focus(&content_tabs_focus)
                        .flex()
                        .gap_2()
                        .child(
                            tab("local-terminal-tab", "Terminal", !document_selected).on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.node_details_requested = false;
                                    if let Some(docs) = &this.documents {
                                        docs.update(cx, |docs, _| docs.select_terminal());
                                    }
                                    this.focus_terminal_input(window, cx);
                                    cx.notify();
                                }),
                            ),
                        )
                        .child(
                            tab(
                                "local-document-tab",
                                if unread { "Document •" } else { "Document" },
                                document_selected,
                            )
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    if let Some(docs) = &this.documents {
                                        docs.update(cx, |docs, cx| {
                                            docs.select_document(window, cx)
                                        });
                                    }
                                    cx.notify();
                                },
                            )),
                        ),
                )
                .when(
                    workspace_wide && self.selection.is_some() && docs.read(cx).has_document(),
                    |bar| {
                        bar.child(button("local-split-tab", "Side by side", split).on_click(
                            cx.listener(|this, _, _, cx| {
                                if let Some(docs) = &this.documents {
                                    docs.update(cx, |docs, _| docs.select_split());
                                }
                                cx.notify();
                            }),
                        ))
                    },
                )
        });
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
            // Leave the PTY's plain Tab and Shift+Tab handling intact.
            .capture_key_down(|event, window, cx| {
                crate::workspace_tab_navigation(event, window, cx, true)
            })
            .on_key_down(|event, window, cx| {
                crate::workspace_tab_navigation(event, window, cx, false)
            })
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
                tree.when(docs.is_some(), |tree| {
                    tree.child(
                        Tabs::new("local-sidebar-tabs")
                            .track_focus(&sidebar_tabs_focus)
                            .flex()
                            .gap_2()
                            .pb_2()
                            .child(
                                tab("local-sessions-tab", "Sessions", !self.drive_selected)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.drive_selected = false;
                                        if let Some(focus) = &this.tree_focus { focus.focus(window, cx); }
                                        cx.notify();
                                    })),
                            )
                            .child(
                                tab("local-drive-tab", "Drive", self.drive_selected).on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.drive_selected = true;
                                        if let Some(docs) = &this.documents {
                                            docs.update(cx, |docs, cx| docs.select_drive(window, cx));
                                        }
                                        cx.notify();
                                    }),
                                ),
                            ),
                    )
                })
                .when_some(drive, |tree, drive| tree.child(drive))
                .when(!self.drive_selected, |tree| {
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
                                action_button("refresh", "Refresh", false)
                                    .track_focus(&refresh_focus)
                                    .on_click(cx.listener(|this, event, window, cx| {
                                        this.refresh();
                                        this.refresh_nodes();
                                        if !matches!(event, gpui::ClickEvent::Keyboard(_)) {
                                            if let Some(focus) = &this.tree_focus {
                                                focus.focus(window, cx);
                                            }
                                        }
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
                                            this.focus_search(window, cx);
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
                                        focus.focus(window, cx);
                                    }
                                    cx.notify();
                                }),
                            ))
                            .child(
                                button("tree-help", "Keys ?", show_help).on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.show_help = !this.show_help;
                                        if let Some(focus) = &this.tree_focus {
                                            focus.focus(window, cx);
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
                        d.child(crate::scroll_area(
                            &self.tree_scroll,
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
                                        div()
                                            .p_2()
                                            .text_sm()
                                            .child("No matching sessions or nodes"),
                                    )
                                }),
                        ))
                    })
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
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .when_some(content_tabs, |d, tabs| d.child(tabs))
                    .when_some(node_panel, |d, panel| d.child(panel))
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_h_0()
                            .min_w_0()
                            .when(!showing_node && (split || !document_selected), |d| {
                                d.child(
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
                                            d.child(
                                                div()
                                                    .text_sm()
                                                    .text_color(rgb(0xb5beca))
                                                    .child(detail),
                                            )
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
                                                                || (!current
                                                                    && self
                                                                        .preview_received
                                                                        .is_some())
                                                            {
                                                                0xe7c279
                                                            } else {
                                                                0xb5beca
                                                            },
                                                        ))
                                                        .child(snapshot),
                                                )
                                                .when(live, |d| {
                                                    d.child(
                                                        action_button("detach-session", "Detach", true)
                                                            .track_focus(&session_action_focus)
                                                            .on_click(cx.listener(
                                                                |this, _, window, cx| {
                                                                    this.detach();
                                                                    if let Some(focus) =
                                                                        &this.tree_focus
                                                                    {
                                                                        focus.focus(window, cx);
                                                                    }
                                                                    cx.notify();
                                                                },
                                                            )),
                                                    )
                                                })
                                                .when(!live && current, |d| {
                                                    d.child(
                                                        action_button("attach-session", "Compose", true)
                                                            .track_focus(&session_action_focus)
                                                            .on_click(cx.listener(
                                                                |this, _, window, cx| {
                                                                    this.attach_input(
                                                                        window, cx, true,
                                                                    )
                                                                },
                                                            )),
                                                    )
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
                                                .border_color(rgb(if live {
                                                    0x799ebe
                                                } else {
                                                    0x262e3a
                                                }))
                                                .track_focus(&input_focus)
                                                .on_key_down(cx.listener(Self::live_key))
                                                .on_scroll(cx.listener(
                                                    |this, event, _, cx| {
                                                        this.scroll_terminal(event, cx)
                                                    },
                                                ))
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_h_0()
                                                        .relative()
                                                        .overflow_hidden()
                                                        .child(crate::terminal_view::measure(
                                                            self.viewport.clone(),
                                                        ))
                                                        .child(
                                                            div()
                                                                .id("terminal-cells")
                                                                .on_mouse_down(MouseButton::Left, cx.listener(
                                                                    |this, _, window, cx| {
                                                                        this.focus_terminal_input(window, cx);
                                                                    },
                                                                ))
                                                                .size_full()
                                                                .overflow_scroll()
                                                                .when_some(
                                                                    self.preview.as_ref(),
                                                                    |d, screen| {
                                                                        d.child(screen.view(
                                                                            &self.terminal_selection,
                                                                            open_terminal_link.clone(),
                                                                        ))
                                                                    },
                                                                ),
                                                        ),
                                                )
                                                .when_some(self.line_input.as_ref(), |d, editor| {
                                                    d.child(editor.view(cx))
                                                }),
                                        ),
                                )
                            })
                            .when_some(document, |d, doc| d.child(doc)),
                    ),
            )
    }
}

fn tab(id: &'static str, text: &'static str, active: bool) -> Tab {
    Tab::new(id)
        .debug_selector(move || id.into())
        .selected(active)
        .styles(|style| style.selected(|style| style.bg(rgb(0x233e57))))
        .flex_none()
        .px_2()
        .py_1()
        .rounded_sm()
        .text_xs()
        .bg(rgb(0x252d38))
        .text_color(rgb(0xe8eaf0))
        .cursor_pointer()
        .hover(|d| d.bg(rgb(0x34445a)))
        .focus_visible(|d| d.bg(rgb(0x34445a)).text_color(rgb(0xffffff)))
        .child(text)
}

fn action_button(id: &'static str, text: &'static str, emphasized: bool) -> Button {
    Button::new(id)
        .debug_selector(move || id.into())
        .flex_none()
        .px_2()
        .py_1()
        .rounded_sm()
        .text_xs()
        .line_height(gpui::TextStyle::default().line_height)
        .bg(rgb(if emphasized { 0x233e57 } else { 0x252d38 }))
        .text_color(rgb(0xe8eaf0))
        .cursor_pointer()
        .hover(|d| d.bg(rgb(0x34445a)))
        .focus_visible(|d| d.bg(rgb(0x34445a)).text_color(rgb(0xffffff)))
        .child(text)
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

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_core::registry::TeamLeader;

    struct MeterLayout;

    impl Render for MeterLayout {
        fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let compact = window.viewport_size().width < px(360.);
            let disk = remuda_core::protocol::ResourceUsage {
                used_bytes: (362.8 * 1_073_741_824.) as u64,
                total_bytes: (460.4 * 1_073_741_824.) as u64,
            };
            div()
                .size_full()
                .font_family("Menlo")
                .child(render_node_metric(
                    remuda_native::node_status::Metric {
                        label: "CPU",
                        percent: Some(15),
                        capacity: None,
                    },
                    false,
                    compact,
                ))
                .child(render_node_metric(
                    remuda_native::node_status::Metric {
                        label: "System disk",
                        percent: Some(79),
                        capacity: Some(disk),
                    },
                    false,
                    compact,
                ))
        }
    }

    #[gpui::test]
    fn capacity_value_stays_on_one_line_at_narrow_and_wide_widths(cx: &mut gpui::TestAppContext) {
        let (_, cx) = cx.add_window_view(|_, _| MeterLayout);
        for width in [260., 320., 420., 720.] {
            cx.simulate_resize(gpui::size(px(width), px(300.)));
            let cpu = cx.debug_bounds("node-metric-value-CPU").unwrap();
            let disk = cx.debug_bounds("node-metric-value-System disk").unwrap();
            assert_eq!(disk.size.height, cpu.size.height, "viewport {width}");
            assert!(disk.right() <= px(width), "viewport {width}: {disk:?}");
        }
    }

    #[test]
    fn team_rows_follow_exact_leaders_with_nested_members_and_no_cycle_loss() {
        let row = |name: &str, leader: Option<(&str, &str)>| Session {
            name: name.into(),
            instance_id: Some(name.into()),
            alive: true,
            unavailable: false,
            decoration: None,
            leader: leader.map(|(name, instance)| TeamLeader {
                name: name.into(),
                instance_id: instance.into(),
            }),
        };
        let rows = [
            row("grandchild", Some(("member", "member"))),
            row("stale", Some(("lead", "old-instance"))),
            row("member", Some(("lead", "lead"))),
            row("other-member", Some(("lead", "lead"))),
            row("lead", None),
            row("cycle-a", Some(("cycle-b", "cycle-b"))),
            row("cycle-b", Some(("cycle-a", "cycle-a"))),
        ];
        assert_eq!(
            team_order(&rows),
            vec![(1, 0), (4, 0), (2, 1), (0, 2), (3, 1), (5, 0), (6, 0)]
        );
    }

    #[test]
    fn worker_preserves_peer_team_relations_without_affecting_other_nodes() {
        let peer_row = |name: &str, instance: &str, team: serde_json::Value| {
            serde_json::from_value(serde_json::json!({
                "name": name, "instance_id": instance, "alive": true,
                "idle": {"secs": 0, "nanos": 0},
                "size": {"cols": 80, "rows": 24}, "attached": false, "team": team
            }))
            .unwrap()
        };
        let member = peer_row(
            "member",
            "member-instance",
            serde_json::json!({"depth": 1, "leader": {
                "name": "lead", "instance_id": "lead-instance"
            }}),
        );
        let leader = peer_row("lead", "lead-instance", serde_json::Value::Null);
        let sessions = session_rows(vec![member, leader], None);
        let payload = serde_json::to_vec(&sessions).unwrap();
        let decoded: Vec<Session> = serde_json::from_slice(&payload).unwrap();
        assert_eq!(team_order(&decoded), vec![(1, 0), (0, 1)]);
        assert_eq!(team_order(&decoded[..1]), vec![(0, 0)]);
    }
}
