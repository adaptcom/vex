//! Repository view lifecycle. Only read queries use this replaceable mailbox;
//! mutations use the separate ordered Git write worker.
use super::{ActivePrompt, App, PromptKind};
use crate::{
    git_status::View,
    input::{self, Prompt},
};
use crossterm::event::{Event, KeyEventKind};
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use vex_core::{CharOffset, Selection, SelectionSet};
use vex_editor::{Key, background::Cancellation};
use vex_git::status::{Batch, Entry, FileKey, Group, Request, Result as StatusResult};
const REFRESH: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(super) struct State {
    pub(super) views: BTreeMap<PathBuf, View>,
    visible: Vec<PathBuf>,
    due: Option<Instant>,
    request: u64,
    cancellation: Cancellation,
}
impl Drop for State {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

impl App {
    pub(super) fn open_git_status(&mut self) -> io::Result<()> {
        if self.active_git_view().is_some() {
            self.refresh_status();
            return Ok(());
        }
        let origin = self
            .git_write
            .drafts
            .get(&self.editor.document().id())
            .map(|draft| draft.status_key.clone())
            .or_else(|| {
                self.files
                    .target()
                    .and_then(Path::parent)
                    .map(Path::to_path_buf)
            })
            .map_or_else(std::env::current_dir, Ok)?;
        if !self.status.views.contains_key(&origin) {
            if self.status.views.len() >= 16 {
                let visible = self.git_view_keys();
                if let Some(old) = self
                    .status
                    .views
                    .keys()
                    .find(|key| {
                        !visible.contains(key)
                            && !self
                                .git_write
                                .drafts
                                .values()
                                .any(|draft| &draft.status_key == *key)
                            && !self.status.views[*key]
                                .snapshot
                                .as_ref()
                                .is_some_and(|snapshot| self.git_operation_for(&snapshot.root))
                    })
                    .cloned()
                {
                    self.status.views.remove(&old);
                } else {
                    return Err(io::Error::other("too many repository views"));
                }
            }
            self.status.views.insert(origin.clone(), View::default());
        }
        self.dismiss_language_help();
        self.editor.finish_undo_group();
        let _ = self.editor.execute("search_cancel", 1);
        self.keys.cancel(&mut self.editor);
        self.git_write.status_prefix = None;
        self.prompt = None;
        self.clear_message();
        self.set_git_view(Some(origin));
        self.refresh_status();
        Ok(())
    }

    pub(super) fn close_git_status(&mut self) {
        self.git_write.status_prefix = None;
        if let Some(key) = self.active_git_view().cloned() {
            self.status.views.get_mut(&key).unwrap().help = false;
        }
        self.set_git_view(None);
        self.keys.cancel(&mut self.editor);
        self.prompt = None;
        self.clear_message();
    }

    pub(super) fn refresh_status(&mut self) {
        let visible = self.git_view_keys();
        if visible.is_empty() {
            return;
        }
        self.status.cancellation.cancel();
        self.status.due = Some(Instant::now());
        for key in visible {
            if let Some(view) = self.status.views.get_mut(&key) {
                view.refreshing = true;
            }
        }
    }
    pub(crate) fn status_deadline(&self) -> Option<Instant> {
        self.status.due
    }

    pub(crate) fn take_status_batch(&mut self, now: Instant) -> Option<Batch> {
        let visible = self.git_view_keys();
        if visible != self.status.visible {
            self.status.cancellation.cancel();
            self.status.visible = visible.clone();
            self.status.due = (!visible.is_empty()).then_some(now);
        }
        let dirty = self.unsaved_paths();
        for view in self.status.views.values_mut() {
            let unsaved = view
                .snapshot
                .as_ref()
                .map(|snapshot| {
                    dirty
                        .iter()
                        .filter_map(|path| path.strip_prefix(&snapshot.root).ok())
                        .map(|path| Entry {
                            key: FileKey {
                                group: Group::Unsaved,
                                path: path.into(),
                            },
                            old_path: None,
                            status: '*',
                            submodule: false,
                        })
                        .collect()
                })
                .unwrap_or_default();
            if view.unsaved != unsaved {
                view.unsaved = unsaved;
                view.rebuild();
            }
        }
        if visible.is_empty() || self.status.due.is_none_or(|deadline| deadline > now) {
            return None;
        }
        self.status.cancellation.cancel();
        self.status.cancellation = Cancellation::default();
        self.status.request += 1;
        self.status.due = None;
        let views = visible
            .into_iter()
            .map(|origin| {
                let view = self.status.views.get_mut(&origin).unwrap();
                view.refreshing = true;
                Request {
                    origin,
                    expanded: view.expanded.iter().cloned().collect(),
                }
            })
            .collect();
        Some(Batch {
            request: self.status.request,
            views,
            cancellation: self.status.cancellation.clone(),
        })
    }

