//! Document/workspace diagnostics share the ordinary picker, syntax preview,
//! queued acceptance, and background location navigation.

use super::*;
use crate::picker::diagnostics::{Hit, Job, Result};

pub(super) struct Source {
    pub path: Option<PathBuf>,
    pub cwd: PathBuf,
    pub generation: u64,
    pub documents: Arc<[crate::picker::search::OpenDocument]>,
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        sync::mpsc,
        time::{Duration, Instant},
    };
    use vex_core::CharOffset;
    use vex_lsp::{Event as LspEvent, Service};

    fn press(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.handle(Event::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )));
        }
    }

    fn key(app: &mut App, code: KeyCode) {
        app.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    fn fixture() -> (tempfile::TempDir, App, Service, mpsc::Receiver<LspEvent>) {
        let directory = tempfile::tempdir().unwrap();
        let main = directory.path().join("main.rs");
        fs::write(&main, "fn target() {}\nfn main() { target(); }\n").unwrap();
        fs::write(directory.path().join("other.rs"), "// 🦀\nfn other() {}\n").unwrap();
        let program = directory.path().join("server.py");
        fs::write(&program, r#"#!/usr/bin/env python3
import sys, json
from pathlib import Path
def send(value):
    value['jsonrpc'] = '2.0'
    data = json.dumps(value).encode()
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n' % len(data)).encode() + data)
    sys.stdout.buffer.flush()
def publish(uri, message, severity=4, line=0, start=0, end=1, version=None):
    params = {'uri':uri, 'diagnostics': [] if message is None else [{
        'range':{'start':{'line':line,'character':start}, 'end':{'line':line,'character':end}},
        'message':message, 'source':'mock', 'code':'E0001', 'severity':severity}]}
    if version is not None: params['version'] = version
    send({'method':'textDocument/publishDiagnostics','params':params})
other = (Path(__file__).parent / 'other.rs').as_uri()
while True:
    size = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        if line == b'\r\n': break
        if line.lower().startswith(b'content-length:'): size = int(line.split(b':')[1])
    value = json.loads(sys.stdin.buffer.read(size))
    method = value.get('method')
    if method == 'initialize': send({'id':value['id'],'result':{'capabilities':{'hoverProvider':True,'textDocumentSync':1}}})
    elif method == 'textDocument/didOpen':
        doc = value['params']['textDocument']
        publish(doc['uri'], 'active error', 1, 0, 3, 9, doc['version'])
        publish(other, 'other warning', 2, 1, 3, 8)
        for n in range(750): publish((Path(__file__).parent / ('unopened%d.rs' % n)).as_uri(), 'extra note %d' % n)
    elif method == 'textDocument/hover':
        publish(other, None)
        send({'id':value['id'],'result':None})
    elif method == 'shutdown': send({'id':value['id'],'result':None})
    elif method == 'exit': sys.exit(0)
"#).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let mut app = App::open(Some(&main), (100, 24)).unwrap();
        app.enable_lsp();
        let (sender, receiver) = mpsc::channel();
        let service = Service::with_program(program, move |event| {
            let _ = sender.send(event);
        })
        .unwrap();
        service.update(app.take_lsp_update().unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                Instant::now() < deadline,
                "{} files; {}",
                app.language.diagnostic_catalog.snapshot().files.len(),
                app.message
            );
            let Ok(event) = receiver.recv_timeout(Duration::from_millis(100)) else {
                continue;
            };
            assert!(
                !matches!(event, LspEvent::Status { failed: true, .. }),
                "{event:?}"
            );
            app.handle_lsp_event(event);
            if app.language.diagnostic_catalog.snapshot().files.len() == 752 {
                break;
            }
        }
        (directory, app, service, receiver)
    }

    fn filter(app: &mut App, worker: &mut crate::picker::diagnostics::Worker) {
        let job = app.take_diagnostic_job().unwrap();
        let result = worker.run(job).unwrap();
        assert!(app.handle_diagnostic_result(result));
    }

    #[test]
    fn empty_catalog_finishes_loading_and_releases_early_acceptance() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("notes.txt");
        fs::write(&path, "plain text\n").unwrap();
        let mut app = App::open(Some(&path), (100, 24)).unwrap();
        let mut worker = crate::picker::diagnostics::Worker::default();
        for binding in [" d", " D"] {
            press(&mut app, binding);
            key(&mut app, KeyCode::Enter);
            assert!(app.input_waiting());
            filter(&mut app, &mut worker);
            let active = app.picker.active.as_ref().unwrap();
            assert!(!active.view.pending);
            assert!(active.view.items.is_empty());
            assert!(!app.input_waiting());
            let mut frame = crate::screen::Frame::default();
            frame.reset(100, 24).unwrap();
            app.paint(&mut frame).unwrap();
            let rows: String = (0..24).map(|row| frame.row_text(row)).collect();
            assert!(rows.contains("No matching diagnostics"));
            assert!(!rows.contains("Loading"));
            key(&mut app, KeyCode::Esc);
        }
    }

    #[test]
    fn diagnostic_pickers_filter_preview_select_ranges_reopen_and_reject_stale_buffers() {
        let (_directory, mut app, _service, _receiver) = fixture();
        let origin = app.editor.document().id();
        let mut worker = crate::picker::diagnostics::Worker::default();
        press(&mut app, " d");
        filter(&mut app, &mut worker);
        let active = app.picker.active.as_ref().unwrap();
        assert_eq!(active.view.title, "Document diagnostics");
        assert_eq!(active.view.total, 1);
        assert!(
            active.view.items[0]
                .entry
                .label
                .contains("ERROR mock E0001 1:4 active error")
        );
        // One result still opens a picker, rather than jumping automatically.
        assert!(!app.location_navigation_waiting());
        key(&mut app, KeyCode::Esc);
        press(&mut app, " D");
        filter(&mut app, &mut worker);
        let active = app.picker.active.as_ref().unwrap();
        assert_eq!(active.view.total, 752);
        assert_eq!(active.view.items.len(), crate::picker::files::MAX_RESULTS);
        assert!(active.view.items[0].entry.label.starts_with("ERROR"));
        assert!(active.view.items[1].entry.label.starts_with("WARN"));
        press(&mut app, "other warning");
        let stale = worker.run(app.take_diagnostic_job().unwrap()).unwrap();
        key(&mut app, KeyCode::Backspace);
        assert!(!app.handle_diagnostic_result(stale));
        filter(&mut app, &mut worker);
        let preview = app.take_preview_job().unwrap().run().unwrap();
        app.handle_preview_result(preview);
        assert!(
            app.picker
                .active
                .as_ref()
                .unwrap()
                .view
                .preview
                .text
                .contains("fn other()")
        );
        press(&mut app, "g");
        key(&mut app, KeyCode::Enter); // Early acceptance waits for the matching result.
        assert!(app.input_waiting());
        filter(&mut app, &mut worker);
        assert!(app.picker.active.is_none());
        let result = app.take_location_navigation().unwrap().run().unwrap();
        app.handle_location_navigation(result);
        assert!(app.files.target().unwrap().ends_with("other.rs"));
        assert_eq!(app.editor.selections().primary().head, CharOffset(8));
        assert_eq!(app.editor.selections().primary().anchor, CharOffset(13));
        assert!(!app.input_waiting());
        app.execute("jump_backward").unwrap();
        assert_eq!(app.editor.document().id(), origin);
        press(&mut app, " '");
        filter(&mut app, &mut worker);
        assert_eq!(
            app.picker.active.as_ref().unwrap().view.query.text(),
            "other warning"
        );
        assert_eq!(app.picker.active.as_ref().unwrap().view.items.len(), 1);
        key(&mut app, KeyCode::Enter);
        let result = app.take_location_navigation().unwrap().run().unwrap();
        app.handle_location_navigation(result);
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("changed").unwrap();
        app.editor.execute("normal_mode", 1).unwrap();
        press(&mut app, " '");
        filter(&mut app, &mut worker);
        let active = app.picker.active.as_ref().unwrap();
        assert!(active.view.items.is_empty());
        assert!(active.view.notice.contains("stale"));
        let cancelled = app.take_diagnostic_job();
        assert!(cancelled.is_none());
        press(&mut app, "x");
        let cancelled = app.take_diagnostic_job().unwrap();
        key(&mut app, KeyCode::Esc);
        assert!(worker.run(cancelled).is_none());
    }

    #[test]
    fn clears_refresh_an_open_picker_and_cancelled_early_acceptance_releases_input() {
        let (_directory, mut app, service, receiver) = fixture();
        let mut worker = crate::picker::diagnostics::Worker::default();
        press(&mut app, " Dother warning");
        filter(&mut app, &mut worker);
        assert_eq!(app.picker.active.as_ref().unwrap().view.items.len(), 1);
        app.editor.execute("hover", 1).unwrap();
        service.update(app.take_lsp_update().unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        while app
            .language
            .diagnostic_catalog
            .snapshot()
            .files
            .iter()
            .any(|file| file.path.ends_with("other.rs"))
        {
            assert!(Instant::now() < deadline);
            app.handle_lsp_event(receiver.recv_timeout(Duration::from_secs(1)).unwrap());
        }
        app.refresh_diagnostic_picker(false);
        key(&mut app, KeyCode::Enter);
        assert!(app.input_waiting());
        filter(&mut app, &mut worker);
        assert!(!app.input_waiting());
        assert!(app.picker.active.as_ref().unwrap().view.items.is_empty());
        assert!(
            app.picker
                .active
                .as_ref()
                .unwrap()
                .view
                .notice
                .contains("No matching")
        );
        press(&mut app, "new query");
        key(&mut app, KeyCode::Enter);
        let job = app.take_diagnostic_job().unwrap();
        assert!(app.input_waiting());
        key(&mut app, KeyCode::Esc);
        assert!(worker.run(job).is_none());
        assert!(!app.input_waiting());
    }

    #[test]
    #[ignore = "manual release-mode diagnostic picker submission and filtering benchmark"]
    fn benchmark_diagnostic_picker_submission_and_filtering() {
        use std::hint::black_box;
        let (_directory, mut app, _service, _receiver) = fixture();
        let mut worker = crate::picker::diagnostics::Worker::default();
        let mut submit = Vec::with_capacity(200);
        let mut filter = Vec::with_capacity(200);
        for _ in 0..200 {
            let start = Instant::now();
            app.open_diagnostic_picker(true).unwrap();
            let job = black_box(app.take_diagnostic_job().unwrap());
            submit.push(start.elapsed());
            let start = Instant::now();
            let result = worker.run(job).unwrap();
            filter.push(start.elapsed());
            app.handle_diagnostic_result(result);
        }
        submit.sort_unstable();
        filter.sort_unstable();
        eprintln!(
            "752 files: picker submission median {:?}, p95 {:?}; worker preparation/filtering median {:?}, p95 {:?}",
            submit[100], submit[189], filter[100], filter[189]
        );
        app.picker.active.as_mut().unwrap().view.paste("extra note");
        submit.clear();
        filter.clear();
        for _ in 0..200 {
            let start = Instant::now();
            app.submit_picker_query();
            let job = black_box(app.take_diagnostic_job().unwrap());
            submit.push(start.elapsed());
            let start = Instant::now();
            let result = worker.run(job).unwrap();
            filter.push(start.elapsed());
            app.handle_diagnostic_result(result);
        }
        submit.sort_unstable();
        filter.sort_unstable();
        eprintln!(
            "752 files, warm nonempty query: submission median {:?}, p95 {:?}; worker filtering median {:?}, p95 {:?}",
            submit[100], submit[189], filter[100], filter[189]
        );
    }
}

