//! Visible-file probes. Metadata checks, UTF-8 reads, comparisons, and reload
//! preparation happen on a worker; the UI validates identity before applying.

use super::FileState;
use std::{
    collections::HashMap,
    fs::{self, File, Metadata},
    io::{self, BufReader, Read},
    path::{Path, PathBuf},
    time::SystemTime,
};
use vex_core::{CharOffset, Document, DocumentId, Edit, Revision, Rope, Snapshot, Transaction};
use vex_editor::background::Cancellation;

pub(crate) struct Probe {
    pub snapshot: Snapshot,
    pub path: PathBuf,
    saved: Rope,
    generation: u64,
    existed: bool,
}

impl FileState {
    pub(crate) fn probe(&self, document: &Document) -> Option<Probe> {
        Some(Probe {
            snapshot: document.snapshot(),
            path: self.target.clone()?,
            saved: self.saved.clone(),
            generation: self.generation,
            existed: self.existed,
        })
    }

    pub(crate) fn accepts(&self, result: &Observation, document: &Document) -> bool {
        self.target.as_ref() == Some(&result.path)
            && self.generation == result.generation
            && document.id() == result.document
            && document.revision() == result.revision
    }
}

pub(crate) struct Batch {
    pub request: u64,
    pub probes: Vec<Probe>,
    pub cancellation: Cancellation,
}

pub(crate) struct Result {
    pub request: u64,
    pub observations: Vec<Observation>,
}

pub(crate) struct Observation {
    pub document: DocumentId,
    pub revision: Revision,
    pub path: PathBuf,
    generation: u64,
    pub outcome: Outcome,
}

pub(crate) enum Outcome {
    Unchanged,
    Reload(Transaction),
    Conflict,
    Missing,
    Error(String),
    Retry,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    len: u64,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

impl Stamp {
    fn new(metadata: &Metadata) -> Self {
        Self {
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            len: metadata.len(),
            #[cfg(unix)]
            identity: {
                use std::os::unix::fs::MetadataExt;
                (
                    metadata.dev(),
                    metadata.ino(),
                    metadata.ctime(),
                    metadata.ctime_nsec(),
                )
            },
        }
    }
}

struct Cached {
    path: PathBuf,
    stamp: Stamp,
    text: Rope,
    saved: Rope,
    matches_saved: bool,
}

#[derive(Default)]
pub(crate) struct Worker(HashMap<DocumentId, Cached>);

impl Worker {
    pub fn run(&mut self, batch: Batch) -> Option<Result> {
        self.0
            .retain(|id, _| batch.probes.iter().any(|probe| probe.snapshot.id() == *id));
        let mut observations = Vec::new();
        for probe in batch.probes {
            if batch.cancellation.is_cancelled() {
                return None;
            }
            let outcome = match self.probe(&probe, &batch.cancellation) {
                Ok(outcome) => outcome,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.0.remove(&probe.snapshot.id());
                    if probe.existed {
                        Outcome::Missing
                    } else {
                        Outcome::Unchanged
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => Outcome::Retry,
                Err(error) => {
                    self.0.remove(&probe.snapshot.id());
                    Outcome::Error(error.to_string())
                }
            };
            observations.push(Observation {
                document: probe.snapshot.id(),
                revision: probe.snapshot.revision(),
                path: probe.path,
                generation: probe.generation,
                outcome,
            });
        }
        (!batch.cancellation.is_cancelled()).then_some(Result {
            request: batch.request,
            observations,
        })
    }

    fn probe(&mut self, probe: &Probe, cancellation: &Cancellation) -> io::Result<Outcome> {
        let metadata = regular_metadata(&probe.path)?;
        let stamp = Stamp::new(&metadata);
        let cached = self.0.get(&probe.snapshot.id());
        if cached.is_none_or(|cached| cached.path != probe.path || cached.stamp != stamp) {
            let (text, stamp) = read(&probe.path, cancellation)?;
            let matches_saved = equal(&text, &probe.saved, cancellation)?;
            self.0.insert(
                probe.snapshot.id(),
                Cached {
                    path: probe.path.clone(),
                    stamp,
                    text,
                    saved: probe.saved.clone(),
                    matches_saved,
                },
            );
        }
        let cached = self.0.get_mut(&probe.snapshot.id()).unwrap();
        if !cached.saved.is_instance(&probe.saved) {
            cached.matches_saved = equal(&cached.text, &probe.saved, cancellation)?;
            cached.saved = probe.saved.clone();
        }
        if cached.matches_saved && probe.existed {
            return Ok(Outcome::Unchanged);
        }
        if !probe.saved.is_instance(probe.snapshot.text()) {
            return Ok(Outcome::Conflict);
        }
        Ok(Outcome::Reload(replacement(
            &probe.snapshot,
            &cached.text,
            cancellation,
        )?))
    }
}

fn regular_metadata(path: &Path) -> io::Result<Metadata> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::other("only regular files can be reloaded"));
    }
    Ok(metadata)
}

