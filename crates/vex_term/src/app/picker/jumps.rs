//! Jump picker sessions retain stable checkpoint identities across query changes.

use super::*;
use crate::picker::jumps::{Job, Location, Result};

impl App {
    pub(in crate::app) fn open_jump_picker(&mut self) {
        self.close_picker();
        self.dismiss_language_help();
        self.keys.cancel(&mut self.editor);
        self.prompt = None;
        if self.editor.search_prompt().is_some() {
            let _ = self.editor.execute("search_cancel", 1);
        }
        let mut view = Picker::new("Jumps · * current buffer in source pane".into());
        view.noun = "jumps";
        self.picker.next_session += 1;
        self.picker.active = Some(Active {
            view,
            session: self.picker.next_session,
            revision: 0,
            source: Source::Jumps(self.jump_catalog()),
            cancellation: Cancellation::default(),
            preview_cancel: Cancellation::default(),
            preview_request: 0,
            preview_target: None,
            accept_pending: false,
        });
        self.submit_picker_query();
    }

    pub(crate) fn take_jump_job(&mut self) -> Option<Job> {
        self.picker.jump_job.take()
    }

    pub(crate) fn handle_jump_result(&mut self, result: Result) -> bool {
        let Some(active) = &mut self.picker.active else {
            return false;
        };
        if !matches!(active.source, Source::Jumps(_))
            || active.session != result.session
            || active.revision != result.revision
            || active.cancellation.is_cancelled()
        {
            return false;
        }
        active.view.replace(
            result
                .items
                .into_iter()
                .map(|item| Item {
                    entry: Arc::new(Entry {
                        label: item.entry.label.clone(),
                        value: Target::Jump(item.entry.value.clone()),
                    }),
                    matched: item.matched,
                })
                .collect(),
        );
        active.view.matched = result.matched;
        active.view.total = result.total;
        active.view.notice = result.notice;
        active.view.pending = false;
        // Reopened rows keep their checkpoint identity while their mapped
        // line/revision changes. Refresh even if the selected identity matches
        // the provisional preview requested from the cached rows.
        active.preview_target = None;
        if active.accept_pending {
            active.accept_pending = false;
            self.accept_picker();
        } else {
            self.request_picker_preview();
        }
        true
    }

    pub(super) fn refresh_jump_picker(&mut self, document: vex_core::DocumentId) -> bool {
        let Some(active) = &self.picker.active else {
            return false;
        };
        let Source::Jumps(catalog) = &active.source else {
            return false;
        };
        if !catalog
            .captures
            .iter()
            .any(|capture| capture.document.snapshot.id() == document)
        {
            return false;
        }
        let catalog = self.jump_catalog();
        let active = self.picker.active.as_mut().unwrap();
        active.source = Source::Jumps(catalog);
        active.view.resume();
        active.view.pending = true;
        // Keep early Enter waiting through an external reload's replacement query.
        let accept = active.accept_pending;
        self.submit_picker_query();
        self.picker.active.as_mut().unwrap().accept_pending = accept;
        true
    }

    pub(super) fn accept_jump_location(&mut self, location: Location) -> io::Result<()> {
        let snapshot = self
            .snapshot_for_buffer(location.document)
            .ok_or_else(|| io::Error::other("jump buffer is no longer open"))?;
        if snapshot.revision() != location.revision {
            self.refresh_jump_picker(location.document);
            return Err(io::Error::other(
                "buffer changed since this result; refreshing jumps",
            ));
        }
        let origin = self.current_jump();
        self.restore_jump(location.document, &location.selections)?;
        self.push_jump(origin);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use vex_core::{CharOffset, Document, Selection, SelectionSet};
    use vex_editor::Mode;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }
    fn press(app: &mut App, keys: &str) {
        for ch in keys.chars() {
            app.handle(key(KeyCode::Char(ch)));
        }
    }
    fn finish(app: &mut App) {
        let result = app.take_jump_job().unwrap().run().unwrap();
        assert!(app.handle_jump_result(result));
    }
    fn selected(app: &App) -> Location {
        let Target::Jump(location) = &app
            .picker
            .active
            .as_ref()
            .unwrap()
            .view
            .selected()
            .unwrap()
            .value
        else {
            panic!("jump target")
        };
        location.clone()
    }

