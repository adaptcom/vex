//! Git write commands and session-local commit drafts. Jobs own their inputs;
//! pane changes never cancel a queued mutation or lose its completion.

use super::App;
use crate::{git_status::Id, input};
use crossterm::event::{Event, KeyEventKind};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    io,
    path::{Path, PathBuf},
};
use vex_core::DocumentId;
use vex_editor::Key;
use vex_git::{
    status::Group,
    write::{Job, Operation, Result as WriteResult},
};

pub(super) struct Draft {
    pub root: PathBuf,
    pub status_key: PathBuf,
    pub committed: Option<vex_core::Revision>,
}

#[derive(Default)]
pub(super) struct State {
    next: u64,
    pending: BTreeMap<PathBuf, u64>,
    jobs: VecDeque<Job>,
    pub drafts: HashMap<DocumentId, Draft>,
    pub prefix: Option<DocumentId>,
    pub status_prefix: Option<PathBuf>,
}

impl App {
    pub(super) fn commit_title(&self, document: DocumentId) -> Option<String> {
        self.git_write.drafts.get(&document).map(|draft| {
            format!(
                "Git commit · {}",
                draft.root.file_name().unwrap_or_default().to_string_lossy()
            )
        })
    }

    pub(super) fn check_git_writes_finished(&self) -> io::Result<()> {
        if self.git_write.pending.is_empty() {
            Ok(())
        } else {
            Err(io::Error::other(
                "Git operation still running; wait for it to finish before quitting",
            ))
        }
    }

