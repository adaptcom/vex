//! Serialized clipboard helpers and cancellable copy/paste preparation.
//! Only the worker accesses platform processes; jobs retain immutable snapshots.

use std::{
    io::{self, Read, Seek, SeekFrom, Write},
    num::NonZeroUsize,
    process::{Child, Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use vex_core::{SelectionSet, Snapshot, Transaction};
use vex_editor::{ClipboardKind, Paste, PastePlan, background::Cancellation};

const MAX_BYTES: usize = 128 << 20;
const MAX_EDIT_BYTES: usize = 256 << 20;
const TIMEOUT: Duration = Duration::from_secs(2);
const SEPARATOR: &str = if cfg!(windows) { "\r\n" } else { "\n" };
type Fragments = Arc<[Arc<str>]>;

pub(crate) enum Operation {
    Copy {
        snapshot: Snapshot,
        selections: SelectionSet,
        cut: Option<PastePlan>,
    },
    Paste {
        plan: PastePlan,
        placement: Paste,
        count: NonZeroUsize,
        selections: usize,
    },
    Read {
        limit: usize,
        trim_controls: bool,
        truncate: bool,
    },
    Write {
        values: Fragments,
    },
}

pub(crate) struct Job {
    pub id: u64,
    pub kind: ClipboardKind,
    pub operation: Operation,
    pub cancellation: Cancellation,
}

pub(crate) enum Outcome {
    Copied(usize),
    Paste(Transaction),
    Text(Arc<str>),
}

pub(crate) struct Result {
    pub id: u64,
    pub outcome: std::result::Result<Outcome, String>,
}

#[derive(Default)]
pub(crate) struct Worker {
    slots: [Slot; 2],
}

#[derive(Default)]
struct Slot {
    provider: Option<Provider>,
    saved: Option<Fragments>,
}

impl Worker {
    pub fn run(&mut self, job: Job) -> Option<Result> {
        let index = match job.kind {
            ClipboardKind::System => 0,
            ClipboardKind::Primary => 1,
        };
        let outcome = self.slots[index]
            .execute(job.kind, job.operation, &job.cancellation)
            .map_err(|e| e.to_string());
        (!job.cancellation.is_cancelled()).then_some(Result {
            id: job.id,
            outcome,
        })
    }
}

impl Slot {
    fn execute(
        &mut self,
        kind: ClipboardKind,
        operation: Operation,
        cancellation: &Cancellation,
    ) -> io::Result<Outcome> {
        check(cancellation)?;
        if self.provider.is_none() {
            self.provider = Some(Provider::detect(kind)?);
        }
        match operation {
            Operation::Copy {
                snapshot,
                selections,
                cut,
            } => {
                let values = capture(&snapshot, &selections, cancellation)?;
                // Construct the edit before writing: failures must not delete
                // text, and an invalid edit should not overwrite the clipboard.
                let transaction = cut
                    .map(|plan| {
                        plan.prepare(&[Arc::from("")], Paste::Replace, NonZeroUsize::MIN, &|| {
                            cancellation.is_cancelled()
                        })
                    })
                    .transpose()
                    .map_err(io::Error::other)?;
                let len = values.len();
                self.write(values, cancellation)?;
                Ok(transaction.map_or(Outcome::Copied(len), Outcome::Paste))
            }
            Operation::Paste {
                plan,
                placement,
                count,
                selections,
            } => {
                let values = self.read(cancellation)?;
                let mut size = 0usize;
                for index in 0..selections {
                    check(cancellation)?;
                    // Reserve room for LF-to-CRLF conversion before allocating
                    // counted text or multiplying it across destination carets.
                    size = values[index.min(values.len() - 1)]
                        .len()
                        .checked_mul(count.get())
                        .and_then(|len| len.checked_mul(2))
                        .and_then(|len| size.checked_add(len))
                        .filter(|&len| len <= MAX_EDIT_BYTES)
                        .ok_or_else(|| io::Error::other("clipboard paste exceeds 256 MiB"))?;
                }
                let transaction = plan
                    .prepare(&values, placement, count, &|| cancellation.is_cancelled())
                    .map_err(io::Error::other)?;
                check(cancellation)?;
                Ok(Outcome::Paste(transaction))
            }
            Operation::Read {
                limit,
                trim_controls,
                truncate,
            } => {
                let values = self.read(cancellation)?;
                let text = values.first().map_or("", AsRef::as_ref);
                if !trim_controls && !truncate {
                    if text.len() > limit {
                        return Err(io::Error::other("clipboard regex exceeds 64 KiB"));
                    }
                    return Ok(Outcome::Text(
                        values.first().cloned().unwrap_or_else(|| Arc::from("")),
                    ));
                }
                let mut result = String::with_capacity(text.len().min(limit));
                for (index, ch) in text.chars().enumerate() {
                    if index % 1024 == 0 {
                        check(cancellation)?;
                    }
                    if trim_controls && ch.is_control() {
                        continue;
                    }
                    if result.len() + ch.len_utf8() > limit {
                        if truncate {
                            break;
                        }
                        return Err(io::Error::other("clipboard text exceeds prompt size limit"));
                    }
                    result.push(ch);
                }
                Ok(Outcome::Text(result.into()))
            }
            Operation::Write { values } => {
                let count = values.len();
                self.write(values, cancellation)?;
                Ok(Outcome::Copied(count))
            }
        }
    }

    fn write(&mut self, values: Fragments, cancellation: &Cancellation) -> io::Result<()> {
        let mut size = values
            .len()
            .saturating_sub(1)
            .saturating_mul(SEPARATOR.len());
        let mut input = tempfile::tempfile()?;
        for (index, value) in values.iter().enumerate() {
            size = size
                .checked_add(value.len())
                .filter(|&size| size <= MAX_BYTES)
                .ok_or_else(|| io::Error::other("clipboard copy exceeds 128 MiB"))?;
            if index > 0 {
                input.write_all(SEPARATOR.as_bytes())?;
            }
            for chunk in value.as_bytes().chunks(8192) {
                check(cancellation)?;
                input.write_all(chunk)?;
            }
        }
        input.seek(SeekFrom::Start(0))?;
        run(
            &mut self.provider.as_ref().unwrap().write.command(),
            Some(input),
            cancellation,
            TIMEOUT,
        )?;
        // Keep fragments, never the source snapshot. Reads compare all bytes.
        self.saved = Some(values);
        Ok(())
    }

    fn read(&self, cancellation: &Cancellation) -> io::Result<Fragments> {
        let bytes = run(
            &mut self.provider.as_ref().unwrap().read.command(),
            None,
            cancellation,
            TIMEOUT,
        )?;
        let text = String::from_utf8(bytes)
            .map_err(|_| io::Error::other("clipboard contains invalid UTF-8"))?;
        Ok(self
            .saved
            .as_ref()
            .filter(|values| matches(values, &text, cancellation))
            .cloned()
            .unwrap_or_else(|| Arc::from([Arc::from(text)])))
    }
}

fn check(cancellation: &Cancellation) -> io::Result<()> {
    if cancellation.is_cancelled() {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "clipboard operation cancelled",
        ))
    } else {
        Ok(())
    }
}

