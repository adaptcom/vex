//! Incremental file discovery and ranking, run by the existing latest-job worker.
//! Query changes retain the scan/index; closing a picker releases them off the UI.

use super::{
    Entry, Item, Preview,
    fuzzy::{Matcher, Query},
    ignore::{Rules, Scratch},
};
use std::{
    cmp::{Ordering, Reverse},
    collections::BinaryHeap,
    fs::{self, File, ReadDir},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use vex_core::{ByteOffset, Document};
use vex_editor::background::Cancellation;
use vex_syntax::{Language, Syntax};

pub(crate) const MAX_RESULTS: usize = 512;
const MAX_FILES: usize = 200_000;
const MAX_PATH_BYTES: usize = 64 << 20;
const PREVIEW_BYTES: usize = 64 << 10;

pub(crate) struct FileJob {
    pub session: u64,
    pub revision: u64,
    /// None closes the session. Paths are captured by the UI; all probing is here.
    pub source: Option<(Option<PathBuf>, PathBuf)>,
    pub query: String,
    pub cancellation: Cancellation,
}

pub(crate) struct FileResult {
    pub session: u64,
    pub revision: u64,
    pub root: PathBuf,
    pub items: Vec<Item<PathBuf>>,
    pub matched: usize,
    pub scanned: usize,
    pub scanning: bool,
    pub notice: String,
}

struct Directory {
    entries: ReadDir,
    pending: Option<fs::DirEntry>,
    rules: Arc<Rules>,
}

pub(super) struct Index {
    pub session: u64,
    pub root: PathBuf,
    stack: Vec<Directory>,
    pub entries: Vec<Arc<Entry<PathBuf>>>,
    bytes: usize,
    pub notice: String,
    ignore_scratch: Scratch,
}

/// Prefer the enclosing repository, otherwise the outermost Cargo project,
/// otherwise the captured working directory. This does not depend on LSP readiness.
fn project_root(origin: Option<&Path>, cwd: &Path) -> PathBuf {
    let mut project = None;
    let start = origin.and_then(Path::parent).unwrap_or(cwd);
    for ancestor in start.ancestors() {
        if ancestor.join(".git").exists() {
            return ancestor.into();
        }
        if ancestor.join("Cargo.toml").is_file() {
            project = Some(ancestor.to_path_buf());
        }
    }
    project.unwrap_or_else(|| cwd.into())
}

impl Index {
    fn new(job: &FileJob) -> Option<Self> {
        let (origin, cwd) = job.source.as_ref().unwrap();
        let root = project_root(origin.as_deref(), cwd);
        Self::at_root(job.session, root, &job.cancellation)
    }

    pub fn at_root(session: u64, root: PathBuf, cancellation: &Cancellation) -> Option<Self> {
        let mut index = Self {
            session,
            root,
            stack: Vec::new(),
            entries: Vec::new(),
            bytes: 0,
            notice: String::new(),
            ignore_scratch: Scratch::default(),
        };
        let mut ancestors = Vec::new();
        if !index.root.join(".git").exists() {
            for ancestor in index.root.ancestors().skip(1).take(64) {
                if cancellation.is_cancelled() {
                    return None;
                }
                ancestors.push(ancestor.to_path_buf());
                if ancestor.join(".git").exists() {
                    break;
                }
            }
        }
        let mut parent = None;
        for ancestor in ancestors.into_iter().rev() {
            if cancellation.is_cancelled() {
                return None;
            }
            parent = Some(index.rules(&ancestor, parent, true));
        }
        if cancellation.is_cancelled() {
            return None;
        }
        index.enter(PathBuf::new(), parent);
        (!cancellation.is_cancelled()).then_some(index)
    }

    fn enter(&mut self, relative: PathBuf, parent: Option<Arc<Rules>>) {
        let path = self.root.join(&relative);
        if self.stack.len() >= 64 {
            self.notice = "directory depth limit reached".into();
            return;
        }
        let entries = match fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(error) => {
                self.notice = format!("cannot scan {}: {error}", relative.display());
                return;
            }
        };
        let rules = self.rules(&path, parent, relative.as_os_str().is_empty());
        self.stack.push(Directory {
            entries,
            pending: None,
            rules,
        });
    }

    fn rules(&mut self, path: &Path, parent: Option<Arc<Rules>>, repository: bool) -> Arc<Rules> {
        let mut contents = String::new();
        // Repository exclusions have lower precedence than .gitignore; .ignore
        // lets projects configure this picker without altering Git's policy.
        let names: &[&str] = if repository {
            &[".git/info/exclude", ".gitignore", ".ignore"]
        } else {
            &[".gitignore", ".ignore"]
        };
        for name in names {
            let file = path.join(name);
            // Never open a symlink or special file as an ignore configuration.
            if !fs::symlink_metadata(&file).is_ok_and(|m| m.is_file()) {
                continue;
            }
            let result = File::open(&file).and_then(|file| {
                let mut bytes = Vec::new();
                file.take((64 << 10) + 1).read_to_end(&mut bytes)?;
                if bytes.len() > 64 << 10 {
                    return Err(io::Error::other("ignore file exceeds 64 KiB"));
                }
                String::from_utf8(bytes).map_err(io::Error::other)
            });
            match result {
                Ok(text) => {
                    contents.push_str(&text);
                    contents.push('\n');
                }
                Err(error) => self.notice = format!("{}: {error}", file.display()),
            }
        }
        Arc::new(Rules::new(path.into(), &contents, parent))
    }

    pub fn complete(&self) -> bool {
        self.stack.is_empty()
    }

    pub fn scan(&mut self, cancellation: &Cancellation) {
        let deadline = Instant::now() + Duration::from_millis(3);
        for _ in 0..256 {
            if cancellation.is_cancelled() || Instant::now() >= deadline {
                break;
            }
            let Some(directory) = self.stack.last_mut() else {
                break;
            };
            let Some(entry) = directory
                .pending
                .take()
                .map(Ok)
                .or_else(|| directory.entries.next())
            else {
                self.stack.pop();
                continue;
            };
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    self.notice = error.to_string();
                    continue;
                }
            };
            self.visit(entry, cancellation);
        }
    }

    fn visit(&mut self, entry: fs::DirEntry, cancellation: &Cancellation) {
        let directory = self.stack.last_mut().unwrap();
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            return;
        }
        let path = entry.path();
        let kind = match entry.file_type() {
            Ok(kind) => kind,
            Err(error) => {
                self.notice = error.to_string();
                return;
            }
        };
        // Symlink directories are never traversed (including cycles).
        if !kind.is_file() && !kind.is_dir() {
            return;
        }
        let relative = path.strip_prefix(&self.root).unwrap();
        let Some(ignored) =
            directory
                .rules
                .check(&path, kind.is_dir(), &mut self.ignore_scratch, cancellation)
        else {
            // read_dir already advanced. Retain the entry so a query change
            // cannot silently skip this file or its entire directory tree.
            directory.pending = Some(entry);
            return;
        };
        if ignored {
            return;
        }
        if kind.is_dir() {
            let rules = directory.rules.clone();
            self.enter(relative.into(), Some(rules));
        } else {
            let label = relative.to_string_lossy().into_owned();
            self.bytes += path.as_os_str().len() + label.len();
            if self.entries.len() == MAX_FILES || self.bytes > MAX_PATH_BYTES {
                self.notice = "file index limit reached; open a smaller project".into();
                self.stack.clear();
                return;
            }
            self.entries.push(Arc::new(Entry { label, value: path }));
        }
    }
}

