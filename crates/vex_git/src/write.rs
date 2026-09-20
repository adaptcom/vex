//! Ordered Git mutations. The caller must execute jobs serially and deliver
//! every completion. A refresh or closed view must never cancel a write.

use crate::status::{Entry, Group};
use std::{
    ffi::OsStr,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
};

#[derive(Clone, Debug)]
pub enum Operation {
    Stage(Entry),
    Unstage(Entry),
    Commit { message: String },
}

impl Operation {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Stage(_) => "Stage",
            Self::Unstage(_) => "Unstage",
            Self::Commit { .. } => "Commit",
        }
    }
}

pub struct Job {
    pub id: u64,
    pub root: PathBuf,
    pub operation: Operation,
}

pub struct Result {
    pub id: u64,
    pub root: PathBuf,
    pub operation: Operation,
    pub outcome: std::result::Result<String, String>,
}

impl Job {
    pub fn run(self) -> Result {
        let outcome = execute(&self.root, &self.operation).map_err(|error| error.to_string());
        Result {
            id: self.id,
            root: self.root,
            operation: self.operation,
            outcome,
        }
    }
}

struct Output {
    status: ExitStatus,
    text: String,
}

/// Hooks and signing may take arbitrarily long. Writes finish before shutdown;
/// they do not inherit read queries' short timeout or cancellation token.
fn run(root: &Path, args: &[&OsStr]) -> io::Result<Output> {
    let mut log = tempfile::tempfile()?;
    let mut command = Command::new("git");
    command
        .args(["--no-pager", "-C"])
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("GIT_EDITOR", ":")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log.try_clone()?));
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
    ] {
        command.env_remove(name);
    }
    let status = command.status()?;
    let length = log.metadata()?.len();
    log.seek(SeekFrom::Start(length.saturating_sub(64 << 10)))?;
    let mut bytes = Vec::new();
    log.take(64 << 10).read_to_end(&mut bytes)?;
    let mut text = String::from_utf8_lossy(&bytes).trim().to_owned();
    if length > 64 << 10 {
        text.insert_str(0, "[earlier output omitted]\n");
    }
    Ok(Output { status, text })
}

fn checked(root: &Path, args: &[&OsStr]) -> io::Result<String> {
    let output = run(root, args)?;
    if output.status.success() {
        Ok(output.text)
    } else {
        Err(io::Error::other(if output.text.is_empty() {
            format!("Git exited with {}", output.status)
        } else {
            output.text
        }))
    }
}

