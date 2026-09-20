//! Preparation and atomic delivery of text edits across retained buffers.
//! Language operations capture Context before sending their server request.

use super::{App, jumps::Jump};
use crate::files::FileState;
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::PathBuf,
    sync::Arc,
};
use vex_core::Document;
use vex_editor::{
    ExternalEditPlan, Language, Mode, PreparedExternalEdit, background::Cancellation,
};
use vex_lsp::workspace_edit::{self, SynchronizedDocument, WorkspaceEdit};

pub(super) struct CapturedDocument {
    pub path: PathBuf,
    pub plan: ExternalEditPlan,
    pub dirty: bool,
    pub language: Option<Language>,
}

/// Immutable origin and buffer views for a single language operation.
#[derive(Clone)]
pub struct Context {
    origin: Jump,
    window: u64,
    mode: Mode,
    pub(super) documents: Arc<[CapturedDocument]>,
}

impl Context {
    /// Shared named snapshots for synchronization before a workspace request.
    pub fn documents(
        &self,
    ) -> impl Iterator<Item = (&std::path::Path, &vex_core::Snapshot, Option<Language>)> {
        self.documents.iter().map(|document| {
            (
                document.path.as_path(),
                document.plan.snapshot(),
                document.language,
            )
        })
    }

    pub(super) fn current(&self, app: &App) -> bool {
        self.origin.document == app.editor.document().id()
            && self.origin.bookmark.revision() == app.editor.document().revision()
            && self.origin.selections.as_ref() == app.editor.selections()
            && self.window == app.focused_window_id()
            && self.mode == app.editor.mode()
    }
}

#[derive(Default)]
pub(super) struct State {
    pending: Option<(Context, Cancellation)>,
    job: Option<Job>,
    pub(super) synchronize: bool,
}

impl Drop for State {
    fn drop(&mut self) {
        if let Some((_, token)) = &self.pending {
            token.cancel();
        }
    }
}

pub(crate) struct Job {
    context: Context,
    edit: WorkspaceEdit,
    versions: Vec<SynchronizedDocument>,
    pub cancellation: Cancellation,
}

pub(super) struct Change {
    pub path: PathBuf,
    pub new_file: Option<(Document, FileState)>,
    pub edit: PreparedExternalEdit,
}

pub(crate) struct Result {
    changes: io::Result<Vec<Change>>,
    cancellation: Cancellation,
}

impl Job {
    pub fn run(self) -> Option<Result> {
        let changes = (|| {
            if self.edit.documents.len() > 4096 {
                return Err(io::Error::other("workspace edit document limit exceeded"));
            }
            let open: BTreeMap<_, _> = self
                .context
                .documents
                .iter()
                .map(|document| (document.path.as_path(), document))
                .collect();
            let versions: BTreeMap<_, _> = self
                .versions
                .iter()
                .map(|version| (version.path.as_path(), version))
                .collect();
            let mut seen = BTreeSet::new();
            let mut changes = Vec::new();
            for document in self.edit.documents {
                if self.cancellation.is_cancelled() {
                    return Err(io::Error::other("workspace edit cancelled"));
                }
                let path = crate::files::resolve(&document.path)?;
                if !seen.insert(path.clone()) {
                    return Err(io::Error::other(
                        "workspace edit targets the same file through multiple aliases",
                    ));
                }
                let version = versions.get(path.as_path());
                let (plan, new_file) = if let Some(open) = open.get(path.as_path()) {
                    if let Some(version) = version {
                        if version.document != open.plan.snapshot().id()
                            || version.revision != open.plan.snapshot().revision()
                        {
                            return Err(io::Error::other(
                                "language server snapshot is stale for an open buffer",
                            ));
                        }
                    } else {
                        if open.dirty {
                            return Err(io::Error::other(format!(
                                "language server has not synchronized unsaved buffer {}",
                                path.display()
                            )));
                        }
                        if !std::fs::metadata(&path)?.is_file() {
                            return Err(io::Error::other(
                                "workspace buffer's disk file is no longer a regular file",
                            ));
                        }
                        let (disk, _) = FileState::load_with_cancel(Some(&path), || {
                            self.cancellation.is_cancelled()
                        })?;
                        if disk.text() != open.plan.snapshot().text() {
                            return Err(io::Error::other(
                                "disk content changed since the buffer was loaded",
                            ));
                        }
                    }
                    (open.plan.clone(), None)
                } else {
                    if version.is_some() {
                        return Err(io::Error::other(
                            "synchronized workspace buffer was not captured",
                        ));
                    }
                    if !std::fs::metadata(&path)?.is_file() {
                        return Err(io::Error::other(
                            "workspace edit requires an existing regular file",
                        ));
                    }
                    let (document, files) = FileState::load_with_cancel(Some(&path), || {
                        self.cancellation.is_cancelled()
                    })?;
                    (
                        ExternalEditPlan::new_document(&document),
                        Some((document, files)),
                    )
                };
                let transaction = workspace_edit::transaction(
                    &document,
                    plan.snapshot(),
                    version.map(|version| version.version),
                    &self.cancellation,
                )
                .map_err(io::Error::other)?;
                let edit = plan
                    .prepare(transaction, &self.cancellation)
                    .map_err(io::Error::other)?;
                if !edit.is_empty() {
                    changes.push(Change {
                        path,
                        new_file,
                        edit,
                    });
                }
            }
            Ok(changes)
        })();
        (!self.cancellation.is_cancelled()).then_some(Result {
            changes,
            cancellation: self.cancellation,
        })
    }
}