    fn queue_git_write(&mut self, root: PathBuf, operation: Operation) -> io::Result<()> {
        if self.git_write.pending.contains_key(&root) {
            return Err(io::Error::other(
                "a Git operation is already running for this repository",
            ));
        }
        if self.git_write.pending.len() >= 16 {
            return Err(io::Error::other("too many pending Git operations"));
        }
        self.git_write.next += 1;
        let id = self.git_write.next;
        let label = format!("{} running…", operation.name());
        let keep_position = matches!(operation, Operation::Stage(_) | Operation::Unstage(_));
        self.git_write.pending.insert(root.clone(), id);
        self.git_write.jobs.push_back(Job {
            id,
            root: root.clone(),
            operation,
        });
        for view in self.status.views.values_mut() {
            if view
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.root == root)
            {
                view.keep_position = keep_position;
                view.operation = Some(label.clone());
                view.output = None;
                view.rebuild();
            }
        }
        self.message = label;
        Ok(())
    }

    pub(crate) fn take_git_write(&mut self) -> Option<Job> {
        self.git_write.jobs.pop_front()
    }

    /// Stage or unstage exactly the selected whole file. Hunk and line rows
    /// deliberately require selecting their file until partial staging exists.
    pub(super) fn change_git_index(&mut self, stage: bool) -> io::Result<()> {
        let key = self
            .active_git_view()
            .ok_or_else(|| io::Error::other("open Git status first"))?;
        let view = &self.status.views[key];
        let Some(Id::File(file)) = view.list.rows.get(view.list.selected).map(|row| &row.id) else {
            return Err(io::Error::other(
                "select a file row; hunk staging is not available yet",
            ));
        };
        if file.group == Group::Unsaved {
            return Err(io::Error::other(
                "save the buffer, then stage its file under unstaged changes",
            ));
        }
        if (stage && !matches!(file.group, Group::Unstaged | Group::Untracked))
            || (!stage && file.group != Group::Staged)
        {
            return Err(io::Error::other(
                "s stages unstaged/untracked files; u unstages staged files",
            ));
        }
        let snapshot = view
            .snapshot
            .as_ref()
            .ok_or_else(|| io::Error::other("wait for repository status"))?;
        let entry = snapshot
            .files
            .iter()
            .find(|entry| entry.key == *file)
            .unwrap()
            .clone();
        self.queue_git_write(
            snapshot.root.clone(),
            if stage {
                Operation::Stage(entry)
            } else {
                Operation::Unstage(entry)
            },
        )
    }

    /// Open a normal editor buffer for this repository's commit message.
    pub(super) fn begin_git_commit(&mut self) -> io::Result<()> {
        let key = self
            .active_git_view()
            .cloned()
            .ok_or_else(|| io::Error::other("open Git status first"))?;
        let root = self.status.views[&key]
            .snapshot
            .as_ref()
            .ok_or_else(|| io::Error::other("wait for repository status"))?
            .root
            .clone();
        self.open_commit_draft(root, key)?;
        self.message =
            "Commit message · Ctrl-c Ctrl-c commit · Ctrl-c Ctrl-k return and keep draft".into();
        Ok(())
    }

    /// Commit the current index with the draft's text. No files are auto-staged.
    pub(super) fn submit_git_commit(&mut self) -> io::Result<()> {
        if self.active_git_view().is_some() {
            return Err(io::Error::other("open the commit message first with c c"));
        }
        let id = self.editor.document().id();
        let root = self
            .git_write
            .drafts
            .get(&id)
            .ok_or_else(|| io::Error::other("open a commit message first"))?
            .root
            .clone();
        if self.editor.document().text().len_bytes() > 1 << 20 {
            return Err(io::Error::other("commit message exceeds 1 MiB"));
        }
        let message = self.editor.document().text().to_string();
        if message.trim().is_empty() {
            return Err(io::Error::other("commit message is empty"));
        }
        self.editor.finish_undo_group();
        self.keys.cancel(&mut self.editor);
        self.queue_git_write(root, Operation::Commit { message })
    }

    /// Leave the composer, retaining the message and undo history this session.
    pub(super) fn cancel_git_commit(&mut self) -> io::Result<()> {
        let id = self.editor.document().id();
        let key = self
            .git_write
            .drafts
            .get(&id)
            .ok_or_else(|| io::Error::other("open a commit message first"))?
            .status_key
            .clone();
        self.leave_commit_draft(&key)?;
        self.keys.cancel(&mut self.editor);
        self.git_write.prefix = None;
        self.message = "Commit draft retained for this session".into();
        Ok(())
    }

    pub(super) fn handle_commit_input(&mut self, event: &Event) -> Option<bool> {
        let id = self.editor.document().id();
        if self.active_git_view().is_some()
            || self.prompt.is_some()
            || !self.keys.pending_keys().is_empty()
            || self.keys.count().is_some()
            || !self.git_write.drafts.contains_key(&id)
        {
            self.git_write.prefix = None;
            return None;
        }
        let Event::Key(event) = event else {
            return None;
        };
        if event.kind == KeyEventKind::Release {
            return Some(false);
        }
        let key = input::key(*event)?;
        if self.git_write.prefix.take() == Some(id) {
            self.clear_message();
            let result = match key {
                Key::Ctrl('c') => self.submit_git_commit(),
                Key::Ctrl('k') => self.cancel_git_commit(),
                _ => Ok(()),
            };
            if let Err(error) = result {
                self.fail(error);
            }
            return Some(true);
        }
        if key == Key::Ctrl('c') {
            self.keys.cancel(&mut self.editor);
            self.git_write.prefix = Some(id);
            self.message =
                "Ctrl-c commit · Ctrl-k return and keep draft · Esc cancel prefix".into();
            return Some(true);
        }
        None
    }

    pub(crate) fn handle_git_write(&mut self, result: WriteResult) -> bool {
        if self.git_write.pending.get(&result.root) != Some(&result.id) {
            return false;
        }
        self.git_write.pending.remove(&result.root);
        let success = result.outcome.is_ok();
        let title = format!(
            "{} {}",
            result.operation.name(),
            if success { "complete" } else { "failed" }
        );
        let output = match &result.outcome {
            Ok(text) | Err(text) => text.clone(),
        };
        for view in self.status.views.values_mut() {
            if view
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.root == result.root)
            {
                view.operation = None;
                view.output = Some((format!("{title}\n{output}"), !success));
                view.rebuild();
                if !success {
                    view.keep_position = false;
                }
            }
        }
        if success && let Operation::Commit { message } = &result.operation {
            let draft = self
                .git_write
                .drafts
                .iter()
                .find(|(_, draft)| draft.root == result.root)
                .map(|(id, _)| *id);
            if let Some(id) = draft
                && let Some(revision) = self.commit_draft_revision(id, message)
            {
                self.git_write.drafts.get_mut(&id).unwrap().committed = Some(revision);
                if self.editor.document().id() == id && self.active_git_view().is_none() {
                    let _ = self.cancel_git_commit();
                }
            }
        }
        self.refresh_git();
        self.refresh_status();
        self.message = if success {
            title
        } else {
            format!(
                "{title}: {} · output in Git status",
                output
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or("Git failed")
            )
        };
        self.error = !success;
        true
    }

    pub(super) fn git_operation_for(&self, root: &Path) -> bool {
        self.git_write.pending.contains_key(root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{draw, key, press};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::{fs, process::Command, time::Instant};

    fn git(root: &Path, args: &[&str]) -> String {
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
        String::from_utf8(output.stdout).unwrap().trim_end().into()
    }
    fn fixture() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.name", "Vex Test"]);
        git(root, &["config", "user.email", "vex@example.invalid"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        git(
            root,
            &[
                "config",
                "core.hooksPath",
                root.join(".git/hooks").to_str().unwrap(),
            ],
        );
        fs::write(root.join("file.txt"), "base\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "base"]);
        fs::write(root.join("file.txt"), "disk\n").unwrap();
        let mut app = App::open(Some(&root.join("file.txt")), (100, 25)).unwrap();
        press(&mut app, " g");
        refresh(&mut app);
        (dir, app)
    }
    fn refresh(app: &mut App) {
        app.refresh_status();
        for _ in 0..3 {
            let Some(job) = app.take_status_batch(Instant::now()) else {
                return;
            };
            assert!(app.handle_status_result(job.run().unwrap()));
        }
        panic!("refresh did not settle");
    }
    fn complete(app: &mut App) {
        let job = app.take_git_write().expect("queued operation");
        assert!(app.handle_git_write(job.run()));
    }
    fn ctrl(app: &mut App, ch: char) {
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char(ch),
            KeyModifiers::CONTROL,
        )));
    }

    #[test]
    fn cancelling_a_find_in_a_commit_draft_does_not_start_the_commit_prefix() {
        let (_dir, mut app) = fixture();
        press(&mut app, "ccsubject");
        key(&mut app, KeyCode::Esc);
        press(&mut app, "v2f");
        ctrl(&mut app, 'c');
        assert!(app.git_write.prefix.is_none());
        assert_eq!(app.editor.mode(), vex_editor::Mode::Select);
        assert!(app.keys.pending_keys().is_empty());
        ctrl(&mut app, 'c');
        assert_eq!(app.git_write.prefix, Some(app.editor.document().id()));
    }
    fn select_file(app: &mut App, group: Group) {
        let key = app.active_git_view().unwrap().clone();
        let view = app.status.views.get_mut(&key).unwrap();
        view.list.selected = view
            .list
            .rows
            .iter()
            .position(|row| matches!(&row.id, Id::File(file) if file.group == group))
            .unwrap();
    }

    #[test]
    fn staging_survives_leaving_status_and_refreshes_without_saving_unsaved_text() {
        let (dir, mut app) = fixture();
        press(&mut app, "qichanged ");
        key(&mut app, KeyCode::Esc);
        let before = app.editor.document().snapshot();
        press(&mut app, " g");
        refresh(&mut app);
        app.execute("git_stage").unwrap();
        assert!(app.execute("git_stage").is_err());
        assert!(app.execute("qa!").is_err());
        press(&mut app, "q");
        complete(&mut app);
        assert_eq!(app.editor.document().revision(), before.revision());
        assert_eq!(git(dir.path(), &["show", ":file.txt"]), "disk");
        assert_eq!(
            fs::read_to_string(dir.path().join("file.txt")).unwrap(),
            "disk\n"
        );
        press(&mut app, " g");
        refresh(&mut app);
        select_file(&mut app, Group::Staged);
        press(&mut app, "u");
        complete(&mut app);
        assert!(git(dir.path(), &["diff", "--cached", "--name-only"]).is_empty());
        refresh(&mut app);
        select_file(&mut app, Group::Unsaved);
        assert!(app.execute("git_stage").is_err());
        assert!(app.take_git_write().is_none());
    }

    #[test]
    fn index_writes_keep_the_current_row_and_scroll_through_output_and_refresh() {
        for stage in [true, false] {
            for refresh_before_completion in [false, true] {
                let (dir, mut app) = fixture();
                let root = dir.path();
                for i in 0..24 {
                    fs::write(root.join(format!("file-{i:02}.txt")), "base\n").unwrap();
                }
                git(root, &["add", "."]);
                git(root, &["commit", "-qm", "more files"]);
                for i in 0..24 {
                    fs::write(root.join(format!("file-{i:02}.txt")), "changed\n").unwrap();
                }
                if !stage {
                    git(root, &["add", "."]);
                }
                refresh(&mut app);
                app.handle(Event::Resize(80, 8));
                let key = app.active_git_view().unwrap().clone();
                let view = app.status.views.get_mut(&key).unwrap();
                view.output = Some(("Previous operation\nPrevious output".into(), false));
                view.rebuild();
                view.list.selected = view
                    .list
                    .rows
                    .iter()
                    .position(|row| {
                        matches!(&row.id, Id::File(file) if file.path == Path::new("file-12.txt"))
                    })
                    .unwrap();
                view.list.top = view.list.selected - 2;
                let position = (view.list.selected, view.list.top);
                let assert_position = |app: &mut App, expected| {
                    let frame = draw(app);
                    let view = &app.status.views[&key];
                    assert_eq!((view.list.selected, view.list.top), expected);
                    assert_eq!(
                        frame.style_at(0, (expected.0 - expected.1) as u16),
                        Some(crate::screen::Style::Selection)
                    );
                };
                press(&mut app, if stage { "s" } else { "u" });
                assert_position(&mut app, position); // Removing the previous output.
                let result = app.take_git_write().unwrap().run();
                assert!(result.outcome.is_ok(), "{:?}", result.outcome);
                if refresh_before_completion {
                    // A periodic query may observe the write before its event arrives.
                    refresh(&mut app);
                    assert_position(&mut app, position);
                    assert!(app.status.views[&key].keep_position);
                }
                press(&mut app, "j"); // Navigation while the worker runs must survive.
                let position = (position.0 + 1, position.1);
                assert!(app.handle_git_write(result));
                assert_position(&mut app, position); // Adding the new output.
                if !refresh_before_completion {
                    app.status
                        .views
                        .get_mut(&key)
                        .unwrap()
                        .update(Err("temporary query failure".into()));
                    assert_position(&mut app, position);
                    assert!(app.status.views[&key].keep_position);
                }
                refresh(&mut app);
                assert_position(&mut app, position); // Moving files between sections.
                assert!(!app.status.views[&key].keep_position);

                // Later refreshes resume preserving the selected row's identity.
                select_file(
                    &mut app,
                    if stage {
                        Group::Unstaged
                    } else {
                        Group::Staged
                    },
                );
                let view = &app.status.views[&key];
                let selected = view.list.rows[view.list.selected].id.clone();
                let index = view.list.selected;
                fs::write(root.join("aaa.txt"), "new\n").unwrap();
                git(root, &["add", "aaa.txt"]);
                if stage {
                    fs::write(root.join("aaa.txt"), "changed\n").unwrap();
                }
                refresh(&mut app);
                let view = &app.status.views[&key];
                assert_eq!(view.list.rows[view.list.selected].id, selected);
                assert!(view.list.selected > index);
            }
        }
    }

    #[test]
    fn index_writes_clamp_when_an_expanded_file_disappears() {
        let (_dir, mut app) = fixture();
        key(&mut app, KeyCode::Tab);
        refresh(&mut app);
        let view_key = app.active_git_view().unwrap().clone();
        let view = app.status.views.get_mut(&view_key).unwrap();
        let file = match &view.list.rows[view.list.selected].id {
            Id::File(file) => file.clone(),
            _ => panic!("expected a file"),
        };
        press(&mut app, "s");
        // Move into the disappearing preview while staging runs.
        key(&mut app, KeyCode::End);
        let view = app.status.views.get_mut(&view_key).unwrap();
        view.list.top = view.list.selected;
        complete(&mut app);
        refresh(&mut app);
        let view = &app.status.views[&view_key];
        assert!(!view.expanded.contains(&file));
        assert_eq!(view.list.selected, view.list.rows.len() - 1);
        assert!(view.list.top <= view.list.selected);
        assert!(!view.keep_position);
        draw(&mut app);
    }

    #[test]
    fn failed_index_write_releases_position_preservation() {
        let (_dir, mut app) = fixture();
        let key = app.active_git_view().unwrap().clone();
        let view = &app.status.views[&key];
        let position = (view.list.selected, view.list.top);
        press(&mut app, "s");
        let job = app.take_git_write().unwrap();
        assert!(app.handle_git_write(WriteResult {
            id: job.id,
            root: job.root,
            operation: job.operation,
            outcome: Err("fixture write failure".into()),
        }));
        let view = &app.status.views[&key];
        assert_eq!((view.list.selected, view.list.top), position);
        assert!(!view.keep_position);
        assert!(app.error);
        refresh(&mut app);
        assert!(!app.status.views[&key].keep_position);
    }

    #[test]
    fn composer_retains_text_and_undo_on_cancel_then_commits_and_returns_to_status() {
        let (dir, mut app) = fixture();
        press(&mut app, "s");
        complete(&mut app);
        refresh(&mut app);
        let original = app.editor.document().id();
        app.handle(Event::Resize(80, 25));
        press(&mut app, "ccSubject");
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Enter);
        press(&mut app, "Body");
        key(&mut app, KeyCode::Esc);
        let message = app.editor.document().text().to_string();
        let draft = app.editor.document().id();
        assert_ne!(draft, original);
        assert!(draw(&mut app).row_text(23).contains("Git commit"));
        ctrl(&mut app, 'c');
        ctrl(&mut app, 'k');
        assert!(app.active_git_view().is_some());
        assert_eq!(app.editor.document().id(), original);
        press(&mut app, "cc");
        assert_eq!(app.editor.document().id(), draft);
        assert_eq!(app.editor.document().text(), message.as_str());
        press(&mut app, "uU");
        assert_eq!(app.editor.document().text(), message.as_str());
        ctrl(&mut app, 'c');
        ctrl(&mut app, 'c');
        assert!(app.execute("git_commit_submit").is_err());
        complete(&mut app);
        assert!(!app.error, "{}", app.message);
        assert!(app.active_git_view().is_some());
        assert_eq!(
            git(dir.path(), &["log", "-1", "--format=%B"]),
            message.trim_end()
        );
        assert_eq!(git(dir.path(), &["show", "HEAD:file.txt"]), "disk");
        press(&mut app, "cc");
        assert_eq!(app.editor.document().text(), "");
        assert!(app.execute("git_commit_submit").is_err());
    }

    #[test]
    fn failed_commit_and_edits_while_running_retain_the_draft() {
        let (dir, mut app) = fixture();
        press(&mut app, "ccKeep draft");
        ctrl(&mut app, 'c');
        ctrl(&mut app, 'c');
        complete(&mut app); // No staged changes.
        assert!(app.error);
        assert_eq!(app.editor.document().text(), "Keep draft");
        let draft = app.editor.document().id();
        ctrl(&mut app, 'c');
        ctrl(&mut app, 'k');
        refresh(&mut app);
        assert!(
            app.status.views[app.active_git_view().unwrap()]
                .output
                .as_ref()
                .unwrap()
                .1
        );
        select_file(&mut app, Group::Unstaged);
        press(&mut app, "s");
        complete(&mut app);
        press(&mut app, "cc");
        app.execute("git_commit_submit").unwrap();
        let job = app.take_git_write().unwrap();
        press(&mut app, " plus newer edits");
        app.handle_git_write(job.run());
        assert_eq!(app.editor.document().id(), draft);
        assert!(app.active_git_view().is_none());
        assert_eq!(app.editor.document().text(), "Keep draft plus newer edits");
        assert_eq!(git(dir.path(), &["log", "-1", "--format=%B"]), "Keep draft");
        ctrl(&mut app, 'c');
        ctrl(&mut app, 'k');
        press(&mut app, "cc");
        assert_eq!(app.editor.document().text(), "Keep draft plus newer edits");
    }

    #[test]
    fn a_commit_finishing_after_the_composer_closes_resets_the_next_message() {
        let (_dir, mut app) = fixture();
        press(&mut app, "s");
        complete(&mut app);
        refresh(&mut app);
        let original = app.editor.document().id();
        press(&mut app, "ccsubject");
        ctrl(&mut app, 'c');
        ctrl(&mut app, 'c');
        let job = app.take_git_write().unwrap();
        ctrl(&mut app, 'c');
        ctrl(&mut app, 'k');
        assert_eq!(app.editor.document().id(), original);
        app.handle_git_write(job.run());
        assert_eq!(app.editor.document().id(), original);
        assert!(app.active_git_view().is_some());
        press(&mut app, "cc");
        assert_eq!(app.editor.document().text(), "");
        assert_eq!(app.editor.mode(), vex_editor::Mode::Insert);
    }

    #[test]
    fn drafts_survive_window_commands_and_tiny_terminals() {
        let (_dir, mut app) = fixture();
        press(&mut app, "ccmessage");
        key(&mut app, KeyCode::Esc);
        let id = app.editor.document().id();
        app.execute("only").unwrap();
        app.execute("git_commit_cancel").unwrap();
        assert!(app.active_git_view().is_some());
        press(&mut app, "cc");
        assert_eq!(app.editor.document().id(), id);
        assert_eq!(app.editor.document().text(), "message");
        app.execute("q").unwrap();
        press(&mut app, "cc");
        assert_eq!(app.editor.document().text(), "message");
        app.execute("git_commit_cancel").unwrap();
        app.handle(Event::Resize(10, 3));
        let before = app.editor.document().id();
        assert!(app.execute("git_commit").is_err());
        assert_eq!(app.editor.document().id(), before);
        draw(&mut app);
    }
}
