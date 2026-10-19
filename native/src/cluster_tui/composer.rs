use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Most entries kept per target session; the oldest is dropped first.
const HISTORY_LIMIT: usize = 500;

/// Submitted-line history for one target session. Memory only: no file I/O,
/// never serialized.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct History {
    entries: Vec<String>,
    /// Index of the recalled entry while browsing, `None` when editing a draft.
    browsing: Option<usize>,
    /// The in-progress draft (text, cursor) saved when browsing started.
    draft: (String, usize),
}

impl History {
    fn record(&mut self, text: &str) {
        self.browsing = None;
        if text.trim().is_empty() || self.entries.last().is_some_and(|last| last == text) {
            return;
        }
        self.entries.push(text.to_owned());
        if self.entries.len() > HISTORY_LIMIT {
            self.entries.remove(0);
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineComposer {
    text: String,
    cursor: usize,
    /// One history per target session: the composer is shared by every
    /// session, but recalling another session's commands would be misleading.
    histories: std::collections::HashMap<String, History>,
    history_key: String,
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

    /// Select which session's history Up/Down browse and submissions feed.
    /// Switching away abandons any browsing in the previous history.
    pub fn set_history_target(&mut self, key: &str) {
        if self.history_key != key {
            if let Some(history) = self.histories.get_mut(&self.history_key) {
                history.browsing = None;
            }
            self.history_key = key.to_owned();
        }
    }

    pub fn restore_draft(&mut self, text: &str) {
        self.text = text.into();
        self.cursor = self.text.graphemes(true).count();
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
            Some("cluster.composer.left" | "cluster.composer.backward") => {
                self.cursor = self.cursor.saturating_sub(1);
                ComposerAction::None
            }
            Some("cluster.composer.right" | "cluster.composer.forward") => {
                self.cursor = (self.cursor + 1).min(self.text.graphemes(true).count());
                ComposerAction::None
            }
            Some("cluster.composer.up") => {
                if self.line_bounds().0 == 0 {
                    self.recall(false);
                } else {
                    self.move_line(false);
                }
                ComposerAction::None
            }
            Some("cluster.composer.down") => {
                if self.line_bounds().1 >= self.text.graphemes(true).count() {
                    self.recall(true);
                } else {
                    self.move_line(true);
                }
                ComposerAction::None
            }
            Some("cluster.composer.kill") => {
                // Like readline: kill to the end of the current line, or join
                // the next line when already at its end.
                let (_, end) = self.line_bounds();
                let from = self.byte_offset();
                let to = if self.cursor == end {
                    self.text[from..]
                        .grapheme_indices(true)
                        .nth(1)
                        .map_or(self.text.len(), |(offset, _)| from + offset)
                } else {
                    self.byte_of(end)
                };
                self.text.replace_range(from..to, "");
                ComposerAction::None
            }
            Some("cluster.composer.home") => {
                self.cursor = 0;
                ComposerAction::None
            }
            Some("cluster.composer.home_emacs") => {
                self.cursor = self.line_bounds().0;
                ComposerAction::None
            }
            Some("cluster.composer.end") => {
                self.cursor = self.text.graphemes(true).count();
                ComposerAction::None
            }
            Some("cluster.composer.end_emacs") => {
                self.cursor = self.line_bounds().1;
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
                let key = self.history_key.clone();
                self.histories.entry(key).or_default().record(&self.text);
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

    pub(super) fn display_lines(&self, prefix: &str, width: usize, limit: usize) -> Vec<String> {
        let safe_prefix = crate::text::strip_terminal_controls(prefix);
        let mut cells: Vec<(String, bool)> = safe_prefix
            .graphemes(true)
            .map(|g| (g.to_owned(), false))
            .collect();
        cells.extend(
            self.text
                .graphemes(true)
                .enumerate()
                .map(|(i, g)| (Self::visible_grapheme(g), i == self.cursor)),
        );
        if self.cursor == self.text.graphemes(true).count() {
            cells.push((" ".into(), true));
        }
        let mut rows = vec![String::new()];
        let mut used = 0;
        let mut cursor_row = 0;
        for (cell, cursor) in cells {
            let size = UnicodeWidthStr::width(cell.as_str());
            if used + size > width.max(1) && used > 0 {
                rows.push(String::new());
                used = 0;
            }
            if cursor {
                cursor_row = rows.len() - 1;
                // Inverse the existing glyph; the cursor consumes no extra cell.
                rows.last_mut()
                    .unwrap()
                    .push_str(&format!("\x1b[7m{cell}\x1b[27m"));
            } else {
                rows.last_mut().unwrap().push_str(&cell);
            }
            used += size;
        }
        let limit = limit.max(1);
        let start = cursor_row
            .saturating_sub(1)
            .min(rows.len().saturating_sub(limit));
        rows.into_iter().skip(start).take(limit).collect()
    }

    fn visible_grapheme(grapheme: &str) -> String {
        match grapheme {
            "\r\n" => "␍↵".into(),
            "\r" => "␍".into(),
            "\n" => "↵".into(),
            "\t" => "⇥".into(),
            _ => crate::text::strip_terminal_controls(grapheme).into_owned(),
        }
    }

    pub(super) fn insert_text(&mut self, text: &str) {
        let offset = self.byte_offset();
        self.text.insert_str(offset, text);
        let end = offset + text.len();
        self.cursor = self
            .text
            .grapheme_indices(true)
            .take_while(|(offset, _)| *offset < end)
            .count();
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        if let Some(history) = self.histories.get_mut(&self.history_key) {
            history.browsing = None;
        }
    }

    /// Step through history. Up lands with the cursor at the end (readline), Down
    /// at the start, so a multi-line entry is walked before history continues.
    /// Past the newest entry the saved draft and cursor return.
    fn recall(&mut self, newer: bool) {
        let history = self.histories.entry(self.history_key.clone()).or_default();
        let target = match (history.browsing, newer) {
            (None, true) => return,
            (None, false) => match history.entries.len().checked_sub(1) {
                Some(last) => {
                    history.draft = (std::mem::take(&mut self.text), self.cursor);
                    Some(last)
                }
                None => return,
            },
            (Some(0), false) => return,
            (Some(index), false) => Some(index - 1),
            (Some(index), true) => (index + 1 < history.entries.len()).then_some(index + 1),
        };
        history.browsing = target;
        match target {
            Some(index) => {
                self.text = history.entries[index].clone();
                self.cursor = if newer {
                    0
                } else {
                    self.text.graphemes(true).count()
                };
            }
            None => {
                (self.text, self.cursor) = std::mem::take(&mut history.draft);
            }
        }
    }

    fn insert(&mut self, character: char) {
        let offset = self.byte_offset();
        self.text.insert(offset, character);
        let end = offset + character.len_utf8();
        self.cursor = self
            .text
            .grapheme_indices(true)
            .take_while(|(offset, _)| *offset < end)
            .count();
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
        if self.cursor >= self.text.graphemes(true).count() {
            return;
        }
        let start = self.byte_offset();
        let end = self.text[start..]
            .grapheme_indices(true)
            .nth(1)
            .map_or(self.text.len(), |(offset, _)| start + offset);
        self.text.replace_range(start..end, "");
    }

    fn byte_offset(&self) -> usize {
        self.byte_of(self.cursor)
    }

    fn byte_of(&self, cursor: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .nth(cursor)
            .map_or(self.text.len(), |(offset, _)| offset)
    }

    fn is_break(grapheme: &str) -> bool {
        matches!(grapheme, "\n" | "\r\n" | "\r")
    }

    /// Grapheme indices of the start and end (before the break) of the
    /// cursor's line. Pasted `\n`, `\r\n` and `\r` all separate lines.
    fn line_bounds(&self) -> (usize, usize) {
        let mut start = 0;
        for (i, g) in self.text.graphemes(true).enumerate() {
            if Self::is_break(g) {
                if i >= self.cursor {
                    return (start, i);
                }
                start = i + 1;
            }
        }
        (start, self.text.graphemes(true).count())
    }

    /// Move one line up or down, keeping the column clamped to the target
    /// line. On the first (last) line the cursor stays put.
    fn move_line(&mut self, down: bool) {
        let (start, end) = self.line_bounds();
        let column = self.cursor - start;
        let total = self.text.graphemes(true).count();
        let (target_start, target_end) = if down {
            if end >= total {
                return;
            }
            let next = end + 1;
            let mut stop = total;
            for (i, g) in self.text.graphemes(true).enumerate().skip(next) {
                if Self::is_break(g) {
                    stop = i;
                    break;
                }
            }
            (next, stop)
        } else {
            if start == 0 {
                return;
            }
            let prev_end = start - 1;
            let prev_start = self
                .text
                .graphemes(true)
                .take(prev_end)
                .enumerate()
                .filter(|(_, g)| Self::is_break(g))
                .last()
                .map_or(0, |(i, _)| i + 1);
            (prev_start, prev_end)
        };
        self.cursor = (target_start + column).min(target_end);
    }
}

#[cfg(test)]
mod tests {
    use super::{ComposerAction, LineComposer};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn history_composer(entries: &[&str]) -> LineComposer {
        let mut composer = LineComposer::default();
        for entry in entries {
            composer.restore_draft(entry);
            press(&mut composer, KeyCode::Enter, KeyModifiers::NONE);
        }
        composer
    }

    #[test]
    fn up_recalls_history_and_down_restores_the_saved_draft() {
        let mut composer = history_composer(&["one", "two"]);
        composer.restore_draft("dra");
        press(&mut composer, KeyCode::Left, KeyModifiers::NONE);
        press(&mut composer, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!((composer.text(), composer.cursor()), ("two", 3));
        press(&mut composer, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(composer.text(), "one");
        press(&mut composer, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(composer.text(), "two");
        press(&mut composer, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!((composer.text(), composer.cursor()), ("dra", 2));
    }

    #[test]
    fn history_boundaries_are_no_ops() {
        let mut composer = LineComposer::default();
        composer.restore_draft("abc");
        press(&mut composer, KeyCode::Up, KeyModifiers::NONE);
        press(&mut composer, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!((composer.text(), composer.cursor()), ("abc", 3));
        let mut composer = history_composer(&["one", "two"]);
        for _ in 0..5 {
            press(&mut composer, KeyCode::Up, KeyModifiers::NONE);
        }
        assert_eq!(composer.text(), "one");
        for _ in 0..5 {
            press(&mut composer, KeyCode::Down, KeyModifiers::NONE);
        }
        assert_eq!(composer.text(), "");
    }

    #[test]
    fn multi_line_recall_moves_within_the_entry_before_continuing() {
        let mut composer = history_composer(&["old", "a\nbc\nd"]);
        press(&mut composer, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(
            composer.cursor(),
            6,
            "Up recall ends at the end of the text"
        );
        press(&mut composer, KeyCode::Up, KeyModifiers::NONE);
        press(&mut composer, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(composer.text(), "a\nbc\nd", "still inside the entry");
        assert_eq!(composer.cursor(), 1);
        press(&mut composer, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(
            composer.text(),
            "old",
            "first line: Up continues to history"
        );
        press(&mut composer, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(composer.text(), "a\nbc\nd");
        assert_eq!(composer.cursor(), 0, "Down recall starts at the beginning");
        press(&mut composer, KeyCode::Down, KeyModifiers::NONE);
        press(&mut composer, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(composer.text(), "a\nbc\nd");
        press(&mut composer, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(composer.text(), "", "last line: Down returns to the draft");
    }

    #[test]
    fn empty_and_repeated_submissions_are_not_recorded() {
        let mut composer = history_composer(&["", "  ", "a", "a", "b", "a"]);
        for _ in 0..3 {
            press(&mut composer, KeyCode::Up, KeyModifiers::NONE);
        }
        assert_eq!(composer.text(), "a", "entries are a, b, a");
        press(&mut composer, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(composer.text(), "a", "oldest entry; nothing older");
    }

    #[test]
    fn history_is_capped_and_kept_per_target() {
        let mut composer = LineComposer::default();
        for i in 0..super::HISTORY_LIMIT + 10 {
            composer.restore_draft(&i.to_string());
            press(&mut composer, KeyCode::Enter, KeyModifiers::NONE);
        }
        for _ in 0..super::HISTORY_LIMIT + 10 {
            press(&mut composer, KeyCode::Up, KeyModifiers::NONE);
        }
        assert_eq!(composer.text(), "10", "oldest entries were dropped");
        composer.clear();
        composer.set_history_target("other");
        press(&mut composer, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(composer.text(), "", "another session has its own history");
        composer.set_history_target("");
        press(&mut composer, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(composer.text(), (super::HISTORY_LIMIT + 9).to_string());
    }

    #[test]
    fn emacs_keys_and_grapheme_navigation_edit_the_draft() {
        let keymap = crate::keybindings::test_snapshot("");
        let mut composer = LineComposer::default();
        composer.restore_draft("a한e\u{301}👩‍💻z");
        composer.handle_key(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &keymap,
        );
        assert_eq!(composer.cursor(), 0);
        for _ in 0..3 {
            composer.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &keymap);
        }
        assert_eq!(composer.cursor(), 3);
        composer.handle_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            &keymap,
        );
        assert_eq!(composer.text(), "a한e\u{301}");
        composer.handle_key(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            &keymap,
        );
        assert_eq!(composer.text(), "a한");
        composer.handle_key(
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
            &keymap,
        );
        assert_eq!(composer.cursor(), 2);
    }

    fn press(composer: &mut LineComposer, code: KeyCode, modifiers: KeyModifiers) {
        let keymap = crate::keybindings::test_snapshot("");
        composer.handle_key(KeyEvent::new(code, modifiers), &keymap);
    }

    #[test]
    fn ctrl_f_and_ctrl_b_move_by_grapheme_and_stop_at_the_ends() {
        let mut composer = LineComposer::default();
        composer.restore_draft("a한e\u{301}👩‍💻");
        for _ in 0..3 {
            press(&mut composer, KeyCode::Char('b'), KeyModifiers::CONTROL);
        }
        assert_eq!(composer.cursor(), 1);
        for _ in 0..10 {
            press(&mut composer, KeyCode::Char('b'), KeyModifiers::CONTROL);
        }
        assert_eq!(composer.cursor(), 0);
        press(&mut composer, KeyCode::Char('f'), KeyModifiers::CONTROL);
        press(&mut composer, KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert_eq!(composer.cursor(), 2);
        press(&mut composer, KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(composer.text(), "a한");
        for _ in 0..10 {
            press(&mut composer, KeyCode::Char('f'), KeyModifiers::CONTROL);
        }
        assert_eq!(composer.cursor(), 2);
    }

    #[test]
    fn up_and_down_move_between_lines_and_clamp_the_column() {
        let mut composer = LineComposer::default();
        composer.restore_draft("한글abc\nx\r\n가나다라");
        // Cursor at the end of the last line (column 4).
        press(&mut composer, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(composer.cursor(), 12, "last line: Down stays put");
        press(&mut composer, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(composer.cursor(), 7, "clamped to the one-char middle line");
        press(&mut composer, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(composer.cursor(), 1, "column follows from the middle line");
        press(&mut composer, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(composer.cursor(), 1, "first line: Up stays put");
        press(&mut composer, KeyCode::Down, KeyModifiers::NONE);
        press(&mut composer, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(composer.cursor(), 9);
    }

    #[test]
    fn single_line_up_and_down_do_not_move() {
        let mut composer = LineComposer::default();
        composer.restore_draft("abc");
        press(&mut composer, KeyCode::Left, KeyModifiers::NONE);
        press(&mut composer, KeyCode::Up, KeyModifiers::NONE);
        press(&mut composer, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(composer.cursor(), 2);
        assert_eq!(composer.text(), "abc");
    }

    #[test]
    fn ctrl_a_e_k_act_on_the_current_line() {
        let mut composer = LineComposer::default();
        composer.restore_draft("ab\ncd\nef");
        press(&mut composer, KeyCode::Up, KeyModifiers::NONE);
        press(&mut composer, KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(composer.cursor(), 3);
        press(&mut composer, KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert_eq!(composer.cursor(), 5);
        press(&mut composer, KeyCode::Char('a'), KeyModifiers::CONTROL);
        press(&mut composer, KeyCode::Char('f'), KeyModifiers::CONTROL);
        press(&mut composer, KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(composer.text(), "ab\nc\nef");
        press(&mut composer, KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(composer.text(), "ab\ncef");
    }

    #[test]
    fn viewport_wraps_and_scrolls_around_cursor_without_shifting_glyphs() {
        let keymap = crate::keybindings::test_snapshot("");
        let mut composer = LineComposer::default();
        composer.restore_draft(&"한글abc".repeat(20));
        let rows = composer.display_lines("$ ", 12, 3);
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().any(|row| row.contains("\x1b[7m \x1b[27m")));
        for row in &rows {
            let mut parser = vt100::Parser::new(1, 12, 0);
            parser.process(row.as_bytes());
            assert!(parser.screen().contents().chars().count() <= 12);
        }
        composer.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), &keymap);
        let first = composer.display_lines("$ ", 12, 3);
        let mut parser = vt100::Parser::new(3, 12, 0);
        parser.process(first.join("\r\n").as_bytes());
        assert!(parser.screen().cell(0, 2).unwrap().inverse());
        assert_eq!(parser.screen().cell(0, 2).unwrap().contents(), "한");
        composer.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &keymap);
        let next = composer.display_lines("$ ", 12, 3);
        let mut moved = vt100::Parser::new(3, 12, 0);
        moved.process(next.join("\r\n").as_bytes());
        assert_eq!(parser.screen().contents(), moved.screen().contents());
        assert!(moved.screen().cell(0, 4).unwrap().inverse());
    }

    #[test]
    fn pasted_line_breaks_and_tabs_are_visible_in_the_draft() {
        let mut composer = LineComposer::default();
        composer.insert_text("echo one\r\necho\t two");
        let rows = composer.display_lines("$ ", 80, 3);
        assert_eq!(rows, ["$ echo one␍↵echo⇥ two\x1b[7m \x1b[27m"]);
        assert_eq!(composer.text(), "echo one\r\necho\t two");
    }

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
