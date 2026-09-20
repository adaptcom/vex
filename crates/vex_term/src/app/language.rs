//! Main-thread language-service state, navigation, and presentation.

use super::App;
use crate::screen::{Frame, Style};
use std::{collections::VecDeque, io, path::PathBuf, time::Instant};
use vex_core::{CharOffset, DocumentId, Revision, SelectionSet, motion};
use vex_editor::{Language, LanguageAction, Mode, background::Cancellation};
use vex_lsp::{Answer, CompletionOptions, CompletionTrigger, Diagnostic, Event, RequestKind};

struct Pending {
    id: u64,
    revision: Revision,
    document: DocumentId,
    selections: SelectionSet,
    mode: Mode,
    cancellation: Cancellation,
}

#[derive(Default)]
pub(super) struct State {
    enabled: bool,
    epoch: u64,
    document: Option<vex_lsp::Document>,
    force: bool,
    next_request: u64,
    pending: Option<Pending>,
    diagnostics: Vec<Diagnostic>,
    diagnostic_revision: Option<(DocumentId, Revision)>,
    popup: Option<String>,
    status: &'static str,
    completion: Option<CompletionOptions>,
    jumps: VecDeque<(PathBuf, CharOffset)>,
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
    pub(super) fn record_jump(&mut self) {
        if let Some(path) = self.files.target() {
            let origin = (path.to_path_buf(), self.language_cursor());
            if self.language.jumps.len() == 32 {
                self.language.jumps.pop_front();
            }
            self.language.jumps.push_back(origin);
        }
    }
    pub fn enable_lsp(&mut self) {
        self.language.enabled = true;
        self.language.force = true;
    }

    pub(super) fn dismiss_language_help(&mut self) {
        self.language.cancel();
        self.language.popup = None;
        self.completion.clear();
    }

    pub(super) fn cancel_language_request(&mut self) {
        self.language.cancel();
    }

    pub(super) fn restart_language_server(&mut self) {
        self.dismiss_language_help();
        self.language.epoch += 1;
        self.language.document = None;
        self.language.completion = None;
        self.language.force = true;
        self.language.diagnostics.clear();
        self.message = "restarting language server".into();
    }

