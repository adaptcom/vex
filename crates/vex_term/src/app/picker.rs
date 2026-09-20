//! Application lifecycle for shared file/symbol pickers and key-prefix hints.

use super::App;
use crate::{
    input,
    picker::{
        self, Action, Entry, Item, Layout, Picker, Preview,
        buffers::{BufferJob, BufferResult, CatalogEntry},
        files::{FileJob, FileResult, PreviewJob, PreviewResult},
        symbols::SymbolJob,
    },
    screen::{Frame, Style},
};
use crossterm::event::{Event, KeyEventKind};
use std::{io, path::PathBuf, sync::Arc};
use vex_editor::background::Cancellation;

mod symbols;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    File(PathBuf),
    Symbol(vex_lsp::Location),
    Buffer(vex_core::DocumentId),
}

enum Source {
    Files(Option<PathBuf>, PathBuf),
    Symbols(symbols::Source),
    Buffers(Arc<[CatalogEntry]>),
}

struct Active {
    view: Picker<Target>,
    session: u64,
    revision: u64,
    source: Source,
    cancellation: Cancellation,
    preview_cancel: Cancellation,
    preview_request: u64,
    preview_target: Option<Target>,
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
    symbol_job: Option<SymbolJob>,
    buffer_job: Option<BufferJob>,
}

impl App {
    pub(super) fn open_file_picker(&mut self) {
        let cwd = match std::env::current_dir() {
            Ok(path) => path,
            Err(error) => {
                self.fail(error);
                return;
            }
        };
        self.close_picker();
        self.dismiss_language_help();
        self.keys.cancel();
        self.prompt = None;
        if self.editor.search_prompt().is_some() {
            let _ = self.editor.execute("search_cancel", 1);
        }
        self.picker.next_session += 1;
        self.picker.active = Some(Active {
            view: Picker::new("Files · discovering project…".into()),
            session: self.picker.next_session,
            revision: 0,
            source: Source::Files(self.files.target().map(PathBuf::from), cwd),
            cancellation: Cancellation::default(),
            preview_cancel: Cancellation::default(),
            preview_request: 0,
            preview_target: None,
            accept_pending: false,
        });
        self.submit_picker_query();
    }

    pub(super) fn open_buffer_picker(&mut self) {
        self.close_picker();
        self.dismiss_language_help();
        self.keys.cancel();
        self.prompt = None;
        if self.editor.search_prompt().is_some() {
            let _ = self.editor.execute("search_cancel", 1);
        }
        let mut view = Picker::new("Buffers · * current · + modified".into());
        view.noun = "buffers";
        self.picker.next_session += 1;
        self.picker.active = Some(Active {
            view,
            session: self.picker.next_session,
            revision: 0,
            source: Source::Buffers(self.buffer_catalog()),
            cancellation: Cancellation::default(),
            preview_cancel: Cancellation::default(),
            preview_request: 0,
            preview_target: None,
            accept_pending: false,
        });
        self.submit_picker_query();
    }

    fn submit_picker_query(&mut self) {
        if self.symbol_picker_active() {
            self.submit_symbol_query();
            return;
        }
        let active = self.picker.active.as_mut().unwrap();
        active.cancellation.cancel();
        active.preview_cancel.cancel();
        active.cancellation = Cancellation::default();
        active.revision += 1;
        active.preview_target = None;
        active.accept_pending = false;
        self.picker.preview_job = None;
        if let Source::Buffers(catalog) = &active.source {
            self.picker.buffer_job = Some(BufferJob {
                session: active.session,
                revision: active.revision,
                catalog: catalog.clone(),
                query: active.view.query.text().into(),
                cancellation: active.cancellation.clone(),
            });
            return;
        }
        let Source::Files(origin, cwd) = &active.source else {
            unreachable!()
        };
        self.picker.file_job = Some(FileJob {
            session: active.session,
            revision: active.revision,
            source: Some((origin.clone(), cwd.clone())),
            query: active.view.query.text().into(),
            cancellation: active.cancellation.clone(),
        });
    }

    pub(super) fn request_picker_preview(&mut self) {
        let Some(active) = &mut self.picker.active else {
            return;
        };
        let target = Layout::new(self.size.0, self.size.1)
            .preview_left()
            .is_some()
            .then(|| active.view.selected().map(|entry| entry.value.clone()))
            .flatten();
        if target == active.preview_target {
            return;
        }
        active.preview_cancel.cancel();
        active.preview_cancel = Cancellation::default();
        active.preview_request += 1;
        active.preview_target = target.clone();
        active.view.preview = if target.is_some() {
            Preview::plain("Loading preview…")
        } else {
            Preview::default()
        };
        let session = active.session;
        let request = active.preview_request;
        let cancellation = active.preview_cancel.clone();
        self.picker.preview_job = target.and_then(|target| {
            let (path, position) = match target {
                Target::Buffer(id) => {
                    return self.buffer_preview(id, session, request, cancellation);
                }
                Target::File(path) => (path, None),
                Target::Symbol(location) => (location.path, Some(location.position)),
            };
            Some(PreviewJob {
                session,
                request,
                snapshot: self.snapshot_for_path(&path),
                language: None,
                position,
                path: Some(path),
                cancellation,
            })
        });
    }

