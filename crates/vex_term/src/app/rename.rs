//! Rename prompt and the capture retained from preparation through edit delivery.

use super::{ActivePrompt, App, PromptKind, workspace::Context};
use vex_lsp::{
    RequestKind, WorkspaceDocument,
    workspace_edit::{SynchronizedDocument, WorkspaceEdit},
};

#[derive(Default)]
pub(super) struct State {
    context: Option<Context>,
    request: Option<RequestKind>,
}

impl State {
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

fn documents(context: &Context) -> std::sync::Arc<[WorkspaceDocument]> {
    context
        .documents()
        .map(|(path, snapshot, language)| WorkspaceDocument {
            path: path.into(),
            snapshot: snapshot.clone(),
            language,
        })
        .collect()
}

impl App {
    pub(super) fn prepare_rename_request(&mut self) -> RequestKind {
        let context = self.workspace_edit_context();
        let request = RequestKind::PrepareRename {
            selection: self.editor.selections().primary(),
            documents: documents(&context),
        };
        self.rename.context = Some(context);
        request
    }

    pub(super) fn take_rename_request(&mut self) -> Option<RequestKind> {
        let request = self.rename.request.take()?;
        if !self
            .rename
            .context
            .as_ref()
            .is_some_and(|context| context.current(self))
        {
            self.rename.clear();
            self.fail("rename cancelled: original buffer or selection changed");
            return None;
        }
        Some(request)
    }

    pub(super) fn receive_rename_preparation(&mut self, name: String) {
        let Some(context) = self.rename.context.take() else {
            return;
        };
        if !context.current(self) {
            return;
        }
        let mut prompt = ActivePrompt::command();
        prompt.kind = PromptKind::Rename { context };
        prompt.input.replace(&name);
        self.prompt = Some(prompt);
        self.clear_message();
    }

    pub(super) fn submit_rename(&mut self, context: Context, name: &str) {
        if name.is_empty() {
            self.clear_message();
            return;
        }
        if !context.current(self) {
            self.fail("rename cancelled: original buffer or selection changed");
            return;
        }
        if name.len() > 4096 || name.chars().any(char::is_control) {
            self.fail("rename name must be one line and at most 4096 bytes");
            return;
        }
        self.rename.request = Some(RequestKind::Rename {
            name: name.into(),
            documents: documents(&context),
        });
        self.rename.context = Some(context);
    }