    /// Called after dispatch and drawing. Only shared snapshots and small
    /// metadata cross this boundary; JSON and UTF-16 work run on the service.
    pub fn take_lsp_update(&mut self) -> Option<vex_lsp::Update> {
        self.invalidate_symbol_picker();
        self.poll_completion(Instant::now());
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
        }) {
            self.language.cancel();
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
            self.language.cancel();
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
            Some(LanguageAction::Definition) => Some(super::completion::Request {
                kind: RequestKind::Definition,
                automatic: false,
            }),
            Some(LanguageAction::Completion) => Some(super::completion::Request {
                kind: RequestKind::Completion(CompletionTrigger::Invoked),
                automatic: false,
            }),
            _ => self
                .take_symbol_request(Instant::now())
                .map(|kind| super::completion::Request {
                    kind,
                    automatic: false,
                })
                .or_else(|| self.take_completion_request()),
        };
        if let Some(super::completion::Request { kind, automatic }) = requested {
            if automatic && self.completion_options().is_none() {
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
                self.language.cancel();
                if matches!(kind, RequestKind::Completion(_)) {
                    self.begin_completion(automatic);
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
        match action {
            Some(LanguageAction::NextDiagnostic(count)) => self.navigate_diagnostic(false, count),
            Some(LanguageAction::PreviousDiagnostic(count)) => {
                self.navigate_diagnostic(true, count)
            }
            Some(LanguageAction::JumpBack) => {
                if let Err(error) = self.jump_back() {
                    self.fail(error);
                }
                self.language.force = true;
                return self.take_lsp_update();
            }
            _ => {}
        }
        self.language.force = false;
        self.language.document = document.clone();
        (changed || request.is_some()).then_some(vex_lsp::Update { document, request })
    }

    pub fn handle_lsp_event(&mut self, event: Event) -> bool {
        match event {
            Event::Capabilities { epoch, completion } => {
                if epoch != self.language.epoch || self.language.document.is_none() {
                    return false;
                }
                self.language.completion = completion;
            }
            Event::Status {
                epoch,
                message,
                failed,
            } => {
                if epoch != self.language.epoch || self.language.document.is_none() {
                    return false;
                }
                self.language.status = if failed {
                    "unavailable"
                } else if message.ends_with("ready") {
                    "ready"
                } else {
                    "starting"
                };
                if failed {
                    self.language.completion = None;
                    self.language.cancel();
                    self.fail_language_request(message);
                }
            }
            Event::Diagnostics {
                epoch,
                revision,
                mut diagnostics,
            } => {
                if !self.current_language_revision(epoch, revision) {
                    return false;
                }
                diagnostics.sort_by_key(|diagnostic| (diagnostic.start, diagnostic.severity));
                self.language.diagnostics = diagnostics;
                self.language.diagnostic_revision = Some((self.editor.document().id(), revision));
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
                    })
                {
                    return false;
                }
                self.language.pending = None;
                match result {
                    Ok(Answer::Hover(text)) if text.is_empty() => {
                        self.message = "no hover information".into()
                    }
                    Ok(Answer::Hover(text)) => {
                        self.message = "hover — any key closes".into();
                        self.language.popup = Some(text);
                    }
                    Ok(Answer::Definition(None)) => self.message = "no definition found".into(),
                    Ok(Answer::Definition(Some(location))) => {
                        if let Err(error) = self.open_location(location) {
                            self.fail(error);
                        }
                    }
                    Ok(Answer::Completion(items)) => self.receive_completions(items),
                    Ok(Answer::Symbols(symbols)) => self.receive_symbols(symbols),
                    Ok(Answer::CompletionResolved(item)) => self.receive_resolved_completion(item),
                    Err(error) => self.fail_language_request(error),
                }
            }
        }
        true
    }

    fn fail_language_request(&mut self, error: String) {
        if self.fail_symbol_picker(&error) {
            self.cancel_language_request();
            self.clear_message();
        } else {
            self.fail_completion(error);
        }
    }

    pub(super) fn completion_options(&self) -> Option<&CompletionOptions> {
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
            return None;
        }
        self.language.completion.as_ref()
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
        self.keys.cancel();
        Ok(())
    }

    pub(super) fn open_location(&mut self, location: vex_lsp::Location) -> io::Result<()> {
        let origin = self
            .files
            .target()
            .map(|path| (path.to_path_buf(), self.language_cursor()));
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
        if let Some(origin) = origin {
            if self.language.jumps.len() == 32 {
                self.language.jumps.pop_front();
            }
            self.language.jumps.push_back(origin);
        }
        self.clear_message();
        Ok(())
    }

    fn jump_back(&mut self) -> io::Result<()> {
        let (path, position) = self
            .language
            .jumps
            .back()
            .cloned()
            .ok_or_else(|| io::Error::other("no previous jump"))?;
        if self.files.target() != Some(path.as_path()) {
            self.open_window_file(&path)?;
        }
        self.move_to(CharOffset(
            position.0.min(self.editor.document().text().len_chars()),
        ))?;
        self.language.jumps.pop_back();
        self.clear_message();
        Ok(())
    }

    fn navigate_diagnostic(&mut self, backward: bool, count: usize) {
        if self.language.diagnostics.is_empty() {
            self.fail("no diagnostics");
            return;
        }
        let cursor = self.language_cursor();
        let diagnostics = &self.language.diagnostics;
        let len = diagnostics.len();
        let index = if backward {
            let before = diagnostics.partition_point(|diagnostic| diagnostic.start < cursor);
            (before + len - (count - 1) % len - 1) % len
        } else {
            (diagnostics.partition_point(|diagnostic| diagnostic.start <= cursor)
                + (count - 1) % len)
                % len
        };
        let position = diagnostics[index].start;
        let message = diagnostics[index].message.clone();
        match self.move_to(position) {
            Ok(()) => self.message = message,
            Err(error) => self.fail(error),
        }
    }

    pub(super) fn refresh_diagnostics(&mut self) {
        if self.language.diagnostic_revision
            != Some((
                self.editor.document().id(),
                self.editor.document().revision(),
            ))
        {
            self.language.diagnostics.clear();
            self.language.diagnostic_revision = None;
        }
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
        if self.language.status.is_empty() {
            return String::new();
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
        let label = self
            .editor
            .language()
            .and_then(Language::server)
            .map_or("LSP", |server| server.label);
        format!(" {label}:{} {errors}E {warnings}W", self.language.status)
    }

    pub(super) fn paint_language(&self, frame: &mut Frame, body_height: u16) {
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
        if let Some(popup) = &self.language.popup {
            let rows: Vec<_> = popup
                .lines()
                .take(usize::from(body_height).min(12))
                .collect();
            let top = body_height.saturating_sub(rows.len() as u16);
            for (index, line) in rows.iter().enumerate() {
                frame.fill_row(top + index as u16, Style::Status);
                frame.label(1, top + index as u16, line, Style::Status);
            }
            frame.cursor = None;
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
            assert!(app.completion_options().is_none());
            assert!(!app.handle_lsp_event(Event::Capabilities {
                epoch: previous_epoch,
                completion: Some(CompletionOptions::default())
            }));
            app.handle_lsp_event(Event::Capabilities {
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
            assert!(
                app.language_status()
                    .contains(document.language.server().unwrap().label)
            );
            assert!(app.completion_options().is_some());
            previous_epoch = document.epoch;
        }
        app.execute("language text").unwrap();
        assert!(app.take_lsp_update().unwrap().document.is_none());
        assert!(app.completion_options().is_none());
        assert_eq!(app.language_status(), "");
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
        assert!(frame.row_text(9).contains("Documentation"));
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
    fn diagnostics_render_navigate_wrap_and_disappear_immediately_after_edit() {
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
        for (command, expected) in [
            ("goto_next_diagnostic", 3),
            ("goto_next_diagnostic", 17),
            ("goto_next_diagnostic", 3),
            ("goto_previous_diagnostic", 17),
        ] {
            app.execute(command).unwrap();
            app.take_lsp_update();
            assert_eq!(app.language_cursor(), CharOffset(expected));
        }
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
            Answer::Definition(Some(location.clone()))
        ));
        assert_eq!(app.files.target(), Some(origin.as_path()));
        assert!(app.is_dirty());
        assert!(app.message.contains("save this buffer"));
        app.execute("write").unwrap();
        let key = issue(&mut app, "goto_definition");
        assert!(answer(&mut app, key, Answer::Definition(Some(location))));
        app.take_lsp_update();
        assert_eq!(app.files.target(), Some(target.as_path()));
        assert_eq!(app.language_cursor(), CharOffset(7));
        app.execute("jump_back").unwrap();
        app.take_lsp_update();
        assert_eq!(app.files.target(), Some(origin.as_path()));
        assert!(app.language.jumps.is_empty());
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
        app.handle_lsp_event(Event::Status {
            epoch: updated.epoch,
            failed: true,
            message: "missing executable".into(),
        });
        app.execute("hover").unwrap();
        assert!(app.take_lsp_update().is_none());
        assert!(app.language.pending.is_none());
        assert!(app.message.contains("lsp-restart"));
        app.execute("language text").unwrap();
        assert!(app.take_lsp_update().unwrap().document.is_none());
    }
}
