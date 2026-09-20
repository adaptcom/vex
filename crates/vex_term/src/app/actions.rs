//! A transient menu of cheap handles into the language service's action cache.

use super::{App, workspace::Context};
use crate::{
    picker::{label, paint_box},
    screen::{Frame, Style},
};
use crossterm::event::{Event, KeyEventKind};
use unicode_width::UnicodeWidthStr;
use vex_editor::{Key, Language};
use vex_lsp::{ActionEdit, CodeActions, RequestKind};

#[derive(Default)]
pub(super) struct State {
    context: Option<Context>,
    language: Option<Language>,
    menu: Option<Menu>,
    request: Option<RequestKind>,
}

struct Menu {
    actions: CodeActions,
    selected: usize,
    top: usize,
    width: u16,
    visible: usize,
}

impl State {
    pub fn clear(&mut self) {
        *self = Self::default();
    }
    pub fn waiting(&self) -> bool {
        self.request.is_some()
    }
}

impl App {
    pub(super) fn prepare_code_actions_request(&mut self) -> RequestKind {
        self.close_picker();
        self.actions.clear();
        let context = self.workspace_edit_context();
        let kind = RequestKind::CodeActions {
            selection: self.editor.selections().primary(),
            documents: context.lsp_documents(),
        };
        self.actions.context = Some(context);
        self.actions.language = self.editor.language();
        kind
    }

    pub(super) fn invalidate_code_actions(&mut self) {
        if self
            .actions
            .context
            .as_ref()
            .is_some_and(|context| !context.current(self))
            || self.actions.language != self.editor.language()
        {
            self.actions.clear();
        }
    }

    pub(super) fn take_code_action_request(&mut self) -> Option<RequestKind> {
        self.invalidate_code_actions();
        self.actions.request.take()
    }

    pub(super) fn receive_code_actions(&mut self, actions: CodeActions) {
        self.invalidate_code_actions();
        if self.actions.context.is_none() {
            return;
        }
        if actions.items.is_empty() {
            self.actions.clear();
            self.message = "no code actions available".into();
            return;
        }
        let width = actions
            .items
            .iter()
            .map(|action| action.title.width())
            .max()
            .unwrap_or(0)
            .saturating_add(4)
            .clamp(24, 96) as u16;
        self.actions.menu = Some(Menu {
            actions,
            selected: 0,
            top: 0,
            width,
            visible: 10,
        });
        self.message = "code actions · ↑/↓ or Ctrl-p/Ctrl-n · Enter applies · Esc closes".into();
    }

    pub(super) fn receive_code_action_ready(&mut self, action: ActionEdit) {
        self.invalidate_code_actions();
        let context = self.actions.context.take();
        self.actions.clear();
        let Some(context) = context else {
            return;
        };
        let result = if let Some(edit) = action.edit {
            self.begin_code_action_edit(context, edit, action.versions, action.command)
        } else if let Some(command) = action.command {
            self.execute_lsp_command(command)
        } else {
            Err(std::io::Error::other("code action has no edit or command"))
        };
        if let Err(error) = result {
            self.fail(error);
        }
    }

    pub(super) fn handle_code_action_input(&mut self, event: &Event) -> Option<bool> {
        self.invalidate_code_actions();
        let menu = self.actions.menu.as_mut()?;
        let Event::Key(event) = event else {
            return matches!(event, Event::Mouse(_)).then_some(false);
        };
        if event.kind == KeyEventKind::Release {
            return Some(false);
        }
        let (down, count) = match crate::input::key(*event) {
            Some(Key::Escape | Key::Ctrl('c')) => {
                self.cancel_language_request();
                self.clear_message();
                return Some(true);
            }
            Some(Key::Enter) => {
                let action = menu.actions.items[menu.selected].clone();
                let documents = self.actions.context.as_ref().unwrap().lsp_documents();
                self.actions.menu = None;
                self.actions.request = Some(RequestKind::ApplyCodeAction { action, documents });
                return Some(true);
            }
            Some(Key::Up | Key::Ctrl('p') | Key::BackTab) => (false, 1),
            Some(Key::Down | Key::Ctrl('n') | Key::Tab) => (true, 1),
            Some(Key::PageUp | Key::Ctrl('u')) => (false, (menu.visible / 2).max(1)),
            Some(Key::PageDown | Key::Ctrl('d')) => (true, (menu.visible / 2).max(1)),
            _ => {
                self.actions.clear();
                return None;
            }
        };
        let len = menu.actions.items.len();
        let count = count % len;
        menu.selected = if down {
            (menu.selected + count) % len
        } else {
            (menu.selected + len - count) % len
        };
        Some(true)
    }

