//! Read-only repository snapshots and lazy patch previews. Paths use porcelain
//! v2's NUL framing; display text never becomes a command argument or path.

use crate::repository;
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};
use vex_editor::background::Cancellation;

pub const MAX_FILES: usize = 10_000;
pub const MAX_EXPANDED: usize = 32;
const MAX_PATCH: usize = 256 << 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Group {
    Conflicts,
    Unstaged,
    Staged,
    Untracked,
    Unsaved,
}

impl Group {
    pub const ALL: [Self; 5] = [
        Self::Conflicts,
        Self::Unstaged,
        Self::Staged,
        Self::Untracked,
        Self::Unsaved,
    ];
    pub fn title(self) -> &'static str {
        match self {
            Self::Conflicts => "Conflicts",
            Self::Unstaged => "Unstaged changes",
            Self::Staged => "Staged changes",
            Self::Untracked => "Untracked files",
            Self::Unsaved => "Unsaved buffers",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileKey {
    pub group: Group,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub key: FileKey,
    pub old_path: Option<PathBuf>,
    pub status: char,
    pub submodule: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// Includes the patch prefix (' ', '+', '-', or '\\').
    pub text: String,
    /// Zero-based position on the new side. Removals point to the boundary.
    pub line: usize,
    pub before_line: usize,
    /// Optional semantic spans, relative to the code after the patch prefix.
    /// Frontends can populate these on the query worker before publication.
    pub highlights: Vec<vex_editor::HighlightSpan>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    /// Content identity, independent of line numbers, for preserving expansion.
    pub id: (u64, usize),
    pub heading: String,
    pub line: usize,
    pub lines: Vec<Line>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Patch {
    pub info: Vec<String>,
    pub hunks: Vec<Hunk>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub root: PathBuf,
    pub branch: String,
    pub head: Option<String>,
    pub subject: String,
    pub upstream: Option<String>,
    pub ahead: Option<usize>,
    pub behind: Option<usize>,
    pub stashes: usize,
    pub files: Vec<Entry>,
    pub truncated: bool,
    pub patches: BTreeMap<FileKey, Patch>,
}

pub struct Request {
    pub origin: PathBuf,
    pub expanded: Vec<FileKey>,
}
pub struct Batch {
    pub request: u64,
    pub views: Vec<Request>,
    pub cancellation: Cancellation,
}
pub struct Result {
    pub request: u64,
    pub views: Vec<(PathBuf, std::result::Result<Snapshot, String>)>,
}

impl Batch {
    pub fn run(self) -> Option<Result> {
        let mut views = Vec::new();
        for view in self.views {
            if self.cancellation.is_cancelled() {
                return None;
            }
            let snapshot = read(&view, &self.cancellation);
            views.push((view.origin, snapshot));
        }
        (!self.cancellation.is_cancelled()).then_some(Result {
            request: self.request,
            views,
        })
    }
}

fn query(
    root: &Path,
    args: &[&OsStr],
    limit: usize,
    cancellation: &Cancellation,
) -> std::result::Result<Vec<u8>, String> {
    repository::output(root, args, limit, cancellation).ok_or_else(|| {
        "Git query failed, timed out, or exceeded its output limit; press r to retry".into()
    })
}

fn read(request: &Request, cancellation: &Cancellation) -> std::result::Result<Snapshot, String> {
    let root = repository::discover(&request.origin.join(".vex-status"), cancellation)
        .ok_or("No Git repository found, or Git is unavailable; press q to return")?;
    let args: Vec<_> = [
        "status",
        "--porcelain=v2",
        "-z",
        "--branch",
        "--show-stash",
        "--untracked-files=all",
        "--ignore-submodules=none",
        "--renames",
    ]
    .map(OsStr::new)
    .into();
    let mut snapshot = parse(&query(&root, &args, 4 << 20, cancellation)?)?;
    snapshot.root = root.clone();
    if let Some(head) = &snapshot.head {
        // Resolve the subject using the exact object reported by status.
        if let Some(bytes) = repository::output(
            &root,
            &[
                OsStr::new("show"),
                OsStr::new("-s"),
                OsStr::new("--format=%s"),
                OsStr::new("--no-show-signature"),
                OsStr::new(head),
                OsStr::new("--"),
            ],
            16 << 10,
            cancellation,
        ) {
            snapshot.subject = String::from_utf8_lossy(&bytes).trim_end().into();
        }
    }
    for key in request.expanded.iter().take(MAX_EXPANDED) {
        if cancellation.is_cancelled() {
            return Err("cancelled".into());
        }
        if let Some(entry) = snapshot.files.iter().find(|entry| entry.key == *key) {
            snapshot
                .patches
                .insert(key.clone(), patch(&root, entry, cancellation));
        }
    }
    Ok(snapshot)
}

fn safe_path(bytes: &[u8]) -> std::result::Result<PathBuf, String> {
    let path = repository::path(bytes.to_vec()).ok_or("Unsupported path encoding")?;
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Err("Invalid path in Git status".into());
    }
    Ok(path)
}

fn parse(bytes: &[u8]) -> std::result::Result<Snapshot, String> {
    let mut snapshot = Snapshot::default();
    let mut records = bytes.split(|b| *b == 0).filter(|record| !record.is_empty());
    for_record(&mut records, &mut snapshot)?;
    snapshot.files.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(snapshot)
}

fn for_record<'a>(
    records: &mut impl Iterator<Item = &'a [u8]>,
    snapshot: &mut Snapshot,
) -> std::result::Result<(), String> {
    while let Some(record) = records.next() {
        if let Some(header) = record.strip_prefix(b"# ") {
            let header = String::from_utf8_lossy(header);
            if let Some(value) = header.strip_prefix("branch.head ") {
                snapshot.branch = value.into();
            }
            if let Some(value) = header.strip_prefix("branch.oid ")
                && matches!(value.len(), 40 | 64)
                && value.bytes().all(|c| c.is_ascii_hexdigit())
            {
                snapshot.head = Some(value.into());
            }
            if let Some(value) = header.strip_prefix("branch.upstream ") {
                snapshot.upstream = Some(value.into());
            }
            if let Some(value) = header.strip_prefix("branch.ab ") {
                let mut parts = value.split_whitespace();
                snapshot.ahead = parts
                    .next()
                    .and_then(|s| s.strip_prefix('+'))
                    .and_then(|s| s.parse().ok());
                snapshot.behind = parts
                    .next()
                    .and_then(|s| s.strip_prefix('-'))
                    .and_then(|s| s.parse().ok());
            }
            if let Some(value) = header.strip_prefix("stash ") {
                snapshot.stashes = value.parse().unwrap_or(0);
            }
            continue;
        }
        let kind = record[0];
        if kind == b'?' {
            let path = safe_path(record.get(2..).ok_or("Invalid untracked record")?)?;
            push(
                snapshot,
                Entry {
                    key: FileKey {
                        group: Group::Untracked,
                        path,
                    },
                    old_path: None,
                    status: '?',
                    submodule: false,
                },
            );
            continue;
        }
        if kind == b'!' {
            continue;
        }
        let count = match kind {
            b'1' => 9,
            b'2' => 10,
            b'u' => 11,
            _ => return Err("Unsupported Git status record".into()),
        };
        let fields: Vec<_> = record.splitn(count, |b| *b == b' ').collect();
        if fields.len() != count || fields[1].len() != 2 {
            return Err("Invalid Git status record".into());
        }
        let path = safe_path(fields[count - 1])?;
        let old_path = if kind == b'2' {
            Some(safe_path(records.next().ok_or("Missing rename source")?)?)
        } else {
            None
        };
        let submodule = fields[2].starts_with(b"S");
        if kind == b'u' {
            push(
                snapshot,
                Entry {
                    key: FileKey {
                        group: Group::Conflicts,
                        path,
                    },
                    old_path,
                    status: 'U',
                    submodule,
                },
            );
        } else {
            for (column, group) in [Group::Staged, Group::Unstaged].into_iter().enumerate() {
                if fields[1][column] != b'.' {
                    push(
                        snapshot,
                        Entry {
                            key: FileKey {
                                group,
                                path: path.clone(),
                            },
                            old_path: old_path.clone(),
                            status: fields[1][column] as char,
                            submodule,
                        },
                    );
                }
            }
        }
    }
    Ok(())
}

fn push(snapshot: &mut Snapshot, entry: Entry) {
    if snapshot.files.len() < MAX_FILES {
        snapshot.files.push(entry);
    } else {
        snapshot.truncated = true;
    }
}

fn patch(root: &Path, entry: &Entry, cancellation: &Cancellation) -> Patch {
    if entry.key.group == Group::Untracked {
        return Patch {
            info: vec!["Untracked path — Enter opens the file".into()],
            ..Patch::default()
        };
    }
    let mut args: Vec<&OsStr> = [
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--no-relative",
        "--find-renames=50%",
        "-l100",
        "--unified=3",
        "--inter-hunk-context=0",
        "--submodule=short",
        "--src-prefix=a/",
        "--dst-prefix=b/",
    ]
    .map(OsStr::new)
    .into();
    if entry.key.group == Group::Staged {
        args.push(OsStr::new("--cached"));
    }
    if entry.key.group == Group::Conflicts {
        args.push(OsStr::new("--ours"));
    }
    args.extend([OsStr::new("--"), entry.key.path.as_os_str()]);
    if let Some(old) = &entry.old_path {
        args.push(old.as_os_str());
    }
    match query(root, &args, MAX_PATCH, cancellation) {
        Ok(bytes) => parse_patch(&bytes).unwrap_or_else(|error| Patch {
            info: vec![error],
            ..Patch::default()
        }),
        Err(error) => Patch {
            info: vec![error],
            ..Patch::default()
        },
    }
}

fn parse_patch(bytes: &[u8]) -> std::result::Result<Patch, String> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| "Non-UTF-8 diff; open the file to inspect it")?;
    if text.lines().count() > 4_000 {
        return Err("Diff exceeds the 4,000-line preview limit".into());
    }
    let mut patch = Patch::default();
    let mut files = 0;
    let mut position = 0usize;
    let mut before_position = 0usize;
    for line in text.lines() {
        if line.starts_with("diff --git ") {
            files += 1;
            if files > 1 {
                return Err(
                    "File identity changed while reading its diff; refresh to retry".into(),
                );
            }
        }
        if line.starts_with("@@ ") {
            before_position = line
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.strip_prefix('-'))
                .and_then(|s| s.split(',').next())
                .and_then(|s| s.parse::<usize>().ok())
                .ok_or("Invalid diff hunk")?
                .saturating_sub(1);
            let start = line
                .split_whitespace()
                .nth(2)
                .and_then(|s| s.strip_prefix('+'))
                .and_then(|s| s.split(',').next())
                .and_then(|s| s.parse::<usize>().ok())
                .ok_or("Invalid diff hunk")?;
            position = start.saturating_sub(1);
            patch.hunks.push(Hunk {
                id: (0, 0),
                heading: line.into(),
                line: position,
                lines: Vec::new(),
            });
        } else if let Some(hunk) = patch.hunks.last_mut() {
            if matches!(line.as_bytes().first(), Some(b' ' | b'+' | b'-' | b'\\')) {
                hunk.lines.push(Line {
                    text: line.into(),
                    line: position,
                    before_line: before_position,
                    highlights: Vec::new(),
                });
                if line.starts_with([' ', '-']) {
                    before_position = before_position.saturating_add(1);
                }
                if line.starts_with([' ', '+']) {
                    position = position.saturating_add(1);
                }
            } else if !line.is_empty() {
                patch.info.push(line.into());
            }
        } else if !["diff ", "index ", "--- ", "+++ "]
            .iter()
            .any(|prefix| line.starts_with(prefix))
            && !line.is_empty()
        {
            patch.info.push(line.into());
        }
    }
    let mut occurrences = BTreeMap::new();
    for hunk in &mut patch.hunks {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        for line in &hunk.lines {
            line.text.hash(&mut hash);
        }
        let fingerprint = hash.finish();
        let ordinal = occurrences.entry(fingerprint).or_insert(0);
        hunk.id = (fingerprint, *ordinal);
        *ordinal += 1;
    }
    if patch.hunks.is_empty() && patch.info.is_empty() {
        patch
            .info
            .push("No textual changes (file mode or repository state may have changed)".into());
    }
    Ok(patch)
}

