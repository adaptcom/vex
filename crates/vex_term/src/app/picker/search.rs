//! Workspace search sessions share picker navigation, preview, and restoration.

use super::*;
use crate::picker::search::{Hit, Job, OpenDocument, Result};
use std::time::{Duration, Instant};

const QUERY_DELAY: Duration = Duration::from_millis(150);

pub(super) struct Source {
    pub root: PathBuf,
    pub documents: Arc<[OpenDocument]>,
    register: char,
    due: Instant,
}

impl App {
    pub(in crate::app) fn workspace_search_active(&self) -> bool {
        self.picker
            .active
            .as_ref()
            .is_some_and(|active| matches!(active.source, super::Source::Search(_)))
    }

    pub(in crate::app) fn open_workspace_search(&mut self, register: char) -> io::Result<()> {
        let root = std::env::current_dir()?;
        self.start_workspace_search(root, register);
        Ok(())
    }

    fn start_workspace_search(&mut self, root: PathBuf, register: char) {
        self.close_picker();
        self.dismiss_language_help();
        self.keys.cancel(&mut self.editor);
        self.prompt = None;
        if self.editor.search_prompt().is_some() {
            let _ = self.editor.execute("search_cancel", 1);
        }
        self.picker.next_session += 1;
        let mut view = Picker::new(format!("Search · {}", crate::paths::display(&root)));
        view.noun = "files";
        self.picker.active = Some(Active {
            view,
            session: self.picker.next_session,
            revision: 0,
            source: super::Source::Search(Source {
                root,
                documents: self.workspace_documents(),
                register,
                due: Instant::now(),
            }),
            cancellation: Cancellation::default(),
            preview_cancel: Cancellation::default(),
            preview_request: 0,
            preview_target: None,
            accept_pending: false,
        });
        self.submit_workspace_query();
    }

    pub(super) fn submit_workspace_query(&mut self) {
        let active = self.picker.active.as_mut().unwrap();
        let super::Source::Search(source) = &mut active.source else {
            return;
        };
        active.cancellation.cancel();
        active.preview_cancel.cancel();
        active.view.preview_pending = false;
        active.cancellation = Cancellation::default();
        active.revision += 1;
        active.preview_target = None;
        active.accept_pending = false;
        active.view.begin_update();
        source.due = Instant::now() + QUERY_DELAY;
        self.picker.preview_job = None;
        self.picker.search_job = Some(Job {
            session: active.session,
            revision: active.revision,
            root: source.root.clone(),
            query: active.view.query.text().into(),
            documents: source.documents.clone(),
            cancellation: active.cancellation.clone(),
        });
    }

    pub(super) fn refresh_workspace_search(&mut self) {
        let documents = self.workspace_documents();
        let active = self.picker.active.as_mut().unwrap();
        let super::Source::Search(source) = &mut active.source else {
            return;
        };
        source.documents = documents;
        let accepting = active.accept_pending;
        active.view.resume();
        self.submit_workspace_query();
        self.picker.active.as_mut().unwrap().accept_pending = accepting;
    }

    pub(crate) fn workspace_search_deadline(&self) -> Option<Instant> {
        self.picker.search_job.as_ref()?;
        let active = self.picker.active.as_ref()?;
        let super::Source::Search(source) = &active.source else {
            return None;
        };
        Some(source.due)
    }

    pub(crate) fn take_workspace_search_job(&mut self, now: Instant) -> Option<Job> {
        let active = self.picker.active.as_ref()?;
        if !active.accept_pending && self.workspace_search_deadline()? > now {
            return None;
        }
        self.picker.search_job.take()
    }

