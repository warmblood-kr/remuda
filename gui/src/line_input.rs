use gpui::{div, prelude::*, rgb, App, Context, Entity, EntityInputHandler, KeyBinding, Window};
use gpui_kit::base::input::{
    BufferPoint, Enter, Escape, MoveDown, MoveLeft, MoveRight, MoveToEnd, MoveToStart, MoveUp,
    Paste, RopeExt, Textarea, TextareaState,
};
use remuda_native::strict_input::MAX_BATCH;
use std::{cell::RefCell, collections::VecDeque, rc::Rc};

const HISTORY_LIMIT: usize = 100;
pub(crate) type LineHistory = Rc<RefCell<VecDeque<String>>>;

gpui::actions!(composer, [LogicalHome, LogicalEnd, KillLine]);

struct Initialized;
impl gpui::Global for Initialized {}

fn init(cx: &mut App) {
    if cx.has_global::<Initialized>() {
        return;
    }
    gpui_kit::init(cx);
    cx.bind_keys([
        KeyBinding::new("home", MoveToStart, Some("Input")),
        KeyBinding::new("end", MoveToEnd, Some("Input")),
        KeyBinding::new("ctrl-a", LogicalHome, Some("Input")),
        KeyBinding::new("ctrl-e", LogicalEnd, Some("Input")),
        KeyBinding::new("ctrl-f", MoveRight, Some("Input")),
        KeyBinding::new("ctrl-b", MoveLeft, Some("Input")),
        KeyBinding::new("ctrl-k", KillLine, Some("Input")),
        KeyBinding::new("ctrl-shift-v", Paste, Some("Input")),
    ]);
    cx.set_global(Initialized);
}

struct HistoryRecall {
    index: usize,
    draft: String,
    cursor: usize,
}

pub(crate) struct LineInput {
    pub(crate) state: Entity<TextareaState>,
    history: LineHistory,
    recall: Option<HistoryRecall>,
}