    pub(crate) fn handle_status_result(&mut self, result: StatusResult) -> bool {
        if result.request != self.status.request || self.status.cancellation.is_cancelled() {
            return false;
        }
        let mut reload = false;
        for (mut origin, snapshot) in result.views {
            // Discover on the worker before reusing a repository view, so
            // nested repositories and submodules retain their own identities.
            if let Ok(snapshot) = &snapshot {
                let existing = self
                    .status
                    .views
                    .iter()
                    .find(|(key, view)| {
                        **key != origin
                            && view
                                .snapshot
                                .as_ref()
                                .is_some_and(|old| old.root == snapshot.root)
                    })
                    .map(|(key, _)| key.clone());
                if let Some(existing) = existing {
                    self.remap_git_view(&origin, &existing);
                    self.status.views.remove(&origin);
                    origin = existing;
                    reload |= !self.status.views[&origin].expanded.is_empty();
                }
            }
            if let Some(view) = self.status.views.get_mut(&origin) {
                view.update(snapshot);
            }
        }
        self.status.visible = self.git_view_keys();
        self.status.due = if self.status.visible.is_empty() {
            None
        } else {
            Some(Instant::now() + if reload { Duration::ZERO } else { REFRESH })
        };
        true
    }

    pub(super) fn toggle_git_section(&mut self) -> io::Result<()> {
        let key = self
            .active_git_view()
            .cloned()
            .ok_or_else(|| io::Error::other("open Git status first"))?;
        if self
            .status
            .views
            .get_mut(&key)
            .unwrap()
            .toggle()
            .map_err(io::Error::other)?
        {
            self.refresh_status();
        }
        Ok(())
    }

    /// Visit the selected file/change, using the existing buffer when open.
    pub(super) fn visit_git_change(&mut self) -> io::Result<()> {
        let key = self
            .active_git_view()
            .cloned()
            .ok_or_else(|| io::Error::other("open Git status first"))?;
        let Some((path, line, anchor)) = self.status.views[&key].destination() else {
            return self.toggle_git_section();
        };
        if !path.is_file() {
            return Err(io::Error::other(
                "This path is deleted or is a directory; its status remains available here",
            ));
        }
        self.open_window_from_picker(&path)?;
        self.set_git_view(None);
        let text = self.editor.document().text();
        let mut line = line.min(text.len_lines().saturating_sub(1));
        // Index positions may have shifted on disk or in an unsaved buffer.
        if let Some(anchor) = anchor.filter(|anchor| anchor.len() <= 4096)
            && let Some(found) = (line.saturating_sub(200)
                ..text.len_lines().min(line.saturating_add(201)))
                .filter(|&i| {
                    text.line(i).len_bytes() <= anchor.len() + 2
                        && text.line(i).to_string().trim_end_matches(['\r', '\n']) == anchor
                })
                .min_by_key(|i| i.abs_diff(line))
        {
            line = found;
        }
        let offset = CharOffset(text.line_to_char(line));
        self.editor
            .set_selections(SelectionSet::single(Selection::cursor(offset)))
            .map_err(io::Error::other)?;
        self.keys.cancel(&mut self.editor);
        self.prompt = None;
        self.clear_message();
        Ok(())
    }

