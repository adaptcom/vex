//! Workspace regex search over cancellable file reads and shared open buffers.
//! Discovery reuses the file picker's ignore rules. No document is flattened.

use super::{
    Entry, Item,
    files::{Index, MAX_RESULTS},
};
use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File},
    io::{self, Read},
    ops::Range,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use vex_core::{
    ByteOffset, Document, DocumentId, Revision, Rope, Snapshot,
    regex::{Cache, Options, Regex},
};
use vex_editor::background::Cancellation;

const MAX_FILE_BYTES: usize = 128 << 20;
const MAX_TOTAL_BYTES: usize = 1 << 30;
const MAX_MATCHES: usize = 100_000;
const MAX_QUERY_TIME: Duration = Duration::from_secs(10);

pub(crate) struct OpenDocument {
    pub path: PathBuf,
    pub snapshot: Snapshot,
}

pub(crate) struct Job {
    pub session: u64,
    pub revision: u64,
    pub root: PathBuf,
    pub query: String,
    pub documents: Arc<[OpenDocument]>,
    pub cancellation: Cancellation,
}

#[derive(Clone, Debug)]
pub(crate) struct Hit {
    pub path: PathBuf,
    /// Matching lines, including the end line's newline, as in Helix global search.
    pub lines: Range<usize>,
    pub version: Option<(DocumentId, Revision)>,
}

impl PartialEq for Hit {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path && self.lines == other.lines
    }
}
impl Eq for Hit {}

pub(crate) struct Result {
    pub session: u64,
    pub revision: u64,
    pub root: PathBuf,
    pub items: Vec<Item<Hit>>,
    pub matched: usize,
    pub scanned: usize,
    pub scanning: bool,
    pub notice: String,
}

#[derive(Default)]
pub(crate) struct Worker {
    index: Option<Index>,
}

struct Matches {
    rows: BTreeMap<(PathBuf, usize, usize), Item<Hit>>,
    matched: usize,
    seen: usize,
    scanned: usize,
    bytes: usize,
    skipped: usize,
    limited: bool,
}

impl Worker {
    pub fn run(&mut self, job: Job, mut emit: impl FnMut(Result)) {
        if job.cancellation.is_cancelled() {
            return;
        }
        if job.query.is_empty() {
            emit(empty(&job, "Type a regular expression".into()));
            return;
        }
        let regex = match Regex::new(
            &job.query,
            Options {
                case_insensitive: !job.query.chars().any(char::is_uppercase),
                multi_line: true,
                crlf: true,
            },
        ) {
            Ok(regex) => regex,
            Err(error) => {
                emit(empty(&job, format!("Invalid regex: {error}")));
                return;
            }
        };
        let started = Instant::now();
        let stop = || job.cancellation.is_cancelled() || started.elapsed() >= MAX_QUERY_TIME;
        if self
            .index
            .as_ref()
            .is_none_or(|index| index.session != job.session)
        {
            let root = fs::canonicalize(&job.root).unwrap_or_else(|_| job.root.clone());
            self.index = Index::at_root(job.session, root, &job.cancellation);
        }
        let Some(index) = self.index.as_mut() else {
            return;
        };
        let documents: HashMap<_, _> = job
            .documents
            .iter()
            .map(|document| (&document.path, &document.snapshot))
            .collect();
        let mut matches = Matches {
            rows: BTreeMap::new(),
            matched: 0,
            seen: 0,
            scanned: 0,
            bytes: 0,
            skipped: 0,
            limited: false,
        };
        let mut cache = Cache::default();
        let mut cursor = 0;
        let mut publish = Instant::now();
        loop {
            if job.cancellation.is_cancelled() {
                return;
            }
            index.scan(&job.cancellation);
            while cursor < index.entries.len() && !stop() && !matches.limited {
                if matches.bytes >= MAX_TOTAL_BYTES {
                    matches.limited = true;
                    break;
                }
                let entry = &index.entries[cursor];
                cursor += 1;
                matches.scanned += 1;
                let loaded;
                let (text, version) = if let Some(snapshot) = documents.get(&entry.value) {
                    if snapshot.text().len_bytes() > MAX_FILE_BYTES {
                        matches.skipped += 1;
                        continue;
                    }
                    if snapshot.text().len_bytes() > MAX_TOTAL_BYTES.saturating_sub(matches.bytes) {
                        matches.limited = true;
                        break;
                    }
                    matches.bytes += snapshot.text().len_bytes();
                    (snapshot.text(), Some((snapshot.id(), snapshot.revision())))
                } else {
                    match read(&entry.value, &stop, &mut matches.bytes) {
                        Ok(document) => {
                            loaded = document;
                            (loaded.text(), None)
                        }
                        Err(_) => {
                            matches.skipped += 1;
                            continue;
                        }
                    }
                };
                find_lines(
                    &regex,
                    &mut cache,
                    text,
                    entry,
                    version,
                    &mut matches,
                    &stop,
                );
                if Instant::now() >= publish {
                    if job.cancellation.is_cancelled() {
                        return;
                    }
                    emit(matches.result(&job, index, true));
                    publish = Instant::now() + Duration::from_millis(40);
                }
            }
            if job.cancellation.is_cancelled() {
                return;
            }
            let complete = index.complete() && cursor == index.entries.len();
            if complete || stop() || matches.limited {
                matches.limited |= stop();
                emit(matches.result(&job, index, false));
                return;
            }
            if Instant::now() >= publish {
                emit(matches.result(&job, index, true));
                publish = Instant::now() + Duration::from_millis(40);
            }
        }
    }
}