impl App {
    pub(in crate::app) fn open_diagnostic_picker(&mut self, workspace: bool) -> io::Result<()> {
        let path = if workspace {
            None
        } else {
            Some(
                self.files
                    .target()
                    .ok_or_else(|| io::Error::other("document has no file path"))?
                    .into(),
            )
        };
        self.close_picker();
        self.dismiss_language_help();
        self.keys.cancel(&mut self.editor);
        self.prompt = None;
        if self.editor.search_prompt().is_some() {
            let _ = self.editor.execute("search_cancel", 1);
        }
        self.clear_message();
        let cwd = std::env::current_dir().unwrap_or_default();
        // When invoked on a file outside the shell's working directory, show
        // nearby diagnostic paths relative to that file's directory.
        let cwd = self
            .files
            .target()
            .filter(|path| !path.starts_with(&cwd))
            .and_then(std::path::Path::parent)
            .map_or(cwd, PathBuf::from);
        let mut view = Picker::new(
            if workspace {
                "Workspace diagnostics"
            } else {
                "Document diagnostics"
            }
            .into(),
        );
        view.noun = "diagnostics";
        self.picker.next_session += 1;
        self.picker.active = Some(Active {
            view,
            session: self.picker.next_session,
            revision: 0,
            source: super::Source::Diagnostics(Source {
                path,
                cwd,
                generation: 0,
                documents: self.workspace_documents(),
            }),
            cancellation: Cancellation::default(),
            preview_cancel: Cancellation::default(),
            preview_request: 0,
            preview_target: None,
            accept_pending: false,
        });
        self.submit_picker_query();
        Ok(())
    }

