//! Directory navigation uses the ordinary picker view, jobs, and file opening.

use super::*;
use crate::picker::{
    Bookmark,
    browser::{Job, Result},
};
use std::{collections::VecDeque, path::Path};

pub(super) struct Source {
    pub directory: PathBuf,
    history: VecDeque<(PathBuf, Bookmark<Target>)>,
}

fn view(path: &Path) -> Picker<Target> {
    let mut view = Picker::new(format!("Browse · {}", crate::paths::display(path)));
    view.noun = "entries";
    view.browse = true;
    view
}

impl App {
    pub(in crate::app) fn open_browser(&mut self, path: Option<&Path>) -> io::Result<()> {
        let directory = std::path::absolute(
            path.or_else(|| self.files.target().and_then(Path::parent))
                .unwrap_or(Path::new(".")),
        )?;
        let mut view = view(&directory);
        if path.is_none()
            && let Some(file) = self.files.target()
        {
            view.select_on_refresh(Target::File(file.into()));
        }
        self.close_picker();
        self.dismiss_language_help();
        self.keys.cancel(&mut self.editor);
        self.prompt = None;
        if self.editor.search_prompt().is_some() {
            let _ = self.editor.execute("search_cancel", 1);
        }
        self.picker.next_session += 1;
        self.picker.active = Some(Active {
            view,
            session: self.picker.next_session,
            revision: 0,
            source: super::Source::Browser(Source {
                directory,
                history: VecDeque::new(),
            }),
            cancellation: Cancellation::default(),
            preview_cancel: Cancellation::default(),
            preview_request: 0,
            preview_target: None,
            accept_pending: false,
        });
        self.submit_picker_query();
        self.clear_message();
        Ok(())
    }

    pub(super) fn browser_parent(&mut self) {
        let Some(Active {
            source: super::Source::Browser(source),
            ..
        }) = &self.picker.active
        else {
            return;
        };
        if let Some(parent) = source.directory.parent() {
            self.browse_directory(parent.into());
        }
    }

    pub(super) fn browse_directory(&mut self, directory: PathBuf) {
        let Some(active) = &mut self.picker.active else {
            return;
        };
        let super::Source::Browser(source) = &mut active.source else {
            return;
        };
        if directory == source.directory {
            return;
        }
        let mut next = view(&directory);
        if let Some(index) = source
            .history
            .iter()
            .position(|(path, _)| *path == directory)
        {
            let (_, bookmark) = source.history.remove(index).unwrap();
            next.restore_bookmark(bookmark);
        } else if source.directory.parent() == Some(directory.as_path()) {
            next.select_on_refresh(Target::Directory(source.directory.clone()));
        }
        // Keep only small checkpoints for the most recent directories.
        if source.history.len() == 64 {
            source.history.pop_front();
        }
        source.history.push_back((
            std::mem::replace(&mut source.directory, directory),
            active.view.bookmark(),
        ));
        active.view = next;
        self.submit_picker_query();
    }

    pub(crate) fn take_browser_job(&mut self) -> Option<Job> {
        self.picker.browser_job.take()
    }