    pub(super) fn handle_status_input(&mut self, event: &Event) -> Option<bool> {
        let key = self.active_git_view()?.to_path_buf();
        if self.prompt.is_some() {
            return None;
        }
        match event {
            Event::Resize(..) | Event::FocusGained => None,
            Event::Key(event) if event.kind != KeyEventKind::Release => {
                let Some(pressed) = input::key(*event) else {
                    return Some(false);
                };
                self.clear_message();
                if self.git_write.status_prefix.take().as_ref() == Some(&key) {
                    if pressed == Key::Char('c')
                        && let Err(error) = self.begin_git_commit()
                    {
                        self.fail(error);
                    }
                    return Some(true);
                }
                let pending = self.keys.pending_keys();
                if !pending.is_empty() || matches!(pressed, Key::Char(' ') | Key::Ctrl('w')) {
                    let space = pending == [Key::Char(' ')];
                    if space && pressed == Key::Char('g') {
                        self.keys.cancel(&mut self.editor);
                        self.refresh_status();
                    } else if (space && !matches!(pressed, Key::Char('w') | Key::Escape))
                        || (!pending.is_empty() && matches!(pressed, Key::Char('f' | 'F')))
                    {
                        self.keys.cancel(&mut self.editor);
                    } else {
                        if let Err(error) = self.keys.handle(&mut self.editor, pressed) {
                            self.fail(error);
                        }
                        self.apply_application_action();
                    }
                    return Some(true);
                }
                let page = usize::from(self.active_size().1.saturating_sub(1)).max(1);
                match pressed {
                    Key::Char(':') => {
                        self.prompt = Some(ActivePrompt {
                            input: Prompt::default(),
                            kind: PromptKind::Command,
                        })
                    }
                    Key::Char('q') | Key::Escape | Key::Ctrl('c' | 'q') => {
                        if self.status.views[&key].help {
                            self.status.views.get_mut(&key).unwrap().help = false;
                        } else {
                            self.close_git_status();
                        }
                    }
                    Key::Char('r') => self.refresh_status(),
                    Key::Char('s' | 'u') => {
                        if let Err(error) = self.change_git_index(pressed == Key::Char('s')) {
                            self.fail(error);
                        }
                    }
                    Key::Char('c') => {
                        self.git_write.status_prefix = Some(key.clone());
                        self.message = "c commit message · Esc cancel".into();
                    }
                    Key::Tab => {
                        if let Err(error) = self.toggle_git_section() {
                            self.fail(error);
                        }
                    }
                    Key::Enter => {
                        if let Err(error) = self.visit_git_change() {
                            self.fail(error);
                        }
                    }
                    _ => {
                        let view = self.status.views.get_mut(&key).unwrap();
                        match pressed {
                            Key::Char('j')|Key::Down=>view.list.move_by(true,1),
                            Key::Char('k')|Key::Up=>view.list.move_by(false,1),
                            Key::Ctrl('d')=>view.list.move_by(true,(page/2).max(1)),
                            Key::Ctrl('u')=>view.list.move_by(false,(page/2).max(1)),
                            Key::PageDown=>view.list.move_by(true,page),
                            Key::PageUp=>view.list.move_by(false,page),
                            Key::Home=>view.list.selected=0,
                            Key::End=>view.list.selected=view.list.rows.len().saturating_sub(1),
                            Key::Char('n')=>view.section(true),
                            Key::Char('p')=>view.section(false),
                            Key::Char('?')=>view.help = !view.help,
                            _=>self.message="Git: Tab expand · Enter visit · s/u stage/unstage · c c commit · ? help".into(),
                        }
                    }
                }
                Some(true)
            }
            Event::Paste(_) => {
                self.message =
                    "Git status cannot edit the retained document; q returns to it".into();
                Some(true)
            }
            _ => Some(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        git_status::Id,
        screen::{Frame, Style},
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::{fs, process::Command};

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fn fixture() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.name", "Vex Test"]);
        git(root, &["config", "user.email", "vex@example.invalid"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join("main.txt"), "one\ntwo\nthree\n").unwrap();
        fs::create_dir(root.join("nested")).unwrap();
        fs::write(root.join("nested/other.txt"), "other\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "initial"]);
        fs::write(root.join("main.txt"), "one\nstaged\nthree\n").unwrap();
        git(root, &["add", "."]);
        fs::write(root.join("main.txt"), "one\ndisk\nthree\n").unwrap();
        let app = App::open(Some(&root.join("main.txt")), (100, 25)).unwrap();
        (dir, app)
    }
    fn key(app: &mut App, key: KeyCode) {
        app.handle(Event::Key(KeyEvent::new(key, KeyModifiers::NONE)));
    }
    fn press(app: &mut App, keys: &str) {
        for ch in keys.chars() {
            key(app, KeyCode::Char(ch));
        }
    }
    fn finish(app: &mut App) {
        let mut job = app.take_status_batch(Instant::now()).unwrap();
        for _ in 0..4 {
            assert!(app.handle_status_result(job.run().unwrap()));
            // Drive follow-up jobs just as the event loop does after a result.
            let Some(next) = app.take_status_batch(Instant::now()) else {
                return;
            };
            job = next;
        }
        panic!("status refresh did not settle");
    }
    fn view(app: &App) -> &View {
        &app.status.views[app.active_git_view().unwrap()]
    }
    fn select(app: &mut App, predicate: impl Fn(&Id) -> bool) {
        let key = app.active_git_view().unwrap().clone();
        let view = app.status.views.get_mut(&key).unwrap();
        view.list.selected = view
            .list
            .rows
            .iter()
            .position(|row| predicate(&row.id))
            .unwrap();
    }
    fn draw(app: &mut App) -> Frame {
        let mut frame = Frame::default();
        frame.reset(app.size.0, app.size.1).unwrap();
        app.paint(&mut frame).unwrap();
        frame
    }

    #[test]
    fn status_keys_preserve_the_document_and_show_disk_index_and_unsaved_separately() {
        let (_dir, mut app) = fixture();
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("unsaved\n").unwrap();
        app.editor.execute("normal_mode", 1).unwrap();
        let snapshot = app.editor.document().snapshot();
        let selections = app.editor.selections().clone();
        let viewport = app.viewport;
        press(&mut app, " g");
        let job = app.take_status_batch(Instant::now()).unwrap();
        assert!(job.views[0].expanded.is_empty());
        app.handle_status_result(job.run().unwrap());
        app.take_status_batch(Instant::now());
        assert!(
            view(&app)
                .list
                .rows
                .iter()
                .any(|row| row.text.contains("Unsaved buffers (1)"))
        );
        assert!(
            view(&app)
                .list
                .rows
                .iter()
                .any(|row| row.text.contains("Staged changes (1)"))
        );
        key(&mut app, KeyCode::Tab);
        finish(&mut app);
        assert!(
            view(&app)
                .list
                .rows
                .iter()
                .any(|row| row.text.contains("+disk"))
        );
        assert!(
            !view(&app)
                .list
                .rows
                .iter()
                .any(|row| row.text.contains("+unsaved"))
        );
        press(&mut app, "iduU");
        app.handle(Event::Paste("accidental paste".into()));
        assert!(app.execute("delete_selection").is_err());
        assert!(app.execute("w").is_err());
        assert_eq!(app.editor.document().revision(), snapshot.revision());
        press(&mut app, "q");
        assert!(app.active_git_view().is_none());
        assert_eq!(app.editor.selections(), &selections);
        assert_eq!(app.viewport, viewport);
        assert!(app.is_dirty());
        press(&mut app, " g");
        finish(&mut app);
        assert!(
            view(&app)
                .list
                .rows
                .iter()
                .any(|row| row.text.contains("+disk"))
        );
        let frame = draw(&mut app);
        assert!(frame.row_text(app.size.1 - 2).contains("Git ·"));
        assert!(frame.row_text(0).starts_with(" Branch"));
        assert!(frame.cursor.is_none());
        for size in [(1, 1), (0, 0), (5, 3), (20, 5)] {
            app.handle(Event::Resize(size.0, size.1));
            draw(&mut app);
        }
    }

    #[test]
    fn periodic_refresh_preserves_selected_hunk_and_ignores_cancelled_results() {
        let (dir, mut app) = fixture();
        press(&mut app, " g");
        finish(&mut app);
        key(&mut app, KeyCode::Tab);
        finish(&mut app);
        select(&mut app, |id| matches!(id, Id::Hunk(..)));
        key(&mut app, KeyCode::Tab);
        let selected = view(&app).list.rows[view(&app).list.selected].id.clone();
        assert!(
            !view(&app)
                .list
                .rows
                .iter()
                .any(|row| matches!(row.id, Id::Line(..)))
        );
        let due = app.status_deadline().unwrap();
        let job = app.take_status_batch(due).unwrap();
        assert!(app.status_deadline().is_none());
        let stale = job.run().unwrap();
        app.refresh_status();
        assert!(!app.handle_status_result(stale));
        fs::write(dir.path().join("new.txt"), "new\n").unwrap();
        finish(&mut app);
        assert_eq!(view(&app).list.rows[view(&app).list.selected].id, selected);
        assert!(
            !view(&app)
                .list
                .rows
                .iter()
                .any(|row| matches!(row.id, Id::Line(..)))
        );
        assert!(
            view(&app)
                .list
                .rows
                .iter()
                .any(|row| row.text.contains("Untracked files (1)"))
        );
        key(&mut app, KeyCode::Tab);
        assert!(
            view(&app)
                .list
                .rows
                .iter()
                .any(|row| matches!(row.id, Id::Line(..)))
        );
    }

    #[test]
    fn git_and_document_panes_coexist_and_file_visits_reuse_unsaved_text() {
        let (_dir, mut app) = fixture();
        press(&mut app, " g");
        finish(&mut app);
        key(&mut app, KeyCode::Tab);
        finish(&mut app);
        select(&mut app, |id| matches!(id, Id::Line(_, _, 2)));
        let target = view(&app).destination().unwrap();
        assert_eq!(target.1, 1);
        press(&mut app, " wv");
        assert!(app.active_git_view().is_none());
        assert_eq!(app.git_view_keys().len(), 1);
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("extra\n").unwrap();
        app.editor.execute("normal_mode", 1).unwrap();
        press(&mut app, " wh");
        assert!(app.active_git_view().is_some());
        let frame = draw(&mut app);
        assert!(frame.row_text(app.size.1 - 2).contains("Git ·"));
        assert!(frame.row_text(0).contains("extra"));
        let selected_y = (view(&app).list.selected - view(&app).list.top) as u16;
        assert_eq!(frame.style_at(1, selected_y), Some(Style::Selection));
        key(&mut app, KeyCode::Enter);
        assert!(app.active_git_view().is_none());
        assert!(app.is_dirty());
        assert_eq!(
            app.editor.document().text().to_string(),
            "extra\none\ndisk\nthree\n"
        );
        assert_eq!(
            app.editor
                .document()
                .text()
                .char_to_line(app.editor.selections().primary().head.0),
            2
        );
    }

    #[test]
    fn reopening_from_a_subdirectory_reuses_repository_navigation() {
        let (dir, mut app) = fixture();
        press(&mut app, " g");
        finish(&mut app);
        key(&mut app, KeyCode::Tab);
        finish(&mut app);
        let original = app.active_git_view().unwrap().clone();
        let expanded = view(&app).expanded.clone();
        press(&mut app, "q");
        app.open_window_file(&dir.path().join("nested/other.txt"))
            .unwrap();
        press(&mut app, " g");
        finish(&mut app);
        assert_eq!(app.active_git_view(), Some(&original));
        assert_eq!(view(&app).expanded, expanded);
        if app
            .status_deadline()
            .is_some_and(|due| due <= Instant::now())
        {
            finish(&mut app);
        }
        assert!(
            view(&app)
                .list
                .rows
                .iter()
                .any(|row| row.text.contains("+disk"))
        );
    }

    #[test]
    fn non_repository_failure_and_deleted_files_leave_a_usable_view() {
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join("plain.txt");
        fs::write(&path, "hello").unwrap();
        let mut app = App::open(Some(&path), (80, 24)).unwrap();
        press(&mut app, " g");
        finish(&mut app);
        assert!(
            view(&app)
                .error
                .as_ref()
                .unwrap()
                .contains("No Git repository")
        );
        press(&mut app, "q");
        assert!(app.active_git_view().is_none());
        let (dir, mut app) = fixture();
        fs::remove_file(dir.path().join("main.txt")).unwrap();
        press(&mut app, " g");
        finish(&mut app);
        key(&mut app, KeyCode::Enter);
        assert!(app.active_git_view().is_some());
        assert!(app.message.contains("deleted"));
        assert!(!dir.path().join("main.txt").exists());
    }
}