impl LineInput {
    pub(crate) fn with_history(history: LineHistory, window: &mut Window, cx: &mut App) -> Self {
        init(cx);
        let state = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 7)
                .placeholder("Type locally · Enter sends · Esc closes")
                .submit_on_enter(true)
        });
        Self {
            state,
            history,
            recall: None,
        }
    }

    pub(crate) fn focus(&self, window: &mut Window, cx: &mut App) {
        self.state.update(cx, |state, cx| state.focus(window, cx));
    }

    pub(crate) fn composing(&self, window: &mut Window, cx: &mut App) -> bool {
        self.state.update(cx, |state, cx| {
            state.marked_text_range(window, cx).is_some()
        })
    }

    pub(crate) fn insert(&self, text: &str, window: &mut Window, cx: &mut App) {
        let text: String = text.chars().filter(|&ch| allowed(ch)).collect();
        self.state.update(cx, |state, cx| {
            if state.text().len() - state.selected_range().len() + text.len() < MAX_BATCH {
                state.replace_text_in_range(None, &text, window, cx);
            }
        });
    }

    pub(crate) fn bytes(&self, cx: &App) -> Result<Vec<u8>, &'static str> {
        let text = self.state.read(cx).value();
        if text.len() >= MAX_BATCH {
            return Err("Draft is too long · draft retained");
        }
        if !text.chars().all(allowed) {
            return Err("Draft contains unsupported characters · draft retained");
        }
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(b'\r');
        Ok(bytes)
    }

    pub(crate) fn queued(&mut self, window: &mut Window, cx: &mut App) {
        let text = self.state.read(cx).value();
        let mut history = self.history.borrow_mut();
        if !text.trim().is_empty() && history.back().is_none_or(|last| last != text.as_ref()) {
            if history.len() == HISTORY_LIMIT {
                history.pop_front();
            }
            history.push_back(text.to_string());
        }
        self.state
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.recall = None;
    }

    fn history_key(&mut self, up: bool, window: &mut Window, cx: &mut App) -> bool {
        if self.composing(window, cx) {
            return false;
        }
        let state = self.state.read(cx);
        let cursor = state.cursor();
        let point = state.text().offset_to_point(cursor);
        let map = state.display_map();
        let row = map
            .buffer_pos_to_display_pos(BufferPoint::new(point.row, point.column))
            .row;
        if (up && row > 0) || (!up && row + 1 < map.display_row_count()) {
            return false;
        }
        let history = self.history.borrow();
        if history.is_empty() {
            return false;
        }
        let (text, cursor) = match (&mut self.recall, up) {
            (None, false) => return false,
            (None, true) => {
                let index = history.len() - 1;
                self.recall = Some(HistoryRecall {
                    index,
                    draft: state.value().to_string(),
                    cursor,
                });
                (history[index].clone(), history[index].len())
            }
            (Some(recall), true) => {
                if recall.index == 0 {
                    return true;
                }
                recall.index -= 1;
                (history[recall.index].clone(), history[recall.index].len())
            }
            (Some(recall), false) if recall.index + 1 < history.len() => {
                recall.index += 1;
                (history[recall.index].clone(), history[recall.index].len())
            }
            (Some(_), false) => {
                let recall = self.recall.take().unwrap();
                (recall.draft, recall.cursor)
            }
        };
        self.state.update(cx, |state, cx| {
            state.set_value(text, window, cx);
            state.set_selected_range(cursor..cursor, cx);
        });
        true
    }

    fn logical_edit(&self, edit: LogicalEdit, window: &mut Window, cx: &mut App) {
        self.state.update(cx, |state, cx| {
            if state.marked_text_range(window, cx).is_some() {
                return;
            }
            let text = state.value();
            let cursor = state.cursor();
            let start = text[..cursor].rfind('\n').map_or(0, |offset| offset + 1);
            let end = text[cursor..]
                .find('\n')
                .map_or(text.len(), |offset| cursor + offset);
            match edit {
                LogicalEdit::Home => state.set_selected_range(start..start, cx),
                LogicalEdit::End => state.set_selected_range(end..end, cx),
                LogicalEdit::Kill => {
                    let end = if cursor == end {
                        (end + 1).min(text.len())
                    } else {
                        end
                    };
                    let start_utf16 = text[..cursor].encode_utf16().count();
                    let end_utf16 = text[..end].encode_utf16().count();
                    state.replace_text_in_range(Some(start_utf16..end_utf16), "", window, cx);
                }
            }
        });
    }

    pub(crate) fn view(&self, cx: &Context<crate::LocalGui>) -> impl IntoElement {
        div()
            .id("line-composer")
            .debug_selector(|| "line-composer".into())
            .w_full()
            .px_2()
            .py_1()
            .text_sm()
            .bg(rgb(0x233145))
            .capture_key_down(cx.listener(|gui, _, _, cx| gui.mark_terminal_input(cx)))
            .capture_action(cx.listener(|gui, action: &Enter, window, cx| {
                if action.secondary {
                    cx.stop_propagation();
                } else if !action.shift {
                    gui.submit_composer(window, cx);
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|gui, _: &Escape, window, cx| {
                gui.close_composer(window, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|gui, _: &Paste, window, cx| {
                gui.paste_composer(window, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|gui, _: &MoveUp, window, cx| {
                if gui
                    .line_input
                    .as_mut()
                    .is_some_and(|input| input.history_key(true, window, cx))
                {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|gui, _: &MoveDown, window, cx| {
                if gui
                    .line_input
                    .as_mut()
                    .is_some_and(|input| input.history_key(false, window, cx))
                {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|gui, _: &LogicalHome, window, cx| {
                if let Some(input) = &gui.line_input {
                    input.logical_edit(LogicalEdit::Home, window, cx);
                }
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|gui, _: &LogicalEnd, window, cx| {
                if let Some(input) = &gui.line_input {
                    input.logical_edit(LogicalEdit::End, window, cx);
                }
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|gui, _: &KillLine, window, cx| {
                if let Some(input) = &gui.line_input {
                    input.logical_edit(LogicalEdit::Kill, window, cx);
                }
                cx.stop_propagation();
            }))
            .child(Textarea::new(&self.state))
    }
}

enum LogicalEdit {
    Home,
    End,
    Kill,
}

fn allowed(ch: char) -> bool {
    ch == '\n'
        || ch == '\t'
        || (!ch.is_control() && !remuda_core::protocol::is_secret_prompt_format_or_separator(ch))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{size, TestAppContext};

    impl gpui::Render for LineInput {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            Textarea::new(&self.state)
        }
    }

    #[gpui::test]
    fn korean_preedit_commits_once_with_utf8_and_no_extra_line_break(cx: &mut TestAppContext) {
        let (editor, cx) = cx
            .add_window_view(|window, cx| LineInput::with_history(Default::default(), window, cx));
        editor.update_in(cx, |editor, window, cx| {
            editor.state.update(cx, |state, cx| {
                state.replace_and_mark_text_in_range(None, "ㅎ", Some(1..1), window, cx);
                state.replace_and_mark_text_in_range(None, "한", Some(1..1), window, cx);
            });
            assert!(editor.composing(window, cx));
            editor.state.update(cx, |state, cx| {
                state.replace_text_in_range(None, "한", window, cx);
            });
            assert!(!editor.composing(window, cx));
            editor.insert("글 🌿", window, cx);
            assert_eq!(editor.bytes(cx).unwrap(), "한글 🌿\r".as_bytes());
        });
    }

    #[gpui::test]
    fn wrapped_draft_prefers_native_movement_before_history(cx: &mut TestAppContext) {
        let (editor, cx) = cx.add_window_view(|window, cx| {
            let history = Rc::new(RefCell::new(VecDeque::from(["previous".into()])));
            LineInput::with_history(history, window, cx)
        });
        cx.simulate_resize(size(gpui::px(220.), gpui::px(220.)));
        let draft = "한글 🌿 ".repeat(25);
        editor.update_in(cx, |editor, window, cx| {
            editor.insert(&draft, window, cx);
        });
        editor.update_in(cx, |editor, window, cx| {
            assert!(editor.state.read(cx).display_map().display_row_count() > 1);
            assert!(!editor.history_key(true, window, cx));
            assert_eq!(editor.bytes(cx).unwrap(), format!("{draft}\r").as_bytes());
            editor
                .state
                .update(cx, |state, cx| state.set_selected_range(0..0, cx));
            assert!(editor.history_key(true, window, cx));
            assert_eq!(editor.bytes(cx).unwrap(), b"previous\r");
            assert!(editor.history_key(false, window, cx));
            assert_eq!(editor.state.read(cx).cursor(), 0);
        });
    }

    #[gpui::test]
    fn submission_bound_still_preserves_the_edit_buffer(cx: &mut TestAppContext) {
        let (editor, cx) = cx
            .add_window_view(|window, cx| LineInput::with_history(Default::default(), window, cx));
        editor.update_in(cx, |editor, window, cx| {
            let oversized = "x".repeat(MAX_BATCH);
            editor
                .state
                .update(cx, |state, cx| state.set_value(oversized, window, cx));
            assert!(editor.bytes(cx).is_err());
            assert_eq!(editor.state.read(cx).value().len(), MAX_BATCH);
        });
    }
}
