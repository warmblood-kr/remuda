use gpui::{div, prelude::*, px, rgb, Keystroke};
use remuda_native::strict_input::MAX_BATCH;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Default)]
pub(crate) struct LineInput {
    text: String,
    cursor: usize,
    marked: Option<Range<usize>>,
}

impl LineInput {
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
        let left = self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i);
        let right = self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(self.cursor, |g| self.cursor + g.len());
        match key.key.as_str() {
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

    pub(crate) fn bytes(&self) -> Vec<u8> {
        let mut bytes = self.text.as_bytes().to_vec();
        bytes.push(b'\r');
        bytes
    }

    pub(crate) fn queued(&mut self) {
        self.marked = None;
        self.text.clear();
        self.cursor = 0;
    }

    pub(crate) fn view(&self, focused: bool) -> impl IntoElement {
        let mut display = self.text.clone();
        if focused && !display.is_empty() {
            display.insert(self.cursor, '│');
        }
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
            .child(if display.is_empty() {
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .when(focused, |d| {
                        d.child(
                            div()
                                .debug_selector(|| "line-composer-caret".into())
                                .w(px(2.))
                                .h(px(14.))
                                .bg(rgb(0xe8f2ff)),
                        )
                    })
                    .text_color(rgb(0xb5beca))
                    .child("Type locally · Enter sends · Esc closes")
            } else {
                div().min_w_0().whitespace_normal().child(display)
            })
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
}
