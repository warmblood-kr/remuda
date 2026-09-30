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

    pub fn handle_key(&mut self, event: KeyEvent) -> ComposerAction {
        if event.modifiers.contains(KeyModifiers::CONTROL) {
            return match event.code {
                KeyCode::Char('c') => {
                    self.clear();
                    ComposerAction::Cleared
                }
                // Crossterm decodes the raw Ctrl-\\ byte as Ctrl-4 on Unix.
                KeyCode::Char('\\' | '4') => ComposerAction::Detach,
                _ => ComposerAction::None,
            };
        }
        match event.code {
            KeyCode::Char(character) if !event.modifiers.contains(KeyModifiers::ALT) => {
                self.insert(character);
                ComposerAction::None
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                ComposerAction::None
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.text.chars().count());
                ComposerAction::None
            }
            KeyCode::Home => {
                self.cursor = 0;
                ComposerAction::None
            }
            KeyCode::End => {
                self.cursor = self.text.chars().count();
                ComposerAction::None
            }
            KeyCode::Backspace => {
                self.backspace();
                ComposerAction::None
            }
            KeyCode::Delete => {
                self.delete();
                ComposerAction::None
            }
            KeyCode::Enter => {
                let mut bytes = self.text.as_bytes().to_vec();
                bytes.push(b'\r');
                self.clear();
                ComposerAction::Submit(bytes)
            }
            _ => ComposerAction::None,
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
        let mut composer = LineComposer::default();
        composer.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        composer.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        composer.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        composer.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        assert_eq!(composer.text(), "abc");
        assert_eq!(
            composer.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ComposerAction::Submit(b"abc\r".to_vec())
        );
        assert_eq!(composer.text(), "");
    }

    #[test]
    fn ctrl_c_clears_while_ctrl_backslash_detaches_and_q_is_text() {
        let mut composer = LineComposer::default();
        composer.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert_eq!(composer.text(), "q");
        assert_eq!(
            composer.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL,)),
            ComposerAction::Cleared
        );
        assert_eq!(composer.text(), "");
        assert_eq!(
            composer.handle_key(KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL,)),
            ComposerAction::Detach
        );
        assert_eq!(
            composer.handle_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::CONTROL)),
            ComposerAction::Detach
        );
    }
}
