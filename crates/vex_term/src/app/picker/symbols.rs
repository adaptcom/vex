//! LSP symbol picker lifecycle. Catalogs and query generations remain separate:
//! typing in a document picker never discards an outstanding outline request.

use super::*;
use crate::picker::symbols::SymbolResult;
use std::time::{Duration, Instant};
use vex_core::{DocumentId, Revision, SelectionSet};
use vex_editor::{Language, Mode};
use vex_lsp::{RequestKind, Symbols};

pub(super) struct Source {
    workspace: bool,
    document: DocumentId,
    revision: Revision,
    language: Option<Language>,
    path: Option<PathBuf>,
    selections: SelectionSet,
    mode: Mode,
    catalog: Option<Arc<Symbols>>,
    due: Option<Instant>,
    loading: bool,
    error: Option<String>,
}

impl App {
    pub(super) fn resume_symbol_picker(&mut self) {
        let Some(active) = &mut self.picker.active else {
            return;
        };
        let super::Source::Symbols(source) = &mut active.source else {
            return;
        };
        // A retained catalog remains useful after accepting a result moved the
        // editor. Future requests and cancellation track the current context.
        source.document = self.editor.document().id();
        source.revision = self.editor.document().revision();
        source.language = self.editor.language();
        source.path = self.files.target().map(PathBuf::from);
        source.selections = self.editor.selections().clone();
        source.mode = self.editor.mode();
        source.loading = false;
        source.due = source.catalog.is_none().then(Instant::now);
        if source.catalog.is_some() {
            self.rank_symbols();
        }
    }

    pub(in crate::app) fn symbol_picker_active(&self) -> bool {
        self.picker
            .active
            .as_ref()
            .is_some_and(|active| matches!(active.source, super::Source::Symbols(_)))
    }

    pub(in crate::app) fn open_symbol_picker(&mut self, workspace: bool) {
        self.close_picker();
        self.dismiss_language_help();
        self.keys.cancel(&mut self.editor);
        self.prompt = None;
        if self.editor.search_prompt().is_some() {
            let _ = self.editor.execute("search_cancel", 1);
        }
        self.clear_message();
        let mut view = Picker::new(
            if workspace {
                "Workspace symbols"
            } else {
                "Document symbols"
            }
            .into(),
        );
        view.noun = "symbols";
        self.picker.next_session += 1;
        self.picker.active = Some(Active {
            view,
            session: self.picker.next_session,
            revision: 0,
            source: super::Source::Symbols(Source {
                workspace,
                document: self.editor.document().id(),
                revision: self.editor.document().revision(),
                language: self.editor.language(),
                path: self.files.target().map(PathBuf::from),
                selections: self.editor.selections().clone(),
                mode: self.editor.mode(),
                catalog: None,
                due: Some(Instant::now()),
                loading: false,
                error: None,
            }),
            cancellation: Cancellation::default(),
            preview_cancel: Cancellation::default(),
            preview_request: 0,
            preview_target: None,
            accept_pending: false,
        });
    }

    pub(in crate::app) fn invalidate_symbol_picker(&mut self) {
        let stale = self.picker.active.as_ref().is_some_and(|active| {
            let super::Source::Symbols(source) = &active.source else {
                return false;
            };
            source.document != self.editor.document().id()
                || source.revision != self.editor.document().revision()
                || source.language != self.editor.language()
                || source.path.as_deref() != self.files.target()
                || source.selections != *self.editor.selections()
                || source.mode != self.editor.mode()
        });
        if stale {
            self.close_picker();
        }
    }

    pub(crate) fn symbol_deadline(&self) -> Option<Instant> {
        let active = self.picker.active.as_ref()?;
        let super::Source::Symbols(source) = &active.source else {
            return None;
        };
        source.due
    }

    pub(in crate::app) fn take_symbol_request(&mut self, now: Instant) -> Option<RequestKind> {
        let active = self.picker.active.as_mut()?;
        let super::Source::Symbols(source) = &mut active.source else {
            return None;
        };
        if source.due.is_none_or(|due| due > now) {
            return None;
        }
        source.due = None;
        source.loading = true;
        Some(if source.workspace {
            RequestKind::WorkspaceSymbols(active.view.query.text().into())
        } else {
            RequestKind::DocumentSymbols
        })
    }