    pub(crate) fn handle_workspace_search_result(&mut self, result: Result) -> bool {
        let Some(active) = &mut self.picker.active else {
            return false;
        };
        if !matches!(active.source, super::Source::Search(_))
            || active.session != result.session
            || active.revision != result.revision
            || active.cancellation.is_cancelled()
        {
            return false;
        }
        active.view.title = format!("Search · {}", crate::paths::display(&result.root));
        let replaced = active.view.replace_incremental(
            result
                .items
                .into_iter()
                .map(|item| Item {
                    entry: Arc::new(Entry {
                        label: item.entry.label.clone(),
                        value: Target::Search(item.entry.value.clone()),
                    }),
                    matched: item.matched,
                })
                .collect(),
            !result.scanning,
        );
        if replaced {
            active.view.matched = result.matched;
            active.view.total = result.scanned;
        }
        active.view.pending = result.scanning;
        active.view.notice = result.notice;
        if active.accept_pending
            && ((active.view.current() && !active.view.items.is_empty()) || !result.scanning)
        {
            active.accept_pending = false;
            if !active.view.items.is_empty() {
                self.accept_picker();
            } else {
                if active.view.notice.is_empty() {
                    active.view.notice = "No matching lines".into();
                }
                self.request_picker_preview();
            }
        } else {
            self.request_picker_preview();
        }
        true
    }

