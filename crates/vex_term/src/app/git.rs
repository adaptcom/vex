//! Per-buffer Git state and deadlines. Complete batches share one result slot;
//! drawing only reads hunks for the current document revision and path.

use super::App;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use vex_core::{DocumentId, Revision};
use vex_editor::background::Cancellation;

const EDIT_DELAY: Duration = Duration::from_millis(150);
const MAX_DELAY: Duration = Duration::from_millis(500);
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

#[derive(PartialEq, Eq)]
struct Version {
    document: DocumentId,
    revision: Revision,
    path: PathBuf,
}

#[derive(Default)]
pub(super) struct State {
    enabled: bool,
    seen: Vec<Version>,
    due: Option<Instant>,
    first_edit: Option<Instant>,
    refresh_at: Option<Instant>,
    refresh: bool,
    request: u64,
    cancellation: Cancellation,
    results: HashMap<DocumentId, vex_git::DocumentDiff>,
}

impl Drop for State {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

impl State {
    pub(super) fn diff(
        &self,
        id: DocumentId,
        revision: Revision,
        path: Option<&Path>,
    ) -> Option<&vex_git::Diff> {
        self.results
            .get(&id)
            .filter(|result| result.revision == revision && Some(result.path.as_path()) == path)?
            .diff
            .as_deref()
    }
}

impl App {
    pub(crate) fn enable_git(&mut self) {
        self.git.enabled = true;
        self.refresh_git();
    }

    pub(super) fn refresh_git(&mut self) {
        if self.git.enabled {
            self.git.cancellation.cancel();
            self.git.refresh = true;
            self.git.due = Some(Instant::now());
        }
    }

    pub(crate) fn git_deadline(&self) -> Option<Instant> {
        if !self.git.enabled {
            return None;
        }
        self.git.due.into_iter().chain(self.git.refresh_at).min()
    }

    pub(crate) fn take_git_batch(&mut self, now: Instant) -> Option<vex_git::Batch> {
        if !self.git.enabled {
            return None;
        }
        let documents = self.git_documents();
        let versions: Vec<_> = documents
            .iter()
            .map(|doc| Version {
                document: doc.snapshot.id(),
                revision: doc.snapshot.revision(),
                path: doc.path.clone(),
            })
            .collect();
        let changed = versions.len() != self.git.seen.len()
            || versions
                .iter()
                .any(|version| !self.git.seen.contains(version));
        if changed {
            let identity = versions.len() != self.git.seen.len()
                || versions.iter().any(|version| {
                    !self
                        .git
                        .seen
                        .iter()
                        .any(|old| old.document == version.document && old.path == version.path)
                });
            self.git.cancellation.cancel();
            let first = *self.git.first_edit.get_or_insert(now);
            self.git.due = Some(if identity {
                now
            } else {
                (now + EDIT_DELAY).min(first + MAX_DELAY)
            });
            self.git
                .results
                .retain(|id, _| versions.iter().any(|version| version.document == *id));
            self.git.seen = versions;
        }
        if self.git.refresh_at.is_some_and(|deadline| deadline <= now) {
            self.git.refresh = true;
            self.git.due.get_or_insert(now);
            self.git.refresh_at = Some(now + REFRESH_INTERVAL);
        }
        if self.git.due.is_none_or(|deadline| deadline > now) {
            return None;
        }
        self.git.cancellation.cancel();
        self.git.cancellation = Cancellation::default();
        self.git.request += 1;
        self.git.due = None;
        self.git.first_edit = None;
        let refresh = self.git.refresh;
        self.git.refresh_at = None;
        Some(vex_git::Batch {
            request: self.git.request,
            documents,
            refresh,
            cancellation: self.git.cancellation.clone(),
        })
    }