    pub(super) fn refresh_picker_buffer(&mut self, document: vex_core::DocumentId) {
        let target = self
            .picker
            .active
            .as_ref()
            .and_then(|active| active.preview_target.as_ref());
        let changed = match target {
            Some(Target::Buffer(id)) => *id == document,
            Some(Target::File(path)) | Some(Target::Symbol(vex_lsp::Location { path, .. })) => self
                .snapshot_for_path(path)
                .is_some_and(|snapshot| snapshot.id() == document),
            None => false,
        };
        if changed {
            self.picker.active.as_mut().unwrap().preview_target = None;
            self.request_picker_preview();
        }
    }

    fn close_picker(&mut self) {
        if let Some(active) = self.picker.active.take() {
            if matches!(active.source, Source::Symbols(_)) {
                self.cancel_language_request();
            } else {
                self.picker.file_job = Some(FileJob {
                    session: active.session,
                    revision: active.revision,
                    source: None,
                    query: String::new(),
                    cancellation: Cancellation::default(),
                });
            }
        }
        self.picker.preview_job = None;
        self.picker.symbol_job = None;
        self.picker.buffer_job = None;
    }

    pub(crate) fn take_picker_job(&mut self) -> Option<FileJob> {
        self.picker.file_job.take()
    }
    pub(crate) fn take_preview_job(&mut self) -> Option<PreviewJob> {
        self.picker.preview_job.take()
    }

    pub(crate) fn take_symbol_job(&mut self) -> Option<SymbolJob> {
        self.picker.symbol_job.take()
    }

    pub(crate) fn take_buffer_job(&mut self) -> Option<BufferJob> {
        self.picker.buffer_job.take()
    }

    pub(crate) fn handle_buffer_result(&mut self, result: BufferResult) -> bool {
        let Some(active) = &mut self.picker.active else {
            return false;
        };
        if !matches!(active.source, Source::Buffers(_))
            || active.session != result.session
            || active.revision != result.revision
            || active.cancellation.is_cancelled()
        {
            return false;
        }
        active.view.replace(
            result
                .items
                .into_iter()
                .map(|item| Item {
                    entry: Arc::new(Entry {
                        label: item.entry.label.clone(),
                        value: Target::Buffer(item.entry.value),
                    }),
                    matched: item.matched,
                })
                .collect(),
        );
        active.view.matched = result.matched;
        active.view.total = result.total;
        active.view.pending = false;
        if active.accept_pending {
            active.accept_pending = false;
            self.accept_picker();
        } else {
            self.request_picker_preview();
        }
        true
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
        self.invalidate_symbol_picker();
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
            Event::FocusLost if matches!(active.source, Source::Symbols(_)) => Action::Cancel,
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
        if !matches!(active.source, Source::Files(..))
            || active.session != result.session
            || active.revision != result.revision
            || active.cancellation.is_cancelled()
        {
            return false;
        }
        active.view.title = format!("Files · {}", result.root.display());
        active.view.replace(
            result
                .items
                .into_iter()
                .map(|item| Item {
                    entry: Arc::new(Entry {
                        label: item.entry.label.clone(),
                        value: Target::File(item.entry.value.clone()),
                    }),
                    matched: item.matched,
                })
                .collect(),
        );
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
            || active.preview_target.is_none()
            || active.preview_cancel.is_cancelled()
        {
            return false;
        }
        active.view.preview = result.preview;
        true
    }

