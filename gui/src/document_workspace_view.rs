//! The GUI document panel: tabs, a side-by-side split, rendered Markdown and
//! inert code colour.
//!
//! Every document is a [`documents::Pane`] with its own browser, so the panel
//! owns only presentation. The host supplies the workspace and keeps it alive
//! across frames; this module never reads the VFS itself and never executes
//! Markdown, a link or a code sample.

use gpui::{
    div, img, prelude::*, px, rgb, AnyElement, Context, Div, FocusHandle, FontStyle, FontWeight,
    HighlightStyle, ImageSource, KeyDownEvent, Pixels, SharedString, Size, StyledText, Window,
    UnderlineStyle,
};
use remuda_gui::documents::{ImageLimit, ImageState, Pane, Workspace, MAX_COLUMNS, MAX_ROWS};
use remuda_gui::{highlight, markdown, media};
use remuda_native::document_html::Target;
use std::ops::Range;
use std::{cell::RefCell, collections::HashMap, rc::Rc};
use crate::rich_selection::{DocumentKey, Selection};
pub type SelectionMap = Rc<RefCell<HashMap<u64, Rc<RefCell<Selection>>>>>;

/// Most Markdown blocks one pane paints; the parser allows more so a caller can
/// still inspect the whole document.
const VIEW_MAX_BLOCKS: usize = 1200;
const MAX_SELECTABLE_BYTES: usize = 64 * 1024;
const MAX_SELECTABLE_BLOCKS: usize = 200;
#[cfg(target_os = "macos")]
const CODE_FONT: &str = "Menlo";
#[cfg(not(target_os = "macos"))]
const CODE_FONT: &str = "monospace";

/// The host owns the workspace; the panel only renders and edits it.
pub trait DocumentHost {
    fn rich_selections(&self) -> Option<SelectionMap> { None }
    fn documents(&self) -> Option<&Workspace>;
    fn documents_mut(&mut self) -> Option<&mut Workspace>;
    fn select_document_source(&mut self, _window: &mut Window, _cx: &mut Context<Self>) where Self: Sized {}
}

/// The panel shown beside the terminal. The caller renders it only while a
/// workspace is open.
pub fn render<T: DocumentHost + 'static>(
    host: &T,
    focus: &FocusHandle,
    divider_focus: &FocusHandle,
    panel_width: f32,
    viewport: Size<Pixels>,
    narrow: bool,
    pane_focused: bool,
    cx: &mut Context<T>,
) -> AnyElement {
    let Some(workspace) = host.documents() else {
        return div().into_any_element();
    };
    let width = panel_width.clamp(160.0, 900.0);
    let columns = ((width / 8.0) as usize).clamp(20, MAX_COLUMNS);
    let rows = ((f32::from(viewport.height) - 120.0) / 17.0).max(6.0) as usize;
    let rows = rows.clamp(6, MAX_ROWS);
    let divider_focus = divider_focus.clone();
    let workspace_focus = focus.clone();
    let active = workspace.active_index();
    let split = workspace.split_index();
    let selection_map = host.rich_selections();
    if let Some(map) = &selection_map {
        map.borrow_mut().retain(|id, state| {
            let present = workspace.tabs().iter().any(|pane| pane.selection_id() == *id);
            if *id != workspace.active().selection_id() { state.borrow_mut().clear(); }
            present
        });
    }
    let tabs = render_tabs(workspace, cx);
    let controls = render_controls(narrow, cx);
    // Root padding plus a small scrollbar allowance. Images fit inside this
    // width and never extend under a vertical scrollbar.
    let pane_width = pane_content_width(width, split.is_some());
    let can_select_source = workspace.active().is_document() && !workspace.active().is_image();
    let has_source = workspace.active().source_text().is_some();
    let mut panes = div()
        .flex()
        .flex_row()
        .flex_1()
        .min_h(px(0.))
        .gap_2()
        .child(render_pane(
            workspace,
            active,
            "left",
            columns,
            if split.is_some() { rows / 2 } else { rows },
            pane_width,
            focus,
            selection_map.clone(),
            cx,
        ));
    if let Some(index) = split {
        panes = panes.child(render_pane(
            workspace,
            index,
            "right",
            columns,
            rows / 2,
            pane_width,
            focus,
            selection_map.clone(),
            cx,
        ));
    }
    div()
        .id("document-workspace")
        .debug_selector(|| "document-workspace".into())
        .w(px(width))
        .min_w(px(160.))
        .h_full()
        .flex()
        .flex_col()
        .bg(rgb(0x151920))
        .p_3()
        .track_focus(focus)
        .on_mouse_down(gpui::MouseButton::Left, cx.listener(move |_, _, window, cx| {
            workspace_focus.focus(window);
            cx.notify();
        }))
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
            let modifiers = &event.keystroke.modifiers;
            if event.keystroke.key == "c" && modifiers.platform && !modifiers.control && !modifiers.alt && !modifiers.shift && !modifiers.function {
                if let (Some(map), Some(workspace)) = (this.rich_selections(), this.documents()) {
                    let pane = workspace.active();
                    if let Some(version) = pane.source_identity() {
                        let key = DocumentKey { pane_id: pane.selection_id(), path: pane.location().to_owned(), version, epoch: pane.selection_epoch() };
                        if let Some(state) = map.borrow().get(&key.pane_id) {
                            if let Some(text) = state.borrow().copy(&key) { cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.to_owned())); }
                        }
                    }
                }
                cx.stop_propagation(); return;
            }
            if modifiers.control || modifiers.platform || modifiers.alt || modifiers.function
                || (modifiers.shift && !matches!(event.keystroke.key.as_str(), "tab" | "backtab")) {
                return;
            }
            if !narrow && event.keystroke.key.as_str() == "d" {
                divider_focus.focus(window);
                return;
            }
            if event.keystroke.key.as_str() == "s" {
                this.select_document_source(window, cx);
                cx.stop_propagation();
                cx.notify();
                return;
            }
            if let Some(workspace) = this.documents_mut() {
                let pane = workspace.active_mut();
                pane.note_key();
                match event.keystroke.key.as_str() {
                    "up" => pane.move_selection(false),
                    "down" => pane.move_selection(true),
                    "enter" => pane.open_selected(),
                    "escape" => {
                        pane.back();
                    }
                    "tab" if modifiers.shift => pane.select_link(false),
                    "tab" | "l" => pane.select_link(true),
                    "backtab" | "h" => pane.select_link(false),
                    "r" => pane.refresh(),
                    _ => {}
                }
            }
            cx.notify();
        }))
        .on_mouse_up(gpui::MouseButton::Left, cx.listener({ let selection_map = selection_map.clone(); move |_, _, _, _| {
            if let Some(map) = &selection_map { for state in map.borrow().values() { state.borrow_mut().end(); } }
        }}))
        .on_mouse_up_out(gpui::MouseButton::Left, cx.listener({ let selection_map = selection_map.clone(); move |_, _, _, _| {
            if let Some(map) = &selection_map { for state in map.borrow().values() { state.borrow_mut().end(); } }
        }}))
        .child(tabs)
        .when(can_select_source, |d| d.child(
            div().id("select-document-text").debug_selector(|| "select-document-text".into())
                .py_1().cursor_pointer()
                .when(pane_focused, |d| d.bg(rgb(0x34445a)))
                .child(if has_source { "Select source (S)" } else { "Source unavailable · Refresh loads current version" })
                .on_click(cx.listener(|this, _, window, cx| { this.select_document_source(window, cx); cx.notify(); }))
        ))
        .child(panes)
        .when(!narrow, |d| d.child(div().text_sm().child("D: divider · then Q: close documents and return to session")))
        .child(controls)
        .into_any_element()
}

fn render_tabs<T: DocumentHost + 'static>(
    workspace: &Workspace,
    cx: &mut Context<T>,
) -> AnyElement {
    let active = workspace.active_index();
    let split = workspace.split_index();
    let mut tabs = Vec::new();
    for index in 0..workspace.len() {
        let title = workspace.title(index);
        let selected = index == active;
        let close_id = SharedString::from(format!("document-tab-close-{index}"));
        let tab = div()
            .id(SharedString::from(format!("document-tab-{index}")))
            .debug_selector(move || format!("document-tab-{index}"))
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .cursor_pointer()
            .when(selected, |d| d.bg(rgb(0x34445a)))
            .child(div().max_w(px(150.)).overflow_hidden().child(title))
            .child(
                div()
                    .id(close_id)
                    .debug_selector(move || format!("document-tab-close-{index}"))
                    .cursor_pointer()
                    .px_1()
                    .child("×")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(workspace) = this.documents_mut() {
                            workspace.close(index);
                        }
                        cx.stop_propagation();
                        cx.notify();
                    })),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(workspace) = this.documents_mut() {
                    workspace.activate(index);
                }
                cx.notify();
            }));
        tabs.push(tab.into_any_element());
    }
    tabs.push(
        div()
            .id("document-new-tab")
            .debug_selector(|| "document-new-tab".into())
            .cursor_pointer()
            .px_2()
            .py_1()
            .child("+ new")
            .on_click(cx.listener(|this, _, _, cx| {
                if let Some(workspace) = this.documents_mut() {
                    workspace.open_tab();
                }
                cx.notify();
            }))
            .into_any_element(),
    );
    tabs.push(
        div()
            .id("document-split")
            .debug_selector(|| "document-split".into())
            .cursor_pointer()
            .px_2()
            .py_1()
            .child(if split.is_some() { "Unsplit" } else { "Split" })
            .on_click(cx.listener(|this, _, _, cx| {
                if let Some(workspace) = this.documents_mut() {
                    workspace.toggle_split();
                }
                cx.notify();
            }))
            .into_any_element(),
    );
    div()
        .id("document-tabs")
        .debug_selector(|| "document-tabs".into())
        .flex()
        .flex_wrap()
        .gap_1()
        .pb_2()
        .children(tabs)
        .into_any_element()
}

fn render_controls<T: DocumentHost + 'static>(narrow: bool, cx: &mut Context<T>) -> AnyElement {
    div()
        .id("document-controls")
        .debug_selector(|| "document-controls".into())
        .flex()
        .flex_wrap()
        .pt_2()
        .children(
            [
                ("doc-up", "Up", 0u8),
                ("doc-down", "Down", 1),
                ("doc-open", "Open", 2),
                ("doc-back", "Back", 3),
                ("doc-prev-link", "Prev link", 4),
                ("doc-next-link", "Next link", 5),
                ("doc-refresh", "Refresh", 6),
                ("doc-new-tab", "New tab", 7),
                ("doc-split", "Split", 8),
                ("doc-close-tab", "Close tab", 9),
            ]
            .into_iter()
            .filter(move |(_, _, action)| !narrow || *action < 7)
            .map(|(id, text, action)| {
                div()
                    .id(id)
                    .debug_selector(move || id.to_string())
                    .py_1()
                    .pr_3()
                    .cursor_pointer()
                    .child(text)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(workspace) = this.documents_mut() {
                            match action {
                                0 => workspace.active_mut().move_selection(false),
                                1 => workspace.active_mut().move_selection(true),
                                2 => workspace.active_mut().open_selected(),
                                3 => {
                                    workspace.active_mut().back();
                                }
                                4 => workspace.active_mut().select_link(false),
                                5 => workspace.active_mut().select_link(true),
                                6 => workspace.active_mut().refresh(),
                                7 => {
                                    workspace.open_tab();
                                }
                                8 => {
                                    workspace.toggle_split();
                                }
                                _ => {
                                    let active = workspace.active_index();
                                    workspace.close(active);
                                }
                            }
                        }
                        cx.notify();
                    }))
            }),
        )
        .into_any_element()
}

fn render_pane<T: DocumentHost + 'static>(
    workspace: &Workspace,
    index: usize,
    tag: &'static str,
    columns: usize,
    rows: usize,
    pane_width: f32,
    focus: &FocusHandle,
    selection_map: Option<SelectionMap>,
    cx: &mut Context<T>,
) -> AnyElement {
    let Some(pane) = workspace.pane(index) else {
        return div().into_any_element();
    };
    let (pane_id, frame_id) = match tag {
        "left" => ("document-pane-left", "document-frame-left"),
        _ => ("document-pane-right", "document-frame-right"),
    };
    let status = pane.status();
    let rich_selectable = pane.source_identity().is_some() && pane.source_text().is_some_and(|source| {
        source.len() <= MAX_SELECTABLE_BYTES && if let Some(markdown) = pane.markdown_source() {
            markdown::parse(markdown).len() <= MAX_SELECTABLE_BLOCKS
        } else {
            !source.ends_with('\r') && !pane.location().to_ascii_lowercase().ends_with(".html") && !pane.location().to_ascii_lowercase().ends_with(".htm") && source.split('\n').count() <= MAX_SELECTABLE_BLOCKS && source.split('\n').all(|line| {
                let line = line.strip_suffix('\r').unwrap_or(line);
                line.split('\t').all(|part| remuda_native::text::strip_terminal_controls(part) == part)
            })
        }
    });
    if !rich_selectable { if let Some(map) = &selection_map { map.borrow_mut().remove(&pane.selection_id()); } }
    let selection = selection_map.filter(|_| rich_selectable).and_then(|map| pane.source_identity().map(|version| {
        let key = DocumentKey { pane_id: pane.selection_id(), path: pane.location().to_owned(), version, epoch: pane.selection_epoch() };
        let selection = map.borrow_mut().entry(key.pane_id).or_default().clone();
        selection.borrow_mut().begin_document(key.clone());
        (selection, key)
    }));
    let body = pane_body(pane, index, tag, columns, rows, pane_width, selection.clone(), cx);
    let location_id = format!("vfs-location-{tag}");
    let directory_location = pane
        .is_directory()
        .then(|| format!("remuda:///{}", remuda_native::documents::path_label(pane.location())));
    let pane_focus = focus.clone();
    let move_selection = selection.clone();
    div()
        .id(pane_id)
        .debug_selector(move || pane_id.into())
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(0.))
        .min_h(px(0.))
        .when_some(directory_location, |d, location| {
            d.child(
                div()
                    .id(SharedString::from(location_id.clone()))
                    .debug_selector(move || location_id.clone())
                    .text_sm()
                    .text_color(rgb(0xc9d4e4))
                    .pb_1()
                    .child(location),
            )
        })
        .child(
            div()
                .text_sm()
                .text_color(rgb(0x8a95a5))
                .pb_1()
                .child(status)
                .when(pane.is_document() && !pane.is_image() && !rich_selectable, |d| d.child(div().id("rich-selection-limit").debug_selector(|| "rich-selection-limit".into()).child("Direct selection requires loaded identity, safe text up to 64 KiB, and 200 blocks or lines. Use Select source (S)."))),
        )
        .child(
            div()
                .id(SharedString::from(frame_id))
                .debug_selector(move || frame_id.into())
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .on_mouse_move(cx.listener(move |_, event: &gpui::MouseMoveEvent, _, _| {
                    if let Some((state,_)) = &move_selection {
                        let mut state = state.borrow_mut();
                        if state.dragging && event.pressed_button == Some(gpui::MouseButton::Left) { state.note_pointer(f32::from(event.position.x), f32::from(event.position.y)); }
                    }
                }))
                .on_click(cx.listener(move |this, _, window, cx| {
                    if let Some(workspace) = this.documents_mut() {
                        workspace.focus(index);
                    }
                    pane_focus.focus(window);
                    cx.notify();
                }))
                .child(body),
        )
        .into_any_element()
}

