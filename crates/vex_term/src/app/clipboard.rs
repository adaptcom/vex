//! Clipboard requests preserve input ordering and reject stale destinations.

use super::App;
use crate::clipboard::{self, Job, Operation, Outcome};
use crate::input::PromptStamp;
use std::num::NonZeroUsize;
use vex_core::{DocumentId, Revision, SelectionSet};
use vex_editor::{ClipboardAction, ClipboardKind, Mode, ViewId, background::Cancellation};

enum Target {
    Document(ClipboardAction),
    Prompt(PromptStamp),
    Picker((u64, PromptStamp)),
}

struct Pending {
    id: u64,
    document: DocumentId,
    revision: Revision,
    view: ViewId,
    selections: SelectionSet,
    mode: Mode,
    target: Target,
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
    pub(super) fn begin_clipboard(
        &mut self,
        kind: ClipboardKind,
        action: ClipboardAction,
        count: usize,
    ) {
        let selections = self.editor.selections().clone();
        let operation = match action {
            ClipboardAction::Yank
            | ClipboardAction::YankMain
            | ClipboardAction::Delete
            | ClipboardAction::Change => Operation::Copy {
                snapshot: self.editor.document().snapshot(),
                selections: if action == ClipboardAction::YankMain {
                    SelectionSet::single(selections.primary())
                } else {
                    selections.clone()
                },
                cut: matches!(action, ClipboardAction::Delete | ClipboardAction::Change)
                    .then(|| self.editor.paste_plan()),
            },
            ClipboardAction::Paste(placement) => Operation::Paste {
                plan: self.editor.paste_plan(),
                placement,
                count: NonZeroUsize::new(count).unwrap_or(NonZeroUsize::MIN),
                selections: selections.ranges().len(),
            },
            ClipboardAction::Search { .. } => Operation::Read {
                limit: vex_core::regex::MAX_PATTERN_BYTES,
                trim_controls: false,
                truncate: false,
            },
            ClipboardAction::SetSearch { .. } => {
                unreachable!("search writes include a query payload")
            }
        };
        self.start_clipboard(kind, operation, Target::Document(action));
    }

    pub(super) fn begin_clipboard_search_write(
        &mut self,
        kind: ClipboardKind,
        text: std::sync::Arc<str>,
        activate: bool,
    ) {
        let action = ClipboardAction::SetSearch {
            register: kind.register(),
            activate,
        };
        self.start_clipboard(
            kind,
            Operation::Write {
                values: std::sync::Arc::from([text]),
            },
            Target::Document(action),
        );
    }

    pub(super) fn begin_prompt_clipboard(&mut self, kind: ClipboardKind) {
        if let Some(prompt) = &self.prompt {
            self.start_clipboard(
                kind,
                Operation::Read {
                    limit: 64 << 10,
                    trim_controls: true,
                    truncate: false,
                },
                Target::Prompt(prompt.input.stamp()),
            );
        }
    }

    pub(super) fn begin_picker_clipboard(&mut self, kind: ClipboardKind) {
        if let Some(stamp) = self.picker_query_stamp() {
            self.start_clipboard(
                kind,
                Operation::Read {
                    limit: crate::picker::MAX_QUERY_BYTES,
                    trim_controls: true,
                    truncate: true,
                },
                Target::Picker(stamp),
            );
        }
    }

