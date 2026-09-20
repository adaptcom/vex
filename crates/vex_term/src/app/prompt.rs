//! Session prompt history and command completion, independent of buffer size.

use super::{App, PromptKind};
use crate::{
    input::{Prompt, PromptStamp},
    picker::{label, paint_box},
    prompt::{Item, Job, Result as CompletionResult},
    screen::{Frame, Style},
};
use std::{collections::VecDeque, ops::Range, sync::Arc};
use vex_editor::background::Cancellation;

const MAX_HISTORY_ENTRIES: usize = 100;
const MAX_HISTORY_BYTES: usize = 256 * 1024;
const MAX_ENTRY_BYTES: usize = 64 * 1024;

#[derive(Default)]
pub(super) struct Completion {
    stamp: Option<PromptStamp>,
    cancellation: Option<Cancellation>,
    outgoing: Option<Job>,
    items: Vec<Item>,
    range: Range<usize>,
    selected: Option<usize>,
    pending_tab: Option<bool>,
    directory: bool,
    notice: String,
}

impl Drop for Completion {
    fn drop(&mut self) {
        if let Some(cancellation) = &self.cancellation {
            cancellation.cancel();
        }
    }
}

impl Completion {
    fn sync(&mut self, input: &Prompt) {
        if self.stamp == Some(input.stamp()) {
            return;
        }
        *self = Self::default();
        self.stamp = Some(input.stamp());
        if input.register_pending() || input.text().len() > crate::prompt::MAX_QUERY_BYTES {
            return;
        }
        let cancellation = Cancellation::default();
        self.cancellation = Some(cancellation.clone());
        self.outgoing = Some(Job {
            stamp: input.stamp(),
            text: input.text().into(),
            cursor: input.cursor(),
            cancellation,
        });
    }

    pub(super) fn recalculate(&mut self, input: &Prompt) {
        self.stamp = None;
        self.sync(input);
    }

    pub(super) fn directory_selected(&self, input: &Prompt) -> bool {
        self.stamp == Some(input.stamp())
            && self.directory
            && input.text().ends_with(std::path::MAIN_SEPARATOR)
    }

    pub(super) fn cycle(&mut self, input: &mut Prompt, backwards: bool) {
        self.sync(input);
        if self.cancellation.is_some() {
            self.pending_tab = Some(backwards);
            return;
        }
        if self.items.is_empty() {
            return;
        }
        let length = self.items.len();
        let index = if backwards {
            self.selected
                .map_or(length - 1, |i| (i + length - 1) % length)
        } else {
            self.selected.map_or(0, |i| (i + 1) % length)
        };
        let item = &self.items[index];
        input.complete(self.range.clone(), &item.text);
        self.range.end = self.range.start + item.text.len();
        self.stamp = Some(input.stamp());
        self.selected = Some(index);
        self.directory = item.directory;
        if length == 1 && self.directory_selected(input) {
            self.recalculate(input);
            self.directory = true;
        }
    }
}

impl App {
    pub(crate) fn take_prompt_completion_job(&mut self) -> Option<Job> {
        let prompt = self.prompt.as_mut()?;
        if !matches!(prompt.kind, PromptKind::Command) {
            return None;
        }
        prompt.completion.sync(&prompt.input);
        prompt.completion.outgoing.take()
    }

    pub(super) fn prompt_completion_waiting(&self) -> bool {
        self.prompt
            .as_ref()
            .is_some_and(|prompt| prompt.completion.pending_tab.is_some())
    }

    pub(crate) fn handle_prompt_completion(&mut self, result: CompletionResult) -> bool {
        let Some(prompt) = self.prompt.as_mut() else {
            return false;
        };
        if prompt.input.stamp() != result.stamp || prompt.completion.stamp != Some(result.stamp) {
            return false;
        }
        prompt.completion.items = result.items;
        prompt.completion.range = result.range;
        prompt.completion.notice = result.notice;
        prompt.completion.cancellation = None;
        if let Some(backwards) = prompt.completion.pending_tab.take() {
            prompt.completion.cycle(&mut prompt.input, backwards);
        }
        true
    }