    pub(crate) fn handle_git_result(&mut self, result: vex_git::Result) -> bool {
        if result.request != self.git.request || self.git.cancellation.is_cancelled() {
            return false;
        }
        self.git.refresh = false;
        self.git.refresh_at = Some(Instant::now() + REFRESH_INTERVAL);
        let current = self.git_documents();
        let mut changed = false;
        for result in result.documents {
            if !current.iter().any(|doc| {
                doc.snapshot.id() == result.document
                    && doc.snapshot.revision() == result.revision
                    && doc.path == result.path
            }) {
                continue;
            }
            changed |= self.git.results.get(&result.document).is_none_or(|old| {
                old.revision != result.revision
                    || old.path != result.path
                    || old.baseline != result.baseline
                    || old.diff != result.diff
            });
            self.git.results.insert(result.document, result);
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        render,
        screen::{Frame, Style},
    };
    use crossterm::event::Event;
    use std::{fs, process::Command};
    use vex_core::{CharOffset, Document};
    use vex_git::{Marker, Worker};

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
    fn fixture() -> (tempfile::TempDir, App, Worker) {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["config", "user.name", "Vex Test"]);
        git(dir.path(), &["config", "user.email", "vex@example.invalid"]);
        git(dir.path(), &["config", "commit.gpgsign", "false"]);
        fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
        fs::write(dir.path().join("other.rs"), "fn other() {}\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-qm", "initial"]);
        let mut app = App::open(Some(&dir.path().join("main.rs")), (121, 20)).unwrap();
        app.enable_git();
        let mut worker = Worker::default();
        finish(&mut app, &mut worker, Instant::now());
        (dir, app, worker)
    }
    fn finish(app: &mut App, worker: &mut Worker, now: Instant) {
        let job = app.take_git_batch(now).unwrap();
        let result = worker.run(job).unwrap();
        app.handle_git_result(result);
    }
    fn diff(app: &App) -> &vex_git::Diff {
        app.git
            .diff(
                app.editor.document().id(),
                app.editor.document().revision(),
                app.files.target(),
            )
            .unwrap()
    }
    fn draw(app: &mut App) -> Frame {
        let mut frame = Frame::default();
        let (width, height) = app.size();
        frame.reset(width, height).unwrap();
        app.paint(&mut frame).unwrap();
        frame
    }

    #[test]
    fn live_markers_share_panes_and_coexist_with_diagnostics_numbers_and_cursors() {
        let (_dir, mut app, mut worker) = fixture();
        assert!(diff(&app).hunks.is_empty());
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("// added\n").unwrap();
        app.editor.execute("normal_mode", 1).unwrap();
        let now = Instant::now();
        assert!(app.take_git_batch(now).is_none());
        assert_eq!(app.git_deadline(), Some(now + EDIT_DELAY));
        assert!(
            app.git
                .diff(
                    app.editor.document().id(),
                    app.editor.document().revision(),
                    app.files.target()
                )
                .is_none()
        );
        finish(&mut app, &mut worker, now + EDIT_DELAY);
        assert_eq!(diff(&app).marker(0), Some(Marker::Added));
        app.enable_lsp();
        let update = app.take_lsp_update().unwrap().document.unwrap();
        assert!(app.handle_lsp_event(vex_lsp::Event::Diagnostics {
            epoch: update.epoch,
            revision: app.editor.document().revision(),
            diagnostics: vec![vex_lsp::Diagnostic {
                start: CharOffset(0),
                end: CharOffset(2),
                line: 0,
                severity: 1,
                message: "example".into()
            }]
        }));
        let frame = draw(&mut app);
        assert!(frame.row_text(0).starts_with("!1 ▍ // added"));
        assert_eq!(frame.style_at(0, 0), Some(Style::Error));
        assert_eq!(frame.style_at(3, 0), Some(Style::GitAdded));
        assert!(frame.cursor.unwrap().x >= 5);
        app.execute("vsplit").unwrap();
        assert_eq!(app.git_documents().len(), 1);
        let frame = draw(&mut app);
        assert_eq!(frame.style_at(3, 0), Some(Style::GitAdded));
        assert_eq!(frame.style_at(64, 0), Some(Style::GitAdded));
        app.handle(Event::Resize(9, 4));
        let frame = draw(&mut app);
        assert!(frame.cursor.unwrap().x < 9);
        assert!(render::gutter(9, 3).diff.is_none());
    }