    #[test]
    fn all_panes_include_hidden_unsaved_buffers_and_restore_full_selections() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("other.txt");
        std::fs::write(&path, "other buffer\n").unwrap();
        let mut app = App::from_document(Document::from("one\nneedle here\ntail\n"), (120, 24));
        let scratch = app.editor.document().id();
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("!").unwrap();
        app.editor.execute("normal_mode", 1).unwrap();
        let expected = SelectionSet::new(
            vec![
                Selection::new(CharOffset(4), CharOffset(0)),
                Selection::new(CharOffset(5), CharOffset(11)),
            ],
            1,
        )
        .unwrap();
        app.editor.set_selections(expected.clone()).unwrap();
        app.execute("save_selection").unwrap();
        app.open_window_file(&path).unwrap();
        app.execute("vsplit").unwrap();
        let other = app.editor.document().id();
        app.editor.execute("move_right", 3).unwrap();
        app.editor.execute("select_mode", 1).unwrap();
        let origin = app.editor.selections().clone();
        press(&mut app, " jneedle");
        app.handle(key(KeyCode::Enter)); // Early Enter waits for the current query.
        assert!(app.input_waiting());
        assert_eq!(app.editor.document().id(), other);
        finish(&mut app);
        assert!(app.picker.active.is_none());
        assert!(!app.input_waiting());
        assert_eq!(app.editor.document().id(), scratch);
        assert_eq!(app.editor.selections(), &expected);
        assert_eq!(app.editor.mode(), Mode::Select);
        assert!(app.is_dirty());
        assert!(app.editor.document().text().to_string().starts_with("!one"));
        app.execute("jump_backward").unwrap();
        assert_eq!(app.editor.document().id(), other);
        assert_eq!(app.editor.selections(), &origin);
        app.execute("jump_forward").unwrap();
        assert_eq!(app.editor.selections(), &expected);
        app.editor.execute("undo", 1).unwrap();
        assert!(app.editor.document().text().to_string().starts_with("one"));
    }

    #[test]
    fn reopened_picker_keeps_query_identity_and_preview_and_rejects_old_jobs() {
        let mut app = App::from_document(Document::from("alpha\nbeta\ngamma\n"), (120, 24));
        for start in [0, 6, 11] {
            app.editor
                .set_selections(SelectionSet::single(Selection::new(
                    CharOffset(start),
                    CharOffset(start + 1),
                )))
                .unwrap();
            app.execute("save_selection").unwrap();
        }
        press(&mut app, " j");
        let stale = app.take_jump_job().unwrap().run().unwrap();
        press(&mut app, "scratch");
        assert!(!app.handle_jump_result(stale));
        finish(&mut app);
        app.handle(key(KeyCode::Down));
        let wanted = selected(&app);
        assert_eq!(wanted.line, 1);
        let preview = app.take_preview_job().unwrap();
        assert_eq!(preview.position.unwrap().line, 1);
        assert_eq!(preview.snapshot.as_ref().unwrap().id(), wanted.document);
        let preview = preview.run().unwrap();
        let original = app.editor.selections().clone();
        app.handle(key(KeyCode::Esc));
        assert_eq!(app.editor.selections(), &original);
        let Source::Jumps(catalog) = &app.picker.last.as_ref().unwrap().source else {
            panic!("jump source")
        };
        assert!(catalog.captures.is_empty());
        press(&mut app, " '");
        assert!(!app.handle_preview_result(preview));
        assert_eq!(
            app.picker.active.as_ref().unwrap().view.query.text(),
            "scratch"
        );
        app.handle(key(KeyCode::Enter)); // Cached rows must wait for refresh too.
        assert!(app.input_waiting());
        finish(&mut app);
        assert!(app.picker.active.is_none());
        assert_eq!(app.editor.selections(), wanted.selections.as_ref());
        press(&mut app, " j");
        let cancelled = app.take_jump_job().unwrap();
        app.handle(key(KeyCode::Esc));
        assert!(cancelled.cancellation.is_cancelled());
        assert!(cancelled.run().is_none());
    }

    #[test]
    fn reload_refreshes_captured_text_and_closed_buffers_cannot_be_reopened() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.txt");
        std::fs::write(&path, "alpha\nbeta\n").unwrap();
        let mut app = App::open(Some(&path), (120, 24)).unwrap();
        app.editor
            .set_selections(SelectionSet::single(Selection::new(
                CharOffset(6),
                CharOffset(10),
            )))
            .unwrap();
        app.execute("save_selection").unwrap();
        press(&mut app, " j");
        finish(&mut app);
        let old = selected(&app);
        assert_eq!(old.line, 1);
        std::fs::write(&path, "xyz\n").unwrap();
        app.reload_current_file(false).unwrap();
        app.handle(key(KeyCode::Enter));
        assert!(app.input_waiting());
        finish(&mut app);
        assert!(!app.input_waiting());
        assert!(app.picker.active.is_none());
        assert!(app.editor.selections().primary().end().0 <= 4);
        assert!(app.accept_jump_location(old.clone()).is_err());
        app.close_buffer(false).unwrap();
        let current = app.editor.document().id();
        assert_ne!(current, old.document);
        assert!(app.accept_jump_location(old).is_err());
        assert_eq!(app.editor.document().id(), current);
    }

    #[test]
    fn reopened_picker_remaps_selection_snippets_preview_lines_and_stable_identity() {
        let mut app = App::from_document(Document::from("first\nneedle\nlast\n"), (120, 24));
        app.editor
            .set_selections(SelectionSet::single(Selection::new(
                CharOffset(6),
                CharOffset(12),
            )))
            .unwrap();
        app.record_jump();
        press(&mut app, " jneedle");
        finish(&mut app);
        let wanted = selected(&app);
        app.handle(key(KeyCode::Esc));
        app.editor.execute("goto_file_start", 1).unwrap();
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("prefix\n").unwrap();
        app.editor.execute("normal_mode", 1).unwrap();
        press(&mut app, " '");
        finish(&mut app);
        let updated = selected(&app);
        assert_eq!(updated.identity, wanted.identity);
        assert_eq!(updated.line, 2);
        assert_eq!(
            updated.selections.as_ref(),
            &SelectionSet::single(Selection::new(CharOffset(13), CharOffset(19)))
        );
        assert!(
            app.picker
                .active
                .as_ref()
                .unwrap()
                .view
                .selected()
                .unwrap()
                .label
                .contains("needle")
        );
        assert_eq!(app.take_preview_job().unwrap().position.unwrap().line, 2);
        app.handle(key(KeyCode::Enter));
        assert_eq!(app.editor.selections(), updated.selections.as_ref());
    }

    #[test]
    fn external_prefix_reload_keeps_jump_on_its_original_text() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.txt");
        std::fs::write(&path, "first\nneedle\nlast\n").unwrap();
        let mut app = App::open(Some(&path), (120, 24)).unwrap();
        app.editor
            .set_selections(SelectionSet::single(Selection::new(
                CharOffset(6),
                CharOffset(12),
            )))
            .unwrap();
        app.record_jump();
        press(&mut app, " jneedle");
        finish(&mut app);
        std::fs::write(&path, "prefix\nfirst\nneedle\nlast\n").unwrap();
        app.reload_current_file(false).unwrap();
        app.handle(key(KeyCode::Enter));
        finish(&mut app);
        assert_eq!(
            app.editor.selections(),
            &SelectionSet::single(Selection::new(CharOffset(13), CharOffset(19)))
        );
    }
}
