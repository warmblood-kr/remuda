use gpui::{
    canvas, div, fill, point, prelude::*, px, rgb, size, Bounds, Keystroke, Pixels, StyledText,
    TextLayout,
};
use remuda_native::strict_input::MAX_BATCH;
use std::{cell::RefCell, collections::VecDeque, ops::Range, rc::Rc};
use unicode_segmentation::UnicodeSegmentation;

const HISTORY_LIMIT: usize = 100;
pub(crate) type LineHistory = Rc<RefCell<VecDeque<String>>>;

struct HistoryRecall {
    index: usize,
    draft: String,
    draft_cursor: usize,
}

#[derive(Default)]
pub(crate) struct LineInput {
    text: String,
    cursor: usize,
    marked: Option<Range<usize>>,
    history: LineHistory,
    recall: Option<HistoryRecall>,
    preferred_x: Option<Pixels>,
    layout: Rc<RefCell<Option<TextLayout>>>,
}

impl LineInput {
    pub(crate) fn with_history(history: LineHistory) -> Self {
        Self {
            history,
            ..Self::default()
        }
    }

    pub(crate) fn insert(&mut self, text: &str) {
        self.replace(None, text, false);
    }

    fn offset(&self, utf16: usize) -> usize {
        let mut units = 0;
        for (offset, ch) in self.text.char_indices() {
            units += ch.len_utf16();
            if units > utf16 {
                return offset;
            }
        }
        self.text.len()
    }

    fn utf16(&self, offset: usize) -> usize {
        self.text[..offset].encode_utf16().count()
    }

    pub(crate) fn composing(&self) -> bool {
        self.marked.is_some()
    }

    fn replace(&mut self, native: Option<Range<usize>>, text: &str, mark: bool) {
        let text: String = text
            .chars()
            .filter(|ch| {
                *ch == '\n'
                    || *ch == '\t'
                    || (!ch.is_control()
                        && !remuda_core::protocol::is_secret_prompt_format_or_separator(*ch))
            })
            .collect();
        let range = native
            .filter(|r| r.start <= r.end)
            .map(|r| self.offset(r.start)..self.offset(r.end))
            .or_else(|| self.marked.clone())
            .unwrap_or(self.cursor..self.cursor);
        if self.text.len() - range.len() + text.len() < MAX_BATCH {
            self.preferred_x = None;
            let end = range.start + text.len();
            self.marked = (mark && !text.is_empty()).then_some(range.start..end);
            self.text.replace_range(range, &text);
            self.cursor = if mark {
                end
            } else {
                self.text
                    .grapheme_indices(true)
                    .map(|(i, _)| i)
                    .find(|&i| i >= end)
                    .unwrap_or(self.text.len())
            };
        }
    }