/// A directory row shows only the file or folder name; identity stays in
/// `Entry::path`, which click, Enter and stale checks use.
fn vfs_row_label(entry: &remuda_native::documents::Entry) -> String {
    entry.display_name()
}

fn pane_body<T: DocumentHost + 'static>(
    pane: &Pane,
    pane_index: usize,
    tag: &'static str,
    columns: usize,
    rows: usize,
    pane_width: f32,
    selection: Option<(Rc<RefCell<Selection>>, DocumentKey)>,
    cx: &mut Context<T>,
) -> AnyElement {
    if let Some((generation, directory, entries, selected)) = pane.directory_entries() {
        let directory = directory.to_owned();
        let rows = entries.iter().enumerate().map(|(index, entry)| {
            let path = entry.path.clone();
            let shown = vfs_row_label(entry);
            let is_directory = entry.directory;
            let parent = directory.clone();
            div().id(SharedString::from(format!("vfs-entry-{tag}-{index}")))
                .debug_selector(move || format!("vfs-entry-{tag}-{index}"))
                .cursor_pointer().py_1().when(index == selected, |d| d.bg(rgb(0x34445a)))
                .child(format!("{} {}", if is_directory { "▸" } else { "·" }, shown))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(workspace) = this.documents_mut() {
                        workspace.focus(pane_index);
                        if let Some(pane) = workspace.pane_mut(pane_index) {
                            pane.open_directory_entry(generation, &parent, index, &path, is_directory);
                        }
                    }
                    cx.stop_propagation(); cx.notify();
                })).into_any_element()
        }).collect::<Vec<_>>();
        return div().id(SharedString::from(format!("vfs-list-{tag}"))).size_full().overflow_y_scroll().track_scroll(&pane.directory_scroll()).flex().flex_col().text_sm().when(rows.is_empty(), |d| d.child("(empty)")).children(rows).into_any_element();
    }
    if pane.is_image() {
        return image_body(pane, tag, pane_width);
    }
    if let Some(source) = pane.markdown_source() {
        return markdown_body(source, pane, pane_index, tag, selection, cx, pane_width)
            .into_any_element();
    }
    let language = pane.language();
    if let (Some(source), Some(selection)) = (pane.source_text(), selection.clone()) {
        let mut previous_cr = false;
        let lines = source.split('\n').enumerate().map(|(row,line)| {
            if row > 0 { selection.0.borrow_mut().add_piece(if previous_cr { "\r\n" } else { "\n" }); }
            previous_cr = line.ends_with('\r');
            let line = line.strip_suffix('\r').unwrap_or(line);
            let highlights = language.map(|lang| highlight::tokens(line,lang).into_iter().map(|token| (token.start..token.end,token_style(token.kind))).collect()).unwrap_or_default();
            let selector = format!("document-line-{tag}-{row}");
            div().id(SharedString::from(selector.clone())).debug_selector(move || selector.clone()).child(selectable_text(line.to_owned(),highlights,Some(selection.clone()),pane_index,cx)).into_any_element()
        }).collect::<Vec<_>>();
        return div().flex().flex_col().font_family(CODE_FONT).text_sm().whitespace_normal().children(lines).into_any_element();
    }
    div()
        .flex()
        .flex_col()
        .font_family(CODE_FONT)
        .text_sm()
        .whitespace_nowrap()
        .children(
            pane.frame(columns, rows)
                .into_iter()
                .enumerate()
                .map(|(row, line)| code_line(tag, &line, language, row).into_any_element()),
        )
        .into_any_element()
}

fn markdown_body<T: DocumentHost + 'static>(
    source: &str,
    pane: &Pane,
    pane_index: usize,
    tag: &'static str,
    selection: Option<(Rc<RefCell<Selection>>, DocumentKey)>,
    cx: &mut Context<T>,
    pane_width: f32,
) -> Div {
    let blocks = selection.as_ref().map(|(state,_)| state.borrow_mut().blocks(source)).unwrap_or_else(|| markdown::parse(source));
    let total = blocks.len();
    let mut children: Vec<AnyElement> = blocks
        .into_iter()
        .take(VIEW_MAX_BLOCKS)
        .enumerate()
        .map(|(index, block)| {
            if index > 0 { if let Some((state,_)) = &selection { state.borrow_mut().add_piece("\n"); } }
            markdown_block(pane, pane_index, tag, index, &block, selection.clone(), cx, pane_width)
        })
        .collect();
    if total > VIEW_MAX_BLOCKS {
        children.push(
            div()
                .text_sm()
                .text_color(rgb(0x8a95a5))
                .child(format!(
                    "Display limited to {VIEW_MAX_BLOCKS} of {total} blocks."
                ))
                .into_any_element(),
        );
    }
    div().flex().flex_col().gap_1().text_sm().children(children)
}

fn markdown_block<T: DocumentHost + 'static>(
    pane: &Pane,
    pane_index: usize,
    tag: &'static str,
    index: usize,
    block: &markdown::Block,
    selection: Option<(Rc<RefCell<Selection>>, DocumentKey)>,
    cx: &mut Context<T>,
    pane_width: f32,
) -> AnyElement {
    match &block.kind {
        markdown::BlockKind::Heading(level) => {
            let selector = format!("md-h{level}-{tag}-{index}");
            let mut heading = div()
                .id(SharedString::from(selector.clone()))
                .debug_selector(move || selector.clone())
                .font_weight(FontWeight::BOLD)
                .pt_2();
            heading = match level {
                1 => heading.text_3xl(),
                2 => heading.text_2xl(),
                3 => heading.text_xl(),
                4 => heading.text_lg(),
                5 => heading.text_base(),
                _ => heading.text_sm(),
            };
            heading
                .child(render_spans(&block.spans, pane, pane_index, tag, index, selection.clone(), cx, pane_width))
                .into_any_element()
        }
        markdown::BlockKind::Paragraph => {
            let selector = format!("md-p-{tag}-{index}");
            div()
                .id(SharedString::from(selector.clone()))
                .debug_selector(move || selector.clone())
                .child(render_spans(&block.spans, pane, pane_index, tag, index, selection.clone(), cx, pane_width))
                .into_any_element()
        }
        markdown::BlockKind::List { level, .. } => {
            let selector = format!("md-list-{tag}-{index}");
            let content_width = (pane_width - 12.0 * *level as f32 - 24.0).max(1.0);
            let mut row = div()
                .id(SharedString::from(selector.clone()))
                .debug_selector(move || selector.clone())
                .flex()
                .pl(px(12.0 * *level as f32));
            row = row.child(
                div()
                    .w(px(24.))
                    .text_color(rgb(0x8a95a5))
                    .child(selectable_text(format!("{} ",block.marker), vec![], selection.clone(), pane_index, cx)),
            );
            row.child(render_spans(&block.spans, pane, pane_index, tag, index, selection.clone(), cx, content_width))
                .into_any_element()
        }
        markdown::BlockKind::Quote => {
            let selector = format!("md-quote-{tag}-{index}");
            let content_width = (pane_width - 8.0).max(1.0);
            div()
                .id(SharedString::from(selector.clone()))
                .debug_selector(move || selector.clone())
                .pl_2()
                .bg(rgb(0x1b2028))
                .text_color(rgb(0xb7c0cc))
                .child(render_spans(&block.spans, pane, pane_index, tag, index, selection.clone(), cx, content_width))
                .into_any_element()
        }
        markdown::BlockKind::Code { language } => {
            let selector = format!("md-code-{tag}-{index}");
            let language = language.as_deref().and_then(normalize_language);
            div()
                .id(SharedString::from(selector.clone()))
                .debug_selector(move || selector.clone())
                .bg(rgb(0x10141a))
                .p_2()
                .overflow_x_scroll()
                .font_family(CODE_FONT)
                .text_sm()
                .whitespace_nowrap()
                .children(
                    block
                        .code
                        .iter()
                        .enumerate()
                        .map(|(row, line)| {
                            if row > 0 { if let Some((state,_)) = &selection { state.borrow_mut().add_piece("\n"); } }
                            let highlights = language.map(|language| highlight::tokens(line,language).into_iter().map(|token| (token.start..token.end,token_style(token.kind))).collect()).unwrap_or_default();
                            selectable_text(line.clone(), highlights, selection.clone(), pane_index, cx)
                        }),
                )
                .into_any_element()
        }
        markdown::BlockKind::Rule => {
            let selector = format!("md-rule-{tag}-{index}");
            div()
                .id(SharedString::from(selector.clone()))
                .debug_selector(move || selector.clone())
                .py_1()
                .child(div().h(px(1.)).w_full().bg(rgb(0x34445a)))
                .into_any_element()
        }
    }
}

/// One display line: inert token colours for known code, plain text otherwise.
fn code_line(tag: &'static str, line: &str, language: Option<&str>, row: usize) -> AnyElement {
    let selector = format!("document-line-{tag}-{row}");
    let highlights: Vec<(Range<usize>, HighlightStyle)> = language
        .map(|language| {
            highlight::tokens(line, language)
                .into_iter()
                .map(|token| (token.start..token.end, token_style(token.kind)))
                .collect()
        })
        .unwrap_or_default();
    div()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .child(StyledText::new(line.to_owned()).with_highlights(highlights))
        .into_any_element()
}

/// Content width available inside one document pane. The floor must not turn
/// a genuinely narrow split pane wider than the space it actually has.
fn pane_content_width(panel_width: f32, split: bool) -> f32 {
    let inner_width = (panel_width - 32.0).max(120.0);
    if split {
        ((inner_width - 8.0) / 2.0 - 8.0).max(1.0)
    } else {
        inner_width
    }
}

/// Display size for one image. It fits the available pane width, never upscales
/// the decoded image and preserves the decoded aspect ratio. Height is free so
/// a tall image scrolls vertically instead of being clipped horizontally.
#[derive(Clone, Copy, Debug, PartialEq)]
struct DisplaySize {
    width: f32,
    height: f32,
}

fn fit_image(
    natural_width: u32,
    natural_height: u32,
    available_width: f32,
    max_width: f32,
) -> DisplaySize {
    if natural_width == 0 || natural_height == 0 {
        return DisplaySize {
            width: 0.0,
            height: 0.0,
        };
    }
    let width = (natural_width as f32)
        .min(available_width.max(1.0))
        .min(max_width.max(1.0));
    DisplaySize {
        width,
        height: width * natural_height as f32 / natural_width as f32,
    }
}

fn image_body(pane: &Pane, tag: &'static str, pane_width: f32) -> AnyElement {
    let selector = format!("document-image-{tag}");
    match pane.image(pane.location()) {
        Some(ImageState::Ready { image, width, height }) => {
            let size = fit_image(*width, *height, pane_width, 680.0);
            div()
                .id(SharedString::from(selector.clone()))
                .debug_selector(move || selector.clone())
                .w(px(size.width))
                .h(px(size.height))
                .child(
                    img(ImageSource::from(image.clone()))
                        .w(px(size.width))
                        .h(px(size.height)),
                )
                .into_any_element()
        }
        Some(ImageState::Failed(error)) => div()
            .child(format!("IMAGE · {} — {error}", pane.location()))
            .into_any_element(),
        Some(ImageState::Loading) | None if pane.image_limited() => div()
            .child(format!(
                "IMAGE · {} — {}",
                pane.location(),
                pane.image_limit().unwrap_or(ImageLimit::WorkspaceEntries).message()
            ))
            .into_any_element(),
        Some(ImageState::Loading) | None => div()
            .child(format!("IMAGE · {} · loading…", pane.location()))
            .into_any_element(),
    }
}

fn image_span_element(
    pane: &Pane,
    destination: &str,
    placeholder: &str,
    tag: &'static str,
    block_index: usize,
    span_index: usize,
    pane_width: f32,
) -> AnyElement {
    let alt = placeholder
        .strip_prefix("[image: ")
        .and_then(|text| text.strip_suffix(']'))
        .unwrap_or(placeholder);
    match remuda_native::document_html::resolve(pane.location(), destination) {
        Target::Vfs(path) if media::extension_is_image(&path) => match pane.image(&path) {
            Some(ImageState::Ready { image, width, height }) => {
                let selector = format!("md-image-{tag}-{block_index}-{span_index}");
                let size = fit_image(*width, *height, pane_width, 240.0);
                div()
                    .id(SharedString::from(selector.clone()))
                    .debug_selector(move || selector.clone())
                    .w(px(size.width))
                    .h(px(size.height))
                    .child(
                        img(ImageSource::from(image.clone()))
                            .w(px(size.width))
                            .h(px(size.height)),
                    )
                    .into_any_element()
            }
            Some(ImageState::Failed(error)) => {
                let selector = format!("md-image-failed-{tag}-{block_index}-{span_index}");
                div()
                    .id(SharedString::from(selector.clone()))
                    .debug_selector(move || selector.clone())
                    .text_sm()
                    .text_color(rgb(0xf28787))
                    .child(format!("[image: {alt}] unavailable: {error}"))
                    .into_any_element()
            }
            Some(ImageState::Loading) | None if pane.image_limited() => {
                let selector = format!("md-image-failed-{tag}-{block_index}-{span_index}");
                div()
                    .id(SharedString::from(selector.clone()))
                    .debug_selector(move || selector.clone())
                    .text_sm()
                    .text_color(rgb(0x6c7086))
                    .child(format!(
                        "[image: {alt}] unavailable: {}",
                        pane.image_limit().unwrap_or(ImageLimit::WorkspaceEntries).message()
                    ))
                    .into_any_element()
            }
            Some(ImageState::Loading) | None => div()
                .text_sm()
                .text_color(rgb(0x8a95a5))
                .child(format!("[image: {alt}] loading…"))
                .into_any_element(),
        },
        Target::Vfs(_) => div()
            .text_sm()
            .text_color(rgb(0x8a95a5))
            .child(format!("[image: {alt}] unsupported image format"))
            .into_any_element(),
        Target::Blocked(_) => {
            let selector = format!("md-image-blocked-{tag}-{block_index}-{span_index}");
            div()
                .id(SharedString::from(selector.clone()))
                .debug_selector(move || selector.clone())
                .text_sm()
                .text_color(rgb(0x6c7086))
                .child(format!("[image: {alt}] (blocked)"))
                .into_any_element()
        }
    }
}

