//! Translate terminal events without coupling editing commands to Crossterm.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;
use vex_editor::Key;

pub fn key(event: KeyEvent) -> Option<Key> {
    if event.kind == KeyEventKind::Release {
        return None;
    }
    if event.modifiers.intersects(
        KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META,
    ) {
        return None;
    }
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        return match event.code {
            KeyCode::Char(ch) => Some(Key::Ctrl(ch.to_ascii_lowercase())),
            _ => None,
        };
    }
    Some(match event.code {
        KeyCode::Char(ch) => Key::Char(ch),
        KeyCode::Esc => Key::Escape,
        KeyCode::Enter => Key::Enter,
        KeyCode::Tab => Key::Tab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        _ => return None,
    })
}

/// An editable command prompt with a UTF-8 byte cursor on grapheme boundaries.
#[derive(Debug, Default)]
pub struct Prompt {
    text: String,
    cursor: usize,
}

impl Prompt {
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn cursor(&self) -> usize {
        self.cursor
    }
    pub fn insert(&mut self, text: &str) {
        // Pasting into a prompt never submits a command or introduces new lines.
        let text: String = text.chars().filter(|ch| !ch.is_control()).collect();
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
        self.snap_cursor();
    }
    fn snap_cursor(&mut self) {
        self.cursor = self
            .text
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .find(|&i| i >= self.cursor)
            .unwrap_or(self.text.len());
    }
    pub fn handle(&mut self, key: Key) {
        match key {
            Key::Char(ch) if !ch.is_control() => self.insert(ch.encode_utf8(&mut [0; 4])),
            Key::Left => self.cursor = self.previous(),
            Key::Right => self.cursor = self.next(),
            Key::Home => self.cursor = 0,
            Key::End => self.cursor = self.text.len(),
            Key::Backspace => {
                let previous = self.previous();
                self.text.replace_range(previous..self.cursor, "");
                self.cursor = previous;
            }
            Key::Delete => {
                let next = self.next();
                self.text.replace_range(self.cursor..next, "");
            }
            _ => {}
        }
        // Removing a separator can join the surrounding regional indicators or
        // emoji into a new cluster. Keep the caret at a whole-cluster boundary.
        self.snap_cursor();
    }
    fn previous(&self) -> usize {
        self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }
    fn next(&self) -> usize {
        self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(self.cursor, |g| self.cursor + g.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    #[test]
    fn modifiers_and_release_events_do_not_insert_unintended_text() {
        assert_eq!(
            key(KeyEvent::new_with_kind(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
                KeyEventKind::Release
            )),
            None
        );
        assert_eq!(
            key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT)),
            None
        );
        assert_eq!(
            key(KeyEvent::new(
                KeyCode::Char('R'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            Some(Key::Ctrl('r'))
        );
        assert_eq!(
            key(KeyEvent::new_with_kind(
                KeyCode::Left,
                KeyModifiers::NONE,
                KeyEventKind::Repeat
            )),
            Some(Key::Left)
        );
    }
    #[test]
    fn prompt_edits_whole_graphemes_and_paste_cannot_submit() {
        let mut prompt = Prompt::default();
        prompt.insert("w e\u{301}🦀");
        prompt.handle(Key::Left);
        prompt.handle(Key::Backspace);
        assert_eq!(prompt.text(), "w 🦀");
        prompt.insert("file\r\n:q!");
        assert_eq!(prompt.text(), "w file:q!🦀");
        prompt.handle(Key::Delete);
        assert_eq!(prompt.text(), "w file:q!");
    }

    #[test]
    fn removing_a_separator_can_join_surrounding_graphemes() {
        for key in [Key::Backspace, Key::Delete] {
            let mut prompt = Prompt::default();
            prompt.insert("🇦x🇧");
            prompt.handle(Key::Left);
            if key == Key::Delete {
                prompt.handle(Key::Left);
            }
            prompt.handle(key);
            assert_eq!(prompt.text(), "🇦🇧");
            assert_eq!(prompt.cursor(), prompt.text().len());
            prompt.handle(Key::Backspace);
            assert_eq!(prompt.text(), "");
        }
    }

    proptest! {
        #[test]
        fn arbitrary_prompt_edits_preserve_grapheme_boundaries(steps in prop::collection::vec(0u8..16, 0..150)) {
            let mut prompt = Prompt::default();
            for step in steps {
                match step {
                    0 => prompt.handle(Key::Left),
                    1 => prompt.handle(Key::Right),
                    2 => prompt.handle(Key::Home),
                    3 => prompt.handle(Key::End),
                    4 => prompt.handle(Key::Backspace),
                    5 => prompt.handle(Key::Delete),
                    n => prompt.insert(["x", "🇦", "🇧", "\u{301}", "👩", "\u{200d}", "💻", "界", "\r\n\x1b", " "][usize::from(n - 6)]),
                }
                prop_assert!(prompt.cursor() == prompt.text().len() || prompt.text().grapheme_indices(true).any(|(i, _)| i == prompt.cursor()));
                prop_assert!(!prompt.text().chars().any(char::is_control));
            }
        }
    }
}
