//! Main-thread language-service state, navigation, and presentation.

use super::App;
use crate::screen::{Frame, Style};
use std::{io, time::Instant};
use vex_core::{CharOffset, DocumentId, Revision, SelectionSet, motion};
use vex_editor::{Language, LanguageAction, Mode, background::Cancellation};
use vex_lsp::{
    Answer, CompletionOptions, CompletionTrigger, Diagnostic, Event, Navigation, RequestKind,
};

struct Pending {
    id: u64,
    revision: Revision,
    document: DocumentId,
    selections: SelectionSet,
    mode: Mode,
    cancellation: Cancellation,
    waiting: bool,
    executing: bool,
    window: u64,
    signature: bool,
    automatic: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct DiagnosticContext {
    epoch: u64,
    document: DocumentId,
    revision: Revision,
    saved: u64,
}

#[derive(Default)]
struct DiagnosticDelay {
    observed: Option<DiagnosticContext>,
    due: Option<Instant>,
    pending: Option<Vec<Diagnostic>>,
}

#[derive(Default)]
pub(super) struct State {
    enabled: bool,
    epoch: u64,
    document: Option<vex_lsp::Document>,
    restart: u64,
    force: bool,
    next_request: u64,
    pending: Option<Pending>,
    command: Option<(super::workspace::Context, vex_lsp::ServerCommand)>,
    workspace_generation: u64,
    pub(super) diagnostic_catalog: vex_lsp::diagnostics::Catalog,
    diagnostics: Vec<Diagnostic>,
    diagnostic_revision: Option<(DocumentId, Revision)>,
    diagnostic_delay: DiagnosticDelay,
    popup: Option<crate::documentation::Popup>,
    status: &'static str,
    completion: Option<CompletionOptions>,
    signature: Option<vex_lsp::SignatureOptions>,
    pub(super) saved: u64,
    pub(super) saved_snapshot: Option<vex_core::Snapshot>,
}

impl State {
    fn cancel(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.cancellation.cancel();
            self.force = true;
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl App {
    pub fn enable_lsp(&mut self) {
        self.language.enabled = true;
        self.language.force = true;
    }

    /// Execute an explicitly selected, advertised language-server command.
    /// Used by code actions; arguments remain protocol data, never shell input.
    pub fn execute_lsp_command(&mut self, command: vex_lsp::ServerCommand) -> io::Result<()> {
        if self.input_waiting() {
            return Err(io::Error::other("another editor operation is pending"));
        }
        if !self.language.enabled {
            return Err(io::Error::other("language services are not enabled"));
        }
        self.close_picker();
        self.dismiss_language_help();
        self.language.command = Some((self.workspace_edit_context(), command));
        Ok(())
    }

    fn take_command_request(&mut self) -> Option<RequestKind> {
        let (context, command) = self.language.command.take()?;
        if !context.current(self) {
            self.fail("server command origin changed");
            return None;
        }
        Some(RequestKind::ExecuteCommand {
            command,
            documents: context.lsp_documents(),
        })
    }

    pub(super) fn command_current(&self, epoch: u64, request: u64) -> bool {
        self.language.epoch == epoch
            && self.language.pending.as_ref().is_some_and(|pending| {
                pending.id == request
                    && pending.executing
                    && !pending.cancellation.is_cancelled()
                    && pending.document == self.editor.document().id()
                    && pending.revision == self.editor.document().revision()
                    && pending.selections == *self.editor.selections()
                    && pending.mode == self.editor.mode()
                    && pending.window == self.focused_window_id()
            })
    }

    pub(super) fn command_waiting(&mut self, waiting: bool) {
        if let Some(pending) = &mut self.language.pending {
            pending.waiting = waiting;
        }
    }

    pub(super) fn acknowledge_command_edit(&mut self) -> vex_lsp::Applied {
        let pending = self
            .language
            .pending
            .as_mut()
            .expect("validated executing command");
        pending.revision = self.editor.document().revision();
        pending.selections = self.editor.selections().clone();
        let mut document = self
            .language
            .document
            .clone()
            .expect("named server document");
        document.snapshot = self.editor.document().snapshot();
        document.saved = self.language.saved;
        document.saved_snapshot = self
            .language
            .saved_snapshot
            .as_ref()
            .filter(|snapshot| snapshot.id() == document.snapshot.id())
            .cloned();
        self.language.workspace_generation += 1;
        vex_lsp::Applied {
            document,
            workspace: vex_lsp::WorkspaceUpdate {
                epoch: self.language.epoch,
                generation: self.language.workspace_generation,
                documents: self.lsp_workspace_documents(),
            },
        }
    }

    /// Buffer lifetimes, reloads, and workspace application schedule this catalog
    /// capture. Normal keystrokes keep their constant-size active-document update.
    pub(crate) fn take_lsp_workspace_update(&mut self) -> Option<vex_lsp::WorkspaceUpdate> {
        if !std::mem::take(&mut self.workspace.synchronize) || !self.language.enabled {
            return None;
        }
        let epoch = self.language.epoch;
        self.language.workspace_generation += 1;
        Some(vex_lsp::WorkspaceUpdate {
            epoch,
            generation: self.language.workspace_generation,
            documents: self.lsp_workspace_documents(),
        })
    }

    pub(super) fn language_waiting(&self) -> bool {
        self.language.command.is_some()
            || self.actions.waiting()
            || self
                .language
                .pending
                .as_ref()
                .is_some_and(|pending| pending.waiting)
    }

    pub(super) fn dismiss_language_help(&mut self) {
        self.cancel_language_request();
        self.language.popup = None;
        self.completion.clear();
    }

    pub(super) fn handle_hover_input(&mut self, event: &crossterm::event::Event) -> Option<bool> {
        use crossterm::event::{Event, KeyEventKind};
        use vex_editor::Key;
        let popup = self.language.popup.as_mut()?;
        let Event::Key(event) = event else {
            return None;
        };
        if event.kind == KeyEventKind::Release {
            return Some(false);
        }
        match crate::input::key(*event) {
            Some(Key::Ctrl('u') | Key::PageUp) => {
                popup.scroll(false);
                Some(true)
            }
            Some(Key::Ctrl('d') | Key::PageDown) => {
                popup.scroll(true);
                Some(true)
            }
            Some(Key::Escape | Key::Ctrl('c')) => {
                self.dismiss_language_help();
                self.clear_message();
                Some(true)
            }
            _ => None,
        }
    }

    pub(super) fn hover_mouse(
        &mut self,
        x: u16,
        y: u16,
        scroll: Option<(bool, usize)>,
    ) -> Option<bool> {
        let popup = self.language.popup.as_mut()?;
        popup.area.filter(|area| area.contains(x, y))?;
        Some(scroll.is_some_and(|(down, count)| popup.scroll_lines(down, count)))
    }

    pub(super) fn cancel_language_request(&mut self) {
        if self.signature_request_pending() {
            self.signature.interrupted();
        }
        self.language.cancel();
        self.rename.clear();
        self.actions.clear();
        self.formatting.clear();
        self.language.command = None;
    }

    pub(super) fn restart_language_server(&mut self) {
        if self.files.target().is_none()
            || self.editor.language().and_then(Language::server).is_none()
        {
            self.message = "no language server configured for this buffer".into();
            return;
        }
        self.dismiss_language_help();
        self.signature.reset();
        self.language.signature = None;
        self.language.epoch += 1;
        self.language.restart += 1;
        self.language.document = None;
        self.language.completion = None;
        self.language.force = true;
        self.language.diagnostics.clear();
        self.message = "restarting language server".into();
    }

    /// Called after dispatch and drawing. Only shared snapshots and small
    /// metadata cross this boundary; JSON and UTF-16 work run on the service.
    pub fn take_lsp_update(&mut self) -> Option<vex_lsp::Update> {
        self.invalidate_code_actions();
        self.invalidate_symbol_picker();
        self.invalidate_location_picker();
        self.poll_completion(Instant::now());
        self.observe_signature(Instant::now(), false);
        let action = self.editor.take_language_action();
        if !self.language.enabled {
            self.fail_symbol_picker("language services are not enabled in this frontend");
            if action.is_some() {
                self.fail("language services are not enabled in this frontend");
            }
            return None;
        }
        self.refresh_diagnostics();
        if self.language.pending.as_ref().is_some_and(|pending| {
            pending.document != self.editor.document().id()
                || pending.revision != self.editor.document().revision()
                || pending.selections != *self.editor.selections()
                || pending.mode != self.editor.mode()
                || pending.window != self.focused_window_id()
        }) {
            self.cancel_language_request();
        }
        let language = self.editor.language();
        let server = language.and_then(Language::server);
        let path = server.is_some().then(|| self.files.target()).flatten();
        let identity_changed = match (&self.language.document, path) {
            (Some(old), Some(path)) => {
                old.path != path
                    || old.snapshot.id() != self.editor.document().id()
                    || Some(old.language) != language
            }
            (None, None) => false,
            _ => true,
        };
        if identity_changed {
            self.workspace.synchronize = true;
            self.language.cancel();
            self.signature.reset();
            self.language.signature = None;
            self.rename.clear();
            self.actions.clear();
            self.formatting.clear();
            self.completion.clear();
            self.language.completion = None;
            self.language.epoch += 1;
            self.language.diagnostics.clear();
            self.language.popup = None;
            self.language.status = if path.is_some() { "starting" } else { "" };
        }
        let changed = identity_changed
            || self.language.force
            || self.language.document.as_ref().is_some_and(|old| {
                old.snapshot.revision() != self.editor.document().revision()
                    || old.saved != self.language.saved
            });
        let document = path.map(|path| vex_lsp::Document {
            epoch: self.language.epoch,
            restart: self.language.restart,
            language: language.unwrap(),
            path: path.into(),
            snapshot: self.editor.document().snapshot(),
            saved: self.language.saved,
            saved_snapshot: self
                .language
                .saved_snapshot
                .as_ref()
                .filter(|snapshot| snapshot.id() == self.editor.document().id())
                .cloned(),
        });
        let mut request = None;
        let requested = match action {
            Some(LanguageAction::Hover) => Some(super::completion::Request {
                kind: RequestKind::Hover,
                automatic: false,
            }),
            Some(LanguageAction::SignatureHelp) => Some(super::completion::Request {
                kind: RequestKind::SignatureHelp,
                automatic: false,
            }),
            Some(LanguageAction::Definition) => Some(super::completion::Request {
                kind: RequestKind::Navigation(Navigation::Definition),
                automatic: false,
            }),
            Some(LanguageAction::TypeDefinition) => Some(super::completion::Request {
                kind: RequestKind::Navigation(Navigation::TypeDefinition),
                automatic: false,
            }),
            Some(LanguageAction::Implementation) => Some(super::completion::Request {
                kind: RequestKind::Navigation(Navigation::Implementation),
                automatic: false,
            }),
            Some(LanguageAction::References) => Some(super::completion::Request {
                kind: RequestKind::Navigation(Navigation::References),
                automatic: false,
            }),
            Some(LanguageAction::DocumentHighlights) => Some(super::completion::Request {
                kind: RequestKind::DocumentHighlights,
                automatic: false,
            }),
            Some(LanguageAction::Rename) => Some(super::completion::Request {
                kind: self.prepare_rename_request(),
                automatic: false,
            }),
            Some(LanguageAction::FormatSelections) => Some(super::completion::Request {
                kind: self.prepare_formatting_request(false),
                automatic: false,
            }),
            Some(LanguageAction::FormatDocument) => Some(super::completion::Request {
                kind: self.prepare_formatting_request(true),
                automatic: false,
            }),
            Some(LanguageAction::CodeAction) => Some(super::completion::Request {
                kind: self.prepare_code_actions_request(),
                automatic: false,
            }),
            Some(LanguageAction::Completion) => Some(super::completion::Request {
                kind: RequestKind::Completion(CompletionTrigger::Invoked),
                automatic: false,
            }),
            _ => self
                .take_command_request()
                .or_else(|| self.take_code_action_request())
                .or_else(|| self.take_rename_request())
                .or_else(|| self.take_symbol_request(Instant::now()))
                .map(|kind| super::completion::Request {
                    kind,
                    automatic: false,
                })
                .or_else(|| self.take_completion_request())
                .or_else(|| self.take_signature_request(Instant::now())),
        };
        if let Some(super::completion::Request { kind, automatic }) = requested {
            if automatic
                && matches!(kind, RequestKind::SignatureHelp)
                && self.signature_options().is_none()
            {
                self.signature.dismiss();
            } else if automatic
                && !matches!(kind, RequestKind::SignatureHelp)
                && self.completion_options().is_none()
            {
                self.dismiss_language_help();
            } else if document.is_none() {
                self.fail_language_request(
                    "language services require a named file with a configured language server"
                        .into(),
                );
            } else if self.language.status == "unavailable" {
                self.fail_language_request(
                    "language server is unavailable; use :lsp-restart to retry".into(),
                );
            } else if self.editor.document().text().len_bytes() > vex_lsp::MAX_DOCUMENT_BYTES {
                self.fail_language_request("document exceeds the initial 8 MiB LSP limit".into());
            } else {
                if self.signature_request_pending() {
                    self.signature.interrupted();
                }
                self.language.cancel();
                if matches!(kind, RequestKind::Completion(_)) {
                    self.begin_completion(automatic);
                }
                if matches!(kind, RequestKind::SignatureHelp) {
                    self.begin_signature();
                }
                self.language.next_request += 1;
                let cancellation = Cancellation::default();
                self.language.pending = Some(Pending {
                    id: self.language.next_request,
                    revision: self.editor.document().revision(),
                    document: self.editor.document().id(),
                    selections: self.editor.selections().clone(),
                    mode: self.editor.mode(),
                    cancellation: cancellation.clone(),
                    executing: matches!(kind, RequestKind::ExecuteCommand { .. }),
                    window: self.focused_window_id(),
                    signature: matches!(kind, RequestKind::SignatureHelp),
                    automatic,
                    waiting: matches!(
                        kind,
                        RequestKind::Navigation(_)
                            | RequestKind::DocumentHighlights
                            | RequestKind::PrepareRename { .. }
                            | RequestKind::Rename { .. }
                            | RequestKind::Format { .. }
                            | RequestKind::CodeActions { .. }
                            | RequestKind::ApplyCodeAction { .. }
                            | RequestKind::ExecuteCommand { .. }
                    ),
                });
                request = Some(vex_lsp::Request {
                    id: self.language.next_request,
                    kind,
                    position: self.language_cursor(),
                    cancellation,
                });
                if !automatic {
                    self.message = format!("waiting for {}...", server.unwrap().command);
                }
            }
        }
        if let Some(
            action @ (LanguageAction::NextDiagnostic
            | LanguageAction::PreviousDiagnostic
            | LanguageAction::FirstDiagnostic
            | LanguageAction::LastDiagnostic),
        ) = action
        {
            self.navigate_diagnostic(action);
        }
        self.language.force = false;
        self.language.document = document.clone();
        (changed || request.is_some()).then_some(vex_lsp::Update { document, request })
    }

    pub fn handle_lsp_event(&mut self, event: Event) -> bool {
        match event {
            Event::DiagnosticCatalog(catalog) => {
                let changed = !self.language.diagnostic_catalog.same_catalog(&catalog);
                self.language.diagnostic_catalog = catalog;
                self.refresh_diagnostic_picker(changed);
            }
            Event::ApplyEdit {
                epoch,
                request_id,
                edit,
                versions,
                reply,
            } => {
                return self.receive_server_edit(epoch, request_id, edit, versions, reply);
            }
            Event::ApplyEditFinished {
                epoch,
                cancellation,
                result,
            } => {
                return self.finish_server_edit(epoch, cancellation, result);
            }
            Event::Capabilities {
                epoch,
                completion,
                signature,
            } => {
                if epoch != self.language.epoch || self.language.document.is_none() {
                    return false;
                }
                self.language.completion = completion;
                self.language.signature = signature;
            }
            Event::Status {
                epoch,
                message,
                failed,
            } => {
                if epoch != self.language.epoch || self.language.document.is_none() {
                    return false;
                }
                let starting = self.language.status != "ready";
                let requested = self.language.pending.is_some();
                self.language.status = if failed {
                    "unavailable"
                } else if message.ends_with("ready") {
                    "ready"
                } else {
                    "starting"
                };
                if failed {
                    self.cancel_server_edit();
                    self.signature.reset();
                    self.language.signature = None;
                    self.language.completion = None;
                    self.language.cancel();
                    if starting && !requested {
                        // Language servers are optional. Opening a file should
                        // only show LSP:down when its server cannot start.
                        self.dismiss_language_help();
                    } else {
                        self.fail_language_request(if starting {
                            "language server is unavailable; use :lsp-restart to retry".into()
                        } else {
                            message
                        });
                    }
                }
            }
            Event::Diagnostics {
                epoch,
                revision,
                diagnostics,
            } => {
                return self.receive_diagnostics(epoch, revision, diagnostics, Instant::now());
            }
            Event::Answer {
                epoch,
                revision,
                id,
                result,
            } => {
                if !self.current_language_revision(epoch, revision)
                    || !self.language.pending.as_ref().is_some_and(|pending| {
                        pending.id == id
                            && !pending.cancellation.is_cancelled()
                            && pending.selections == *self.editor.selections()
                            && pending.document == self.editor.document().id()
                            && pending.mode == self.editor.mode()
                            && pending.window == self.focused_window_id()
                    })
                {
                    return false;
                }
                let pending = self.language.pending.take().unwrap();
                match result {
                    Ok(Answer::SignatureHelp(response)) => {
                        self.receive_signature(response, pending.automatic)
                    }
                    Ok(Answer::Hover(text)) if text.is_empty() => {
                        self.message = "no hover information".into()
                    }
                    Ok(Answer::Hover(text)) => {
                        self.message = "hover · Ctrl-u/Ctrl-d scroll".into();
                        self.language.popup = Some(crate::documentation::Popup::new(text));
                    }
                    Ok(Answer::Locations(kind, locations)) => {
                        self.receive_locations(kind, locations)
                    }
                    Ok(Answer::DocumentHighlights(Some(selections))) => {
                        if self.editor.apply_prepared_selections(selections) {
                            self.clear_message();
                        }
                    }
                    Ok(Answer::DocumentHighlights(None)) => {
                        self.message = "no document references found".into()
                    }
                    Ok(Answer::Completion(items)) => self.receive_completions(items),
                    Ok(Answer::Symbols(symbols)) => self.receive_symbols(symbols),
                    Ok(Answer::CompletionResolved(item)) => self.receive_resolved_completion(item),
                    Ok(Answer::RenamePrepared(name)) => self.receive_rename_preparation(name),
                    Ok(Answer::Formatted { edit, versions }) => {
                        self.receive_formatting(edit, versions)
                    }
                    Ok(Answer::CodeActions(actions)) => self.receive_code_actions(actions),
                    Ok(Answer::CodeActionReady(action)) => self.receive_code_action_ready(action),
                    Ok(Answer::CommandExecuted) => {
                        self.message = "language server command completed".into()
                    }
                    Ok(Answer::WorkspaceEdit { edit, versions }) => {
                        self.receive_rename_edit(edit, versions)
                    }
                    Err(error) if pending.signature => {
                        self.signature.dismiss();
                        if !pending.automatic {
                            self.fail(error);
                        }
                    }
                    Err(error) => self.fail_language_request(error),
                }
            }
        }
        true
    }

    fn fail_language_request(&mut self, error: String) {
        self.rename.clear();
        self.actions.clear();
        self.formatting.clear();
        if self.fail_symbol_picker(&error) {
            self.cancel_language_request();
            self.clear_message();
        } else {
            self.fail_completion(error);
        }
    }

    pub(super) fn completion_options(&self) -> Option<&CompletionOptions> {
        self.language_ready()
            .then_some(self.language.completion.as_ref())
            .flatten()
    }

    pub(super) fn signature_options(&self) -> Option<&vex_lsp::SignatureOptions> {
        self.language_ready()
            .then_some(self.language.signature.as_ref())
            .flatten()
    }

    pub(super) fn language_request_pending(&self) -> bool {
        self.language.pending.is_some()
    }

    pub(super) fn signature_request_pending(&self) -> bool {
        self.language
            .pending
            .as_ref()
            .is_some_and(|pending| pending.signature)
    }

    fn language_ready(&self) -> bool {
        if !self.language.enabled
            || self.language.status != "ready"
            || self.editor.language().and_then(Language::server).is_none()
            || self.editor.document().text().len_bytes() > vex_lsp::MAX_DOCUMENT_BYTES
            || !self.language.document.as_ref().is_some_and(|document| {
                document.snapshot.id() == self.editor.document().id()
                    && Some(document.language) == self.editor.language()
                    && self.files.target() == Some(document.path.as_path())
            })
        {
            return false;
        }
        true
    }

    fn current_language_revision(&self, epoch: u64, revision: Revision) -> bool {
        epoch == self.language.epoch
            && revision == self.editor.document().revision()
            && self.language.document.as_ref().is_some_and(|doc| {
                doc.snapshot.id() == self.editor.document().id()
                    && Some(doc.language) == self.editor.language()
            })
    }

    fn language_cursor(&self) -> CharOffset {
        if self.editor.mode() == Mode::Insert {
            self.editor.selections().primary().head
        } else {
            motion::cursor(
                self.editor.document().text(),
                self.editor.selections().primary(),
            )
            .unwrap()
        }
    }

    fn move_to(&mut self, position: CharOffset) -> io::Result<()> {
        self.editor
            .execute("normal_mode", 1)
            .map_err(io::Error::other)?;
        let selection =
            motion::block(self.editor.document().text(), position).map_err(io::Error::other)?;
        self.editor
            .set_selections(SelectionSet::single(selection))
            .map_err(io::Error::other)?;
        self.keys.cancel(&mut self.editor);
        Ok(())
    }

    pub(super) fn open_location(&mut self, location: vex_lsp::Location) -> io::Result<()> {
        let origin = self.current_jump();
        if self.files.target() == Some(location.path.as_path()) {
            let offset = vex_lsp::offset(self.editor.document().text(), location.position)
                .ok_or_else(|| io::Error::other("invalid symbol position"))?;
            self.move_to(offset)?;
        } else {
            if self.snapshot_for_path(&location.path).is_none()
                && !std::fs::metadata(&location.path)?.is_file()
            {
                return Err(io::Error::other("symbol location is not a regular file"));
            }
            let offset = self.open_window_definition(&location)?;
            self.move_to(offset)?;
        }
        self.push_jump(origin);
        self.clear_message();
        Ok(())
    }

    fn navigate_diagnostic(&mut self, action: LanguageAction) {
        let cursor = self.language_cursor();
        let diagnostics = &self.language.diagnostics;
        let diagnostic = match action {
            LanguageAction::FirstDiagnostic => diagnostics.first(),
            LanguageAction::LastDiagnostic => diagnostics.last(),
            LanguageAction::NextDiagnostic => diagnostics
                .get(diagnostics.partition_point(|diagnostic| diagnostic.start <= cursor)),
            LanguageAction::PreviousDiagnostic => diagnostics
                .partition_point(|diagnostic| diagnostic.start < cursor)
                .checked_sub(1)
                .and_then(|index| diagnostics.get(index)),
            _ => unreachable!(),
        };
        let Some(diagnostic) = diagnostic else {
            return;
        };
        let selection = if action == LanguageAction::PreviousDiagnostic {
            vex_core::Selection::new(diagnostic.end, diagnostic.start)
        } else {
            vex_core::Selection::new(diagnostic.start, diagnostic.end)
        };
        self.begin_diagnostic_navigation(selection, diagnostic.message.clone());
    }

    pub(super) fn refresh_diagnostics(&mut self) {
        self.observe_diagnostics(Instant::now());
    }

    fn receive_diagnostics(
        &mut self,
        epoch: u64,
        revision: Revision,
        diagnostics: Vec<Diagnostic>,
        now: Instant,
    ) -> bool {
        if !self.current_language_revision(epoch, revision) {
            return false;
        }
        let redraw = self.poll_diagnostics(now);
        if self.language.diagnostic_delay.due.is_some() {
            self.language.diagnostic_delay.pending = Some(diagnostics);
            return redraw;
        }
        self.publish_diagnostics(diagnostics);
        true
    }

    fn observe_diagnostics(&mut self, now: Instant) -> bool {
        let current = DiagnosticContext {
            epoch: self.language.epoch,
            document: self.editor.document().id(),
            revision: self.editor.document().revision(),
            saved: self.language.saved,
        };
        let delay = &mut self.language.diagnostic_delay;
        let previous = delay.observed.replace(current);
        let session_changed = previous
            .is_none_or(|old| old.epoch != current.epoch || old.document != current.document);
        let edited = previous.is_some_and(|old| old.revision != current.revision);
        if session_changed || edited {
            delay.pending = None;
            delay.due = (!session_changed).then_some(now + vex_lsp::EDIT_IDLE_DELAY);
        }
        if previous.is_some_and(|old| old.saved != current.saved) {
            delay.due = None;
        }
        let mut redraw = false;
        if self.language.diagnostic_revision != Some((current.document, current.revision))
            || session_changed
        {
            redraw = !self.language.diagnostics.is_empty();
            self.language.diagnostics.clear();
            self.language.diagnostic_revision = None;
        }
        redraw
    }

    /// Diagnostic display waits for the latest edit even when completion or
    /// signature help flushes didChange before the ordinary sync deadline.
    /// Only callers that propagate redraws publish deferred results.
    pub(crate) fn poll_diagnostics(&mut self, now: Instant) -> bool {
        let mut redraw = self.observe_diagnostics(now);
        let delay = &mut self.language.diagnostic_delay;
        if delay.due.is_some_and(|due| now >= due) {
            delay.due = None;
        }
        if delay.due.is_none()
            && let Some(diagnostics) = delay.pending.take()
        {
            self.publish_diagnostics(diagnostics);
            redraw = true;
        }
        redraw
    }

    pub(crate) fn diagnostic_deadline(&self) -> Option<Instant> {
        self.language.diagnostic_delay.pending.as_ref()?;
        Some(
            self.language
                .diagnostic_delay
                .due
                .unwrap_or_else(Instant::now),
        )
    }

    fn publish_diagnostics(&mut self, mut diagnostics: Vec<Diagnostic>) {
        diagnostics.sort_by_key(|diagnostic| (diagnostic.start, diagnostic.severity));
        self.language.diagnostics = diagnostics;
        self.language.diagnostic_revision = Some((
            self.editor.document().id(),
            self.editor.document().revision(),
        ));
    }

    pub(super) fn diagnostic_message(&self) -> String {
        let cursor = self.language_cursor();
        self.language
            .diagnostics
            .iter()
            .find(|diagnostic| {
                diagnostic.start == cursor || (diagnostic.start < cursor && cursor < diagnostic.end)
            })
            .map(|diagnostic| diagnostic.message.clone())
            .unwrap_or_default()
    }

    pub(super) fn language_status(&self) -> String {
        if !matches!(self.language.status, "starting" | "ready") {
            return "LSP:down".into();
        }
        let errors = self
            .language
            .diagnostics
            .iter()
            .filter(|d| d.severity == 1)
            .count();
        let warnings = self
            .language
            .diagnostics
            .iter()
            .filter(|d| d.severity == 2)
            .count();
        format!("LSP:{} {errors}E {warnings}W", self.language.status)
    }

    pub(super) fn paint_language(&mut self, frame: &mut Frame, body_height: u16) {
        if let Some(column) = crate::render::gutter(
            usize::from(frame.width()),
            self.editor.document().text().len_lines(),
        )
        .diagnostic
        {
            for row in 0..body_height {
                let line = self.viewport.top_line + usize::from(row);
                if let Some(severity) = self
                    .language
                    .diagnostics
                    .iter()
                    .filter(|d| d.line == line)
                    .map(|d| d.severity)
                    .min()
                {
                    frame.put(
                        column,
                        row,
                        if severity == 1 { "!" } else { "·" },
                        if severity == 1 {
                            Style::Error
                        } else {
                            Style::Message
                        },
                    );
                }
            }
        }
        if let Some(popup) = &mut self.language.popup {
            popup.paint(frame, body_height);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{Event as TerminalEvent, KeyCode, KeyEvent, KeyModifiers};
    use std::fs;

    fn app(directory: &std::path::Path) -> App {
        let path = directory.join("main.rs");
        fs::write(&path, "fn target() {}\nfn main() { target(); }\n").unwrap();
        let mut app = App::open(Some(&path), (80, 12)).unwrap();
        app.enable_lsp();
        app.take_lsp_update().unwrap();
        app
    }
    fn issue(app: &mut App, command: &str) -> (u64, Revision, u64) {
        app.execute(command).unwrap();
        let update = app.take_lsp_update().unwrap();
        (
            update.document.unwrap().epoch,
            app.editor.document().revision(),
            update.request.unwrap().id,
        )
    }
    fn answer(app: &mut App, key: (u64, Revision, u64), result: Answer) -> bool {
        app.handle_lsp_event(Event::Answer {
            epoch: key.0,
            revision: key.1,
            id: key.2,
            result: Ok(result),
        })
    }
    fn press(app: &mut App, code: KeyCode) {
        app.handle(TerminalEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
        app.take_lsp_update();
    }

    fn diagnostics_at(app: &mut App, message: Option<&str>, now: Instant) -> bool {
        let diagnostics = message
            .into_iter()
            .map(|message| Diagnostic {
                start: CharOffset(0),
                end: CharOffset(1),
                line: 0,
                severity: 1,
                message: message.into(),
            })
            .collect();
        app.receive_diagnostics(
            app.language.epoch,
            app.editor.document().revision(),
            diagnostics,
            now,
        )
    }

    #[test]
    fn buffer_lifetimes_publish_catalogs_without_recapturing_every_keystroke() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path());
        let first = app.editor.document().id();
        let initial = app.take_lsp_workspace_update().unwrap();
        assert_eq!(initial.documents.len(), 1);
        let epoch = app.language.epoch;
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("// unsaved\n").unwrap();
        app.editor.execute("normal_mode", 1).unwrap();
        let typed = app.take_lsp_update().unwrap().document.unwrap();
        assert_eq!(typed.epoch, epoch);
        assert_eq!(typed.restart, 0);
        assert!(app.take_lsp_workspace_update().is_none());
        let path = directory.path().join("other.rs");
        fs::write(&path, "fn other() {}\n").unwrap();
        app.open_window_file(&path).unwrap();
        let switched = app.take_lsp_update().unwrap().document.unwrap();
        assert!(switched.epoch > epoch);
        assert_eq!(switched.restart, 0);
        let captured = app.take_lsp_workspace_update().unwrap();
        assert_eq!(captured.documents.len(), 2);
        let hidden = captured
            .documents
            .iter()
            .find(|doc| doc.snapshot.id() == first)
            .unwrap();
        assert_eq!(hidden.snapshot.revision(), typed.snapshot.revision());
        app.execute("bc").unwrap();
        app.take_lsp_update().unwrap();
        assert_eq!(app.editor.document().id(), first);
        assert_eq!(app.take_lsp_workspace_update().unwrap().documents.len(), 1);
        app.execute("lsp-restart").unwrap();
        assert_eq!(app.take_lsp_update().unwrap().document.unwrap().restart, 1);
        app.take_lsp_workspace_update().unwrap();
        let saved = directory.path().join("renamed.rs");
        app.execute(&format!("w {}", saved.display())).unwrap();
        let update = app.take_lsp_update().unwrap().document.unwrap();
        let captured = app.take_lsp_workspace_update().unwrap();
        assert_eq!(captured.documents.len(), 1);
        assert_eq!(captured.documents[0].path, update.path);
        assert!(update.path.ends_with("renamed.rs"));
        app.execute("language text").unwrap();
        assert!(app.take_lsp_update().unwrap().document.is_none());
        assert_eq!(
            app.take_lsp_workspace_update().unwrap().documents[0].language,
            None
        );
        app.execute("lsp-restart").unwrap();
        assert_eq!(app.language.restart, 1);
    }

    #[test]
    fn diagnostics_wait_for_idle_even_when_requests_flush_current_text() {
        use std::time::Duration;
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path());
        let now = Instant::now();
        assert!(diagnostics_at(&mut app, Some("initial"), now));
        assert_eq!(app.language.diagnostics[0].message, "initial");
        app.editor.execute("insert_mode", 1).unwrap();
        app.handle_at(
            TerminalEvent::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            now,
        );
        assert!(app.language.diagnostics.is_empty());
        assert!(!diagnostics_at(&mut app, Some("incomplete syntax"), now));
        let due = now + vex_lsp::EDIT_IDLE_DELAY;
        assert_eq!(app.diagnostic_deadline(), Some(due));
        let (_, revision, _) = issue(&mut app, "signature_help");
        assert_eq!(revision, app.editor.document().revision());
        assert_eq!(app.diagnostic_deadline(), Some(due));
        app.handle_at(
            TerminalEvent::Resize(80, 12),
            now + Duration::from_millis(100),
        );
        assert_eq!(app.diagnostic_deadline(), Some(due));
        assert!(!app.poll_diagnostics(due - Duration::from_millis(1)));
        assert!(!diagnostics_at(
            &mut app,
            Some("latest result"),
            due - Duration::from_millis(1)
        ));
        assert!(app.language.diagnostics.is_empty());
        // Metadata refresh during event batching must not consume the pending
        // repaint before the event loop polls the deadline.
        app.observe_diagnostics(due);
        assert!(app.language.diagnostics.is_empty());
        assert_eq!(app.diagnostic_deadline(), Some(due));
        assert!(
            app.poll_diagnostics(due),
            "idle deadline must request a redraw without a key"
        );
        assert_eq!(app.language.diagnostics[0].message, "latest result");
        assert!(app.diagnostic_deadline().is_none());
        assert!(!app.poll_diagnostics(due + Duration::from_secs(1)));
    }

    #[test]
    fn more_typing_discards_pending_diagnostics_and_coalesces_clears() {
        use std::time::Duration;
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path());
        let now = Instant::now();
        app.poll_diagnostics(now);
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("x").unwrap();
        app.poll_diagnostics(now);
        diagnostics_at(&mut app, Some("old syntax"), now);
        let old_revision = app.editor.document().revision();
        app.editor.insert_text("y").unwrap();
        let edited = now + Duration::from_millis(200);
        app.poll_diagnostics(edited);
        assert!(app.language.diagnostic_delay.pending.is_none());
        assert!(!app.receive_diagnostics(app.language.epoch, old_revision, Vec::new(), edited));
        diagnostics_at(&mut app, Some("temporary error"), edited);
        diagnostics_at(&mut app, None, edited + Duration::from_millis(50));
        assert_eq!(
            app.diagnostic_deadline(),
            Some(edited + vex_lsp::EDIT_IDLE_DELAY)
        );
        assert!(!app.poll_diagnostics(now + vex_lsp::EDIT_IDLE_DELAY));
        assert!(app.poll_diagnostics(edited + vex_lsp::EDIT_IDLE_DELAY));
        assert!(app.language.diagnostics.is_empty());
        assert_eq!(
            app.language.diagnostic_revision,
            Some((app.editor.document().id(), app.editor.document().revision()))
        );
    }

    #[test]
    fn saves_flush_diagnostic_display_and_restart_discards_pending_results() {
        use std::time::Duration;
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path());
        let now = Instant::now();
        app.poll_diagnostics(now);
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("x").unwrap();
        app.poll_diagnostics(now);
        diagnostics_at(&mut app, Some("saved error"), now);
        app.execute("w").unwrap();
        assert!(app.poll_diagnostics(now + Duration::from_millis(1)));
        assert_eq!(app.language.diagnostics[0].message, "saved error");
        assert!(app.diagnostic_deadline().is_none());
        app.editor.insert_text("y").unwrap();
        app.poll_diagnostics(now + Duration::from_millis(2));
        diagnostics_at(
            &mut app,
            Some("retired session"),
            now + Duration::from_millis(2),
        );
        let epoch = app.language.epoch;
        app.restart_language_server();
        app.poll_diagnostics(now + Duration::from_millis(3));
        assert!(app.language.diagnostic_delay.pending.is_none());
        assert!(app.diagnostic_deadline().is_none());
        assert!(!app.receive_diagnostics(epoch, app.editor.document().revision(), Vec::new(), now));
        assert!(!app.poll_diagnostics(now + Duration::from_secs(1)));
        assert!(app.language.diagnostics.is_empty());
    }

