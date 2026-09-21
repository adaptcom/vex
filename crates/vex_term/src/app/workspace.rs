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
    ExternalEditPlan, Indentation, Language, Mode, PreparedExternalEdit, background::Cancellation,
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
    settings: Option<(Option<Language>, Indentation)>,
    pub(super) documents: Arc<[CapturedDocument]>,
}

impl Context {
    pub(super) fn lsp_documents(&self) -> Arc<[vex_lsp::WorkspaceDocument]> {
        self.documents()
            .map(|(path, snapshot, language)| vex_lsp::WorkspaceDocument {
                path: path.into(),
                snapshot: snapshot.clone(),
                language,
            })
            .collect()
    }
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
            && self.settings.is_none_or(|(language, indentation)| {
                language == app.editor.language() && indentation == app.editor.indentation()
            })
    }
}

#[derive(Default)]
pub(super) struct State {
    pending: Option<Pending>,
    job: Option<Job>,
    pub(super) synchronize: bool,
}

struct Pending {
    context: Context,
    cancellation: Cancellation,
    server: Option<ServerEdit>,
    command: Option<vex_lsp::ServerCommand>,
}

struct ServerEdit {
    epoch: u64,
    request: u64,
    reply: vex_lsp::ApplyReply,
}

impl Drop for State {
    fn drop(&mut self) {
        if let Some(pending) = &self.pending {
            pending.cancellation.cancel();
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
                                crate::paths::display(&path)
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
            settings: None,
        }
    }

