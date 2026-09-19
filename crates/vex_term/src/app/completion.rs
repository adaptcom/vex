//! Insert-mode completion sessions and cursor-anchored presentation.

use super::App;
use crate::{
    input,
    picker::{label, paint_box},
    screen::{Frame, Style},
};
use crossterm::event::{Event, KeyEventKind};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use vex_core::{DocumentId, Revision, SelectionSet, display};
use vex_editor::{Key, Mode};
use vex_lsp::{CompletionItem, Completions, RequestKind};

struct Session {
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

#[derive(Default)]
pub(super) struct State {
    active: Option<Session>,
    outgoing: Option<RequestKind>,
}

impl App {
    pub(super) fn begin_completion(&mut self) {
        self.completion = State {
            active: Some(Session {
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
            }),
            outgoing: None,
        };
    }

    pub(super) fn take_completion_request(&mut self) -> Option<RequestKind> {
        self.completion.outgoing.take()
    }

    pub(super) fn invalidate_completion(&mut self) {
        if self.completion.active.as_ref().is_some_and(|session| {
            session.document != self.editor.document().id()
                || session.revision != self.editor.document().revision()
                || session.selections != *self.editor.selections()
                || self.editor.mode() != Mode::Insert
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

    /// Typing before choosing an entry requests a fresh list for the new text.
    /// Only an explicitly opened session does this; automatic triggering is separate.
    pub(super) fn refresh_completion_on_input(&self, event: &Event) -> bool {
        self.completion
            .active
            .as_ref()
            .is_some_and(|session| session.selected.is_none())
            && matches!(event, Event::Key(key) if key.kind != KeyEventKind::Release
                && matches!(input::key(*key), Some(Key::Char(ch)) if ch == '_' || ch.is_alphanumeric())
                || key.kind != KeyEventKind::Release
                    && matches!(input::key(*key), Some(Key::Backspace | Key::Ctrl('h'))))
    }

    /// Completion keys take precedence over insertion only while the menu is active.
    /// Ctrl-c rejects; other keys commit an explicitly selected item, then dispatch.
    pub(super) fn handle_completion_input(&mut self, event: &Event) -> Option<bool> {
        self.invalidate_completion();
        let session = self.completion.active.as_ref()?;
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
        session.resolving = request.as_ref().map(|_| index);
        self.cancel_language_request();
        self.completion.outgoing = request;
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
        let following = self
            .completion
            .active
            .as_mut()
            .and_then(|session| session.following.take());
        self.dismiss_language_help();
        if let Some(event) = following {
            self.handle(event);
        }
        self.fail(error);
    }

    pub(super) fn paint_completion(&mut self, frame: &mut Frame) {
        self.invalidate_completion();
        let Some(session) = &mut self.completion.active else {
            return;
        };
        let Some(cursor) = frame.cursor else { return };
        let body = frame.height().saturating_sub(2);
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
            RequestKind::Completion
        ));
        request
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
                RequestKind::Completion
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
                app.paint_completion(&mut frame);
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
