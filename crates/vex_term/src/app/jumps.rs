//! Bounded per-pane navigation history, independent of language services.
//! Checkpoints retain selections and document identities, never document text.

use super::App;
use std::{collections::VecDeque, io, sync::Arc};
use vex_core::{CharOffset, DocumentId, Selection, SelectionSet};
use vex_editor::Mode;

const CAPACITY: usize = 32;

#[derive(Clone, Debug)]
pub(super) struct Jump {
    document: DocumentId,
    selections: Arc<SelectionSet>,
}

impl Jump {
    pub fn new(document: DocumentId, selections: &SelectionSet) -> Self {
        Self {
            document,
            selections: Arc::new(selections.clone()),
        }
    }

    fn matches(&self, document: DocumentId, selections: &SelectionSet) -> bool {
        self.document == document && self.selections.as_ref() == selections
    }
}

#[derive(Clone, Debug)]
pub(super) struct History {
    entries: VecDeque<Jump>,
    // entries.len() means the live view is newer than the recorded history.
    cursor: usize,
}

impl History {
    pub fn new(initial: Jump) -> Self {
        Self {
            entries: VecDeque::from([initial]),
            cursor: 0,
        }
    }

    fn push(&mut self, jump: Jump) -> usize {
        self.entries.truncate(self.cursor);
        let mut removed = 0;
        if !self
            .entries
            .back()
            .is_some_and(|last| last.matches(jump.document, &jump.selections))
        {
            if self.entries.len() == CAPACITY {
                self.entries.pop_front();
                removed = 1;
            }
            self.entries.push_back(jump);
        }
        self.cursor = self.entries.len();
        removed
    }

    fn backward(
        &mut self,
        document: DocumentId,
        selections: &SelectionSet,
        count: usize,
    ) -> Option<&Jump> {
        let mut target = self.cursor.checked_sub(count)?;
        if self.cursor == self.entries.len() {
            let removed = self.push(Jump::new(document, selections));
            target = target.saturating_sub(removed);
        }
        if self.entries.get(target)?.matches(document, selections) {
            target = target.checked_sub(1)?;
        }
        self.cursor = target;
        self.entries.get(target)
    }

    fn forward(&mut self, count: usize) -> Option<&Jump> {
        let target = self.cursor.checked_add(count)?;
        if target >= self.entries.len() {
            return None;
        }
        self.cursor = target;
        self.entries.get(target)
    }

    pub fn remove(&mut self, document: DocumentId) {
        let before = self
            .entries
            .iter()
            .take(self.cursor)
            .filter(|jump| jump.document == document)
            .count();
        self.entries.retain(|jump| jump.document != document);
        self.cursor = self.cursor.saturating_sub(before).min(self.entries.len());
    }
}

impl App {
    pub(super) fn current_jump(&self) -> Jump {
        Jump::new(self.editor.document().id(), self.editor.selections())
    }

    pub(super) fn push_jump(&mut self, jump: Jump) {
        self.jump_history_mut().push(jump);
    }

    pub(super) fn record_jump(&mut self) {
        self.push_jump(self.current_jump());
    }

    pub(super) fn navigate_jump(&mut self, forward: bool, count: usize) -> io::Result<()> {
        // Copy at most CAPACITY Arc handles. Commit the new history position
        // only after navigation succeeds; failed requests keep their return path.
        let mut history = self.jump_history().clone();
        let destination = if forward {
            history.forward(count)
        } else {
            history.backward(self.editor.document().id(), self.editor.selections(), count)
        }
        .cloned();
        let Some(jump) = destination else {
            return Ok(());
        };
        let mode = self.editor.mode();
        self.open_buffer(jump.document)?;
        self.editor
            .execute("normal_mode", 1)
            .map_err(io::Error::other)?;
        if mode == Mode::Select {
            self.editor
                .execute("select_mode", 1)
                .map_err(io::Error::other)?;
        }
        let length = self.editor.document().text().len_chars();
        let selections = if jump
            .selections
            .ranges()
            .iter()
            .all(|selection| selection.end().0 <= length)
        {
            jump.selections.as_ref().clone()
        } else {
            SelectionSet::new(
                jump.selections
                    .ranges()
                    .iter()
                    .map(|selection| {
                        Selection::new(
                            CharOffset(selection.anchor.0.min(length)),
                            CharOffset(selection.head.0.min(length)),
                        )
                    })
                    .collect(),
                jump.selections.primary_index(),
            )
            .map_err(io::Error::other)?
        };
        self.editor
            .set_selections(selections)
            .map_err(io::Error::other)?;
        *self.jump_history_mut() = history;
        self.clear_message();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use vex_core::Document;

    fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
        app.handle(Event::Key(KeyEvent::new(code, modifiers)));
    }
    fn at(app: &mut App, position: usize) {
        app.editor
            .set_selections(SelectionSet::single(Selection::new(
                CharOffset(position),
                CharOffset(position + 1),
            )))
            .unwrap();
    }
    fn cursor(app: &App) -> usize {
        app.editor.selections().primary().start().0
    }

