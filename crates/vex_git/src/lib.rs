//! Git baselines and line hunks for immutable editor snapshots. One persistent
//! worker caches HEAD blobs and returns complete batches for all visible buffers.

mod diff;
mod repository;
pub mod status;
pub mod write;

use diff::BaseLines;
pub use diff::{Diff, Hunk, Marker};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use vex_core::{DocumentId, Revision, Rope, Snapshot};
use vex_editor::background::Cancellation;

pub const MAX_BYTES: usize = 8 << 20;

pub struct Document {
    pub path: PathBuf,
    pub snapshot: Snapshot,
}

pub struct Batch {
    pub request: u64,
    pub documents: Vec<Document>,
    pub refresh: bool,
    pub cancellation: Cancellation,
}

pub struct Result {
    pub request: u64,
    pub documents: Vec<DocumentDiff>,
}

pub struct DocumentDiff {
    pub document: DocumentId,
    pub revision: Revision,
    pub path: PathBuf,
    /// Identifies the exact committed blob used to compute these hunks.
    pub baseline: Option<String>,
    pub diff: Option<Arc<Diff>>,
}

struct Cached {
    initialized: bool,
    path: PathBuf,
    root: Option<PathBuf>,
    head: Option<String>,
    blob: Option<String>,
    base: Option<BaseLines>,
    revision: Option<Revision>,
    diff: Option<Arc<Diff>>,
}

#[derive(Default)]
pub struct Worker {
    buffers: HashMap<DocumentId, Cached>,
}

impl Worker {
    pub fn run(&mut self, batch: Batch) -> Option<Result> {
        self.buffers
            .retain(|id, _| batch.documents.iter().any(|doc| doc.snapshot.id() == *id));
        let mut heads: HashMap<PathBuf, Option<String>> = HashMap::new();
        let mut results = Vec::new();
        for document in batch.documents {
            if batch.cancellation.is_cancelled() {
                return None;
            }
            let id = document.snapshot.id();
            let changed = self
                .buffers
                .get(&id)
                .is_none_or(|old| old.path != document.path);
            if changed {
                self.buffers.insert(
                    id,
                    Cached {
                        initialized: false,
                        path: document.path.clone(),
                        root: None,
                        head: None,
                        blob: None,
                        base: None,
                        revision: None,
                        diff: None,
                    },
                );
            }
            let cached = self.buffers.get_mut(&id).unwrap();
            if changed || batch.refresh || !cached.initialized {
                if cached.root.is_none() {
                    cached.root = repository::discover(&document.path, &batch.cancellation);
                }
                let head = cached.root.as_ref().and_then(|root| {
                    heads
                        .entry(root.clone())
                        .or_insert_with(|| repository::head(root, &batch.cancellation))
                        .clone()
                });
                if changed || head != cached.head || cached.base.is_none() {
                    let blob =
                        cached
                            .root
                            .as_ref()
                            .zip(head.as_deref())
                            .and_then(|(root, head)| {
                                repository::blob(root, head, &document.path, &batch.cancellation)
                            });
                    if blob != cached.blob || cached.base.is_none() {
                        let base = cached
                            .root
                            .as_ref()
                            .zip(blob.as_deref())
                            .and_then(|(root, blob)| {
                                repository::contents(root, blob, &batch.cancellation)
                            })
                            .and_then(|text| {
                                BaseLines::new(&Rope::from_str(&text), &batch.cancellation)
                            });
                        if batch.cancellation.is_cancelled() {
                            return None;
                        }
                        cached.base = base;
                        cached.blob = blob;
                        cached.revision = None;
                        cached.diff = None;
                    }
                    if batch.cancellation.is_cancelled() {
                        return None;
                    }
                    cached.head = head;
                }
                if batch.cancellation.is_cancelled() {
                    return None;
                }
                cached.initialized = true;
            }
            if cached.revision != Some(document.snapshot.revision()) {
                let text = document.snapshot.text();
                cached.diff = if text.len_bytes() <= MAX_BYTES
                    && !text.chunks().any(|chunk| chunk.contains('\0'))
                {
                    cached
                        .base
                        .as_ref()
                        .and_then(|base| base.diff(text, &batch.cancellation))
                        .map(Arc::new)
                } else {
                    None
                };
                if batch.cancellation.is_cancelled() {
                    return None;
                }
                cached.revision = Some(document.snapshot.revision());
            }
            results.push(DocumentDiff {
                document: id,
                revision: document.snapshot.revision(),
                path: document.path,
                baseline: cached.blob.clone(),
                diff: cached.diff.clone(),
            });
        }
        (!batch.cancellation.is_cancelled()).then_some(Result {
            request: batch.request,
            documents: results,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::Path, process::Command};
    use vex_core::{CharOffset, Document as TextDocument, Edit, Selection, SelectionSet};

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
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["config", "user.name", "Vex Test"]);
        git(dir.path(), &["config", "user.email", "vex@example.invalid"]);
        git(dir.path(), &["config", "commit.gpgsign", "false"]);
        dir
    }
    fn run(worker: &mut Worker, path: &Path, doc: &TextDocument, refresh: bool) -> DocumentDiff {
        worker
            .run(Batch {
                request: 1,
                documents: vec![Document {
                    path: path.canonicalize().unwrap(),
                    snapshot: doc.snapshot(),
                }],
                refresh,
                cancellation: Cancellation::default(),
            })
            .unwrap()
            .documents
            .remove(0)
    }

