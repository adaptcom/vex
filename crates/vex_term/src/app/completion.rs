//! Insert-mode completion sessions and cursor-anchored presentation.

use super::App;
use crate::{
    input,
    picker::{label, paint_box},
    screen::{Frame, Style},
};
use crossterm::event::{Event, KeyEventKind};
use std::{
    io,
    time::{Duration, Instant},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use vex_core::{DocumentId, Revision, SelectionSet, display};
use vex_editor::{Key, Mode};
use vex_lsp::{CompletionItem, CompletionTrigger, Completions, RequestKind};

struct Session {
    automatic: bool,
    incomplete: bool,
    document: DocumentId,
    revision: Revision,
    selections: SelectionSet,
    items: Vec<CompletionItem>,
    label_width: usize,
    loaded: bool,
    selected: Option<usize>,
    top: usize,
    resolving: Option<usize>,
    accepting: bool,
    following: Option<Event>,
    limited: bool,
}

#[derive(Clone, Copy)]
struct Config {
    enabled: bool,
    delay: Duration,
    min_length: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            delay: Duration::from_millis(100),
            min_length: 2,
        }
    }
}

pub(super) struct Request {
    pub kind: RequestKind,
    pub automatic: bool,
}

struct Scheduled {
    document: DocumentId,
    revision: Revision,
    selections: SelectionSet,
    due: Instant,
    trigger: CompletionTrigger,
    automatic: bool,
}

#[derive(Clone, Copy)]
struct Refresh {
    automatic: bool,
    incomplete: bool,
}

pub(super) struct Typing {
    key: Key,
    revision: Revision,
    refresh: Option<Refresh>,
}

#[derive(Default)]
pub(super) struct State {
    config: Config,
    active: Option<Session>,
    outgoing: Option<Request>,
    scheduled: Option<Scheduled>,
}

impl State {
    pub(super) fn clear(&mut self) {
        self.active = None;
        self.outgoing = None;
        self.scheduled = None;
    }
}

impl App {
    pub(super) fn begin_completion(&mut self, automatic: bool) {
        self.completion.clear();
        self.completion.active = Some(Session {
            automatic,
            incomplete: false,
            document: self.editor.document().id(),
            revision: self.editor.document().revision(),
            selections: self.editor.selections().clone(),
            items: Vec::new(),
            label_width: 20,
            loaded: false,
            selected: None,
            top: 0,
            resolving: None,
            accepting: false,
            following: None,
            limited: false,
        });
    }

    pub(super) fn take_completion_request(&mut self) -> Option<Request> {
        self.completion.outgoing.take()
    }

    pub(super) fn invalidate_completion(&mut self) {
        if self.completion.active.as_ref().is_some_and(|session| {
            session.document != self.editor.document().id()
                || session.revision != self.editor.document().revision()
                || session.selections != *self.editor.selections()
                || self.editor.mode() != Mode::Insert
        }) || self.completion.scheduled.as_ref().is_some_and(|scheduled| {
            scheduled.document != self.editor.document().id()
                || scheduled.revision != self.editor.document().revision()
                || scheduled.selections != *self.editor.selections()
                || self.editor.mode() != Mode::Insert
                || self.prompt.is_some()
                || scheduled.automatic && self.completion_options().is_none()
        }) {
            self.dismiss_language_help();
        }
    }

    pub(super) fn completion_waiting(&self) -> bool {
        self.completion
            .active
            .as_ref()
            .is_some_and(|session| session.accepting)
    }

    /// Capture intent before dispatch cancels obsolete requests. Only actual
    /// keyboard edits schedule completion; paste and navigation never open it.
    pub(super) fn completion_typing(&self, event: &Event) -> Option<Typing> {
        if self.prompt.is_some()
            || self.editor.mode() != Mode::Insert
            || self.editor.selections().ranges().len() != 1
        {
            return None;
        }
        let Event::Key(event) = event else {
            return None;
        };
        if event.kind == KeyEventKind::Release {
            return None;
        }
        let key = input::key(*event)?;
        if !matches!(key, Key::Char(_) | Key::Backspace | Key::Ctrl('h')) {
            return None;
        }
        let refresh = self
            .completion
            .active
            .as_ref()
            .filter(|session| session.selected.is_none())
            .map(|session| Refresh {
                automatic: session.automatic,
                incomplete: session.incomplete,
            })
            .or_else(|| {
                self.completion.scheduled.as_ref().map(|scheduled| Refresh {
                    automatic: scheduled.automatic,
                    incomplete: scheduled.trigger == CompletionTrigger::Incomplete,
                })
            });
        Some(Typing {
            key,
            revision: self.editor.document().revision(),
            refresh,
        })
    }