    pub(super) fn receive_rename_edit(
        &mut self,
        edit: WorkspaceEdit,
        versions: Vec<SynchronizedDocument>,
    ) {
        let Some(context) = self.rename.context.take() else {
            return;
        };
        if let Err(error) = self.begin_workspace_edit(context, edit, versions) {
            self.fail(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use vex_lsp::{
        Answer, Event as LspEvent, Position, Range, Update,
        workspace_edit::{DocumentEdit, TextEdit},
    };

    fn key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
        app.handle(Event::Key(KeyEvent::new(code, modifiers)));
    }
    fn press(app: &mut App, text: &str) {
        for ch in text.chars() {
            key(app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
    }
    fn reply(app: &mut App, update: &Update, answer: Result<Answer, String>) -> bool {
        let doc = update.document.as_ref().unwrap();
        app.handle_lsp_event(LspEvent::Answer {
            epoch: doc.epoch,
            revision: doc.snapshot.revision(),
            id: update.request.as_ref().unwrap().id,
            result: answer,
        })
    }
    fn prepare(app: &mut App) -> Update {
        press(app, " r");
        let update = app.take_lsp_update().unwrap();
        assert!(matches!(
            update.request.as_ref().unwrap().kind,
            RequestKind::PrepareRename { .. }
        ));
        assert!(app.input_waiting());
        update
    }
    fn fixture() -> (tempfile::TempDir, App) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.rs");
        std::fs::write(&path, "foo\n").unwrap();
        let mut app = App::open(Some(&path), (100, 24)).unwrap();
        app.enable_lsp();
        app.take_lsp_update();
        (directory, app)
    }

    #[test]
    #[ignore = "manual release-mode rename submission benchmark"]
    fn benchmark_rename_submission() {
        use std::{hint::black_box, time::Instant};
        use vex_core::{CharOffset, Selection, SelectionSet};
        for mib in [1usize, 8] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("buffer.txt");
            std::fs::write(&path, "foo \n".repeat((mib << 20) / 5)).unwrap();
            for count in [1usize, 1000] {
                let mut app = App::open(Some(&path), (100, 24)).unwrap();
                app.editor.set_background_syntax(true);
                app.editor.set_language(Some(vex_editor::Language::Rust));
                app.editor
                    .set_selections(
                        SelectionSet::new(
                            (0..count)
                                .map(|i| Selection::new(CharOffset(i * 5), CharOffset(i * 5 + 1)))
                                .collect(),
                            0,
                        )
                        .unwrap(),
                    )
                    .unwrap();
                app.editor.duplicate_view();
                app.editor.duplicate_view();
                app.enable_lsp();
                app.take_lsp_update();
                let mut samples = Vec::with_capacity(500);
                for _ in 0..500 {
                    let start = Instant::now();
                    app.editor.execute("rename_symbol", 1).unwrap();
                    let update = black_box(app.take_lsp_update().unwrap());
                    samples.push(start.elapsed());
                    app.cancel_language_request();
                    drop(update);
                }
                samples.sort_unstable();
                eprintln!(
                    "rename submit {mib}MiB, {count} selections/view, 3 views: median {:?}, sample p95 {:?}",
                    samples[250], samples[474]
                );
            }
        }
    }

    #[test]
    fn rename_prompt_delivers_workspace_edits_to_active_and_hidden_unsaved_buffers_with_undo() {
        let (directory, mut app) = fixture();
        let main = app.files.target().unwrap().to_path_buf();
        let hidden = directory.path().canonicalize().unwrap().join("hidden.rs");
        std::fs::write(&hidden, "foo\n").unwrap();
        app.open_window_file(&hidden).unwrap();
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("// unsaved\n").unwrap();
        app.editor.execute("normal_mode", 1).unwrap();
        app.open_window_file(&main).unwrap();
        let update = prepare(&mut app);
        assert!(reply(
            &mut app,
            &update,
            Ok(Answer::RenamePrepared("foo".into()))
        ));
        assert!(!app.input_waiting());
        assert_eq!(app.prompt.as_ref().unwrap().prefix(), "rename-to:");
        assert_eq!(app.prompt.as_ref().unwrap().input.text(), "foo");
        key(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        press(&mut app, "bar");
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let update = app.take_lsp_update().unwrap();
        let RequestKind::Rename { name, documents } = &update.request.as_ref().unwrap().kind else {
            panic!("no rename request")
        };
        assert_eq!(name, "bar");
        assert_eq!(documents.len(), 2);
        assert_eq!(
            documents
                .iter()
                .find(|doc| doc.path == hidden)
                .unwrap()
                .snapshot
                .text(),
            "// unsaved\nfoo\n"
        );
        let versions = documents
            .iter()
            .map(|doc| SynchronizedDocument {
                path: doc.path.clone(),
                version: 7,
                document: doc.snapshot.id(),
                revision: doc.snapshot.revision(),
            })
            .collect();
        let edit = WorkspaceEdit {
            documents: documents
                .iter()
                .map(|doc| {
                    let line = u32::from(doc.path == hidden);
                    DocumentEdit {
                        path: doc.path.clone(),
                        version: Some(7),
                        edits: vec![TextEdit {
                            range: Range {
                                start: Position { line, character: 0 },
                                end: Position { line, character: 3 },
                            },
                            new_text: "bar".into(),
                        }],
                    }
                })
                .collect(),
        };
        assert!(reply(
            &mut app,
            &update,
            Ok(Answer::WorkspaceEdit { edit, versions })
        ));
        assert!(app.input_waiting());
        let result = app.take_workspace_edit().unwrap().run().unwrap();
        assert!(app.handle_workspace_edit(result));
        assert!(!app.input_waiting());
        let update = app.take_lsp_workspace_update().unwrap();
        assert_eq!(
            update
                .documents
                .iter()
                .find(|doc| doc.path == hidden)
                .unwrap()
                .snapshot
                .text(),
            "// unsaved\nbar\n"
        );
        assert!(app.take_lsp_workspace_update().is_none());
        assert_eq!(app.editor.document().text(), "bar\n");
        assert_eq!(
            app.snapshot_for_path(&hidden).unwrap().text(),
            "// unsaved\nbar\n"
        );
        assert_eq!(std::fs::read_to_string(&main).unwrap(), "foo\n");
        assert_eq!(std::fs::read_to_string(&hidden).unwrap(), "foo\n");
        app.editor.execute("undo", 1).unwrap();
        assert_eq!(app.editor.document().text(), "foo\n");
        app.open_window_file(&hidden).unwrap();
        app.editor.execute("undo", 1).unwrap();
        assert_eq!(app.editor.document().text(), "// unsaved\nfoo\n");
    }

    #[test]
    fn rename_cancellation_stale_prompts_empty_names_and_server_failures_never_edit_or_borrow_command_history()
     {
        let (_directory, mut app) = fixture();
        app.prompt_history.push(':', "write");
        let update = prepare(&mut app);
        key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(update.request.as_ref().unwrap().cancellation.is_cancelled());
        assert!(!reply(
            &mut app,
            &update,
            Ok(Answer::RenamePrepared("foo".into()))
        ));
        assert!(app.prompt.is_none());
        for cancel in [false, true] {
            let update = prepare(&mut app);
            reply(&mut app, &update, Ok(Answer::RenamePrepared("foo".into())));
            key(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
            key(&mut app, KeyCode::Up, KeyModifiers::NONE);
            assert_eq!(app.prompt.as_ref().unwrap().input.text(), "");
            key(
                &mut app,
                if cancel { KeyCode::Esc } else { KeyCode::Enter },
                KeyModifiers::NONE,
            );
            assert!(
                app.take_lsp_update()
                    .is_none_or(|update| update.request.is_none())
            );
            assert!(app.prompt.is_none());
            assert_eq!(app.prompt_history.last(':'), Some("write".into()));
        }
        let update = prepare(&mut app);
        reply(&mut app, &update, Err("cannot rename".into()));
        assert!(!app.input_waiting());
        assert!(app.error);
        assert!(app.prompt.is_none());
        let update = prepare(&mut app);
        reply(&mut app, &update, Ok(Answer::RenamePrepared("foo".into())));
        // A file reload or programmatic selection change can happen while the
        // prompt is editable; its original capture must not be silently replaced.
        app.editor.execute("move_right", 1).unwrap();
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(
            app.take_lsp_update()
                .is_none_or(|update| update.request.is_none())
        );
        assert!(app.error);
        assert!(app.message.contains("original buffer or selection changed"));
        assert_eq!(app.editor.document().text(), "foo\n");
        assert!(!app.is_dirty());
    }
}
