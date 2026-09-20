//! Clipboard requests preserve input ordering and reject stale destinations.

use super::App;
use crate::clipboard::{self, Job, Operation, Outcome};
use std::num::NonZeroUsize;
use vex_core::{DocumentId, Revision, SelectionSet};
use vex_editor::{ClipboardAction, Mode, ViewId, background::Cancellation};

struct Pending {
    id: u64,
    document: DocumentId,
    revision: Revision,
    view: ViewId,
    selections: SelectionSet,
    cancellation: Cancellation,
    job: Option<Job>,
}

#[derive(Default)]
pub(super) struct State {
    next_id: u64,
    pending: Option<Pending>,
}

impl State {
    fn cancel(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.cancellation.cancel();
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl App {
    pub(super) fn begin_clipboard(&mut self, action: ClipboardAction, count: usize) {
        self.clipboard.cancel();
        self.clipboard.next_id += 1;
        let id = self.clipboard.next_id;
        let cancellation = Cancellation::default();
        let selections = self.editor.selections().clone();
        let operation = match action {
            ClipboardAction::Yank | ClipboardAction::YankMain => Operation::Copy {
                snapshot: self.editor.document().snapshot(),
                selections: if action == ClipboardAction::YankMain {
                    SelectionSet::single(selections.primary())
                } else {
                    selections.clone()
                },
            },
            ClipboardAction::Paste(placement) => Operation::Paste {
                plan: self.editor.paste_plan(),
                placement,
                count: NonZeroUsize::new(count).unwrap_or(NonZeroUsize::MIN),
                selections: selections.ranges().len(),
            },
        };
        self.message = if matches!(action, ClipboardAction::Paste(_)) {
            "reading clipboard..."
        } else {
            "copying to clipboard..."
        }
        .into();
        self.clipboard.pending = Some(Pending {
            id,
            document: self.editor.document().id(),
            revision: self.editor.document().revision(),
            view: self.editor.active_view(),
            selections,
            cancellation: cancellation.clone(),
            job: Some(Job {
                id,
                operation,
                cancellation,
            }),
        });
    }

    pub(super) fn clipboard_waiting(&self) -> bool {
        self.clipboard.pending.as_ref().is_some_and(|pending| {
            !pending.cancellation.is_cancelled()
                && pending.document == self.editor.document().id()
                && pending.revision == self.editor.document().revision()
                && pending.view == self.editor.active_view()
                && self.editor.mode() == Mode::Normal
                && pending.selections == *self.editor.selections()
        })
    }

    pub(super) fn cancel_clipboard(&mut self) {
        self.clipboard.cancel();
    }

    pub(super) fn invalidate_clipboard(&mut self) {
        if self.clipboard.pending.is_some() && !self.clipboard_waiting() {
            self.cancel_clipboard();
            if matches!(
                self.message.as_str(),
                "reading clipboard..." | "copying to clipboard..."
            ) {
                self.clear_message();
            }
        }
    }

    pub(crate) fn take_clipboard_job(&mut self) -> Option<Job> {
        self.invalidate_clipboard();
        self.clipboard.pending.as_mut()?.job.take()
    }

    pub(crate) fn handle_clipboard_result(&mut self, result: clipboard::Result) -> bool {
        if self
            .clipboard
            .pending
            .as_ref()
            .is_none_or(|pending| pending.id != result.id)
        {
            return false;
        }
        if !self.clipboard_waiting() {
            self.invalidate_clipboard();
            return false;
        }
        self.clipboard.pending = None;
        self.clear_message();
        match result.outcome {
            Ok(Outcome::Copied(count)) => {
                self.message = format!(
                    "yanked {count} selection{} to clipboard",
                    if count == 1 { "" } else { "s" }
                );
            }
            Ok(Outcome::Paste(transaction)) => {
                if let Err(error) = self.editor.apply_paste(transaction) {
                    self.fail(error);
                }
                self.observe_buffer_revision();
            }
            Err(error) => self.fail(error),
        }
        true
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::clipboard::{Worker, tests::file_worker};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use vex_core::{CharOffset, Document, Selection};
    use vex_editor::Editor;

    fn press(app: &mut App, keys: &str) {
        for ch in keys.chars() {
            app.handle(Event::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )));
        }
    }

    fn fixture(
        source: &str,
        contents: &str,
    ) -> (tempfile::TempDir, std::path::PathBuf, App, Worker) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("clipboard");
        std::fs::write(&path, contents).unwrap();
        let app = App::from_document(Document::from(source), (80, 24));
        let worker = file_worker(&path);
        (directory, path, app, worker)
    }