    #[test]
    fn head_baseline_includes_staged_unstaged_and_unsaved_changes_and_refreshes_on_commit() {
        let dir = repo();
        let path = dir.path().join("- space [界].txt");
        fs::write(&path, "a\nb\nc\n").unwrap();
        git(dir.path(), &["add", "--", "- space [界].txt"]);
        git(dir.path(), &["commit", "-qm", "initial"]);
        fs::write(&path, "a\nB\nc\n").unwrap();
        git(dir.path(), &["add", "--", "- space [界].txt"]);
        fs::write(&path, "a\nB\nC\n").unwrap();
        let mut doc = TextDocument::from("a\nB\nC\nunsaved\n");
        let mut worker = Worker::default();
        let first = run(&mut worker, &path, &doc, true);
        assert_eq!(
            first.diff.as_ref().unwrap().marker(1),
            Some(Marker::Modified)
        );
        assert_eq!(
            first.diff.as_ref().unwrap().marker(3),
            Some(Marker::Modified)
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "a\nB\nC\n");
        git(dir.path(), &["add", "--", "- space [界].txt"]);
        git(dir.path(), &["commit", "-qm", "changed"]);
        let next = run(&mut worker, &path, &doc, true);
        assert_ne!(first.baseline, next.baseline);
        assert_eq!(next.diff.as_ref().unwrap().marker(1), None);
        assert_eq!(next.diff.as_ref().unwrap().marker(3), Some(Marker::Added));
        let transaction = doc
            .transaction([Edit::new(CharOffset(6)..CharOffset(14), "")])
            .unwrap();
        doc.apply(
            transaction,
            &mut SelectionSet::single(Selection::cursor(CharOffset(0))),
        )
        .unwrap();
        assert!(
            run(&mut worker, &path, &doc, false)
                .diff
                .unwrap()
                .hunks
                .is_empty()
        );
        // A repository change cannot leak into ordinary typing until refresh.
        git(dir.path(), &["checkout", "-q", "HEAD~1"]);
        assert!(
            run(&mut worker, &path, &doc, false)
                .diff
                .unwrap()
                .hunks
                .is_empty()
        );
        assert!(
            !run(&mut worker, &path, &doc, true)
                .diff
                .unwrap()
                .hunks
                .is_empty()
        );
    }

    #[test]
    fn worktrees_unborn_untracked_binary_and_non_repository_files_are_handled() {
        let dir = repo();
        let path = dir.path().join("file.txt");
        fs::write(&path, "base\n").unwrap();
        let doc = TextDocument::from("changed\n");
        let mut worker = Worker::default();
        assert!(run(&mut worker, &path, &doc, true).diff.is_none());
        git(dir.path(), &["add", "file.txt"]);
        git(dir.path(), &["commit", "-qm", "initial"]);
        assert_eq!(
            run(&mut worker, &path, &doc, true).diff.unwrap().marker(0),
            Some(Marker::Modified)
        );
        let other = tempfile::tempdir().unwrap();
        let worktree = other.path().join("linked");
        git(
            dir.path(),
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                worktree.to_str().unwrap(),
            ],
        );
        assert!(
            run(&mut worker, &worktree.join("file.txt"), &doc, true)
                .diff
                .is_some()
        );
        let untracked = dir.path().join("new.txt");
        fs::write(&untracked, "new").unwrap();
        assert!(run(&mut worker, &untracked, &doc, true).diff.is_none());
        let outside = other.path().join("outside.txt");
        fs::write(&outside, "outside").unwrap();
        assert!(run(&mut worker, &outside, &doc, true).diff.is_none());
        fs::write(&path, [0, 1, 2]).unwrap();
        git(dir.path(), &["add", "file.txt"]);
        git(dir.path(), &["commit", "-qm", "binary"]);
        assert!(run(&mut worker, &path, &doc, true).diff.is_none());
    }
}