    pub(super) fn schedule_completion(&mut self, typing: Typing, now: Instant) {
        if self.editor.mode() != Mode::Insert
            || self.prompt.is_some()
            || self.error
            || self.editor.selections().ranges().len() != 1
            || typing.revision == self.editor.document().revision()
        {
            return;
        }
        let automatic = typing.refresh.is_none_or(|refresh| refresh.automatic);
        if automatic && (!self.completion.config.enabled || self.completion_options().is_none()) {
            return;
        }
        let is_trigger = |ch| {
            self.completion_options()
                .is_some_and(|options| options.trigger_characters.contains(&ch))
        };
        let trigger = match typing.key {
            Key::Char(ch) if is_trigger(ch) => CompletionTrigger::Character(ch),
            key if matches!(key, Key::Char(ch) if identifier(ch))
                || matches!(key, Key::Backspace | Key::Ctrl('h')) && typing.refresh.is_some() =>
            {
                let head = self.editor.selections().primary().head;
                let text = self.editor.document().text();
                let mut chars = text.chars_at(head.0);
                let enough = (0..self.completion.config.min_length)
                    .all(|_| chars.prev().is_some_and(identifier));
                let after_trigger = head.0 > 0 && is_trigger(text.char(head.0 - 1));
                // A trigger-character menu keeps refreshing from the first
                // identifier character; the threshold only governs opening it.
                let continuing = typing.refresh.is_some()
                    && matches!(typing.key, Key::Char(ch) if identifier(ch));
                if automatic && !enough && !after_trigger && !continuing {
                    return;
                }
                if typing.refresh.is_some_and(|refresh| refresh.incomplete) {
                    CompletionTrigger::Incomplete
                } else {
                    CompletionTrigger::Invoked
                }
            }
            _ => return,
        };
        let delay = if automatic && !matches!(trigger, CompletionTrigger::Character(_)) {
            self.completion.config.delay
        } else {
            Duration::ZERO
        };
        self.completion.scheduled = Some(Scheduled {
            document: self.editor.document().id(),
            revision: self.editor.document().revision(),
            selections: self.editor.selections().clone(),
            due: now + delay,
            trigger,
            automatic,
        });
    }

    /// The event loop uses the earliest completion deadline as its wait timeout.
    pub(crate) fn completion_deadline(&mut self) -> Option<Instant> {
        self.invalidate_completion();
        self.completion
            .scheduled
            .as_ref()
            .map(|scheduled| scheduled.due)
    }

    pub(super) fn poll_completion(&mut self, now: Instant) {
        self.invalidate_completion();
        if self
            .completion
            .scheduled
            .as_ref()
            .is_some_and(|scheduled| scheduled.due <= now)
        {
            let scheduled = self.completion.scheduled.take().unwrap();
            self.completion.outgoing = Some(Request {
                kind: RequestKind::Completion(scheduled.trigger),
                automatic: scheduled.automatic,
            });
        }
    }

    pub(super) fn configure_completion(&mut self, argument: &str) -> io::Result<()> {
        let mut config = self.completion.config;
        let words: Vec<_> = argument.split_whitespace().collect();
        let invalid =
            || io::Error::other("use :auto-completion [on|off|delay 0..10000|min-length 1..256]");
        match words.as_slice() {
            [] => {}
            ["on"] => config.enabled = true,
            ["off"] => config.enabled = false,
            ["delay", value] => {
                let value: u64 = value.parse().map_err(|_| invalid())?;
                if value > 10_000 {
                    return Err(invalid());
                }
                config.delay = Duration::from_millis(value);
            }
            ["min-length", value] => {
                let value: usize = value.parse().map_err(|_| invalid())?;
                if !(1..=256).contains(&value) {
                    return Err(invalid());
                }
                config.min_length = value;
            }
            _ => return Err(invalid()),
        }
        if !words.is_empty() {
            self.dismiss_language_help();
            self.completion.config = config;
        }
        self.message = format!(
            "automatic completion: {}, {} characters, {} ms",
            if config.enabled { "on" } else { "off" },
            config.min_length,
            config.delay.as_millis()
        );
        Ok(())
    }