    pub(super) fn paint_code_actions(&mut self, frame: &mut Frame, body: u16) {
        self.invalidate_code_actions();
        let Some(menu) = &mut self.actions.menu else {
            return;
        };
        let Some(cursor) = frame.cursor else {
            return;
        };
        let width = menu.width.min(frame.width());
        if width < 8 {
            return;
        }
        let desired = menu.actions.items.len().min(10) as u16 + 2;
        let below = body.saturating_sub(cursor.y.saturating_add(1));
        let above = cursor.y;
        let under = below >= desired || below >= above;
        let height = desired.min(if under { below } else { above });
        if height < 3 {
            return;
        }
        let x = cursor.x.saturating_sub(1).min(frame.width() - width);
        let y = if under {
            cursor.y + 1
        } else {
            cursor.y - height
        };
        menu.visible = usize::from(height - 2);
        menu.top = menu
            .top
            .min(menu.selected)
            .max(menu.selected.saturating_sub(menu.visible - 1));
        menu.top = menu
            .top
            .min(menu.actions.items.len().saturating_sub(menu.visible));
        paint_box(
            frame,
            x,
            y,
            x + width,
            y + height,
            if menu.actions.limited {
                " Code actions · limited "
            } else {
                " Code actions "
            },
        );
        for (index, action) in menu
            .actions
            .items
            .iter()
            .enumerate()
            .skip(menu.top)
            .take(menu.visible)
        {
            let row = y + 1 + (index - menu.top) as u16;
            let style = if index == menu.selected {
                Style::Selection
            } else {
                Style::Text
            };
            for column in x + 1..x + width - 1 {
                frame.put(column, row, " ", style);
            }
            label(frame, x + 2, row, width - 4, &action.title, style);
        }
        frame.cursor = None;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::workspace::server_tests::{fixture, flush, receive};
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::{
        fs,
        sync::mpsc,
        time::{Duration, Instant},
    };
    use vex_lsp::{Answer, Event as LanguageEvent, Service};

    fn key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
        app.handle(Event::Key(KeyEvent::new(code, modifiers)));
    }
    fn open(app: &mut App, service: &Service, receiver: &mpsc::Receiver<LanguageEvent>) {
        for ch in [' ', 'a'] {
            key(app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        flush(app, service);
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.input_waiting() {
            assert!(Instant::now() < deadline);
            app.handle_lsp_event(receive(receiver));
            flush(app, service);
        }
        assert_eq!(
            app.actions.menu.as_ref().unwrap().actions.items[0]
                .title
                .as_ref(),
            "Resolve and apply"
        );
    }
    fn complete(
        app: &mut App,
        service: &Service,
        receiver: &mpsc::Receiver<LanguageEvent>,
    ) -> usize {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut edits = 0;
        flush(app, service);
        while app.input_waiting() {
            assert!(Instant::now() < deadline);
            if let Some(job) = app.take_workspace_edit() {
                let result = job.run().unwrap();
                assert!(app.handle_workspace_edit(result));
                edits += 1;
                // A following command is queued before deferred input resumes.
                if !app.error {
                    assert!(app.input_waiting());
                }
            } else {
                app.handle_lsp_event(receive(receiver));
            }
            flush(app, service);
        }
        edits
    }

    #[test]
    fn code_action_menu_resolves_and_applies_before_command_to_hidden_unsaved_buffers() {
        let (directory, mut app, service, receiver, paths) = fixture();
        open(&mut app, &service, &receiver);
        let mut frame = Frame::default();
        frame.reset(100, 24).unwrap();
        app.paint(&mut frame).unwrap();
        assert!(frame.row_text(0).contains("foo"));
        assert!((0..24).any(|row| frame.row_text(row).contains("┌─ Code actions")));
        assert_eq!(app.actions.menu.as_ref().unwrap().selected, 0);
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.input_waiting());
        assert_eq!(complete(&mut app, &service, &receiver), 2);
        assert!(!app.error, "{}", app.message);
        assert_eq!(app.editor.document().text(), "baz\n");
        assert_eq!(
            app.snapshot_for_path(&paths[1]).unwrap().text(),
            "// unsaved\nbaz\n"
        );
        // The command reply can precede the peer processing our apply ack.
        // A subsequent round trip observes both its acknowledgement and text.
        app.editor.execute("hover", 1).unwrap();
        let update = app.take_lsp_update().unwrap();
        let id = update.request.as_ref().unwrap().id;
        service.update(update);
        loop {
            let event = receive(&receiver);
            if let LanguageEvent::Answer {
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
        let log = fs::read_to_string(directory.path().join("events.log")).unwrap();
        assert!(log.contains("RESOLVE\n"));
        assert!(log.find("CHANGE").unwrap() < log.find("AFTER_LITERAL").unwrap());
        assert!(log.contains("APPLIED baz\n"));
        for path in &paths {
            assert_eq!(fs::read_to_string(path).unwrap(), "foo\n");
        }
        app.editor.execute("undo", 1).unwrap();
        assert_eq!(app.editor.document().text(), "bar\n");
        app.editor.execute("undo", 1).unwrap();
        assert_eq!(app.editor.document().text(), "foo\n");
        app.open_window_file(&paths[1]).unwrap();
        app.editor.execute("undo", 1).unwrap();
        assert_eq!(app.editor.document().text(), "// unsaved\nbar\n");
        app.editor.execute("undo", 1).unwrap();
        assert_eq!(app.editor.document().text(), "// unsaved\nfoo\n");
        drop(service);
    }

    #[test]
    fn code_action_literal_edit_skips_resolution_and_bad_edit_never_executes_command() {
        for bad in [false, true] {
            let (directory, mut app, service, receiver, paths) = fixture();
            open(&mut app, &service, &receiver);
            key(&mut app, KeyCode::Down, KeyModifiers::NONE);
            if bad {
                key(&mut app, KeyCode::Down, KeyModifiers::NONE);
            }
            let expected = if bad { "Bad version" } else { "Literal edit" };
            let menu = app.actions.menu.as_ref().unwrap();
            assert_eq!(menu.actions.items[menu.selected].title.as_ref(), expected);
            key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
            assert_eq!(
                complete(&mut app, &service, &receiver),
                if bad { 1 } else { 2 }
            );
            let log = fs::read_to_string(directory.path().join("events.log")).unwrap();
            assert!(!log.contains("RESOLVE"));
            if bad {
                assert!(
                    app.error && app.message.contains("version"),
                    "{}",
                    app.message
                );
                assert_eq!(app.editor.document().text(), "foo\n");
                assert_eq!(
                    app.snapshot_for_path(&paths[1]).unwrap().text(),
                    "// unsaved\nfoo\n"
                );
                assert!(!log.contains("AFTER_LITERAL"));
            } else {
                assert!(!app.error, "{}", app.message);
                assert_eq!(app.editor.document().text(), "baz\n");
                assert!(log.contains("AFTER_LITERAL"));
            }
            drop(service);
        }
    }

    #[test]
    fn code_action_menu_helix_keys_wrap_scroll_cancel_and_pass_other_input() {
        let (_directory, mut app, service, receiver, _) = fixture();
        open(&mut app, &service, &receiver);
        let count = app.actions.menu.as_ref().unwrap().actions.items.len();
        for (code, modifiers, expected) in [
            (KeyCode::Up, KeyModifiers::NONE, count - 1),
            (KeyCode::Tab, KeyModifiers::NONE, 0),
            (KeyCode::Char('n'), KeyModifiers::CONTROL, 1),
            (KeyCode::BackTab, KeyModifiers::SHIFT, 0),
            (KeyCode::Char('d'), KeyModifiers::CONTROL, 5),
            (KeyCode::PageUp, KeyModifiers::NONE, 0),
        ] {
            key(&mut app, code, modifiers);
            assert_eq!(app.actions.menu.as_ref().unwrap().selected, expected);
        }
        key(&mut app, KeyCode::Up, KeyModifiers::NONE);
        let mut frame = Frame::default();
        for width in [8, 20, 100] {
            app.handle(Event::Resize(width, 8));
            frame.reset(width, 8).unwrap();
            app.paint(&mut frame).unwrap();
            assert!(app.actions.menu.as_ref().unwrap().top > 0);
        }
        key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.actions.context.is_none());
        open(&mut app, &service, &receiver);
        key(&mut app, KeyCode::Char('l'), KeyModifiers::NONE);
        assert!(app.actions.menu.is_none());
        assert_eq!(app.editor.selections().primary().start().0, 1);
        open(&mut app, &service, &receiver);
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.input_waiting());
        key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!app.input_waiting());
        assert!(app.actions.context.is_none());
        assert!(
            app.take_lsp_update()
                .is_none_or(|update| update.request.is_none())
        );
        assert!(!app.is_dirty());
    }