    fn accept_picker(&mut self) {
        let Some(target) = self
            .picker
            .active
            .as_ref()
            .and_then(|active| active.view.selected())
            .map(|entry| entry.value.clone())
        else {
            if let Some(active) = &mut self.picker.active {
                active.view.notice = format!("No matching {} · Esc close", active.view.noun);
            }
            return;
        };
        let result = match target {
            Target::File(path) => self.open_picked_file(path),
            Target::Symbol(location) => self.open_location(location),
            Target::Buffer(id) => {
                if id != self.editor.document().id() {
                    self.record_jump();
                }
                self.open_buffer(id)
            }
        };
        match result {
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
        // A path may disappear or become a directory after discovery. Do not
        // turn an outdated picker entry into an unrelated empty new buffer.
        if !std::fs::metadata(&path)?.is_file() {
            return Err(io::Error::other(
                "Selected path is no longer a regular file",
            ));
        }
        self.open_window_from_picker(&path)
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
        // Show aliases together so window mode fits without repeating each
        // command's documentation for its letter, arrow, and Ctrl variants.
        let mut entries: Vec<(String, &str)> = Vec::new();
        for (key, description) in hints.entries {
            if let Some((keys, _)) = entries.iter_mut().find(|(_, doc)| *doc == description) {
                keys.push_str(&format!("/{key}"));
            } else {
                entries.push((key.to_string(), description));
            }
        }
        let width = frame.width().min(78);
        let bottom = frame.height().saturating_sub(1);
        let height = (entries.len() + 2).min(usize::from(bottom)) as u16;
        if width < 4 || height < 3 {
            return;
        }
        let x = frame.width() - width;
        let y = bottom - height;
        picker::paint_box(
            frame,
            x,
            y,
            x + width,
            bottom,
            &format!(" {} · Esc cancel ", hints.title),
        );
        for (offset, (key, description)) in entries.iter().take(usize::from(height - 2)).enumerate()
        {
            let description = description.lines().next().unwrap_or(description);
            picker::label(
                frame,
                x + 2,
                y + 1 + offset as u16,
                width - 4,
                &format!("{key}  {description}"),
                Style::Text,
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
    fn buffer_picker_previews_unsaved_text_accepts_early_and_rejects_stale_results() {
        let (directory, mut app) = fixture();
        press(&mut app, "iUNSAVED ");
        app.handle(key(KeyCode::Esc));
        let original = app.editor.document().id();
        app.open_window_file(&directory.path().join("beta.txt"))
            .unwrap();
        press(&mut app, " b");
        let old = app.take_buffer_job().unwrap().run().unwrap();
        press(&mut app, "alpha");
        assert!(!app.handle_buffer_result(old));
        let result = app.take_buffer_job().unwrap().run().unwrap();
        assert!(app.handle_buffer_result(result));
        let active = app.picker.active.as_ref().unwrap();
        assert_eq!(active.view.items[0].entry.value, Target::Buffer(original));
        assert!(active.view.items[0].entry.label.contains('+'));
        let preview = app.take_preview_job().unwrap().run().unwrap();
        assert!(preview.preview.text.contains("UNSAVED alpha"));
        assert!(app.handle_preview_result(preview));
        let stale = app
            .buffer_preview(
                original,
                app.picker.active.as_ref().unwrap().session,
                app.picker.active.as_ref().unwrap().preview_request,
                Cancellation::default(),
            )
            .unwrap()
            .run()
            .unwrap();
        app.refresh_picker_buffer(original);
        assert!(!app.handle_preview_result(stale));
        // A query change cancels the previous generation, including early Enter.
        app.handle(key(KeyCode::Backspace));
        app.handle(key(KeyCode::Enter));
        assert!(app.input_waiting());
        let result = app.take_buffer_job().unwrap().run().unwrap();
        assert!(app.handle_buffer_result(result));
        assert!(!app.input_waiting());
        assert!(app.picker.active.is_none());
        assert_eq!(app.editor.document().id(), original);
        assert!(app.is_dirty());
        press(&mut app, " b");
        let job = app.take_buffer_job().unwrap();
        app.handle(key(KeyCode::Esc));
        assert!(job.run().is_none());
        press(&mut app, " bdoes-not-exist");
        app.handle(key(KeyCode::Enter));
        let result = app.take_buffer_job().unwrap().run().unwrap();
        app.handle_buffer_result(result);
        assert!(!app.input_waiting());
        assert!(
            app.picker
                .active
                .as_ref()
                .unwrap()
                .view
                .notice
                .contains("No matching buffers")
        );
    }

    #[test]
    fn scratch_buffer_picker_uses_the_configured_language_and_has_no_disk_dependency() {
        let mut app = App::from_document(vex_core::Document::from("fn main() {}\n"), (120, 24));
        app.editor.set_language(Some(vex_editor::Language::Rust));
        press(&mut app, " b");
        let result = app.take_buffer_job().unwrap().run().unwrap();
        app.handle_buffer_result(result);
        let job = app.take_preview_job().unwrap();
        assert!(job.path.is_none());
        let result = job.run().unwrap();
        assert_eq!(result.preview.text, "fn main() {}\n");
        assert!(!result.preview.highlights.is_empty());
        app.handle_preview_result(result);
        app.handle(key(KeyCode::Enter));
        assert!(app.picker.active.is_none());
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
        assert!(app.picker.active.is_none());
        assert_eq!(app.editor.document().text(), "beta contents");
        app.execute("jump_back").unwrap();
        app.take_lsp_update();
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
