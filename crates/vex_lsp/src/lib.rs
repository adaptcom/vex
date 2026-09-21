//! Language servers over stdio, with a small futures executor and immutable editor
//! snapshots. The UI submits coalesced state and receives typed, ordered events.

mod actions;
mod apply;
pub use actions::{ActionEdit, CodeAction, CodeActions};
pub mod diagnostics;
mod documentation;
mod formatting;
pub use apply::{Applied, ApplyReply};
pub use vex_syntax::markup::Document as Documentation;
mod command;
pub use command::ServerCommand;
mod completion;
mod executor;
mod navigation;
mod protocol;
mod rename;
mod sessions;
mod signature;
pub use signature::{SignatureHelp, SignatureOptions};
mod symbols;
mod transport;
mod workspace;
pub mod workspace_edit;
pub use workspace::{WorkspaceDocument, WorkspaceUpdate};

pub use completion::{CompletionItem, Completions};
use executor::Executor;
pub use navigation::{Destination, Locations, Navigation, destination_selection};
pub use protocol::{Location, Position, Range, file_path, file_uri, offset, position};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    future::{Future, poll_fn},
    io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
pub use symbols::{Symbol, Symbols};
use vex_core::{CharOffset, Revision, Selection, Snapshot};
use vex_editor::{Language, background::Cancellation};