#[derive(Clone)]
struct Ranked {
    score: i32,
    entry: Arc<Entry<PathBuf>>,
}
impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Ranked {}
impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .cmp(&other.score)
            .then_with(|| other.entry.label.cmp(&self.entry.label))
            .then_with(|| other.entry.value.cmp(&self.entry.value))
    }
}

#[derive(Default)]
pub(crate) struct FileWorker {
    index: Option<Index>,
    matcher: Matcher,
}

impl FileWorker {
    pub fn run(&mut self, job: FileJob, mut emit: impl FnMut(FileResult)) {
        if job.source.is_none() {
            self.index = None;
            return;
        }
        if job.cancellation.is_cancelled() {
            return;
        }
        if self
            .index
            .as_ref()
            .is_none_or(|index| index.session != job.session)
        {
            self.index = Index::new(&job);
        }
        let Some(index) = self.index.as_mut() else {
            return;
        };
        let query = Query::new(&job.query);
        let mut ranked: BinaryHeap<Reverse<Ranked>> = BinaryHeap::new();
        let mut cursor = 0;
        let mut matched = 0;
        let mut next_publish = Instant::now();
        loop {
            if job.cancellation.is_cancelled() {
                return;
            }
            index.scan(&job.cancellation);
            let deadline = Instant::now() + Duration::from_millis(3);
            while cursor < index.entries.len() {
                if job.cancellation.is_cancelled() {
                    return;
                }
                let entry = &index.entries[cursor];
                if let Some(score) = self.matcher.score(&entry.label, &query) {
                    matched += 1;
                    let candidate = Ranked {
                        score,
                        entry: entry.clone(),
                    };
                    if ranked.len() < MAX_RESULTS {
                        ranked.push(Reverse(candidate));
                    } else if ranked.peek().is_some_and(|worst| candidate > worst.0) {
                        ranked.pop();
                        ranked.push(Reverse(candidate));
                    }
                }
                cursor += 1;
                if cursor % 64 == 0 && Instant::now() >= deadline {
                    break;
                }
            }
            let complete = index.stack.is_empty() && cursor == index.entries.len();
            if complete || Instant::now() >= next_publish {
                let mut matches: Vec<_> = ranked.iter().map(|r| r.0.clone()).collect();
                matches.sort_unstable_by(|a, b| b.cmp(a));
                let mut items = Vec::with_capacity(matches.len());
                for candidate in matches {
                    if job.cancellation.is_cancelled() {
                        return;
                    }
                    let matched = self.matcher.indices(&candidate.entry.label, &query);
                    items.push(Item {
                        entry: candidate.entry,
                        matched,
                    });
                }
                emit(FileResult {
                    session: job.session,
                    revision: job.revision,
                    root: index.root.clone(),
                    items,
                    matched,
                    scanned: index.entries.len(),
                    scanning: !complete,
                    notice: index.notice.clone(),
                });
                next_publish = Instant::now() + Duration::from_millis(40);
            }
            if complete {
                return;
            }
        }
    }
}