    /// Completion keys take precedence over insertion only while the menu is active.
    /// Ctrl-c rejects; other keys commit an explicitly selected item, then dispatch.
    pub(super) fn handle_completion_input(&mut self, event: &Event) -> Option<bool> {
        self.invalidate_completion();
        let session = self.completion.active.as_ref()?;
        // An automatic request has no visible menu until results arrive. Tab,
        // arrows, and Return must keep their normal behavior during that time.
        if session.automatic && !session.loaded {
            return None;
        }
        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Release {
                return Some(false);
            }
            match input::key(*key) {
                Some(Key::Ctrl('c')) => {
                    let following = self
                        .completion
                        .active
                        .as_mut()
                        .and_then(|session| session.following.take());
                    self.dismiss_language_help();
                    if let Some(event) = following {
                        self.handle(event);
                    }
                    self.clear_message();
                    return Some(true);
                }
                Some(Key::Escape) if session.accepting => {
                    let following = self
                        .completion
                        .active
                        .as_mut()
                        .and_then(|session| session.following.take());
                    self.dismiss_language_help();
                    if let Some(event) = following {
                        self.handle(event);
                    }
                    self.handle(event.clone());
                    return Some(true);
                }
                Some(Key::Tab | Key::Down | Key::Ctrl('n')) => {
                    self.move_completion(false);
                    return Some(true);
                }
                Some(Key::BackTab | Key::Up | Key::Ctrl('p')) => {
                    self.move_completion(true);
                    return Some(true);
                }
                Some(Key::Ctrl('x')) => return None,
                Some(Key::Enter) if session.selected.is_some() => {
                    self.accept_completion(None);
                    return Some(true);
                }
                _ => {}
            }
        }
        if session.selected.is_some() && matches!(event, Event::Key(_) | Event::Paste(_)) {
            self.accept_completion(Some(event.clone()));
            return Some(true);
        }
        None
    }

    /// Select the next or previous suggestion without editing the document.
    fn move_completion(&mut self, previous: bool) {
        let session = self.completion.active.as_mut().unwrap();
        let len = session.items.len();
        session.selected = Some(if len == 0 {
            if previous { usize::MAX } else { 0 }
        } else {
            match session.selected {
                None => {
                    if previous {
                        len - 1
                    } else {
                        0
                    }
                }
                Some(index) => {
                    if previous {
                        (index + len - 1) % len
                    } else {
                        (index + 1) % len
                    }
                }
            }
        });
        self.resolve_completion();
    }

    fn resolve_completion(&mut self) {
        let Some(session) = &mut self.completion.active else {
            return;
        };
        let Some(index) = session
            .selected
            .filter(|index| *index < session.items.len())
        else {
            return;
        };
        if session.resolving == Some(index) {
            return;
        }
        let item = &session.items[index];
        let request =
            (!item.resolved).then(|| RequestKind::ResolveCompletion(Box::new(item.clone())));
        let automatic = session.automatic;
        session.resolving = request.as_ref().map(|_| index);
        self.cancel_language_request();
        self.completion.outgoing = request.map(|kind| Request { kind, automatic });
    }

    pub(super) fn receive_completions(&mut self, result: Completions) {
        let Some(session) = &mut self.completion.active else {
            return;
        };
        session.items = result.items;
        session.label_width = session
            .items
            .iter()
            .map(|item| {
                item.label
                    .graphemes(true)
                    .map(|g| display::visible(g).width())
                    .sum::<usize>()
            })
            .max()
            .unwrap_or(20);
        session.loaded = true;
        session.incomplete = result.incomplete;
        session.limited = result.limited;
        if session.items.is_empty() {
            self.fail_completion("no completion suggestions".into());
            return;
        }
        session.selected = session
            .selected
            .map(|index| index.min(session.items.len() - 1));
        let accepting = session.accepting;
        self.clear_message();
        self.resolve_completion();
        if accepting {
            self.finish_completion();
        }
    }

    pub(super) fn receive_resolved_completion(&mut self, item: CompletionItem) {
        let Some(session) = &mut self.completion.active else {
            return;
        };
        let Some(index) = session.resolving.take() else {
            return;
        };
        session.items[index] = item;
        if session.accepting {
            self.finish_completion();
        }
    }

    /// Accept the selected replacement and imports together. Following input is
    /// retained until resolution finishes, including the key that accepts implicitly.
    fn accept_completion(&mut self, following: Option<Event>) {
        let session = self.completion.active.as_mut().unwrap();
        session.accepting = true;
        session.following = following;
        self.resolve_completion();
        self.finish_completion();
    }

    fn finish_completion(&mut self) {
        let Some(session) = &self.completion.active else {
            return;
        };
        let Some(item) = session.selected.and_then(|index| session.items.get(index)) else {
            return;
        };
        if !item.resolved {
            return;
        }
        let edit = item.edit.clone();
        let additional = item.additional_edits.clone();
        let following = self.completion.active.as_mut().unwrap().following.take();
        self.dismiss_language_help();
        let result = self.editor.apply_completion(edit, additional);
        self.clear_message();
        if let Some(event) = following {
            self.handle(event);
        }
        if let Err(error) = result {
            self.fail(error);
        }
    }

    pub(super) fn fail_completion(&mut self, error: String) {
        let quiet = self
            .completion
            .active
            .as_ref()
            .is_some_and(|session| session.automatic && !session.accepting);
        let following = self
            .completion
            .active
            .as_mut()
            .and_then(|session| session.following.take());
        self.dismiss_language_help();
        if let Some(event) = following {
            self.handle(event);
        }
        if !quiet {
            self.fail(error);
        }
    }

    pub(super) fn paint_completion(&mut self, frame: &mut Frame, body: u16) {
        self.invalidate_completion();
        let Some(session) = &mut self.completion.active else {
            return;
        };
        if session.automatic && !session.loaded {
            return;
        }
        let Some(cursor) = frame.cursor else { return };
        let below = body.saturating_sub(cursor.y + 1);
        let above = cursor.y;
        let desired = (session.items.len().clamp(1, 10) + 2) as u16;
        let under = below >= desired || below >= above;
        let height = desired.min(if under { below } else { above });
        let width = session
            .label_width
            .saturating_add(4)
            .clamp(24, 48)
            .min(usize::from(frame.width())) as u16;
        if height < 3 || width < 8 {
            return;
        }
        let x = cursor.x.saturating_sub(1).min(frame.width() - width);
        let y = if under {
            cursor.y + 1
        } else {
            cursor.y - height
        };
        paint_box(
            frame,
            x,
            y,
            x + width,
            y + height,
            if session.limited {
                " Complete · limited "
            } else {
                " Complete "
            },
        );
        let rows = usize::from(height - 2);
        if let Some(index) = session
            .selected
            .filter(|index| *index < session.items.len())
        {
            session.top = session.top.min(index);
            if index >= session.top + rows {
                session.top = index + 1 - rows;
            }
        }
        if !session.loaded {
            label(frame, x + 2, y + 1, width - 3, "Loading…", Style::Gutter);
        }
        for (index, item) in session
            .items
            .iter()
            .enumerate()
            .skip(session.top)
            .take(rows)
        {
            let row = y + 1 + (index - session.top) as u16;
            let style = if session.selected == Some(index) {
                Style::Selection
            } else {
                Style::Text
            };
            for col in x + 1..x + width - 1 {
                frame.put(col, row, " ", style);
            }
            label(frame, x + 2, row, width - 3, &item.label, style);
        }
        let Some(item) = session.selected.and_then(|index| session.items.get(index)) else {
            return;
        };
        let right = frame.width().saturating_sub(x + width + 1);
        let (docs_x, docs_width) = if right >= 24 {
            (x + width + 1, right.min(60))
        } else if x >= 25 {
            let width = (x - 1).min(60);
            (x - width - 1, width)
        } else {
            return;
        };
        let text = format!(
            "{}{}{}",
            item.detail,
            if item.detail.is_empty() { "" } else { "\n\n" },
            if !item.resolved && item.documentation.is_empty() {
                "Loading documentation…"
            } else {
                &item.documentation
            }
        );
        if text.is_empty() {
            return;
        }
        let lines = wrap(
            &text,
            usize::from(docs_width - 4),
            usize::from(body.saturating_sub(2).min(14)),
        );
        let docs_height = (lines.len() + 2) as u16;
        let docs_y = y.min(body.saturating_sub(docs_height));
        paint_box(
            frame,
            docs_x,
            docs_y,
            docs_x + docs_width,
            docs_y + docs_height,
            " Documentation ",
        );
        for (row, line) in lines.iter().enumerate() {
            label(
                frame,
                docs_x + 2,
                docs_y + 1 + row as u16,
                docs_width - 4,
                line,
                Style::Text,
            );
        }
    }
}