    fn start_clipboard(&mut self, kind: ClipboardKind, operation: Operation, target: Target) {
        self.clipboard.cancel();
        self.clipboard.next_id += 1;
        let id = self.clipboard.next_id;
        let cancellation = Cancellation::default();
        self.clear_message();
        self.message = if !matches!(operation, Operation::Copy { .. } | Operation::Write { .. }) {
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
            selections: self.editor.selections().clone(),
            mode: self.editor.mode(),
            target,
            cancellation: cancellation.clone(),
            job: Some(Job {
                id,
                kind,
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
                && pending.mode == self.editor.mode()
                && match pending.target {
                    Target::Document(_) => {
                        self.editor.clipboard_command_pending()
                            && pending.selections == *self.editor.selections()
                    }
                    Target::Prompt(stamp) => self
                        .prompt
                        .as_ref()
                        .is_some_and(|p| p.input.stamp() == stamp),
                    Target::Picker(stamp) => self.picker_query_stamp() == Some(stamp),
                }
        })
    }

    pub(super) fn cancel_clipboard(&mut self) {
        self.clipboard.cancel();
        self.editor.cancel_clipboard_command();
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
        let pending = self.clipboard.pending.take().unwrap();
        self.clear_message();
        match result.outcome {
            Ok(Outcome::Copied(count)) => {
                if let Target::Document(action) = pending.target
                    && let Err(error) = self.editor.complete_clipboard_command(action, None)
                {
                    self.fail(error);
                    return true;
                }
                self.message = if matches!(
                    pending.target,
                    Target::Document(ClipboardAction::SetSearch { .. })
                ) {
                    "search pattern copied to clipboard".into()
                } else {
                    format!(
                        "yanked {count} selection{} to clipboard",
                        if count == 1 { "" } else { "s" }
                    )
                };
            }
            Ok(Outcome::Paste(transaction)) => {
                if let Target::Document(action) = pending.target
                    && let Err(error) = self
                        .editor
                        .complete_clipboard_command(action, Some(transaction))
                {
                    self.fail(error);
                }
                self.observe_buffer_revision();
            }
            Ok(Outcome::Text(text)) => match pending.target {
                Target::Prompt(_) => {
                    let mut prompt = self.prompt.take().unwrap();
                    prompt.input.insert(&text);
                    self.preview_search(&prompt);
                    self.prompt = Some(prompt);
                }
                Target::Picker(_) => self.insert_picker_register(&text),
                Target::Document(ClipboardAction::Search { reverse }) => {
                    if let Err(error) = self.editor.complete_clipboard_search(reverse, text) {
                        self.fail(error);
                    }
                }
                Target::Document(_) => {
                    unreachable!("only clipboard search reads return text for a document")
                }
            },
            Err(error) => {
                self.editor.cancel_clipboard_command();
                self.fail(error);
            }
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

    fn ctrl(app: &mut App, ch: char) {
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char(ch),
            KeyModifiers::CONTROL,
        )));
    }

    fn escape(app: &mut App) {
        app.handle(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    }

    fn deliver(app: &mut App, worker: &mut Worker) {
        let job = app.take_clipboard_job().unwrap();
        assert!(app.handle_clipboard_result(worker.run(job).unwrap()));
        assert!(!app.error, "{}", app.message);
    }

    fn replay(app: &mut App, worker: &mut Worker) {
        for _ in 0..32 {
            if !app.editor.repeat_pending() {
                return;
            }
            app.advance_repeat();
            if app.clipboard_waiting() {
                deliver(app, worker);
            }
        }
        panic!("clipboard replay did not complete");
    }

    fn search_result(app: &mut App) {
        let result = app.editor.take_search_job().unwrap().run().unwrap();
        assert!(app.handle_search_result(result));
        assert!(!app.error, "{}", app.message);
    }

    #[test]
    fn clipboard_search_reads_then_scans_and_acceptance_writes_before_activating_register() {
        let (_directory, path, mut app, mut worker) = fixture("cat dog cat dog", "dog");
        app.editor.set_background_search(true);
        press(&mut app, "\"+2n");
        deliver(&mut app, &mut worker);
        assert!(app.input_waiting());
        assert!(app.editor.search_waiting());
        search_result(&mut app);
        assert_eq!(
            app.editor.selections().primary().range(),
            CharOffset(12)..CharOffset(15)
        );
        assert!(!app.input_waiting());
        press(&mut app, "gg\"+/dog");
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        search_result(&mut app);
        assert!(app.clipboard_waiting());
        complete(&mut app, &mut worker);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "dog");
        press(&mut app, "n"); // Accepted search made + the active register.
        deliver(&mut app, &mut worker);
        search_result(&mut app);
        assert_eq!(
            app.editor.selections().primary().range(),
            CharOffset(12)..CharOffset(15)
        );
        std::fs::write(&path, "cat").unwrap();
        press(&mut app, "n");
        deliver(&mut app, &mut worker);
        search_result(&mut app);
        assert_eq!(
            app.editor.selections().primary().range(),
            CharOffset(0)..CharOffset(3)
        );
        press(&mut app, "\"+*");
        search_result(&mut app);
        assert!(app.clipboard_waiting());
        complete(&mut app, &mut worker);
        assert_eq!(std::fs::read_to_string(path).unwrap(), r"\bcat\b");
    }

