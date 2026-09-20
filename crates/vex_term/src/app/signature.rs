//! Debounced signature help uses the existing language service and popup painter.
//! Completion/resolve requests take priority; a due signature waits without
//! cancelling them or keeping the event loop awake.

use super::App;
use crate::{
    documentation::{Area, Popup},
    input,
    screen::Frame,
};
use crossterm::event::{Event, KeyEventKind};
use std::time::{Duration, Instant};
use vex_core::{DocumentId, Revision, Selection};
use vex_editor::{Key, Mode};
use vex_lsp::{RequestKind, SignatureHelp};

const DELAY: Duration = Duration::from_millis(120);

#[derive(Clone, Copy, PartialEq, Eq)]
struct Context {
    document: DocumentId,
    revision: Revision,
    selection: Selection,
    mode: Mode,
    window: u64,
}

struct Session {
    popups: Vec<Popup>,
    selected: usize,
    server: Option<usize>,
    limited: bool,
    title: String,
    visible: bool,
}

impl Session {
    fn title(&mut self) {
        self.title = format!(
            " Signature {}/{}{} ",
            self.selected + 1,
            self.popups.len(),
            if self.limited { " · limited" } else { "" }
        );
    }
}

#[derive(Default)]
pub(super) struct State {
    observed: Option<Context>,
    due: Option<Instant>,
    engaged: bool,
    active: Option<Session>,
}

impl State {
    pub(super) fn dismiss(&mut self) {
        self.due = None;
        self.engaged = false;
        self.active = None;
    }

    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(super) fn interrupted(&mut self) {
        if self.engaged
            && self
                .observed
                .is_some_and(|context| context.mode == Mode::Insert)
        {
            self.due = Some(Instant::now() + DELAY);
        }
    }
}

impl App {
    fn signature_context(&self) -> Context {
        Context {
            document: self.editor.document().id(),
            revision: self.editor.document().revision(),
            selection: self.editor.selections().primary(),
            mode: self.editor.mode(),
            window: self.focused_window_id(),
        }
    }

    /// The observed stamp contains only the primary selection; ordinary idle
    /// polls and keystrokes do not clone text or enumerate every selection.
    pub(super) fn observe_signature(&mut self, now: Instant, inserted: bool) {
        let context = self.signature_context();
        if self.signature_options().is_none()
            || self.prompt.is_some()
            || self.picker_query_stamp().is_some()
            || self.active_git_view().is_some()
            || self.editor.repeat_pending()
        {
            self.signature.dismiss();
            self.signature.observed = None;
            return;
        }
        let old = self.signature.observed.replace(context);
        if old == Some(context) {
            return;
        }
        if old.is_some_and(|old| old.document != context.document || old.window != context.window) {
            self.signature.dismiss();
        }
        if context.mode != Mode::Insert {
            self.signature.dismiss();
            return;
        }
        let entered = old.is_none_or(|old| {
            old.mode != Mode::Insert
                || old.document != context.document
                || old.window != context.window
        });
        let triggered = !entered
            && !self.signature.engaged
            && inserted
            && old.is_some_and(|old| old.revision != context.revision)
            && self.signature_options().is_some_and(|options| {
                let text = self.editor.document().text();
                options.trigger_characters.iter().any(|trigger| {
                    let count = trigger.chars().count();
                    context
                        .selection
                        .head
                        .0
                        .checked_sub(count)
                        .is_some_and(|start| {
                            text.slice(start..context.selection.head.0)
                                .chars()
                                .eq(trigger.chars())
                        })
                })
            });
        if entered || triggered || self.signature.engaged {
            self.signature.engaged = true;
            self.signature.due = Some(now + DELAY);
        }
    }

    pub(crate) fn signature_deadline(&self) -> Option<Instant> {
        if self.language_request_pending() {
            None
        } else {
            self.signature.due
        }
    }