impl Matches {
    fn result(&self, job: &Job, index: &Index, scanning: bool) -> Result {
        let mut notices = Vec::new();
        if !index.notice.is_empty() {
            notices.push(index.notice.clone());
        }
        if self.skipped != 0 {
            notices.push(format!(
                "{} unreadable, binary, or oversized files skipped",
                self.skipped
            ));
        }
        if self.limited {
            notices.push("Search limit reached; narrow the query".into());
        }
        Result {
            session: job.session,
            revision: job.revision,
            root: index.root.clone(),
            items: self
                .rows
                .values()
                .map(|item| Item {
                    entry: item.entry.clone(),
                    matched: item.matched.clone(),
                })
                .collect(),
            matched: self.matched,
            scanned: self.scanned,
            scanning,
            notice: notices.join(" · "),
        }
    }
}

fn empty(job: &Job, notice: String) -> Result {
    Result {
        session: job.session,
        revision: job.revision,
        root: job.root.clone(),
        items: Vec::new(),
        matched: 0,
        scanned: 0,
        scanning: false,
        notice,
    }
}

fn find_lines(
    regex: &Regex,
    cache: &mut Cache,
    text: &Rope,
    file: &Entry<PathBuf>,
    version: Option<(DocumentId, Revision)>,
    matches: &mut Matches,
    stop: &impl Fn() -> bool,
) {
    let mut at = 0;
    let mut previous = None;
    while let Some(found) = regex.find(
        text.slice(..),
        ByteOffset(at)..ByteOffset(text.len_bytes()),
        cache,
        stop,
    ) {
        matches.seen += 1;
        if matches.seen > MAX_MATCHES {
            matches.limited = true;
            break;
        }
        let first = text.byte_to_line(found.start.0);
        let last = text.byte_to_line(found.end.0.saturating_sub(1).max(found.start.0)) + 1;
        if previous != Some((first, last)) {
            previous = Some((first, last));
            matches.matched += 1;
            if matches.rows.len() < MAX_RESULTS
                || matches
                    .rows
                    .last_key_value()
                    .is_some_and(|((path, start, end), _)| {
                        (&file.value, first, last) < (path, *start, *end)
                    })
            {
                let mut label = format!("{}:{}: ", file.label, first + 1);
                let mut highlighted = Vec::new();
                let char_start = text
                    .byte_to_char(found.start.0)
                    .saturating_sub(40)
                    .max(text.line_to_char(first));
                let mut byte = text.char_to_byte(char_start);
                for ch in text.chars_at(char_start).take(160) {
                    if matches!(ch, '\r' | '\n') {
                        break;
                    }
                    if found.start.0 <= byte && byte < found.end.0 {
                        highlighted.push(label.len());
                    }
                    label.push(if ch.is_control() { ' ' } else { ch });
                    byte += ch.len_utf8();
                }
                matches.rows.insert(
                    (file.value.clone(), first, last),
                    Item {
                        entry: Arc::new(Entry {
                            label,
                            value: Hit {
                                path: file.value.clone(),
                                lines: first..last,
                                version,
                            },
                        }),
                        matched: highlighted,
                    },
                );
                if matches.rows.len() > MAX_RESULTS {
                    matches.rows.pop_last();
                }
            }
        }
        if found.end > found.start {
            at = found.end.0;
        } else if found.end.0 == text.len_bytes() {
            break;
        } else {
            let next = text.byte_to_char(found.end.0) + 1;
            at = text.char_to_byte(next);
        }
        if stop() {
            break;
        }
    }
}

