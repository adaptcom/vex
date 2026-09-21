//! Poll only documents displayed in visible panes, with one background batch
//! in flight. Late reads never replace a newer edit, save, or file identity.

use super::App;
use crate::files::watch::{self, Outcome};
use std::{
    io,
    time::{Duration, Instant},
};
use vex_editor::{Language, background::Cancellation};

const INTERVAL: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(super) struct State {
    due: Option<Instant>,
    request: u64,
    cancellation: Cancellation,
}

impl Drop for State {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

impl App {
    pub(crate) fn enable_file_polling(&mut self, now: Instant) {
        self.reload.due = Some(now + INTERVAL);
    }

    pub(crate) fn file_poll_deadline(&self) -> Option<Instant> {
        self.reload.due
    }

    pub(crate) fn take_file_poll(&mut self, now: Instant) -> Option<watch::Batch> {
        if self.reload.due.is_none_or(|due| due > now) {
            return None;
        }
        let probes = self.visible_file_probes();
        if probes.is_empty() {
            self.reload.due = Some(now + INTERVAL);
            return None;
        }
        self.reload.due = None;
        self.reload.request += 1;
        Some(watch::Batch {
            request: self.reload.request,
            probes,
            cancellation: self.reload.cancellation.clone(),
        })
    }

    pub(crate) fn handle_file_poll(&mut self, result: watch::Result, now: Instant) -> bool {
        if result.request != self.reload.request || self.reload.cancellation.is_cancelled() {
            return false;
        }
        self.reload.due = Some(now + INTERVAL);
        let visible = self.visible_file_probes();
        let mut redraw = false;
        for result in result.observations {
            if !visible
                .iter()
                .any(|probe| probe.snapshot.id() == result.document)
            {
                continue;
            }
            let id = result.document;
            let path = crate::paths::display(&result.path).to_string();
            let accepted = self.with_file_buffer_mut(id, |editor, files, automatic_language| -> io::Result<Option<(bool, String)>> {
                if !files.accepts(&result, editor.document()) { return Ok(None); }
                let notice = match result.outcome {
                    Outcome::Reload(transaction) if !files.is_dirty(editor.document()) => {
                        editor.apply_external_change(transaction).map_err(io::Error::other)?;
                        files.mark_saved(editor.document());
                        if automatic_language {
                            let language = Language::detect(files.path(), editor.document().text());
                            if language != editor.language() { editor.set_language(language); }
                        }
                        return Ok(Some((true, format!("reloaded {path}"))));
                    }
                    Outcome::Reload(_) | Outcome::Conflict => Some(format!("disk changed; unsaved edits kept (:reload! to reload, :w! to overwrite): {path}")),
                    Outcome::Missing => Some(format!("file removed on disk; buffer kept (:w! to recreate): {path}")),
                    Outcome::Error(error) => Some(format!("cannot reload: {error} ({path})")),
                    Outcome::Unchanged => None,
                    Outcome::Retry => return Ok(None),
                };
                let changed = files.notice(notice.clone());
                Ok(changed.then_some(notice).flatten().map(|message| (false, message)))
            });
            match accepted {
                Some(Ok(Some((reloaded, message)))) => {
                    if reloaded {
                        self.after_reload(id);
                        // Keep a conflict warning if another pane also reloaded.
                        if !redraw || !self.error {
                            self.clear_message();
                            self.message = message;
                        }
                    } else {
                        self.fail(message);
                    }
                    redraw = true;
                }
                Some(Err(error)) => {
                    self.fail(error);
                    redraw = true;
                }
                _ => {}
            }
        }
        redraw
    }

    fn after_reload(&mut self, id: vex_core::DocumentId) {
        self.refresh_picker_buffer(id);
        if id == self.editor.document().id() {
            self.dismiss_language_help();
            self.invalidate_completion();
            self.keys.cancel(&mut self.editor);
            self.open_search_prompt();
        }
        self.refresh_git();
    }