    pub(super) fn submit_symbol_query(&mut self) {
        let active = self.picker.active.as_mut().unwrap();
        let super::Source::Symbols(source) = &mut active.source else {
            unreachable!()
        };
        active.cancellation.cancel();
        active.cancellation = Cancellation::default();
        active.preview_cancel.cancel();
        active.preview_target = None;
        active.accept_pending = false;
        active.revision += 1;
        self.picker.preview_job = None;
        self.picker.symbol_job = None;
        if source.workspace {
            source.catalog = None;
            source.loading = false;
            source.error = None;
            source.due = Some(Instant::now() + Duration::from_millis(150));
            self.cancel_language_request();
        } else if source.catalog.is_some() {
            self.rank_symbols();
        } else if let Some(error) = &source.error {
            active.view.pending = false;
            active.view.notice = error.clone();
        }
    }

    fn rank_symbols(&mut self) {
        let active = self.picker.active.as_ref().unwrap();
        let super::Source::Symbols(source) = &active.source else {
            return;
        };
        let Some(symbols) = source.catalog.clone() else {
            return;
        };
        self.picker.symbol_job = Some(SymbolJob {
            session: active.session,
            revision: active.revision,
            symbols,
            query: active.view.query.text().into(),
            workspace: source.workspace,
            cancellation: active.cancellation.clone(),
        });
    }

    pub(in crate::app) fn receive_symbols(&mut self, symbols: Symbols) {
        let Some(active) = &mut self.picker.active else {
            return;
        };
        let super::Source::Symbols(source) = &mut active.source else {
            return;
        };
        if !source.loading {
            return;
        }
        source.loading = false;
        source.catalog = Some(Arc::new(symbols));
        self.clear_message();
        self.rank_symbols();
    }

    pub(in crate::app) fn fail_symbol_picker(&mut self, error: &str) -> bool {
        let Some(active) = &mut self.picker.active else {
            return false;
        };
        let super::Source::Symbols(source) = &mut active.source else {
            return false;
        };
        active.cancellation.cancel();
        active.accept_pending = false;
        active.view.pending = false;
        active.view.notice = format!("{error} · Esc close");
        source.error = Some(active.view.notice.clone());
        source.loading = false;
        source.due = None;
        self.picker.symbol_job = None;
        true
    }