    pub(super) fn paint_prompt_completion(&self, frame: &mut Frame) {
        let Some(prompt) = &self.prompt else {
            return;
        };
        let completion = &prompt.completion;
        if completion.stamp != Some(prompt.input.stamp()) || prompt.input.register_pending() {
            return;
        }
        let width = frame.width().min(96);
        let bottom = frame.height().saturating_sub(1);
        if width < 12 || bottom < 3 {
            return;
        }
        let notice = if completion.pending_tab.is_some() {
            "Loading…"
        } else {
            &completion.notice
        };
        if completion.items.is_empty() && notice.is_empty() {
            return;
        }
        let footer = usize::from(!notice.is_empty());
        let rows = completion
            .items
            .len()
            .min(8)
            .min(usize::from(bottom).saturating_sub(2 + footer));
        let top = bottom - (rows + footer + 2) as u16;
        paint_box(frame, 0, top, width, bottom, " Completions ");
        let start = completion
            .selected
            .unwrap_or(0)
            .saturating_sub(rows.saturating_sub(1));
        for (offset, item) in completion.items.iter().enumerate().skip(start).take(rows) {
            let row = top + 1 + (offset - start) as u16;
            let selected = completion.selected == Some(offset);
            let style = if selected {
                Style::Selection
            } else {
                Style::Text
            };
            for col in 1..width - 1 {
                frame.put(col, row, " ", style);
            }
            let label_width = (width / 2).min(36);
            label(
                frame,
                2,
                row,
                label_width.saturating_sub(2),
                &item.text,
                style,
            );
            label(
                frame,
                label_width + 1,
                row,
                width - label_width - 3,
                item.description,
                if selected { style } else { Style::Gutter },
            );
        }
        if footer != 0 {
            label(frame, 2, bottom - 2, width - 4, notice, Style::Gutter);
        }
    }
}

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

    fn command(text: &str) -> App {
        let mut app = App::from_document(Document::from("original\n"), (80, 24));
        let mut prompt = ActivePrompt::command();
        prompt.input.insert(text);
        app.prompt = Some(prompt);
        app
    }

    fn finish_completion(app: &mut App) {
        let result = app.take_prompt_completion_job().unwrap().run().unwrap();
        assert!(app.handle_prompt_completion(result));
    }

    #[test]
    fn completion_cycles_the_same_candidates_and_preserves_arguments_at_the_caret() {
        let mut app = command("wri a file.rs");
        for _ in 0..10 {
            app.handle_prompt_key(Key::Left);
        }
        app.handle_prompt_key(Key::Tab);
        assert!(app.input_waiting());
        finish_completion(&mut app);
        assert!(!app.input_waiting());
        assert_eq!(app.prompt.as_ref().unwrap().input.text(), "write a file.rs");
        app.handle_prompt_key(Key::Tab);
        assert_eq!(
            app.prompt.as_ref().unwrap().input.text(),
            "write-quit a file.rs"
        );
        app.handle_prompt_key(Key::BackTab);
        assert_eq!(app.prompt.as_ref().unwrap().input.text(), "write a file.rs");
        assert!(app.take_prompt_completion_job().is_none());
    }

    #[test]
    fn stale_results_cannot_change_edited_reopened_or_register_prompts() {
        let mut app = command("lang ru");
        let stale = app.take_prompt_completion_job().unwrap().run().unwrap();
        app.handle_prompt_key(Key::Char('b'));
        assert!(!app.handle_prompt_completion(stale));
        let stale = app.take_prompt_completion_job().unwrap().run().unwrap();
        app.handle_prompt_key(Key::Escape);
        app.prompt = Some(ActivePrompt::command());
        assert!(!app.handle_prompt_completion(stale));
        let pending = app.take_prompt_completion_job().unwrap();
        app.handle_prompt_key(Key::Ctrl('r'));
        assert!(app.take_prompt_completion_job().is_none());
        assert!(pending.cancellation.is_cancelled());
        app.handle_prompt_key(Key::Escape); // only cancels the register prefix
        app.handle_prompt_key(Key::Tab);
        let pending = app.take_prompt_completion_job().unwrap();
        assert!(app.input_waiting());
        app.handle_prompt_key(Key::Escape);
        assert!(pending.cancellation.is_cancelled());
        assert!(!app.input_waiting());
    }

    #[test]
    fn directory_completion_opens_literal_paths_and_preserves_unsaved_buffers() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("dir 🦀");
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("a file.txt");
        std::fs::write(&path, "opened\n").unwrap();
        let mut app = command(&format!("open {}/di", root.path().display()));
        let original = app.editor.document().id();
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("unsaved ").unwrap();
        app.editor.execute("normal_mode", 1).unwrap();
        app.handle_prompt_key(Key::Tab);
        finish_completion(&mut app); // the sole directory candidate expands
        assert!(
            app.prompt
                .as_ref()
                .unwrap()
                .input
                .text()
                .ends_with("dir 🦀/")
        );
        app.handle_prompt_key(Key::Enter); // browse, do not execute :open yet
        assert_eq!(app.editor.document().id(), original);
        app.handle_prompt_key(Key::Tab);
        finish_completion(&mut app);
        assert_eq!(
            app.prompt.as_ref().unwrap().input.text(),
            format!("open {}", path.display())
        );
        app.handle_prompt_key(Key::Enter);
        assert!(app.prompt.is_none());
        assert_eq!(app.editor.document().text().to_string(), "opened\n");
        app.open_buffer(original).unwrap();
        assert_eq!(
            app.editor.document().text().to_string(),
            "unsaved original\n"
        );
        assert!(app.is_dirty());
    }

    #[test]
    fn completion_box_is_grey_bold_clipped_and_keeps_the_global_prompt_cursor() {
        let mut app = command("wri");
        app.handle_prompt_key(Key::Tab);
        finish_completion(&mut app);
        for (width, height) in [(0, 0), (8, 2), (12, 4), (80, 24)] {
            let mut frame = Frame::default();
            frame.reset(width, height).unwrap();
            app.paint(&mut frame).unwrap();
            if width == 80 {
                assert_eq!(frame.cursor.unwrap().y, height - 1);
                assert!(frame.row_text(height - 1).starts_with(":write"));
                assert!((0..height - 1).any(|row| frame.row_text(row).contains("Completions")));
                assert!(
                    (0..height - 1).any(|row| frame.style_at(2, row) == Some(Style::Selection))
                );
                assert!(
                    (0..height - 1).any(|row| frame.style_at(3, row) == Some(Style::PopupTitle))
                );
            }
        }
    }

    #[test]
    fn long_prompts_do_not_copy_or_schedule_completion_jobs() {
        let mut app = command(&"x".repeat(crate::prompt::MAX_QUERY_BYTES + 1));
        app.handle_prompt_key(Key::Tab);
        assert!(app.take_prompt_completion_job().is_none());
        assert!(!app.input_waiting());
    }

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