    #[test]
    fn failed_clipboard_search_writes_leave_the_previous_search_register_active() {
        let (directory, _path, mut app, mut worker) =
            fixture("x cat dog cat", "original clipboard");
        app.editor.set_background_search(true);
        press(&mut app, "/cat");
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        search_result(&mut app);
        press(&mut app, "\"+/dog");
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        search_result(&mut app);
        let mut broken = file_worker(&directory.path().join("missing").join("clipboard"));
        let result = broken.run(app.take_clipboard_job().unwrap()).unwrap();
        assert!(app.handle_clipboard_result(result));
        assert!(app.error);
        press(&mut app, "n");
        assert!(!app.clipboard_waiting());
        search_result(&mut app);
        assert_eq!(
            app.editor.selections().primary().range(),
            CharOffset(10)..CharOffset(13)
        );
        press(&mut app, "\"+n");
        let result = worker.run(app.take_clipboard_job().unwrap()).unwrap();
        app.editor.execute("move_left", 1).unwrap();
        app.editor.execute("move_right", 1).unwrap();
        assert!(!app.handle_clipboard_result(result));
        assert!(!app.input_waiting());
    }

    #[test]
    fn clipboard_replay_yields_while_waiting_and_cancellation_keeps_one_undoable_prefix() {
        let (_directory, _path, mut app, mut worker) = fixture("x", "B");
        app.editor.set_deferred_repeat(true);
        press(&mut app, "iA");
        ctrl(&mut app, 'r');
        press(&mut app, "+");
        complete(&mut app, &mut worker);
        escape(&mut app);
        let original = app.editor.document().text().to_string();
        press(&mut app, "1000.");
        assert!(app.advance_repeat());
        assert!(app.editor.repeat_pending());
        assert!(!app.editor.repeat_ready());
        assert!(!app.advance_repeat());
        let result = worker.run(app.take_clipboard_job().unwrap()).unwrap();
        app.handle(Event::Resize(50, 12));
        escape(&mut app);
        assert!(!app.editor.repeat_pending());
        assert!(!app.input_waiting());
        assert_eq!(app.editor.mode(), Mode::Normal);
        assert!(!app.handle_clipboard_result(result));
        assert_ne!(app.editor.document().text().to_string(), original);
        press(&mut app, "u");
        assert_eq!(app.editor.document().text().to_string(), original);
    }

    #[test]
    fn clipboard_registers_have_separate_fragment_caches_and_support_cuts() {
        let (directory, system, mut app, _) = fixture("cat dog", "old");
        let primary = directory.path().join("primary");
        std::fs::write(&primary, "old primary").unwrap();
        let mut worker = crate::clipboard::tests::two_file_worker(&system, &primary);
        app.editor.execute("yank", 1).unwrap();
        selections(&mut app, &[(0, 3), (4, 7)]);
        press(&mut app, "\"+y");
        complete(&mut app, &mut worker);
        assert_eq!(std::fs::read_to_string(&system).unwrap(), "cat\ndog");
        selections(&mut app, &[(4, 7)]);
        press(&mut app, "\"*d");
        assert_eq!(app.editor.document().text(), "cat dog");
        complete(&mut app, &mut worker);
        assert_eq!(app.editor.document().text(), "cat ");
        assert_eq!(std::fs::read_to_string(&primary).unwrap(), "dog");
        press(&mut app, "u");
        selections(&mut app, &[(0, 1), (4, 5)]);
        press(&mut app, "\"+R");
        complete(&mut app, &mut worker);
        assert_eq!(app.editor.document().text(), "catat dogog");
        press(&mut app, "u\"*R");
        complete(&mut app, &mut worker);
        assert_eq!(app.editor.document().text(), "dogat dogog");
        assert_eq!(
            app.editor.register_first('"').unwrap().as_deref(),
            Some("c")
        );
    }