/// Load the exact sides represented by a patch for syntax context. This runs on
/// the caller's worker; sources are discarded after producing semantic spans.
/// Plain diffs remain available if either side is binary, large, or unavailable.
pub fn sources(
    root: &Path,
    head: Option<&str>,
    entry: &Entry,
    cancellation: &Cancellation,
) -> Option<(String, String)> {
    use std::{fs::File, io::Read};
    const LIMIT: usize = 2 << 20;
    let content = |id: &str| {
        let size = repository::output(
            root,
            &[OsStr::new("cat-file"), OsStr::new("-s"), OsStr::new(id)],
            64,
            cancellation,
        )?;
        let size = std::str::from_utf8(&size)
            .ok()?
            .trim()
            .parse::<usize>()
            .ok()?;
        (size <= LIMIT)
            .then(|| repository::contents(root, id, cancellation))
            .flatten()
    };
    let index = |stage: u8| {
        let bytes = repository::output(
            root,
            &[
                OsStr::new("ls-files"),
                OsStr::new("--stage"),
                OsStr::new("-z"),
                OsStr::new("--"),
                entry.key.path.as_os_str(),
            ],
            64 << 10,
            cancellation,
        )?;
        for record in bytes.split(|b| *b == 0) {
            let header = record.split(|b| *b == b'\t').next()?;
            let fields: Vec<_> = header.split(|b| *b == b' ').collect();
            if fields.len() == 3 && fields[2] == [b'0' + stage] {
                return content(std::str::from_utf8(fields[1]).ok()?);
            }
        }
        None
    };
    let before = if entry.key.group == Group::Staged {
        if entry.status == 'A' || head.is_none() {
            String::new()
        } else {
            let path = root.join(entry.old_path.as_ref().unwrap_or(&entry.key.path));
            content(&repository::blob(root, head?, &path, cancellation)?)?
        }
    } else {
        index(if entry.key.group == Group::Conflicts {
            2
        } else {
            0
        })?
    };
    let after = if entry.status == 'D' {
        String::new()
    } else if entry.key.group == Group::Staged {
        index(0)?
    } else {
        let path = root.join(&entry.key.path);
        let metadata = std::fs::symlink_metadata(&path).ok()?;
        if !metadata.is_file() || metadata.len() > LIMIT as u64 {
            return None;
        }
        let mut file = File::open(path).ok()?.take(LIMIT as u64 + 1);
        let mut bytes = Vec::new();
        let mut chunk = [0; 8192];
        loop {
            if cancellation.is_cancelled() {
                return None;
            }
            let n = file.read(&mut chunk).ok()?;
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..n]);
        }
        if bytes.len() > LIMIT || bytes.contains(&0) {
            return None;
        }
        String::from_utf8(bytes).ok()?
    };
    (!cancellation.is_cancelled()).then_some((before, after))
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn porcelain_keeps_raw_paths_both_sides_renames_conflicts_and_headers() {
        let data=b"# branch.head main\0# branch.oid (initial)\0# branch.upstream origin/main\0# branch.ab +2 -3\0# future header\0# stash 4\x001 MM N... 100644 100644 100644 a b weird\n name\0? new dir/\x002 R. N... 100644 100644 100644 a b R100 new name\0old\tname\0u UU N... 100644 100644 100644 100644 a b c conflict\0";
        let snapshot = parse(data).unwrap();
        assert_eq!(snapshot.files.len(), 5);
        assert_eq!(snapshot.ahead, Some(2));
        assert_eq!(snapshot.behind, Some(3));
        assert_eq!(snapshot.stashes, 4);
        assert!(
            snapshot
                .files
                .iter()
                .any(|f| f.key.path == Path::new("weird\n name") && f.key.group == Group::Unstaged)
        );
        assert!(
            snapshot
                .files
                .iter()
                .any(|f| f.old_path.as_deref() == Some(Path::new("old\tname")))
        );
        assert_eq!(snapshot.files[0].key.group, Group::Conflicts);
        assert!(parse(b"? ../escape\0").is_err());
        assert!(parse(b"2 R.\0").is_err());
    }

    #[test]
    fn hunks_keep_navigation_and_identity_across_line_number_changes() {
        let a=parse_patch(b"diff --git a/x b/x\n@@ -2,2 +2,3 @@ function\n same\n-old\n+new\n+extra\n\\ No newline at end of file\n").unwrap();
        let b=parse_patch(b"@@ -12,2 +12,3 @@ function\n same\n-old\n+new\n+extra\n\\ No newline at end of file\n").unwrap();
        assert_eq!(a.hunks[0].id, b.hunks[0].id);
        assert_eq!(
            a.hunks[0].lines.iter().map(|l| l.line).collect::<Vec<_>>(),
            vec![1, 2, 2, 3, 4]
        );
        assert!(
            parse_patch(b"Binary files a/x and b/x differ\n")
                .unwrap()
                .hunks
                .is_empty()
        );
    }

    #[test]
    fn real_repository_separates_index_disk_and_untracked_and_supports_unborn() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.name", "Vex Test"]);
        git(root, &["config", "user.email", "vex@example.invalid"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join("file [界].txt"), "base\n").unwrap();
        let mut request = Request {
            origin: root.into(),
            expanded: vec![],
        };
        let c = Cancellation::default();
        assert_eq!(
            read(&request, &c).unwrap().files[0].key.group,
            Group::Untracked
        );
        git(root, &["add", "."]);
        request.expanded = vec![FileKey {
            group: Group::Staged,
            path: "file [界].txt".into(),
        }];
        assert_eq!(
            read(&request, &c)
                .unwrap()
                .patches
                .values()
                .next()
                .unwrap()
                .hunks
                .len(),
            1
        );
        git(root, &["commit", "-qm", "initial"]);
        fs::write(root.join("file [界].txt"), "staged\n").unwrap();
        git(root, &["add", "."]);
        fs::write(root.join("file [界].txt"), "disk\n").unwrap();
        request.expanded.push(FileKey {
            group: Group::Unstaged,
            path: "file [界].txt".into(),
        });
        let result = read(&request, &c).unwrap();
        assert_eq!(result.files.len(), 2);
        let staged = &result.patches[&request.expanded[0]].hunks[0].lines;
        assert!(staged.iter().any(|line| line.text == "+staged"));
        let unstaged = &result.patches[&request.expanded[1]].hunks[0].lines;
        assert!(unstaged.iter().any(|line| line.text == "-staged"));
        assert!(unstaged.iter().any(|line| line.text == "+disk"));
        assert_eq!(
            fs::read_to_string(root.join("file [界].txt")).unwrap(),
            "disk\n"
        );
        c.cancel();
        assert!(
            Batch {
                request: 1,
                views: vec![request],
                cancellation: c
            }
            .run()
            .is_none()
        );
    }
}