pub const MAX_DOCUMENT_BYTES: usize = 8 << 20;
/// Quiet interval for routine document synchronization and diagnostic display.
pub const EDIT_IDLE_DELAY: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionTrigger {
    Invoked,
    Character(char),
    Incomplete,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompletionOptions {
    pub trigger_characters: Vec<char>,
}

impl CompletionOptions {
    fn from_capabilities(capabilities: &Value) -> Option<Self> {
        let provider = &capabilities["completionProvider"];
        if !provider.is_object() {
            return None;
        }
        let mut trigger_characters = Vec::new();
        for value in provider["triggerCharacters"]
            .as_array()
            .into_iter()
            .flatten()
            .take(64)
        {
            let Some(text) = value.as_str() else { continue };
            let mut chars = text.chars();
            if let Some(ch) = chars.next()
                && !ch.is_control()
                && chars.next().is_none()
                && !trigger_characters.contains(&ch)
            {
                trigger_characters.push(ch);
            }
        }
        Some(Self { trigger_characters })
    }
}

#[derive(Clone, Debug)]
pub struct Document {
    pub epoch: u64,
    /// Monotonic explicit-restart sequence. Focus changes only advance epoch.
    pub restart: u64,
    pub language: Language,
    pub path: PathBuf,
    pub snapshot: Snapshot,
    /// Monotonic save sequence; distinguishes repeated saves of the same revision.
    pub saved: u64,
    /// Exact contents of the latest completed save, even when later edits coalesce.
    pub saved_snapshot: Option<Snapshot>,
}

#[derive(Clone, Debug)]
pub enum RequestKind {
    Hover,
    SignatureHelp,
    Navigation(Navigation),
    DocumentHighlights,
    DocumentSymbols,
    WorkspaceSymbols(String),
    Completion(CompletionTrigger),
    ResolveCompletion(Box<CompletionItem>),
    PrepareRename {
        selection: Selection,
        documents: Arc<[WorkspaceDocument]>,
    },
    Rename {
        name: String,
        documents: Arc<[WorkspaceDocument]>,
    },
    Format {
        selection: Option<Selection>,
        indentation: vex_editor::Indentation,
    },
    CodeActions {
        selection: Selection,
        documents: Arc<[WorkspaceDocument]>,
    },
    ApplyCodeAction {
        action: CodeAction,
        documents: Arc<[WorkspaceDocument]>,
    },
    ExecuteCommand {
        command: ServerCommand,
        documents: Arc<[WorkspaceDocument]>,
    },
}

#[derive(Debug)]
pub struct Request {
    pub id: u64,
    pub kind: RequestKind,
    pub position: CharOffset,
    pub cancellation: Cancellation,
}

#[derive(Debug)]
pub struct Update {
    pub document: Option<Document>,
    pub request: Option<Request>,
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub start: CharOffset,
    pub end: CharOffset,
    pub line: usize,
    pub severity: u32,
    pub message: String,
}

#[derive(Debug)]
pub enum Answer {
    Hover(Documentation),
    SignatureHelp(SignatureHelp),
    Locations(Navigation, Locations),
    DocumentHighlights(Option<vex_editor::PreparedSelections>),
    Symbols(Symbols),
    Completion(Completions),
    CompletionResolved(CompletionItem),
    RenamePrepared(String),
    Formatted {
        edit: workspace_edit::WorkspaceEdit,
        versions: Vec<workspace_edit::SynchronizedDocument>,
    },
    CodeActions(CodeActions),
    CodeActionReady(ActionEdit),
    CommandExecuted,
    WorkspaceEdit {
        edit: workspace_edit::WorkspaceEdit,
        versions: Vec<workspace_edit::SynchronizedDocument>,
    },
}

#[derive(Debug)]
pub enum Event {
    DiagnosticCatalog(diagnostics::Catalog),
    ApplyEdit {
        epoch: u64,
        request_id: u64,
        edit: workspace_edit::WorkspaceEdit,
        versions: Vec<workspace_edit::SynchronizedDocument>,
        reply: ApplyReply,
    },
    ApplyEditFinished {
        epoch: u64,
        cancellation: Cancellation,
        result: Result<(), String>,
    },
    Capabilities {
        epoch: u64,
        completion: Option<CompletionOptions>,
        signature: Option<SignatureOptions>,
    },
    Status {
        epoch: u64,
        message: String,
        failed: bool,
    },
    Diagnostics {
        epoch: u64,
        revision: Revision,
        diagnostics: Vec<Diagnostic>,
    },
    Answer {
        epoch: u64,
        revision: Revision,
        id: u64,
        result: Result<Answer, String>,
    },
}

#[derive(Default)]
struct InboxState {
    update: Option<Update>,
    workspace: Option<WorkspaceUpdate>,
    update_due: Option<Instant>,
    update_immediate: bool,
    last_document: Option<(u64, vex_core::DocumentId, Revision, u64)>,
    wire: VecDeque<Value>,
    diagnostics: diagnostics::Pending,
    failure: Option<String>,
    stopped: bool,
    waker: Option<Waker>,
}

#[derive(Clone, Default)]
struct Inbox(Arc<Mutex<InboxState>>);

impl Inbox {
    fn take_wire(&self) -> Option<Value> {
        self.0.lock().unwrap().wire.pop_front()
    }
    fn update_workspace(&self, update: WorkspaceUpdate) {
        let mut state = self.0.lock().unwrap();
        state.workspace = Some(update);
        Self::wake(&mut state);
    }
    fn take_workspace(&self) -> Option<WorkspaceUpdate> {
        self.0.lock().unwrap().workspace.take()
    }
    fn route(&self, update: Update) {
        // The outer mailbox already applied the typing debounce.
        self.0.lock().unwrap().update_immediate = true;
        self.update(update);
    }
    fn session_interrupted(&self, cx: &Context<'_>, epoch: u64) -> bool {
        let mut state = self.0.lock().unwrap();
        state.waker = Some(cx.waker().clone());
        state.stopped
            || state.failure.is_some()
            || state
                .update
                .as_ref()
                .is_some_and(|update| update.document.as_ref().map(|doc| doc.epoch) != Some(epoch))
    }
    fn interrupted(&self, cx: &Context<'_>, document: &Document, token: &Cancellation) -> bool {
        let mut state = self.0.lock().unwrap();
        state.waker = Some(cx.waker().clone());
        state.stopped
            || state.failure.is_some()
            || token.is_cancelled()
            || state.update.as_ref().is_some_and(|update| {
                !update.document.as_ref().is_some_and(|newer| {
                    newer.epoch == document.epoch
                        && newer.snapshot.id() == document.snapshot.id()
                        && newer.snapshot.revision() == document.snapshot.revision()
                }) || update.request.is_some()
            })
    }
    fn wake(state: &mut InboxState) {
        if let Some(waker) = state.waker.take() {
            waker.wake();
        }
    }
    fn update(&self, update: Update) {
        self.update_at(update, Instant::now());
    }
    fn update_at(&self, update: Update, now: Instant) {
        let mut state = self.0.lock().unwrap();
        let document = update.document.as_ref().map(|document| {
            (
                document.epoch,
                document.snapshot.id(),
                document.snapshot.revision(),
                document.saved,
            )
        });
        let session_changed = document.map(|(epoch, id, _, _)| (epoch, id))
            != state.last_document.map(|(epoch, id, _, _)| (epoch, id));
        let edited = document.map(|(_, _, revision, _)| revision)
            != state.last_document.map(|(_, _, revision, _)| revision);
        let saved = document.map(|(_, _, _, saved)| saved)
            != state.last_document.map(|(_, _, _, saved)| saved);
        // Preserve an urgent pending save/request when later edits coalesce.
        state.update_immediate |=
            update.request.is_some() || session_changed || document.is_none() || saved;
        state.update_due = Some(if state.update_immediate {
            now
        } else if edited {
            now + EDIT_IDLE_DELAY
        } else {
            state.update_due.unwrap_or(now)
        });
        state.last_document = document;
        state.update = Some(update);
        Self::wake(&mut state);
    }
    fn publish_diagnostics(&self, publication: diagnostics::Publication) {
        let mut state = self.0.lock().unwrap();
        let retired = state.diagnostics.push(publication);
        Self::wake(&mut state);
        drop(state);
        drop(retired);
    }
    fn wire(&self, value: Value) {
        let mut state = self.0.lock().unwrap();
        if let Some(error) = value["transportError"].as_str() {
            state.failure = Some(error.into());
        } else if state.wire.len() == 128 {
            state.failure = Some("language server notification queue overflow".into());
        } else {
            state.wire.push_back(value);
        }
        Self::wake(&mut state);
    }
    fn stop(&self) {
        let mut state = self.0.lock().unwrap();
        state.stopped = true;
        Self::wake(&mut state);
    }
    fn stopped(&self, cx: &Context<'_>) -> bool {
        let mut state = self.0.lock().unwrap();
        state.waker = Some(cx.waker().clone());
        state.stopped
    }
    fn poll(&self, cx: &Context<'_>, executor: &Executor, now: Instant) -> Poll<Input> {
        let mut state = self.0.lock().unwrap();
        state.waker = Some(cx.waker().clone());
        if state.stopped {
            return Poll::Ready(Input::Stop);
        }
        if let Some(error) = state.failure.take() {
            return Poll::Ready(Input::Failed(error));
        }
        if let Some(deadline) = state.update_due {
            if now >= deadline {
                state.update_due = None;
                state.update_immediate = false;
                if let Some(update) = state.update.take() {
                    return Poll::Ready(Input::Update(update));
                }
            } else {
                executor.deadline(deadline);
            }
        }
        if let Some(value) = state.wire.pop_front() {
            return Poll::Ready(Input::Wire(value));
        }
        if std::mem::take(&mut state.diagnostics.reset) {
            return Poll::Ready(Input::DiagnosticsReset);
        }
        if let Some(publication) = state.diagnostics.pop() {
            return Poll::Ready(Input::Diagnostics(publication));
        }
        Poll::Pending
    }
}

enum Input {
    Update(Update),
    Workspace(WorkspaceUpdate),
    Wire(Value),
    Diagnostics(diagnostics::Publication),
    DiagnosticsReset,
    Failed(String),
    Stop,
    Answer(Result<Value, String>),
    Applied(Result<Applied, String>),
    NextEdit,
}

/// Owns the service and all its child-process threads. Updates never block on
/// pipe I/O or wait for a language server. Servers persist across focus changes;
/// explicit restart retries a failed server.
pub struct Service {
    inbox: Inbox,
    thread: Option<JoinHandle<()>>,
}

impl Service {
    pub fn start(emit: impl Fn(Event) + Send + 'static) -> io::Result<Self> {
        Self::with_override(None, emit)
    }

    /// Override the executable for integration tests, retaining registry arguments.
    pub fn with_program(
        program: PathBuf,
        emit: impl Fn(Event) + Send + 'static,
    ) -> io::Result<Self> {
        Self::with_override(Some(program), emit)
    }

    fn with_override(
        program: Option<PathBuf>,
        emit: impl Fn(Event) + Send + 'static,
    ) -> io::Result<Self> {
        let inbox = Inbox::default();
        let shared = inbox.clone();
        let thread = thread::Builder::new()
            .name("vex-lsp".into())
            .spawn(move || {
                let executor = Executor::default();
                executor.run(sessions::serve(
                    program.as_deref(),
                    &shared,
                    &executor,
                    &emit,
                ));
            })?;
        Ok(Self {
            inbox,
            thread: Some(thread),
        })
    }

    pub fn update(&self, update: Update) {
        self.inbox.update(update);
    }
    pub fn update_workspace(&self, update: WorkspaceUpdate) {
        self.inbox.update_workspace(update);
    }
    pub fn stop(&self) {
        self.inbox.stop();
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn root(path: &Path, language: Language) -> PathBuf {
    let parent = path.parent().unwrap_or(path);
    let Some(server) = language.server() else {
        return parent.into();
    };
    let mut root = None;
    for ancestor in parent.ancestors() {
        if server
            .root_markers
            .iter()
            .any(|marker| ancestor.join(marker).is_file())
        {
            root = Some(ancestor.to_path_buf());
            if !server.outermost_root {
                break;
            }
        }
        if ancestor.join(".git").exists() {
            root.get_or_insert_with(|| ancestor.into());
            break;
        }
    }
    root.unwrap_or_else(|| parent.into())
}

async fn session(
    program: &Path,
    inbox: &Inbox,
    executor: &Executor,
    mut document: Document,
    mut initial_request: Option<Request>,
    catalog: &diagnostics::Catalog,
    emit: &impl Fn(Event),
) -> Result<(), String> {
    check_size(&document)?;
    let mut epoch = document.epoch;
    let server = document
        .language
        .server()
        .ok_or("no language server configured")?;
    let root = root(&document.path, document.language);
    let root_uri = file_uri(&root).map_err(|e| e.to_string())?;
    let mut uri = file_uri(&document.path).map_err(|e| e.to_string())?;
    let notifications = inbox.clone();
    let mut transport =
        transport::Transport::start(program, server.arguments, &root, move |value| {
            if value["method"] == "textDocument/publishDiagnostics" {
                if let Some(publication) = diagnostics::Publication::decode(value) {
                    notifications.publish_diagnostics(publication);
                }
            } else {
                notifications.wire(value);
            }
        })
        .map_err(|e| e.to_string())?;
    let mut initialize = transport.request(executor, "initialize", json!({
        "processId":std::process::id(), "clientInfo":{"name":"vex","version":env!("CARGO_PKG_VERSION")},
        "rootUri":root_uri, "workspaceFolders":[{"uri":root_uri,"name":root.file_name().unwrap_or_default().to_string_lossy()}],
        "capabilities":{
            "general":{"positionEncodings":["utf-16"]},
            "textDocument":{
                "synchronization":{"didSave":true},
                "publishDiagnostics":{"versionSupport":true},
                "hover":{"contentFormat":["markdown","plaintext"]},
                "definition":{"linkSupport":true},
                "typeDefinition":{"linkSupport":true},
                "implementation":{"linkSupport":true},
                "references":{},
                "documentHighlight":{},
                "rename":{"prepareSupport":true},
                "formatting":{},
                "rangeFormatting":{},
                "codeAction":{"codeActionLiteralSupport":{"codeActionKind":{"valueSet":["","quickfix","refactor","refactor.extract","refactor.inline","refactor.rewrite","source","source.organizeImports","source.fixAll"]}},"isPreferredSupport":true,"disabledSupport":true,"dataSupport":true,"resolveSupport":{"properties":["edit","command"]}},
                "documentSymbol":{"hierarchicalDocumentSymbolSupport":true,"symbolKind":{"valueSet":(1..=26).collect::<Vec<_>>()}},
                "signatureHelp":{"signatureInformation":{"documentationFormat":["markdown","plaintext"],"parameterInformation":{"labelOffsetSupport":true},"activeParameterSupport":true}},
                "completion":{"contextSupport":true,"completionItemKind":{"valueSet":(1..=25).collect::<Vec<_>>()},"completionItem":{
                    "snippetSupport":false,"insertReplaceSupport":true,
                    "documentationFormat":["plaintext"],
                    "resolveSupport":{"properties":["documentation","detail","additionalTextEdits"]}
                }}
            },
            "workspace":{"configuration":true,"applyEdit":true,"workspaceEdit":{"documentChanges":true,"failureHandling":"textOnlyTransactional"},"symbol":{"symbolKind":{"valueSet":(1..=26).collect::<Vec<_>>()}}}, "window":{"workDoneProgress":false}
        }
    }), Duration::from_secs(30))?;
    let initialized = poll_fn(|cx| {
        if inbox.stopped(cx) {
            return Poll::Ready(Err("stopped".into()));
        }
        transport.poll_lifecycle_response(inbox, &mut initialize, cx)
    })
    .await?;
    drop(initialize);
    if initialized["capabilities"]["positionEncoding"]
        .as_str()
        .is_some_and(|encoding| encoding != "utf-16")
    {
        return Err("server selected an unsupported position encoding".into());
    }
    let capabilities = initialized["capabilities"].clone();
    let sync = &capabilities["textDocumentSync"];
    if !matches!(
        sync.as_u64().or_else(|| sync["change"].as_u64()),
        Some(1 | 2)
    ) {
        return Err("server does not support document synchronization".into());
    }
    transport.notify("initialized", json!({}))?;
    let mut version = 0i32;
    let mut focused = true;
    let mut pending: Option<PendingRequest> = None;
    let mut workspace = workspace::Workspace::default();
    let mut workspace_generation = 0;
    let mut applying: Option<PendingApply> = None;
    let mut queued_edits = VecDeque::new();
    let result = async {
        // Initialization may be slow; use the newest snapshot before didOpen.
        let latest = {
            let mut state = inbox.0.lock().unwrap();
            state.update_due = None;
            state.update_immediate = false;
            state.update.take()
        };
        if let Some(update) = latest {
            focused = update.document.is_some();
            if let Some(latest) = update.document {
                document = latest;
            }
            epoch = document.epoch;
            uri = file_uri(&document.path).map_err(|e| e.to_string())?;
            initial_request = update.request;
        }
        check_size(&document)?;
        if focused {
            let old = workspace
                .activate(&transport, executor, &document, |cx| inbox.stopped(cx))
                .await?;
            version = old.version;
        }
        let mut positions = protocol::Positions::new(document.snapshot.text());
        let mut action_diagnostics = actions::Diagnostics::default();
        let mut diagnostic_context = diagnostics::ContextCache::default();
        let mut code_actions = actions::Catalog::default();
        if focused {
            emit(Event::Capabilities {
                epoch,
                completion: CompletionOptions::from_capabilities(&capabilities),
                signature: SignatureOptions::from_capabilities(&capabilities),
            });
            emit(Event::Status {
                epoch,
                message: format!("{} ready", server.command),
                failed: false,
            });
        }
        if let Some(update) = inbox.take_workspace() {
            workspace_generation = update.generation;
            workspace
                .synchronize_catalog(
                    &transport,
                    executor,
                    focused.then_some((&document, version)),
                    document.language,
                    &root,
                    &update.documents,
                    |cx| inbox.stopped(cx),
                )
                .await?;
        }
        if let Some(request) = initial_request.take() {
            start_request(
                &mut transport,
                executor,
                &capabilities,
                &uri,
                &document,
                version,
                &root,
                &mut workspace,
                &action_diagnostics,
                &code_actions,
                inbox,
                request,
                &mut pending,
                emit,
            )
            .await;
        }
        let mut budget = 0;
        loop {
            if budget == 32 {
                executor.yield_now().await;
                budget = 0;
            }
            budget += 1;
            let input = poll_fn(|cx| {
                // Register the inbox waker even while waiting for a reply, so
                // edits, cancellation, and shutdown never wait for that reply.
                if inbox.stopped(cx) {
                    return Poll::Ready(Input::Stop);
                }
                if let Some(edit) = &mut applying {
                    if pending.as_ref().is_none_or(|pending| {
                        pending.request.id != edit.request
                            || pending.request.cancellation.is_cancelled()
                    }) {
                        edit.response.cancellation().cancel();
                    }
                    if let Poll::Ready(result) = Pin::new(&mut edit.response).poll(cx) {
                        return Poll::Ready(Input::Applied(result));
                    }
                } else if !queued_edits.is_empty() {
                    return Poll::Ready(Input::NextEdit);
                }
                // A focus update must precede a captured buffer catalog, so the
                // previous active snapshot cannot replace newer unsaved text.
                {
                    let mut state = inbox.0.lock().unwrap();
                    if state.update_due.is_some_and(|due| due <= Instant::now()) {
                        state.update_due = None;
                        state.update_immediate = false;
                        if let Some(update) = state.update.take() {
                            return Poll::Ready(Input::Update(update));
                        }
                    }
                }
                if let Some(update) = inbox.take_workspace() {
                    return Poll::Ready(Input::Workspace(update));
                }
                if applying.is_none()
                    && let Some(PendingRequest {
                        request, response, ..
                    }) = &mut pending
                {
                    if request.cancellation.is_cancelled() {
                        return Poll::Ready(Input::Answer(Err("cancelled".into())));
                    }
                    if let Poll::Ready(answer) = Pin::new(response).poll(cx) {
                        return Poll::Ready(Input::Answer(answer));
                    }
                }
                inbox.poll(cx, executor, Instant::now())
            })
            .await;
            match input {
                Input::Stop => return Ok(()),
                Input::Failed(error) => return Err(error),
                Input::NextEdit => {
                    let (request, value) = queued_edits.pop_front().unwrap();
                    applying = start_server_edit(
                        &transport,
                        executor,
                        inbox,
                        &document,
                        &workspace,
                        version,
                        pending.as_mut(),
                        request,
                        value,
                        emit,
                    )
                    .await?;
                }
                Input::Applied(result) => {
                    let edit = applying.take().unwrap();
                    let cancellation = edit.response.cancellation();
                    let result = match result {
                        Ok(applied) => {
                            // Text has already been installed: synchronization failure
                            // ends this session instead of falsely reporting applied:false.
                            if applied.document.epoch != epoch
                                || applied.workspace.epoch != epoch
                                || applied.document.path != document.path
                                || applied.document.language != document.language
                                || applied.document.snapshot.id() != document.snapshot.id()
                                || applied.document.snapshot.revision()
                                    < document.snapshot.revision()
                            {
                                return Err("invalid workspace application acknowledgement".into());
                            }
                            check_size(&applied.document)?;
                            let mut synced = document.snapshot.clone();
                            synchronize(
                                &transport,
                                executor,
                                inbox,
                                &uri,
                                &mut version,
                                &mut synced,
                                &applied.document.snapshot,
                            )
                            .await?;
                            if document.snapshot.revision() != applied.document.snapshot.revision()
                            {
                                positions =
                                    protocol::Positions::new(applied.document.snapshot.text());
                            }
                            document = applied.document;
                            workspace.remember(&document, version);
                            if applied.workspace.generation > workspace_generation {
                                workspace_generation = applied.workspace.generation;
                                workspace
                                    .synchronize(
                                        &transport,
                                        executor,
                                        &document,
                                        version,
                                        &root,
                                        &applied.workspace.documents,
                                        |cx| inbox.session_interrupted(cx, epoch),
                                    )
                                    .await?;
                            }
                            Ok(())
                        }
                        Err(error) => Err(error),
                    };
                    if let Err(error) = &result
                        && let Some(pending) = &mut pending
                    {
                        pending.edit_failure.get_or_insert_with(|| error.clone());
                    }
                    transport
                        .reply_edit(executor, inbox, edit.wire_id, &result)
                        .await?;
                    if let Some(pending) = &mut pending {
                        pending.response.resume_timeout();
                    }
                    emit(Event::ApplyEditFinished {
                        epoch,
                        cancellation,
                        result,
                    });
                }
                Input::Workspace(update) => {
                    if update.generation <= workspace_generation {
                        continue;
                    }
                    workspace_generation = update.generation;
                    workspace
                        .synchronize_catalog(
                            &transport,
                            executor,
                            focused.then_some((&document, version)),
                            document.language,
                            &root,
                            &update.documents,
                            |cx| inbox.stopped(cx),
                        )
                        .await?;
                }
                Input::Update(update) => {
                    let changed_focus = !focused
                        || update.document.as_ref().is_none_or(|newer| {
                            newer.epoch != epoch
                                || newer.path != document.path
                                || newer.snapshot.id() != document.snapshot.id()
                        });
                    if changed_focus {
                        if let Some(edit) = &applying {
                            edit.response.cancellation().cancel();
                            inbox.route(update);
                            continue;
                        }
                        pending.take();
                        transport.enable_edits(false);
                        action_diagnostics = actions::Diagnostics::default();
                        code_actions = actions::Catalog::default();
                    }
                    let Some(newer) = update.document else {
                        focused = false;
                        continue;
                    };
                    check_size(&newer)?;
                    let old = workspace
                        .activate(&transport, executor, &newer, |cx| inbox.stopped(cx))
                        .await?;
                    // An application acknowledgement can overtake a coalesced
                    // UI snapshot. Never send that older text back to the server.
                    if old.snapshot.id() == newer.snapshot.id()
                        && old.snapshot.revision() > newer.snapshot.revision()
                    {
                        if let Some(request) = update.request {
                            emit(Event::Answer {
                                epoch: newer.epoch,
                                revision: newer.snapshot.revision(),
                                id: request.id,
                                result: Err(
                                    "request snapshot was superseded by a workspace edit".into()
                                ),
                            });
                        }
                        continue;
                    }
                    epoch = newer.epoch;
                    uri = file_uri(&newer.path).map_err(|e| e.to_string())?;
                    version = old.version;
                    let mut synced = old.snapshot;
                    if newer.saved != old.saved
                        && let Some(saved) = &newer.saved_snapshot
                    {
                        synchronize(
                            &transport,
                            executor,
                            inbox,
                            &uri,
                            &mut version,
                            &mut synced,
                            saved,
                        )
                        .await?;
                        let save = &capabilities["textDocumentSync"]["save"];
                        if save == true || save.is_object() {
                            let mut params = json!({"textDocument":{"uri":uri}});
                            if save["includeText"] == true {
                                params["text"] = json!(saved.text().to_string());
                            }
                            transport
                                .notify_wait(executor, "textDocument/didSave", params, |cx| {
                                    inbox.stopped(cx)
                                })
                                .await?;
                        }
                    }
                    synchronize(
                        &transport,
                        executor,
                        inbox,
                        &uri,
                        &mut version,
                        &mut synced,
                        &newer.snapshot,
                    )
                    .await?;
                    if changed_focus || newer.snapshot.revision() != document.snapshot.revision() {
                        if pending.as_ref().is_some_and(|pending| {
                            !matches!(pending.request.kind, RequestKind::ExecuteCommand { .. })
                        }) {
                            pending.take();
                            transport.enable_edits(false);
                        }
                        positions = protocol::Positions::new(newer.snapshot.text());
                    }
                    document = newer;
                    focused = true;
                    workspace.remember(&document, version);
                    if changed_focus {
                        emit(Event::Capabilities {
                            epoch,
                            completion: CompletionOptions::from_capabilities(&capabilities),
                            signature: SignatureOptions::from_capabilities(&capabilities),
                        });
                        emit(Event::Status {
                            epoch,
                            message: format!("{} ready", server.command),
                            failed: false,
                        });
                        let diagnostics = diagnostic_context.get(&document, version).map_or_else(
                            || catalog.for_document(&document, &positions),
                            |values| {
                                Some(
                                    action_diagnostics
                                        .update(values, &document, &positions, version),
                                )
                            },
                        );
                        if let Some(diagnostics) = diagnostics {
                            emit(Event::Diagnostics {
                                epoch,
                                revision: document.snapshot.revision(),
                                diagnostics,
                            });
                        }
                    }
                    if let Some(update) = inbox.take_workspace()
                        && update.generation > workspace_generation
                    {
                        workspace_generation = update.generation;
                        workspace
                            .synchronize_catalog(
                                &transport,
                                executor,
                                Some((&document, version)),
                                document.language,
                                &root,
                                &update.documents,
                                |cx| inbox.stopped(cx),
                            )
                            .await?;
                    }
                    if let Some(request) = update.request {
                        start_request(
                            &mut transport,
                            executor,
                            &capabilities,
                            &uri,
                            &document,
                            version,
                            &root,
                            &mut workspace,
                            &action_diagnostics,
                            &code_actions,
                            inbox,
                            request,
                            &mut pending,
                            emit,
                        )
                        .await;
                    }
                }
                Input::Answer(result) => {
                    let PendingRequest {
                        request,
                        versions,
                        edit_failure,
                        ..
                    } = pending.take().unwrap();
                    transport.enable_edits(false);
                    if !request.cancellation.is_cancelled() {
                        let result = result.and_then(|value| match request.kind {
                            RequestKind::Hover => {
                                documentation::hover(&value, &request.cancellation)
                                    .map(Answer::Hover)
                            }
                            RequestKind::SignatureHelp => {
                                signature::parse(&value, &request.cancellation)
                                    .map(Answer::SignatureHelp)
                            }
                            RequestKind::Navigation(kind) => {
                                navigation::locations(&value, &request.cancellation)
                                    .map(|locations| Answer::Locations(kind, locations))
                            }
                            RequestKind::DocumentHighlights => navigation::highlights(
                                &value,
                                &document.snapshot,
                                &positions,
                                request.position,
                                &request.cancellation,
                            )
                            .map(Answer::DocumentHighlights),
                            RequestKind::DocumentSymbols => {
                                symbols::parse(&value, Some(&document.path)).map(Answer::Symbols)
                            }
                            RequestKind::WorkspaceSymbols(_) => {
                                symbols::parse(&value, None).map(Answer::Symbols)
                            }
                            RequestKind::Completion(_) => completion::parse(
                                value,
                                document.snapshot.text(),
                                request.position,
                                capabilities["completionProvider"]["resolveProvider"] == true,
                            )
                            .map(Answer::Completion),
                            RequestKind::ResolveCompletion(item) => completion::resolve(
                                &item,
                                value,
                                document.snapshot.text(),
                                request.position,
                            )
                            .map(Answer::CompletionResolved),
                            RequestKind::PrepareRename { selection, .. } => rename::placeholder(
                                &value,
                                &document.snapshot,
                                selection,
                                request.position,
                                &positions,
                                &request.cancellation,
                            )
                            .map(Answer::RenamePrepared),
                            RequestKind::Rename { .. } => {
                                workspace_edit::parse(&value, &request.cancellation)
                                    .map(|edit| Answer::WorkspaceEdit { edit, versions })
                            }
                            RequestKind::Format { .. } => formatting::parse(
                                value,
                                &uri,
                                versions[0].version,
                                &request.cancellation,
                            )
                            .map(|edit| Answer::Formatted { edit, versions }),
                            RequestKind::CodeActions { .. } => code_actions
                                .replace(value, &document, &request.cancellation)
                                .map(Answer::CodeActions),
                            RequestKind::ApplyCodeAction { action, .. } => code_actions
                                .ready(
                                    &action,
                                    &document,
                                    Some(value),
                                    &capabilities,
                                    versions,
                                    &request.cancellation,
                                )
                                .map(Answer::CodeActionReady),
                            RequestKind::ExecuteCommand { .. } => edit_failure
                                .map_or(Ok(Answer::CommandExecuted), |error| {
                                    Err(format!("workspace edit failed: {error}"))
                                }),
                        });
                        emit(Event::Answer {
                            epoch,
                            revision: document.snapshot.revision(),
                            id: request.id,
                            result,
                        });
                    }
                }
                Input::DiagnosticsReset => {
                    catalog.reset_limited();
                    action_diagnostics = actions::Diagnostics::default();
                    diagnostic_context = diagnostics::ContextCache::default();
                    if focused {
                        emit(Event::Diagnostics {
                            epoch,
                            revision: document.snapshot.revision(),
                            diagnostics: Vec::new(),
                        });
                    }
                    emit(Event::DiagnosticCatalog(catalog.clone()));
                }
                Input::Diagnostics(mut publication) => {
                    let active = focused && publication.file.path == document.path;
                    let synchronized = if active {
                        Some((
                            version,
                            document.snapshot.id(),
                            document.snapshot.revision(),
                        ))
                    } else {
                        workspace.diagnostic_version(&publication.file.path)
                    };
                    if let Some((current_version, id, revision)) = synchronized {
                        if publication
                            .version
                            .is_some_and(|v| v != i64::from(current_version))
                        {
                            continue;
                        }
                        publication.file.version = Some((id, revision));
                    }
                    if let (Some(synchronized), Some(raw)) = (synchronized, &publication.raw) {
                        diagnostic_context.insert(
                            publication.file.path.clone(),
                            synchronized,
                            raw.clone(),
                        );
                    }
                    catalog.replace(publication.file);
                    emit(Event::DiagnosticCatalog(catalog.clone()));
                    if active {
                        let diagnostics = action_diagnostics.update(
                            publication.raw.take().unwrap_or(Value::Null),
                            &document,
                            &positions,
                            version,
                        );
                        emit(Event::Diagnostics {
                            epoch,
                            revision: document.snapshot.revision(),
                            diagnostics,
                        });
                    }
                }
                Input::Wire(value) => {
                    if transport.receive_response(&value) {
                        continue;
                    }
                    if value["method"] == "workspace/applyEdit" {
                        let request = pending.as_ref().map_or(0, |pending| pending.request.id);
                        if applying.is_some() {
                            if queued_edits.len() == 8 {
                                if let Some(pending) = &mut pending {
                                    pending.edit_failure =
                                        Some("too many pending workspace edit requests".into());
                                }
                                transport
                                    .reply_edit(
                                        executor,
                                        inbox,
                                        value["id"].clone(),
                                        &Err("too many pending workspace edit requests".into()),
                                    )
                                    .await?;
                            } else {
                                queued_edits.push_back((request, value));
                            }
                        } else {
                            applying = start_server_edit(
                                &transport,
                                executor,
                                inbox,
                                &document,
                                &workspace,
                                version,
                                pending.as_mut(),
                                request,
                                value,
                                emit,
                            )
                            .await?;
                        }
                        continue;
                    }
                }
            }
        }
    }
    .await;
    applying.take();
    pending.take();
    transport.enable_edits(false);
    transport
        .shutdown(executor, inbox, focused.then_some(uri.as_str()))
        .await;
    result.map_err(|error| transport.error_context(&error))
}

async fn synchronize(
    transport: &transport::Transport,
    executor: &Executor,
    inbox: &Inbox,
    uri: &str,
    version: &mut i32,
    current: &mut Snapshot,
    next: &Snapshot,
) -> Result<(), String> {
    if current.id() == next.id() && current.revision() == next.revision() {
        return Ok(());
    }
    if next.text().len_bytes() > MAX_DOCUMENT_BYTES {
        return Err("document exceeds the initial 8 MiB LSP limit".into());
    }
    let next_version = version
        .checked_add(1)
        .ok_or("LSP document version exhausted")?;
    transport.notify_wait(executor, "textDocument/didChange", json!({"textDocument":{"uri":uri,"version":next_version},"contentChanges":[{"text":next.text().to_string()}]}), |cx| inbox.stopped(cx)).await?;
    *version = next_version;
    *current = next.clone();
    Ok(())
}

fn check_size(document: &Document) -> Result<(), String> {
    if document.snapshot.text().len_bytes() > MAX_DOCUMENT_BYTES {
        Err("document exceeds the initial 8 MiB LSP limit".into())
    } else {
        Ok(())
    }
}

struct PendingRequest {
    request: Request,
    response: transport::Response,
    versions: Vec<workspace_edit::SynchronizedDocument>,
    edit_failure: Option<String>,
}

struct PendingApply {
    wire_id: Value,
    request: u64,
    response: apply::AwaitApply,
}

#[allow(clippy::too_many_arguments)]
async fn start_server_edit(
    transport: &transport::Transport,
    executor: &Executor,
    inbox: &Inbox,
    document: &Document,
    workspace: &workspace::Workspace,
    version: i32,
    pending: Option<&mut PendingRequest>,
    request_id: u64,
    value: Value,
    emit: &impl Fn(Event),
) -> Result<Option<PendingApply>, String> {
    let parsed = (|| {
        let pending = pending
            .as_ref()
            .filter(|pending| {
                pending.request.id == request_id
                    && !pending.request.cancellation.is_cancelled()
                    && matches!(pending.request.kind, RequestKind::ExecuteCommand { .. })
            })
            .ok_or("workspace edits require an active server command")?;
        let value = value["params"]
            .get("edit")
            .ok_or("workspace/applyEdit is missing its edit")?;
        workspace_edit::parse(value, &pending.request.cancellation)
    })();
    match parsed {
        Ok(edit) => {
            let (reply, response) = apply::channel(executor);
            emit(Event::ApplyEdit {
                epoch: document.epoch,
                request_id,
                edit,
                versions: workspace.versions(document, version),
                reply,
            });
            Ok(Some(PendingApply {
                wire_id: value["id"].clone(),
                request: request_id,
                response,
            }))
        }
        Err(error) => {
            if let Some(pending) = pending.filter(|pending| pending.request.id == request_id) {
                pending.edit_failure.get_or_insert_with(|| error.clone());
            }
            transport
                .reply_edit(executor, inbox, value["id"].clone(), &Err(error))
                .await?;
            Ok(None)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn start_request(
    transport: &mut transport::Transport,
    executor: &Executor,
    capabilities: &Value,
    uri: &str,
    document: &Document,
    version: i32,
    root: &Path,
    workspace: &mut workspace::Workspace,
    diagnostics: &actions::Diagnostics,
    code_actions: &actions::Catalog,
    inbox: &Inbox,
    request: Request,
    pending: &mut Option<PendingRequest>,
    emit: &impl Fn(Event),
) {
    pending.take();
    transport.enable_edits(false);
    if request.cancellation.is_cancelled() {
        return;
    }
    let (method, capability) = match &request.kind {
        RequestKind::Hover => ("textDocument/hover", "hoverProvider"),
        RequestKind::SignatureHelp => ("textDocument/signatureHelp", "signatureHelpProvider"),
        RequestKind::Navigation(kind) => kind.request(),
        RequestKind::DocumentHighlights => (
            "textDocument/documentHighlight",
            "documentHighlightProvider",
        ),
        RequestKind::DocumentSymbols => ("textDocument/documentSymbol", "documentSymbolProvider"),
        RequestKind::WorkspaceSymbols(_) => ("workspace/symbol", "workspaceSymbolProvider"),
        RequestKind::Completion(_) => ("textDocument/completion", "completionProvider"),
        RequestKind::ResolveCompletion(_) => ("completionItem/resolve", "completionProvider"),
        RequestKind::PrepareRename { .. } => ("textDocument/prepareRename", "renameProvider"),
        RequestKind::Rename { .. } => ("textDocument/rename", "renameProvider"),
        RequestKind::Format {
            selection: Some(_), ..
        } => (
            "textDocument/rangeFormatting",
            "documentRangeFormattingProvider",
        ),
        RequestKind::Format {
            selection: None, ..
        } => ("textDocument/formatting", "documentFormattingProvider"),
        RequestKind::CodeActions { .. } => ("textDocument/codeAction", "codeActionProvider"),
        RequestKind::ApplyCodeAction { .. } => ("codeAction/resolve", "codeActionProvider"),
        RequestKind::ExecuteCommand { .. } => {
            ("workspace/executeCommand", "executeCommandProvider")
        }
    };
    let mut versions = Vec::new();
    let result = async {
        if capabilities[capability] != true && !capabilities[capability].is_object() {
            return Err(match &request.kind {
                RequestKind::Format { selection: Some(_), .. } => "language server does not support range formatting; use :format for the whole file",
                RequestKind::Format { selection: None, .. } => "language server does not support document formatting",
                _ => "language server does not support this request",
            }.into());
        }
        let command_params = if let RequestKind::ExecuteCommand { command, .. } = &request.kind {
            Some(command.params(capabilities)?)
        } else {
            None
        };
        if matches!(&request.kind, RequestKind::Format { .. }) {
            versions.push(workspace_edit::SynchronizedDocument {
                path: document.path.clone(), version,
                document: document.snapshot.id(), revision: document.snapshot.revision(),
            });
        }
        if let RequestKind::ApplyCodeAction { action, .. } = &request.kind {
            code_actions.validate(action, document)?;
        }
        if let RequestKind::Rename { name, .. } = &request.kind {
            rename::name(name)?;
            if name.is_empty() {
                return Err("rename name is empty".into());
            }
        }
        if let RequestKind::PrepareRename { documents, .. }
        | RequestKind::Rename { documents, .. }
        | RequestKind::CodeActions { documents, .. }
        | RequestKind::ApplyCodeAction { documents, .. }
        | RequestKind::ExecuteCommand { documents, .. } = &request.kind
        {
            workspace::validate_origin(document, documents)?;
            versions = workspace
                .synchronize(
                    transport,
                    executor,
                    document,
                    version,
                    root,
                    documents,
                    |cx| inbox.interrupted(cx, document, &request.cancellation),
                )
                .await?;
        }
        if let RequestKind::ApplyCodeAction { action, .. } = &request.kind
            && !code_actions.needs_resolution(action, document, capabilities)?
        {
            let result = code_actions.ready(
                action,
                document,
                None,
                capabilities,
                std::mem::take(&mut versions),
                &request.cancellation,
            )?;
            emit(Event::Answer {
                epoch: document.epoch,
                revision: document.snapshot.revision(),
                id: request.id,
                result: Ok(Answer::CodeActionReady(result)),
            });
            return Ok(None);
        }
        if let RequestKind::PrepareRename { selection, .. } = &request.kind
            && capabilities["renameProvider"]["prepareProvider"] != true
        {
            let name = rename::fallback(
                &document.snapshot,
                *selection,
                request.position,
                &request.cancellation,
            )?;
            emit(Event::Answer {
                epoch: document.epoch,
                revision: document.snapshot.revision(),
                id: request.id,
                result: Ok(Answer::RenamePrepared(name)),
            });
            return Ok(None);
        }
        let params = match &request.kind {
            RequestKind::DocumentSymbols => json!({"textDocument":{"uri":uri}}),
            RequestKind::WorkspaceSymbols(query) => json!({"query":query}),
            RequestKind::ResolveCompletion(item) => item.raw.clone(),
            RequestKind::ExecuteCommand { .. } => command_params.unwrap(),
            RequestKind::Format { selection, indentation } => formatting::params(uri, document, *selection, *indentation)?,
            RequestKind::CodeActions { selection, .. } => {
                diagnostics.params(uri, document, version, *selection, &request.cancellation)?
            }
            RequestKind::ApplyCodeAction { action, .. } => code_actions.params(action, document)?,
            kind => {
                let position = position(document.snapshot.text(), request.position)
                    .ok_or("invalid request position")?;
                let mut params = json!({"textDocument":{"uri":uri},"position":position});
                if matches!(kind, RequestKind::Navigation(Navigation::References)) {
                    params["context"] = json!({"includeDeclaration": true});
                }
                if let RequestKind::Rename { name, .. } = kind {
                    params["newName"] = json!(name);
                }
                if let RequestKind::Completion(trigger) = kind {
                    params["context"] = match trigger {
                        CompletionTrigger::Invoked => json!({"triggerKind":1}),
                        CompletionTrigger::Character(ch) => {
                            json!({"triggerKind":2,"triggerCharacter":ch.to_string()})
                        }
                        CompletionTrigger::Incomplete => json!({"triggerKind":3}),
                    };
                }
                params
            }
        };
        transport.enable_edits(matches!(request.kind, RequestKind::ExecuteCommand { .. }));
        transport
            .request_wait(executor, method, params, |cx| {
                inbox.interrupted(cx, document, &request.cancellation)
            })
            .await
            .map(Some)
    }
    .await;
    match result {
        Ok(Some(response)) => {
            *pending = Some(PendingRequest {
                request,
                response,
                versions,
                edit_failure: None,
            })
        }
        Ok(None) => {}
        Err(message) => {
            transport.enable_edits(false);
            emit(Event::Answer {
                epoch: document.epoch,
                revision: document.snapshot.revision(),
                id: request.id,
                result: Err(message),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, sync::mpsc, time::Instant};
    use vex_core::{Document as TextDocument, SelectionSet};

    fn document(path: PathBuf, text: &TextDocument) -> Document {
        Document {
            epoch: 1,
            restart: 0,
            language: Language::Rust,
            path,
            snapshot: text.snapshot(),
            saved: 0,
            saved_snapshot: None,
        }
    }
    fn request(id: u64, kind: RequestKind, position: usize) -> Request {
        Request {
            id,
            kind,
            position: CharOffset(position),
            cancellation: Cancellation::default(),
        }
    }

    fn poll_inbox(inbox: &Inbox, now: Instant) -> Poll<Input> {
        inbox.poll(
            &Context::from_waker(Waker::noop()),
            &Executor::default(),
            now,
        )
    }

    fn edit_document(text: &mut TextDocument, document: &mut Document) {
        let mut selections = SelectionSet::default();
        let edit = text.replace_selections(&selections, "x").unwrap();
        text.apply(edit, &mut selections).unwrap();
        document.snapshot = text.snapshot();
    }

    #[test]
    fn routine_updates_wait_for_typing_to_stop_and_keep_only_the_latest_snapshot() {
        let inbox = Inbox::default();
        let mut text = TextDocument::from("fn main() {}");
        let mut doc = document(PathBuf::from("main.rs"), &text);
        let start = Instant::now();
        inbox.update_at(
            Update {
                document: Some(doc.clone()),
                request: None,
            },
            start,
        );
        assert!(matches!(
            poll_inbox(&inbox, start),
            Poll::Ready(Input::Update(_))
        ));
        let mut now = start;
        for _ in 0..12 {
            now += Duration::from_millis(100);
            edit_document(&mut text, &mut doc);
            inbox.update_at(
                Update {
                    document: Some(doc.clone()),
                    request: None,
                },
                now,
            );
            assert!(poll_inbox(&inbox, now + Duration::from_millis(99)).is_pending());
        }
        // Metadata refreshes do not restart the quiet interval. Wire replies
        // still progress while routine synchronization is waiting.
        now += Duration::from_millis(200);
        inbox.update_at(
            Update {
                document: Some(doc.clone()),
                request: None,
            },
            now,
        );
        inbox.wire(json!({"id": 7, "result": null}));
        assert!(matches!(
            poll_inbox(&inbox, now),
            Poll::Ready(Input::Wire(_))
        ));
        assert!(poll_inbox(&inbox, now + Duration::from_millis(99)).is_pending());
        let Poll::Ready(Input::Update(update)) =
            poll_inbox(&inbox, now + Duration::from_millis(100))
        else {
            panic!("latest edit must be delivered at the idle deadline");
        };
        assert_eq!(
            update.document.unwrap().snapshot.revision(),
            doc.snapshot.revision()
        );
        assert!(poll_inbox(&inbox, now + Duration::from_secs(1)).is_pending());
    }

    #[test]
    fn requests_saves_and_session_changes_bypass_routine_debounce() {
        let inbox = Inbox::default();
        let mut text = TextDocument::from("fn main() {}");
        let mut doc = document(PathBuf::from("main.rs"), &text);
        let mut now = Instant::now();
        inbox.update_at(
            Update {
                document: Some(doc.clone()),
                request: None,
            },
            now,
        );
        assert!(matches!(
            poll_inbox(&inbox, now),
            Poll::Ready(Input::Update(_))
        ));
        for kind in [
            RequestKind::Hover,
            RequestKind::Completion(CompletionTrigger::Invoked),
            RequestKind::SignatureHelp,
        ] {
            now += Duration::from_millis(1);
            edit_document(&mut text, &mut doc);
            inbox.update_at(
                Update {
                    document: Some(doc.clone()),
                    request: None,
                },
                now,
            );
            assert!(poll_inbox(&inbox, now).is_pending());
            inbox.update_at(
                Update {
                    document: Some(doc.clone()),
                    request: Some(request(7, kind, 0)),
                },
                now,
            );
            let Poll::Ready(Input::Update(update)) = poll_inbox(&inbox, now) else {
                panic!("request delayed")
            };
            assert_eq!(update.request.unwrap().id, 7);
            assert_eq!(
                update.document.unwrap().snapshot.revision(),
                doc.snapshot.revision()
            );
        }
        doc.saved += 1;
        doc.saved_snapshot = Some(doc.snapshot.clone());
        let saved_revision = doc.snapshot.revision();
        inbox.update_at(
            Update {
                document: Some(doc.clone()),
                request: None,
            },
            now,
        );
        edit_document(&mut text, &mut doc);
        inbox.update_at(
            Update {
                document: Some(doc.clone()),
                request: None,
            },
            now,
        );
        let Poll::Ready(Input::Update(update)) = poll_inbox(&inbox, now) else {
            panic!("save delayed by following typing")
        };
        let delivered = update.document.unwrap();
        assert_eq!(delivered.saved_snapshot.unwrap().revision(), saved_revision);
        assert_eq!(delivered.snapshot.revision(), doc.snapshot.revision());
        edit_document(&mut text, &mut doc);
        inbox.update_at(
            Update {
                document: Some(doc.clone()),
                request: None,
            },
            now,
        );
        assert!(poll_inbox(&inbox, now).is_pending());
        doc.epoch += 1;
        inbox.update_at(
            Update {
                document: Some(doc.clone()),
                request: None,
            },
            now,
        );
        assert!(matches!(
            poll_inbox(&inbox, now),
            Poll::Ready(Input::Update(_))
        ));
        inbox.update_at(
            Update {
                document: None,
                request: None,
            },
            now,
        );
        assert!(matches!(
            poll_inbox(&inbox, now),
            Poll::Ready(Input::Update(Update { document: None, .. }))
        ));
    }
    fn until(receiver: &mpsc::Receiver<Event>, predicate: impl Fn(&Event) -> bool) -> Event {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let event = receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("missing LSP event");
            assert!(
                !matches!(&event, Event::Status { failed: true, .. }),
                "{event:?}"
            );
            if predicate(&event) {
                return event;
            }
        }
    }

    #[cfg(unix)]
    fn mock(directory: &Path, hang_initialize: bool) -> PathBuf {
        mock_with_versions(directory, hang_initialize, true)
    }

    #[cfg(unix)]
    fn mock_with_versions(
        directory: &Path,
        hang_initialize: bool,
        diagnostic_versions: bool,
    ) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let program = directory.join("server.py");
        let script = r##"#!/usr/bin/env python3
import sys, json, os
log = open(os.path.join(os.path.dirname(__file__), 'messages.jsonl'), 'a', buffering=1)
with open(os.path.join(os.path.dirname(__file__), 'launches.jsonl'), 'a') as launches:
    launches.write(json.dumps({'arguments':sys.argv[1:],'cwd':os.getcwd()}) + '\n')
uri = None
version = 0
def send(value):
    value['jsonrpc'] = '2.0'
    body = json.dumps(value, ensure_ascii=False).encode()
    header = ('Content-Length: %d\r\n\r\n' % len(body)).encode()
    for part in [header[:7], header[7:], body[:3], body[3:]]:
        sys.stdout.buffer.write(part)
        sys.stdout.buffer.flush()
def diagnostics(v, text):
    params = {'uri':uri,'diagnostics':[{'range':{'start':{'line':0,'character':3},'end':{'line':0,'character':4}},'severity':1,'message':text}]}
    if DIAGNOSTIC_VERSIONS: params['version'] = v
    send({'method':'textDocument/publishDiagnostics','params':params})
while True:
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        if line == b'\r\n': break
        if line.lower().startswith(b'content-length:'): length = int(line.split(b':')[1])
    value = json.loads(sys.stdin.buffer.read(length))
    log.write(json.dumps(value) + '\n')
    method = value.get('method')
    params = value.get('params')
    if method == 'initialize':
        if HANG_INITIALIZE: continue
        send({'id':value['id'],'result':{'capabilities':{'positionEncoding':'utf-16','textDocumentSync':{'openClose':True,'change':2,'save':True},'hoverProvider':True,'signatureHelpProvider':{'triggerCharacters':['(',',','::','',None,'\n']},'definitionProvider':True,'typeDefinitionProvider':{},'implementationProvider':True,'referencesProvider':True,'documentHighlightProvider':True,'documentSymbolProvider':True,'workspaceSymbolProvider':{},'completionProvider':{'resolveProvider':True,'triggerCharacters':['.',':','.',None,'..','\n']}}}})
        send({'id':'configuration','method':'workspace/configuration','params':{'items':[{'section':'rust-analyzer'}]}})
    elif method == 'textDocument/didOpen':
        uri = params['textDocument']['uri']; version = params['textDocument']['version']
        diagnostics(version, 'initial')
    elif method == 'textDocument/didChange':
        version = params['textDocument']['version']
        if DIAGNOSTIC_VERSIONS: diagnostics(version - 1, 'stale')
        diagnostics(version, 'fresh')
    elif method == 'textDocument/hover':
        if params['position']['character'] == 0: continue
        send({'id':value['id'],'result':{'contents':{'kind':'plaintext','value':'fn example() -> u32'}}})
    elif method == 'textDocument/signatureHelp':
        assert set(params) == {'textDocument','position'}
        send({'id':value['id'],'result':{'activeSignature':0,'activeParameter':1,'signatures':[{'label':'call(first: T, second: U)','parameters':[{'label':'first: T'},{'label':'second: U'}]}]}})
    elif method == 'textDocument/definition':
        send({'id':value['id'],'result':[{'targetUri':uri,'targetRange':{'start':{'line':0,'character':0},'end':{'line':0,'character':4}},'targetSelectionRange':{'start':{'line':0,'character':3},'end':{'line':0,'character':4}}}]})
    elif method in ('textDocument/typeDefinition', 'textDocument/implementation', 'textDocument/references'):
        if method == 'textDocument/references': assert params['context']['includeDeclaration'] is True
        else: assert 'context' not in params
        span = {'start':{'line':0,'character':3},'end':{'line':0,'character':4}}
        send({'id':value['id'],'result':[{'uri':uri,'range':span},{'uri':uri,'range':{'start':{'line':0,'character':0},'end':{'line':0,'character':1}}}]})
    elif method == 'textDocument/documentHighlight':
        assert set(params) == {'textDocument', 'position'}
        send({'id':value['id'],'result':[{'range':{'start':{'line':0,'character':3},'end':{'line':0,'character':4}}, 'kind':3}]})
    elif method == 'textDocument/documentSymbol':
        assert set(params) == {'textDocument'}
        span = {'start':{'line':0,'character':3},'end':{'line':0,'character':4}}
        send({'id':value['id'],'result':[{'name':'outer','kind':2,'range':span,'selectionRange':span,'children':[{'name':'inner','kind':12,'range':span,'selectionRange':span}]}]})
    elif method == 'workspace/symbol':
        assert set(params) == {'query'}
        if params['query'] == 'hold': continue
        send({'id':value['id'],'result':[{'name':params['query'],'kind':12,'containerName':'outer','location':{'uri':uri,'range':{'start':{'line':0,'character':3},'end':{'line':0,'character':4}}}}]})
    elif method == 'textDocument/completion':
        send({'id':value['id'],'result':{'isIncomplete':False,'items':[{'label':'xray','data':{'ticket':7},'insertText':'xray'}]}})
    elif method == 'completionItem/resolve':
        assert params['data']['ticket'] == 7
        params['documentation'] = {'kind':'plaintext','value':'Resolved docs'}
        params['additionalTextEdits'] = [{'range':{'start':{'line':0,'character':0},'end':{'line':0,'character':0}},'newText':'use demo::xray;\n'}]
        send({'id':value['id'],'result':params})
    elif method == 'shutdown': send({'id':value['id'],'result':None})
    elif method == 'exit': break
"##.replace("HANG_INITIALIZE", if hang_initialize { "True" } else { "False" })
   .replace("DIAGNOSTIC_VERSIONS", if diagnostic_versions { "True" } else { "False" });
        fs::write(&program, script).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        program
    }

    #[cfg(unix)]
    #[test]
    fn unversioned_diagnostics_remain_available_after_edits() {
        let directory = tempfile::tempdir().unwrap();
        let (sender, receiver) = mpsc::channel();
        let service = Service::with_program(
            mock_with_versions(directory.path(), false, false),
            move |event| {
                let _ = sender.send(event);
            },
        )
        .unwrap();
        let mut text = TextDocument::from("const x = 1;");
        let mut doc = document(directory.path().join("main.ts"), &text);
        doc.language = Language::TypeScript;
        service.update(Update {
            document: Some(doc.clone()),
            request: None,
        });
        until(&receiver, |event| {
            matches!(event, Event::Diagnostics { .. })
        });
        let mut selections = SelectionSet::default();
        let edit = text.replace_selections(&selections, " ").unwrap();
        text.apply(edit, &mut selections).unwrap();
        doc.snapshot = text.snapshot();
        service.update(Update {
            document: Some(doc.clone()),
            request: None,
        });
        let Event::Diagnostics {
            revision,
            diagnostics,
            ..
        } = until(&receiver, |event| {
            matches!(event, Event::Diagnostics { .. })
        })
        else {
            unreachable!()
        };
        assert_eq!(revision, doc.snapshot.revision());
        assert_eq!(diagnostics[0].message, "fresh");
        assert_eq!(diagnostics[0].start, CharOffset(3));
    }

    #[cfg(unix)]
    #[test]
    fn registry_servers_use_their_arguments_ids_and_share_compatible_languages() {
        let directory = tempfile::tempdir().unwrap();
        let (sender, receiver) = mpsc::channel();
        let service = Service::with_program(mock(directory.path(), false), move |event| {
            let _ = sender.send(event);
        })
        .unwrap();
        let text = TextDocument::from("example");
        let mut doc = document(directory.path().join("same-file"), &text);
        let cases = [
            (Language::Rust, "rust", vec![]),
            (Language::Markdown, "markdown", vec!["server"]),
            (Language::Bash, "shellscript", vec!["start"]),
            (Language::TypeScript, "typescript", vec!["--stdio"]),
            (Language::Tsx, "typescriptreact", vec!["--stdio"]),
            (Language::JavaScript, "javascript", vec!["--stdio"]),
            (Language::Jsx, "javascriptreact", vec!["--stdio"]),
            (Language::Python, "python", vec!["--stdio"]),
            (Language::Go, "go", vec![]),
            (Language::C, "c", vec![]),
            (Language::Cpp, "cpp", vec![]),
            (Language::Java, "java", vec![]),
            (Language::CSharp, "csharp", vec![]),
            (Language::Swift, "swift", vec![]),
            (Language::Ruby, "ruby", vec![]),
            (Language::Php, "php", vec!["--stdio"]),
            (Language::Lua, "lua", vec![]),
            (Language::Html, "html", vec!["--stdio"]),
            (Language::Css, "css", vec!["--stdio"]),
            (Language::Json, "json", vec!["--stdio"]),
            (Language::Jsonc, "jsonc", vec!["--stdio"]),
            (Language::Yaml, "yaml", vec!["--stdio"]),
            (Language::Toml, "toml", vec!["lsp", "stdio"]),
        ];
        for (index, (language, _, _)) in cases.iter().enumerate() {
            doc.epoch = index as u64 + 1;
            doc.language = *language;
            service.update(Update {
                document: Some(doc.clone()),
                request: Some(request(doc.epoch, RequestKind::Hover, 1)),
            });
            until(
                &receiver,
                |event| matches!(event, Event::Answer { epoch, result: Ok(Answer::Hover(_)), .. } if *epoch == doc.epoch),
            );
        }
        drop(service);
        let messages: Vec<Value> = fs::read_to_string(directory.path().join("messages.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let opens: Vec<_> = messages
            .iter()
            .filter(|message| message["method"] == "textDocument/didOpen")
            .collect();
        let launches: Vec<Value> = fs::read_to_string(directory.path().join("launches.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(opens.len(), cases.len());
        let mut configurations = Vec::new();
        for (language, _, arguments) in &cases {
            let server = language.server().unwrap();
            if !configurations.iter().any(|(command, _, environment)| {
                *command == server.command && *environment == server.environment
            }) {
                configurations.push((server.command, arguments, server.environment));
            }
        }
        assert_eq!(launches.len(), configurations.len());
        for (index, (_, id, _)) in cases.iter().enumerate() {
            assert_eq!(opens[index]["params"]["textDocument"]["languageId"], *id);
        }
        for (index, (_, arguments, _)) in configurations.iter().enumerate() {
            assert_eq!(launches[index]["arguments"], json!(arguments));
        }
        assert_eq!(
            messages
                .iter()
                .filter(|message| message["method"] == "shutdown")
                .count(),
            configurations.len()
        );
    }

    #[test]
    fn project_roots_follow_language_markers_and_repository_boundaries() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path();
        let package = workspace.join("packages/app");
        let source = package.join("src");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir(workspace.join(".git")).unwrap();
        fs::write(workspace.join("Cargo.toml"), "[workspace]").unwrap();
        fs::write(package.join("Cargo.toml"), "[package]").unwrap();
        fs::write(package.join("tsconfig.json"), "{}").unwrap();
        fs::write(workspace.join(".marksman.toml"), "").unwrap();
        let file = source.join("file");
        assert_eq!(root(&file, Language::Rust), workspace);
        assert_eq!(root(&file, Language::TypeScript), package);
        assert_eq!(root(&file, Language::Markdown), workspace);
        assert_eq!(root(&file, Language::Bash), workspace);
        fs::write(package.join(".shellcheckrc"), "").unwrap();
        assert_eq!(root(&file, Language::Bash), package);
        fs::write(workspace.join("go.work"), "").unwrap();
        fs::write(package.join("go.mod"), "").unwrap();
        assert_eq!(root(&file, Language::Go), workspace);
        for (language, marker) in [
            (Language::Python, "pyproject.toml"),
            (Language::C, "compile_commands.json"),
            (Language::Cpp, "compile_commands.json"),
            (Language::Java, "pom.xml"),
            (Language::CSharp, "Directory.Build.props"),
            (Language::Swift, "Package.swift"),
            (Language::Ruby, "Gemfile"),
            (Language::Php, "composer.json"),
            (Language::Lua, ".luarc.json"),
            (Language::Html, "package.json"),
            (Language::Css, "package.json"),
            (Language::Json, "package.json"),
            (Language::Jsonc, "package.json"),
            (Language::Yaml, ".yamllint"),
            (Language::Toml, ".taplo.toml"),
        ] {
            fs::write(package.join(marker), "").unwrap();
            assert_eq!(root(&file, language), package, "{language:?}");
        }
        fs::create_dir(source.join(".git")).unwrap();
        assert_eq!(root(&file, Language::Rust), source);
        assert_eq!(root(&file, Language::TypeScript), source);
    }

    #[cfg(unix)]
    #[test]
    fn symbols_sync_before_requests_and_superseded_workspace_queries_cancel() {
        let directory = tempfile::tempdir().unwrap();
        let (sender, receiver) = mpsc::channel();
        let service = Service::with_program(mock(directory.path(), false), move |event| {
            let _ = sender.send(event);
        })
        .unwrap();
        let mut text = TextDocument::from("a🦀x\n");
        let mut doc = document(directory.path().join("symbols.rs"), &text);
        service.update(Update {
            document: Some(doc.clone()),
            request: Some(request(1, RequestKind::DocumentSymbols, 0)),
        });
        let Event::Answer {
            result: Ok(Answer::Symbols(symbols)),
            ..
        } = until(&receiver, |event| {
            matches!(event, Event::Answer { id: 1, .. })
        })
        else {
            panic!("missing outline")
        };
        assert_eq!(symbols.items.len(), 2);
        assert_eq!(symbols.items[1].container, "outer");
        assert_eq!(symbols.items[1].location.path, doc.path);
        let edit = text
            .transaction([vex_core::Edit::new(CharOffset(0)..CharOffset(0), "new")])
            .unwrap();
        text.apply(
            edit,
            &mut SelectionSet::single(vex_core::Selection::cursor(CharOffset(0))),
        )
        .unwrap();
        doc.snapshot = text.snapshot();
        service.update(Update {
            document: Some(doc.clone()),
            request: Some(request(2, RequestKind::WorkspaceSymbols("hold".into()), 0)),
        });
        // Diagnostics prove didChange was processed before the following request.
        until(
            &receiver,
            |event| matches!(event, Event::Diagnostics { revision, .. } if *revision == doc.snapshot.revision()),
        );
        service.update(Update {
            document: Some(doc.clone()),
            request: Some(request(3, RequestKind::WorkspaceSymbols("界".into()), 0)),
        });
        let Event::Answer {
            result: Ok(Answer::Symbols(symbols)),
            ..
        } = until(&receiver, |event| {
            matches!(event, Event::Answer { id: 3, .. })
        })
        else {
            panic!("missing workspace symbols")
        };
        assert_eq!(symbols.items[0].name, "界");
        drop(service);
        let messages: Vec<Value> = fs::read_to_string(directory.path().join("messages.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let methods: Vec<_> = messages
            .iter()
            .filter_map(|message| message["method"].as_str())
            .collect();
        assert!(
            methods
                .iter()
                .position(|method| *method == "textDocument/didChange")
                .unwrap()
                < methods
                    .iter()
                    .position(|method| *method == "workspace/symbol")
                    .unwrap()
        );
        assert!(methods.contains(&"$/cancelRequest"));
        let init = &messages
            .iter()
            .find(|message| message["method"] == "initialize")
            .unwrap()["params"]["capabilities"];
        assert_eq!(
            init["textDocument"]["documentSymbol"]["hierarchicalDocumentSymbolSupport"],
            true
        );
        assert!(init["workspace"]["symbol"].get("resolveSupport").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn diagnostic_catalog_survives_file_sessions_with_document_revision_stamps() {
        let directory = tempfile::tempdir().unwrap();
        let (sender, receiver) = mpsc::channel();
        let service = Service::with_program(mock(directory.path(), false), move |event| {
            let _ = sender.send(event);
        })
        .unwrap();
        let Event::DiagnosticCatalog(catalog) = until(&receiver, |event| {
            matches!(event, Event::DiagnosticCatalog(_))
        }) else {
            unreachable!()
        };
        let first = TextDocument::from("fn first() {}\n");
        let second = TextDocument::from("fn second() {}\n");
        for (index, text) in [&first, &second].into_iter().enumerate() {
            let mut doc = document(directory.path().join(format!("file{index}.rs")), text);
            doc.epoch = index as u64 + 1;
            service.update(Update {
                document: Some(doc.clone()),
                request: None,
            });
            until(
                &receiver,
                |event| matches!(event, Event::Diagnostics { epoch, .. } if *epoch == doc.epoch),
            );
            assert_eq!(catalog.snapshot().files.len(), index + 1);
        }
        let files = catalog.snapshot().files;
        assert_eq!(files[0].version, Some((first.id(), first.revision())));
        assert_eq!(files[1].version, Some((second.id(), second.revision())));
        assert_eq!(&*files[0].entries[0].message, "initial");
        assert_eq!(&*files[1].entries[0].message, "initial");
    }

    #[cfg(unix)]
    #[test]
    fn stdio_session_syncs_unicode_answers_requests_rejects_old_diagnostics_and_shuts_down() {
        let directory = tempfile::tempdir().unwrap();
        let (sender, receiver) = mpsc::channel();
        let service = Service::with_program(mock(directory.path(), false), move |event| {
            let _ = sender.send(event);
        })
        .unwrap();
        let mut text = TextDocument::from("a🦀x\r\n");
        let mut doc = document(directory.path().join("space 界.rs"), &text);
        service.update(Update {
            document: Some(doc.clone()),
            request: None,
        });
        let Event::Capabilities {
            completion: Some(options),
            signature: Some(signatures),
            ..
        } = until(&receiver, |event| {
            matches!(event, Event::Capabilities { .. })
        })
        else {
            panic!("missing completion capabilities")
        };
        assert_eq!(options.trigger_characters, ['.', ':']);
        assert_eq!(signatures.trigger_characters, ["(", ",", "::"]);
        let initial = until(&receiver, |event| {
            matches!(event, Event::Diagnostics { .. })
        });
        let Event::Diagnostics { diagnostics, .. } = initial else {
            unreachable!()
        };
        assert_eq!(diagnostics[0].start, CharOffset(2));
        service.update(Update {
            document: Some(doc.clone()),
            request: Some(request(24, RequestKind::SignatureHelp, 2)),
        });
        assert!(
            matches!(until(&receiver, |event| matches!(event, Event::Answer { id: 24, .. })),
            Event::Answer { result: Ok(Answer::SignatureHelp(help)), .. }
                if help.signatures[0].contains("call(first: T, second: U)"))
        );

        service.update(Update {
            document: Some(doc.clone()),
            request: Some(request(1, RequestKind::Hover, 2)),
        });
        assert!(
            matches!(until(&receiver, |event| matches!(event, Event::Answer { id: 1, .. })), Event::Answer { result: Ok(Answer::Hover(text)), .. } if text.contains("example"))
        );
        service.update(Update {
            document: Some(doc.clone()),
            request: Some(request(
                2,
                RequestKind::Navigation(Navigation::Definition),
                2,
            )),
        });
        assert!(
            matches!(until(&receiver, |event| matches!(event, Event::Answer { id: 2, .. })), Event::Answer { result: Ok(Answer::Locations(Navigation::Definition, locations)), .. }
                if locations.items[0].path == doc.path
                    && locations.items[0].range.start == Position { line: 0, character: 3 }
                    && locations.items[0].range.end == Position { line: 0, character: 4 })
        );
        for (id, kind) in [
            (20, Navigation::TypeDefinition),
            (21, Navigation::Implementation),
            (22, Navigation::References),
        ] {
            service.update(Update {
                document: Some(doc.clone()),
                request: Some(request(id, RequestKind::Navigation(kind), 2)),
            });
            let event = until(
                &receiver,
                |event| matches!(event, Event::Answer { id: found, .. } if *found == id),
            );
            assert!(
                matches!(event, Event::Answer { result: Ok(Answer::Locations(found, locations)), .. } if found == kind && locations.items.len() == 2)
            );
        }
        service.update(Update {
            document: Some(doc.clone()),
            request: Some(request(23, RequestKind::DocumentHighlights, 2)),
        });
        let event = until(&receiver, |event| {
            matches!(event, Event::Answer { id: 23, .. })
        });
        assert!(matches!(
            event,
            Event::Answer {
                result: Ok(Answer::DocumentHighlights(Some(_))),
                ..
            }
        ));
        service.update(Update {
            document: Some(doc.clone()),
            request: Some(request(
                4,
                RequestKind::Completion(CompletionTrigger::Invoked),
                3,
            )),
        });
        let Event::Answer {
            result: Ok(Answer::Completion(mut list)),
            ..
        } = until(&receiver, |event| {
            matches!(event, Event::Answer { id: 4, .. })
        })
        else {
            panic!("missing completion list")
        };
        let item = list.items.pop().unwrap();
        assert_eq!(item.edit.range(), CharOffset(2)..CharOffset(3));
        assert!(!item.resolved);
        service.update(Update {
            document: Some(doc.clone()),
            request: Some(request(
                5,
                RequestKind::ResolveCompletion(Box::new(item)),
                3,
            )),
        });
        let Event::Answer {
            result: Ok(Answer::CompletionResolved(item)),
            ..
        } = until(&receiver, |event| {
            matches!(event, Event::Answer { id: 5, .. })
        })
        else {
            panic!("missing resolved completion")
        };
        assert_eq!(item.documentation, "Resolved docs");
        assert_eq!(item.additional_edits.len(), 1);
        assert!(item.resolved);
        for (id, trigger) in [
            (6, CompletionTrigger::Character('.')),
            (7, CompletionTrigger::Incomplete),
        ] {
            service.update(Update {
                document: Some(doc.clone()),
                request: Some(request(id, RequestKind::Completion(trigger), 3)),
            });
            until(
                &receiver,
                |event| matches!(event, Event::Answer { id: answer, .. } if *answer == id),
            );
        }
        let stalled = request(3, RequestKind::Hover, 0);
        let cancellation = stalled.cancellation.clone();
        service.update(Update {
            document: Some(doc.clone()),
            request: Some(stalled),
        });
        let mut selections = SelectionSet::default();
        let edit = text.replace_selections(&selections, " ").unwrap();
        text.apply(edit, &mut selections).unwrap();
        doc.saved_snapshot = Some(text.snapshot());
        let edit = text.replace_selections(&selections, "_").unwrap();
        text.apply(edit, &mut selections).unwrap();
        doc.snapshot = text.snapshot();
        doc.saved = 1;
        cancellation.cancel();
        service.update(Update {
            document: Some(doc.clone()),
            request: None,
        });
        loop {
            let event = until(&receiver, |event| {
                matches!(event, Event::Diagnostics { .. })
            });
            let Event::Diagnostics {
                revision,
                diagnostics,
                ..
            } = event
            else {
                unreachable!()
            };
            assert_ne!(diagnostics[0].message, "stale");
            if revision == text.revision() {
                assert_eq!(diagnostics[0].message, "fresh");
                break;
            }
        }
        let started = Instant::now();
        drop(service);
        assert!(started.elapsed() < Duration::from_secs(3));
        let messages: Vec<Value> = fs::read_to_string(directory.path().join("messages.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let methods: Vec<_> = messages
            .iter()
            .filter_map(|message| message["method"].as_str())
            .collect();
        assert_eq!(methods[0], "initialize");
        assert_eq!(
            messages[0]["params"]["capabilities"]["textDocument"]["completion"]["contextSupport"],
            true
        );
        let contexts: Vec<_> = messages
            .iter()
            .filter(|message| message["method"] == "textDocument/completion")
            .map(|message| message["params"]["context"].clone())
            .collect();
        assert_eq!(
            contexts,
            vec![
                json!({"triggerKind":1}),
                json!({"triggerKind":2,"triggerCharacter":"."}),
                json!({"triggerKind":3})
            ]
        );
        assert_eq!(
            messages[0]["params"]["capabilities"]["textDocument"]["completion"]["completionItem"]["snippetSupport"],
            false
        );
        assert!(
            messages
                .iter()
                .any(|message| message["method"] == "textDocument/completion"
                    && message["params"]["position"]["character"] == 4)
        );
        let saved = messages
            .iter()
            .position(|message| message["method"] == "textDocument/didSave")
            .unwrap();
        assert_eq!(messages[saved - 1]["method"], "textDocument/didChange");
        assert_eq!(messages[saved + 1]["method"], "textDocument/didChange");
        assert_eq!(
            messages[saved - 1]["params"]["contentChanges"][0]["text"],
            doc.saved_snapshot.as_ref().unwrap().text().to_string()
        );
        assert_eq!(
            messages[saved + 1]["params"]["contentChanges"][0]["text"],
            doc.snapshot.text().to_string()
        );
        assert_eq!(
            &methods[methods.len() - 3..],
            &["textDocument/didClose", "shutdown", "exit"]
        );
        assert!(
            messages
                .iter()
                .any(|message| message["id"] == "configuration"
                    && message["result"] == json!([null]))
        );
        assert!(
            messages
                .iter()
                .any(|message| message["method"] == "textDocument/hover"
                    && message["params"]["position"]["character"] == 3)
        );
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_interrupts_initialize_and_missing_servers_do_not_retry_on_updates() {
        let directory = tempfile::tempdir().unwrap();
        let (sender, receiver) = mpsc::channel();
        let service = Service::with_program(mock(directory.path(), true), move |event| {
            let _ = sender.send(event);
        })
        .unwrap();
        let doc = document(
            directory.path().join("main.rs"),
            &TextDocument::from("fn main() {}"),
        );
        service.update(Update {
            document: Some(doc.clone()),
            request: None,
        });
        until(&receiver, |event| matches!(event, Event::Status { .. }));
        let started = Instant::now();
        drop(service);
        assert!(started.elapsed() < Duration::from_secs(3));
        let (sender, receiver) = mpsc::channel();
        let service =
            Service::with_program(directory.path().join("missing-server"), move |event| {
                let _ = sender.send(event);
            })
            .unwrap();
        service.update(Update {
            document: Some(doc.clone()),
            request: None,
        });
        loop {
            if matches!(
                receiver.recv_timeout(Duration::from_secs(3)).unwrap(),
                Event::Status { failed: true, .. }
            ) {
                break;
            }
        }
        service.update(Update {
            document: Some(doc),
            request: None,
        });
        assert!(receiver.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    #[ignore = "requires rust-analyzer and a Rust toolchain; run explicitly for end-to-end validation"]
    fn real_rust_analyzer_hover_definition_and_diagnostics() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname = \"vex_lsp_smoke\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::create_dir(directory.path().join("src")).unwrap();
        let path = fs::canonicalize(directory.path())
            .unwrap()
            .join("src/main.rs");
        let source = "fn answer() -> u32 { 42 }\nenum Highlight { Keyword, Type }\nfn main() { let mut my_var = Highlight::Keyword; my_var = Highlight::Type; let _: bool = answer(); }\n";
        let definitions = [
            ("answer", 0, 3, 9),
            ("Highlight", 1, 5, 14),
            ("my_var", 2, 20, 26),
        ];
        fs::write(&path, source).unwrap();
        let text = TextDocument::from(source);
        let doc = document(path.clone(), &text);
        let (sender, receiver) = mpsc::channel();
        let service = Service::start(move |event| {
            let _ = sender.send(event);
        })
        .unwrap();
        service.update(Update {
            document: Some(doc.clone()),
            request: None,
        });
        let deadline = Instant::now() + Duration::from_secs(45);
        let (mut hover, mut diagnostic) = (false, false);
        let mut found = [false; 3];
        let mut id = 0;
        while Instant::now() < deadline
            && !(hover && found.iter().all(|found| *found) && diagnostic)
        {
            match receiver.recv_timeout(Duration::from_millis(500)) {
                Ok(Event::Status {
                    failed: true,
                    message,
                    ..
                }) => panic!("{message}"),
                Ok(Event::Diagnostics { diagnostics, .. }) => {
                    diagnostic |= diagnostics.iter().any(|d| d.severity == 1)
                }
                Ok(Event::Answer {
                    result: Ok(Answer::Hover(text)),
                    ..
                }) => hover |= text.contains("answer"),
                Ok(Event::Answer {
                    result: Ok(Answer::Locations(Navigation::Definition, locations)),
                    ..
                }) => {
                    // The server can return no destinations while indexing.
                    for (index, &(_, line, start, end)) in definitions.iter().enumerate() {
                        found[index] |= locations.items.iter().any(|location| {
                            location.path == path
                                && location.range.start
                                    == Position {
                                        line,
                                        character: start,
                                    }
                                && location.range.end
                                    == Position {
                                        line,
                                        character: end,
                                    }
                        });
                    }
                }
                _ => {}
            }
            id += 1;
            let (kind, name) = if hover {
                let next = found.iter().position(|found| !found).unwrap_or(0);
                (
                    RequestKind::Navigation(Navigation::Definition),
                    definitions[next].0,
                )
            } else {
                (RequestKind::Hover, "answer")
            };
            service.update(Update {
                document: Some(doc.clone()),
                request: Some(request(id, kind, source.rfind(name).unwrap())),
            });
        }
        assert!(
            hover && found.iter().all(|found| *found) && diagnostic,
            "hover={hover}, definitions={found:?}, diagnostic={diagnostic}"
        );
    }
}