    pub(super) fn reload_current_file(&mut self, force: bool) -> io::Result<()> {
        let path = self
            .files
            .target()
            .ok_or_else(|| io::Error::other("no file to reload"))?
            .to_path_buf();
        if self.is_dirty() && !force {
            return Err(io::Error::other(
                "unsaved changes; use :reload! to replace them with the disk version",
            ));
        }
        let transaction = watch::reload(&self.editor.document().snapshot(), &path)?;
        self.editor
            .apply_external_change(transaction)
            .map_err(io::Error::other)?;
        self.files.mark_saved(self.editor.document());
        if self.automatic_language {
            let language = Language::detect(self.files.path(), self.editor.document().text());
            if language != self.editor.language() {
                self.editor.set_language(language);
            }
        }
        self.after_reload(self.editor.document().id());
        self.message = format!("reloaded {}", crate::paths::display(&path));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::Event;
    use std::fs;
    use vex_core::{CharOffset, Document, Selection, SelectionSet};

    fn poll(app: &mut App, worker: &mut watch::Worker) -> bool {
        let now = app.file_poll_deadline().unwrap();
        let batch = app.take_file_poll(now).unwrap();
        let result = worker.run(batch).unwrap();
        app.handle_file_poll(result, now)
    }

    #[test]
    fn clean_shared_buffers_reload_on_a_two_second_deadline_and_keep_undo_and_lsp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rs");
        let original = "fn main() {}\n";
        fs::write(&path, original).unwrap();
        let mut app = App::open(Some(&path), (100, 24)).unwrap();
        app.editor.set_background_syntax(true);
        app.enable_lsp();
        app.take_lsp_update().unwrap();
        let id = app.editor.document().id();
        app.editor
            .set_selections(SelectionSet::single(Selection::new(
                CharOffset(3),
                CharOffset(4),
            )))
            .unwrap();
        app.execute("vsplit").unwrap();
        let now = Instant::now();
        app.enable_file_polling(now);
        assert!(
            app.take_file_poll(now + INTERVAL - Duration::from_millis(1))
                .is_none()
        );
        fs::write(&path, format!("// external\n{original}")).unwrap();
        let batch = app.take_file_poll(now + INTERVAL).unwrap();
        assert_eq!(
            batch.probes.len(),
            1,
            "shared panes should read a file once"
        );
        assert!(
            app.take_file_poll(now + INTERVAL * 3).is_none(),
            "do not overlap reads"
        );
        let mut worker = watch::Worker::default();
        assert!(app.handle_file_poll(worker.run(batch).unwrap(), now + INTERVAL));
        assert_eq!(app.editor.document().id(), id);
        assert_eq!(app.editor.selections().primary().start(), CharOffset(15));
        assert!(!app.is_dirty());
        let update = app.take_lsp_update().unwrap().document.unwrap();
        assert_eq!(update.snapshot.id(), id);
        assert_eq!(update.snapshot.text(), "// external\nfn main() {}\n");
        app.execute("jump_view_left").unwrap();
        assert_eq!(app.editor.selections().primary().start(), CharOffset(15));
        app.editor.execute("undo", 1).unwrap();
        assert_eq!(app.editor.document().text(), original);
        assert!(app.is_dirty());
        app.editor.execute("redo", 1).unwrap();
        assert!(!app.is_dirty());
        assert!(!poll(&mut app, &mut worker), "unchanged files don't redraw");
    }

    #[test]
    fn dirty_buffers_keep_edits_until_explicit_reload_and_reload_is_undoable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        fs::write(&path, "before").unwrap();
        let mut app = App::open(Some(&path), (80, 24)).unwrap();
        app.enable_file_polling(Instant::now());
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("local ").unwrap();
        fs::write(&path, "remote").unwrap();
        let mut worker = watch::Worker::default();
        assert!(poll(&mut app, &mut worker));
        assert!(app.message.contains("unsaved edits kept"));
        assert!(app.error);
        assert_eq!(app.editor.document().text(), "local before");
        assert!(
            !poll(&mut app, &mut worker),
            "don't repeat the warning every poll"
        );
        assert!(app.execute("w").is_err());
        assert!(app.execute("reload").is_err());
        app.execute("reload!").unwrap();
        assert_eq!(app.editor.document().text(), "remote");
        assert!(!app.is_dirty());
        app.editor.execute("undo", 1).unwrap();
        assert_eq!(app.editor.document().text(), "local before");
        assert!(app.is_dirty());
        assert!(!poll(&mut app, &mut worker));
        app.editor.execute("redo", 1).unwrap();
        assert!(!app.is_dirty());
    }