impl App {
    pub fn workspace_edit_context(&self) -> Context {
        Context {
            origin: self.current_jump(),
            window: self.focused_window_id(),
            mode: self.editor.mode(),
            documents: self.capture_workspace_buffers(),
        }
    }

    /// Queue a response to an explicitly requested language operation. Protocol
    /// adapters supply the versions they synchronized before making the request.
    pub fn begin_workspace_edit(
        &mut self,
        context: Context,
        edit: WorkspaceEdit,
        versions: Vec<SynchronizedDocument>,
    ) -> io::Result<()> {
        if !context.current(self) {
            return Err(io::Error::other("workspace edit origin changed"));
        }
        if self.input_waiting() {
            return Err(io::Error::other(
                "another editor operation is still pending",
            ));
        }
        self.close_picker();
        self.prompt = None;
        self.cancel_workspace_edit();
        let cancellation = Cancellation::default();
        self.workspace.pending = Some((context.clone(), cancellation.clone()));
        self.workspace.job = Some(Job {
            context,
            edit,
            versions,
            cancellation,
        });
        self.message = "preparing workspace edits…".into();
        Ok(())
    }

    pub(super) fn cancel_workspace_edit(&mut self) {
        if let Some((_, token)) = self.workspace.pending.take() {
            token.cancel();
        }
        self.workspace.job = None;
    }

    pub(super) fn workspace_edit_waiting(&self) -> bool {
        self.workspace.pending.is_some()
    }
    pub(crate) fn take_workspace_edit(&mut self) -> Option<Job> {
        self.workspace.job.take()
    }

