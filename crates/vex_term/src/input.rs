//! Translate terminal events without coupling editing commands to Crossterm.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use unicode_segmentation::{GraphemeCursor, UnicodeSegmentation};
use vex_editor::{Key, Modifier, NamedKey};

pub fn key(event: KeyEvent) -> Option<Key> {
    if event.kind == KeyEventKind::Release {
        return None;
    }
    if event
        .modifiers
        .intersects(KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META)
    {
        return None;
    }
    let control = event.modifiers.contains(KeyModifiers::CONTROL);
    let alt = event.modifiers.contains(KeyModifiers::ALT);
    if control && alt {
        return None;
    }
    if control || alt {
        if let KeyCode::Char(ch) = event.code {
            return Some(if control {
                Key::Ctrl(ch.to_ascii_lowercase())
            } else {
                Key::Alt(ch)
            });
        }
        let code = match event.code {
            KeyCode::Esc => NamedKey::Escape,
            KeyCode::Enter => NamedKey::Enter,
            KeyCode::Tab | KeyCode::BackTab => NamedKey::Tab,
            KeyCode::PageUp => NamedKey::PageUp,
            KeyCode::PageDown => NamedKey::PageDown,
            KeyCode::Backspace => NamedKey::Backspace,
            KeyCode::Delete => NamedKey::Delete,
            KeyCode::Left => NamedKey::Left,
            KeyCode::Right => NamedKey::Right,
            KeyCode::Up => NamedKey::Up,
            KeyCode::Down => NamedKey::Down,
            KeyCode::Home => NamedKey::Home,
            KeyCode::End => NamedKey::End,
            _ => return None,
        };
        let modifier = if control {
            Modifier::Control
        } else if event.modifiers.contains(KeyModifiers::SHIFT) {
            Modifier::AltShift
        } else {
            Modifier::Alt
        };
        return Some(Key::Modified(modifier, code));
    }
    Some(match event.code {
        KeyCode::Char(ch) => Key::Char(ch),
        KeyCode::Esc => Key::Escape,
        KeyCode::Enter => Key::Enter,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
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

/// Editable prompt text with a UTF-8 byte cursor on grapheme boundaries.
#[derive(Debug)]
pub struct Prompt {
    text: String,
    cursor: usize,
    register_pending: bool,
    stamp: PromptStamp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PromptStamp {
    identity: u64,
    revision: u64,
}

impl Default for Prompt {
    fn default() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self {
            text: String::new(),
            cursor: 0,
            register_pending: false,
            stamp: PromptStamp {
                identity: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                revision: 0,
            },
        }
    }
}

impl Prompt {
    pub(crate) fn stamp(&self) -> PromptStamp {
        self.stamp
    }
    pub fn register_pending(&self) -> bool {
        self.register_pending
    }

    /// Register input is a prefix: Escape or a non-character cancels only the
    /// prefix. The frontend resolves text and performs any platform I/O.
    pub fn register_key(&mut self, key: Key) -> Option<Option<char>> {
        if self.register_pending {
            self.stamp.revision += 1;
            self.register_pending = false;
            return Some(match key {
                Key::Char(ch) if !ch.is_control() => Some(ch),
                _ => None,
            });
        }
        if key == Key::Ctrl('r') {
            self.stamp.revision += 1;
            self.register_pending = true;
            return Some(None);
        }
        None
    }

    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn cursor(&self) -> usize {
        self.cursor
    }
    /// Replace prompt input, leaving the cursor at the end and invalidating reads.
    pub(crate) fn replace(&mut self, text: &str) {
        self.text.clear();
        self.cursor = 0;
        self.insert(text);
    }
    /// Apply a validated completion to this exact prompt revision.
    pub(crate) fn complete(&mut self, range: std::ops::Range<usize>, text: &str) {
        self.stamp.revision += 1;
        self.cursor = range.start + text.len();
        self.text.replace_range(range, text);
        self.snap_cursor();
    }
    pub fn insert(&mut self, text: &str) {
        self.stamp.revision += 1;
        self.register_pending = false;
        // Pasting into a prompt never submits a command or introduces new lines.
        let text: String = text.chars().filter(|ch| !ch.is_control()).collect();
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
        self.snap_cursor();
    }
    fn snap_cursor(&mut self) {
        let mut cursor = GraphemeCursor::new(self.cursor, self.text.len(), true);
        if !cursor.is_boundary(&self.text, 0).expect("complete prompt") {
            self.cursor = cursor
                .next_boundary(&self.text, 0)
                .expect("complete prompt")
                .unwrap_or(self.text.len());
        }
    }
    /// Apply Helix-inspired prompt editing; return whether the text changed.
    pub fn handle(&mut self, key: Key) -> bool {
        self.stamp.revision += 1;
        let before = self.text.len();
        match key {
            Key::Char(ch) if !ch.is_control() => self.insert(ch.encode_utf8(&mut [0; 4])),
            Key::Left | Key::Ctrl('b') => self.cursor = self.previous(),
            Key::Right | Key::Ctrl('f') => self.cursor = self.next(),
            Key::Home | Key::Ctrl('a') => self.cursor = 0,
            Key::End | Key::Ctrl('e') => self.cursor = self.text.len(),
            Key::Alt('b') | Key::Modified(Modifier::Control, NamedKey::Left) => {
                self.cursor = self.word_start()
            }
            Key::Alt('f') | Key::Modified(Modifier::Control, NamedKey::Right) => {
                self.cursor = self.next_word()
            }
            Key::Backspace | Key::Ctrl('h') => self.delete(self.previous()..self.cursor),
            Key::Delete | Key::Ctrl('d') => self.delete(self.cursor..self.next()),
            Key::Ctrl('w')
            | Key::Modified(Modifier::Alt | Modifier::Control, NamedKey::Backspace) => {
                self.delete(self.word_start()..self.cursor)
            }
            Key::Alt('d') | Key::Modified(Modifier::Alt | Modifier::Control, NamedKey::Delete) => {
                self.delete(self.cursor..self.next_word())
            }
            Key::Ctrl('u') => self.delete(0..self.cursor),
            Key::Ctrl('k') => self.delete(self.cursor..self.text.len()),
            _ => {}
        }
        // Removing a separator can join the surrounding regional indicators or
        // emoji into a new cluster. Keep the caret at a whole-cluster boundary.
        self.snap_cursor();
        before != self.text.len()
    }
    fn previous(&self) -> usize {
        GraphemeCursor::new(self.cursor, self.text.len(), true)
            .prev_boundary(&self.text, 0)
            .expect("complete prompt")
            .unwrap_or(0)
    }
    fn next(&self) -> usize {
        GraphemeCursor::new(self.cursor, self.text.len(), true)
            .next_boundary(&self.text, 0)
            .expect("complete prompt")
            .unwrap_or(self.text.len())
    }
    fn delete(&mut self, range: std::ops::Range<usize>) {
        self.cursor = range.start;
        self.text.replace_range(range, "");
    }
    fn word_start(&self) -> usize {
        self.text[..self.previous()]
            .grapheme_indices(true)
            .rev()
            .find(|(_, g)| word_separator(g.chars().next().unwrap()))
            .map_or(0, |(i, g)| i + g.len())
    }
    fn next_word(&self) -> usize {
        let mut chars = self.text[self.cursor..].grapheme_indices(true).peekable();
        while chars
            .peek()
            .is_some_and(|(_, g)| !word_separator(g.chars().next().unwrap()))
        {
            chars.next();
        }
        while chars
            .peek()
            .is_some_and(|(_, g)| word_separator(g.chars().next().unwrap()))
        {
            chars.next();
        }
        chars
            .next()
            .map_or(self.text.len(), |(i, _)| self.cursor + i)
    }
}

fn word_separator(ch: char) -> bool {
    ch.is_whitespace() || ch == std::path::MAIN_SEPARATOR
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use unicode_segmentation::UnicodeSegmentation;
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
            Some(Key::Alt('x'))
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
    fn modified_keys_preserve_case_and_never_insert_as_plain_text() {
        for (code, modifiers, expected) in [
            (
                KeyCode::Char('B'),
                KeyModifiers::ALT | KeyModifiers::SHIFT,
                Key::Alt('B'),
            ),
            (
                KeyCode::Left,
                KeyModifiers::CONTROL,
                Key::Modified(Modifier::Control, NamedKey::Left),
            ),
            (
                KeyCode::Delete,
                KeyModifiers::ALT,
                Key::Modified(Modifier::Alt, NamedKey::Delete),
            ),
            (
                KeyCode::Down,
                KeyModifiers::ALT | KeyModifiers::SHIFT,
                Key::Modified(Modifier::AltShift, NamedKey::Down),
            ),
        ] {
            assert_eq!(key(KeyEvent::new(code, modifiers)), Some(expected));
        }
        for modifiers in [
            KeyModifiers::SUPER,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ] {
            assert_eq!(key(KeyEvent::new(KeyCode::Char('x'), modifiers)), None);
        }
    }

    #[test]
    fn prompt_words_use_paths_and_spaces_and_kills_preserve_whole_clusters() {
        let mut prompt = Prompt::default();
        prompt.insert("edit src/été.rs next");
        prompt.handle(Key::Alt('b'));
        assert_eq!(&prompt.text()[prompt.cursor()..], "next");
        prompt.handle(Key::Ctrl('w'));
        assert_eq!(prompt.text(), "edit src/next");
        prompt.handle(Key::Ctrl('a'));
        prompt.handle(Key::Alt('f'));
        assert_eq!(&prompt.text()[prompt.cursor()..], "src/next");
        prompt.handle(Key::Alt('d'));
        assert_eq!(prompt.text(), "edit next");
        prompt.handle(Key::Ctrl('k'));
        assert_eq!(prompt.text(), "edit ");
        prompt.handle(Key::Ctrl('u'));
        assert_eq!(prompt.text(), "");
        prompt.insert("a \u{301}e\u{301}👩\u{200d}💻");
        prompt.handle(Key::Ctrl('w'));
        assert_eq!(prompt.text(), "a \u{301}");
        prompt.handle(Key::Ctrl('b'));
        prompt.handle(Key::Ctrl('d'));
        assert_eq!(prompt.text(), "a");
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
    fn backspace_and_its_alias_edit_prompts_consistently() {
        for event in [
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL),
        ] {
            let mut prompt = Prompt::default();
            prompt.insert("a👩\u{200d}💻");
            prompt.handle(key(event).unwrap());
            assert_eq!(prompt.text(), "a");
        }
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
        fn arbitrary_prompt_edits_preserve_grapheme_boundaries(steps in prop::collection::vec(0u8..22, 0..150)) {
            let mut prompt = Prompt::default();
            for step in steps {
                match step {
                    0 => prompt.handle(Key::Left),
                    1 => prompt.handle(Key::Right),
                    2 => prompt.handle(Key::Home),
                    3 => prompt.handle(Key::End),
                    4 => prompt.handle(Key::Backspace),
                    5 => prompt.handle(Key::Delete),
                    6 => prompt.handle(Key::Alt('b')),
                    7 => prompt.handle(Key::Alt('f')),
                    8 => prompt.handle(Key::Ctrl('w')),
                    9 => prompt.handle(Key::Alt('d')),
                    10 => prompt.handle(Key::Ctrl('u')),
                    11 => prompt.handle(Key::Ctrl('k')),
                    n => { prompt.insert(["x", "🇦", "🇧", "\u{301}", "👩", "\u{200d}", "💻", "界", "\r\n\x1b", " "][usize::from(n - 12)]); true },
                };
                prop_assert!(prompt.cursor() == prompt.text().len() || prompt.text().grapheme_indices(true).any(|(i, _)| i == prompt.cursor()));
                prop_assert!(!prompt.text().chars().any(char::is_control));
            }
        }
    }
}