fn capture(
    snapshot: &Snapshot,
    selections: &SelectionSet,
    cancellation: &Cancellation,
) -> io::Result<Fragments> {
    let mut size = selections
        .ranges()
        .len()
        .saturating_sub(1)
        .saturating_mul(SEPARATOR.len());
    let mut values = Vec::with_capacity(selections.ranges().len());
    for selection in selections.ranges() {
        check(cancellation)?;
        let slice = snapshot
            .text()
            .slice(selection.start().0..selection.end().0);
        size = size
            .checked_add(slice.len_bytes())
            .filter(|&len| len <= MAX_BYTES)
            .ok_or_else(|| io::Error::other("clipboard copy exceeds 128 MiB"))?;
        let mut value = String::with_capacity(slice.len_bytes());
        for chunk in slice.chunks() {
            check(cancellation)?;
            value.push_str(chunk);
        }
        values.push(Arc::from(value));
    }
    Ok(values.into())
}

fn matches(values: &[Arc<str>], mut text: &str, cancellation: &Cancellation) -> bool {
    for (index, value) in values.iter().enumerate() {
        if cancellation.is_cancelled() {
            return false;
        }
        if index > 0 {
            let Some(rest) = text.strip_prefix(SEPARATOR) else {
                return false;
            };
            text = rest;
        }
        let Some(rest) = text.strip_prefix(value.as_ref()) else {
            return false;
        };
        text = rest;
    }
    text.is_empty()
}

