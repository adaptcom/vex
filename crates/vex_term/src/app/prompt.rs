//! Session prompt history and command completion, independent of buffer size.

use std::{collections::VecDeque, sync::Arc};

const MAX_HISTORY_ENTRIES: usize = 100;
const MAX_HISTORY_BYTES: usize = 256 * 1024;
const MAX_ENTRY_BYTES: usize = 64 * 1024;

/// A shared, bounded history, partitioned by the prompt's selected register.
/// Register fragments remain ordinary yank/search values rather than history.
#[derive(Default)]
pub(super) struct History {
    entries: VecDeque<(char, Arc<str>)>,
    bytes: usize,
}

impl History {
    pub(super) fn push(&mut self, register: char, text: &str) {
        if text.is_empty()
            || text.len() > MAX_ENTRY_BYTES
            || self
                .last(register)
                .is_some_and(|last| last.as_ref() == text)
        {
            return;
        }
        self.bytes += text.len();
        self.entries.push_back((register, Arc::from(text)));
        while self.entries.len() > MAX_HISTORY_ENTRIES || self.bytes > MAX_HISTORY_BYTES {
            self.bytes -= self.entries.pop_front().unwrap().1.len();
        }
    }

    pub(super) fn last(&self, register: char) -> Option<Arc<str>> {
        self.entries
            .iter()
            .rev()
            .find(|(name, _)| *name == register)
            .map(|(_, text)| text.clone())
    }

    pub(super) fn step(
        &self,
        register: char,
        position: &mut Option<usize>,
        backwards: bool,
    ) -> Option<Arc<str>> {
        let mut entries = self.entries.iter().filter(|(name, _)| *name == register);
        let end = entries.clone().count().checked_sub(1)?;
        let index = if backwards {
            position.map_or(end, |i| i.saturating_sub(1))
        } else {
            position.map_or(0, |i| i.saturating_add(1)).min(end)
        };
        *position = Some(index);
        entries.nth(index).map(|(_, text)| text.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{ActivePrompt, App};
    use vex_core::Document;
    use vex_editor::Key;

    #[test]
    fn command_history_reexecutes_and_search_history_updates_equal_length_previews() {
        let mut app = App::from_document(Document::from("aa bb"), (80, 24));
        app.execute("language rust").unwrap();
        app.execute("language text").unwrap();
        app.prompt = Some(ActivePrompt::command());
        app.handle_prompt_key(Key::Up);
        app.handle_prompt_key(Key::Ctrl('p'));
        assert_eq!(app.prompt.as_ref().unwrap().input.text(), "language rust");
        app.handle_prompt_key(Key::Enter);
        assert_eq!(app.editor.language(), Some(vex_editor::Language::Rust));
        app.editor.set_language(None);
        app.prompt = Some(ActivePrompt::command());
        app.handle_prompt_key(Key::Enter);
        assert_eq!(app.editor.language(), Some(vex_editor::Language::Rust));
        for text in ["aa", "bb"] {
            app.editor.execute("search_forward", 1).unwrap();
            app.open_search_prompt();
            for ch in text.chars() {
                app.handle_prompt_key(Key::Char(ch));
            }
            app.handle_prompt_key(Key::Enter);
        }
        app.editor.execute("search_forward", 1).unwrap();
        app.open_search_prompt();
        app.handle_prompt_key(Key::Up);
        assert_eq!(app.editor.selections().primary().start().0, 3);
        app.handle_prompt_key(Key::Up);
        assert_eq!(app.editor.selections().primary().start().0, 0);
        app.handle_prompt_key(Key::Escape);
        assert!(app.prompt.is_none());
    }

    #[test]
    fn history_is_partitioned_clamped_deduplicated_and_bounded() {
        let mut history = History::default();
        history.push(':', "write");
        history.push('/', "needle");
        history.push(':', "help");
        history.push(':', "help");
        let mut position = None;
        for expected in ["help", "write", "write"] {
            assert_eq!(
                history.step(':', &mut position, true).as_deref(),
                Some(expected)
            );
        }
        assert_eq!(
            history.step(':', &mut position, false).as_deref(),
            Some("help")
        );
        assert_eq!(history.last('/').as_deref(), Some("needle"));
        for n in 0..200 {
            history.push(':', &n.to_string());
        }
        assert_eq!(history.entries.len(), MAX_HISTORY_ENTRIES);
        for n in 0..10 {
            history.push(':', &format!("{n}{}", "x".repeat(MAX_ENTRY_BYTES - 1)));
        }
        assert!(history.bytes <= MAX_HISTORY_BYTES);
        assert_eq!(history.entries.len(), 4);
        history.push(':', &"x".repeat(MAX_ENTRY_BYTES + 1));
        assert_eq!(history.entries.len(), 4);
    }
}