pub(crate) struct PreviewJob {
    pub session: u64,
    pub request: u64,
    pub path: Option<PathBuf>,
    pub snapshot: Option<vex_core::Snapshot>,
    pub language: Option<Language>,
    pub position: Option<vex_lsp::Position>,
    pub cancellation: Cancellation,
}

pub(crate) struct PreviewResult {
    pub session: u64,
    pub request: u64,
    pub preview: Preview,
}

impl PreviewJob {
    pub fn run(self) -> Option<PreviewResult> {
        if self.cancellation.is_cancelled() {
            return None;
        }
        let preview = if self.snapshot.is_some() || self.position.is_some() {
            snapshot_preview(
                self.path.as_deref(),
                self.snapshot,
                self.language,
                self.position.unwrap_or(vex_lsp::Position {
                    line: 0,
                    character: 0,
                }),
                &self.cancellation,
            )
        } else if let Some(path) = &self.path {
            preview(path, &self.cancellation)
        } else {
            Err(io::Error::other("preview has no file or buffer"))
        }
        .unwrap_or_else(|error| Preview::plain(format!("Preview unavailable: {error}")));
        (!self.cancellation.is_cancelled()).then_some(PreviewResult {
            session: self.session,
            request: self.request,
            preview,
        })
    }
}

fn snapshot_preview(
    path: Option<&Path>,
    snapshot: Option<vex_core::Snapshot>,
    language: Option<Language>,
    position: vex_lsp::Position,
    cancellation: &Cancellation,
) -> io::Result<Preview> {
    let snapshot = match snapshot {
        Some(snapshot) => snapshot,
        None => {
            let path = path.ok_or_else(|| io::Error::other("preview has no file or buffer"))?;
            if !fs::symlink_metadata(path)?.is_file() {
                return Err(io::Error::other("not a regular file"));
            }
            let mut file = File::open(path)?.take(vex_lsp::MAX_DOCUMENT_BYTES as u64 + 1);
            let mut bytes = Vec::new();
            let mut chunk = [0; 8192];
            loop {
                if cancellation.is_cancelled() {
                    return Ok(Preview::default());
                }
                let read = file.read(&mut chunk)?;
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..read]);
            }
            if bytes.len() > vex_lsp::MAX_DOCUMENT_BYTES {
                return Err(io::Error::other("symbol preview exceeds 8 MiB"));
            }
            if bytes.contains(&0) {
                return Err(io::Error::other("binary file"));
            }
            let text = std::str::from_utf8(&bytes).map_err(io::Error::other)?;
            Document::from(text).snapshot()
        }
    };
    let text = snapshot.text();
    let offset = vex_lsp::offset(text, position)
        .ok_or_else(|| io::Error::other("invalid symbol position"))?;
    let line = text.char_to_line(offset.0);
    let first_line = line.saturating_sub(3);
    let start = text.line_to_byte(first_line);
    let last_line = (first_line + 200).min(text.len_lines());
    let end = text.line_to_byte(last_line).min(start + PREVIEW_BYTES);
    let end = text.char_to_byte(text.byte_to_char(end));
    let mut preview = Preview {
        text: text.byte_slice(start..end).to_string(),
        line_offset: Some(first_line),
        focus_line: Some(line - first_line),
        ..Preview::default()
    };
    // Large retained buffers still get a bounded text preview. Parsing remains
    // capped and entirely on this worker, never on the input/drawing thread.
    if !cancellation.is_cancelled()
        && text.len_bytes() <= vex_lsp::MAX_DOCUMENT_BYTES
        && let Some(language) = language.or_else(|| Language::detect(path, text))
    {
        let mut syntax = Syntax::from_snapshot(language, snapshot.clone());
        preview.highlights = syntax
            .highlights_current(ByteOffset(start)..ByteOffset(end), || {
                cancellation.is_cancelled()
            })
            .iter()
            .filter_map(|span| {
                let a = span.range.start.0.max(start);
                let b = span.range.end.0.min(end);
                (a < b).then_some(vex_editor::HighlightSpan {
                    range: ByteOffset(a - start)..ByteOffset(b - start),
                    highlight: span.highlight,
                })
            })
            .collect::<Vec<_>>()
            .into();
    }
    if end < text.len_bytes() {
        preview.text.push_str("\n… preview truncated");
    }
    Ok(preview)
}