    #[test]
    fn cancelled_or_stale_code_action_replies_cannot_reopen_the_menu() {
        let (_directory, mut app, service, receiver, _) = fixture();
        for cancel in [true, false] {
            app.editor.execute("code_action", 1).unwrap();
            let update = app.take_lsp_update().unwrap();
            let id = update.request.as_ref().unwrap().id;
            service.update(update);
            // Hold a real service reply until its origin has been cancelled or moved.
            let answer = loop {
                let event = receive(&receiver);
                if matches!(&event, LanguageEvent::Answer { id: received, result: Ok(Answer::CodeActions(_)), .. } if *received == id)
                {
                    break event;
                }
                app.handle_lsp_event(event);
            };
            if cancel {
                key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
            } else {
                app.editor.execute("move_right", 1).unwrap();
            }
            assert!(!app.handle_lsp_event(answer));
            app.take_lsp_update();
            assert!(app.actions.menu.is_none());
            assert!(!app.input_waiting());
            assert!(!app.is_dirty());
        }
    }

    #[test]
    fn bare_commands_and_cancellation_during_resolution_preserve_input_order() {
        let (_directory, mut app, service, receiver, _) = fixture();
        open(&mut app, &service, &receiver);
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let update = app.take_lsp_update().unwrap();
        let id = update.request.as_ref().unwrap().id;
        service.update(update);
        let answer = loop {
            let event = receive(&receiver);
            if matches!(&event, LanguageEvent::Answer { id: received, result: Ok(Answer::CodeActionReady(_)), .. } if *received == id)
            {
                break event;
            }
            app.handle_lsp_event(event);
        };
        key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.handle_lsp_event(answer));
        assert!(app.take_workspace_edit().is_none());
        assert_eq!(app.editor.document().text(), "foo\n");
        open(&mut app, &service, &receiver);
        for _ in 0..3 {
            key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        }
        let menu = app.actions.menu.as_ref().unwrap();
        assert_eq!(
            menu.actions.items[menu.selected].title.as_ref(),
            "Command only"
        );
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(complete(&mut app, &service, &receiver), 1);
        assert!(!app.error, "{}", app.message);
        assert_eq!(app.editor.document().text(), "bar\n");
    }

    #[test]
    #[ignore = "manual release-mode code action submission and menu benchmark"]
    fn benchmark_code_action_submission_and_menu() {
        use std::hint::black_box;
        use vex_core::{CharOffset, Selection, SelectionSet};
        for mib in [1usize, 8] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("buffer.txt");
            fs::write(&path, "foo \n".repeat((mib << 20) / 5)).unwrap();
            for count in [1usize, 1000] {
                let mut app = App::open(Some(&path), (100, 24)).unwrap();
                app.editor.set_background_syntax(true);
                app.editor.set_language(Some(Language::Rust));
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
                    app.editor.execute("code_action", 1).unwrap();
                    let update = black_box(app.take_lsp_update().unwrap());
                    samples.push(start.elapsed());
                    app.cancel_language_request();
                    drop(update);
                }
                samples.sort_unstable();
                eprintln!(
                    "code actions submit {mib}MiB, {count} selections/view, 3 views: median {:?}, p95 {:?}",
                    samples[250], samples[474]
                );
            }
        }
        let (_directory, mut app, service, receiver, _) = fixture();
        open(&mut app, &service, &receiver);
        let mut frame = Frame::default();
        frame.reset(100, 24).unwrap();
        // Exercise the maximum label count/length without retaining raw JSON.
        let first = app.actions.menu.as_ref().unwrap().actions.items[0].clone();
        let mut actions = Vec::new();
        for _ in 0..256 {
            let mut action = first.clone();
            action.title = "界".repeat(256).into();
            actions.push(action);
        }
        let mut opening = Vec::with_capacity(500);
        let mut redraw = Vec::with_capacity(500);
        let mut closing = Vec::with_capacity(500);
        for _ in 0..500 {
            let items = actions.clone();
            app.prepare_code_actions_request();
            let start = Instant::now();
            app.receive_code_actions(CodeActions {
                items,
                limited: false,
            });
            opening.push(start.elapsed());
            frame.cursor = Some(crate::screen::Cursor {
                x: 4,
                y: 0,
                shape: crate::screen::CursorShape::Block,
            });
            let start = Instant::now();
            app.paint_code_actions(black_box(&mut frame), 22);
            redraw.push(start.elapsed());
            let start = Instant::now();
            app.actions.clear();
            closing.push(start.elapsed());
        }
        for (name, samples) in [
            ("open", &mut opening),
            ("redraw", &mut redraw),
            ("close", &mut closing),
        ] {
            samples.sort_unstable();
            eprintln!(
                "code actions {name} 256 labels of 256 wide characters, 10 visible rows: median {:?}, p95 {:?}",
                samples[250], samples[474]
            );
        }
    }
}