    pub(super) fn take_signature_request(
        &mut self,
        now: Instant,
    ) -> Option<super::completion::Request> {
        if self.language_request_pending() || !self.signature.due.is_some_and(|due| due <= now) {
            return None;
        }
        self.signature.due = None;
        Some(super::completion::Request {
            kind: RequestKind::SignatureHelp,
            automatic: true,
        })
    }

    pub(super) fn begin_signature(&mut self) {
        self.signature.observed = Some(self.signature_context());
        self.signature.engaged = true;
        self.signature.due = None;
    }

    pub(super) fn receive_signature(&mut self, response: SignatureHelp, automatic: bool) {
        if response.signatures.is_empty() {
            self.signature.dismiss();
            if !automatic {
                self.message = "no signature help".into();
            }
            return;
        }
        let last = response.signatures.len() - 1;
        let server = response.active_signature.filter(|index| *index <= last);
        let selected = self
            .signature
            .active
            .as_ref()
            .map_or(server.unwrap_or(0), |old| {
                if old.server != server {
                    server.unwrap_or(old.selected)
                } else {
                    old.selected
                }
            })
            .min(last);
        let mut session = Session {
            popups: response.signatures.into_iter().map(Popup::new).collect(),
            selected,
            server,
            limited: response.limited,
            title: String::new(),
            visible: false,
        };
        session.title();
        self.signature.active = Some(session);
        self.signature.engaged = true;
        if !automatic {
            self.clear_message();
        }
    }

    pub(super) fn handle_signature_input(&mut self, event: &Event) -> Option<bool> {
        if matches!(event, Event::FocusLost) {
            self.signature.dismiss();
        }
        let Event::Key(event) = event else {
            return None;
        };
        if event.kind == KeyEventKind::Release {
            return None;
        }
        match input::key(*event) {
            Some(key @ (Key::Ctrl('u' | 'd') | Key::PageUp | Key::PageDown)) => {
                let session = self
                    .signature
                    .active
                    .as_mut()
                    .filter(|session| session.visible)?;
                session.popups[session.selected]
                    .scroll(matches!(key, Key::Ctrl('d') | Key::PageDown));
                Some(true)
            }
            Some(Key::Alt(ch @ ('p' | 'n'))) => {
                let session = self
                    .signature
                    .active
                    .as_mut()
                    .filter(|session| session.visible)?;
                if session.popups.len() <= 1 {
                    return None;
                }
                session.selected = if ch == 'n' {
                    (session.selected + 1) % session.popups.len()
                } else {
                    (session.selected + session.popups.len() - 1) % session.popups.len()
                };
                session.title();
                Some(true)
            }
            Some(Key::Escape | Key::Ctrl('c')) if self.signature.engaged => {
                let visible = self
                    .signature
                    .active
                    .as_ref()
                    .is_some_and(|session| session.visible);
                if self.signature_request_pending() {
                    self.cancel_language_request();
                }
                self.signature.dismiss();
                // Escape leaves insert mode. A visible signature owns Ctrl-c;
                // hidden or pending help lets the completion menu handle it.
                (input::key(*event) == Some(Key::Ctrl('c'))
                    && (visible || !self.completion_visible()))
                .then_some(true)
            }
            _ => None,
        }
    }

    pub(super) fn paint_signature(&mut self, frame: &mut Frame, body: u16, avoid: Option<Area>) {
        if let Some(active) = &mut self.signature.active {
            active.visible =
                active.popups[active.selected].paint_signature(frame, body, &active.title, avoid);
        }
    }

