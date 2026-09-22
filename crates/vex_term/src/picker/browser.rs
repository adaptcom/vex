//! One-directory catalogs and previews, prepared on the shared picker workers.

use super::{Entry, Preview, catalog, ignore};
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};
use vex_editor::background::Cancellation;

const MAX_ENTRIES: usize = 200_000;
const MAX_BYTES: usize = 64 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub path: PathBuf,
    pub directory: bool,
}

pub(crate) struct Job {
    pub session: u64,
    pub revision: u64,
    pub directory: PathBuf,
    pub query: String,
    pub cancellation: Cancellation,
}

pub(crate) struct Result {
    pub directory: Option<PathBuf>,
    pub ranked: catalog::Result<Target>,
}

#[derive(Default)]
pub(crate) struct Worker {
    cached: Option<(u64, PathBuf, Listing)>,
}

struct Listing {
    directory: Option<PathBuf>,
    entries: Arc<[catalog::CatalogEntry<Target>]>,
    notice: String,
}

impl Worker {
    pub fn run(&mut self, job: Job) -> Option<Result> {
        if job.cancellation.is_cancelled() {
            return None;
        }
        if !self
            .cached
            .as_ref()
            .is_some_and(|(session, path, listing)| {
                *session == job.session
                    && (*path == job.directory
                        || listing.directory.as_ref() == Some(&job.directory))
            })
        {
            let listing =
                listing(&job.directory, &job.cancellation).unwrap_or_else(|error| Listing {
                    directory: None,
                    entries: Arc::from([]),
                    notice: format!("Cannot browse: {error}"),
                });
            if job.cancellation.is_cancelled() {
                return None;
            }
            self.cached = Some((job.session, job.directory, listing));
        }
        let listing = &self.cached.as_ref().unwrap().2;
        let mut result = catalog::Job {
            session: job.session,
            revision: job.revision,
            catalog: listing.entries.clone(),
            query: job.query,
            cancellation: job.cancellation,
        }
        .run()?;
        // Keep fuzzy relevance within each group, with directories first.
        result.items.sort_by_key(|item| !item.entry.value.directory);
        result.notice.clone_from(&listing.notice);
        Some(Result {
            directory: listing.directory.clone(),
            ranked: result,
        })
    }
}

