//! Application lifecycle for the reusable file picker and key-prefix hints.

use super::App;
use crate::{
    files::FileState,
    input,
    picker::{
        self, Action, Layout, Picker, Preview,
        files::{FileJob, FileResult, PreviewJob, PreviewResult},
    },
    render::Viewport,
    screen::{Frame, Style},
};
use crossterm::event::{Event, KeyEventKind};
use std::{io, path::PathBuf};
use vex_editor::{ApplicationAction, Editor, Language, background::Cancellation};

struct Active {
    view: Picker<PathBuf>,
    session: u64,
    revision: u64,
    source: (Option<PathBuf>, PathBuf),
    cancellation: Cancellation,
    preview_cancel: Cancellation,
    preview_request: u64,
    preview_path: Option<PathBuf>,
    accept_pending: bool,
}

impl Drop for Active {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.preview_cancel.cancel();
    }
}

#[derive(Default)]
pub(super) struct State {
    active: Option<Active>,
    next_session: u64,
    file_job: Option<FileJob>,
    preview_job: Option<PreviewJob>,
}

impl App {
    pub(super) fn open_requested_picker(&mut self) {
        if self.editor.take_application_action() != Some(ApplicationAction::FilePicker) {
            return;
        }
        let cwd = match std::env::current_dir() {
            Ok(path) => path,
            Err(error) => {
                self.fail(error);
                return;
            }
        };
        self.dismiss_language_help();
        self.keys.cancel();
        self.prompt = None;
        if self.editor.search_direction().is_some() {
            let _ = self.editor.execute("search_cancel", 1);
        }
        self.picker.next_session += 1;
        self.picker.active = Some(Active {
            view: Picker::new("discovering project…".into()),
            session: self.picker.next_session,
            revision: 0,
            source: (self.files.target().map(PathBuf::from), cwd),
            cancellation: Cancellation::default(),
            preview_cancel: Cancellation::default(),
            preview_request: 0,
            preview_path: None,
            accept_pending: false,
        });
        self.submit_picker_query();
    }

    fn submit_picker_query(&mut self) {
        let active = self.picker.active.as_mut().unwrap();
        active.cancellation.cancel();
        active.preview_cancel.cancel();
        active.cancellation = Cancellation::default();
        active.revision += 1;
        active.preview_path = None;
        active.accept_pending = false;
        self.picker.preview_job = None;
        self.picker.file_job = Some(FileJob {
            session: active.session,
            revision: active.revision,
            source: Some(active.source.clone()),
            query: active.view.query.text().into(),
            cancellation: active.cancellation.clone(),
        });
    }

    pub(super) fn request_picker_preview(&mut self) {
        let Some(active) = &mut self.picker.active else {
            return;
        };
        let path = Layout::new(self.size.0, self.size.1)
            .preview_left()
            .is_some()
            .then(|| active.view.selected().map(|entry| entry.value.clone()))
            .flatten();
        if path == active.preview_path {
            return;
        }
        active.preview_cancel.cancel();
        active.preview_cancel = Cancellation::default();
        active.preview_request += 1;
        active.preview_path = path.clone();
        active.view.preview = if path.is_some() {
            Preview::plain("Loading preview…")
        } else {
            Preview::default()
        };
        self.picker.preview_job = path.map(|path| PreviewJob {
            session: active.session,
            request: active.preview_request,
            path,
            cancellation: active.preview_cancel.clone(),
        });
    }

    fn close_picker(&mut self) {
        if let Some(active) = self.picker.active.take() {
            self.picker.file_job = Some(FileJob {
                session: active.session,
                revision: active.revision,
                source: None,
                query: String::new(),
                cancellation: Cancellation::default(),
            });
        }
        self.picker.preview_job = None;
    }

    pub(crate) fn take_picker_job(&mut self) -> Option<FileJob> {
        self.picker.file_job.take()
    }
    pub(crate) fn take_preview_job(&mut self) -> Option<PreviewJob> {
        self.picker.preview_job.take()
    }

    /// Early Enter waits for the current ranking, preserving later editing keys
    /// in the event queue. Escape and resize retain the search queue's behavior.
    pub(crate) fn input_waiting(&self) -> bool {
        self.editor.search_waiting()
            || self.completion_waiting()
            || self
                .picker
                .active
                .as_ref()
                .is_some_and(|active| active.accept_pending)
    }