    pub(super) fn signature_mouse(
        &mut self,
        x: u16,
        y: u16,
        scroll: Option<(bool, usize)>,
    ) -> Option<bool> {
        let active = self.signature.active.as_mut()?;
        let popup = &mut active.popups[active.selected];
        if !active.visible || !popup.area.is_some_and(|area| area.contains(x, y)) {
            return None;
        }
        Some(scroll.is_some_and(|(down, count)| popup.scroll_lines(down, count)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use vex_core::{CharOffset, Edit, SelectionSet};
    use vex_lsp::{Answer, CompletionItem, Completions, Event as LspEvent, Update};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }
    fn ctrl(ch: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL))
    }
    fn fixture(source: &str) -> (tempfile::TempDir, App) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.rs");
        std::fs::write(&path, source).unwrap();
        let mut app = App::open(Some(&path), (100, 24)).unwrap();
        app.enable_lsp();
        let epoch = app.take_lsp_update().unwrap().document.unwrap().epoch;
        app.handle_lsp_event(LspEvent::Capabilities {
            epoch,
            completion: Some(vex_lsp::CompletionOptions {
                trigger_characters: vec!['.'],
            }),
            signature: Some(vex_lsp::SignatureOptions {
                trigger_characters: vec!["(".into(), ",".into(), "::".into()],
            }),
        });
        app.handle_lsp_event(LspEvent::Status {
            epoch,
            message: "server ready".into(),
            failed: false,
        });
        app.editor.execute("insert_mode", 1).unwrap();
        let end = app.editor.document().text().len_chars();
        app.editor
            .set_selections(SelectionSet::single(Selection::cursor(CharOffset(end))))
            .unwrap();
        app.observe_signature(Instant::now(), false);
        (directory, app)
    }
    fn expire(app: &mut App) -> Update {
        app.signature.due = Some(Instant::now() - DELAY);
        let update = app.take_lsp_update().unwrap();
        assert!(matches!(
            update.request.as_ref().unwrap().kind,
            RequestKind::SignatureHelp
        ));
        update
    }
    fn reply(app: &mut App, update: Update, result: Result<Answer, String>) -> bool {
        let document = update.document.unwrap();
        app.handle_lsp_event(LspEvent::Answer {
            epoch: document.epoch,
            revision: document.snapshot.revision(),
            id: update.request.unwrap().id,
            result,
        })
    }
    fn signatures(server: Option<usize>) -> Answer {
        Answer::SignatureHelp(SignatureHelp {
            signatures: vec![
                vex_lsp::Documentation::plain("fn call(first: u32, second: u32)"),
                vex_lsp::Documentation::plain("fn call(value: T)"),
            ],
            active_signature: server,
            limited: false,
        })
    }
    fn paint(app: &mut App) -> Frame {
        let mut frame = Frame::default();
        frame.reset(100, 24).unwrap();
        app.paint(&mut frame).unwrap();
        frame
    }

    #[test]
    fn debounce_retrigger_overload_keys_stale_replies_and_insert_keys() {
        let (_directory, mut app) = fixture("call(");
        let due = app.signature_deadline().unwrap();
        assert!(app.take_lsp_update().is_none());
        assert_eq!(app.signature_deadline(), Some(due));
        let update = expire(&mut app);
        assert!(app.signature_deadline().is_none());
        assert!(reply(&mut app, update, Ok(signatures(None))));
        let frame = paint(&mut app);
        assert!(frame.cursor.is_some());
        assert!(app.signature.active.as_ref().unwrap().visible);
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::ALT,
        )));
        assert_eq!(app.signature.active.as_ref().unwrap().selected, 1);
        app.handle(key(KeyCode::Char('1')));
        assert!(app.signature_deadline().is_some());
        let update = expire(&mut app);
        reply(&mut app, update, Ok(signatures(None)));
        assert_eq!(app.signature.active.as_ref().unwrap().selected, 1);
        app.handle(key(KeyCode::Char(',')));
        let old = expire(&mut app);
        app.handle(key(KeyCode::Left));
        assert!(!reply(&mut app, old, Ok(signatures(None))));
        let current = expire(&mut app);
        reply(&mut app, current, Ok(signatures(Some(0))));
        assert_eq!(app.signature.active.as_ref().unwrap().selected, 0);
        app.handle(ctrl('k'));
        assert_eq!(app.editor.document().text(), "call(1");
        assert_eq!(app.editor.mode(), Mode::Insert);
        app.handle(ctrl('c'));
        app.handle(ctrl('u'));
        assert_eq!(app.editor.document().text(), "");
        app.handle(key(KeyCode::Char('(')));
        let late = expire(&mut app);
        app.handle(key(KeyCode::Esc));
        assert_eq!(app.editor.mode(), Mode::Normal);
        assert!(app.signature.active.is_none());
        assert!(app.signature.due.is_none());
        assert!(!reply(&mut app, late, Ok(signatures(None))));
    }

    #[test]
    fn empty_cancelled_unsupported_and_failed_help_stays_quiet_and_can_reopen() {
        let (_directory, mut app) = fixture("call(");
        let update = expire(&mut app);
        reply(
            &mut app,
            update,
            Ok(Answer::SignatureHelp(SignatureHelp::default())),
        );
        assert!(app.signature.due.is_none());
        app.handle(key(KeyCode::Char('1')));
        assert!(app.signature.due.is_none());
        app.handle(key(KeyCode::Char(',')));
        assert!(app.signature.due.is_some());
        let update = expire(&mut app);
        let message = app.message.clone();
        reply(&mut app, update, Err("automatic failure".into()));
        assert_eq!(app.message, message);
        app.handle(key(KeyCode::Char(',')));
        let update = expire(&mut app);
        app.handle(ctrl('c'));
        assert!(!reply(&mut app, update, Ok(signatures(None))));
        assert!(app.signature.due.is_none());
        assert_eq!(app.editor.mode(), Mode::Insert);
        app.restart_language_server();
        app.take_lsp_update();
        app.handle(key(KeyCode::Char('(')));
        assert!(app.signature.due.is_none());
    }

    #[test]
    fn completion_and_resolution_take_priority_and_popups_coexist_without_overlap() {
        let (_directory, mut app) = fixture("\n\n\n\n\n\n\n\ncall(");
        app.signature.due = Some(Instant::now() - DELAY);
        app.editor.execute("completion", 1).unwrap();
        let update = app.take_lsp_update().unwrap();
        assert!(matches!(
            update.request.as_ref().unwrap().kind,
            RequestKind::Completion(_)
        ));
        let cancellation = update.request.as_ref().unwrap().cancellation.clone();
        assert!(app.take_lsp_update().is_none());
        assert!(!cancellation.is_cancelled());
        assert!(app.signature_deadline().is_none());
        let cursor = app.editor.selections().primary().head;
        let mut item = CompletionItem::plain("answer", Edit::insert(cursor, "answer"));
        item.resolved = false;
        reply(
            &mut app,
            update,
            Ok(Answer::Completion(Completions {
                items: vec![item.clone()],
                incomplete: false,
                limited: false,
            })),
        );
        app.handle(key(KeyCode::Tab));
        let resolve = app.take_lsp_update().unwrap();
        assert!(matches!(
            resolve.request.as_ref().unwrap().kind,
            RequestKind::ResolveCompletion(_)
        ));
        item.resolved = true;
        reply(&mut app, resolve, Ok(Answer::CompletionResolved(item)));
        let update = expire(&mut app);
        reply(&mut app, update, Ok(signatures(None)));
        let frame = paint(&mut app);
        let text: String = (0..24).map(|row| frame.row_text(row)).collect();
        assert!(text.contains("Complete"));
        assert!(text.contains("Signature"));
        assert!(frame.cursor.is_some());
        app.handle(ctrl('c'));
        assert!(app.signature.active.is_none());
        assert!(app.completion_visible());
        // Long signature docs should use the available rows above the caret
        // instead of trying the larger area occupied by completion below it.
        app.editor.execute("signature_help", 1).unwrap();
        let update = app.take_lsp_update().unwrap();
        reply(
            &mut app,
            update,
            Ok(Answer::SignatureHelp(SignatureHelp {
                signatures: vec![vex_lsp::Documentation::plain(
                    &"Signature documentation\n".repeat(40),
                )],
                active_signature: None,
                limited: false,
            })),
        );
        paint(&mut app);
        assert!(app.signature.active.as_ref().unwrap().visible);
        // On a short screen the completion menu wins the shared space.
        let mut frame = Frame::default();
        frame.reset(60, 5).unwrap();
        frame.cursor = Some(crate::screen::Cursor {
            x: 3,
            y: 0,
            shape: crate::screen::CursorShape::Bar,
        });
        let area = app.paint_completion(&mut frame, 4);
        app.paint_signature(&mut frame, 4, area);
        assert!(!app.signature.active.as_ref().unwrap().visible);
        assert!((0..5).any(|row| frame.row_text(row).contains("Complete")));
    }

    #[test]
    fn visible_signature_scrolls_with_helix_popup_keys_without_editing_text() {
        let (_directory, mut app) = fixture("call(");
        let update = expire(&mut app);
        let documentation = (0..60)
            .map(|index| format!("Documentation line {index}\n"))
            .collect::<String>();
        reply(
            &mut app,
            update,
            Ok(Answer::SignatureHelp(SignatureHelp {
                signatures: vec![vex_lsp::Documentation::plain(&documentation)],
                active_signature: None,
                limited: false,
            })),
        );
        let frame = paint(&mut app);
        let first: Vec<_> = (0..24).map(|row| frame.row_text(row)).collect();
        for (down, up) in [
            (ctrl('d'), ctrl('u')),
            (key(KeyCode::PageDown), key(KeyCode::PageUp)),
        ] {
            app.handle(down);
            let frame = paint(&mut app);
            assert_ne!(
                first,
                (0..24).map(|row| frame.row_text(row)).collect::<Vec<_>>()
            );
            app.handle(up);
            let frame = paint(&mut app);
            assert_eq!(
                first,
                (0..24).map(|row| frame.row_text(row)).collect::<Vec<_>>()
            );
            assert_eq!(app.editor.document().text(), "call(");
            assert_eq!(app.editor.mode(), Mode::Insert);
        }
    }

    #[test]
    fn manual_signature_command_also_works_in_normal_mode_without_an_insert_binding() {
        let (_directory, mut app) = fixture("call(");
        app.editor.execute("normal_mode", 1).unwrap();
        app.editor.execute("signature_help", 1).unwrap();
        let update = app.take_lsp_update().unwrap();
        assert!(matches!(
            update.request.as_ref().unwrap().kind,
            RequestKind::SignatureHelp
        ));
        reply(&mut app, update, Ok(signatures(None)));
        paint(&mut app);
        assert!(app.signature.active.as_ref().unwrap().visible);
        app.handle(key(KeyCode::Char('h')));
        assert!(app.signature.active.is_none());
    }

    #[test]
    #[ignore = "manual release-mode signature scheduling and cached popup benchmark"]
    fn benchmark_signature_idle_and_cached_paint() {
        let (_directory, mut app) = fixture(&format!("{}\ncall(", "// source\n".repeat(100_000)));
        let update = expire(&mut app);
        reply(&mut app, update, Ok(signatures(None)));
        let mut frame = paint(&mut app);
        let cursor = frame.cursor;
        let mut observations = Vec::new();
        let mut paints = Vec::new();
        for _ in 0..1000 {
            let now = Instant::now();
            app.observe_signature(now, false);
            observations.push(now.elapsed());
            frame.cursor = cursor;
            let now = Instant::now();
            app.paint_signature(&mut frame, 22, None);
            paints.push(now.elapsed());
        }
        observations.sort_unstable();
        paints.sort_unstable();
        eprintln!(
            "1 MB document: signature idle median {:?}, p95 {:?}; cached popup paint median {:?}, p95 {:?}",
            observations[500], observations[949], paints[500], paints[949]
        );
    }
}