struct Spec {
    program: std::path::PathBuf,
    args: Vec<String>,
}

impl Spec {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        if matches!(self.program.to_str(), Some("pbcopy" | "pbpaste")) {
            command.env("LC_ALL", "en_US.UTF-8");
        }
        command
    }
}

struct Provider {
    read: Spec,
    write: Spec,
}

impl Provider {
    fn new(
        read: &'static str,
        read_args: &'static [&'static str],
        write: &'static str,
        write_args: &'static [&'static str],
    ) -> Self {
        Self {
            read: Spec {
                program: read.into(),
                args: read_args.iter().map(|arg| (*arg).into()).collect(),
            },
            write: Spec {
                program: write.into(),
                args: write_args.iter().map(|arg| (*arg).into()).collect(),
            },
        }
    }

    fn detect(kind: ClipboardKind) -> io::Result<Self> {
        let set = |name| std::env::var_os(name).is_some_and(|value| !value.is_empty());
        if kind == ClipboardKind::Primary {
            if set("WAYLAND_DISPLAY") && available("wl-copy") && available("wl-paste") {
                return Ok(Self::new(
                    "wl-paste",
                    &["-p", "--no-newline"],
                    "wl-copy",
                    &["-p", "--type", "text/plain"],
                ));
            }
            if set("DISPLAY") && available("xclip") {
                return Ok(Self::new(
                    "xclip",
                    &["-o", "-selection", "primary"],
                    "xclip",
                    &["-i", "-selection", "primary"],
                ));
            }
            if set("DISPLAY") && available("xsel") {
                return Ok(Self::new("xsel", &["-o", "-p"], "xsel", &["-i", "-p"]));
            }
            return Err(io::Error::other(
                "primary clipboard requires wl-clipboard or xclip/xsel with a running display server",
            ));
        }
        if set("TMUX") && available("tmux") {
            return Ok(Self::new(
                "tmux",
                &["save-buffer", "-"],
                "tmux",
                &["load-buffer", "-w", "-"],
            ));
        }
        if cfg!(target_os = "macos") && available("pbcopy") && available("pbpaste") {
            return Ok(Self::new("pbpaste", &["-Prefer", "txt"], "pbcopy", &[]));
        }
        if available("termux-clipboard-set") && available("termux-clipboard-get") {
            return Ok(Self::new(
                "termux-clipboard-get",
                &[],
                "termux-clipboard-set",
                &[],
            ));
        }
        if set("WAYLAND_DISPLAY") && available("wl-copy") && available("wl-paste") {
            return Ok(Self::new(
                "wl-paste",
                &["--no-newline"],
                "wl-copy",
                &["--type", "text/plain"],
            ));
        }
        if set("DISPLAY") && available("xclip") {
            return Ok(Self::new(
                "xclip",
                &["-o", "-selection", "clipboard"],
                "xclip",
                &["-i", "-selection", "clipboard"],
            ));
        }
        if set("DISPLAY") && available("xsel") {
            return Ok(Self::new("xsel", &["-o", "-b"], "xsel", &["-i", "-b"]));
        }
        if available("win32yank.exe") {
            return Ok(Self::new(
                "win32yank.exe",
                &["-o"],
                "win32yank.exe",
                &["-i"],
            ));
        }
        if (cfg!(windows) || set("WSL_DISTRO_NAME")) && available("powershell.exe") {
            return Ok(Self::new(
                "powershell.exe",
                &[
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); [Console]::Out.Write([string](Get-Clipboard -Raw))",
                ],
                "powershell.exe",
                &[
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "[Console]::InputEncoding = [System.Text.UTF8Encoding]::new($false); Set-Clipboard -Value ([Console]::In.ReadToEnd())",
                ],
            ));
        }
        Err(io::Error::other(
            "no clipboard provider found (pbcopy/pbpaste, wl-clipboard, xclip/xsel, tmux, or a Windows clipboard helper)",
        ))
    }
}