    pub(super) fn handle_picker_input(&mut self, event: &Event) -> Option<bool> {
        let active = self.picker.active.as_mut()?;
        let action = match event {
            Event::Paste(text) => {
                active.view.paste(text);
                Action::Query
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let Some(key) = input::key(*key) else {
                    return Some(false);
                };
                active.view.handle(
                    key,
                    usize::from(Layout::new(self.size.0, self.size.1).rows()),
                )
            }
            Event::Resize(..) | Event::FocusGained | Event::FocusLost => return None,
            _ => return Some(false),
        };
        match action {
            Action::Cancel => self.close_picker(),
            Action::Query => self.submit_picker_query(),
            Action::Selection => self.request_picker_preview(),
            Action::Accept => {
                if active.view.pending && active.view.items.is_empty() {
                    active.accept_pending = true;
                } else {
                    self.accept_picker();
                }
            }
            Action::None => {}
        }
        Some(true)
    }

    pub(crate) fn handle_picker_result(&mut self, result: FileResult) -> bool {
        let Some(active) = &mut self.picker.active else {
            return false;
        };
        if active.session != result.session
            || active.revision != result.revision
            || active.cancellation.is_cancelled()
        {
            return false;
        }
        active.view.title = result.root.display().to_string();
        active.view.replace(result.items);
        active.view.matched = result.matched;
        active.view.total = result.scanned;
        active.view.pending = result.scanning;
        active.view.notice = result.notice;
        let accept = active.accept_pending && !result.scanning;
        if accept {
            active.accept_pending = false;
            self.accept_picker();
        } else {
            self.request_picker_preview();
        }
        true
    }

    pub(crate) fn handle_preview_result(&mut self, result: PreviewResult) -> bool {
        let Some(active) = &mut self.picker.active else {
            return false;
        };
        if active.session != result.session
            || active.preview_request != result.request
            || active.preview_path.as_ref() != Some(&result.path)
            || active.preview_cancel.is_cancelled()
        {
            return false;
        }
        active.view.preview = result.preview;
        true
    }

    fn accept_picker(&mut self) {
        let Some(path) = self
            .picker
            .active
            .as_ref()
            .and_then(|active| active.view.selected())
            .map(|entry| entry.value.clone())
        else {
            if let Some(active) = &mut self.picker.active {
                active.view.notice = "No matching files · Esc close".into();
            }
            return;
        };
        match self.open_picked_file(path) {
            Ok(()) => {
                self.close_picker();
                self.clear_message();
            }
            Err(error) => self.picker.active.as_mut().unwrap().view.notice = error.to_string(),
        }
    }

    fn open_picked_file(&mut self, path: PathBuf) -> io::Result<()> {
        if self.files.target() == Some(path.as_path()) {
            return Ok(());
        }
        if self.is_dirty() {
            return Err(io::Error::other(
                "Save this buffer before opening another file · Esc close",
            ));
        }
        // A path may disappear or become a directory after discovery. Do not
        // turn an outdated picker entry into an unrelated empty new buffer.
        if !std::fs::metadata(&path)?.is_file() {
            return Err(io::Error::other(
                "Selected path is no longer a regular file",
            ));
        }
        let (document, files) = FileState::load(Some(&path))?;
        self.record_jump();
        let mut editor = Editor::new(document);
        editor.set_language(files.path().and_then(Language::from_path));
        editor.set_background_search(true);
        editor.set_background_syntax(true);
        self.editor = editor;
        self.files = files;
        self.automatic_language = true;
        self.viewport = Viewport::default();
        self.keys.cancel();
        self.prompt = None;
        Ok(())
    }

    pub(super) fn paint_active_picker(&mut self, frame: &mut Frame) {
        if let Some(active) = &mut self.picker.active {
            active.view.paint(frame);
        }
    }