fn listing(path: &Path, cancellation: &Cancellation) -> io::Result<Listing> {
    let canonical = fs::canonicalize(path)?;
    let path = canonical.as_path();
    let directory = fs::read_dir(path)?;
    let mut ancestors = Vec::new();
    for ancestor in path.ancestors().take(64) {
        if cancellation.is_cancelled() {
            return Err(io::Error::other("cancelled"));
        }
        ancestors.push(ancestor);
        if ancestor.join(".git").exists() {
            break;
        }
    }
    let mut notice = String::new();
    let mut rules = None;
    for ancestor in ancestors.into_iter().rev() {
        if cancellation.is_cancelled() {
            return Err(io::Error::other("cancelled"));
        }
        rules = Some(ignore::Rules::load(ancestor, rules, true, &mut notice));
    }
    let rules = rules.unwrap();
    let mut scratch = ignore::Scratch::default();
    let mut entries = Vec::new();
    let mut bytes = 0;
    for (scanned, entry) in directory.enumerate() {
        if cancellation.is_cancelled() {
            return Err(io::Error::other("cancelled"));
        }
        if scanned == MAX_ENTRIES || bytes >= MAX_BYTES {
            notice = "Directory listing limit reached".into();
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                notice = error.to_string();
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let kind = match entry.file_type() {
            Ok(kind) => kind,
            Err(error) => {
                notice = error.to_string();
                continue;
            }
        };
        if !kind.is_file() && !kind.is_dir() {
            continue;
        }
        let target = Target {
            path: entry.path(),
            directory: kind.is_dir(),
        };
        if rules.check(&target.path, target.directory, &mut scratch, cancellation) != Some(false) {
            continue;
        }
        // Render control characters literally rather than as extra preview rows.
        let mut label = String::with_capacity(name.len() + usize::from(target.directory));
        for ch in name.chars() {
            if ch.is_control() {
                label.extend(ch.escape_default());
            } else {
                label.push(ch);
            }
        }
        if target.directory {
            label.push('/');
        }
        bytes += target.path.as_os_str().len()
            + label.len()
            + std::mem::size_of::<catalog::CatalogEntry<Target>>();
        entries.push(catalog::CatalogEntry {
            entry: Arc::new(Entry {
                label,
                value: target,
            }),
            accessed: 0,
        });
    }
    entries.sort_by(|a, b| {
        (
            !a.entry.value.directory,
            &a.entry.label,
            &a.entry.value.path,
        )
            .cmp(&(
                !b.entry.value.directory,
                &b.entry.label,
                &b.entry.value.path,
            ))
    });
    Ok(Listing {
        directory: Some(canonical),
        entries: entries.into(),
        notice,
    })
}

pub(super) fn preview(path: &Path, cancellation: &Cancellation) -> io::Result<Preview> {
    let listing = listing(path, cancellation)?;
    let mut text = String::new();
    let mut shown = 0;
    for entry in listing.entries.iter().take(200) {
        if text.len() + entry.entry.label.len() + 1 > 64 << 10 {
            break;
        }
        text.push_str(&entry.entry.label);
        text.push('\n');
        shown += 1;
    }
    if shown < listing.entries.len() {
        text.push_str("… preview truncated\n");
    } else if listing.entries.is_empty() && listing.notice.is_empty() {
        text.push_str("No visible entries\n");
    }
    if !listing.notice.is_empty() {
        text.push_str(&listing.notice);
    }
    Ok(Preview::plain(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(path: &Path, query: &str) -> Job {
        Job {
            session: 1,
            revision: 1,
            directory: path.into(),
            query: query.into(),
            cancellation: Cancellation::default(),
        }
    }

    #[test]
    fn lists_children_including_dotfiles_with_directories_first_and_ignore_rules() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".git")).unwrap();
        fs::write(root.path().join(".gitignore"), "*.log\nignored/\n!.git\n").unwrap();
        let path = root.path().join("src");
        fs::create_dir(&path).unwrap();
        for name in ["zebra", "ignored", ".hidden"] {
            fs::create_dir(path.join(name)).unwrap();
        }
        for name in [
            "alpha.rs",
            "界 file.rs",
            "debug.log",
            "keep.log",
            ".hidden.rs",
        ] {
            fs::write(path.join(name), "contents").unwrap();
        }
        fs::write(path.join(".ignore"), "!keep.log\n").unwrap();
        fs::write(path.join("zebra/nested.rs"), "nested").unwrap();
        fs::write(path.join("zebra/.git"), "gitdir: elsewhere\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&path, path.join("cycle")).unwrap();
        let mut worker = Worker::default();
        let result = worker.run(job(&path, "")).unwrap();
        assert_eq!(
            result
                .ranked
                .items
                .iter()
                .map(|item| item.entry.label.as_str())
                .collect::<Vec<_>>(),
            [
                ".hidden/",
                "zebra/",
                ".hidden.rs",
                ".ignore",
                "alpha.rs",
                "keep.log",
                "界 file.rs"
            ]
        );
        assert_eq!(result.ranked.total, 7);
        assert!(result.ranked.items[0].entry.value.directory);
        let result = worker.run(job(&path, "界 f")).unwrap();
        assert_eq!(result.ranked.items.len(), 1);
        assert_eq!(
            result.ranked.items[0].entry.value.path,
            path.join("界 file.rs").canonicalize().unwrap()
        );
        assert!(!result.ranked.items[0].matched.is_empty());
        assert_eq!(
            preview(&path, &Cancellation::default()).unwrap().text,
            ".hidden/\nzebra/\n.hidden.rs\n.ignore\nalpha.rs\nkeep.log\n界 file.rs\n"
        );
        let result = worker.run(job(root.path(), "")).unwrap();
        assert_eq!(
            result
                .ranked
                .items
                .iter()
                .map(|item| item.entry.label.as_str())
                .collect::<Vec<_>>(),
            ["src/", ".gitignore"]
        );
        assert_eq!(
            preview(root.path(), &Cancellation::default()).unwrap().text,
            "src/\n.gitignore\n"
        );
        assert_eq!(
            preview(&path.join("zebra"), &Cancellation::default())
                .unwrap()
                .text,
            "nested.rs\n"
        );
    }

    #[test]
    fn filters_beyond_the_result_cap_and_refreshes_on_directory_or_session_changes() {
        let root = tempfile::tempdir().unwrap();
        for i in 0..520 {
            fs::write(root.path().join(format!("file{i:03}")), "").unwrap();
        }
        let mut worker = Worker::default();
        let result = worker.run(job(root.path(), "")).unwrap();
        assert_eq!(result.ranked.items.len(), super::super::files::MAX_RESULTS);
        assert_eq!(result.ranked.total, 520);
        let result = worker.run(job(root.path(), "file519")).unwrap();
        assert_eq!(result.ranked.items[0].entry.label, "file519");
        assert!(
            preview(root.path(), &Cancellation::default())
                .unwrap()
                .text
                .ends_with("… preview truncated\n")
        );
        fs::write(root.path().join("new"), "").unwrap();
        assert_eq!(
            worker.run(job(root.path(), "new")).unwrap().ranked.matched,
            0
        );
        let mut fresh = job(root.path(), "new");
        fresh.session += 1;
        assert_eq!(worker.run(fresh).unwrap().ranked.matched, 1);
        let cancelled = job(root.path(), "");
        cancelled.cancellation.cancel();
        assert!(worker.run(cancelled).is_none());
    }

    #[test]
    fn empty_and_missing_directories_report_status_and_control_characters_stay_on_one_row() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            preview(root.path(), &Cancellation::default()).unwrap().text,
            "No visible entries\n"
        );
        let mut worker = Worker::default();
        let result = worker.run(job(&root.path().join("missing"), "")).unwrap();
        assert!(result.ranked.items.is_empty());
        assert!(result.ranked.notice.starts_with("Cannot browse:"));
        #[cfg(unix)]
        {
            fs::write(root.path().join("a\nb\t.rs"), "").unwrap();
            let result = worker.run(job(root.path(), "")).unwrap();
            assert_eq!(result.ranked.items[0].entry.label, "a\\nb\\t.rs");
            assert_eq!(
                preview(root.path(), &Cancellation::default())
                    .unwrap()
                    .text
                    .lines()
                    .count(),
                1
            );
        }
    }
}