fn preview(path: &Path, cancellation: &Cancellation) -> io::Result<Preview> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    let mut file = File::open(path)?.take(PREVIEW_BYTES as u64 + 1);
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        if cancellation.is_cancelled() {
            return Ok(Preview::default());
        }
        let n = file.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
    let truncated = bytes.len() > PREVIEW_BYTES;
    bytes.truncate(PREVIEW_BYTES);
    if bytes.contains(&0) {
        return Ok(Preview::plain("Binary file — no preview"));
    }
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(error) if truncated && error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap()
        }
        Err(_) => return Ok(Preview::plain("Non-UTF-8 file — no preview")),
    };
    let mut lines = text.lines();
    let mut preview = Preview::plain(lines.by_ref().take(200).collect::<Vec<_>>().join("\n"));
    let document = Document::from(preview.text.as_str());
    if !cancellation.is_cancelled()
        && let Some(language) = Language::detect(Some(path), document.text())
    {
        // Parse exactly the displayed prefix, before appending any UI notices.
        // The existing parser/query budgets fall back to plain text on timeout.
        let mut syntax = Syntax::new(language, &document);
        preview.highlights = syntax
            .highlights_current(ByteOffset(0)..ByteOffset(preview.text.len()), || {
                cancellation.is_cancelled()
            });
    }
    if truncated || lines.next().is_some() {
        preview.text.push_str("\n… preview truncated");
    }
    Ok(preview)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_buffer_preview_uses_a_bounded_slice_without_parsing_or_disk_reads() {
        let document = Document::from("line\n".repeat(2_000_000).as_str());
        let result = PreviewJob {
            session: 1,
            request: 1,
            path: None,
            snapshot: Some(document.snapshot()),
            language: Some(Language::Rust),
            position: Some(vex_lsp::Position {
                line: 1_000_000,
                character: 0,
            }),
            cancellation: Cancellation::default(),
        }
        .run()
        .unwrap();
        assert_eq!(result.preview.line_offset, Some(999_997));
        assert!(result.preview.text.starts_with("line\n"));
        assert!(result.preview.text.len() < PREVIEW_BYTES + 100);
        assert!(result.preview.highlights.is_empty());
    }
    #[test]
    fn symbol_previews_follow_late_lines_preserve_multiline_syntax_and_use_snapshots() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("late.rs");
        let source = format!(
            "/*\n{}🦀 target\n*/\nfn tail() {{}}\n",
            "comment\n".repeat(300)
        );
        fs::write(&path, "saved contents differ").unwrap();
        let document = Document::from(source.as_str());
        let result = snapshot_preview(
            Some(&path),
            Some(document.snapshot()),
            None,
            vex_lsp::Position {
                line: 301,
                character: 3,
            },
            &Cancellation::default(),
        )
        .unwrap();
        assert!(result.text.contains("🦀 target"));
        assert_eq!(result.line_offset, Some(298));
        assert_eq!(result.focus_line, Some(3));
        let byte = result.text.find("target").unwrap();
        assert!(
            result
                .highlights
                .iter()
                .any(|span| span.range.contains(&ByteOffset(byte))
                    && span.highlight == vex_syntax::Highlight::Comment)
        );
        fs::write(&path, &source).unwrap();
        let disk = snapshot_preview(
            Some(&path),
            None,
            None,
            vex_lsp::Position {
                line: 301,
                character: 3,
            },
            &Cancellation::default(),
        )
        .unwrap();
        assert_eq!(disk.text, result.text);
        assert!(
            snapshot_preview(
                Some(&path),
                None,
                None,
                vex_lsp::Position {
                    line: 999,
                    character: 0
                },
                &Cancellation::default()
            )
            .is_err()
        );
        let mut frame = crate::screen::Frame::default();
        frame.reset(60, 8).unwrap();
        result.paint(&mut frame, 1, 1, 58, 7);
        assert!(frame.row_text(4).contains("302 >🦀 target"));
    }

    fn job(root: &Path, query: &str) -> FileJob {
        FileJob {
            session: 1,
            revision: 1,
            source: Some((None, root.into())),
            query: query.into(),
            cancellation: Cancellation::default(),
        }
    }

    #[test]
    fn walking_respects_nested_ignores_pruning_hidden_files_and_query_changes() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src/generated")).unwrap();
        fs::write(
            root.join(".gitignore"),
            "*.log\ngenerated/\n!generated/keep.rs\n",
        )
        .unwrap();
        fs::write(root.join("src/.gitignore"), "!keep.log\n").unwrap();
        for path in [
            "app.rs",
            "src/界.rs",
            ".hidden",
            "bad.log",
            "src/keep.log",
            "src/bad.log",
            "src/generated/keep.rs",
        ] {
            fs::write(root.join(path), "preview").unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(root, root.join("src/cycle")).unwrap();
        let mut worker = FileWorker::default();
        let mut last = None;
        worker.run(job(root, ""), |result| last = Some(result));
        let result = last.unwrap();
        let labels: Vec<_> = result
            .items
            .iter()
            .map(|i| i.entry.label.as_str())
            .collect();
        assert_eq!(labels, ["app.rs", "src/keep.log", "src/界.rs"]);
        assert!(!result.scanning);
        worker.run(job(root, "界"), |result| {
            assert_eq!(result.items.len(), 1);
            assert_eq!(result.items[0].entry.value, root.join("src/界.rs"));
        });
        let cancelled = job(root, "");
        cancelled.cancellation.cancel();
        worker.run(cancelled, |_| panic!("cancelled result"));
        let mut close = job(root, "");
        close.source = None;
        worker.run(close, |_| panic!("close result"));
        assert!(worker.index.is_none());
    }

    #[test]
    fn cancelling_an_ignore_check_retries_the_consumed_directory_entry() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join(".gitignore"), "*.log").unwrap();
        fs::write(root.join("src/keep.txt"), "keep").unwrap();
        let mut index = Index::new(&job(root, "")).unwrap();
        let entry = index
            .stack
            .last_mut()
            .unwrap()
            .entries
            .by_ref()
            .map(Result::unwrap)
            .find(|entry| entry.file_name() == "src")
            .unwrap();
        // Cancellation can arrive after read_dir.next(), before rule matching.
        let cancelled = Cancellation::default();
        cancelled.cancel();
        index.visit(entry, &cancelled);
        let cancellation = Cancellation::default();
        while !index.stack.is_empty() {
            index.scan(&cancellation);
        }
        assert_eq!(index.entries.len(), 1);
        assert_eq!(index.entries[0].label, "src/keep.txt");
    }

    #[test]
    fn preview_is_bounded_handles_binary_and_split_utf8_and_never_reads_directories() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file");
        let cancel = Cancellation::default();
        fs::write(&path, "hello\n界\n").unwrap();
        assert_eq!(preview(&path, &cancel).unwrap().text, "hello\n界");
        fs::write(&path, [0, 1, 2]).unwrap();
        assert!(preview(&path, &cancel).unwrap().text.contains("Binary"));
        fs::write(&path, format!("{}界end", "a".repeat(PREVIEW_BYTES - 1))).unwrap();
        assert!(
            preview(&path, &cancel)
                .unwrap()
                .text
                .ends_with("preview truncated")
        );
        assert!(preview(directory.path(), &cancel).is_err());
    }

    #[test]
    fn previews_use_shared_detection_for_new_languages_and_shell_shebangs() {
        use vex_syntax::Highlight;
        let directory = tempfile::tempdir().unwrap();
        for (name, source, token, highlight) in [
            (
                "README.md",
                "# Heading\n\n**bold**",
                "bold",
                Highlight::Strong,
            ),
            (
                "script",
                "#!/bin/bash\necho hello",
                "echo",
                Highlight::Function,
            ),
            (
                "types.ts",
                "interface User { name: string }",
                "User",
                Highlight::Type,
            ),
            ("view.tsx", "const view = <div />", "div", Highlight::Type),
        ] {
            let path = directory.path().join(name);
            fs::write(&path, source).unwrap();
            let result = preview(&path, &Cancellation::default()).unwrap();
            let byte = result.text.find(token).unwrap();
            assert!(
                result
                    .highlights
                    .iter()
                    .any(|span| span.highlight == highlight
                        && span.range.contains(&ByteOffset(byte))),
                "{name}: {:?}",
                result.highlights
            );
        }
    }

    #[test]
    fn rust_preview_highlights_displayed_bytes_and_keeps_notices_and_unknown_files_plain() {
        use vex_syntax::Highlight;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("example.rs");
        let cancel = Cancellation::default();
        fs::write(
            &path,
            "// 界e\u{301}\r\nfn main() { let s = \"hello\"; }\r\n",
        )
        .unwrap();
        let rust = preview(&path, &cancel).unwrap();
        for (token, highlight) in [
            ("// 界e\u{301}", Highlight::Comment),
            ("fn", Highlight::Keyword),
            ("\"hello\"", Highlight::String),
        ] {
            let byte = rust.text.find(token).unwrap();
            assert!(
                rust.highlights
                    .iter()
                    .any(|span| span.highlight == highlight
                        && span.range.contains(&ByteOffset(byte))),
                "missing {highlight:?}: {:?}",
                rust.highlights
            );
        }
        let plain_path = directory.path().join("example.txt");
        fs::write(&plain_path, &rust.text).unwrap();
        assert!(preview(&plain_path, &cancel).unwrap().highlights.is_empty());
        fs::write(&path, "/* comment\n".repeat(201)).unwrap();
        let truncated = preview(&path, &cancel).unwrap();
        assert_eq!(truncated.text.lines().count(), 201);
        let notice = truncated.text.find("\n… preview truncated").unwrap();
        assert!(
            truncated
                .highlights
                .iter()
                .all(|span| span.range.end.0 <= notice)
        );
        for bytes in [&[0, 1][..], &[0xff, 0xfe][..]] {
            fs::write(&path, bytes).unwrap();
            assert!(preview(&path, &cancel).unwrap().highlights.is_empty());
        }
        cancel.cancel();
        assert!(
            PreviewJob {
                session: 1,
                request: 1,
                snapshot: None,
                language: None,
                position: None,
                path: Some(path),
                cancellation: cancel
            }
            .run()
            .is_none()
        );
    }

    #[test]
    #[ignore = "requires Git; explicit compatibility check, never a runtime dependency"]
    fn project_ignore_results_agree_with_git() {
        use std::{
            collections::BTreeSet,
            io::Write,
            process::{Command, Stdio},
        };
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .arg(root)
                .status()
                .unwrap()
                .success()
        );
        fs::write(root.join(".gitignore"), "*.log\n!important.log\n/build/\n!build/keep.txt\nsrc/**/generated?.[ch]\nassets/[a-c]?.tmp\nfoo/**/bar\n[[:digit:]].txt\nspace\\ \n\\#literal\n\\!literal\n").unwrap();
        let paths = [
            "keep.rs",
            "bad.log",
            "important.log",
            "src/bad.log",
            "src/important.log",
            "build/keep.txt",
            "src/build/yes.txt",
            "src/generated1.c",
            "src/nested/generated2.h",
            "src/generated11.c",
            "assets/ab.tmp",
            "assets/zz.tmp",
            "foo/bar",
            "foo/x/bar",
            "foo/xxbar",
            "1.txt",
            "a.txt",
            "space ",
            "space",
            "#literal",
            "!literal",
            "src/界.rs",
        ];
        for name in paths {
            let path = root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "text").unwrap();
        }
        fs::write(root.join("src/.gitignore"), "!bad.log\nimportant.log\n").unwrap();
        let mut child = Command::new("git")
            .current_dir(root)
            .args(["check-ignore", "--no-index", "--stdin", "-z"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        for path in paths {
            input.write_all(path.as_bytes()).unwrap();
            input.write_all(&[0]).unwrap();
        }
        drop(input);
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let ignored: BTreeSet<_> = output
            .stdout
            .split(|&byte| byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| String::from_utf8(path.into()).unwrap())
            .collect();
        let expected: BTreeSet<_> = paths
            .iter()
            .filter(|path| !ignored.contains(**path))
            .map(|path| path.to_string())
            .collect();
        let mut actual = BTreeSet::new();
        FileWorker::default().run(job(root, ""), |result| {
            if !result.scanning {
                actual = result
                    .items
                    .into_iter()
                    .map(|item| item.entry.label.clone())
                    .collect();
            }
        });
        assert_eq!(actual, expected);
    }
}