    pub(crate) fn handle_symbol_result(&mut self, result: SymbolResult) -> bool {
        self.invalidate_symbol_picker();
        let Some(active) = &mut self.picker.active else {
            return false;
        };
        if !matches!(active.source, super::Source::Symbols(_))
            || result.session != active.session
            || result.revision != active.revision
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
                        value: Target::Symbol(item.entry.value.clone()),
                    }),
                    matched: item.matched,
                })
                .collect(),
        );
        active.view.matched = result.matched;
        active.view.total = result.total;
        active.view.pending = false;
        active.view.notice = if result.limited {
            "Symbol limit reached · narrow the query".into()
        } else {
            String::new()
        };
        let accept = active.accept_pending;
        active.accept_pending = false;
        if accept {
            self.accept_picker();
        } else {
            self.request_picker_preview();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use vex_lsp::{Answer, Event as LspEvent, Location, Position, Symbol, Update};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }
    fn press(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.handle(key(KeyCode::Char(ch)));
        }
    }
    fn fixture() -> (tempfile::TempDir, App) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.rs");
        std::fs::write(&path, "// 🦀\nfn alpha() {}\nfn beta() {}\n").unwrap();
        let mut app = App::open(Some(&path), (140, 24)).unwrap();
        app.enable_lsp();
        app.take_lsp_update();
        (directory, app)
    }
    fn symbol(path: PathBuf, name: &str, line: u32) -> Symbol {
        Symbol {
            name: name.into(),
            container: String::new(),
            kind: 12,
            location: Location {
                path,
                position: Position { line, character: 3 },
            },
        }
    }
    fn answer(app: &mut App, request: &Update, result: Result<Symbols, String>) -> bool {
        let document = request.document.as_ref().unwrap();
        app.handle_lsp_event(LspEvent::Answer {
            epoch: document.epoch,
            revision: document.snapshot.revision(),
            id: request.request.as_ref().unwrap().id,
            result: result.map(Answer::Symbols),
        })
    }
    fn catalog(app: &App) -> Symbols {
        let path = app.files.target().unwrap().to_path_buf();
        Symbols {
            items: vec![symbol(path.clone(), "alpha", 1), symbol(path, "beta", 2)],
            limited: false,
        }
    }
    fn finish(app: &mut App) {
        let result = app.take_symbol_job().unwrap().run().unwrap();
        assert!(app.handle_symbol_result(result));
    }
    fn request_due(app: &mut App) -> Update {
        let super::super::Source::Symbols(source) = &mut app.picker.active.as_mut().unwrap().source
        else {
            panic!()
        };
        source.due = Some(Instant::now());
        app.take_lsp_update().unwrap()
    }

    #[test]
    fn last_symbol_picker_keeps_query_and_selection_after_accepting_a_symbol() {
        let (_directory, mut app) = fixture();
        press(&mut app, " s");
        let request = app.take_lsp_update().unwrap();
        let symbols = catalog(&app);
        assert!(answer(&mut app, &request, Ok(symbols)));
        finish(&mut app);
        press(&mut app, "bet");
        finish(&mut app);
        let wanted = app
            .picker
            .active
            .as_ref()
            .unwrap()
            .view
            .selected()
            .unwrap()
            .value
            .clone();
        app.handle(key(KeyCode::Enter));
        assert!(app.picker.active.is_none());
        press(&mut app, " '");
        finish(&mut app);
        assert_eq!(app.picker.active.as_ref().unwrap().view.query.text(), "bet");
        assert_eq!(
            app.picker
                .active
                .as_ref()
                .unwrap()
                .view
                .selected()
                .unwrap()
                .value,
            wanted
        );
        app.handle(key(KeyCode::Enter));
        assert!(app.picker.active.is_none());
    }

    #[test]
    fn document_query_during_loading_uses_latest_filter_and_early_enter_jumps_back() {
        let (_directory, mut app) = fixture();
        press(&mut app, " s");
        let request = app.take_lsp_update().unwrap();
        assert!(matches!(
            request.request.as_ref().unwrap().kind,
            RequestKind::DocumentSymbols
        ));
        press(&mut app, "bet");
        assert!(app.take_lsp_update().is_none());
        app.handle(key(KeyCode::Enter));
        assert!(app.input_waiting());
        let symbols = catalog(&app);
        assert!(answer(&mut app, &request, Ok(symbols)));
        finish(&mut app);
        assert!(!app.input_waiting());
        assert!(app.picker.active.is_none());
        assert_eq!(
            vex_lsp::position(
                app.editor.document().text(),
                vex_core::motion::cursor(
                    app.editor.document().text(),
                    app.editor.selections().primary()
                )
                .unwrap()
            )
            .unwrap(),
            Position {
                line: 2,
                character: 3
            }
        );
        app.execute("jump_back").unwrap();
        app.take_lsp_update();
        assert_eq!(app.editor.selections().primary().start().0, 0);
    }

    #[test]
    fn document_picker_reuses_catalog_and_cancel_preserves_select_mode_and_unsaved_text() {
        let (_directory, mut app) = fixture();
        press(&mut app, "i// unsaved");
        app.handle(key(KeyCode::Esc));
        press(&mut app, "vl s");
        let selections = app.editor.selections().clone();
        let snapshot = app.editor.document().snapshot();
        let request = app.take_lsp_update().unwrap();
        let symbols = catalog(&app);
        assert!(answer(&mut app, &request, Ok(symbols)));
        finish(&mut app);
        let preview = app.take_preview_job().unwrap().run().unwrap();
        assert!(preview.preview.text.contains("unsaved"));
        assert!(!preview.preview.highlights.is_empty());
        assert!(app.handle_preview_result(preview));
        press(&mut app, "beta");
        assert!(app.take_lsp_update().is_none());
        finish(&mut app);
        assert_eq!(app.picker.active.as_ref().unwrap().view.items.len(), 1);
        let mut frame = Frame::default();
        frame.reset(140, 24).unwrap();
        app.paint(&mut frame).unwrap();
        assert!((0..24).any(|row| frame.row_text(row).contains("Document symbols")));
        assert!((0..24).any(|row| frame.row_text(row).contains("beta  [function]")));
        app.handle(key(KeyCode::Esc));
        assert_eq!(app.editor.mode(), Mode::Select);
        assert_eq!(app.editor.selections(), &selections);
        assert_eq!(app.editor.document().revision(), snapshot.revision());
        assert!(app.is_dirty());
    }

    #[test]
    fn workspace_queries_debounce_and_reject_old_replies_rankings_and_previews() {
        let (_directory, mut app) = fixture();
        press(&mut app, " S");
        let old = app.take_lsp_update().unwrap();
        press(&mut app, "al");
        assert!(old.request.as_ref().unwrap().cancellation.is_cancelled());
        assert!(app.take_lsp_update().unwrap().request.is_none());
        let due = app.symbol_deadline().unwrap();
        assert!(
            app.take_symbol_request(due - Duration::from_millis(1))
                .is_none()
        );
        let request = request_due(&mut app);
        assert!(
            matches!(&request.request.as_ref().unwrap().kind, RequestKind::WorkspaceSymbols(query) if query == "al")
        );
        assert!(!answer(&mut app, &old, Ok(Symbols::default())));
        let symbols = catalog(&app);
        assert!(answer(&mut app, &request, Ok(symbols)));
        let ranking = app.take_symbol_job().unwrap().run().unwrap();
        press(&mut app, "p");
        assert!(!app.handle_symbol_result(ranking));
        let request = request_due(&mut app);
        let symbols = catalog(&app);
        assert!(answer(&mut app, &request, Ok(symbols)));
        finish(&mut app);
        let preview = app.take_preview_job().unwrap().run().unwrap();
        app.handle(key(KeyCode::Down));
        assert!(!app.handle_preview_result(preview)); // Different symbol, same file.
        app.handle(key(KeyCode::Esc));
        assert!(app.symbol_deadline().is_none());
        assert!(app.take_symbol_job().is_none());
    }

    #[test]
    fn workspace_jumps_protect_dirty_buffers_and_reuse_open_buffers() {
        let (directory, mut app) = fixture();
        let path = directory.path().canonicalize().unwrap().join("other.rs");
        std::fs::write(&path, "fn target() {}\n").unwrap();
        press(&mut app, "ix");
        app.handle(key(KeyCode::Esc));
        app.execute("workspace_symbol_picker").unwrap();
        let request = app.take_lsp_update().unwrap();
        assert!(answer(
            &mut app,
            &request,
            Ok(Symbols {
                items: vec![symbol(path.clone(), "target", 0)],
                limited: false
            })
        ));
        finish(&mut app);
        app.handle(key(KeyCode::Enter));
        assert!(app.picker.active.is_none());
        assert_eq!(app.files.target(), Some(path.as_path()));
        app.execute("jump_back").unwrap();
        app.take_lsp_update();
        assert!(app.is_dirty());
        app.execute("workspace_symbol_picker").unwrap();
        let request = app.take_lsp_update().unwrap();
        assert!(answer(
            &mut app,
            &request,
            Ok(Symbols {
                items: vec![symbol(path.clone(), "target", 0)],
                limited: false
            })
        ));
        finish(&mut app);
        app.handle(key(KeyCode::Enter));
        assert_eq!(app.files.target(), Some(path.as_path()));
        app.execute("jump_back").unwrap();
        app.take_lsp_update();
        assert!(app.is_dirty());
        assert!(app.editor.document().text().to_string().starts_with('x'));
    }

    #[test]
    fn empty_errors_focus_loss_and_external_changes_release_pending_acceptance() {
        let (_directory, mut app) = fixture();
        for result in [Ok(Symbols::default()), Err("unsupported symbols".into())] {
            app.execute("symbol_picker").unwrap();
            let request = app.take_lsp_update().unwrap();
            app.handle(key(KeyCode::Enter));
            assert!(app.input_waiting());
            let success = result.is_ok();
            assert!(answer(&mut app, &request, result));
            if success {
                finish(&mut app);
            }
            assert!(!app.input_waiting());
            assert!(!app.picker.active.as_ref().unwrap().view.pending);
            app.handle(key(KeyCode::Esc));
        }
        app.execute("symbol_picker").unwrap();
        let request = app.take_lsp_update().unwrap();
        app.handle(key(KeyCode::Enter));
        app.handle(Event::FocusLost);
        assert!(!app.input_waiting());
        assert!(
            request
                .request
                .as_ref()
                .unwrap()
                .cancellation
                .is_cancelled()
        );
        assert!(!answer(&mut app, &request, Ok(Symbols::default())));
        app.execute("symbol_picker").unwrap();
        let request = app.take_lsp_update().unwrap();
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("changed").unwrap();
        app.take_lsp_update();
        assert!(app.picker.active.is_none());
        assert!(!answer(&mut app, &request, Ok(Symbols::default())));
        let mut scratch = App::from_document(vex_core::Document::from(""), (80, 24));
        scratch.enable_lsp();
        scratch.execute("symbol_picker").unwrap();
        scratch.take_lsp_update();
        assert!(
            scratch
                .picker
                .active
                .as_ref()
                .unwrap()
                .view
                .notice
                .contains("named file")
        );
        assert!(!scratch.picker.active.as_ref().unwrap().view.pending);
    }
}