fn execute(root: &Path, operation: &Operation) -> io::Result<String> {
    if let Operation::Commit { message } = operation {
        if message.trim().is_empty() {
            return Err(io::Error::other("commit message is empty"));
        }
        let mut file = tempfile::NamedTempFile::new()?;
        file.write_all(message.as_bytes())?;
        file.flush()?;
        return checked(
            root,
            &[
                OsStr::new("commit"),
                OsStr::new("--file"),
                file.path().as_os_str(),
            ],
        );
    }
    let (entry, stage) = match operation {
        Operation::Stage(entry) => (entry, true),
        Operation::Unstage(entry) => (entry, false),
        _ => unreachable!(),
    };
    if (stage && !matches!(entry.key.group, Group::Unstaged | Group::Untracked))
        || (!stage && entry.key.group != Group::Staged)
    {
        return Err(io::Error::other(
            "select an unstaged/untracked file to stage, or a staged file to unstage",
        ));
    }
    let mut paths = vec![entry.key.path.as_path()];
    if entry.status == 'R'
        && let Some(old) = &entry.old_path
    {
        paths.push(old);
    }
    if paths.iter().any(|path| {
        path.as_os_str().is_empty()
            || path
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
    }) {
        return Err(io::Error::other("invalid repository path"));
    }
    if stage
        && !entry.submodule
        && paths.iter().any(|path| {
            std::fs::symlink_metadata(root.join(path)).is_ok_and(|metadata| metadata.is_dir())
        })
    {
        return Err(io::Error::other(
            "file became a directory; refresh status before staging",
        ));
    }
    let mut args: Vec<&OsStr> = if stage {
        ["add", "--all", "--"].map(OsStr::new).into()
    } else {
        let head = run(
            root,
            &[
                OsStr::new("rev-parse"),
                OsStr::new("--verify"),
                OsStr::new("--quiet"),
                OsStr::new("HEAD"),
            ],
        )?;
        if head.status.success() {
            ["restore", "--staged", "--"].map(OsStr::new).into()
        } else if head.status.code() == Some(1) {
            // Verify an unborn branch, rather than treating an arbitrary Git
            // failure as permission to remove entries from the index.
            checked(
                root,
                &[
                    OsStr::new("symbolic-ref"),
                    OsStr::new("--quiet"),
                    OsStr::new("HEAD"),
                ],
            )?;
            ["rm", "--cached", "--force", "--"].map(OsStr::new).into()
        } else {
            return Err(io::Error::other(head.text));
        }
    };
    args.extend(paths.iter().map(|path| path.as_os_str()));
    checked(root, &args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::FileKey;
    use std::fs;

    fn git(root: &Path, args: &[&str]) -> String {
        checked(root, &args.iter().map(OsStr::new).collect::<Vec<_>>()).unwrap()
    }
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["config", "user.name", "Vex Test"]);
        git(dir.path(), &["config", "user.email", "vex@example.invalid"]);
        git(dir.path(), &["config", "commit.gpgsign", "false"]);
        git(
            dir.path(),
            &[
                "config",
                "core.hooksPath",
                dir.path().join(".git/hooks").to_str().unwrap(),
            ],
        );
        dir
    }
    fn entry(path: &str, group: Group, status: char) -> Entry {
        Entry {
            key: FileKey {
                group,
                path: path.into(),
            },
            old_path: None,
            status,
            submodule: false,
        }
    }
    fn commit(root: &Path, message: &str) -> io::Result<String> {
        execute(
            root,
            &Operation::Commit {
                message: message.into(),
            },
        )
    }

    #[test]
    fn stage_and_unstage_are_literal_and_preserve_disk_including_before_first_commit() {
        let dir = repo();
        let root = dir.path();
        let name = ":(glob)* [界]\nfile.txt";
        fs::write(root.join(name), "saved\n").unwrap();
        fs::write(root.join("unrelated"), "leave alone").unwrap();
        execute(root, &Operation::Stage(entry(name, Group::Untracked, '?'))).unwrap();
        let indexed = git(root, &["ls-files", "-z"]);
        assert_eq!(indexed, format!("{name}\0"));
        execute(root, &Operation::Unstage(entry(name, Group::Staged, 'A'))).unwrap();
        assert!(git(root, &["ls-files"]).is_empty());
        assert_eq!(fs::read_to_string(root.join(name)).unwrap(), "saved\n");
        execute(root, &Operation::Stage(entry(name, Group::Untracked, '?'))).unwrap();
        commit(root, "First commit\n\nBody").unwrap();
        assert_eq!(
            git(root, &["log", "-1", "--format=%B"]),
            "First commit\n\nBody"
        );
        fs::write(root.join(name), "new disk text\n").unwrap();
        execute(root, &Operation::Stage(entry(name, Group::Unstaged, 'M'))).unwrap();
        execute(root, &Operation::Unstage(entry(name, Group::Staged, 'M'))).unwrap();
        assert!(git(root, &["diff", "--cached", "--name-only"]).is_empty());
        assert_eq!(
            fs::read_to_string(root.join(name)).unwrap(),
            "new disk text\n"
        );
    }

    #[test]
    fn unstaging_renames_and_staging_deletions_preserve_working_files() {
        let dir = repo();
        let root = dir.path();
        fs::write(root.join("old"), "base\n").unwrap();
        git(root, &["add", "old"]);
        commit(root, "base").unwrap();
        git(root, &["mv", "old", "new"]);
        let mut renamed = entry("new", Group::Staged, 'R');
        renamed.old_path = Some("old".into());
        execute(root, &Operation::Unstage(renamed)).unwrap();
        assert!(git(root, &["diff", "--cached", "--name-only"]).is_empty());
        assert!(!root.join("old").exists());
        assert_eq!(fs::read_to_string(root.join("new")).unwrap(), "base\n");
        execute(root, &Operation::Stage(entry("old", Group::Unstaged, 'D'))).unwrap();
        assert!(git(root, &["ls-files"]).is_empty());
        execute(root, &Operation::Unstage(entry("old", Group::Staged, 'D'))).unwrap();
        assert_eq!(git(root, &["ls-files"]), "old");
        assert!(!root.join("old").exists());
    }

    #[test]
    fn commit_uses_only_the_index_and_errors_leave_it_intact() {
        let dir = repo();
        let root = dir.path();
        fs::write(root.join("file"), "staged\n").unwrap();
        git(root, &["add", "file"]);
        fs::write(root.join("file"), "unstaged\n").unwrap();
        let index = fs::read(root.join(".git/index")).unwrap();
        assert!(commit(root, " \n").is_err());
        assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
        commit(root, "subject\n\nbody with ' quotes and $() and `literal`").unwrap();
        assert_eq!(git(root, &["show", "HEAD:file"]), "staged");
        assert_eq!(fs::read_to_string(root.join("file")).unwrap(), "unstaged\n");
        let head = git(root, &["rev-parse", "HEAD"]);
        assert!(commit(root, "nothing staged").is_err());
        assert_eq!(git(root, &["rev-parse", "HEAD"]), head);
        assert!(
            execute(
                root,
                &Operation::Stage(entry("../outside", Group::Untracked, '?'))
            )
            .is_err()
        );
    }

    #[test]
    #[cfg(unix)]
    fn failed_hooks_report_output_and_keep_index_and_head() {
        use std::os::unix::fs::PermissionsExt;
        let dir = repo();
        let root = dir.path();
        fs::write(root.join("file"), "staged\n").unwrap();
        git(root, &["add", "file"]);
        commit(root, "base").unwrap();
        fs::write(root.join("file"), "changed\n").unwrap();
        git(root, &["add", "file"]);
        let head = git(root, &["rev-parse", "HEAD"]);
        let hook = root.join(".git/hooks/pre-commit");
        fs::write(
            &hook,
            "#!/bin/sh\nprintf 'hook says no\\nsecond line\\n' >&2\nexit 1\n",
        )
        .unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        let error = commit(root, "try commit").unwrap_err().to_string();
        assert!(error.contains("hook says no\nsecond line"));
        assert_eq!(git(root, &["rev-parse", "HEAD"]), head);
        assert_eq!(git(root, &["show", ":file"]), "changed");
    }
}