fn render_spans<T: DocumentHost + 'static>(
    spans: &[markdown::Span],
    pane: &Pane,
    pane_index: usize,
    tag: &'static str,
    block_index: usize,
    selection: Option<(Rc<RefCell<Selection>>, DocumentKey)>,
    cx: &mut Context<T>,
    pane_width: f32,
) -> AnyElement {
    if spans.iter().all(|span| span.target.is_none()) {
        let text = spans.iter().map(|s| s.text.as_str()).collect::<String>();
        let mut pos = 0;
        let highlights = spans.iter().filter_map(|s| { let start=pos; pos += s.text.len(); let style=inline_highlight(s.style); (style != HighlightStyle::default() && start < pos).then_some((start..pos, style)) }).collect();
        return selectable_text(text, highlights, selection, pane_index, cx);
    }
    let mut row = div().flex().flex_wrap();
    for (span_index, span) in spans.iter().enumerate() {
        let Some(destination) = &span.target else {
            row = row.child(selectable_text(span.text.clone(), vec![(0..span.text.len(),inline_highlight(span.style))], selection.clone(), pane_index, cx));
            continue;
        };
        if span.style.image {
            // Copy an explicit alternative for the non-text image, independent of load status.
            if let Some((state,_)) = &selection { state.borrow_mut().add_piece(&span.text); }
            row = row.child(image_span_element(
                pane,
                destination,
                &span.text,
                tag,
                block_index,
                span_index,
                pane_width,
            ));
            continue;
        }
        match remuda_native::document_html::resolve(pane.location(), destination) {
            Target::Vfs(path) => {
                let selector = format!("md-link-{tag}-{block_index}-{span_index}");
                row = row.child(
                    div()
                        .id(SharedString::from(selector.clone()))
                        .debug_selector(move || selector.clone())
                        .cursor_pointer()
                        .child(selectable_text(span.text.clone(), vec![(0..span.text.len(),inline_highlight(span.style))], selection.clone(), pane_index, cx))
                        .on_click(cx.listener({ let selection = selection.clone(); move |this, _, _, cx| {
                            if selection.as_ref().is_some_and(|(state,_)| state.borrow().moved) { return; }
                            if let Some(workspace) = this.documents_mut() {
                                if let Some(pane) = workspace.pane_mut(pane_index) {
                                    pane.open_path(&path);
                                }
                            }
                            cx.notify();
                        }})),
                );
            }
            Target::Blocked(_) => {
                let selector = format!("md-link-blocked-{tag}-{block_index}-{span_index}");
                row = row.child(
                    div()
                        .id(SharedString::from(selector.clone()))
                        .debug_selector(move || selector.clone())
                        .text_color(rgb(0x6c7086))
                        .child(selectable_text(span.text.clone(), vec![(0..span.text.len(),inline_highlight(span.style))], selection.clone(), pane_index, cx))
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(0x6c7086))
                                .child(selectable_text(" (blocked)".to_string(),vec![],selection.clone(),pane_index,cx)),
                        ),
                );
            }
        }
    }
    row.into_any_element()
}

fn selectable_text<T: DocumentHost + 'static>(text: String, highlights: Vec<(Range<usize>, HighlightStyle)>, selection: Option<(Rc<RefCell<Selection>>, DocumentKey)>, pane_index: usize, cx: &mut Context<T>) -> AnyElement {
    let Some((state, key)) = selection else { return StyledText::new(text).with_highlights(highlights).into_any_element(); };
    let range = state.borrow_mut().add_piece(&text);
    let selected = state.borrow().selected_range(range.clone());
    let mut boundaries = vec![0,text.len()];
    for (r,_) in &highlights { boundaries.extend([r.start,r.end]); }
    if let Some(r) = &selected { boundaries.extend([r.start,r.end]); }
    boundaries.sort_unstable(); boundaries.dedup();
    let mut combined = Vec::new();
    for pair in boundaries.windows(2) {
        let r = pair[0]..pair[1]; if r.is_empty() { continue; }
        let mut style = highlights.iter().find(|(h,_)| h.start <= r.start && h.end >= r.end).map(|(_,s)| s.clone()).unwrap_or_default();
        if selected.as_ref().is_some_and(|sel| sel.start <= r.start && sel.end >= r.end) { style.background_color = Some(rgb(0x405a80).into()); }
        combined.push((r,style));
    }
    let styled = StyledText::new(text).with_highlights(combined);
    let layout = styled.layout().clone(); let down_layout = layout.clone(); let down_state = state.clone(); let move_state = state.clone();
    let down_range = range.clone(); let move_range = range.clone(); let down_key = key.clone();
    let selector = format!("rich-piece-{}",range.start);
    div().id(SharedString::from(selector.clone())).debug_selector(move || selector.clone()).child(styled)
        .on_mouse_down(gpui::MouseButton::Left, cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
            if down_state.borrow().key.as_ref() != Some(&down_key) { return; }
            let at=down_layout.index_for_position(event.position).unwrap_or_else(|n| n).min(down_range.len());
            { let mut state=down_state.borrow_mut(); state.start(down_range.start + at); state.pointer_start=Some((f32::from(event.position.x),f32::from(event.position.y))); }
            if let Some(workspace) = this.documents_mut() { workspace.focus(pane_index); }
            cx.notify();
        }))
        .on_mouse_move(cx.listener(move |_, event: &gpui::MouseMoveEvent, _, cx| {
            if event.pressed_button != Some(gpui::MouseButton::Left) || !move_state.borrow().dragging { return; }
            let at=layout.index_for_position(event.position).unwrap_or_else(|n| n).min(move_range.len());
            move_state.borrow_mut().extend(move_range.start + at, false); cx.notify();
        })).into_any_element()
}

/// Map an inline Markdown style to GPUI attributes. Pure, so a test can assert
/// the exact weight/underline/background without a window.
pub fn inline_highlight(style: markdown::InlineStyle) -> HighlightStyle {
    let mut highlight = HighlightStyle::default();
    if style.bold {
        highlight.font_weight = Some(FontWeight::BOLD);
    }
    if style.italic {
        highlight.font_style = Some(FontStyle::Italic);
    }
    if style.code {
        highlight.color = Some(rgb(0xe8c07a).into());
        highlight.background_color = Some(rgb(0x2a2f3a).into());
    }
    if style.link {
        highlight.color = Some(rgb(0x82aaff).into());
        highlight.underline = Some(UnderlineStyle {
            thickness: px(1.),
            color: Some(rgb(0x82aaff).into()),
            wavy: false,
        });
    }
    if style.image {
        highlight.color = Some(rgb(0x8a95a5).into());
        highlight.font_style = Some(FontStyle::Italic);
    }
    highlight
}

fn token_style(kind: highlight::Kind) -> HighlightStyle {
    match kind {
        highlight::Kind::Keyword => rgb(0x82aaff).into(),
        highlight::Kind::String => rgb(0xa6e3a1).into(),
        highlight::Kind::Comment => rgb(0x6c7086).into(),
        highlight::Kind::Number => rgb(0xfab387).into(),
    }
}

