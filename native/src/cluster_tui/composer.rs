use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineComposer {
    text: String,
    cursor: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComposerAction {
    None,
    Submit(Vec<u8>),
    Cleared,
    Detach,
}

impl LineComposer {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn restore_draft(&mut self, text: &str) {
        self.text = text.into();
        self.cursor = self.text.chars().count();
    }

    pub fn handle_key(
        &mut self,
        event: KeyEvent,
        keymap: &crate::keybindings::Snapshot,
    ) -> ComposerAction {
        match keymap.resolve("cluster.composer", event) {
            Some("cluster.composer.clear") => {
                self.clear();
                ComposerAction::Cleared
            }
            Some("cluster.composer.back" | "cluster.composer.detach") => ComposerAction::Detach,
            Some("cluster.composer.left") => {
                self.cursor = self.cursor.saturating_sub(1);
                ComposerAction::None
            }
            Some("cluster.composer.right") => {
                self.cursor = (self.cursor + 1).min(self.text.chars().count());
                ComposerAction::None
            }
            Some("cluster.composer.home") => {
                self.cursor = 0;
                ComposerAction::None
            }
            Some("cluster.composer.end") => {
                self.cursor = self.text.chars().count();
                ComposerAction::None
            }
            Some("cluster.composer.backspace") => {
                self.backspace();
                ComposerAction::None
            }
            Some("cluster.composer.delete") => {
                self.delete();
                ComposerAction::None
            }
            Some("cluster.composer.submit") => {
                let mut bytes = self.text.as_bytes().to_vec();
                bytes.push(b'\r');
                self.clear();
                ComposerAction::Submit(bytes)
            }
            _ => {
                if !event
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    if let KeyCode::Char(character) = event.code {
                        self.insert(character);
                    }
                }
                ComposerAction::None
            }
        }
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    fn insert(&mut self, character: char) {
        let offset = self.byte_offset();
        self.text.insert(offset, character);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = self.byte_offset();
        self.cursor -= 1;
        let start = self.byte_offset();
        self.text.replace_range(start..end, "");
    }

    fn delete(&mut self) {
        if self.cursor >= self.text.chars().count() {
            return;
        }
        let start = self.byte_offset();
        let end = self.text[start..]
            .char_indices()
            .nth(1)
            .map_or(self.text.len(), |(offset, _)| start + offset);
        self.text.replace_range(start..end, "");
    }

    fn byte_offset(&self) -> usize {
        self.text
            .char_indices()
            .nth(self.cursor)
            .map_or(self.text.len(), |(offset, _)| offset)
    }
}

#[cfg(test)]
mod tests {
    use super::{ComposerAction, LineComposer};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn line_composer_edits_and_submits_cr() {
        let keymap = crate::keybindings::test_snapshot("");
        let mut composer = LineComposer::default();
        composer.handle_key(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
            &keymap,
        );
        composer.handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
            &keymap,
        );
        composer.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &keymap);
        composer.handle_key(
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
            &keymap,
        );
        assert_eq!(composer.text(), "abc");
        assert_eq!(
            composer.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &keymap),
            ComposerAction::Submit(b"abc\r".to_vec())
        );
        assert_eq!(composer.text(), "");
    }

    #[test]
    fn ctrl_c_clears_while_ctrl_backslash_detaches_and_q_is_text() {
        let keymap = crate::keybindings::test_snapshot("");
        let mut composer = LineComposer::default();
        composer.handle_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            &keymap,
        );
        assert_eq!(composer.text(), "q");
        assert_eq!(
            composer.handle_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL,),
                &keymap
            ),
            ComposerAction::Cleared
        );
        assert_eq!(composer.text(), "");
        assert_eq!(
            composer.handle_key(
                KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL,),
                &keymap
            ),
            ComposerAction::Detach
        );
        assert_eq!(
            composer.handle_key(
                KeyEvent::new(KeyCode::Char('4'), KeyModifiers::CONTROL),
                &keymap
            ),
            ComposerAction::Detach
        );
        assert_eq!(
            composer.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &keymap),
            ComposerAction::Detach
        );
    }
}