struct Cancellable<'a> {
    file: File,
    cancellation: &'a Cancellation,
}

impl Read for Cancellable<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        check(self.cancellation)?;
        self.file.read(bytes)
    }
}

fn check(cancellation: &Cancellation) -> io::Result<()> {
    if cancellation.is_cancelled() {
        Err(io::Error::other("file read cancelled"))
    } else {
        Ok(())
    }
}

fn read(path: &Path, cancellation: &Cancellation) -> io::Result<(Rope, Stamp)> {
    regular_metadata(path)?;
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other("only regular files can be reloaded"));
    }
    let before = Stamp::new(&metadata);
    let mut reader = BufReader::new(Cancellable { file, cancellation });
    let text = Rope::from_reader(&mut reader)?;
    // Detect in-place writes and atomic replacement during the read. Retry on
    // the next poll instead of publishing a known inconsistent snapshot.
    if before != Stamp::new(&reader.get_ref().file.metadata()?)
        || before != Stamp::new(&regular_metadata(path)?)
    {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "file changed while reading",
        ));
    }
    Ok((text, before))
}

fn equal(left: &Rope, right: &Rope, cancellation: &Cancellation) -> io::Result<bool> {
    if left.len_bytes() != right.len_bytes() {
        return Ok(false);
    }
    for (index, (a, b)) in left.chars().zip(right.chars()).enumerate() {
        if index.is_multiple_of(8192) {
            check(cancellation)?;
        }
        if a != b {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Preserve unchanged prefix/suffix text so cursors outside the replacement map
/// naturally. This is scalar-safe, including CRLF and multibyte text.
fn replacement(
    snapshot: &Snapshot,
    text: &Rope,
    cancellation: &Cancellation,
) -> io::Result<Transaction> {
    let before = snapshot.text();
    let mut start = 0usize;
    for (a, b) in before.chars().zip(text.chars()) {
        if start.is_multiple_of(8192) {
            check(cancellation)?;
        }
        if a != b {
            break;
        }
        start += 1;
    }
    let mut old_end = before.len_chars();
    let mut new_end = text.len_chars();
    let mut old = before.chars_at(old_end);
    let mut new = text.chars_at(new_end);
    while old_end > start && new_end > start && old.prev() == new.prev() {
        if old_end.is_multiple_of(8192) {
            check(cancellation)?;
        }
        old_end -= 1;
        new_end -= 1;
    }
    snapshot
        .transaction([Edit::new(
            CharOffset(start)..CharOffset(old_end),
            text.slice(start..new_end).to_string(),
        )])
        .map_err(io::Error::other)
}

/// Explicit reload shares validation and edit preparation with background reads.
pub(crate) fn reload(snapshot: &Snapshot, path: &Path) -> io::Result<Transaction> {
    let cancellation = Cancellation::default();
    let (text, _) = read(path, &cancellation)?;
    replacement(snapshot, &text, &cancellation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vex_core::SelectionSet;

    #[test]
    fn replacements_preserve_unicode_prefixes_suffixes_and_empty_files() {
        for (before, after) in [
            ("same", "same"),
            ("", ""),
            ("", "new"),
            ("old", ""),
            ("a🦀b\r\n", "a界🦀b\r\n"),
            ("e\u{301}nd", "e\u{302}nd"),
            ("first\nsecond\n", "new first\nsecond\n"),
        ] {
            let mut document = Document::from(before);
            let transaction = replacement(
                &document.snapshot(),
                &Rope::from_str(after),
                &Cancellation::default(),
            )
            .unwrap();
            let changed = document
                .apply(transaction, &mut SelectionSet::default())
                .unwrap();
            assert_eq!(document.text(), after);
            assert_eq!(changed, before != after);
        }
    }

    #[test]
    fn unchanged_metadata_reuses_cached_text_and_new_files_can_appear_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");
        let (document, mut files) = FileState::load(Some(&path)).unwrap();
        let batch = || Batch {
            request: 1,
            probes: vec![files.probe(&document).unwrap()],
            cancellation: Cancellation::default(),
        };
        let mut worker = Worker::default();
        assert!(matches!(
            worker.run(batch()).unwrap().observations[0].outcome,
            Outcome::Unchanged
        ));
        fs::write(&path, "").unwrap();
        assert!(matches!(
            worker.run(batch()).unwrap().observations[0].outcome,
            Outcome::Reload(_)
        ));
        files.mark_saved(&document);
        let batch = || Batch {
            request: 1,
            probes: vec![files.probe(&document).unwrap()],
            cancellation: Cancellation::default(),
        };
        let cached = worker.0[&document.id()].text.clone();
        assert!(matches!(
            worker.run(batch()).unwrap().observations[0].outcome,
            Outcome::Unchanged
        ));
        assert!(cached.is_instance(&worker.0[&document.id()].text));
        let cancelled = batch();
        cancelled.cancellation.cancel();
        assert!(worker.run(cancelled).is_none());
    }
}