fn normalize_language(language: &str) -> Option<&'static str> {
    Some(match language.to_ascii_lowercase().as_str() {
        "py" | "python" | "python3" => "python",
        "rs" | "rust" => "rust",
        "js" | "javascript" | "node" | "ts" | "typescript" => "javascript",
        "sh" | "bash" | "zsh" | "shell" => "shell",
        "json" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Entity, TestAppContext, VisualTestContext, Window};
    use remuda_gui::documents::{ImageBudget, MAX_IMAGES_PER_PANE};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    static DAEMON_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct TestHost {
        rich_selections: SelectionMap,
        documents: Option<Workspace>,
        focus: FocusHandle,
        panel_width: Option<f32>,
    }

    impl DocumentHost for TestHost {
        fn rich_selections(&self) -> Option<SelectionMap> { Some(self.rich_selections.clone()) }
        fn documents(&self) -> Option<&Workspace> {
            self.documents.as_ref()
        }

        fn documents_mut(&mut self) -> Option<&mut Workspace> {
            self.documents.as_mut()
        }
    }

    impl Render for TestHost {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let viewport = window.viewport_size();
            let focus = self.focus.clone();
            let width = self.panel_width.unwrap_or_else(|| {
                (f32::from(viewport.width) * 0.42).clamp(260.0, 720.0)
            });
            div().size_full().when(self.documents.is_some(), |d| {
                d.child(render(self, &focus, &focus, width, viewport, false, focus.is_focused(window), cx))
            })
        }
    }

    struct Daemon {
        bin: PathBuf,
        child: Child,
        runtime: PathBuf,
        root: PathBuf,
    }

    impl Daemon {
        fn start(bin: &Path) -> Self {
            let root = if Path::new("/private/tmp").is_dir() {
                PathBuf::from("/private/tmp")
            } else {
                std::fs::canonicalize(std::env::temp_dir()).unwrap()
            };
            let runtime = root.join(format!(
                "gwd-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let _ = std::fs::remove_dir_all(&runtime);
            std::fs::create_dir_all(runtime.join("home")).unwrap();
            let child = Command::new(bin)
                .args(["-s", "gwd", "daemon"])
                .env("REMUDA_RUNTIME_DIR", &runtime)
                .env("HOME", runtime.join("home"))
                .env("XDG_CONFIG_HOME", runtime.join("config"))
                .env("XDG_DATA_HOME", runtime.join("data"))
                .env("XDG_CACHE_HOME", runtime.join("cache"))
                .env("XDG_STATE_HOME", runtime.join("state"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
            let mut daemon = Self {
                bin: bin.to_owned(),
                child,
                runtime,
                root,
            };
            daemon.wait_ready(bin);
            daemon
        }

        fn socket(&self) -> PathBuf {
            remuda_native::daemon::socket_path_in(&self.runtime, "gwd")
        }

        fn wait_ready(&mut self, bin: &Path) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while remuda_native::client::request(
                &self.socket(),
                &remuda_core::protocol::Request::List,
            )
            .is_err()
            {
                if let Some(status) = self.child.try_wait().unwrap() {
                    panic!("private daemon exited before binding: {status}");
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(20));
            }
            let out = Command::new(bin)
                .args(["-s", "gwd", "cluster", "init", "--no-listen"])
                .env("REMUDA_RUNTIME_DIR", &self.runtime)
                .env("HOME", self.runtime.join("home"))
                .env("XDG_CONFIG_HOME", self.runtime.join("config"))
                .env("XDG_DATA_HOME", self.runtime.join("data"))
                .env("XDG_CACHE_HOME", self.runtime.join("cache"))
                .env("XDG_STATE_HOME", self.runtime.join("state"))
                .output()
                .unwrap();
            assert!(out.status.success(), "cluster init failed: {out:?}");
        }

        fn start_session(&self, bin: &Path, name: &str, command: &[&str]) {
            let mut args = vec!["-s", "gwd", "run", "-n", name, "--"];
            args.extend_from_slice(command);
            let _ = Command::new(bin)
                .args(args)
                .env("REMUDA_RUNTIME_DIR", &self.runtime)
                .env("HOME", self.runtime.join("home"))
                .env("XDG_CONFIG_HOME", self.runtime.join("config"))
                .env("XDG_DATA_HOME", self.runtime.join("data"))
                .env("XDG_CACHE_HOME", self.runtime.join("cache"))
                .env("XDG_STATE_HOME", self.runtime.join("state"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }

        fn sessions(&self, bin: &Path) -> Vec<serde_json::Value> {
            let out = Command::new(bin)
                .args(["-s", "gwd", "ls", "--json"])
                .env("REMUDA_RUNTIME_DIR", &self.runtime)
                .env("HOME", self.runtime.join("home"))
                .env("XDG_CONFIG_HOME", self.runtime.join("config"))
                .env("XDG_DATA_HOME", self.runtime.join("data"))
                .env("XDG_CACHE_HOME", self.runtime.join("cache"))
                .env("XDG_STATE_HOME", self.runtime.join("state"))
                .output()
                .unwrap();
            serde_json::from_slice(&out.stdout).unwrap_or_default()
        }

        fn session_alive(&self, bin: &Path, name: &str) -> bool {
            self.sessions(bin)
                .iter()
                .any(|row| row["name"] == name && row["alive"] == true)
        }

        fn put(&self, bin: &Path, path: &str, bytes: &[u8]) {
            use std::io::Write;
            let mut child = Command::new(bin)
                .args(["-s", "gwd", "vfs", "put", "-", &format!("remuda:///{path}")])
                .env("REMUDA_RUNTIME_DIR", &self.runtime)
                .env("HOME", self.runtime.join("home"))
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(bytes).unwrap();
            assert!(child.wait().unwrap().success(), "vfs put {path} failed");
        }
    }

    impl Drop for Daemon {
        fn drop(&mut self) {
            assert!(self.runtime.starts_with(&self.root));
            let _ = Command::new(&self.bin)
                .args(["-s", "gwd", "stop", "-f", "--yes"])
                .env("REMUDA_RUNTIME_DIR", &self.runtime)
                .env("HOME", self.runtime.join("home"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.runtime);
        }
    }

    fn remuda_bin() -> PathBuf {
        let path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../target/debug/remuda"
        ));
        assert!(
            path.is_file(),
            "build the daemon binary first: cargo build -p remuda-native --bin remuda ({})",
            path.display()
        );
        path
    }

    fn settle(view: &Entity<TestHost>, cx: &mut VisualTestContext) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            view.update(cx, |host, cx| {
                if let Some(workspace) = host.documents.as_mut() {
                    workspace.poll();
                }
                cx.notify();
            });
            cx.run_until_parked();
            let ready = cx.read(|app| {
                view.read(app).documents.as_ref().is_some_and(|workspace| {
                    workspace.tabs().iter().all(|pane| {
                        let status = pane.status();
                        !status.starts_with("LOADING") && !status.contains("IMAGE · loading")
                    })
                })
            });
            if ready {
                return;
            }
            assert!(Instant::now() < deadline, "workspace did not settle");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_selector(cx: &mut VisualTestContext, selector: &'static str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            if cx.debug_bounds(selector).is_some() {
                return;
            }
            assert!(Instant::now() < deadline, "{selector} was not painted");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_text(view: &Entity<TestHost>, cx: &mut VisualTestContext, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            view.update(cx, |host, cx| {
                if let Some(workspace) = host.documents.as_mut() {
                    workspace.poll();
                }
                cx.notify();
            });
            cx.run_until_parked();
            let found = cx.read(|app| {
                view.read(app).documents.as_ref().is_some_and(|workspace| {
                    workspace
                        .frame(workspace.active_index(), 100, 30)
                        .join("\n")
                        .contains(needle)
                })
            });
            if found {
                return;
            }
            assert!(Instant::now() < deadline, "{needle} never appeared");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_status(view: &Entity<TestHost>, cx: &mut VisualTestContext, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            view.update(cx, |host, cx| {
                if let Some(workspace) = host.documents.as_mut() {
                    workspace.poll();
                }
                cx.notify();
            });
            cx.run_until_parked();
            let found = cx.read(|app| {
                view.read(app)
                    .documents
                    .as_ref()
                    .is_some_and(|workspace| workspace.active().status().contains(needle))
            });
            if found {
                return;
            }
            assert!(Instant::now() < deadline, "{needle} never appeared");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_image(view: &Entity<TestHost>, cx: &mut VisualTestContext, path: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            view.update(cx, |host, cx| {
                if let Some(workspace) = host.documents.as_mut() {
                    workspace.poll();
                }
                cx.notify();
            });
            cx.run_until_parked();
            let done = cx.read(|app| {
                view.read(app).documents.as_ref().is_some_and(|workspace| {
                    matches!(
                        workspace.active().image(path),
                        Some(ImageState::Ready { .. } | ImageState::Failed(_))
                    )
                })
            });
            if done {
                return;
            }
            assert!(Instant::now() < deadline, "{path} did not load");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn host(
        socket: PathBuf,
        cx: &mut TestAppContext,
    ) -> (Entity<TestHost>, &mut VisualTestContext) {
        cx.add_window_view(|_, cx| TestHost {
            rich_selections: Default::default(),
            documents: Some(Workspace::new(socket)),
            focus: cx.focus_handle(),
            panel_width: None,
        })
    }

    #[test]
    fn inline_styles_map_to_weight_underline_and_background() {
        let bold = inline_highlight(markdown::InlineStyle {
            bold: true,
            ..Default::default()
        });
        assert_eq!(bold.font_weight, Some(FontWeight::BOLD));
        let italic = inline_highlight(markdown::InlineStyle {
            italic: true,
            ..Default::default()
        });
        assert_eq!(italic.font_style, Some(FontStyle::Italic));
        let code = inline_highlight(markdown::InlineStyle {
            code: true,
            ..Default::default()
        });
        assert!(code.background_color.is_some());
        let link = inline_highlight(markdown::InlineStyle {
            link: true,
            ..Default::default()
        });
        assert!(link.underline.is_some());
        assert_eq!(link.color, Some(rgb(0x82aaff).into()));
        let image = inline_highlight(markdown::InlineStyle {
            image: true,
            ..Default::default()
        });
        assert_eq!(image.font_style, Some(FontStyle::Italic));
    }

    #[test]
    fn image_fit_preserves_aspect_ratio_and_avoids_upscaling() {
        assert_eq!(
            fit_image(480, 300, 296.0, 680.0),
            DisplaySize {
                width: 296.0,
                height: 185.0
            }
        );
        assert_eq!(
            fit_image(200, 100, 296.0, 680.0),
            DisplaySize {
                width: 200.0,
                height: 100.0
            }
        );
        assert_eq!(
            fit_image(1000, 500, 900.0, 680.0),
            DisplaySize {
                width: 680.0,
                height: 340.0
            }
        );
        assert_eq!(
            fit_image(480, 300, 100.0, 680.0),
            DisplaySize {
                width: 100.0,
                height: 62.5
            }
        );
        assert_eq!(
            fit_image(0, 300, 296.0, 680.0),
            DisplaySize {
                width: 0.0,
                height: 0.0
            }
        );
        assert_eq!(pane_content_width(160.0, false), 128.0);
        assert_eq!(pane_content_width(160.0, true), 52.0);
        assert!(
            pane_content_width(160.0, true) <= 60.0,
            "a narrow split must not claim more width than the pane has"
        );
        assert_eq!(pane_content_width(900.0, true), 422.0);
    }

    #[test]
    fn media_read_accepts_nested_base64_near_the_advertised_limit() {
        let _guard = DAEMON_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let bin = remuda_bin();
        let daemon = Daemon::start(&bin);
        let payload = vec![0xabu8; 8 * 1024 * 1024];
        daemon.put(&bin, "big.bin", &payload);
        let read =
            remuda_native::documents::read_media(&daemon.socket(), "big.bin", 8 * 1024 * 1024)
                .unwrap();
        assert_eq!(read, payload);
        assert!(
            remuda_native::documents::read_media(&daemon.socket(), "big.bin", 4 * 1024 * 1024,)
                .is_err()
        );
    }

    #[test]
    fn worker_reserves_decoded_bytes_before_the_ui_polls() {
        let _guard = DAEMON_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let bin = remuda_bin();
        let daemon = Daemon::start(&bin);
        daemon.put(&bin, "a.png", &png_bytes(1000, 1000));
        daemon.put(&bin, "b.png", &png_bytes(1000, 1000));
        daemon.put(&bin, "c.png", &png_bytes(1000, 1000));
        let budget = std::sync::Arc::new(ImageBudget::with_limits(4 * 1024 * 1024, 3));
        let mut workspace = Workspace::with_budget(daemon.socket(), budget.clone());
        workspace.open_in_new_tab("a.png");
        workspace.open_in_new_tab("b.png");
        workspace.open_in_new_tab("c.png");
        // One poll queues the reads. Do not poll again: results wait in their
        // channels while the workers must already hold byte reservations.
        workspace.poll();
        let deadline = Instant::now() + Duration::from_secs(10);
        while budget.usage().0 == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let in_flight = budget.usage();
        assert_eq!(in_flight.1, 3, "{in_flight:?}");
        assert_eq!(in_flight.0, 4_000_000, "{in_flight:?}");
        drop(workspace);
        assert_eq!(budget.usage(), (0, 0));
    }

    #[test]
    fn refresh_and_close_release_in_flight_reservations() {
        let _guard = DAEMON_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let bin = remuda_bin();
        let daemon = Daemon::start(&bin);
        daemon.put(&bin, "a.png", &png_bytes(1000, 1000));
        let budget = std::sync::Arc::new(ImageBudget::with_limits(8 * 1024 * 1024, 3));
        let mut workspace = Workspace::with_budget(daemon.socket(), budget.clone());
        workspace.open_in_new_tab("a.png");
        workspace.poll();
        workspace.active_mut().refresh();
        workspace.close(workspace.active_index());
        let deadline = Instant::now() + Duration::from_secs(5);
        while budget.usage() != (0, 0) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(budget.usage(), (0, 0));
    }

    #[gpui::test]
    fn image_entries_stay_bounded_when_many_paths_fail(cx: &mut gpui::TestAppContext) {
        let _guard = DAEMON_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let bin = remuda_bin();
        let daemon = Daemon::start(&bin);
        let mut source = String::from("# Many\n\n");
        for index in 0..20 {
            source.push_str(&format!("![m{index}](missing-{index}.png)\n\n"));
        }
        daemon.put(&bin, "many.md", source.as_bytes());
        let (view, cx) = host(daemon.socket(), cx);
        view.update(cx, |host, cx| {
            host.documents.as_mut().unwrap().open_in_new_tab("many.md");
            cx.notify();
        });
        settle(&view, cx);
        wait_for_image(&view, cx, "missing-0.png");
        let (entries, limited, usage) = cx.read(|app| {
            let workspace = view.read(app).documents.as_ref().unwrap();
            (
                workspace.active().image_entry_count(),
                workspace.active().image_limited(),
                workspace.image_budget().usage(),
            )
        });
        assert!(entries <= MAX_IMAGES_PER_PANE, "{entries}");
        assert_eq!(entries, MAX_IMAGES_PER_PANE);
        assert!(limited, "paths past the entry cap must not be cached");
        assert!(usage.1 <= remuda_gui::documents::MAX_WORKSPACE_IMAGE_ENTRIES);
    }

    #[gpui::test]
    fn decoded_image_memory_is_shared_by_all_tabs(cx: &mut gpui::TestAppContext) {
        let _guard = DAEMON_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let bin = remuda_bin();
        let daemon = Daemon::start(&bin);
        daemon.put(&bin, "a.png", &png_bytes(1000, 1000));
        daemon.put(&bin, "b.png", &png_bytes(1000, 1000));
        daemon.put(&bin, "c.png", &png_bytes(1000, 1000));
        let budget = std::sync::Arc::new(ImageBudget::with_limits(4 * 1024 * 1024, 3));
        let socket = daemon.socket();
        let budget_for_view = budget.clone();
        let (view, cx) = cx.add_window_view(|_, cx| TestHost {
            rich_selections: Default::default(),
            documents: Some(Workspace::with_budget(socket, budget_for_view)),
            focus: cx.focus_handle(),
            panel_width: None,
        });
        view.update(cx, |host, cx| {
            let workspace = host.documents.as_mut().unwrap();
            workspace.open_in_new_tab("a.png");
            workspace.open_in_new_tab("b.png");
            workspace.open_in_new_tab("c.png");
            cx.notify();
        });
        settle(&view, cx);
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            view.update(cx, |host, cx| {
                if let Some(workspace) = host.documents.as_mut() {
                    workspace.poll();
                }
                cx.notify();
            });
            cx.run_until_parked();
            let settled = cx.read(|app| {
                let workspace = view.read(app).documents.as_ref().unwrap();
                workspace.tabs().iter().all(|pane| {
                    !pane.is_image()
                        || !matches!(
                            pane.image(pane.location()),
                            Some(ImageState::Loading) | None
                        )
                })
            });
            if settled {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "shared image budget did not settle"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let (usage, states) = cx.read(|app| {
            let workspace = view.read(app).documents.as_ref().unwrap();
            (
                workspace.image_budget().usage(),
                workspace
                    .tabs()
                    .iter()
                    .map(|pane| {
                        matches!(pane.image(pane.location()), Some(ImageState::Ready { .. }))
                    })
                    .collect::<Vec<_>>(),
            )
        });
        assert!(usage.0 <= 4 * 1024 * 1024, "{usage:?}");
        assert!(usage.1 <= 3, "{usage:?}");
        assert!(
            states.iter().filter(|ready| **ready).count() <= 1,
            "small shared budget admitted more than one 4 MiB image: {states:?}"
        );
        assert!(states.iter().any(|ready| !*ready), "{states:?}");

        // Closing a tab releases that pane's reservation.
        view.update(cx, |host, _| {
            let workspace = host.documents.as_mut().unwrap();
            while workspace.len() > 1 {
                workspace.close(workspace.active_index());
            }
        });
        cx.run_until_parked();
        let usage = cx.read(|app| {
            view.read(app)
                .documents
                .as_ref()
                .unwrap()
                .image_budget()
                .usage()
        });
        assert_eq!(usage, (0, 0));
    }

    #[gpui::test]
    fn navigating_to_a_second_large_image_releases_the_first_reservation(
        cx: &mut gpui::TestAppContext,
    ) {
        let _guard = DAEMON_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let bin = remuda_bin();
        let daemon = Daemon::start(&bin);
        daemon.put(&bin, "a.png", &png_bytes(1000, 1000));
        daemon.put(&bin, "b.png", &png_bytes(1000, 1000));
        let budget = std::sync::Arc::new(ImageBudget::with_limits(4 * 1024 * 1024, 4));
        let socket = daemon.socket();
        let budget_for_view = budget.clone();
        let (view, cx) = cx.add_window_view(|_, cx| TestHost {
            rich_selections: Default::default(),
            documents: Some(Workspace::with_budget(socket, budget_for_view)),
            focus: cx.focus_handle(),
            panel_width: None,
        });
        view.update(cx, |host, cx| {
            host.documents.as_mut().unwrap().open_in_active("a.png");
            cx.notify();
        });
        settle(&view, cx);
        wait_for_image(&view, cx, "a.png");
        let first_usage = budget.usage();
        assert!(first_usage.0 >= 4_000_000, "large image did not reserve memory: {first_usage:?}");

        view.update(cx, |host, cx| {
            host.documents.as_mut().unwrap().open_in_active("b.png");
            cx.notify();
        });
        assert_eq!(budget.usage(), (0, 0), "navigation must release the old image first");
        settle(&view, cx);
        wait_for_image(&view, cx, "b.png");
        let (usage, old, ready) = cx.read(|app| {
            let workspace = view.read(app).documents.as_ref().unwrap();
            (
                workspace.image_budget().usage(),
                workspace.active().image("a.png").is_some(),
                matches!(
                    workspace.active().image("b.png"),
                    Some(ImageState::Ready { .. })
                ),
            )
        });
        assert!(usage.0 <= 4 * 1024 * 1024, "{usage:?}");
        assert!(usage.1 <= 1, "{usage:?}");
        assert!(!old, "the old image stayed cached after navigation");
        assert!(ready, "the second large image did not load");
    }

    #[gpui::test]
    fn navigation_in_one_tab_keeps_the_other_tabs_image(cx: &mut gpui::TestAppContext) {
        let _guard = DAEMON_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let bin = remuda_bin();
        let daemon = Daemon::start(&bin);
        for path in ["a.png", "b.png", "c.png"] {
            daemon.put(&bin, path, &png_bytes(1000, 1000));
        }
        let budget = std::sync::Arc::new(ImageBudget::with_limits(8 * 1024 * 1024, 4));
        let socket = daemon.socket();
        let budget_for_view = budget.clone();
        let (view, cx) = cx.add_window_view(|_, cx| TestHost {
            rich_selections: Default::default(),
            documents: Some(Workspace::with_budget(socket, budget_for_view)),
            focus: cx.focus_handle(),
            panel_width: None,
        });
        view.update(cx, |host, cx| {
            host.documents.as_mut().unwrap().open_in_active("a.png");
            cx.notify();
        });
        settle(&view, cx);
        wait_for_image(&view, cx, "a.png");
        view.update(cx, |host, cx| {
            host.documents.as_mut().unwrap().open_in_new_tab("b.png");
            cx.notify();
        });
        settle(&view, cx);
        wait_for_image(&view, cx, "b.png");
        view.update(cx, |host, cx| {
            let workspace = host.documents.as_mut().unwrap();
            workspace.activate(0);
            workspace.open_in_active("c.png");
            cx.notify();
        });
        settle(&view, cx);
        wait_for_image(&view, cx, "c.png");
        let (usage, first_ready, second_ready, first_has_a) = cx.read(|app| {
            let workspace = view.read(app).documents.as_ref().unwrap();
            (
                workspace.image_budget().usage(),
                matches!(
                    workspace.tabs()[0].image("c.png"),
                    Some(ImageState::Ready { .. })
                ),
                matches!(
                    workspace.tabs()[1].image("b.png"),
                    Some(ImageState::Ready { .. })
                ),
                workspace.tabs()[0].image("a.png").is_some(),
            )
        });
        assert!(usage.0 <= 8 * 1024 * 1024, "{usage:?}");
        assert!(!first_has_a, "the active tab kept its old image");
        assert!(first_ready, "the active tab did not load its new image");
        assert!(second_ready, "navigation in the active tab released the other tab's image");
    }

    #[gpui::test]
    fn navigating_after_the_pane_image_cap_reopens_the_new_image(cx: &mut gpui::TestAppContext) {
        let _guard = DAEMON_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let bin = remuda_bin();
        let daemon = Daemon::start(&bin);
        let mut source = String::from("# Many\n\n");
        for index in 0..12 {
            source.push_str(&format!("![m{index}](missing-{index}.png)\n\n"));
        }
        daemon.put(&bin, "many.md", source.as_bytes());
        daemon.put(&bin, "later.png", &png_bytes(3, 2));
        let (view, cx) = host(daemon.socket(), cx);
        view.update(cx, |host, cx| {
            host.documents.as_mut().unwrap().open_in_active("many.md");
            cx.notify();
        });
        settle(&view, cx);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            view.update(cx, |host, cx| {
                if let Some(workspace) = host.documents.as_mut() {
                    workspace.poll();
                }
                cx.notify();
            });
            cx.run_until_parked();
            if cx.read(|app| {
                view.read(app).documents.as_ref().is_some_and(|workspace| {
                    let pane = workspace.active();
                    pane.image_entry_count() == MAX_IMAGES_PER_PANE && pane.image_limited()
                })
            }) {
                break;
            }
            assert!(Instant::now() < deadline, "pane image cap did not settle");
            std::thread::sleep(Duration::from_millis(20));
        }
        view.update(cx, |host, cx| {
            host.documents.as_mut().unwrap().open_in_active("later.png");
            cx.notify();
        });
        settle(&view, cx);
        wait_for_image(&view, cx, "later.png");
        let (limited, ready) = cx.read(|app| {
            let workspace = view.read(app).documents.as_ref().unwrap();
            (
                workspace.active().image_limited(),
                matches!(
                    workspace.active().image("later.png"),
                    Some(ImageState::Ready { .. })
                ),
            )
        });
        assert!(!limited, "navigation kept the old pane image limit");
        assert!(ready, "the new image was still blocked after navigation");
    }

    #[gpui::test]
    fn vfs_split_row_identity_and_escaped_labels(cx: &mut gpui::TestAppContext) {
        let _guard=DAEMON_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let bin=remuda_bin(); let daemon=Daemon::start(&bin);
        let raw="a\u{202e}b.txt"; let literal=r"a\u{202e}b.txt";
        daemon.put(&bin,raw,b"raw"); daemon.put(&bin,"ab.txt",b"plain");
        assert_ne!(remuda_native::documents::path_label(raw),remuda_native::documents::path_label("ab.txt"));
        assert_ne!(remuda_native::documents::path_label(raw),remuda_native::documents::path_label(literal));
        assert_eq!(remuda_native::documents::path_label(raw),literal);
        let (view,cx)=host(daemon.socket(),cx); cx.simulate_resize(gpui::size(px(1200.),px(600.))); settle(&view,cx);
        view.update(cx,|h,cx| { let w=h.documents.as_mut().unwrap(); w.open_tab(); w.split_with(0); cx.notify(); }); settle(&view,cx);
        let left=cx.debug_bounds("vfs-entry-left-0").unwrap(); let right=cx.debug_bounds("vfs-entry-right-0").unwrap(); assert!(left.right() <= right.left());
        let index=view.read_with(cx,|h,_|h.documents.as_ref().unwrap().active().directory_entries().unwrap().2.iter().position(|e|e.path == raw).unwrap());
        let id: &'static str=Box::leak(format!("vfs-entry-right-{index}").into_boxed_str());
        let row=cx.debug_bounds(id).unwrap(); cx.simulate_click(row.center(),gpui::Modifiers::none()); settle(&view,cx);
        assert_eq!(view.read_with(cx,|h,_|h.documents.as_ref().unwrap().active().location().to_owned()),raw);
        assert_eq!(view.read_with(cx,|h,_|h.documents.as_ref().unwrap().split_index()),Some(1));
    }

    #[gpui::test]
    fn parent_row_moves_up_one_level_and_stops_at_root(cx: &mut gpui::TestAppContext) {
        let _guard=DAEMON_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let bin=remuda_bin(); let daemon=Daemon::start(&bin);
        daemon.put(&bin,"docs/guide/note.md",b"note"); daemon.put(&bin,"docs/readme.md",b"readme");
        let (view,cx)=host(daemon.socket(),cx); cx.simulate_resize(gpui::size(px(900.),px(600.))); settle(&view,cx);
        // The root itself has no parent row.
        assert!(view.read_with(cx,|h,_| h.documents.as_ref().unwrap().active().directory_entries().unwrap().2.iter().all(|e| e.label.is_none())));
        let docs=view.read_with(cx,|h,_| h.documents.as_ref().unwrap().active().directory_entries().unwrap().2.iter().position(|e| e.path=="docs/").unwrap());
        let docs_id: &'static str=Box::leak(format!("vfs-entry-left-{docs}").into_boxed_str());
        let row=cx.debug_bounds(docs_id).unwrap(); cx.simulate_click(row.center(),gpui::Modifiers::none()); settle(&view,cx);
        let (label, first, position)=view.read_with(cx,|h,_| {
            let (_,_,entries,position)=h.documents.as_ref().unwrap().active().directory_entries().unwrap();
            (entries[0].label.clone(), entries[1].path.clone(), position)
        });
        assert_eq!(label.as_deref(), Some(".."));
        assert_eq!(first, "docs/guide/");
        assert_eq!(position, 1, "the first real entry stays selected");
        // Click `..` once: docs -> root.
        let parent=cx.debug_bounds("vfs-entry-left-0").unwrap(); cx.simulate_click(parent.center(),gpui::Modifiers::none()); settle(&view,cx);
        assert_eq!(view.read_with(cx,|h,_| h.documents.as_ref().unwrap().active().location().to_owned()), "");
        // Root -> docs -> guide, then keyboard Up + Enter goes to the parent.
        let row=cx.debug_bounds(docs_id).unwrap(); cx.simulate_click(row.center(),gpui::Modifiers::none()); settle(&view,cx);
        let guide=cx.debug_bounds("vfs-entry-left-1").unwrap(); cx.simulate_click(guide.center(),gpui::Modifiers::none()); settle(&view,cx);
        assert_eq!(view.read_with(cx,|h,_| h.documents.as_ref().unwrap().active().location().to_owned()), "docs/guide/");
        view.update(cx,|h,_| { h.documents.as_mut().unwrap().active_mut().move_selection(false); });
        cx.run_until_parked();
        view.update(cx,|h,cx| { h.documents.as_mut().unwrap().active_mut().open_selected(); cx.notify(); }); settle(&view,cx);
        assert_eq!(view.read_with(cx,|h,_| h.documents.as_ref().unwrap().active().location().to_owned()), "docs/");
        // A second `..` stops at the root instead of leaving it.
        let parent=cx.debug_bounds("vfs-entry-left-0").unwrap(); cx.simulate_click(parent.center(),gpui::Modifiers::none()); settle(&view,cx);
        assert_eq!(view.read_with(cx,|h,_| h.documents.as_ref().unwrap().active().location().to_owned()), "");
        assert!(view.read_with(cx,|h,_| h.documents.as_ref().unwrap().active().directory_entries().unwrap().2.iter().all(|e| e.label.is_none())));
    }

    #[test]
    fn vfs_rows_show_basenames_and_parent_keeps_full_identity() {
        use remuda_native::documents::Entry;
        let file = Entry { path: "docs/guide/note.md".into(), directory: false, label: None };
        assert_eq!(vfs_row_label(&file), "note.md");
        assert_eq!(file.path, "docs/guide/note.md");
        let folder = Entry { path: "docs/한글 폴더/".into(), directory: true, label: None };
        assert_eq!(vfs_row_label(&folder), "한글 폴더");
        let parent = Entry { path: "docs/".into(), directory: true, label: Some("..".into()) };
        assert_eq!(vfs_row_label(&parent), "..");
        let long = Entry { path: format!("docs/{}", "긴이름".repeat(20)), directory: false, label: None };
        assert!(vfs_row_label(&long).starts_with("긴이름"));
    }

    #[gpui::test]
    fn directory_header_and_duplicate_basenames_keep_full_identity(cx: &mut gpui::TestAppContext) {
        let _guard=DAEMON_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let bin=remuda_bin(); let daemon=Daemon::start(&bin);
        daemon.put(&bin,"a/note.md",b"a"); daemon.put(&bin,"b/note.md",b"b");
        daemon.put(&bin,"docs/한글 폴더/아주 긴 이름의 문서.md",b"note");
        let (view,cx)=host(daemon.socket(),cx); cx.simulate_resize(gpui::size(px(900.),px(600.))); settle(&view,cx);
        // The root header names the VFS root, and there is no parent row.
        assert!(cx.debug_bounds("vfs-location-left").is_some());
        // Enter `a/`, then open its note by mouse: the row label is `note.md`
        // while click validation and the target use `a/note.md`.
        let a=view.read_with(cx,|h,_| h.documents.as_ref().unwrap().active().directory_entries().unwrap().2.iter().position(|e| e.path=="a/").unwrap());
        let a_id: &'static str=Box::leak(format!("vfs-entry-left-{a}").into_boxed_str());
        let row=cx.debug_bounds(a_id).unwrap(); cx.simulate_click(row.center(),gpui::Modifiers::none()); settle(&view,cx);
        assert_eq!(view.read_with(cx,|h,_| h.documents.as_ref().unwrap().active().location().to_owned()), "a/");
        assert!(cx.debug_bounds("vfs-location-left").is_some());
        let note=cx.debug_bounds("vfs-entry-left-1").unwrap(); cx.simulate_click(note.center(),gpui::Modifiers::none()); settle(&view,cx);
        assert_eq!(view.read_with(cx,|h,_| h.documents.as_ref().unwrap().active().location().to_owned()), "a/note.md");
        // Back to `a/`, then Back to the root.
        for _ in 0..2 { view.update(cx,|h,cx| { h.documents.as_mut().unwrap().active_mut().back(); cx.notify(); }); settle(&view,cx); }
        assert_eq!(view.read_with(cx,|h,_| h.documents.as_ref().unwrap().active().location().to_owned()), "");
        // Enter `b/` and open the same basename with Enter; it must reach b/note.md.
        let b=view.read_with(cx,|h,_| h.documents.as_ref().unwrap().active().directory_entries().unwrap().2.iter().position(|e| e.path=="b/").unwrap());
        let b_id: &'static str=Box::leak(format!("vfs-entry-left-{b}").into_boxed_str());
        let row=cx.debug_bounds(b_id).unwrap(); cx.simulate_click(row.center(),gpui::Modifiers::none()); settle(&view,cx);
        view.update(cx,|h,cx| { h.documents.as_mut().unwrap().active_mut().open_selected(); cx.notify(); }); settle(&view,cx);
        assert_eq!(view.read_with(cx,|h,_| h.documents.as_ref().unwrap().active().location().to_owned()), "b/note.md");
        // A unicode folder keeps the header identity and a visible long row.
        for _ in 0..2 { view.update(cx,|h,cx| { h.documents.as_mut().unwrap().active_mut().back(); cx.notify(); }); settle(&view,cx); }
        let docs=view.read_with(cx,|h,_| h.documents.as_ref().unwrap().active().directory_entries().unwrap().2.iter().position(|e| e.path=="docs/").unwrap());
        let docs_id: &'static str=Box::leak(format!("vfs-entry-left-{docs}").into_boxed_str());
        let row=cx.debug_bounds(docs_id).unwrap(); cx.simulate_click(row.center(),gpui::Modifiers::none()); settle(&view,cx);
        let folder=view.read_with(cx,|h,_| h.documents.as_ref().unwrap().active().directory_entries().unwrap().2.iter().position(|e| e.path=="docs/한글 폴더/").unwrap());
        let folder_id: &'static str=Box::leak(format!("vfs-entry-left-{folder}").into_boxed_str());
        let row=cx.debug_bounds(folder_id).unwrap(); cx.simulate_click(row.center(),gpui::Modifiers::none()); settle(&view,cx);
        assert_eq!(view.read_with(cx,|h,_| h.documents.as_ref().unwrap().active().location().to_owned()), "docs/한글 폴더/");
        assert!(cx.debug_bounds("vfs-location-left").is_some());
        let long=cx.debug_bounds("vfs-entry-left-1").unwrap();
        assert!(long.size.width > px(0.) && long.right() <= px(900.), "long unicode row must stay in the pane: {long:?}");
    }

    #[gpui::test]
    fn vfs_long_listing_scroll_click_and_history(cx: &mut gpui::TestAppContext) {
        let _guard=DAEMON_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let bin=remuda_bin(); let daemon=Daemon::start(&bin);
        for i in 0..205 { daemon.put(&bin, &format!("item{i:03}.txt"), b"content"); }
        daemon.put(&bin,"zfolder/inside.md",b"inside");
        let (view,cx)=host(daemon.socket(),cx); cx.simulate_resize(gpui::size(px(900.),px(360.))); settle(&view,cx);
        let (generation, directory, index, path, directory_flag)=view.read_with(cx,|h,_| {
            let pane=h.documents.as_ref().unwrap().active(); let (g,d,entries,_)=pane.directory_entries().unwrap();
            assert!(entries.len() >= 206); let (i,e)=entries.iter().enumerate().find(|(_,e)|e.path.contains("item204")).unwrap();
            (g,d.to_owned(),i,e.path.clone(),e.directory)
        });
        assert!(!directory_flag);
        let frame=cx.debug_bounds("document-frame-left").unwrap();
        view.update(cx,|h,cx| { for _ in 0..30 { h.documents.as_mut().unwrap().active_mut().move_selection(true); } cx.notify(); }); cx.run_until_parked();
        let keyboard_row=cx.debug_bounds("vfs-entry-left-30").unwrap(); assert!(keyboard_row.top() < frame.bottom() && keyboard_row.bottom() > frame.top());
        cx.simulate_event(gpui::ScrollWheelEvent { position:frame.center(), delta:gpui::ScrollDelta::Pixels(gpui::point(px(0.),px(10000.))), ..Default::default() }); cx.run_until_parked();
        let entry_id: &'static str = Box::leak(format!("vfs-entry-left-{index}").into_boxed_str());
        let before=cx.debug_bounds(entry_id).unwrap(); assert!(before.top() > frame.bottom());
        cx.simulate_event(gpui::ScrollWheelEvent { position:frame.center(), delta:gpui::ScrollDelta::Pixels(gpui::point(px(0.),px(-10000.))), ..Default::default() }); cx.run_until_parked();
        let after=cx.debug_bounds(entry_id).unwrap(); assert!(after.top() < frame.bottom() && after.bottom() > frame.top(), "{after:?} {frame:?}");
        cx.simulate_click(after.center(),gpui::Modifiers::none()); settle(&view,cx);
        let location=view.read_with(cx,|h,_|h.documents.as_ref().unwrap().active().location().to_owned()); assert_eq!(location,path);
        view.update(cx,|h,_| { let p=h.documents.as_mut().unwrap().active_mut(); assert!(!p.open_directory_entry(generation,&directory,index,&path,false)); });
        view.update(cx,|h,cx| {h.documents.as_mut().unwrap().active_mut().back();cx.notify();}); settle(&view,cx);
        let folder_index=view.read_with(cx,|h,_| h.documents.as_ref().unwrap().active().directory_entries().unwrap().2.iter().position(|e| e.path.contains("zfolder")).unwrap());
        let folder_id: &'static str=Box::leak(format!("vfs-entry-left-{folder_index}").into_boxed_str());
        cx.simulate_event(gpui::ScrollWheelEvent { position:frame.center(), delta:gpui::ScrollDelta::Pixels(gpui::point(px(0.),px(10000.))), ..Default::default() }); cx.run_until_parked();
        let folder=cx.debug_bounds(folder_id).unwrap(); cx.simulate_click(folder.center(),gpui::Modifiers::none()); settle(&view,cx);
        assert!(view.read_with(cx,|h,_|h.documents.as_ref().unwrap().active().location().contains("zfolder")));
        let file=cx.debug_bounds("vfs-entry-left-1").unwrap(); cx.simulate_click(file.center(),gpui::Modifiers::none()); settle(&view,cx);
        assert!(view.read_with(cx,|h,_|h.documents.as_ref().unwrap().active().location().ends_with("inside.md")));
        view.update(cx,|h,cx| {h.documents.as_mut().unwrap().active_mut().back();cx.notify();}); settle(&view,cx);
        assert!(view.read_with(cx,|h,_|h.documents.as_ref().unwrap().active().location().contains("zfolder")));
    }

    #[gpui::test]
    fn rich_document_cross_block_drag_and_copy(cx: &mut gpui::TestAppContext) {
        let _guard = DAEMON_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let bin = remuda_bin(); let daemon = Daemon::start(&bin);
        daemon.put(&bin, "guide.md", "# Alpha 한글\n\nBeta  code\n".as_bytes());
        daemon.put(&bin, "other.md", b"Other\n");
        let (view,cx) = host(daemon.socket(),cx);
        cx.simulate_resize(gpui::size(px(900.),px(600.)));
        view.update(cx,|host,cx| { host.documents.as_mut().unwrap().open_in_new_tab("guide.md"); cx.notify(); });
        settle(&view,cx); cx.run_until_parked();
        let first = cx.debug_bounds("rich-piece-0").unwrap();
        let second = cx.debug_bounds("rich-piece-13").unwrap();
        let a=gpui::point(first.left()+px(0.1),first.top()+px(5.));
        let b=gpui::point(second.right()-px(0.1),second.top()+px(5.));
        cx.update(|_, app| app.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into())));
        cx.simulate_mouse_down(a,gpui::MouseButton::Left,gpui::Modifiers::none());
        cx.simulate_mouse_move(b,Some(gpui::MouseButton::Left),gpui::Modifiers::none());
        cx.simulate_mouse_up(b,gpui::MouseButton::Left,gpui::Modifiers::none());
        assert_eq!(cx.read(|app| app.read_from_clipboard().and_then(|x|x.text())).as_deref(), Some("sentinel"));
        cx.simulate_keystrokes("cmd-c");
        assert_eq!(cx.read(|app| app.read_from_clipboard().and_then(|x|x.text())).as_deref(), Some("Alpha 한글\nBeta  code"));
        view.update(cx,|host,cx| { host.documents.as_mut().unwrap().active_mut().open_document("other.md"); cx.notify(); }); settle(&view,cx);
        view.update(cx,|host,cx| { host.documents.as_mut().unwrap().active_mut().back(); cx.notify(); }); settle(&view,cx);
        cx.update(|_, app| app.write_to_clipboard(gpui::ClipboardItem::new_string("after history".into())));
        cx.simulate_keystrokes("cmd-c");
        assert_eq!(cx.read(|app| app.read_from_clipboard().and_then(|x|x.text())).as_deref(), Some("after history"));
        view.update(cx,|host,cx| { host.documents.as_mut().unwrap().active_mut().refresh(); cx.notify(); });
        settle(&view,cx);
        cx.update(|_, app| app.write_to_clipboard(gpui::ClipboardItem::new_string("after refresh".into())));
        cx.simulate_keystrokes("cmd-c");
        assert_eq!(cx.read(|app| app.read_from_clipboard().and_then(|x|x.text())).as_deref(), Some("after refresh"));
    }

    #[gpui::test]
    fn rich_drag_does_not_activate_link_but_click_does(cx: &mut gpui::TestAppContext) {
        let _guard=DAEMON_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let bin=remuda_bin(); let daemon=Daemon::start(&bin);
        daemon.put(&bin,"guide.md",b"Start [linked](other.md) end\n");
        daemon.put(&bin,"other.md",b"Other\n");
        let (view,cx)=host(daemon.socket(),cx);
        cx.simulate_resize(gpui::size(px(900.),px(600.)));
        view.update(cx,|host,cx| {host.documents.as_mut().unwrap().open_in_new_tab("guide.md");cx.notify();});
        settle(&view,cx); cx.run_until_parked();
        let link=cx.debug_bounds("md-link-left-0-1").unwrap();
        let start=gpui::point(link.left()+px(2.),link.top()+px(5.));
        let end=gpui::point(link.right()-px(2.),link.top()+px(5.));
        cx.simulate_mouse_down(start,gpui::MouseButton::Left,gpui::Modifiers::none());
        cx.simulate_mouse_move(end,Some(gpui::MouseButton::Left),gpui::Modifiers::none());
        cx.simulate_mouse_up(end,gpui::MouseButton::Left,gpui::Modifiers::none());
        assert_eq!(view.read_with(cx,|h,_|h.documents.as_ref().unwrap().active().location().to_owned()),"guide.md");
        let prefix=cx.debug_bounds("rich-piece-0").unwrap();
        let outside=gpui::point(prefix.left()+px(2.),prefix.top()+px(5.));
        cx.simulate_mouse_down(outside,gpui::MouseButton::Left,gpui::Modifiers::none());
        cx.simulate_mouse_move(start,Some(gpui::MouseButton::Left),gpui::Modifiers::none());
        cx.simulate_mouse_up(start,gpui::MouseButton::Left,gpui::Modifiers::none());
        assert_eq!(view.read_with(cx,|h,_|h.documents.as_ref().unwrap().active().location().to_owned()),"guide.md");
        cx.simulate_click(start,gpui::Modifiers::none()); settle(&view,cx);
        assert_eq!(view.read_with(cx,|h,_|h.documents.as_ref().unwrap().active().location().to_owned()),"other.md");
    }

    #[gpui::test]
    fn rich_scroll_during_drag_uses_only_visible_target(cx: &mut gpui::TestAppContext) {
        let _guard=DAEMON_TEST_LOCK.lock().unwrap_or_else(|e|e.into_inner()); let bin=remuda_bin(); let daemon=Daemon::start(&bin);
        let source=(0..120).map(|i|format!("line{i:03}\n\n")).collect::<String>(); daemon.put(&bin,"long.md",source.as_bytes());
        let (view,cx)=host(daemon.socket(),cx); cx.simulate_resize(gpui::size(px(900.),px(360.)));
        view.update(cx,|host,cx| {host.documents.as_mut().unwrap().open_in_new_tab("long.md");cx.notify();}); settle(&view,cx); cx.run_until_parked();
        let first=cx.debug_bounds("rich-piece-0").unwrap(); let frame=cx.debug_bounds("document-frame-left").unwrap();
        let start=gpui::point(first.left()+px(1.),first.top()+px(5.));
        let edge=gpui::point(first.left()+px(4.),frame.bottom()-px(5.));
        cx.simulate_mouse_down(start,gpui::MouseButton::Left,gpui::Modifiers::none());
        cx.simulate_event(gpui::ScrollWheelEvent { position: edge, delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.),px(-160.))), ..Default::default() });
        cx.run_until_parked();
        assert!(cx.debug_bounds("rich-piece-0").unwrap().bottom() < frame.top());
        cx.simulate_mouse_move(edge,Some(gpui::MouseButton::Left),gpui::Modifiers::none());
        cx.simulate_mouse_up(edge,gpui::MouseButton::Left,gpui::Modifiers::none());
        cx.simulate_keystrokes("cmd-c");
        let copied=cx.read(|app|app.read_from_clipboard().and_then(|x|x.text())).unwrap();
        assert!(copied.starts_with("line000\n"),"{copied:?}");
        assert!(copied.contains("line006"),"{copied:?}");
    }

    #[gpui::test]
    fn rich_split_drag_does_not_select_other_pane(cx: &mut gpui::TestAppContext) {
        let _guard=DAEMON_TEST_LOCK.lock().unwrap_or_else(|e|e.into_inner()); let bin=remuda_bin(); let daemon=Daemon::start(&bin);
        daemon.put(&bin,"one.md",b"One text\n"); daemon.put(&bin,"two.md",b"Two text\n");
        let (view,cx)=host(daemon.socket(),cx); cx.simulate_resize(gpui::size(px(1200.),px(600.)));
        view.update(cx,|host,cx| { let w=host.documents.as_mut().unwrap(); w.open_in_new_tab("one.md"); w.open_in_new_tab("two.md"); w.split_with(1); cx.notify(); }); settle(&view,cx); cx.run_until_parked();
        let l=cx.debug_bounds("md-p-left-0").unwrap(); let r=cx.debug_bounds("md-p-right-0").unwrap();
        let a=gpui::point(l.left()+px(1.),l.top()+px(5.)); let b=gpui::point(r.left()+px(30.),r.top()+px(5.));
        cx.update(|_,app|app.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into())));
        cx.simulate_mouse_down(a,gpui::MouseButton::Left,gpui::Modifiers::none()); cx.simulate_mouse_move(b,Some(gpui::MouseButton::Left),gpui::Modifiers::none()); cx.simulate_mouse_up(b,gpui::MouseButton::Left,gpui::Modifiers::none()); cx.simulate_keystrokes("cmd-c");
        assert_eq!(cx.read(|app|app.read_from_clipboard().and_then(|x|x.text())).as_deref(),Some("sentinel"));
    }

    #[gpui::test]
    fn rich_drag_crosses_styled_link_and_preserves_code_spacing(cx: &mut gpui::TestAppContext) {
        let _guard=DAEMON_TEST_LOCK.lock().unwrap_or_else(|e|e.into_inner()); let bin=remuda_bin(); let daemon=Daemon::start(&bin);
        daemon.put(&bin,"guide.md", "# Title\n\nStart **bold** [linked](other.md) end\n\n```text\n    x  한\n```\n".as_bytes());
        let (view,cx)=host(daemon.socket(),cx); cx.simulate_resize(gpui::size(px(900.),px(600.)));
        view.update(cx,|host,cx| {host.documents.as_mut().unwrap().open_in_new_tab("guide.md");cx.notify();}); settle(&view,cx); cx.run_until_parked();
        let first=cx.debug_bounds("rich-piece-0").unwrap(); let code=cx.debug_bounds("rich-piece-28").unwrap();
        let a=gpui::point(first.left()+px(0.1),first.top()+px(5.)); let b=gpui::point(code.right()-px(0.1),code.top()+px(5.));
        cx.simulate_mouse_down(a,gpui::MouseButton::Left,gpui::Modifiers::none()); cx.simulate_mouse_move(b,Some(gpui::MouseButton::Left),gpui::Modifiers::none()); cx.simulate_mouse_up(b,gpui::MouseButton::Left,gpui::Modifiers::none()); cx.simulate_keystrokes("cmd-c");
        assert_eq!(cx.read(|app|app.read_from_clipboard().and_then(|x|x.text())).as_deref(),Some("Title\nStart bold linked end\n    x  한"));
    }

    #[gpui::test]
    fn rich_drag_across_image_includes_alt_text(cx: &mut gpui::TestAppContext) {
        let _guard=DAEMON_TEST_LOCK.lock().unwrap_or_else(|e|e.into_inner()); let bin=remuda_bin(); let daemon=Daemon::start(&bin);
        daemon.put(&bin,"guide.md",b"Before ![diagram](img.png) after\n"); daemon.put(&bin,"img.png",&png_bytes(8,8));
        let (view,cx)=host(daemon.socket(),cx); cx.simulate_resize(gpui::size(px(900.),px(600.)));
        view.update(cx,|host,cx| {host.documents.as_mut().unwrap().open_in_new_tab("guide.md");cx.notify();}); settle(&view,cx); cx.run_until_parked();
        let first=cx.debug_bounds("rich-piece-0").unwrap(); let last=cx.debug_bounds("rich-piece-23").unwrap();
        let a=gpui::point(first.left()+px(0.1),first.top()+px(5.)); let b=gpui::point(last.right()-px(0.1),last.top()+px(5.));
        cx.simulate_mouse_down(a,gpui::MouseButton::Left,gpui::Modifiers::none()); cx.simulate_mouse_move(b,Some(gpui::MouseButton::Left),gpui::Modifiers::none()); cx.simulate_mouse_up(b,gpui::MouseButton::Left,gpui::Modifiers::none()); cx.simulate_keystrokes("cmd-c");
        assert_eq!(cx.read(|app|app.read_from_clipboard().and_then(|x|x.text())).as_deref(),Some("Before [image: diagram] after"));
    }

    #[gpui::test]
    fn rich_large_document_drag_timing(cx: &mut gpui::TestAppContext) {
        let _guard=DAEMON_TEST_LOCK.lock().unwrap_or_else(|e|e.into_inner()); let bin=remuda_bin(); let daemon=Daemon::start(&bin);
        let source=(0..200).map(|i|format!("{i:04} {}\n\n", "x".repeat(320))).collect::<String>(); daemon.put(&bin,"large.md",source.as_bytes());
        let (view,cx)=host(daemon.socket(),cx); cx.simulate_resize(gpui::size(px(900.),px(600.)));
        view.update(cx,|host,cx| {host.documents.as_mut().unwrap().open_in_new_tab("large.md");cx.notify();}); settle(&view,cx); cx.run_until_parked();
        let first=cx.debug_bounds("rich-piece-0").unwrap(); let a=gpui::point(first.left()+px(1.),first.top()+px(5.));
        cx.simulate_mouse_down(a,gpui::MouseButton::Left,gpui::Modifiers::none());
        let start=std::time::Instant::now();
        for i in 0..10 { let b=gpui::point(a.x+px((i+1) as f32 * 2.),a.y); cx.simulate_mouse_move(b,Some(gpui::MouseButton::Left),gpui::Modifiers::none()); }
        eprintln!("10 drag moves, {} source bytes: {:?}",source.len(),start.elapsed());
        cx.simulate_mouse_up(a,gpui::MouseButton::Left,gpui::Modifiers::none());
    }

    #[gpui::test]
    fn rich_limits_show_explicit_source_fallback(cx: &mut gpui::TestAppContext) {
        let _guard=DAEMON_TEST_LOCK.lock().unwrap_or_else(|e|e.into_inner()); let bin=remuda_bin(); let daemon=Daemon::start(&bin);
        daemon.put(&bin,"big.md",&vec![b'x'; 65537]);
        let many=(0..201).map(|i|format!("line{i}\n\n")).collect::<String>(); daemon.put(&bin,"many.md",many.as_bytes());

        daemon.put(&bin,"plain.txt",b"plain\x1btext");
        let (view,cx)=host(daemon.socket(),cx); cx.simulate_resize(gpui::size(px(900.),px(600.)));
        for path in ["big.md","many.md","plain.txt"] {
            view.update(cx,|host,cx| {host.documents.as_mut().unwrap().open_in_new_tab(path);cx.notify();}); settle(&view,cx);cx.run_until_parked();
            assert!(cx.debug_bounds("rich-selection-limit").is_some(),"missing fallback for {path}");
            assert!(cx.debug_bounds("rich-piece-0").is_none(),"selection should be disabled for {path}");
        }
    }

    #[gpui::test]
    fn rich_program_source_copy_keeps_indentation_unicode_and_trailing_spaces(cx: &mut gpui::TestAppContext) {
        let _guard=DAEMON_TEST_LOCK.lock().unwrap_or_else(|e|e.into_inner()); let bin=remuda_bin(); let daemon=Daemon::start(&bin);
        let source="fn main() {\n    let n = \"한\";  \n}"; daemon.put(&bin,"main.rs",source.as_bytes());
        let (view,cx)=host(daemon.socket(),cx); cx.simulate_resize(gpui::size(px(420.),px(600.)));
        view.update(cx,|host,cx| {host.documents.as_mut().unwrap().open_in_new_tab("main.rs");cx.notify();}); settle(&view,cx);cx.run_until_parked();
        let first=cx.debug_bounds("rich-piece-0").unwrap(); let last=cx.debug_bounds("rich-piece-33").unwrap();
        let a=gpui::point(first.left()+px(0.1),first.top()+px(5.)); let b=gpui::point(last.right()-px(0.1),last.top()+px(5.));
        cx.simulate_mouse_down(a,gpui::MouseButton::Left,gpui::Modifiers::none()); cx.simulate_mouse_move(b,Some(gpui::MouseButton::Left),gpui::Modifiers::none()); cx.simulate_mouse_up(b,gpui::MouseButton::Left,gpui::Modifiers::none()); cx.simulate_keystrokes("cmd-c");
        assert_eq!(cx.read(|app|app.read_from_clipboard().and_then(|x|x.text())).as_deref(),Some(source));
    }

    #[gpui::test]
    fn rich_code_single_long_line_drag_timing(cx: &mut gpui::TestAppContext) {
        let _guard=DAEMON_TEST_LOCK.lock().unwrap_or_else(|e|e.into_inner()); let bin=remuda_bin(); let daemon=Daemon::start(&bin);
        let source="x".repeat(65400); daemon.put(&bin,"long.rs",source.as_bytes());
        let (view,cx)=host(daemon.socket(),cx); cx.simulate_resize(gpui::size(px(900.),px(600.)));
        view.update(cx,|host,cx| {host.documents.as_mut().unwrap().open_in_new_tab("long.rs");cx.notify();}); settle(&view,cx);cx.run_until_parked();
        let first=cx.debug_bounds("rich-piece-0").unwrap(); let a=gpui::point(first.left()+px(1.),first.top()+px(5.));
        cx.simulate_mouse_down(a,gpui::MouseButton::Left,gpui::Modifiers::none()); let start=std::time::Instant::now();
        for i in 0..10 { let b=gpui::point(a.x+px((i+1) as f32*2.),a.y); cx.simulate_mouse_move(b,Some(gpui::MouseButton::Left),gpui::Modifiers::none()); }
        eprintln!("10 code drag moves, {} source bytes: {:?}",source.len(),start.elapsed()); cx.simulate_mouse_up(a,gpui::MouseButton::Left,gpui::Modifiers::none());
    }

    #[gpui::test]
    fn rich_program_copy_preserves_tabs_and_crlf(cx: &mut gpui::TestAppContext) {
        let _guard=DAEMON_TEST_LOCK.lock().unwrap_or_else(|e|e.into_inner()); let bin=remuda_bin(); let daemon=Daemon::start(&bin);
        let source="let a=1;\r\n\tprintln!(\"한\");  \r\n}"; daemon.put(&bin,"crlf.rs",source.as_bytes());
        let (view,cx)=host(daemon.socket(),cx); cx.simulate_resize(gpui::size(px(420.),px(600.)));
        view.update(cx,|host,cx| {host.documents.as_mut().unwrap().open_in_new_tab("crlf.rs");cx.notify();}); settle(&view,cx);cx.run_until_parked();
        let first=cx.debug_bounds("rich-piece-0").unwrap(); let last=cx.debug_bounds("rich-piece-31").unwrap();
        let a=gpui::point(first.left()+px(0.1),first.top()+px(5.)); let b=gpui::point(last.right()-px(0.1),last.top()+px(5.));
        cx.simulate_mouse_down(a,gpui::MouseButton::Left,gpui::Modifiers::none()); cx.simulate_mouse_move(b,Some(gpui::MouseButton::Left),gpui::Modifiers::none()); cx.simulate_mouse_up(b,gpui::MouseButton::Left,gpui::Modifiers::none()); cx.simulate_keystrokes("cmd-c");
        assert_eq!(cx.read(|app|app.read_from_clipboard().and_then(|x|x.text())).as_deref(),Some(source));
    }

    #[gpui::test]
    fn rich_program_can_select_final_newline(cx: &mut gpui::TestAppContext) {
        let _guard=DAEMON_TEST_LOCK.lock().unwrap_or_else(|e|e.into_inner()); let bin=remuda_bin(); let daemon=Daemon::start(&bin);
        let source="one\n"; daemon.put(&bin,"end.rs",source.as_bytes());
        let (view,cx)=host(daemon.socket(),cx); cx.simulate_resize(gpui::size(px(420.),px(600.)));
        view.update(cx,|host,cx| {host.documents.as_mut().unwrap().open_in_new_tab("end.rs");cx.notify();}); settle(&view,cx);cx.run_until_parked();
        let first=cx.debug_bounds("rich-piece-0").unwrap(); let last=cx.debug_bounds("rich-piece-4").unwrap();
        let a=gpui::point(first.left()+px(0.1),first.top()+px(5.)); let b=gpui::point(last.left()+px(1.),last.top()+px(2.));
        cx.simulate_mouse_down(a,gpui::MouseButton::Left,gpui::Modifiers::none()); cx.simulate_mouse_move(b,Some(gpui::MouseButton::Left),gpui::Modifiers::none()); cx.simulate_mouse_up(b,gpui::MouseButton::Left,gpui::Modifiers::none()); cx.simulate_keystrokes("cmd-c");
        assert_eq!(cx.read(|app|app.read_from_clipboard().and_then(|x|x.text())).as_deref(),Some(source));
        let source="one\r\n"; daemon.put(&bin,"end-crlf.rs",source.as_bytes());
        view.update(cx,|host,cx| {host.documents.as_mut().unwrap().open_in_new_tab("end-crlf.rs");cx.notify();}); settle(&view,cx);cx.run_until_parked();
        let first=cx.debug_bounds("rich-piece-0").unwrap(); let last=cx.debug_bounds("rich-piece-5").unwrap();
        let a=gpui::point(first.left()+px(0.1),first.top()+px(5.)); let b=gpui::point(last.left()+px(1.),last.top()+px(2.));
        cx.simulate_mouse_down(a,gpui::MouseButton::Left,gpui::Modifiers::none()); cx.simulate_mouse_move(b,Some(gpui::MouseButton::Left),gpui::Modifiers::none()); cx.simulate_mouse_up(b,gpui::MouseButton::Left,gpui::Modifiers::none()); cx.simulate_keystrokes("cmd-c");
        assert_eq!(cx.read(|app|app.read_from_clipboard().and_then(|x|x.text())).as_deref(),Some(source));
    }

    #[gpui::test]
    fn rich_exact_byte_and_block_limits_are_selectable(cx: &mut gpui::TestAppContext) {
        let _guard=DAEMON_TEST_LOCK.lock().unwrap_or_else(|e|e.into_inner()); let bin=remuda_bin(); let daemon=Daemon::start(&bin);
        daemon.put(&bin,"exact.md",&vec![b'x'; 65536]);
        let exact=(0..200).map(|i|format!("line{i}\n\n")).collect::<String>(); daemon.put(&bin,"blocks.md",exact.as_bytes());
        let (view,cx)=host(daemon.socket(),cx); cx.simulate_resize(gpui::size(px(900.),px(600.)));
        for path in ["exact.md","blocks.md"] {
            view.update(cx,|host,cx| {host.documents.as_mut().unwrap().open_in_new_tab(path);cx.notify();}); settle(&view,cx);cx.run_until_parked();
            assert!(view.read_with(cx,|host,_| {let id=host.documents.as_ref().unwrap().active().selection_id(); host.rich_selections.borrow().contains_key(&id)}),"selection absent at limit for {path}");
            assert!(cx.debug_bounds("rich-piece-0").is_some(),"render missing at limit for {path}");
        }
    }

    #[gpui::test]
    fn rich_fence_copy_preserves_tab_indentation(cx: &mut gpui::TestAppContext) {
        let _guard=DAEMON_TEST_LOCK.lock().unwrap_or_else(|e|e.into_inner()); let bin=remuda_bin(); let daemon=Daemon::start(&bin);
        let source="```rs\n\tprintln!(\"한\");\nnext\n```\n"; daemon.put(&bin,"fence.md",source.as_bytes());
        let (view,cx)=host(daemon.socket(),cx); cx.simulate_resize(gpui::size(px(900.),px(600.)));
        view.update(cx,|host,cx| {host.documents.as_mut().unwrap().open_in_new_tab("fence.md");cx.notify();}); settle(&view,cx);cx.run_until_parked();
        let first=cx.debug_bounds("rich-piece-0").unwrap();
        let offset="\tprintln!(\"한\");\n".len();
        assert_eq!(offset, 18);
        let last=cx.debug_bounds("rich-piece-18").unwrap();
        let a=gpui::point(first.left()+px(0.1),first.top()+px(5.)); let b=gpui::point(last.right()-px(0.1),last.top()+px(5.));
        cx.simulate_mouse_down(a,gpui::MouseButton::Left,gpui::Modifiers::none()); cx.simulate_mouse_move(b,Some(gpui::MouseButton::Left),gpui::Modifiers::none()); cx.simulate_mouse_up(b,gpui::MouseButton::Left,gpui::Modifiers::none()); cx.simulate_keystrokes("cmd-c");
        assert_eq!(cx.read(|app|app.read_from_clipboard().and_then(|x|x.text())).as_deref(),Some("\tprintln!(\"한\");\nnext"));
    }

    #[gpui::test]
    fn markdown_renders_heading_paragraph_list_code_and_link_with_hierarchy(
        cx: &mut gpui::TestAppContext,
    ) {
        let _guard = DAEMON_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let bin = remuda_bin();
        let daemon = Daemon::start(&bin);
        let source = "# Title\n\nIntro **bold** with `code` and [link](other.md).\n\n## Steps\n\n- first\n- second\n\n```python\ndef greet(name):\n    return \"hi \" + name\n```\n";
        daemon.put(&bin, "guide.md", source.as_bytes());
        let (view, cx) = host(daemon.socket(), cx);
        view.update(cx, |host, cx| {
            host.documents.as_mut().unwrap().open_in_new_tab("guide.md");
            cx.notify();
        });
        settle(&view, cx);
        for id in [
            "document-workspace",
            "document-tabs",
            "document-pane-left",
            "md-h1-left-0",
            "md-p-left-1",
            "md-h2-left-2",
            "md-list-left-3",
            "md-code-left-5",
        ] {
            assert!(cx.debug_bounds(id).is_some(), "missing {id}");
        }
        let heading = cx.debug_bounds("md-h1-left-0").unwrap();
        let paragraph = cx.debug_bounds("md-p-left-1").unwrap();
        assert!(
            heading.size.height > paragraph.size.height,
            "heading {} vs paragraph {}",
            heading.size.height,
            paragraph.size.height
        );
        let blocks = cx.read(|app| {
            let host = view.read(app);
            let pane = host.documents.as_ref().unwrap().pane(1).unwrap();
            markdown::parse(pane.markdown_source().unwrap())
        });
        assert_eq!(blocks[0].kind, markdown::BlockKind::Heading(1));
        assert_eq!(blocks[0].text(), "Title");
        assert_eq!(blocks[1].kind, markdown::BlockKind::Paragraph);
        let paragraph_block = &blocks[1];
        assert!(paragraph_block
            .spans
            .iter()
            .any(|span| span.text == "bold" && span.style.bold));
        assert!(paragraph_block
            .spans
            .iter()
            .any(|span| span.text == "code" && span.style.code));
        assert!(paragraph_block
            .spans
            .iter()
            .any(|span| span.text == "link" && span.style.link));
        assert_eq!(
            blocks[5].kind,
            markdown::BlockKind::Code {
                language: Some("python".into())
            }
        );
        assert_eq!(blocks[5].code[1], "    return \"hi \" + name");
    }

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([40, 120, 200, 255]));
        let mut bytes = Vec::new();
        image
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    #[gpui::test]
    fn images_open_directly_and_render_from_markdown_with_fallbacks(cx: &mut gpui::TestAppContext) {
        let _guard = DAEMON_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let bin = remuda_bin();
        let daemon = Daemon::start(&bin);
        daemon.put(&bin, "art.png", &png_bytes(3, 2));
        daemon.put(
            &bin,
            "pics.md",
            b"# Pics\n\n![art](art.png)\n\n![bad](missing.png)\n\n![ext](https://example.com/x.png)\n",
        );
        let (view, cx) = host(daemon.socket(), cx);
        view.update(cx, |host, cx| {
            host.documents.as_mut().unwrap().open_in_new_tab("art.png");
            cx.notify();
        });
        settle(&view, cx);
        let (status, selector) = cx.read(|app| {
            let workspace = view.read(app).documents.as_ref().unwrap();
            (
                workspace.active().status(),
                format!("{}", workspace.active().image("art.png").is_some()),
            )
        });
        assert!(status.contains("IMAGE · 3×2"), "{status}");
        assert_eq!(selector, "true");
        assert!(cx.debug_bounds("document-image-left").is_some());

        // Refresh must drop the old decoded pixels; only the new file shows.
        daemon.put(&bin, "art.png", &png_bytes(4, 5));
        view.update(cx, |host, cx| {
            host.documents.as_mut().unwrap().active_mut().refresh();
            cx.notify();
        });
        wait_for_status(&view, cx, "IMAGE · 4×5");

        view.update(cx, |host, cx| {
            let workspace = host.documents.as_mut().unwrap();
            workspace.open_in_new_tab("pics.md");
            cx.notify();
        });
        settle(&view, cx);
        wait_for_image(&view, cx, "art.png");
        wait_for_selector(cx, "md-image-left-1-0");
        wait_for_selector(cx, "md-image-blocked-left-3-0");
        wait_for_image(&view, cx, "missing.png");
        wait_for_selector(cx, "md-image-failed-left-2-0");
    }

    #[gpui::test]
    fn images_fit_the_pane_width_in_standalone_and_markdown_paths(
        cx: &mut gpui::TestAppContext,
    ) {
        let _guard = DAEMON_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let bin = remuda_bin();
        let daemon = Daemon::start(&bin);
        daemon.put(&bin, "wide.png", &png_bytes(480, 300));
        daemon.put(&bin, "small.png", &png_bytes(3, 2));
        daemon.put(&bin, "wide.md", b"![wide](wide.png)\n");
        daemon.put(
            &bin,
            "blocks.md",
            b"- outer\n  - inner ![wide](wide.png)\n\n> quote ![wide](wide.png)\n",
        );
        let (view, cx) = host(daemon.socket(), cx);
        cx.simulate_resize(gpui::size(px(520.), px(600.)));

        view.update(cx, |host, cx| {
            host.documents.as_mut().unwrap().open_in_active("wide.png");
            cx.notify();
        });
        settle(&view, cx);
        wait_for_image(&view, cx, "wide.png");
        let image = cx.debug_bounds("document-image-left").unwrap();
        let frame = cx.debug_bounds("document-frame-left").unwrap();
        assert!(
            f32::from(image.size.width) <= f32::from(frame.size.width) + 0.5,
            "standalone image {image:?} overflows frame {frame:?}"
        );
        let ratio = f32::from(image.size.width) / f32::from(image.size.height);
        assert!(
            (ratio - 480.0 / 300.0).abs() < 0.01,
            "standalone image lost its aspect ratio: {image:?}"
        );
        assert!(
            f32::from(image.size.width) < 480.0,
            "standalone image was not fitted: {image:?}"
        );

        view.update(cx, |host, cx| {
            host.documents.as_mut().unwrap().open_in_active("small.png");
            cx.notify();
        });
        settle(&view, cx);
        wait_for_image(&view, cx, "small.png");
        let small = cx.debug_bounds("document-image-left").unwrap();
        assert!(
            f32::from(small.size.width) <= 3.5,
            "small image was upscaled: {small:?}"
        );

        view.update(cx, |host, cx| {
            host.documents.as_mut().unwrap().open_in_active("wide.md");
            cx.notify();
        });
        settle(&view, cx);
        wait_for_image(&view, cx, "wide.png");
        wait_for_selector(cx, "md-image-left-0-0");
        let inline = cx.debug_bounds("md-image-left-0-0").unwrap();
        let frame = cx.debug_bounds("document-frame-left").unwrap();
        assert!(
            f32::from(inline.size.width) <= f32::from(frame.size.width) + 0.5,
            "markdown image {inline:?} overflows frame {frame:?}"
        );
        let ratio = f32::from(inline.size.width) / f32::from(inline.size.height);
        assert!(
            (ratio - 480.0 / 300.0).abs() < 0.01,
            "markdown image lost its aspect ratio: {inline:?}"
        );
        assert!(
            f32::from(inline.size.width) <= 240.5,
            "markdown image ignored its display cap: {inline:?}"
        );

        view.update(cx, |host, cx| {
            host.documents.as_mut().unwrap().open_in_active("blocks.md");
            cx.notify();
        });
        settle(&view, cx);
        wait_for_image(&view, cx, "wide.png");
        wait_for_selector(cx, "md-image-left-1-1");
        wait_for_selector(cx, "md-image-left-2-1");
        let nested = cx.debug_bounds("md-image-left-1-1").unwrap();
        let quote = cx.debug_bounds("md-image-left-2-1").unwrap();
        assert!(
            (f32::from(nested.size.width) - 192.0).abs() < 1.5,
            "nested list image ignored indent/marker: {nested:?}"
        );
        assert!(
            (f32::from(quote.size.width) - 220.0).abs() < 1.5,
            "quote image ignored quote padding: {quote:?}"
        );
    }

    #[gpui::test]
    fn split_images_fit_in_a_260px_panel(cx: &mut gpui::TestAppContext) {
        let _guard = DAEMON_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let bin = remuda_bin();
        let daemon = Daemon::start(&bin);
        daemon.put(&bin, "a.png", &png_bytes(480, 300));
        daemon.put(&bin, "b.png", &png_bytes(480, 300));
        let (view, cx) = host(daemon.socket(), cx);
        cx.simulate_resize(gpui::size(px(520.), px(600.)));
        view.update(cx, |host, cx| {
            let workspace = host.documents.as_mut().unwrap();
            workspace.open_in_active("a.png");
            workspace.open_in_new_tab("b.png");
            workspace.split_with(0);
            cx.notify();
        });
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            view.update(cx, |host, cx| {
                if let Some(workspace) = host.documents.as_mut() {
                    workspace.poll();
                }
                cx.notify();
            });
            cx.run_until_parked();
            let ready = cx.read(|app| {
                let workspace = view.read(app).documents.as_ref().unwrap();
                workspace.tabs().len() >= 2
                    && matches!(
                        workspace.tabs()[0].image("a.png"),
                        Some(ImageState::Ready { .. })
                    )
                    && matches!(
                        workspace.tabs()[1].image("b.png"),
                        Some(ImageState::Ready { .. })
                    )
            });
            if ready {
                break;
            }
            assert!(Instant::now() < deadline, "split images did not settle");
            std::thread::sleep(Duration::from_millis(20));
        }
        for (image_id, frame_id) in [
            ("document-image-left", "document-frame-left"),
            ("document-image-right", "document-frame-right"),
        ] {
            let image = cx.debug_bounds(image_id).unwrap();
            let frame = cx.debug_bounds(frame_id).unwrap();
            assert!(
                f32::from(image.size.width) <= f32::from(frame.size.width) + 0.5,
                "{image_id} {image:?} overflows {frame_id} {frame:?}"
            );
            let ratio = f32::from(image.size.width) / f32::from(image.size.height);
            assert!(
                (ratio - 480.0 / 300.0).abs() < 0.01,
                "{image_id} lost its aspect ratio: {image:?}"
            );
            assert!(
                (f32::from(image.size.width) - 102.0).abs() < 1.5,
                "{image_id} ignored the narrow split width: {image:?}"
            );
        }
    }

    #[gpui::test]
    fn split_images_fit_and_scroll_at_the_160px_product_panel(
        cx: &mut gpui::TestAppContext,
    ) {
        let _guard = DAEMON_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let bin = remuda_bin();
        let daemon = Daemon::start(&bin);
        daemon.put(&bin, "a.png", &png_bytes(480, 300));
        daemon.put(&bin, "tall.png", &png_bytes(100, 4000));
        let (view, cx) = cx.add_window_view(|_, cx| TestHost {
            rich_selections: Default::default(),
            documents: Some(Workspace::new(daemon.socket())),
            focus: cx.focus_handle(),
            panel_width: Some(160.0),
        });
        cx.simulate_resize(gpui::size(px(400.), px(600.)));
        view.update(cx, |host, cx| {
            let workspace = host.documents.as_mut().unwrap();
            workspace.open_in_active("a.png");
            workspace.open_in_new_tab("tall.png");
            workspace.split_with(0);
            cx.notify();
        });
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            view.update(cx, |host, cx| {
                if let Some(workspace) = host.documents.as_mut() {
                    workspace.poll();
                }
                cx.notify();
            });
            cx.run_until_parked();
            let ready = cx.read(|app| {
                let workspace = view.read(app).documents.as_ref().unwrap();
                workspace.tabs().len() >= 2
                    && matches!(
                        workspace.tabs()[0].image("a.png"),
                        Some(ImageState::Ready { .. })
                    )
                    && matches!(
                        workspace.tabs()[1].image("tall.png"),
                        Some(ImageState::Ready { .. })
                    )
            });
            if ready {
                break;
            }
            assert!(Instant::now() < deadline, "minimum-panel images did not settle");
            std::thread::sleep(Duration::from_millis(20));
        }
        let tall = cx.debug_bounds("document-image-left").unwrap();
        let tall_frame = cx.debug_bounds("document-frame-left").unwrap();
        let normal = cx.debug_bounds("document-image-right").unwrap();
        let normal_frame = cx.debug_bounds("document-frame-right").unwrap();
        for (image, frame) in [(tall, tall_frame), (normal, normal_frame)] {
            assert!(
                f32::from(image.size.width) <= f32::from(frame.size.width) + 0.5,
                "minimum-panel image {image:?} overflows {frame:?}"
            );
            assert!(
                (f32::from(image.size.width) - 52.0).abs() < 1.5,
                "minimum-panel image ignored the 52px content width: {image:?}"
            );
        }
        assert!(
            (f32::from(tall.size.width) / f32::from(tall.size.height) - 100.0 / 4000.0).abs()
                < 0.001,
            "tall image lost its aspect ratio: {tall:?}"
        );
        assert!(
            tall.size.height > tall_frame.size.height,
            "tall image should overflow the frame vertically: {tall:?} {tall_frame:?}"
        );
        let before = tall.top();
        // Negative Y scrolls toward the end of the content (the VFS listing
        // test uses the same convention); a positive delta at offset 0 cannot
        // move anything.
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: tall_frame.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-800.))),
            ..Default::default()
        });
        cx.run_until_parked();
        let after = cx.debug_bounds("document-image-left").unwrap().top();
        assert!(
            after < before,
            "tall image did not scroll vertically: before {before:?} after {after:?}"
        );
    }

    #[gpui::test]
    fn rendered_links_navigate_local_vfs_and_blocked_links_stay_inert(
        cx: &mut gpui::TestAppContext,
    ) {
        let _guard = DAEMON_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let bin = remuda_bin();
        let daemon = Daemon::start(&bin);
        daemon.put(
            &bin,
            "index.md",
            b"# Index\n\nGo to [one](one.md) or [two](two.md).\n\n[external](https://example.com) is blocked.\n",
        );
        daemon.put(&bin, "one.md", b"# One\n\nOne prose.\n");
        daemon.put(&bin, "two.md", b"# Two\n\nTwo prose.\n");
        let (view, cx) = host(daemon.socket(), cx);
        view.update(cx, |host, cx| {
            host.documents.as_mut().unwrap().open_in_new_tab("index.md");
            cx.notify();
        });
        settle(&view, cx);
        assert!(cx.debug_bounds("md-link-left-1-1").is_some());
        assert!(cx.debug_bounds("md-link-left-1-3").is_some());
        assert!(cx.debug_bounds("md-link-blocked-left-2-0").is_some());

        let one = cx.debug_bounds("md-link-left-1-1").unwrap();
        cx.simulate_click(one.center(), gpui::Modifiers::none());
        settle(&view, cx);
        let (location, text) = cx.read(|app| {
            let workspace = view.read(app).documents.as_ref().unwrap();
            (
                workspace.active().location().to_owned(),
                workspace.frame(workspace.active_index(), 80, 20).join("\n"),
            )
        });
        assert_eq!(location, "one.md");
        if !text.contains("One prose.") {
            wait_for_text(&view, cx, "One prose.");
        }
        let back = cx.debug_bounds("doc-back").unwrap();
        cx.simulate_click(back.center(), gpui::Modifiers::none());
        settle(&view, cx);
        assert_eq!(
            cx.read(|app| view
                .read(app)
                .documents
                .as_ref()
                .unwrap()
                .active()
                .location()
                .to_owned()),
            "index.md"
        );

        let two = cx.debug_bounds("md-link-left-1-3").unwrap();
        cx.simulate_click(two.center(), gpui::Modifiers::none());
        settle(&view, cx);
        assert_eq!(
            cx.read(|app| view
                .read(app)
                .documents
                .as_ref()
                .unwrap()
                .active()
                .location()
                .to_owned()),
            "two.md"
        );
        let back = cx.debug_bounds("doc-back").unwrap();
        cx.simulate_click(back.center(), gpui::Modifiers::none());
        settle(&view, cx);

        let blocked = cx.debug_bounds("md-link-blocked-left-2-0").unwrap();
        cx.simulate_click(blocked.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            cx.read(|app| view
                .read(app)
                .documents
                .as_ref()
                .unwrap()
                .active()
                .location()
                .to_owned()),
            "index.md"
        );

        // Keyboard reuses the shared Browser link selection: l selects the
        // next link, Enter follows it, and Back returns.
        let frame = cx.debug_bounds("document-frame-left").unwrap();
        cx.simulate_click(frame.center(), gpui::Modifiers::none());
        cx.simulate_keystrokes("l enter");
        settle(&view, cx);
        assert_eq!(
            cx.read(|app| view
                .read(app)
                .documents
                .as_ref()
                .unwrap()
                .active()
                .location()
                .to_owned()),
            "one.md"
        );
    }

    struct RuntimeEnv(Option<std::ffi::OsString>);

    impl Drop for RuntimeEnv {
        fn drop(&mut self) {
            match &self.0 {
                Some(value) => std::env::set_var("REMUDA_RUNTIME_DIR", value),
                None => std::env::remove_var("REMUDA_RUNTIME_DIR"),
            }
        }
    }

    #[gpui::test]
    fn closing_documents_keeps_the_attached_session_process(cx: &mut gpui::TestAppContext) {
        let _guard = DAEMON_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let bin = remuda_bin();
        let daemon = Daemon::start(&bin);
        daemon.start_session(&bin, "keep", &["sleep", "60"]);
        let deadline = Instant::now() + Duration::from_secs(15);
        while !daemon.session_alive(&bin, "keep") {
            assert!(Instant::now() < deadline, "private session did not start");
            std::thread::sleep(Duration::from_millis(50));
        }
        let instance = daemon
            .sessions(&bin)
            .into_iter()
            .find(|row| row["name"] == "keep")
            .and_then(|row| row["instance_id"].as_str().map(str::to_owned))
            .expect("session instance");
        let _env = RuntimeEnv(std::env::var_os("REMUDA_RUNTIME_DIR"));
        std::env::set_var("REMUDA_RUNTIME_DIR", &daemon.runtime);
        let socket = daemon.socket();
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = crate::LocalGui::new();
            gui.preview_busy = true;
            gui.sessions = vec![crate::Session {
                name: "keep".into(),
                instance_id: Some(instance),
                alive: true,
                unavailable: false,
                attached: false,
                depth: 0,
                state: None,
                detail: None,
                structured: true,
            }];
            gui.select_session(gui.sessions[0].clone());
            gui.documents = Some(Workspace::new(socket.clone()));
            gui
        });
        view.update(cx, |gui, _| {
            gui.request_attach();
            gui.attach_selected();
        });
        cx.run_until_parked();
        let (has_terminal, status) = cx.read(|app| {
            let gui = view.read(app);
            (gui.terminal.is_some(), gui.terminal_status.clone())
        });
        assert!(has_terminal, "attach did not start: {status}");

        let toggle = cx.debug_bounds("documents-toggle").unwrap();
        cx.simulate_click(toggle.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        let (documents, terminal) = cx.read(|app| {
            let gui = view.read(app);
            (gui.documents.is_some(), gui.terminal.is_some())
        });
        assert!(!documents, "closing documents must hide the panel");
        assert!(terminal, "closing documents must keep the terminal");
        assert!(cx.debug_bounds("terminal-screen").is_some());
        std::thread::sleep(Duration::from_millis(250));
        assert!(
            daemon.session_alive(&bin, "keep"),
            "the isolated session process must outlive the document panel"
        );
        view.update(cx, |gui, _| gui.terminal = None);
    }

    #[gpui::test]
    fn tabs_split_focus_and_close_with_two_documents(cx: &mut gpui::TestAppContext) {
        let bin = remuda_bin();
        let daemon = Daemon::start(&bin);
        daemon.put(&bin, "alpha.md", b"# Alpha\n\nRead [beta](beta.md).\n");
        daemon.put(
            &bin,
            "beta.py",
            b"def greet(name):\n    return \"hi \" + name\n",
        );
        let (view, cx) = host(daemon.socket(), cx);
        view.update(cx, |host, cx| {
            let workspace = host.documents.as_mut().unwrap();
            workspace.open_in_new_tab("alpha.md");
            workspace.open_in_new_tab("beta.py");
            cx.notify();
        });
        settle(&view, cx);
        for id in [
            "document-tabs",
            "document-tab-0",
            "document-tab-1",
            "document-tab-2",
            "document-new-tab",
            "document-split",
            "document-pane-left",
        ] {
            assert!(cx.debug_bounds(id).is_some(), "missing {id}");
        }
        let beta_tab = cx.debug_bounds("document-tab-2").unwrap();
        cx.simulate_click(beta_tab.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            cx.read(|app| view.read(app).documents.as_ref().unwrap().active_index()),
            2
        );
        let alpha_tab = cx.debug_bounds("document-tab-1").unwrap();
        cx.simulate_click(alpha_tab.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            cx.read(|app| view.read(app).documents.as_ref().unwrap().active_index()),
            1
        );
        let split = cx.debug_bounds("document-split").unwrap();
        cx.simulate_click(split.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(cx.debug_bounds("document-pane-right").is_some());
        let (left, right, split_index) = cx.read(|app| {
            let workspace = view.read(app).documents.as_ref().unwrap();
            (
                workspace.frame(1, 60, 20).join("\n"),
                workspace.frame(2, 60, 20).join("\n"),
                workspace.split_index(),
            )
        });
        assert_eq!(split_index, Some(2));
        assert!(left.contains("Read beta") && right.contains("def greet"));
        let right_pane = cx.debug_bounds("document-pane-right").unwrap();
        cx.simulate_click(right_pane.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        let (active, split_index) = cx.read(|app| {
            let workspace = view.read(app).documents.as_ref().unwrap();
            (workspace.active_index(), workspace.split_index())
        });
        assert_eq!(active, 2);
        assert_eq!(split_index, Some(1));
        let close = cx.debug_bounds("document-tab-close-2").unwrap();
        cx.simulate_click(close.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        let (len, split_index) = cx.read(|app| {
            let workspace = view.read(app).documents.as_ref().unwrap();
            (workspace.len(), workspace.split_index())
        });
        assert_eq!(len, 2);
        assert_eq!(split_index, None);
        let close = cx.debug_bounds("document-tab-close-1").unwrap();
        cx.simulate_click(close.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            cx.read(|app| view.read(app).documents.as_ref().unwrap().len()),
            1
        );
    }
}