    #[test]
    fn cancelled_batches_reissue_other_buffers_and_cannot_replace_newer_revisions() {
        let (dir, mut app, mut worker) = fixture();
        app.execute(&format!("vsplit {}", dir.path().join("other.rs").display()))
            .unwrap();
        let old = app.take_git_batch(Instant::now()).unwrap();
        assert_eq!(old.documents.len(), 2);
        let old_result = worker.run(old).unwrap();
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("// change\n").unwrap();
        let now = Instant::now();
        assert!(app.take_git_batch(now).is_none());
        assert!(!app.handle_git_result(old_result));
        let next = app.take_git_batch(now + EDIT_DELAY).unwrap();
        assert_eq!(next.documents.len(), 2);
        assert!(app.handle_git_result(worker.run(next).unwrap()));
        assert_eq!(app.git.results.len(), 2);
        let old = app
            .git
            .results
            .get(&app.editor.document().id())
            .unwrap()
            .baseline
            .clone();
        app.execute("w").unwrap();
        let result = worker
            .run(app.take_git_batch(Instant::now()).unwrap())
            .unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-qm", "commit while results wait"]);
        app.handle(Event::FocusGained);
        assert!(!app.handle_git_result(result));
        finish(&mut app, &mut worker, Instant::now());
        assert_ne!(app.git.results[&app.editor.document().id()].baseline, old);
        assert!(diff(&app).hunks.is_empty());
    }

    #[test]
    fn saves_keep_head_markers_and_periodic_refresh_observes_commits_without_input() {
        let (dir, mut app, mut worker) = fixture();
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("// new\n").unwrap();
        let now = Instant::now();
        app.take_git_batch(now);
        finish(&mut app, &mut worker, now + EDIT_DELAY);
        app.execute("w").unwrap();
        finish(&mut app, &mut worker, Instant::now());
        assert!(!app.is_dirty());
        assert_eq!(diff(&app).marker(0), Some(Marker::Added));
        git(dir.path(), &["add", "."]);
        let due = app.git.refresh_at.unwrap();
        finish(&mut app, &mut worker, due);
        assert_eq!(diff(&app).marker(0), Some(Marker::Added));
        git(dir.path(), &["commit", "-qm", "save"]);
        let due = app.git.refresh_at.unwrap();
        let batch = app.take_git_batch(due).unwrap();
        assert!(batch.refresh);
        assert!(app.git_deadline().is_none()); // Do not interrupt a slow batch with another periodic poll.
        app.handle_git_result(worker.run(batch).unwrap());
        assert!(diff(&app).hunks.is_empty());
    }

    #[test]
    fn refresh_survives_cancelled_initial_load_and_save_as_invalidates_old_identity() {
        let (_dir, mut app, mut worker) = fixture();
        app.refresh_git();
        let old = app.take_git_batch(Instant::now()).unwrap();
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("x").unwrap();
        let now = Instant::now();
        assert!(app.take_git_batch(now).is_none());
        assert!(old.cancellation.is_cancelled());
        let job = app.take_git_batch(now + EDIT_DELAY).unwrap();
        assert!(job.refresh);
        let stale = worker.run(job).unwrap();
        let target = tempfile::tempdir().unwrap();
        app.execute(&format!("w {}", target.path().join("saved.txt").display()))
            .unwrap();
        assert!(!app.handle_git_result(stale));
        finish(&mut app, &mut worker, Instant::now());
        assert!(
            app.git
                .diff(
                    app.editor.document().id(),
                    app.editor.document().revision(),
                    app.files.target()
                )
                .is_none()
        );
        let mut scratch = App::from_document(Document::from("hello"), (80, 24));
        scratch.enable_git();
        assert!(
            scratch
                .take_git_batch(Instant::now())
                .unwrap()
                .documents
                .is_empty()
        );
    }

    #[test]
    fn deletion_overlines_render_at_start_middle_end_and_in_empty_buffers() {
        let (dir, _, _) = fixture();
        let path = dir.path().join("main.rs");
        fs::write(&path, "a\nb\nc\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-qm", "three lines"]);
        for (text, line, glyph, style) in [
            ("b\nc\n", 0, '▔', Style::GitDeleted),
            ("a\nc\n", 1, '▔', Style::GitDeleted),
            ("a\nb\n", 2, '▔', Style::GitDeleted),
            ("", 0, '▔', Style::GitDeleted),
            ("a\nB\nc\n", 1, '▍', Style::GitModified),
        ] {
            fs::write(&path, text).unwrap();
            let mut app = App::open(Some(&path), (80, 10)).unwrap();
            app.enable_git();
            finish(&mut app, &mut Worker::default(), Instant::now());
            let frame = draw(&mut app);
            assert_eq!(frame.row_text(line).chars().nth(3), Some(glyph));
            assert_eq!(frame.style_at(3, line), Some(style));
        }
    }
}
