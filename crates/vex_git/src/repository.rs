//! Read-only Git plumbing. Git owns repository discovery and object decoding;
//! commands never run on the UI thread and are bounded and cancellable.

use crate::MAX_BYTES;
use std::{
    ffi::{OsStr, OsString},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use vex_editor::background::Cancellation;

struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn output(
    directory: &Path,
    arguments: &[&OsStr],
    limit: usize,
    cancellation: &Cancellation,
) -> Option<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .args([
            "--no-pager",
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "-C",
        ])
        .arg(directory)
        .args(arguments)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_LITERAL_PATHSPECS", "1");
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
    ] {
        command.env_remove(name);
    }
    bounded_output(&mut command, limit, Duration::from_secs(2), cancellation)
}

fn bounded_output(
    command: &mut Command,
    limit: usize,
    timeout: Duration,
    cancellation: &Cancellation,
) -> Option<Vec<u8>> {
    if cancellation.is_cancelled() {
        return None;
    }
    // A regular temporary file avoids both pipe backpressure and reader threads.
    let mut output = tempfile::tempfile().ok()?;
    command
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::from(output.try_clone().ok()?));
    let mut child = Running(command.spawn().ok()?);
    let deadline = Instant::now() + timeout;
    loop {
        if cancellation.is_cancelled()
            || Instant::now() >= deadline
            || output.metadata().ok()?.len() > limit as u64
        {
            return None;
        }
        if let Some(status) = child.0.try_wait().ok()? {
            if !status.success() {
                return None;
            }
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    output.seek(SeekFrom::Start(0)).ok()?;
    let mut bytes = Vec::new();
    output.take(limit as u64 + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() <= limit).then_some(bytes)
}

fn path(bytes: Vec<u8>) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(OsString::from_vec(bytes).into())
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes).ok().map(PathBuf::from)
    }
}

pub(crate) fn discover(file: &Path, cancellation: &Cancellation) -> Option<PathBuf> {
    let mut bytes = output(
        file.parent()?,
        &[OsStr::new("rev-parse"), OsStr::new("--show-toplevel")],
        64 << 10,
        cancellation,
    )?;
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    path(bytes)?.canonicalize().ok()
}

fn object_id(bytes: &[u8]) -> Option<String> {
    let id = std::str::from_utf8(bytes).ok()?.trim();
    (matches!(id.len(), 40 | 64) && id.bytes().all(|b| b.is_ascii_hexdigit())).then(|| id.into())
}

pub(crate) fn head(root: &Path, cancellation: &Cancellation) -> Option<String> {
    object_id(&output(
        root,
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new("HEAD^{commit}"),
        ],
        256,
        cancellation,
    )?)
}

pub(crate) fn blob(
    root: &Path,
    head: &str,
    file: &Path,
    cancellation: &Cancellation,
) -> Option<String> {
    let relative = file.strip_prefix(root).ok()?;
    let bytes = output(
        root,
        &[
            OsStr::new("ls-tree"),
            OsStr::new("--full-tree"),
            OsStr::new("-z"),
            OsStr::new(head),
            OsStr::new("--"),
            relative.as_os_str(),
        ],
        64 << 10,
        cancellation,
    )?;
    let header = bytes.split(|b| *b == b'\t').next()?;
    let fields: Vec<_> = header.split(|b| *b == b' ').collect();
    if fields.len() != 3 || !matches!(fields[0], b"100644" | b"100755") || fields[1] != b"blob" {
        return None;
    }
    object_id(fields[2])
}

pub(crate) fn contents(root: &Path, blob: &str, cancellation: &Cancellation) -> Option<String> {
    let size = output(
        root,
        &[OsStr::new("cat-file"), OsStr::new("-s"), OsStr::new(blob)],
        64,
        cancellation,
    )?;
    let size = std::str::from_utf8(&size)
        .ok()?
        .trim()
        .parse::<usize>()
        .ok()?;
    if size > MAX_BYTES {
        return None;
    }
    let bytes = output(
        root,
        &[OsStr::new("cat-file"), OsStr::new("blob"), OsStr::new(blob)],
        MAX_BYTES,
        cancellation,
    )?;
    if bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancelled_commands_never_start() {
        let cancellation = Cancellation::default();
        cancellation.cancel();
        assert!(discover(Path::new("/not-a-repository/file"), &cancellation).is_none());
    }

    #[test]
    #[cfg(unix)]
    fn command_output_failures_and_deadlines_are_bounded() {
        let cancellation = Cancellation::default();
        let run = |script: &str, limit| {
            bounded_output(
                Command::new("sh").args(["-c", script]),
                limit,
                Duration::from_secs(2),
                &cancellation,
            )
        };
        assert_eq!(run("printf hello", 5), Some(b"hello".to_vec()));
        assert_eq!(run("printf hello", 4), None);
        assert_eq!(run("printf hello; exit 1", 5), None);
        assert_eq!(
            bounded_output(
                &mut Command::new("/nonexistent/vex-git-test"),
                10,
                Duration::from_secs(1),
                &cancellation
            ),
            None
        );
        let start = Instant::now();
        assert_eq!(
            bounded_output(
                Command::new("sh").args(["-c", "exec sleep 10"]),
                5,
                Duration::from_millis(50),
                &cancellation,
            ),
            None
        );
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    #[cfg(unix)]
    fn cancellation_stops_a_running_command() {
        let dir = tempfile::tempdir().unwrap();
        let started = dir.path().join("started");
        let cancellation = Cancellation::default();
        thread::scope(|scope| {
            let job = scope.spawn(|| {
                bounded_output(
                    Command::new("sh")
                        .args(["-c", "printf started > \"$1\"; exec sleep 10", "vex-test"])
                        .arg(&started),
                    10,
                    Duration::from_secs(10),
                    &cancellation,
                )
            });
            let deadline = Instant::now() + Duration::from_secs(2);
            while !started.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            let was_started = started.exists();
            let cancel_at = Instant::now();
            cancellation.cancel();
            assert_eq!(job.join().unwrap(), None);
            assert!(was_started);
            assert!(cancel_at.elapsed() < Duration::from_secs(5));
        });
    }
}