    pub(super) fn accept_workspace_hit(&mut self, hit: Hit) -> io::Result<()> {
        let active = self.picker.active.as_ref().unwrap();
        let super::Source::Search(source) = &active.source else {
            unreachable!()
        };
        let register = source.register;
        let query: Arc<str> = Arc::from(active.view.query.text());
        self.open_workspace_hit(&hit)?;
        self.prompt_history.push(register, &query);
        self.editor
            .remember_search_query(register, query)
            .map_err(io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker::search::Worker;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use vex_core::Document;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }
    fn press(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.handle(key(KeyCode::Char(ch)));
        }
    }
    fn fixture() -> (tempfile::TempDir, App) {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), "original\n").unwrap();
        std::fs::write(root.path().join("b.txt"), "needle on disk\n").unwrap();
        let app = App::open(Some(&root.path().join("a.txt")), (120, 24)).unwrap();
        (root, app)
    }
    fn finish(app: &mut App, worker: &mut Worker) {
        let job = app
            .take_workspace_search_job(Instant::now() + Duration::from_secs(1))
            .unwrap();
        worker.run(job, |result| {
            app.handle_workspace_search_result(result);
        });
    }
    fn selected(app: &App) -> String {
        let selection = app.editor.selections().primary();
        app.editor
            .document()
            .text()
            .slice(selection.start().0..selection.end().0)
            .to_string()
    }

    #[test]
    fn typing_debounces_without_copying_open_document_catalog_and_enter_dispatches_immediately() {
        let (root, mut app) = fixture();
        app.start_workspace_search(root.path().into(), '/');
        press(&mut app, "nee");
        let catalog = app.picker.search_job.as_ref().unwrap().documents.clone();
        let old = app.picker.search_job.as_ref().unwrap().cancellation.clone();
        let deadline = app.workspace_search_deadline().unwrap();
        assert!(
            app.take_workspace_search_job(deadline - Duration::from_nanos(1))
                .is_none()
        );
        press(&mut app, "dle");
        assert!(old.is_cancelled());
        assert!(Arc::ptr_eq(
            &catalog,
            &app.picker.search_job.as_ref().unwrap().documents
        ));
        let deadline = app.workspace_search_deadline().unwrap();
        assert_eq!(
            app.take_workspace_search_job(deadline).unwrap().query,
            "needle"
        );
        assert!(app.workspace_search_deadline().is_none());
        press(&mut app, "s");
        let before = app.workspace_search_deadline().unwrap() - QUERY_DELAY;
        app.handle(key(KeyCode::Enter));
        assert_eq!(
            app.take_workspace_search_job(before).unwrap().query,
            "needles"
        );
        app.handle(key(KeyCode::Esc));
        assert!(app.workspace_search_deadline().is_none());
        assert!(!app.input_waiting());
    }

    #[test]
    fn oversized_pasted_queries_keep_the_picker_limit_and_cancel_prior_work() {
        let (root, mut app) = fixture();
        app.start_workspace_search(root.path().into(), '/');
        press(&mut app, "needle");
        let old = app.picker.search_job.as_ref().unwrap().cancellation.clone();
        app.handle(Event::Paste("x".repeat(vex_core::regex::MAX_PATTERN_BYTES)));
        assert!(old.is_cancelled());
        assert_eq!(
            app.picker.search_job.as_ref().unwrap().query.len(),
            crate::picker::MAX_QUERY_BYTES
        );
        assert!(!app.input_waiting());
        app.handle(key(KeyCode::Esc));
        assert!(app.workspace_search_deadline().is_none());
        assert!(app.picker.active.is_none());
    }

    #[test]
    fn global_search_binding_respects_modes_and_named_register_validation() {
        for mode in [vex_editor::Mode::Normal, vex_editor::Mode::Select] {
            let mut app = App::from_document(Document::from("text"), (80, 24));
            if mode == vex_editor::Mode::Select {
                app.editor.execute("select_mode", 1).unwrap();
            }
            press(&mut app, "\"a /");
            assert!(app.workspace_search_active());
            let super::super::Source::Search(source) = &app.picker.active.as_ref().unwrap().source
            else {
                panic!()
            };
            assert_eq!(source.register, 'a');
            assert_eq!(source.root, std::env::current_dir().unwrap());
            app.handle(key(KeyCode::Esc));
            press(&mut app, "\"# /");
            assert!(!app.workspace_search_active());
            assert!(app.error);
        }
    }

    #[test]
    fn unsaved_hidden_buffers_accept_matching_lines_remember_query_and_reopen() {
        let (root, mut app) = fixture();
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("unsaved marker\n").unwrap();
        app.editor.execute("normal_mode", 1).unwrap();
        let original = app.editor.document().id();
        app.open_window_file(&root.path().join("b.txt")).unwrap();
        let origin = app.editor.document().id();
        app.start_workspace_search(root.path().into(), 'a');
        press(&mut app, "marker");
        app.handle(key(KeyCode::Enter));
        assert!(app.input_waiting());
        finish(&mut app, &mut Worker::default());
        assert!(!app.input_waiting());
        assert!(app.picker.active.is_none());
        assert_eq!(app.editor.document().id(), original);
        assert_eq!(selected(&app), "unsaved marker\n");
        assert_eq!(app.editor.register('a').unwrap()[0].as_ref(), "marker");
        app.editor.execute("search_next", 1).unwrap();
        assert_eq!(selected(&app), "marker");
        app.execute("jump_back").unwrap();
        app.take_lsp_update();
        assert_eq!(app.editor.document().id(), origin);
        press(&mut app, " '");
        assert_eq!(
            app.picker.active.as_ref().unwrap().view.query.text(),
            "marker"
        );
        finish(&mut app, &mut Worker::default());
        app.handle(key(KeyCode::Enter));
        assert_eq!(selected(&app), "unsaved marker\n");
        assert_eq!(
            std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
            "original\n"
        );
    }

    #[test]
    fn stale_results_invalid_patterns_and_deleted_files_leave_the_picker_usable() {
        let (root, mut app) = fixture();
        app.start_workspace_search(root.path().into(), '/');
        press(&mut app, "needle");
        let mut worker = Worker::default();
        let mut old = Vec::new();
        worker.run(
            app.take_workspace_search_job(Instant::now() + Duration::from_secs(1))
                .unwrap(),
            |result| old.push(result),
        );
        press(&mut app, "[");
        for result in old {
            assert!(!app.handle_workspace_search_result(result));
        }
        app.handle(key(KeyCode::Enter));
        finish(&mut app, &mut worker);
        assert!(!app.input_waiting());
        assert!(
            app.picker
                .active
                .as_ref()
                .unwrap()
                .view
                .notice
                .contains("Invalid regex")
        );
        app.handle(key(KeyCode::Backspace));
        finish(&mut app, &mut worker);
        let preview = app.take_preview_job().unwrap().run().unwrap();
        assert!(app.handle_preview_result(preview));
        let original = app.editor.document().id();
        std::fs::remove_file(root.path().join("b.txt")).unwrap();
        app.handle(key(KeyCode::Enter));
        assert_eq!(app.editor.document().id(), original);
        assert!(app.picker.active.is_some());
        app.handle(key(KeyCode::Esc));
        assert!(app.picker.active.is_none());
    }

    #[test]
    fn empty_partial_scans_keep_old_rows_without_accepting_them_for_a_new_query() {
        let (root, mut app) = fixture();
        let mut worker = Worker::default();
        app.start_workspace_search(root.path().into(), '/');
        press(&mut app, "needle");
        finish(&mut app, &mut worker);
        let previous = app
            .picker
            .active
            .as_ref()
            .unwrap()
            .view
            .selected()
            .unwrap()
            .clone();
        press(&mut app, "missing");
        app.handle(key(KeyCode::Enter));
        assert!(app.input_waiting());
        let active = app.picker.active.as_ref().unwrap();
        let partial = Result {
            session: active.session,
            revision: active.revision,
            root: root.path().into(),
            items: vec![],
            matched: 0,
            scanned: 0,
            scanning: true,
            notice: String::new(),
        };
        assert!(app.handle_workspace_search_result(partial));
        let active = app.picker.active.as_ref().unwrap();
        assert!(Arc::ptr_eq(active.view.selected().unwrap(), &previous));
        assert!(!active.view.current());
        assert!(app.input_waiting());
        finish(&mut app, &mut worker);
        assert!(!app.input_waiting());
        let view = &app.picker.active.as_ref().unwrap().view;
        assert!(view.items.is_empty() && view.current() && !view.pending);
        assert!(view.preview.text.is_empty() && !view.preview_pending);
        assert_eq!(app.editor.document().text(), "original\n");
    }

    #[test]
    fn accepted_workspace_queries_write_clipboard_registers_after_navigation() {
        let (root, mut app) = fixture();
        app.start_workspace_search(root.path().into(), '+');
        press(&mut app, "needle");
        app.handle(key(KeyCode::Enter));
        finish(&mut app, &mut Worker::default());
        assert_eq!(selected(&app), "needle on disk\n");
        assert!(app.input_waiting());
        let clipboard_path = root.path().join("clipboard");
        std::fs::write(&clipboard_path, "old").unwrap();
        let mut clipboard = crate::clipboard::tests::file_worker(&clipboard_path);
        let copied = clipboard.run(app.take_clipboard_job().unwrap()).unwrap();
        assert!(app.handle_clipboard_result(copied));
        assert!(!app.input_waiting());
        assert_eq!(std::fs::read_to_string(&clipboard_path).unwrap(), "needle");
        press(&mut app, "n");
        let read = clipboard.run(app.take_clipboard_job().unwrap()).unwrap();
        assert!(app.handle_clipboard_result(read));
        while let Some(job) = app.editor.take_search_job() {
            let result = job.run().unwrap();
            app.handle_search_result(result);
        }
        assert_eq!(selected(&app), "needle");
    }

    #[test]
    fn changed_open_buffers_reject_old_hits_and_refresh_without_losing_early_enter() {
        let (root, mut app) = fixture();
        app.start_workspace_search(root.path().into(), '/');
        press(&mut app, "original");
        let mut worker = Worker::default();
        finish(&mut app, &mut worker);
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("new line\n").unwrap();
        app.editor.execute("normal_mode", 1).unwrap();
        app.handle(key(KeyCode::Enter));
        assert!(
            app.picker
                .active
                .as_ref()
                .unwrap()
                .view
                .notice
                .contains("changed since")
        );
        app.refresh_picker_buffer(app.editor.document().id());
        app.handle(key(KeyCode::Enter));
        assert!(app.input_waiting());
        app.refresh_picker_buffer(app.editor.document().id());
        assert!(app.input_waiting());
        finish(&mut app, &mut worker);
        assert!(app.picker.active.is_none());
        assert_eq!(selected(&app), "original\n");
    }
}