fn available(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|path| {
            let Ok(metadata) = path.join(program).metadata() else {
                return false;
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            metadata.is_file()
        })
    })
}

struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run(
    command: &mut Command,
    input: Option<std::fs::File>,
    cancellation: &Cancellation,
    timeout: Duration,
) -> io::Result<Vec<u8>> {
    check(cancellation)?;
    // Regular temporary files avoid pipe deadlock and extra reader threads.
    // No clipboard text is interpolated into a command line or shell script.
    let mut output = tempfile::tempfile()?;
    let mut errors = tempfile::tempfile()?;
    command
        .stdin(input.map_or_else(Stdio::null, Stdio::from))
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::from(errors.try_clone()?));
    let mut child = Running(command.spawn()?);
    let deadline = Instant::now() + timeout;
    loop {
        check(cancellation)?;
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "clipboard helper timed out",
            ));
        }
        if output.metadata()?.len() > MAX_BYTES as u64 || errors.metadata()?.len() > 64 << 10 {
            return Err(io::Error::other(
                "clipboard helper output exceeds its size limit",
            ));
        }
        if let Some(status) = child.0.try_wait()? {
            if !status.success() {
                errors.seek(SeekFrom::Start(0))?;
                let mut bytes = Vec::new();
                errors.take(4096).read_to_end(&mut bytes)?;
                let detail = String::from_utf8_lossy(&bytes);
                return Err(io::Error::other(format!(
                    "clipboard helper failed ({status}): {}",
                    detail.trim()
                )));
            }
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    output.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    output.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_BYTES {
        return Err(io::Error::other("clipboard contents exceed 128 MiB"));
    }
    check(cancellation)?;
    Ok(bytes)
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;

    // A real subprocess transport backed by a private test file. Tests never
    // read or overwrite the developer's desktop clipboard or mutate PATH.
    pub(crate) fn file_worker(path: &std::path::Path) -> Worker {
        Worker {
            slots: [file_slot(path), Slot::default()],
        }
    }

    pub(crate) fn two_file_worker(system: &std::path::Path, primary: &std::path::Path) -> Worker {
        Worker {
            slots: [file_slot(system), file_slot(primary)],
        }
    }

    fn file_slot(path: &std::path::Path) -> Slot {
        Slot {
            provider: Some(Provider {
                read: Spec {
                    program: "cat".into(),
                    args: vec![path.to_str().unwrap().into()],
                },
                write: Spec {
                    program: "sh".into(),
                    args: vec![
                        "-c".into(),
                        "cat > \"$1\"".into(),
                        "vex-clipboard".into(),
                        path.to_str().unwrap().into(),
                    ],
                },
            }),
            saved: None,
        }
    }

    #[test]
    fn joined_cache_requires_exact_bytes_including_empty_fragments() {
        let cancellation = Cancellation::default();
        let values: Fragments = Arc::from([Arc::from(""), Arc::from("cat"), Arc::from("")]);
        assert!(matches(&values, "\ncat\n", &cancellation));
        assert!(!matches(&values, "\ncat\nmore", &cancellation));
        assert!(!matches(&values, "\ncat", &cancellation));
        cancellation.cancel();
        assert!(!matches(&values, "\ncat\n", &cancellation));
    }

    #[test]
    fn prompt_reads_keep_first_fragment_controls_and_size_limits_on_worker() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("clipboard");
        let mut worker = file_worker(&path);
        let mut request = |operation| {
            worker
                .run(Job {
                    id: 1,
                    kind: ClipboardKind::System,
                    operation,
                    cancellation: Cancellation::default(),
                })
                .unwrap()
                .outcome
        };
        request(Operation::Write {
            values: Arc::from([Arc::from("e\u{301}\n"), Arc::from("unused")]),
        })
        .unwrap();
        let Outcome::Text(text) = request(Operation::Read {
            limit: 64,
            trim_controls: true,
            truncate: false,
        })
        .unwrap() else {
            panic!()
        };
        assert_eq!(text.as_ref(), "e\u{301}");
        std::fs::write(&path, "界".repeat(128)).unwrap();
        assert!(
            request(Operation::Read {
                limit: 64,
                trim_controls: true,
                truncate: false
            })
            .is_err()
        );
        let Outcome::Text(text) = request(Operation::Read {
            limit: 64,
            trim_controls: true,
            truncate: true,
        })
        .unwrap() else {
            panic!()
        };
        assert_eq!(text.len(), 63);
        std::fs::write(&path, "line\r\n").unwrap();
        let Outcome::Text(text) = request(Operation::Read {
            limit: 64,
            trim_controls: false,
            truncate: false,
        })
        .unwrap() else {
            panic!()
        };
        assert_eq!(text.as_ref(), "line\r\n");
    }

    #[test]
    fn helper_failure_and_timeout_return_without_blocking_shutdown() {
        let cancellation = Cancellation::default();
        let error = run(
            Command::new("sh").args(["-c", "printf 'clipboard unavailable' >&2; exit 7"]),
            None,
            &cancellation,
            TIMEOUT,
        )
        .unwrap_err();
        assert!(error.to_string().contains("clipboard unavailable"));
        let error = run(
            Command::new("sh").args(["-c", "exec sleep 20"]),
            None,
            &cancellation,
            Duration::from_millis(20),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn cancellation_reaps_a_running_helper() {
        let directory = tempfile::tempdir().unwrap();
        let started = directory.path().join("started");
        let cancellation = Cancellation::default();
        let token = cancellation.clone();
        let path = started.clone();
        let (send, receive) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let error = run(
                Command::new("sh")
                    .args([
                        "-c",
                        "printf ready > \"$1\"; exec sleep 20",
                        "vex-clipboard",
                    ])
                    .arg(path),
                None,
                &token,
                TIMEOUT,
            )
            .unwrap_err();
            send.send(error.kind()).unwrap();
        });
        let deadline = Instant::now() + TIMEOUT;
        while !started.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        assert!(started.exists());
        cancellation.cancel();
        assert_eq!(
            receive.recv_timeout(TIMEOUT).unwrap(),
            io::ErrorKind::Interrupted
        );
        worker.join().unwrap();
    }

    #[test]
    fn copy_captures_unicode_selected_ranges_without_shell_interpolation() {
        use vex_core::{CharOffset, Document, Selection};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("clipboard file");
        let text = "e\u{301}界 $(printf bad)\r\n";
        let document = Document::from(text);
        let mut worker = file_worker(&path);
        let result = worker
            .run(Job {
                id: 3,
                kind: ClipboardKind::System,
                cancellation: Cancellation::default(),
                operation: Operation::Copy {
                    cut: None,
                    snapshot: document.snapshot(),
                    selections: SelectionSet::single(Selection::new(
                        CharOffset(0),
                        CharOffset(document.text().len_chars()),
                    )),
                },
            })
            .unwrap();
        assert!(matches!(result.outcome, Ok(Outcome::Copied(1))));
        assert_eq!(std::fs::read_to_string(path).unwrap(), text);
    }
}