    pub(crate) fn handle_browser_result(&mut self, result: Result) -> bool {
        let Result {
            directory,
            ranked: result,
        } = result;
        let Some(active) = &mut self.picker.active else {
            return false;
        };
        if !matches!(active.source, super::Source::Browser(_))
            || active.session != result.session
            || active.revision != result.revision
            || active.cancellation.is_cancelled()
        {
            return false;
        }
        if let Some(directory) = directory
            && let super::Source::Browser(source) = &mut active.source
        {
            active.view.title = format!("Browse · {}", crate::paths::display(&directory));
            source.directory = directory;
        }
        active.view.replace(
            result
                .items
                .into_iter()
                .map(|item| Item {
                    entry: Arc::new(Entry {
                        label: item.entry.label.clone(),
                        value: if item.entry.value.directory {
                            Target::Directory(item.entry.value.path.clone())
                        } else {
                            Target::File(item.entry.value.path.clone())
                        },
                    }),
                    matched: item.matched,
                })
                .collect(),
        );
        active.view.matched = result.matched;
        active.view.total = result.total;
        active.view.pending = false;
        active.view.notice = result.notice;
        let accept = active.accept_pending;
        active.accept_pending = false;
        if accept && !active.view.items.is_empty() {
            self.accept_picker();
        } else {
            self.request_picker_preview();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker::browser::Worker;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::fs;

    fn key(app: &mut App, code: KeyCode) {
        app.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    fn press(app: &mut App, text: &str) {
        for ch in text.chars() {
            key(app, KeyCode::Char(ch));
        }
    }

    fn fixture() -> (tempfile::TempDir, App, Worker) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path();
        fs::create_dir(path.join(".git")).unwrap();
        fs::create_dir(path.join("src")).unwrap();
        fs::write(path.join("origin.rs"), "fn original() {}\n").unwrap();
        fs::write(path.join("src/界 target.rs"), "fn target() {}\n").unwrap();
        let app = App::open(Some(&path.join("origin.rs")), (120, 20)).unwrap();
        (root, app, Worker::default())
    }

    fn finish(app: &mut App, worker: &mut Worker) {
        let result = worker.run(app.take_browser_job().unwrap()).unwrap();
        assert!(app.handle_browser_result(result));
    }

    fn preview(app: &mut App) -> String {
        let result = app.take_preview_job().unwrap().run().unwrap();
        assert!(app.handle_preview_result(result));
        app.picker
            .active
            .as_ref()
            .unwrap()
            .view
            .preview
            .text
            .clone()
    }

    fn selected(app: &App) -> String {
        app.picker
            .active
            .as_ref()
            .unwrap()
            .view
            .selected()
            .unwrap()
            .label
            .clone()
    }

    fn directory(app: &App) -> &Path {
        let super::super::Source::Browser(source) = &app.picker.active.as_ref().unwrap().source
        else {
            panic!()
        };
        &source.directory
    }

    #[test]
    fn opens_beside_current_file_and_previews_unsaved_text_without_changing_the_view() {
        let (root, mut app, mut worker) = fixture();
        press(&mut app, "i// unsaved ");
        key(&mut app, KeyCode::Esc);
        press(&mut app, "v");
        let selections = app.editor.selections().clone();
        let revision = app.editor.document().revision();
        press(&mut app, " e");
        finish(&mut app, &mut worker);
        assert_eq!(selected(&app), "origin.rs");
        assert!(preview(&mut app).starts_with("// unsaved fn original"));
        assert!(
            !app.picker
                .active
                .as_ref()
                .unwrap()
                .view
                .preview
                .highlights
                .is_empty()
        );
        key(&mut app, KeyCode::Esc);
        assert!(app.picker.active.is_none());
        assert_eq!(app.editor.mode(), vex_editor::Mode::Select);
        assert_eq!(app.editor.selections(), &selections);
        assert_eq!(app.editor.document().revision(), revision);
        // Explicit paths may contain aliases such as macOS's /var symlink.
        app.open_browser(Some(root.path())).unwrap();
        press(&mut app, "origin");
        finish(&mut app, &mut worker);
        assert!(preview(&mut app).starts_with("// unsaved fn original"));
    }

    #[test]
    fn directory_preview_early_enter_query_history_and_file_open_share_the_picker() {
        let (root, mut app, mut worker) = fixture();
        press(&mut app, " e");
        press(&mut app, "src");
        finish(&mut app, &mut worker);
        assert_eq!(preview(&mut app), "界 target.rs\n");
        // The preview title and its contents stay together while loading.
        let mut frame = Frame::default();
        frame.reset(120, 20).unwrap();
        app.paint(&mut frame).unwrap();
        assert!((0..20).any(|row| frame.row_text(row).contains("Preview · src/")));
        key(&mut app, KeyCode::Left); // Remember the query caret as well.
        key(&mut app, KeyCode::Enter);
        assert!(directory(&app).ends_with("src"));
        finish(&mut app, &mut worker);
        assert!(preview(&mut app).contains("fn target"));
        key(&mut app, KeyCode::Backspace);
        finish(&mut app, &mut worker);
        assert_eq!(selected(&app), "src/");
        let view = &app.picker.active.as_ref().unwrap().view;
        assert_eq!(view.query.text(), "src");
        assert_eq!(view.query.cursor(), 2);
        key(&mut app, KeyCode::Enter);
        // A query and Enter arriving before listing completion must not open
        // a stale result or release subsequent editor input too early.
        press(&mut app, "界");
        key(&mut app, KeyCode::Enter);
        assert!(app.input_waiting());
        finish(&mut app, &mut worker);
        assert!(!app.input_waiting());
        assert!(app.picker.active.is_none());
        assert!(app.files.target().unwrap().ends_with("src/界 target.rs"));
        assert_eq!(app.editor.document().text(), "fn target() {}\n");
        assert!(app.execute("last_picker").is_ok());
        finish(&mut app, &mut worker);
        assert!(directory(&app).ends_with("src"));
        assert_eq!(app.picker.active.as_ref().unwrap().view.query.text(), "界");
        key(&mut app, KeyCode::Backspace); // Edit the query first.
        assert!(directory(&app).ends_with("src"));
        finish(&mut app, &mut worker);
        key(&mut app, KeyCode::Backspace);
        finish(&mut app, &mut worker);
        assert_eq!(directory(&app), root.path().canonicalize().unwrap());
        assert_eq!(selected(&app), "src/");
    }

    #[test]
    fn stale_results_after_navigation_close_and_reopen_are_ignored() {
        let (root, mut app, mut worker) = fixture();
        app.open_browser(Some(root.path())).unwrap();
        finish(&mut app, &mut worker);
        assert_eq!(selected(&app), "src/");
        let old_preview = app.take_preview_job().unwrap().run().unwrap();
        key(&mut app, KeyCode::Enter);
        let old_result = worker.run(app.take_browser_job().unwrap()).unwrap();
        key(&mut app, KeyCode::Backspace);
        assert!(!app.handle_browser_result(old_result));
        assert!(!app.handle_preview_result(old_preview));
        let old_result = worker.run(app.take_browser_job().unwrap()).unwrap();
        key(&mut app, KeyCode::Esc);
        app.execute("last_picker").unwrap();
        assert!(!app.handle_browser_result(old_result));
        finish(&mut app, &mut worker);
        assert_eq!(directory(&app), root.path().canonicalize().unwrap());
    }

    #[test]
    fn explicit_paths_errors_scratch_buffers_and_root_navigation_are_recoverable() {
        let (root, mut app, mut worker) = fixture();
        let path = root.path().join("a directory");
        fs::create_dir(&path).unwrap();
        app.execute(&format!("browse {}", path.display())).unwrap();
        finish(&mut app, &mut worker);
        assert_eq!(directory(&app), path.canonicalize().unwrap());
        assert!(app.picker.active.as_ref().unwrap().view.items.is_empty());
        assert!(app.execute("browse!").is_err());
        app.open_browser(Some(&path.join("missing"))).unwrap();
        key(&mut app, KeyCode::Enter);
        finish(&mut app, &mut worker);
        assert!(!app.input_waiting());
        assert!(
            app.picker
                .active
                .as_ref()
                .unwrap()
                .view
                .notice
                .starts_with("Cannot browse:")
        );
        key(&mut app, KeyCode::Backspace);
        finish(&mut app, &mut worker);
        assert_eq!(directory(&app), path.canonicalize().unwrap());
        let root_path = path.ancestors().last().unwrap();
        app.open_browser(Some(root_path)).unwrap();
        key(&mut app, KeyCode::Backspace);
        assert_eq!(directory(&app), root_path);
        let mut scratch = App::from_document(vex_core::Document::from("scratch"), (40, 8));
        press(&mut scratch, " e");
        assert_eq!(directory(&scratch), std::env::current_dir().unwrap());
        let selections = scratch.editor.selections().clone();
        key(&mut scratch, KeyCode::Esc);
        assert_eq!(scratch.editor.selections(), &selections);
        assert_eq!(scratch.editor.document().text(), "scratch");
    }

    #[test]
    fn removed_files_keep_the_browser_open_and_narrow_layout_avoids_preview_work() {
        let (root, mut app, mut worker) = fixture();
        app.open_browser(Some(root.path())).unwrap();
        press(&mut app, "origin");
        finish(&mut app, &mut worker);
        app.handle(Event::Resize(40, 8));
        assert!(app.take_preview_job().is_none());
        fs::remove_file(root.path().join("origin.rs")).unwrap();
        // Use another selected target: the current buffer may be opened even
        // if its on-disk file is gone, retaining unsaved text.
        app.open_browser(Some(&root.path().join("src"))).unwrap();
        finish(&mut app, &mut worker);
        fs::remove_file(root.path().join("src/界 target.rs")).unwrap();
        key(&mut app, KeyCode::Enter);
        assert!(app.picker.active.is_some());
        assert!(!app.picker.active.as_ref().unwrap().view.notice.is_empty());
        assert_eq!(app.editor.document().text(), "fn original() {}\n");
    }
}