    #[test]
    fn changing_language_restarts_the_session_and_invalidates_server_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path());
        let mut previous_epoch = app.language.epoch;
        for language in [
            "markdown",
            "shell",
            "typescript",
            "tsx",
            "javascript",
            "jsx",
            "rust",
        ] {
            app.execute(&format!("language {language}")).unwrap();
            let document = app.take_lsp_update().unwrap().document.unwrap();
            assert!(document.epoch > previous_epoch);
            assert_eq!(document.language, Language::from_name(language).unwrap());
            assert_eq!(app.language_status(), "LSP:starting 0E 0W");
            assert!(app.completion_options().is_none());
            assert!(!app.handle_lsp_event(Event::Capabilities {
                signature: None,
                epoch: previous_epoch,
                completion: Some(CompletionOptions::default())
            }));
            app.handle_lsp_event(Event::Capabilities {
                signature: None,
                epoch: document.epoch,
                completion: Some(CompletionOptions {
                    trigger_characters: vec!['.'],
                }),
            });
            app.handle_lsp_event(Event::Status {
                epoch: document.epoch,
                message: "server ready".into(),
                failed: false,
            });
            assert_eq!(app.language_status(), "LSP:ready 0E 0W");
            assert!(app.completion_options().is_some());
            previous_epoch = document.epoch;
        }
        app.execute("language text").unwrap();
        assert!(app.take_lsp_update().unwrap().document.is_none());
        assert!(app.completion_options().is_none());
        assert_eq!(app.language_status(), "LSP:down");
    }

    #[test]
    fn hover_is_rendered_and_late_replies_cannot_survive_keys_edits_or_restart() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path());
        let key = issue(&mut app, "hover");
        assert!(answer(
            &mut app,
            key,
            Answer::Hover("fn target()\nDocumentation".into())
        ));
        let mut frame = Frame::default();
        frame.reset(80, 12).unwrap();
        app.paint(&mut frame).unwrap();
        assert!((0..12).any(|row| frame.row_text(row).contains("Documentation")));
        let selections = app.editor.selections().clone();
        app.handle(TerminalEvent::Key(KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
        )));
        assert!(app.language.popup.is_some());
        assert_eq!(app.editor.selections(), &selections);
        press(&mut app, KeyCode::Esc);
        assert!(app.language.popup.is_none());
        let key = issue(&mut app, "hover");
        press(&mut app, KeyCode::Char('l'));
        press(&mut app, KeyCode::Char('h'));
        assert!(!answer(&mut app, key, Answer::Hover("stale".into())));
        let key = issue(&mut app, "hover");
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text(" ").unwrap();
        app.take_lsp_update();
        assert!(!answer(&mut app, key, Answer::Hover("stale".into())));
        let key = issue(&mut app, "hover");
        app.execute("lsp-restart").unwrap();
        app.take_lsp_update();
        assert!(!answer(&mut app, key, Answer::Hover("stale".into())));
    }

    #[test]
    fn mouse_scrolls_hover_documentation_without_scrolling_the_underlying_file() {
        use crossterm::event::{MouseEvent, MouseEventKind};
        let mut app = App::from_document(
            vex_core::Document::from("text\n".repeat(100).as_str()),
            (80, 12),
        );
        let source = (0..30)
            .map(|line| format!("hover line {line:02}\n"))
            .collect::<String>();
        app.language.popup = Some(crate::documentation::Popup::new(
            vex_syntax::markup::Document::plain(&source),
        ));
        let frame = crate::app::tests::draw(&mut app);
        assert!((0..12).any(|row| frame.row_text(row).contains("hover line 00")));
        let area = app.language.popup.as_ref().unwrap().area.unwrap();
        let before = app.viewport;
        let selections = app.editor.selections().clone();
        app.handle(TerminalEvent::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: area.left + 1,
            row: area.top + 1,
            modifiers: KeyModifiers::NONE,
        }));
        let frame = crate::app::tests::draw(&mut app);
        assert!(!(0..12).any(|row| frame.row_text(row).contains("hover line 00")));
        assert!((0..12).any(|row| frame.row_text(row).contains("hover line 03")));
        assert_eq!(app.viewport, before);
        assert_eq!(app.editor.selections(), &selections);
    }

    #[test]
    fn diagnostics_select_ranges_stop_at_ends_and_disappear_immediately_after_edit() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path());
        let epoch = app.language.epoch;
        let revision = app.editor.document().revision();
        let diagnostics = vec![
            Diagnostic {
                start: CharOffset(3),
                end: CharOffset(9),
                line: 0,
                severity: 1,
                message: "first error".into(),
            },
            Diagnostic {
                start: CharOffset(17),
                end: CharOffset(21),
                line: 1,
                severity: 2,
                message: "second warning".into(),
            },
        ];
        assert!(app.handle_lsp_event(Event::Diagnostics {
            epoch,
            revision,
            diagnostics: diagnostics.clone()
        }));
        let mut frame = Frame::default();
        frame.reset(80, 12).unwrap();
        app.paint(&mut frame).unwrap();
        assert!(frame.row_text(0).starts_with('!'));
        assert!(frame.row_text(10).contains("1E 1W"));
        for (command, anchor, head, moves) in [
            ("goto_next_diagnostic", 3, 9, true),
            ("goto_next_diagnostic", 17, 21, true),
            ("goto_next_diagnostic", 17, 21, false),
            ("goto_previous_diagnostic", 21, 17, true),
            ("goto_previous_diagnostic", 9, 3, true),
            ("goto_previous_diagnostic", 9, 3, false),
            ("goto_last_diagnostic", 17, 21, true),
            ("goto_first_diagnostic", 3, 9, true),
        ] {
            // Helix diagnostic motions ignore a numeric prefix.
            app.editor.execute(command, 99).unwrap();
            app.take_lsp_update();
            let job = app.take_location_navigation();
            assert_eq!(job.is_some(), moves, "{command}");
            if let Some(job) = job {
                assert!(app.input_waiting());
                assert!(app.handle_location_navigation(job.run().unwrap()));
            }
            assert!(!app.input_waiting());
            assert_eq!(
                app.editor.selections().primary(),
                vex_core::Selection::new(CharOffset(anchor), CharOffset(head)),
                "{command}"
            );
            assert_eq!(app.editor.mode(), Mode::Normal);
        }
        app.execute("jump_backward").unwrap();
        assert_eq!(app.editor.selections().primary().anchor, CharOffset(17));
        app.editor.execute("select_mode", 1).unwrap();
        app.editor.execute("goto_first_diagnostic", 1).unwrap();
        app.take_lsp_update();
        let result = app.take_location_navigation().unwrap().run().unwrap();
        app.handle_location_navigation(result);
        assert_eq!(app.editor.mode(), Mode::Select);
        assert_eq!(app.editor.selections().primary().anchor, CharOffset(3));
        assert_eq!(app.editor.selections().primary().head, CharOffset(9));
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("x").unwrap();
        app.paint(&mut frame).unwrap();
        assert!(app.language.diagnostics.is_empty());
        assert!(!app.handle_lsp_event(Event::Diagnostics {
            epoch,
            revision,
            diagnostics
        }));
    }

    #[test]
    fn diagnostic_navigation_rejects_cancelled_and_changed_origins() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path());
        let destination = vex_core::Selection::new(CharOffset(3), CharOffset(9));
        let initial = app.editor.selections().clone();
        app.begin_diagnostic_navigation(destination, "message".into());
        let job = app.take_location_navigation().unwrap();
        press(&mut app, KeyCode::Esc);
        assert!(job.run().is_none());
        assert_eq!(app.editor.selections(), &initial);
        assert!(!app.input_waiting());

        for change in ["move_right", "select_mode", "insert_mode"] {
            app.begin_diagnostic_navigation(destination, "message".into());
            let result = app.take_location_navigation().unwrap().run().unwrap();
            app.editor.execute(change, 1).unwrap();
            if change == "insert_mode" {
                app.editor.insert_text("x").unwrap();
            }
            let expected = app.editor.selections().clone();
            assert!(app.handle_location_navigation(result));
            assert_eq!(app.editor.selections(), &expected);
            assert!(!app.input_waiting());
        }
    }

    #[test]
    fn reference_requests_reject_late_results_and_release_input_on_cancel_error_and_empty() {
        for command in [
            "goto_reference",
            "goto_type_definition",
            "goto_implementation",
            "select_references_to_symbol_under_cursor",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let mut app = app(directory.path());
            let key = issue(&mut app, command);
            assert!(app.input_waiting());
            press(&mut app, KeyCode::Esc);
            assert!(!app.input_waiting());
            assert!(!answer(&mut app, key, Answer::DocumentHighlights(None)));
            let key = issue(&mut app, command);
            assert!(app.handle_lsp_event(Event::Answer {
                epoch: key.0,
                revision: key.1,
                id: key.2,
                result: Err("unsupported request".into())
            }));
            assert!(!app.input_waiting());
            assert!(app.message.contains("unsupported request"));
            let key = issue(&mut app, command);
            app.editor.execute("move_right", 1).unwrap();
            assert!(!answer(&mut app, key, Answer::DocumentHighlights(None)));
            app.take_lsp_update();
            assert!(!app.input_waiting());
            let key = issue(&mut app, command);
            assert!(answer(&mut app, key, Answer::DocumentHighlights(None)));
            assert!(!app.input_waiting());
        }
    }

    #[test]
    fn definitions_protect_unsaved_files_and_jump_back_restores_the_origin() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path());
        let origin = app.files.target().unwrap().to_path_buf();
        let target = directory.path().join("other.rs");
        fs::write(&target, "pub fn there() {}\n").unwrap();
        let target = fs::canonicalize(target).unwrap();
        let location = vex_lsp::Location {
            path: target.clone(),
            position: vex_lsp::Position {
                line: 0,
                character: 7,
            },
        };
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text(" ").unwrap();
        let key = issue(&mut app, "goto_definition");
        assert!(answer(
            &mut app,
            key,
            Answer::Locations(
                Navigation::Definition,
                vex_lsp::Locations {
                    items: vec![vex_lsp::Destination {
                        path: location.path.clone(),
                        range: vex_lsp::Range {
                            start: location.position,
                            end: location.position
                        }
                    }],
                    ..Default::default()
                }
            )
        ));
        let result = app.take_location_navigation().unwrap().run().unwrap();
        assert!(app.handle_location_navigation(result));
        app.take_lsp_update();
        assert_eq!(app.files.target(), Some(target.as_path()));
        assert_eq!(app.language_cursor(), CharOffset(7));
        app.execute("jump_back").unwrap();
        app.take_lsp_update();
        assert_eq!(app.files.target(), Some(origin.as_path()));
        assert!(app.is_dirty());
        assert!(app.editor.document().text().to_string().starts_with(' '));
        app.execute("jump_forward").unwrap();
        assert_eq!(app.files.target(), Some(target.as_path()));
    }

    #[test]
    fn save_as_changes_session_and_unavailable_services_report_without_waiting() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path());
        let old_epoch = app.language.epoch;
        let path = directory.path().join("renamed.rs");
        app.execute(&format!("write {}", path.display())).unwrap();
        let updated = app.take_lsp_update().unwrap().document.unwrap();
        assert!(updated.epoch > old_epoch);
        assert_eq!(updated.saved, 1);
        let message = app.message.clone();
        let error = app.error;
        app.handle_lsp_event(Event::Status {
            epoch: updated.epoch,
            failed: true,
            message: "failed to start language server: executable not found".into(),
        });
        assert_eq!(app.language_status(), "LSP:down");
        assert_eq!(app.message, message);
        assert_eq!(app.error, error);
        app.execute("hover").unwrap();
        assert!(app.take_lsp_update().is_none());
        assert!(app.language.pending.is_none());
        assert!(app.message.contains("lsp-restart"));
        app.execute("language text").unwrap();
        assert!(app.take_lsp_update().unwrap().document.is_none());

        // A command issued during startup must still finish and release queued
        // input, with a short availability message instead of a spawn error.
        app.execute("language rust").unwrap();
        let key = issue(&mut app, "goto_definition");
        assert!(app.input_waiting());
        assert!(app.handle_lsp_event(Event::Status {
            epoch: key.0,
            failed: true,
            message: "failed to start language server: executable not found".into(),
        }));
        assert!(!app.input_waiting());
        assert_eq!(app.language_status(), "LSP:down");
        assert_eq!(
            app.message,
            "language server is unavailable; use :lsp-restart to retry"
        );
    }
}