fn identifier(ch: char) -> bool {
    ch == '_' || ch.is_alphanumeric()
}

fn wrap(text: &str, width: usize, rows: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for line in text.lines() {
        let mut out = String::new();
        let mut column = 0;
        for grapheme in line.graphemes(true) {
            let size = display::visible(grapheme).width();
            if column + size > width {
                lines.push(std::mem::take(&mut out));
                if lines.len() >= rows {
                    return lines;
                }
                column = 0;
            }
            out.push_str(grapheme);
            column += size;
        }
        lines.push(out);
        if lines.len() >= rows {
            break;
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use vex_core::{CharOffset, Edit, Selection};
    use vex_lsp::{Answer, Event as LspEvent, Update};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }
    fn ctrl(ch: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL))
    }
    fn fixture() -> (tempfile::TempDir, App) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.rs");
        std::fs::write(&path, "// 界\r\nans").unwrap();
        let mut app = App::open(Some(&path), (120, 20)).unwrap();
        app.enable_lsp();
        app.take_lsp_update();
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor
            .set_selections(SelectionSet::single(Selection::cursor(CharOffset(9))))
            .unwrap();
        (directory, app)
    }
    fn candidate(name: &str, resolved: bool) -> CompletionItem {
        let mut item = CompletionItem::plain(name, Edit::new(CharOffset(6)..CharOffset(9), name));
        item.resolved = resolved;
        item.detail = format!("fn {name}() -> u32");
        item.documentation = "Documented completion with 界 and e\u{301}.".into();
        item
    }
    fn list(resolved: bool) -> Answer {
        Answer::Completion(Completions {
            items: vec![
                candidate("answer", resolved),
                candidate("another", resolved),
            ],
            incomplete: false,
            limited: false,
        })
    }
    fn answer(app: &mut App, request: Update, result: Answer) -> bool {
        let document = request.document.unwrap();
        app.handle_lsp_event(LspEvent::Answer {
            epoch: document.epoch,
            revision: document.snapshot.revision(),
            id: request.request.unwrap().id,
            result: Ok(result),
        })
    }
    fn start(app: &mut App) -> Update {
        app.handle(ctrl('x'));
        let request = app.take_lsp_update().unwrap();
        assert!(matches!(
            request.request.as_ref().unwrap().kind,
            RequestKind::Completion(_)
        ));
        request
    }

    fn automatic_fixture(source: &str) -> (tempfile::TempDir, App) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.rs");
        std::fs::write(&path, source).unwrap();
        let mut app = App::open(Some(&path), (120, 20)).unwrap();
        app.enable_lsp();
        let epoch = app.take_lsp_update().unwrap().document.unwrap().epoch;
        app.handle_lsp_event(LspEvent::Capabilities {
            epoch,
            completion: Some(vex_lsp::CompletionOptions {
                trigger_characters: vec!['.', ':'],
            }),
        });
        app.handle_lsp_event(LspEvent::Status {
            epoch,
            message: "rust-analyzer ready".into(),
            failed: false,
        });
        app.editor.execute("goto_file_end", 1).unwrap();
        app.editor.execute("insert_mode", 1).unwrap();
        (directory, app)
    }

    fn automatic_answer(app: &App, incomplete: bool) -> Answer {
        Answer::Completion(Completions {
            items: vec![CompletionItem::plain(
                "answer",
                Edit::new(
                    CharOffset(0)..app.editor.selections().primary().head,
                    "answer",
                ),
            )],
            incomplete,
            limited: false,
        })
    }

    fn expire(app: &mut App) -> Update {
        let deadline = app.completion_deadline().expect("scheduled completion");
        app.poll_completion(deadline);
        app.take_lsp_update().expect("completion update")
    }

    #[test]
    fn automatic_completion_waits_for_threshold_and_deadline_without_selecting_an_item() {
        let (_directory, mut app) = automatic_fixture("");
        let now = Instant::now() + Duration::from_secs(10);
        app.handle_at(key(KeyCode::Char('a')), now);
        assert!(app.completion_deadline().is_none());
        assert!(app.take_lsp_update().unwrap().request.is_none());
        app.handle_at(key(KeyCode::Char('n')), now);
        assert_eq!(
            app.completion_deadline(),
            Some(now + Duration::from_millis(100))
        );
        app.poll_completion(now + Duration::from_millis(99));
        assert!(app.take_lsp_update().unwrap().request.is_none());
        let request = expire(&mut app);
        assert!(matches!(
            request.request.as_ref().unwrap().kind,
            RequestKind::Completion(CompletionTrigger::Invoked)
        ));
        assert!(app.message.is_empty());
        assert!(!app.error);
        let mut frame = Frame::default();
        frame.reset(120, 20).unwrap();
        app.paint(&mut frame).unwrap();
        assert!((0..20).all(|row| !frame.row_text(row).contains("Complete")));
        let result = automatic_answer(&app, false);
        assert!(answer(&mut app, request, result));
        app.paint(&mut frame).unwrap();
        assert!((0..20).any(|row| frame.row_text(row).contains("Complete")));
        assert_eq!(app.completion.active.as_ref().unwrap().selected, None);
        app.handle(key(KeyCode::Enter));
        assert_eq!(app.editor.document().text(), "an\n");
        assert!(app.completion.active.is_none());
        assert!(app.completion_deadline().is_none());
    }

    #[test]
    fn automatic_completion_debounces_and_rejects_replies_after_further_typing() {
        let (_directory, mut app) = automatic_fixture("a");
        let now = Instant::now() + Duration::from_secs(10);
        app.handle_at(key(KeyCode::Char('n')), now);
        let old_due = app.completion_deadline().unwrap();
        app.handle_at(key(KeyCode::Char('s')), now + Duration::from_millis(50));
        app.poll_completion(old_due);
        assert!(app.take_lsp_update().unwrap().request.is_none());
        assert_eq!(
            app.completion_deadline(),
            Some(old_due + Duration::from_millis(50))
        );
        let old = expire(&mut app);
        let cancellation = old.request.as_ref().unwrap().cancellation.clone();
        let result = automatic_answer(&app, false);
        app.handle_at(key(KeyCode::Char('w')), now + Duration::from_millis(200));
        assert!(cancellation.is_cancelled());
        assert!(!answer(&mut app, old, result));
        let newer = expire(&mut app);
        assert_eq!(newer.document.unwrap().snapshot.text(), "answ");
        assert!(newer.request.is_some());
    }

    #[test]
    fn navigation_paste_focus_loss_and_mode_changes_cancel_delayed_and_hidden_completions() {
        for event in [
            key(KeyCode::Left),
            key(KeyCode::Down),
            key(KeyCode::Tab),
            key(KeyCode::Enter),
            key(KeyCode::Esc),
            ctrl('c'),
            Event::Paste("text".into()),
            Event::FocusLost,
        ] {
            for in_flight in [false, true] {
                let (_directory, mut app) = automatic_fixture("a");
                let now = Instant::now() + Duration::from_secs(10);
                app.handle_at(key(KeyCode::Char('n')), now);
                let request = in_flight.then(|| expire(&mut app));
                let result = automatic_answer(&app, false);
                app.handle_at(event.clone(), now + Duration::from_secs(1));
                assert!(app.completion_deadline().is_none(), "{event:?}");
                assert!(app.completion.active.is_none(), "{event:?}");
                assert!(
                    app.take_lsp_update()
                        .is_none_or(|update| update.request.is_none())
                );
                if let Some(request) = request {
                    assert!(!answer(&mut app, request, result));
                }
                if event == key(KeyCode::Tab) {
                    assert_eq!(app.editor.document().text(), "an\t");
                }
                if event == key(KeyCode::Enter) {
                    assert_eq!(app.editor.document().text(), "an\n");
                }
            }
        }
    }

    #[test]
    fn automatic_trigger_characters_are_immediate_and_incomplete_lists_retrigger() {
        let (_directory, mut app) = automatic_fixture("value");
        app.execute("auto-completion min-length 50").unwrap();
        let now = Instant::now() + Duration::from_secs(10);
        app.handle_at(key(KeyCode::Char('.')), now);
        assert_eq!(app.completion_deadline(), Some(now));
        let request = expire(&mut app);
        assert!(matches!(
            request.request.as_ref().unwrap().kind,
            RequestKind::Completion(CompletionTrigger::Character('.'))
        ));
        let result = automatic_answer(&app, true);
        answer(&mut app, request, result);
        app.handle_at(key(KeyCode::Char('a')), now);
        let request = expire(&mut app);
        assert!(matches!(
            request.request.unwrap().kind,
            RequestKind::Completion(CompletionTrigger::Incomplete)
        ));
        app.execute("auto-completion min-length 2").unwrap();
        app.handle_at(key(KeyCode::Char('n')), now);
        let request = expire(&mut app);
        let result = automatic_answer(&app, true);
        answer(&mut app, request, result);
        app.handle_at(key(KeyCode::Char('s')), now);
        let request = expire(&mut app);
        assert!(matches!(
            request.request.unwrap().kind,
            RequestKind::Completion(CompletionTrigger::Incomplete)
        ));
    }

    #[test]
    fn automatic_completion_refreshes_on_backspace_and_closes_below_threshold() {
        let (_directory, mut app) = automatic_fixture("an");
        let now = Instant::now() + Duration::from_secs(10);
        app.handle_at(key(KeyCode::Char('s')), now);
        let request = expire(&mut app);
        let result = automatic_answer(&app, true);
        answer(&mut app, request, result);
        app.handle_at(key(KeyCode::Backspace), now);
        let request = expire(&mut app);
        assert!(matches!(
            request.request.as_ref().unwrap().kind,
            RequestKind::Completion(CompletionTrigger::Incomplete)
        ));
        let result = automatic_answer(&app, false);
        answer(&mut app, request, result);
        app.handle_at(ctrl('h'), now);
        assert_eq!(app.editor.document().text(), "a");
        assert!(app.completion_deadline().is_none());
    }

    #[test]
    fn automatic_empty_results_errors_and_dismissal_do_not_reopen_or_report_messages() {
        for error in [false, true] {
            let (_directory, mut app) = automatic_fixture("a");
            app.handle(key(KeyCode::Char('n')));
            let request = expire(&mut app);
            let document = request.document.unwrap();
            app.handle_lsp_event(LspEvent::Answer {
                epoch: document.epoch,
                revision: document.snapshot.revision(),
                id: request.request.unwrap().id,
                result: if error {
                    Err("temporary failure".into())
                } else {
                    Ok(Answer::Completion(Completions {
                        items: vec![],
                        incomplete: false,
                        limited: false,
                    }))
                },
            });
            assert!(app.message.is_empty());
            assert!(!app.error);
            assert!(app.completion.active.is_none());
            assert!(app.completion_deadline().is_none());
        }
        let (_directory, mut app) = automatic_fixture("a");
        app.handle(key(KeyCode::Char('n')));
        let request = expire(&mut app);
        let result = automatic_answer(&app, false);
        answer(&mut app, request, result);
        app.handle(ctrl('c'));
        assert_eq!(app.editor.mode(), Mode::Insert);
        assert!(app.completion.active.is_none());
        assert!(app.completion_deadline().is_none());
        app.handle(key(KeyCode::Char('s')));
        assert!(app.completion_deadline().is_some());
    }

    #[test]
    fn automatic_settings_survive_dismissal_and_manual_completion_bypasses_them() {
        let (_directory, mut app) = automatic_fixture("a");
        app.execute("auto-completion delay 250").unwrap();
        app.execute("auto-completion min-length 3").unwrap();
        let now = Instant::now() + Duration::from_secs(10);
        app.handle_at(key(KeyCode::Char('n')), now);
        assert!(app.completion_deadline().is_none());
        app.handle_at(key(KeyCode::Char('s')), now);
        assert_eq!(
            app.completion_deadline(),
            Some(now + Duration::from_millis(250))
        );
        for argument in [
            "min-length 0",
            "min-length 257",
            "delay 10001",
            "delay nope",
            "unknown",
        ] {
            assert!(app.execute(&format!("auto-completion {argument}")).is_err());
        }
        assert_eq!(app.completion.config.min_length, 3);
        app.execute("auto-completion off").unwrap();
        assert!(app.completion_deadline().is_none());
        app.handle_at(key(KeyCode::Char('w')), now);
        assert!(app.completion_deadline().is_none());
        let request = start(&mut app);
        assert!(request.request.is_some());
        assert!(!app.completion.active.as_ref().unwrap().automatic);
        app.restart_language_server();
        assert!(!app.completion.config.enabled);
        assert_eq!(app.completion.config.delay, Duration::from_millis(250));
    }

    #[test]
    fn automatic_menu_accepts_only_after_selection_and_old_capabilities_are_ignored() {
        let (_directory, mut app) = automatic_fixture("a");
        assert!(!app.handle_lsp_event(LspEvent::Capabilities {
            epoch: u64::MAX,
            completion: None
        }));
        app.handle(key(KeyCode::Char('n')));
        let request = expire(&mut app);
        let result = automatic_answer(&app, false);
        answer(&mut app, request, result);
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.editor.document().text(), "an");
        app.handle(key(KeyCode::Enter));
        assert_eq!(app.editor.document().text(), "answer");
        assert_eq!(app.editor.mode(), Mode::Insert);
        assert!(app.completion_deadline().is_none());
        app.editor.execute("undo", 1).unwrap();
        assert_eq!(app.editor.document().text(), "an");
        let (_directory, mut app) = automatic_fixture("界");
        app.handle(key(KeyCode::Char('日')));
        assert!(app.completion_deadline().is_some());
    }

    #[test]
    fn automatic_completion_skips_unavailable_services_multiple_carets_and_external_changes() {
        let (_directory, mut app) = fixture(); // Server not initialized yet.
        app.handle(key(KeyCode::Char('w')));
        assert!(app.completion_deadline().is_none());
        assert!(!app.error);
        let (_directory, mut app) = automatic_fixture("a");
        app.editor
            .set_selections(
                SelectionSet::new(
                    vec![
                        Selection::cursor(CharOffset(0)),
                        Selection::cursor(CharOffset(1)),
                    ],
                    0,
                )
                .unwrap(),
            )
            .unwrap();
        app.handle(key(KeyCode::Char('n')));
        assert!(app.completion_deadline().is_none());
        let (_directory, mut app) = automatic_fixture("a");
        app.handle(key(KeyCode::Char('n')));
        app.editor.execute("move_left", 1).unwrap();
        assert!(app.completion_deadline().is_none());
        app.handle(key(KeyCode::Char('s')));
        app.editor = vex_editor::Editor::new(vex_core::Document::from("new buffer"));
        assert!(app.completion_deadline().is_none());
    }

    #[test]
    fn helix_navigation_acceptance_rejection_and_escape_dispatch() {
        let (_directory, mut app) = fixture();
        let request = start(&mut app);
        assert!(answer(&mut app, request, list(true)));
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.completion.active.as_ref().unwrap().selected, Some(0));
        app.handle(ctrl('n'));
        assert_eq!(app.completion.active.as_ref().unwrap().selected, Some(1));
        app.handle(key(KeyCode::BackTab));
        app.handle(key(KeyCode::Enter));
        assert_eq!(app.editor.document().text(), "// 界\r\nanswer");
        assert_eq!(app.editor.mode(), Mode::Insert);
        app.editor.execute("undo", 1).unwrap();
        let request = start(&mut app);
        answer(&mut app, request, list(true));
        app.handle(ctrl('p'));
        app.handle(ctrl('c'));
        assert_eq!(app.editor.document().text(), "// 界\r\nans");
        assert_eq!(app.editor.mode(), Mode::Insert);
        let request = start(&mut app);
        answer(&mut app, request, list(true));
        app.handle(key(KeyCode::Down));
        app.handle(key(KeyCode::Esc));
        assert_eq!(app.editor.document().text(), "// 界\r\nanswer");
        assert_eq!(app.editor.mode(), Mode::Normal);
    }

    #[test]
    fn backspace_aliases_refresh_unselected_completions_after_deleting() {
        for event in [key(KeyCode::Backspace), ctrl('h')] {
            let (_directory, mut app) = fixture();
            let old = start(&mut app);
            app.handle(event);
            assert_eq!(app.editor.document().text(), "// 界\r\nan");
            let newer = app.take_lsp_update().unwrap();
            assert!(matches!(
                newer.request.as_ref().unwrap().kind,
                RequestKind::Completion(_)
            ));
            assert!(!answer(&mut app, old, list(true)));
            assert!(answer(&mut app, newer, list(true)));
        }
    }

    #[test]
    fn resolution_waits_for_imports_preserves_following_input_and_rejects_stale_results() {
        let (_directory, mut app) = fixture();
        let old = start(&mut app);
        app.handle(key(KeyCode::Char('w'))); // Refresh, without selecting a suggestion.
        let newer = app.take_lsp_update().unwrap();
        assert!(!answer(&mut app, old, list(false)));
        assert_eq!(app.editor.document().text(), "// 界\r\nansw");
        app.handle(ctrl('c'));
        assert!(!answer(&mut app, newer, list(false)));
        app.editor.execute("undo", 1).unwrap();
        let request = start(&mut app);
        answer(&mut app, request, list(false));
        app.handle(key(KeyCode::Tab));
        let stale = app.take_lsp_update().unwrap();
        app.handle(key(KeyCode::Down));
        let current = app.take_lsp_update().unwrap();
        assert!(!answer(
            &mut app,
            stale,
            Answer::CompletionResolved(candidate("answer", true))
        ));
        app.handle(key(KeyCode::Char(';'))); // Implicit acceptance waits for resolve.
        assert!(app.input_waiting());
        assert_eq!(app.editor.document().text(), "// 界\r\nans");
        let mut item = candidate("another", true);
        item.additional_edits
            .push(Edit::insert(CharOffset(0), "use demo::another;\r\n"));
        assert!(answer(&mut app, current, Answer::CompletionResolved(item)));
        assert!(!app.input_waiting());
        assert_eq!(
            app.editor.document().text(),
            "use demo::another;\r\n// 界\r\nanother;"
        );
        app.editor.execute("undo", 1).unwrap(); // Following input is a separate typing group.
        app.editor.execute("undo", 1).unwrap(); // Replacement + import are one group.
        assert_eq!(app.editor.document().text(), "// 界\r\nans");
    }

    #[test]
    fn early_navigation_accepts_after_list_and_resolution_and_cancellation_retains_input() {
        let (_directory, mut app) = fixture();
        let request = start(&mut app);
        app.handle(key(KeyCode::Tab));
        app.handle(key(KeyCode::Enter));
        assert!(app.input_waiting());
        answer(&mut app, request, list(false));
        let resolve = app.take_lsp_update().unwrap();
        answer(
            &mut app,
            resolve,
            Answer::CompletionResolved(candidate("answer", true)),
        );
        assert_eq!(app.editor.document().text(), "// 界\r\nanswer");
        app.editor.execute("undo", 1).unwrap();
        let request = start(&mut app);
        answer(&mut app, request, list(false));
        app.handle(key(KeyCode::Tab));
        let resolve = app.take_lsp_update().unwrap();
        app.handle(key(KeyCode::Char(';')));
        app.handle(ctrl('c'));
        assert_eq!(app.editor.document().text(), "// 界\r\nans;");
        assert!(!app.input_waiting());
        assert!(!answer(
            &mut app,
            resolve,
            Answer::CompletionResolved(candidate("answer", true))
        ));
    }

    #[test]
    fn completion_boxes_keep_cursor_visible_and_fit_above_below_and_at_edges() {
        let (_directory, mut app) = fixture();
        let request = start(&mut app);
        answer(&mut app, request, list(true));
        app.handle(key(KeyCode::Tab));
        for (width, height) in [(0u16, 0u16), (1, 1), (8, 3), (24, 8), (80, 12), (120, 20)] {
            for (x, y) in [(0, 0), (width.saturating_sub(1), height.saturating_sub(3))] {
                let mut frame = Frame::default();
                frame.reset(width, height).unwrap();
                let cursor = crate::screen::Cursor {
                    x,
                    y,
                    shape: crate::screen::CursorShape::Bar,
                };
                frame.cursor = (width > 0 && height > 0).then_some(cursor);
                app.paint_completion(&mut frame, height.saturating_sub(2));
                if let Some(actual) = frame.cursor {
                    assert_eq!(actual, cursor);
                    assert_eq!(frame.style_at(x, y), Some(Style::Text));
                }
                if width == 120 && height == 20 {
                    assert!((0..height).any(|row| frame.row_text(row).contains("Complete")));
                    assert!((0..height).any(|row| frame.row_text(row).contains("Documentation")));
                    assert!((0..height).any(|row| {
                        (0..width).any(|col| frame.style_at(col, row) == Some(Style::Selection))
                    }));
                }
            }
        }
        app.editor.insert_text("x").unwrap();
        let mut frame = Frame::default();
        frame.reset(80, 12).unwrap();
        app.paint(&mut frame).unwrap();
        assert!(app.completion.active.is_none());
    }
}