    #[test]
    fn backward_forward_counts_duplicates_and_branching_follow_live_selection() {
        let mut app = App::from_document(Document::from("abcdefghijk"), (80, 24));
        for position in [0, 3, 6] {
            at(&mut app, position);
            app.execute("save_selection").unwrap();
        }
        // The most recent saved selection is also the live position: skip it.
        press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_eq!(cursor(&app), 3);
        press(&mut app, KeyCode::Char('i'), KeyModifiers::CONTROL);
        assert_eq!(cursor(&app), 6);
        app.execute("jump_backward").unwrap();
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(cursor(&app), 6);
        press(&mut app, KeyCode::Char('2'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_eq!(cursor(&app), 0);
        press(&mut app, KeyCode::Char('2'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('i'), KeyModifiers::CONTROL);
        assert_eq!(cursor(&app), 6);
        app.navigate_jump(false, usize::MAX).unwrap();
        app.navigate_jump(true, usize::MAX).unwrap();
        assert_eq!(cursor(&app), 6);
        app.execute("jump_backward").unwrap();
        at(&mut app, 4);
        app.execute("save_selection").unwrap();
        at(&mut app, 9);
        app.execute("jump_forward").unwrap(); // Old forward branch was discarded.
        assert_eq!(cursor(&app), 9);
        app.execute("jump_backward").unwrap();
        assert_eq!(cursor(&app), 4);
        app.execute("jump_forward").unwrap();
        assert_eq!(cursor(&app), 9);
    }

    #[test]
    fn jumps_restore_direction_primary_and_select_mode_across_hidden_scratch_and_files() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("target.txt");
        std::fs::write(&path, "target text").unwrap();
        let mut app = App::from_document(Document::from("abcdefghi"), (80, 24));
        let initial = app.editor.document().id();
        let selections = SelectionSet::new(
            vec![
                Selection::new(CharOffset(3), CharOffset(1)),
                Selection::new(CharOffset(5), CharOffset(8)),
            ],
            1,
        )
        .unwrap();
        app.editor.set_selections(selections.clone()).unwrap();
        app.open_window_from_picker(&path).unwrap();
        let target = app.editor.document().id();
        at(&mut app, 4);
        app.execute("select_mode").unwrap();
        app.execute("jump_backward").unwrap();
        assert_eq!(app.editor.document().id(), initial);
        assert_eq!(app.editor.mode(), Mode::Select);
        assert_eq!(app.editor.selections(), &selections);
        app.execute("jump_forward").unwrap();
        assert_eq!(app.editor.document().id(), target);
        assert_eq!(cursor(&app), 4);
        assert_eq!(app.editor.mode(), Mode::Select);
        // Tab retains its editing meaning in insert mode.
        app.execute("insert_mode").unwrap();
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert!(app.editor.document().text().to_string().contains('\t'));
    }

    #[test]
    fn each_pane_has_its_own_history_and_closed_buffers_leave_no_dangling_entries() {
        let mut app = App::from_document(Document::from("abcdefghij"), (100, 24));
        app.execute("save_selection").unwrap();
        at(&mut app, 3);
        app.execute("vsplit").unwrap();
        app.execute("jump_backward").unwrap();
        assert_eq!(cursor(&app), 3); // New pane cannot consume the other's history.
        app.execute("save_selection").unwrap();
        at(&mut app, 6);
        app.execute("jump_backward").unwrap();
        assert_eq!(cursor(&app), 3);
        app.execute("jump_view_left").unwrap();
        app.execute("jump_backward").unwrap();
        assert_eq!(cursor(&app), 0);
        app.execute("jump_view_right").unwrap();
        app.execute("jump_forward").unwrap();
        assert_eq!(cursor(&app), 6);

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("target.txt");
        std::fs::write(&path, "target").unwrap();
        let original = app.editor.document().id();
        app.open_window_from_picker(&path).unwrap();
        let target = app.editor.document().id();
        app.execute("jump_backward").unwrap();
        app.execute("bc").unwrap();
        assert_eq!(app.editor.document().id(), target);
        assert!(
            app.jump_history()
                .entries
                .iter()
                .all(|jump| jump.document != original)
        );
        app.execute("jump_forward").unwrap();
        assert_eq!(app.editor.document().id(), target);
    }

    #[test]
    fn history_capacity_shares_selection_storage_and_failed_destinations_preserve_history() {
        let mut app = App::from_document(Document::from("x".repeat(100).as_str()), (80, 24));
        for position in 0..80 {
            at(&mut app, position);
            app.record_jump();
        }
        assert_eq!(app.jump_history().entries.len(), CAPACITY);
        at(&mut app, 90);
        app.navigate_jump(false, CAPACITY).unwrap();
        assert_eq!(cursor(&app), 49); // Saving the live return location evicts 48.
        app.navigate_jump(true, CAPACITY - 1).unwrap();
        assert_eq!(cursor(&app), 90);
        let history = app.jump_history().clone();
        assert!(Arc::ptr_eq(
            &history.entries[0].selections,
            &app.jump_history().entries[0].selections
        ));
        let missing = Document::from("missing");
        app.push_jump(Jump::new(
            missing.id(),
            &SelectionSet::single(Selection::cursor(CharOffset(0))),
        ));
        let position = app.jump_history().cursor;
        let length = app.jump_history().entries.len();
        assert!(app.navigate_jump(false, 1).is_err());
        assert_eq!(app.jump_history().cursor, position);
        assert_eq!(app.jump_history().entries.len(), length);
        assert_eq!(cursor(&app), 90);
    }
}