    pub(in crate::app) fn refresh_diagnostic_picker(&mut self, force: bool) -> bool {
        let Some(active) = &self.picker.active else {
            return false;
        };
        let super::Source::Diagnostics(source) = &active.source else {
            return false;
        };
        if force || source.generation != self.language.diagnostic_catalog.generation() {
            let accept = active.accept_pending;
            if force {
                let documents = self.workspace_documents();
                if let super::Source::Diagnostics(source) =
                    &mut self.picker.active.as_mut().unwrap().source
                {
                    source.documents = documents;
                }
            }
            self.submit_picker_query();
            self.picker.active.as_mut().unwrap().accept_pending = accept;
        }
        true
    }

    pub(crate) fn take_diagnostic_job(&mut self) -> Option<Job> {
        self.picker.diagnostic_job.take()
    }

    pub(crate) fn handle_diagnostic_result(&mut self, result: Result) -> bool {
        let Some(active) = &mut self.picker.active else {
            return false;
        };
        if !matches!(active.source, super::Source::Diagnostics(_))
            || active.session != result.ranked.session
            || active.revision != result.ranked.revision
            || active.cancellation.is_cancelled()
        {
            return false;
        }
        if result.generation != self.language.diagnostic_catalog.generation() {
            self.refresh_diagnostic_picker(true);
            return true;
        }
        active.view.replace(
            result
                .ranked
                .items
                .into_iter()
                .map(|item| Item {
                    entry: Arc::new(Entry {
                        label: item.entry.label.clone(),
                        value: Target::Diagnostic(item.entry.value.clone()),
                    }),
                    matched: item.matched,
                })
                .collect(),
        );
        active.view.total = result.ranked.total;
        active.view.matched = result.ranked.matched;
        active.view.notice = result.ranked.notice;
        active.view.pending = false;
        let accept = std::mem::take(&mut active.accept_pending);
        if accept {
            self.accept_picker();
        } else {
            self.request_picker_preview();
        }
        true
    }

    pub(super) fn accept_diagnostic(&mut self, hit: Hit) {
        let current = self.snapshot_for_path(&hit.path);
        let valid = match (hit.version, current) {
            (Some((id, revision)), Some(current)) => {
                id == current.id() && revision == current.revision()
            }
            (Some(_), None) => false,
            (None, Some(current)) => current.revision().get() == 0,
            (None, None) => true,
        };
        if !valid {
            self.refresh_diagnostic_picker(true);
            return;
        }
        self.close_picker();
        self.begin_location_navigation(vex_lsp::Destination {
            path: (*hit.path).clone(),
            range: hit.range,
        });
    }
}