    pub(super) fn paint_key_hints(&self, frame: &mut Frame) {
        if self.prompt.is_some() {
            return;
        }
        let Some(hints) = self.keys.hints() else {
            return;
        };
        let width = frame.width().min(78);
        let height = frame.height().min((hints.entries.len() + 2) as u16);
        if width == 0 || height == 0 {
            return;
        }
        let x = frame.width() - width;
        let y = frame.height() - height;
        for row in y..frame.height() {
            for col in x..frame.width() {
                frame.put(col, row, " ", Style::Selection);
            }
        }
        picker::label(
            frame,
            x,
            y,
            width,
            &format!(" {} · Esc cancel", hints.title),
            Style::Status,
        );
        for (offset, (key, description)) in hints
            .entries
            .iter()
            .take(usize::from(height.saturating_sub(1)))
            .enumerate()
        {
            let description = description.lines().next().unwrap_or(description);
            picker::label(
                frame,
                x,
                y + 1 + offset as u16,
                width,
                &format!(" {key}  {description}"),
                Style::Selection,
            );
        }
        frame.cursor = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker::files::FileWorker;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::fs;
    use vex_editor::Mode;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }
    fn press(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.handle(key(KeyCode::Char(ch)));
        }
    }
    fn fixture() -> (tempfile::TempDir, App) {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join(".git")).unwrap();
        fs::write(directory.path().join("alpha.txt"), "alpha contents").unwrap();
        fs::write(directory.path().join("beta.txt"), "beta contents").unwrap();
        let app = App::open(Some(&directory.path().join("alpha.txt")), (120, 18)).unwrap();
        (directory, app)
    }
    fn results(app: &mut App, worker: &mut FileWorker) -> Vec<FileResult> {
        let job = app.take_picker_job().unwrap();
        let mut results = Vec::new();
        worker.run(job, |result| results.push(result));
        results
    }
    fn finish(app: &mut App, worker: &mut FileWorker) {
        for result in results(app, worker) {
            app.handle_picker_result(result);
        }
    }

    #[test]
    fn hints_and_picker_render_and_escape_preserves_selection_viewport_and_document() {
        let (_directory, mut app) = fixture();
        press(&mut app, "vl");
        let selection = app.editor.selections().clone();
        let revision = app.editor.document().revision();
        let mut frame = Frame::default();
        frame.reset(120, 18).unwrap();
        app.paint(&mut frame).unwrap();
        let origin = frame.row_text(0);
        let status = frame.row_text(16);
        let viewport = app.viewport;
        press(&mut app, " ");
        frame.reset(120, 18).unwrap();
        app.paint(&mut frame).unwrap();
        assert!((0..18).any(|row| frame.row_text(row).contains("Space · Esc cancel")));
        press(&mut app, "f");
        finish(&mut app, &mut FileWorker::default());
        let preview = app.take_preview_job().unwrap().run().unwrap();
        assert!(app.handle_preview_result(preview));
        frame.reset(120, 18).unwrap();
        app.paint(&mut frame).unwrap();
        assert_eq!(frame.row_text(0), origin);
        assert_eq!(frame.row_text(16), status);
        assert!((2..16).any(|row| frame.row_text(row).contains("alpha contents")));
        assert!(frame.row_text(2).contains('┌'));
        assert_eq!(app.viewport, viewport);
        press(&mut app, "beta");
        app.handle(key(KeyCode::Esc));
        assert!(app.picker.active.is_none());
        assert_eq!(app.editor.mode(), Mode::Select);
        assert_eq!(app.editor.selections(), &selection);
        assert_eq!(app.editor.document().revision(), revision);
        assert_eq!(app.editor.document().text(), "alpha contents");
        frame.reset(120, 18).unwrap();
        app.paint(&mut frame).unwrap();
        assert_eq!(frame.row_text(0), origin);
        assert_eq!(app.viewport, viewport);
    }

    #[test]
    fn highlighted_previews_cancel_on_resize_and_do_not_color_another_selection() {
        let (directory, mut app) = fixture();
        fs::write(directory.path().join("example.rs"), "fn main() {}\n").unwrap();
        let mut worker = FileWorker::default();
        press(&mut app, " fexample");
        finish(&mut app, &mut worker);
        let preview = app.take_preview_job().unwrap().run().unwrap();
        assert!(!preview.preview.highlights.is_empty());
        app.handle(Event::Resize(44, 9));
        assert!(!app.handle_preview_result(preview));
        assert!(app.take_preview_job().is_none());
        assert!(
            app.picker
                .active
                .as_ref()
                .unwrap()
                .view
                .preview
                .highlights
                .is_empty()
        );
        app.handle(Event::Resize(120, 18));
        let preview = app.take_preview_job().unwrap().run().unwrap();
        assert!(app.handle_preview_result(preview));
        let mut frame = Frame::default();
        frame.reset(120, 18).unwrap();
        app.paint(&mut frame).unwrap();
        assert_eq!(
            frame.style_at(63, 3),
            Some(Style::Syntax(vex_editor::Highlight::Keyword))
        );
        assert_eq!(app.editor.document().text(), "alpha contents");
        app.handle(key(KeyCode::Backspace));
        assert!(
            app.picker
                .active
                .as_ref()
                .unwrap()
                .view
                .preview
                .highlights
                .is_empty()
        );
        finish(&mut app, &mut worker);
        let late = app.take_preview_job().unwrap().run().unwrap();
        app.handle(key(KeyCode::Esc));
        assert!(!app.handle_preview_result(late));
    }

    #[test]
    fn stale_query_preview_and_previous_session_results_cannot_replace_current_picker() {
        let (_directory, mut app) = fixture();
        let mut worker = FileWorker::default();
        press(&mut app, " f");
        let old = results(&mut app, &mut worker);
        press(&mut app, "beta");
        for result in old {
            assert!(!app.handle_picker_result(result));
        }
        finish(&mut app, &mut worker);
        let preview = app.take_preview_job().unwrap().run().unwrap();
        app.handle(key(KeyCode::Backspace));
        assert!(!app.handle_preview_result(preview));
        let old = results(&mut app, &mut worker);
        app.handle(key(KeyCode::Esc));
        press(&mut app, " f");
        for result in old {
            assert!(!app.handle_picker_result(result));
        }
        assert!(app.picker.active.as_ref().unwrap().view.items.is_empty());
    }

    #[test]
    fn early_enter_opens_the_current_query_and_unsaved_changes_are_protected() {
        let (directory, mut app) = fixture();
        app.enable_lsp();
        press(&mut app, "ll fbeta");
        app.handle(key(KeyCode::Enter));
        assert!(app.input_waiting());
        finish(&mut app, &mut FileWorker::default());
        assert!(!app.input_waiting());
        assert!(app.picker.active.is_none());
        assert_eq!(app.editor.document().text(), "beta contents");
        app.editor.execute("jump_back", 1).unwrap();
        app.take_lsp_update();
        assert_eq!(app.editor.document().text(), "alpha contents");
        assert_eq!(app.editor.selections().primary().start().0, 2);
        press(&mut app, "ix");
        app.handle(key(KeyCode::Esc));
        press(&mut app, " fbeta");
        app.handle(key(KeyCode::Enter));
        finish(&mut app, &mut FileWorker::default());
        assert!(
            app.picker
                .active
                .as_ref()
                .unwrap()
                .view
                .notice
                .contains("Save this buffer")
        );
        assert!(app.is_dirty());
        assert_eq!(app.editor.document().text(), "alxpha contents");
        assert_eq!(
            fs::read_to_string(directory.path().join("alpha.txt")).unwrap(),
            "alpha contents"
        );
    }

    #[test]
    fn named_command_and_custom_binding_open_the_same_picker_and_late_deleted_files_fail() {
        let (directory, mut app) = fixture();
        app.execute("file_picker").unwrap();
        press(&mut app, "beta");
        finish(&mut app, &mut FileWorker::default());
        fs::remove_file(directory.path().join("beta.txt")).unwrap();
        app.handle(key(KeyCode::Enter));
        assert_eq!(app.editor.document().text(), "alpha contents");
        assert!(app.picker.active.is_some());
        app.handle(key(KeyCode::Esc));
        let mut map = vex_editor::Keymap::default();
        map.bind(
            Mode::Normal,
            vec![vex_editor::Key::Char('p')],
            "file_picker",
        )
        .unwrap();
        app.keys = vex_editor::KeyHandler::new(map);
        press(&mut app, "p");
        assert!(app.picker.active.is_some());
    }
}
