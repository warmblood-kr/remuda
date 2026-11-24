use gpui::{div, prelude::*, px, rgb, Div, Keystroke};
use remuda_native::strict_input::MAX_BATCH;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Default)]
pub(crate) struct LineInput {
    text: String,
    cursor: usize,
}

impl LineInput {
    pub(crate) fn insert(&mut self, text: &str) {
        let text: String = text
            .chars()
            .filter(|ch| {
                *ch == '\n'
                    || *ch == '\t'
                    || (!ch.is_control()
                        && !remuda_core::protocol::is_secret_prompt_format_or_separator(*ch))
            })
            .collect();
        if self.text.len() + text.len() < MAX_BATCH {
            self.text.insert_str(self.cursor, &text);
            let end = self.cursor + text.len();
            self.cursor = self
                .text
                .grapheme_indices(true)
                .map(|(i, _)| i)
                .find(|&i| i >= end)
                .unwrap_or(self.text.len());
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
        self.text.clear();
        self.cursor = 0;
    }

    pub(crate) fn view(&self) -> Div {
        let before = self.text[..self.cursor]
            .rsplit('\n')
            .next()
            .unwrap_or_default();
        let after = self.text[self.cursor..]
            .split('\n')
            .next()
            .unwrap_or_default();
        div()
            .debug_selector(|| "line-composer".into())
            .flex()
            .gap_2()
            .items_center()
            .px_2()
            .py_1()
            .text_sm()
            .bg(rgb(0x233145))
            .child("›")
            .child(if self.text.is_empty() {
                div()
                    .text_color(rgb(0xb5beca))
                    .child("Type locally · Enter sends · Esc closes")
            } else {
                div()
                    .flex()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(before.to_owned())
                    .child(div().w(px(1.)).bg(rgb(0xadcceb)))
                    .child(after.to_owned())
                    .when(self.text.contains('\n'), |d| {
                        d.child(
                            div()
                                .pl_2()
                                .text_color(rgb(0xb5beca))
                                .child(format!("{} lines", self.text.lines().count())),
                        )
                    })
            })
    }
}