    #[test]
    fn clipboard_change_enters_insert_only_after_copy_and_replays_as_one_undo_group() {
        for deferred in [false, true] {
            let (_directory, path, mut app, mut worker) = fixture("abc", "old");
            app.editor.set_deferred_repeat(deferred);
            press(&mut app, "\"+c");
            assert_eq!(app.editor.mode(), Mode::Normal);
            assert_eq!(app.editor.document().text(), "abc");
            complete(&mut app, &mut worker);
            assert_eq!(app.editor.mode(), Mode::Insert);
            press(&mut app, "X");
            escape(&mut app);
            assert_eq!(app.editor.document().text(), "Xbc");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "a");
            press(&mut app, "u");
            assert_eq!(app.editor.document().text(), "abc");
            press(&mut app, "l.");
            replay(&mut app, &mut worker);
            assert_eq!(app.editor.document().text(), "aXc");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "b");
            press(&mut app, "u");
            assert_eq!(app.editor.document().text(), "abc");
        }
    }

    #[test]
    fn clipboard_insert_preserves_carets_and_replay_reads_current_contents() {
        for deferred in [false, true] {
            let (_directory, path, mut app, mut worker) = fixture("ab\r\n", "界\nX");
            app.editor.set_deferred_repeat(deferred);
            press(&mut app, "a");
            ctrl(&mut app, 'r');
            press(&mut app, "+");
            assert_eq!(app.editor.document().text(), "ab\r\n");
            complete(&mut app, &mut worker);
            assert_eq!(app.editor.mode(), Mode::Insert);
            assert_eq!(app.editor.document().text(), "a界\r\nXb\r\n");
            escape(&mut app);
            std::fs::write(&path, "Z").unwrap();
            press(&mut app, "2.");
            replay(&mut app, &mut worker);
            assert_eq!(app.editor.document().text(), "a界\r\nXZZb\r\n");
            press(&mut app, "u");
            assert_eq!(app.editor.document().text(), "a界\r\nXb\r\n");
        }
    }

    #[test]
    fn failed_clipboard_change_and_cancelled_insert_leave_mode_text_and_repeat_intact() {
        let (directory, _path, mut app, mut worker) = fixture("abc", "value");
        press(&mut app, "iX");
        escape(&mut app);
        press(&mut app, "\"+c");
        let mut broken = file_worker(&directory.path().join("missing").join("clipboard"));
        let result = broken.run(app.take_clipboard_job().unwrap()).unwrap();
        assert!(app.handle_clipboard_result(result));
        assert!(app.error);
        assert_eq!(app.editor.mode(), Mode::Normal);
        assert_eq!(app.editor.document().text(), "Xabc");
        press(&mut app, ".");
        assert_eq!(app.editor.document().text(), "XXabc");
        press(&mut app, "a");
        ctrl(&mut app, 'r');
        press(&mut app, "+");
        let result = worker.run(app.take_clipboard_job().unwrap()).unwrap();
        escape(&mut app);
        assert!(!app.handle_clipboard_result(result));
        assert_eq!(app.editor.mode(), Mode::Insert);
        assert_eq!(app.editor.document().text(), "XXabc");
        assert!(!app.input_waiting());
    }

    #[test]
    fn clipboard_prompt_insertion_is_literal_and_stale_prompt_results_are_ignored() {
        let (_directory, _path, mut app, mut worker) = fixture("abc", "e\u{301}\n:q!\r\n");
        press(&mut app, ":xx");
        app.handle(Event::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)));
        ctrl(&mut app, 'r');
        press(&mut app, "+");
        complete(&mut app, &mut worker);
        assert_eq!(app.prompt.as_ref().unwrap().input.text(), "e\u{301}:q!xx");
        assert!(!app.should_quit());
        ctrl(&mut app, 'r');
        press(&mut app, "+");
        let result = worker.run(app.take_clipboard_job().unwrap()).unwrap();
        app.prompt
            .as_mut()
            .unwrap()
            .input
            .handle(vex_editor::Key::Left);
        assert!(!app.handle_clipboard_result(result));
        assert!(!app.input_waiting());
        ctrl(&mut app, 'r');
        press(&mut app, "+");
        let result = worker.run(app.take_clipboard_job().unwrap()).unwrap();
        escape(&mut app);
        assert!(app.prompt.is_some()); // First Escape cancels only the read.
        assert!(!app.handle_clipboard_result(result));
        escape(&mut app);
        assert!(app.prompt.is_none());
        assert_eq!(app.editor.document().text(), "abc");
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
        for kind in 0..7 {
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
                5 => {
                    let original = app.editor.selections().clone();
                    selections(&mut app, &[(1, 2)]);
                    app.editor.set_selections(original).unwrap();
                }
                6 => {
                    let original = app.editor.active_view();
                    let next = app.editor.duplicate_view();
                    app.editor.focus_view(next);
                    app.editor.focus_view(original);
                }
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