    /// Capture just the active document and all of its views for edits that
    /// cannot target another file (formatting). Hidden buffers are not visited.
    pub(super) fn document_edit_context(&self) -> Context {
        Context {
            origin: self.current_jump(),
            window: self.focused_window_id(),
            mode: self.editor.mode(),
            settings: Some((self.editor.language(), self.editor.indentation())),
            documents: self
                .files
                .target()
                .map(|path| CapturedDocument {
                    path: path.into(),
                    plan: self.editor.external_edit_plan(),
                    dirty: self.is_dirty(),
                    language: self.editor.language(),
                })
                .into_iter()
                .collect(),
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
        self.queue_workspace_edit(context, edit, versions, None, None)
    }

    pub(super) fn begin_code_action_edit(
        &mut self,
        context: Context,
        edit: WorkspaceEdit,
        versions: Vec<SynchronizedDocument>,
        command: Option<vex_lsp::ServerCommand>,
    ) -> io::Result<()> {
        self.queue_workspace_edit(context, edit, versions, None, command)
    }

    fn queue_workspace_edit(
        &mut self,
        context: Context,
        edit: WorkspaceEdit,
        versions: Vec<SynchronizedDocument>,
        server: Option<ServerEdit>,
        command: Option<vex_lsp::ServerCommand>,
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
        let cancellation = server
            .as_ref()
            .map_or_else(Cancellation::default, |server| server.reply.cancellation());
        self.workspace.pending = Some(Pending {
            context: context.clone(),
            cancellation: cancellation.clone(),
            server,
            command,
        });
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
        if let Some(pending) = self.workspace.pending.take() {
            pending.cancellation.cancel();
            if pending.server.is_some() {
                self.cancel_language_request();
            }
        }
        self.workspace.job = None;
    }

    pub(super) fn workspace_edit_waiting(&self) -> bool {
        self.workspace.pending.is_some()
    }
    pub(crate) fn take_workspace_edit(&mut self) -> Option<Job> {
        self.workspace.job.take()
    }

    pub(super) fn receive_server_edit(
        &mut self,
        epoch: u64,
        request: u64,
        edit: WorkspaceEdit,
        versions: Vec<SynchronizedDocument>,
        reply: vex_lsp::ApplyReply,
    ) -> bool {
        if !self.command_current(epoch, request) {
            reply.finish(Err("language server command is no longer current".into()));
            return false;
        }
        self.command_waiting(false);
        let result = self.queue_workspace_edit(
            self.workspace_edit_context(),
            edit,
            versions,
            Some(ServerEdit {
                epoch,
                request,
                reply,
            }),
            None,
        );
        self.command_waiting(true);
        if let Err(error) = result {
            self.fail(error);
        }
        true
    }

    pub(super) fn finish_server_edit(
        &mut self,
        epoch: u64,
        cancellation: Cancellation,
        result: std::result::Result<(), String>,
    ) -> bool {
        if !self.workspace.pending.as_ref().is_some_and(|pending| {
            pending
                .server
                .as_ref()
                .is_some_and(|server| server.epoch == epoch)
                && pending.cancellation.same_request(&cancellation)
        }) {
            return false;
        }
        self.cancel_workspace_edit();
        if let Err(error) = result {
            self.fail(error);
        }
        true
    }

    pub(super) fn cancel_server_edit(&mut self) {
        if self
            .workspace
            .pending
            .as_ref()
            .is_some_and(|pending| pending.server.is_some())
        {
            self.cancel_workspace_edit();
        }
    }

    pub(crate) fn handle_workspace_edit(&mut self, result: Result) -> bool {
        let Some(pending) = &self.workspace.pending else {
            return false;
        };
        if !pending.cancellation.same_request(&result.cancellation) {
            return false;
        }
        let Pending {
            context,
            cancellation,
            mut server,
            command,
        } = self.workspace.pending.take().unwrap();
        if cancellation.is_cancelled() || !context.current(self) {
            cancellation.cancel();
            if server.is_some() {
                self.cancel_language_request();
            }
            return true;
        }
        let mut hidden_changed = false;
        let outcome = result.changes.and_then(|changes| {
            if let Some(server) = &mut server
                && (!self.command_current(server.epoch, server.request) || !server.reply.claim())
            {
                return Err(io::Error::other(
                    "language server edit is no longer current",
                ));
            }
            hidden_changed = changes
                .iter()
                .any(|change| change.edit.document_id() != self.editor.document().id());
            self.commit_workspace_edit(changes)
        });
        match outcome {
            Ok(count) => {
                if server.is_none() {
                    // The normal document update already synchronizes active
                    // edits. Only changed hidden buffers need a catalog update.
                    self.workspace.synchronize |= count > 0 && hidden_changed;
                    self.dismiss_language_help();
                }
                self.invalidate_completion();
                self.refresh_git();
                self.refresh_status();
                self.observe_buffer_revision();
                self.message = if count == 0 {
                    "no workspace changes".into()
                } else {
                    format!("updated {count} buffer(s) · changes are unsaved")
                };
                if let Some(server) = server {
                    let applied = self.acknowledge_command_edit();
                    server.reply.finish(Ok(applied));
                }
                if let Some(command) = command
                    && let Err(error) = self.execute_lsp_command(command)
                {
                    self.fail(error);
                }
            }
            Err(error) => {
                if let Some(server) = server {
                    server.reply.finish(Err(error.to_string()));
                }
                self.fail(error);
            }
        }
        cancellation.cancel();
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

#[cfg(all(test, unix))]
pub(super) mod server_tests {
    use super::*;
    use crossterm::event::{Event as TerminalEvent, KeyCode, KeyEvent, KeyModifiers};
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        sync::mpsc,
        time::{Duration, Instant},
    };
    use vex_lsp::{Answer, Event, Request, RequestKind, ServerCommand, Service, Update};

    fn server(directory: &std::path::Path) -> PathBuf {
        let path = directory.join("commands.py");
        fs::write(&path,r#"#!/usr/bin/env python3
import json, sys, os
log = open(os.path.join(os.path.dirname(__file__), 'events.log'), 'w', buffering=1)
documents = {}
held = None
requests = {}
def send(value):
    value['jsonrpc'] = '2.0'
    body = json.dumps(value).encode()
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n' % len(body)).encode() + body)
    sys.stdout.buffer.flush()
def edits(name, advance=0, old='foo'):
    changes = []
    for uri, doc in documents.items():
        at = doc['text'].index(old); prefix = doc['text'][:at]
        line = prefix.count('\n'); column = len(prefix.split('\n')[-1].encode('utf-16-le')) // 2
        changes.append({'textDocument':{'uri':uri,'version':doc['version'] + advance},'edits':[{'range':{'start':{'line':line,'character':column},'end':{'line':line,'character':column+3}},'newText':name}]})
    return {'documentChanges':changes}
def ask(id, edit, expected):
    requests[id] = expected
    send({'id':id,'method':'workspace/applyEdit','params':{'edit':edit}})
while True:
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        if line == b'\r\n': break
        if line.lower().startswith(b'content-length:'): length = int(line.split(b':')[1])
    value = json.loads(sys.stdin.buffer.read(length))
    method = value.get('method'); params = value.get('params', {})
    if method == 'initialize':
        assert params['capabilities']['workspace']['applyEdit']
        send({'id':value['id'],'result':{'capabilities':{'textDocumentSync':2,'hoverProvider':True,'codeActionProvider':{'resolveProvider':True},'executeCommandProvider':{'commands':['test.action','test.early','test.multiple','test.bad','test.resource','test.hold']}}}})
    elif method == 'textDocument/didOpen':
        doc = params['textDocument']; documents[doc['uri']] = doc
    elif method == 'textDocument/didChange':
        doc = documents[params['textDocument']['uri']]
        assert params['textDocument']['version'] == doc['version'] + 1
        doc['version'] += 1; doc['text'] = params['contentChanges'][0]['text']
        log.write('CHANGE ' + str(doc['version']) + '\n')
    elif method == 'textDocument/didClose': documents.pop(params['textDocument']['uri'])
    elif method == 'textDocument/hover': send({'id':value['id'],'result':{'contents':'|'.join(doc['text'] for doc in documents.values())}})
    elif method == 'textDocument/codeAction':
        assert params['context']['triggerKind'] == 1
        assert len(documents) == 2
        assert any('// unsaved' in doc['text'] for doc in documents.values())
        actions = [
            {'title':'Resolve and apply', 'kind':'quickfix', 'isPreferred':True, 'data':{'opaque':[17,'kept']}},
            {'title':'Disabled', 'kind':'quickfix', 'disabled':{'reason':'unavailable'}},
            {'title':'Literal edit', 'kind':'refactor', 'edit':edits('bar'), 'command':{'title':'After edit','command':'test.action'}},
            {'title':'Bad version', 'edit':edits('bar',99), 'command':{'title':'Never execute','command':'test.action'}},
            {'title':'Command only', 'command':'test.early'}
        ]
        actions.extend({'title':'Action %02d' % i, 'command':'test.early'} for i in range(24))
        send({'id':value['id'],'result':actions})
    elif method == 'codeAction/resolve':
        assert params['data'] == {'opaque':[17,'kept']}
        params['edit'] = edits('bar')
        params['command'] = {'title':'After edit', 'command':'test.action'}
        log.write('RESOLVE\n')
        send({'id':value['id'],'result':params})
    elif method == 'workspace/executeCommand':
        command = params['command']; assert params['arguments'] == []
        if command == 'test.action':
            assert all('bar' in doc['text'] and 'foo' not in doc['text'] for doc in documents.values())
            log.write('AFTER_LITERAL\n')
            ask('apply-1', edits('baz', old='bar'), 'baz')
        elif command == 'test.resource': ask('apply-1', {'documentChanges':[{'kind':'delete','uri':next(iter(documents))}]}, None)
        elif command == 'test.bad': ask('apply-1', edits('bar',99), None)
        elif command == 'test.hold':
            held = value['id']; ask('apply-1',edits('bar'),None); continue
        else:
            ask('apply-1',edits('bar'),'bar')
            if command == 'test.multiple': ask(92,edits('baz',1),'baz')
        # Deliberately complete before the client acknowledges its edit(s).
        send({'id':value['id'],'result':None})
    elif method == 'shutdown': send({'id':value['id'],'result':None})
    elif method == 'exit': break
    elif method is None and value.get('id') in requests:
        expected = requests.pop(value['id'])
        if expected is None:
            assert value['result']['applied'] is False
            assert value['result']['failureReason']
            log.write('REJECTED\n')
        else:
            assert value['result']['applied'] is True
            # Every didChange must have reached the server before applied:true.
            assert all(expected in doc['text'] and 'foo' not in doc['text'] for doc in documents.values()), documents
            log.write('APPLIED ' + expected + '\n')
        if held is not None:
            send({'id':held,'result':None}); held = None
"#).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    pub(in crate::app) fn fixture() -> (
        tempfile::TempDir,
        App,
        Service,
        mpsc::Receiver<Event>,
        [PathBuf; 2],
    ) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let paths = [root.join("main.rs"), root.join("hidden.rs")];
        for path in &paths {
            fs::write(path, "foo\n").unwrap();
        }
        let mut app = App::open(Some(&paths[0]), (100, 24)).unwrap();
        app.open_window_file(&paths[1]).unwrap();
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("// unsaved\n").unwrap();
        app.editor.execute("normal_mode", 1).unwrap();
        app.open_window_file(&paths[0]).unwrap();
        app.enable_lsp();
        let (sender, receiver) = mpsc::channel();
        let service = Service::with_program(server(&root), move |event| {
            let _ = sender.send(event);
        })
        .unwrap();
        flush(&mut app, &service);
        (directory, app, service, receiver, paths)
    }

    pub(in crate::app) fn flush(app: &mut App, service: &Service) {
        if let Some(update) = app.take_lsp_update() {
            service.update(update);
        }
        if let Some(update) = app.take_lsp_workspace_update() {
            service.update_workspace(update);
        }
    }

    pub(in crate::app) fn receive(receiver: &mpsc::Receiver<Event>) -> Event {
        let event = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("missing language event");
        assert!(
            !matches!(&event, Event::Status { failed: true, .. }),
            "{event:?}"
        );
        event
    }

    fn invoke(app: &mut App, service: &Service, command: &str) -> Update {
        app.execute_lsp_command(ServerCommand {
            name: command.into(),
            arguments: Arc::default(),
        })
        .unwrap();
        let update = app.take_lsp_update().unwrap();
        let copy = Update {
            document: update.document.clone(),
            request: None,
        };
        service.update(update);
        assert!(app.input_waiting());
        copy
    }

    fn complete(app: &mut App, service: &Service, receiver: &mpsc::Receiver<Event>) -> usize {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut edits = 0;
        while app.input_waiting() {
            assert!(Instant::now() < deadline, "command did not finish");
            if let Some(job) = app.take_workspace_edit() {
                let result = job.run().unwrap();
                assert!(app.handle_workspace_edit(result));
                edits += 1;
                // A command must remain live across its own revision changes.
                assert!(app.input_waiting());
                flush(app, service);
            } else {
                let event = receive(receiver);
                if matches!(
                    &event,
                    Event::Answer {
                        result: Ok(Answer::CommandExecuted),
                        ..
                    }
                ) {
                    assert!(app.take_workspace_edit().is_none());
                }
                app.handle_lsp_event(event);
                flush(app, service);
            }
        }
        edits
    }

    #[test]
    fn server_commands_apply_serial_batches_before_early_completion_sync_before_ack_and_preserve_undo()
     {
        let (directory, mut app, service, receiver, paths) = fixture();
        app.workspace.synchronize = true;
        let old_workspace = app.take_lsp_workspace_update().unwrap();
        service.update_workspace(old_workspace.clone());
        let old = invoke(&mut app, &service, "test.multiple");
        assert_eq!(complete(&mut app, &service, &receiver), 2);
        assert!(!app.error, "{}", app.message);
        assert_eq!(app.editor.document().text(), "baz\n");
        assert_eq!(
            app.snapshot_for_path(&paths[1]).unwrap().text(),
            "// unsaved\nbaz\n"
        );
        assert!(app.take_lsp_workspace_update().is_none()); // The reply carried these snapshots.
        // Older queued metadata cannot roll active or hidden text back after an ack.
        service.update_workspace(old_workspace);
        let document = old.document.unwrap();
        service.update(Update {
            document: Some(document),
            request: Some(Request {
                id: 999,
                kind: RequestKind::Hover,
                position: vex_core::CharOffset(1),
                cancellation: Cancellation::default(),
            }),
        });
        loop {
            if let Event::Answer {
                id: 999, result, ..
            } = receive(&receiver)
            {
                assert!(result.is_err());
                break;
            }
        }
        app.editor.execute("hover", 1).unwrap();
        let update = app.take_lsp_update().unwrap();
        let id = update.request.as_ref().unwrap().id;
        service.update(update);
        loop {
            let event = receive(&receiver);
            if let Event::Answer {
                id: found,
                result: Ok(Answer::Hover(text)),
                ..
            } = &event
                && *found == id
            {
                assert!(text.contains("baz") && !text.contains("foo"));
                app.handle_lsp_event(event);
                break;
            }
            app.handle_lsp_event(event);
        }
        drop(service);
        let log = fs::read_to_string(directory.path().join("events.log")).unwrap();
        assert!(
            log.contains("APPLIED bar\n") && log.contains("APPLIED baz\n"),
            "{log}"
        );
        for path in &paths {
            assert_eq!(fs::read_to_string(path).unwrap(), "foo\n");
        }
        app.editor.execute("undo", 1).unwrap();
        assert_eq!(app.editor.document().text(), "bar\n");
        app.editor.execute("undo", 1).unwrap();
        assert_eq!(app.editor.document().text(), "foo\n");
        app.open_window_file(&paths[1]).unwrap();
        app.editor.execute("undo", 2).unwrap();
        assert_eq!(app.editor.document().text(), "// unsaved\nfoo\n");
    }

    #[test]
    fn rejected_server_batches_report_failure_and_cancellation_rejects_without_editing() {
        for command in ["test.bad", "test.resource", "test.hold"] {
            let (directory, mut app, service, receiver, paths) = fixture();
            invoke(&mut app, &service, command);
            if command == "test.hold" {
                loop {
                    let event = receive(&receiver);
                    let edit = matches!(&event, Event::ApplyEdit { .. });
                    app.handle_lsp_event(event);
                    if edit {
                        break;
                    }
                }
                let job = app.take_workspace_edit().unwrap();
                app.handle(TerminalEvent::Key(KeyEvent::new(
                    KeyCode::Esc,
                    KeyModifiers::NONE,
                )));
                assert!(!app.input_waiting());
                assert!(job.run().is_none());
                flush(&mut app, &service);
                // Await the acknowledgement, without accepting a cancelled command reply.
                loop {
                    if matches!(
                        receive(&receiver),
                        Event::ApplyEditFinished { result: Err(_), .. }
                    ) {
                        break;
                    }
                }
            } else {
                complete(&mut app, &service, &receiver);
                assert!(app.error, "{}", app.message);
                assert!(
                    app.message.contains("workspace edit failed"),
                    "{}",
                    app.message
                );
            }
            drop(service);
            assert_eq!(app.editor.document().text(), "foo\n");
            assert_eq!(
                app.snapshot_for_path(&paths[1]).unwrap().text(),
                "// unsaved\nfoo\n"
            );
            assert!(
                fs::read_to_string(directory.path().join("events.log"))
                    .unwrap()
                    .contains("REJECTED")
            );
        }
    }
}