    fn complete(app: &mut App, worker: &mut Worker) {
        assert!(app.input_waiting());
        let job = app.take_clipboard_job().unwrap();
        let result = worker.run(job).unwrap();
        assert!(app.handle_clipboard_result(result));
        assert!(!app.input_waiting());
        assert!(!app.error, "{}", app.message);
    }

    fn selections(app: &mut App, ranges: &[(usize, usize)]) {
        app.editor
            .set_selections(
                SelectionSet::new(
                    ranges
                        .iter()
                        .map(|&(a, b)| Selection::new(CharOffset(a), CharOffset(b)))
                        .collect(),
                    ranges.len() - 1,
                )
                .unwrap(),
            )
            .unwrap();
    }

    #[test]
    fn clipboard_bindings_keep_fragments_counts_primary_and_internal_register() {
        let (_directory, path, mut app, mut worker) = fixture("cat dog", "old");
        app.editor.execute("yank", 1).unwrap(); // Internal register retains c.
        selections(&mut app, &[(0, 3), (4, 7)]);
        press(&mut app, "v y");
        assert_eq!(app.editor.mode(), Mode::Normal);
        assert_eq!(app.editor.document().text(), "cat dog");
        complete(&mut app, &mut worker);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "cat\ndog");
        selections(&mut app, &[(0, 1), (4, 5)]);
        press(&mut app, "2 R");
        assert_eq!(app.editor.document().text(), "cat dog");
        complete(&mut app, &mut worker);
        assert_eq!(app.editor.document().text(), "catcatat dogdogog");
        assert_eq!(app.editor.selections().primary_index(), 1);
        press(&mut app, "u");
        assert_eq!(app.editor.document().text(), "cat dog");
        let mut internal =
            Editor::with_yank_register(Document::default(), app.editor.yank_register());
        internal.execute("paste_before", 1).unwrap();
        assert_eq!(internal.document().text(), "c");
    }

    #[test]
    fn main_selection_yank_ignores_counts_and_repeats_at_other_destinations() {
        let (_directory, path, mut app, mut worker) = fixture("cat dog", "old");
        selections(&mut app, &[(0, 3), (4, 7)]);
        press(&mut app, "9 Y");
        complete(&mut app, &mut worker);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "dog");
        press(&mut app, " P");
        complete(&mut app, &mut worker);
        assert_eq!(app.editor.document().text(), "dogcat dogdog");
    }

    #[test]
    fn externally_changed_clipboard_is_one_fragment_even_when_it_shares_the_saved_prefix() {
        let (_directory, path, mut app, mut worker) = fixture("cat dog", "");
        selections(&mut app, &[(0, 3), (4, 7)]);
        press(&mut app, " y");
        complete(&mut app, &mut worker);
        std::fs::write(path, "cat\ndog!").unwrap();
        selections(&mut app, &[(0, 1), (4, 5)]);
        press(&mut app, " R");
        complete(&mut app, &mut worker);
        assert_eq!(app.editor.document().text(), "cat\ndog!at cat\ndog!og");
    }

    #[test]
    fn clipboard_paste_handles_unicode_linewise_crlf_replacement_and_empty_text() {
        for (keys, text, expected) in [
            ("2 p", "界\n", "a\r\n界\r\n界\r\nb"),
            (" P", "界\n", "界\r\na\r\nb"),
            (" R", "e\u{301}", "e\u{301}\r\nb"),
            (" R", "", "\r\nb"),
            (" p", "", "a\r\nb"),
        ] {
            let (_directory, _path, mut app, mut worker) = fixture("a\r\nb", text);
            press(&mut app, keys);
            complete(&mut app, &mut worker);
            assert_eq!(app.editor.document().text(), expected, "{keys} {text:?}");
            if !text.is_empty() || keys == " R" {
                press(&mut app, "u");
                assert_eq!(app.editor.document().text(), "a\r\nb");
                press(&mut app, "U");
                assert_eq!(app.editor.document().text(), expected);
            }
        }
    }

    #[test]
    fn cancel_and_resize_keep_the_document_and_ignore_late_results() {
        for key in [
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            let (_directory, _path, mut app, mut worker) = fixture("abc", "replacement");
            press(&mut app, " R");
            let job = app.take_clipboard_job().unwrap();
            let cancellation = job.cancellation.clone();
            let result = worker.run(job).unwrap();
            app.handle(Event::Resize(40, 12));
            assert_eq!(app.size(), (40, 12));
            assert!(app.input_waiting());
            app.handle(Event::Key(key));
            assert!(cancellation.is_cancelled());
            assert!(!app.handle_clipboard_result(result));
            assert!(!app.input_waiting());
            assert_eq!(app.editor.document().text(), "abc");
        }
    }

    #[test]
    fn stale_pastes_cannot_edit_a_new_cursor_view_mode_buffer_or_revision() {
        for kind in 0..5 {
            let (_directory, path, mut app, mut worker) = fixture("abc", "replacement");
            press(&mut app, " R");
            let result = worker.run(app.take_clipboard_job().unwrap()).unwrap();
            match kind {
                0 => app.editor.execute("move_right", 1).unwrap(),
                1 => app.editor.execute("select_mode", 1).unwrap(),
                2 => {
                    let view = app.editor.duplicate_view();
                    app.editor.focus_view(view);
                }
                3 => app.open_window_file(&path).unwrap(),
                _ => {
                    app.editor.execute("insert_mode", 1).unwrap();
                    app.editor.insert_text("z").unwrap();
                    app.editor.execute("normal_mode", 1).unwrap();
                }
            }
            let before = app.editor.document().snapshot();
            assert!(!app.handle_clipboard_result(result));
            assert!(!app.input_waiting());
            assert_eq!(app.editor.document().text(), before.text());
        }
    }

    #[test]
    fn failed_and_oversized_reads_release_input_without_changing_text() {
        for kind in 0..3 {
            let (_directory, path, mut app, mut worker) = fixture("abc", "x");
            match kind {
                0 => std::fs::remove_file(&path).unwrap(),
                1 => std::fs::write(&path, [0xff]).unwrap(),
                _ => {}
            }
            app.editor
                .execute(
                    "paste_clipboard_after",
                    if kind == 2 { usize::MAX } else { 1 },
                )
                .unwrap();
            app.apply_application_action();
            let result = worker.run(app.take_clipboard_job().unwrap()).unwrap();
            assert!(app.handle_clipboard_result(result));
            assert!(app.error);
            assert!(!app.input_waiting());
            assert_eq!(app.editor.document().text(), "abc");
        }
    }

    #[test]
    fn an_old_cancelled_result_does_not_discard_the_next_request() {
        let (_directory, _path, mut app, mut worker) = fixture("abc", "X");
        press(&mut app, " p");
        let old = worker.run(app.take_clipboard_job().unwrap()).unwrap();
        app.handle(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        press(&mut app, " P");
        assert!(!app.handle_clipboard_result(old));
        assert!(app.input_waiting());
        complete(&mut app, &mut worker);
        assert_eq!(app.editor.document().text(), "Xabc");
    }
}