    #[test]
    fn late_reads_cannot_overwrite_typing_a_save_or_a_new_path() {
        for action in ["edit", "save", "save-as"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("file.txt");
            fs::write(&path, "before").unwrap();
            let mut app = App::open(Some(&path), (80, 24)).unwrap();
            app.enable_file_polling(Instant::now());
            fs::write(&path, "remote").unwrap();
            let now = app.file_poll_deadline().unwrap();
            let batch = app.take_file_poll(now).unwrap();
            let result = watch::Worker::default().run(batch).unwrap();
            match action {
                "edit" => {
                    app.editor.execute("insert_mode", 1).unwrap();
                    app.editor.insert_text("local ").unwrap();
                }
                "save" => {
                    app.execute("w!").unwrap();
                }
                _ => {
                    app.execute(&format!("w {}", dir.path().join("other.txt").display()))
                        .unwrap();
                }
            }
            let text = app.editor.document().text().clone();
            let revision = app.editor.document().revision();
            assert!(!app.handle_file_poll(result, now));
            assert!(text.is_instance(app.editor.document().text()));
            assert_eq!(app.editor.document().revision(), revision);
        }
    }

    #[test]
    fn poll_only_visible_documents_and_skip_scratch_and_hidden_splits() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("one.txt");
        let second = dir.path().join("two.txt");
        fs::write(&first, "one").unwrap();
        fs::write(&second, "two").unwrap();
        let mut app = App::open(Some(&first), (100, 24)).unwrap();
        let first_id = app.editor.document().id();
        app.execute(&format!("vsplit {}", second.display()))
            .unwrap();
        assert_eq!(app.visible_file_probes().len(), 2);
        app.handle(Event::Resize(10, 4));
        let probes = app.visible_file_probes();
        assert_eq!(probes.len(), 1);
        assert_eq!(probes[0].path.file_name().unwrap(), "two.txt");
        app.enable_file_polling(Instant::now());
        fs::write(&first, "external one").unwrap();
        let mut worker = watch::Worker::default();
        assert!(!poll(&mut app, &mut worker));
        app.with_file_buffer_mut(first_id, |editor, _, _| {
            assert_eq!(editor.document().text(), "one")
        })
        .unwrap();
        app.handle(Event::Resize(100, 24));
        assert!(poll(&mut app, &mut worker));
        app.with_file_buffer_mut(first_id, |editor, files, _| {
            assert_eq!(editor.document().text(), "external one");
            assert!(!files.is_dirty(editor.document()));
        })
        .unwrap();
        let mut scratch = App::from_document(Document::default(), (80, 24));
        scratch.enable_file_polling(Instant::now());
        assert!(
            scratch
                .take_file_poll(scratch.file_poll_deadline().unwrap())
                .is_none()
        );
    }

    #[test]
    fn missing_invalid_and_atomically_replaced_files_keep_safe_reload_behavior() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        fs::write(&path, "before").unwrap();
        let mut app = App::open(Some(&path), (80, 24)).unwrap();
        app.enable_file_polling(Instant::now());
        let mut worker = watch::Worker::default();
        assert!(!poll(&mut app, &mut worker));
        fs::remove_file(&path).unwrap();
        assert!(poll(&mut app, &mut worker));
        assert!(app.message.contains("removed on disk"));
        assert_eq!(app.editor.document().text(), "before");
        assert!(app.execute("reload!").is_err());
        fs::write(&path, [0xff]).unwrap();
        assert!(poll(&mut app, &mut worker));
        assert_eq!(app.editor.document().text(), "before");
        assert!(app.execute("reload!").is_err());
        let replacement = dir.path().join("replacement");
        fs::write(&replacement, "🦀\r\nnew\r\n").unwrap();
        fs::rename(&replacement, &path).unwrap();
        assert!(poll(&mut app, &mut worker));
        assert_eq!(app.editor.document().text(), "🦀\r\nnew\r\n");
        assert_eq!(app.editor.newline(), "\r\n");
        assert!(!app.is_dirty());
    }
}