    pub(crate) fn edit(&mut self, key: &Keystroke) {
        let modifiers = key.modifiers;
        let name = key.key.as_str();
        if !modifiers.modified() && matches!(name, "up" | "down") {
            let up = name == "up";
            if !self.move_vertical(up) {
                self.recall_history(up);
            }
            return;
        }
        self.preferred_x = None;
        let left = self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i);
        let right = self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(self.cursor, |g| self.cursor + g.len());
        let emacs = modifiers.control && !modifiers.platform && !modifiers.alt && !modifiers.shift;
        let line_start = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        let line_end = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |i| self.cursor + i);
        match name {
            "a" if emacs => self.cursor = line_start,
            "e" if emacs => self.cursor = line_end,
            "f" if emacs => self.cursor = right,
            "b" if emacs => self.cursor = left,
            "k" if emacs => {
                // At the end of a logical line, kill its newline.
                let end = if line_end == self.cursor {
                    (line_end + 1).min(self.text.len())
                } else {
                    line_end
                };
                self.text.drain(self.cursor..end);
            }
            "left" => self.cursor = left,
            "right" => self.cursor = right,
            "home" => self.cursor = 0,
            "end" => self.cursor = self.text.len(),
            "backspace" => {
                self.text.drain(left..self.cursor);
                self.cursor = left;
            }
            "delete" => {
                self.text.drain(self.cursor..right);
            }
            _ if !key.modifiers.control && !key.modifiers.platform => {
                if let Some(text) = &key.key_char {
                    self.insert(text);
                }
            }
            _ => {}
        }
    }

    fn move_vertical(&mut self, up: bool) -> bool {
        let layout = self.layout.borrow();
        let Some(layout) = layout.as_ref() else {
            return false;
        };
        let Some(position) = layout.position_for_index(self.cursor) else {
            return false;
        };
        let height = layout.line_height();
        let row_offset = if up { -0.5 } else { 1.5 };
        let y = position.y + height * row_offset;
        let bounds = layout.bounds();
        if y < bounds.top() || y >= bounds.bottom() {
            return false;
        }
        let x = *self.preferred_x.get_or_insert(position.x - bounds.left());
        let x = x + bounds.left();
        let index = layout
            .index_for_position(point(x, y))
            .unwrap_or_else(|index| index);
        let row_y = y - height * 0.5;
        let mut boundaries = self
            .text
            .grapheme_indices(true)
            .map(|(index, _)| index)
            .chain([self.text.len()])
            .peekable();
        let mut previous = None;
        let mut closest = None;
        while boundaries.peek().is_some_and(|&boundary| boundary <= index) {
            previous = closest;
            closest = boundaries.next();
        }
        // The layout maps an edge of a wrapped row to the adjacent row's byte
        // index. Only consider caret positions that land on the intended row.
        let index = [previous, closest, boundaries.next()]
            .into_iter()
            .flatten()
            .filter_map(|index| {
                layout
                    .position_for_index(index)
                    .map(|position| (index, position))
            })
            .filter(|(_, position)| (position.y - row_y).abs() < px(0.5))
            .min_by(|(_, a), (_, b)| {
                (a.x - x)
                    .abs()
                    .partial_cmp(&(b.x - x).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(index, _)| index);
        if let Some(index) = index {
            self.cursor = index;
        }
        true
    }

    fn recall_history(&mut self, up: bool) {
        let history = self.history.borrow();
        if history.is_empty() {
            return;
        }
        self.preferred_x = None;
        match (&mut self.recall, up) {
            (None, false) => return,
            (None, true) => {
                self.recall = Some(HistoryRecall {
                    index: history.len() - 1,
                    draft: std::mem::take(&mut self.text),
                    draft_cursor: self.cursor,
                });
            }
            (Some(recall), true) => {
                if recall.index == 0 {
                    return;
                }
                recall.index -= 1;
            }
            (Some(recall), false) if recall.index + 1 < history.len() => recall.index += 1,
            (Some(_), false) => {
                let recall = self.recall.take().unwrap();
                self.text = recall.draft;
                self.cursor = recall.draft_cursor;
                return;
            }
        }
        self.text
            .clone_from(&history[self.recall.as_ref().unwrap().index]);
        self.cursor = self.text.len();
    }

    pub(crate) fn bytes(&self) -> Vec<u8> {
        let mut bytes = self.text.as_bytes().to_vec();
        bytes.push(b'\r');
        bytes
    }

    pub(crate) fn queued(&mut self) {
        self.marked = None;
        if !self.text.trim().is_empty() {
            let mut history = self.history.borrow_mut();
            if history.back() != Some(&self.text) {
                if history.len() == HISTORY_LIMIT {
                    history.pop_front();
                }
                history.push_back(std::mem::take(&mut self.text));
            }
        }
        self.text.clear();
        self.cursor = 0;
        self.recall = None;
        self.preferred_x = None;
    }

    pub(crate) fn view(&self, focused: bool) -> impl IntoElement {
        let empty = self.text.is_empty();
        let display = StyledText::new(if empty {
            "Type locally · Enter sends · Esc closes".to_string()
        } else {
            self.text.clone()
        });
        let layout = display.layout().clone();
        let layout_cache = self.layout.clone();
        let cursor = self.cursor;
        div()
            .id("line-composer")
            .debug_selector(|| "line-composer".into())
            .w_full()
            .max_h(px(144.))
            .overflow_y_scroll()
            .px_2()
            .py_1()
            .text_sm()
            .bg(rgb(0x233145))
            .child(
                div()
                    .relative()
                    .min_w_0()
                    .whitespace_normal()
                    .when(empty, |d| d.pl_1().text_color(rgb(0xb5beca)))
                    .child(display)
                    .when(empty && focused, |d| {
                        d.child(
                            div()
                                .absolute()
                                .left_0()
                                .top(px(2.))
                                .w(px(2.))
                                .h(px(14.))
                                .bg(rgb(0xe8f2ff))
                                .debug_selector(|| "line-composer-caret".into()),
                        )
                    })
                    .child(
                        canvas(
                            move |_, _, _| {
                                let caret = (!empty && focused)
                                    .then(|| {
                                        let origin = layout.position_for_index(cursor)?
                                            + point(px(0.), px(2.));
                                        Some(Bounds {
                                            origin,
                                            size: size(
                                                px(2.),
                                                (layout.line_height() - px(4.)).max(px(1.)),
                                            ),
                                        })
                                    })
                                    .flatten();
                                *layout_cache.borrow_mut() = (!empty).then_some(layout);
                                caret
                            },
                            |_, caret, window, _| {
                                if let Some(bounds) = caret {
                                    window.paint_quad(fill(bounds, rgb(0xe8f2ff)));
                                }
                            },
                        )
                        .absolute()
                        .size_full(),
                    ),
            )
    }
}

impl gpui::EntityInputHandler for crate::LocalGui {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> Option<String> {
        let editor = self.line_input.as_ref()?;
        let start = editor.offset(range.start);
        let end = editor.offset(range.end);
        *adjusted = Some(editor.utf16(start)..editor.utf16(end));
        editor.text.get(start..end).map(str::to_owned)
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> Option<gpui::UTF16Selection> {
        let editor = self.line_input.as_ref()?;
        let at = editor.utf16(editor.cursor);
        Some(gpui::UTF16Selection {
            range: at..at,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> Option<Range<usize>> {
        let editor = self.line_input.as_ref()?;
        let marked = editor.marked.as_ref()?;
        Some(editor.utf16(marked.start)..editor.utf16(marked.end))
    }

    fn unmark_text(&mut self, _: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        if let Some(editor) = &mut self.line_input {
            editor.marked = None;
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if let Some(editor) = &mut self.line_input {
            editor.replace(range, text, false);
            cx.notify();
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if let Some(editor) = &mut self.line_input {
            editor.replace(range, text, true);
            cx.notify();
        }
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        bounds: gpui::Bounds<gpui::Pixels>,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> Option<gpui::Bounds<gpui::Pixels>> {
        Some(bounds)
    }

    fn character_index_for_point(
        &mut self,
        _: gpui::Point<gpui::Pixels>,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_composition_replaces_jamo_without_sending_intermediate_text() {
        let mut editor = LineInput::default();
        editor.insert("🙂 ");
        editor.replace(None, "ㄲ", true);
        editor.replace(Some(3..4), "꼬", true);
        editor.replace(None, "꽈", true);
        assert!(editor.composing());
        editor.replace(None, "꽜", false);
        assert!(!editor.composing());
        assert_eq!(editor.bytes(), "🙂 꽜\r".as_bytes());
    }

    struct Draft(LineInput);
    impl gpui::Render for Draft {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            _: &mut gpui::Context<Self>,
        ) -> impl IntoElement {
            div().w(px(230.)).child(self.0.view(true))
        }
    }

    #[gpui::test]
    fn long_draft_wraps_and_keeps_its_payload(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| Draft(LineInput::default()));
        let empty = cx.debug_bounds("line-composer").unwrap().size.height;
        let value = "꽜 값 쌍자음 and a long draft ".repeat(8);
        view.update(cx, |draft, cx| {
            draft.0.insert(&value);
            cx.notify();
        });
        let wrapped = cx.debug_bounds("line-composer").unwrap().size.height;
        assert!(wrapped > empty);
        assert_eq!(
            cx.read(|app| view.read(app).0.bytes()),
            format!("{value}\r").as_bytes()
        );
    }

    #[gpui::test]
    fn wrapped_draft_moves_by_visual_row_without_changing_text_geometry(
        cx: &mut gpui::TestAppContext,
    ) {
        let value = "Wide 꽜 🙂 좁은 i Ω ".repeat(8);
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut editor = LineInput::default();
            editor.insert(&value);
            Draft(editor)
        });
        let bottom = cx.read(|app| {
            let editor = &view.read(app).0;
            let layout = editor.layout.borrow();
            let layout = layout.as_ref().unwrap();
            (
                layout.bounds(),
                layout.position_for_index("Wide 꽜 🙂".len()).unwrap(),
                layout.position_for_index(editor.cursor).unwrap(),
            )
        });
        view.update(cx, |draft, cx| {
            draft.0.edit(&Keystroke::parse("up").unwrap());
            cx.notify();
        });
        let after_up = cx.read(|app| {
            let editor = &view.read(app).0;
            let layout = editor.layout.borrow();
            let layout = layout.as_ref().unwrap();
            let caret = layout.position_for_index(editor.cursor).unwrap();
            assert_eq!(layout.bounds(), bottom.0);
            assert_eq!(
                layout.position_for_index("Wide 꽜 🙂".len()).unwrap(),
                bottom.1
            );
            assert!((bottom.2.y - caret.y - layout.line_height()).abs() < px(0.5));
            assert_eq!(editor.text, value);
            caret
        });
        view.update(cx, |draft, cx| {
            draft.0.edit(&Keystroke::parse("left").unwrap());
            cx.notify();
        });
        cx.read(|app| {
            let editor = &view.read(app).0;
            let layout = editor.layout.borrow();
            let layout = layout.as_ref().unwrap();
            assert_eq!(layout.bounds(), bottom.0);
            assert_eq!(
                layout.position_for_index("Wide 꽜 🙂".len()).unwrap(),
                bottom.1,
            );
            assert!(layout.position_for_index(editor.cursor).unwrap().x < after_up.x);
        });
    }
}