fn read(path: &PathBuf, stop: &impl Fn() -> bool, bytes: &mut usize) -> io::Result<Document> {
    if stop() {
        return Err(io::Error::other("cancelled"));
    }
    let metadata = fs::symlink_metadata(path)?;
    let remaining = MAX_TOTAL_BYTES.saturating_sub(*bytes);
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES.min(remaining) as u64 {
        return Err(io::Error::other("file limit"));
    }
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    struct Reader<'a, F> {
        file: File,
        stop: &'a F,
        remaining: usize,
        bytes: &'a mut usize,
    }
    impl<F: Fn() -> bool> Read for Reader<'_, F> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if (self.stop)() {
                return Err(io::Error::other("cancelled"));
            }
            let length = buffer
                .len()
                .min(64 * 1024)
                .min(self.remaining.saturating_add(1));
            let count = self.file.read(&mut buffer[..length])?;
            *self.bytes += count;
            if count > self.remaining || buffer[..count].contains(&0) {
                return Err(io::Error::other("binary file or file limit"));
            }
            self.remaining -= count;
            Ok(count)
        }
    }
    Document::from_reader(Reader {
        file,
        stop,
        remaining: MAX_FILE_BYTES.min(remaining),
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(root: &std::path::Path, query: &str) -> Job {
        Job {
            session: 1,
            revision: 1,
            root: root.into(),
            query: query.into(),
            documents: Arc::from([]),
            cancellation: Cancellation::default(),
        }
    }
    fn run(worker: &mut Worker, job: Job) -> Result {
        let mut result = None;
        worker.run(job, |next| result = Some(next));
        result.unwrap()
    }

    #[test]
    fn searches_dotfiles_and_unsaved_unicode_text_while_skipping_ignored_and_binary_files() {
        let root = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(root.path()).unwrap();
        fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::create_dir(root.join(".config")).unwrap();
        fs::create_dir(root.join(".git")).unwrap();
        for name in [
            "ignored.txt",
            ".hidden",
            ".config/settings",
            ".git/HEAD",
            ".config/.git",
            "main.txt",
        ] {
            fs::write(root.join(name), "old needle\n").unwrap();
        }
        fs::write(root.join("binary"), b"needle\0").unwrap();
        fs::write(root.join("invalid"), b"needle\xff").unwrap();
        let document = Document::from("🦀 Needle\r\nsecond line\r\nneedle needle\n");
        let mut request = job(&root, "needle\\r?\\nsecond");
        request.documents = Arc::from([OpenDocument {
            path: root.join("main.txt"),
            snapshot: document.snapshot(),
        }]);
        let mut worker = Worker::default();
        let result = run(&mut worker, request);
        assert!(!result.scanning);
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].entry.value.lines, 0..2);
        assert_eq!(
            result.items[0].entry.value.version,
            Some((document.id(), document.revision()))
        );
        assert!(result.notice.contains("2 unreadable"));
        let result = run(&mut worker, job(&root, "needle"));
        assert_eq!(
            result
                .items
                .iter()
                .map(|item| item.entry.value.path.clone())
                .collect::<Vec<_>>(),
            [
                root.join(".config/settings"),
                root.join(".hidden"),
                root.join("main.txt")
            ]
        );
        let result = run(&mut worker, job(&root, "NEEDLE"));
        assert!(result.items.is_empty());
    }

    #[test]
    fn subdirectory_search_inherits_ignore_rules_without_searching_outside_its_root() {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        fs::create_dir_all(root.join(".git/info")).unwrap();
        fs::create_dir_all(root.join("src/sub")).unwrap();
        fs::write(root.join(".gitignore"), "/src/ignored.txt\n*.log\n").unwrap();
        fs::write(root.join(".git/info/exclude"), "excluded.txt\n").unwrap();
        fs::write(root.join("src/.ignore"), "!keep.log\n").unwrap();
        for name in [
            "outside.txt",
            "src/ignored.txt",
            "src/bad.log",
            "src/excluded.txt",
            "src/keep.log",
            "src/sub/visible.txt",
        ] {
            fs::write(root.join(name), "needle\n").unwrap();
        }
        let result = run(&mut Worker::default(), job(&root.join("src"), "needle"));
        let paths: Vec<_> = result
            .items
            .iter()
            .map(|item| {
                item.entry
                    .value
                    .path
                    .strip_prefix(&root)
                    .unwrap()
                    .to_path_buf()
            })
            .collect();
        assert_eq!(
            paths,
            [
                PathBuf::from("src/keep.log"),
                PathBuf::from("src/sub/visible.txt")
            ]
        );
    }

    #[test]
    fn reads_and_matching_cancel_cooperatively_and_cancelled_queries_can_resume_discovery() {
        use std::cell::Cell;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("large.txt");
        fs::write(&path, "x".repeat(256 << 10)).unwrap();
        let checks = Cell::new(0);
        let stop = || {
            checks.set(checks.get() + 1);
            checks.get() >= 3
        };
        let mut bytes = 0;
        assert!(read(&path, &stop, &mut bytes).is_err());
        assert!((1..=64 << 10).contains(&bytes));
        // Failed binary reads also count against the total byte budget.
        fs::write(&path, b"needle\0").unwrap();
        bytes = 0;
        assert!(read(&path, &|| false, &mut bytes).is_err());
        assert_eq!(bytes, 7);
        fs::write(&path, "needle\n").unwrap();
        for n in 0..300 {
            fs::write(root.path().join(format!("{n}.txt")), "needle\n").unwrap();
        }
        let request = job(root.path(), "needle");
        let cancellation = request.cancellation.clone();
        let mut published = 0;
        let mut worker = Worker::default();
        worker.run(request, |result| {
            assert!(result.scanning);
            published += 1;
            cancellation.cancel();
        });
        assert_eq!(published, 1);
        let result = run(&mut worker, job(root.path(), "needle"));
        assert_eq!(result.matched, 301);
        assert!(!result.scanning);
        let cancelled = Cancellation::default();
        cancelled.cancel();
        assert!(Index::at_root(2, root.path().into(), &cancelled).is_none());
    }

    #[test]
    fn frequent_matches_and_oversized_files_stop_at_search_limits() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("text.txt");
        fs::write(&path, "x".repeat(MAX_MATCHES + 1)).unwrap();
        let result = run(&mut Worker::default(), job(root.path(), "x"));
        assert_eq!(result.matched, 1); // Repeated matches on one line coalesce.
        assert!(result.notice.contains("Search limit reached"));
        let mut bytes = MAX_TOTAL_BYTES - 1;
        assert!(read(&path, &|| false, &mut bytes).is_err());
        assert_eq!(bytes, MAX_TOTAL_BYTES - 1);
        File::create(&path)
            .unwrap()
            .set_len(MAX_FILE_BYTES as u64 + 1)
            .unwrap();
        bytes = 0;
        assert!(read(&path, &|| false, &mut bytes).is_err());
        assert_eq!(bytes, 0); // Reject the metadata before reading/allocating.
    }

    #[test]
    #[ignore = "manual release-mode workspace worker performance measurement"]
    fn workspace_worker_performance() {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let path = root.join("large.txt");
        fs::write(&path, "disk placeholder\n").unwrap();
        for mib in [1usize, 100] {
            let mut text = "ordinary source line\n".repeat((mib << 20).div_ceil(21));
            text.push_str("unique_needle\n");
            let document = Document::from(text.as_str());
            drop(text);
            let documents: Arc<[OpenDocument]> = Arc::from([OpenDocument {
                path: path.clone(),
                snapshot: document.snapshot(),
            }]);
            let mut worker = Worker::default();
            let mut elapsed = Vec::new();
            for _ in 0..5 {
                let mut request = job(&root, "unique_needle");
                request.documents = documents.clone();
                let started = Instant::now();
                let result = run(&mut worker, request);
                elapsed.push(started.elapsed());
                assert_eq!(result.matched, 1, "{}", result.notice);
                assert!(result.notice.is_empty());
            }
            elapsed.sort();
            eprintln!(
                "workspace worker {mib} MiB shared buffer, unique EOF match: {:?} median of 5",
                elapsed[2]
            );
        }
    }

    #[test]
    fn empty_invalid_cancelled_and_bounded_results() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("many.txt"),
            "match match\n".repeat(MAX_RESULTS + 10),
        )
        .unwrap();
        let mut worker = Worker::default();
        assert!(run(&mut worker, job(root.path(), "")).items.is_empty());
        assert!(
            run(&mut worker, job(root.path(), "["))
                .notice
                .contains("Invalid regex")
        );
        let result = run(&mut worker, job(root.path(), "match"));
        assert_eq!(result.matched, MAX_RESULTS + 10);
        assert_eq!(result.items.len(), MAX_RESULTS);
        let result = run(&mut worker, job(root.path(), "^"));
        assert_eq!(result.matched, MAX_RESULTS + 11); // includes the empty final line
        let request = job(root.path(), "match");
        request.cancellation.cancel();
        worker.run(request, |_| panic!("cancelled job published"));
    }
}