    pub(crate) fn handle_workspace_edit(&mut self, result: Result) -> bool {
        let Some((_, token)) = &self.workspace.pending else {
            return false;
        };
        if !token.same_request(&result.cancellation) {
            return false;
        }
        let (context, token) = self.workspace.pending.take().unwrap();
        if token.is_cancelled() || !context.current(self) {
            token.cancel();
            return true;
        }
        token.cancel();
        match result
            .changes
            .and_then(|changes| self.commit_workspace_edit(changes))
        {
            Ok(count) => {
                self.workspace.synchronize |= count > 0;
                self.dismiss_language_help();
                self.invalidate_completion();
                self.refresh_git();
                self.refresh_status();
                self.observe_buffer_revision();
                self.message = if count == 0 {
                    "no workspace changes".into()
                } else {
                    format!("updated {count} buffer(s) · changes are unsaved")
                };
            }
            Err(error) => self.fail(error),
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vex_core::CharOffset;
    use vex_lsp::{
        Position, Range,
        workspace_edit::{DocumentEdit, TextEdit},
    };

    fn fixture() -> (tempfile::TempDir, App, [PathBuf; 3]) {
        let directory = tempfile::tempdir().unwrap();
        let paths = ["a.rs", "b.rs", "c.rs"]
            .map(|name| directory.path().canonicalize().unwrap().join(name));
        for path in &paths {
            std::fs::write(path, "foo\n").unwrap();
        }
        let mut app = App::open(Some(&paths[0]), (80, 24)).unwrap();
        app.open_window_file(&paths[1]).unwrap();
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("// unsaved\n").unwrap();
        app.editor.execute("normal_mode", 1).unwrap();
        app.open_window_file(&paths[0]).unwrap();
        (directory, app, paths)
    }

    fn edits(paths: &[PathBuf; 3]) -> WorkspaceEdit {
        WorkspaceEdit {
            documents: paths
                .iter()
                .enumerate()
                .map(|(index, path)| {
                    let line = u32::from(index == 1);
                    DocumentEdit {
                        path: path.clone(),
                        version: None,
                        edits: vec![TextEdit {
                            range: Range {
                                start: Position { line, character: 0 },
                                end: Position { line, character: 3 },
                            },
                            new_text: ["alpha", "beta", "gamma"][index].into(),
                        }],
                    }
                })
                .collect(),
        }
    }

    fn versions(context: &Context) -> Vec<SynchronizedDocument> {
        context
            .documents()
            .map(|(path, snapshot, _)| SynchronizedDocument {
                path: path.into(),
                version: 7,
                document: snapshot.id(),
                revision: snapshot.revision(),
            })
            .collect()
    }

    #[test]
    fn batches_change_open_hidden_and_unopened_buffers_with_independent_undo_and_no_disk_writes() {
        let (_dir, mut app, paths) = fixture();
        let origin = app.editor.document().id();
        let context = app.workspace_edit_context();
        let versions = versions(&context);
        app.begin_workspace_edit(context, edits(&paths), versions)
            .unwrap();
        assert!(app.input_waiting());
        let result = app.take_workspace_edit().unwrap().run().unwrap();
        assert_eq!(app.editor.document().text(), "foo\n");
        assert!(app.handle_workspace_edit(result));
        assert!(!app.input_waiting());
        assert_eq!(app.editor.document().id(), origin);
        assert_eq!(app.editor.document().text(), "alpha\n");
        assert!(app.is_dirty());
        assert_eq!(
            app.snapshot_for_path(&paths[1]).unwrap().text(),
            "// unsaved\nbeta\n"
        );
        assert_eq!(app.snapshot_for_path(&paths[2]).unwrap().text(), "gamma\n");
        for path in &paths {
            assert_eq!(std::fs::read_to_string(path).unwrap(), "foo\n");
        }
        app.editor.execute("undo", 1).unwrap();
        assert_eq!(app.editor.document().text(), "foo\n");
        app.open_window_file(&paths[1]).unwrap();
        app.editor.execute("undo", 1).unwrap();
        assert_eq!(app.editor.document().text(), "// unsaved\nfoo\n");
        app.editor.execute("undo", 1).unwrap();
        assert_eq!(app.editor.document().text(), "foo\n");
        app.open_window_file(&paths[2]).unwrap();
        app.editor.execute("undo", 1).unwrap();
        assert_eq!(app.editor.document().text(), "foo\n");
        assert!(!app.is_dirty());
    }

    #[test]
    fn late_changes_in_any_destination_reject_the_entire_batch_before_catalog_or_history_changes() {
        let (_dir, mut app, paths) = fixture();
        let context = app.workspace_edit_context();
        let before = app.editor.document().snapshot();
        let versions = versions(&context);
        app.begin_workspace_edit(context, edits(&paths), versions)
            .unwrap();
        let result = app.take_workspace_edit().unwrap().run().unwrap();
        let hidden = app.snapshot_for_path(&paths[1]).unwrap().id();
        app.with_file_buffer_mut(hidden, |editor, _, _| {
            editor.execute("insert_mode", 1).unwrap();
            editor.insert_text("later").unwrap();
        });
        assert!(app.handle_workspace_edit(result));
        assert_eq!(app.editor.document().revision(), before.revision());
        assert!(app.editor.document().text().is_instance(before.text()));
        assert_eq!(app.editor.document().undo_depth(), 0);
        assert!(app.snapshot_for_path(&paths[2]).is_none());
        assert!(
            app.snapshot_for_path(&paths[1])
                .unwrap()
                .text()
                .to_string()
                .contains("later")
        );
        assert!(!app.input_waiting());
    }

    #[test]
    fn cancelled_unknown_version_and_unsynchronized_dirty_buffers_leave_every_file_unchanged() {
        let (_dir, mut app, paths) = fixture();
        let context = app.workspace_edit_context();
        app.begin_workspace_edit(context.clone(), edits(&paths), Vec::new())
            .unwrap();
        let result = app.take_workspace_edit().unwrap().run().unwrap();
        assert!(app.handle_workspace_edit(result));
        assert!(app.message.contains("not synchronized unsaved buffer"));
        assert_eq!(app.editor.document().text(), "foo\n");
        assert!(app.snapshot_for_path(&paths[2]).is_none());
        let mut edit = edits(&paths);
        edit.documents[0].version = Some(99);
        app.begin_workspace_edit(context.clone(), edit, versions(&context))
            .unwrap();
        let result = app.take_workspace_edit().unwrap().run().unwrap();
        app.handle_workspace_edit(result);
        assert!(app.message.contains("version"));
        app.begin_workspace_edit(context.clone(), edits(&paths), versions(&context))
            .unwrap();
        let job = app.take_workspace_edit().unwrap();
        app.cancel_workspace_edit();
        assert!(job.run().is_none());
        app.begin_workspace_edit(context.clone(), edits(&paths), versions(&context))
            .unwrap();
        let result = app.take_workspace_edit().unwrap().run().unwrap();
        app.cancel_workspace_edit();
        assert!(!app.handle_workspace_edit(result));
        assert_eq!(app.editor.document().text(), "foo\n");
        assert_eq!(app.editor.selections().primary().head, CharOffset(1));
    }
}
