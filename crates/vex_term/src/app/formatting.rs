//! Captured origin and buffer settings for asynchronous formatting.

use super::{App, workspace::Context};
use vex_lsp::{
    RequestKind,
    workspace_edit::{SynchronizedDocument, WorkspaceEdit},
};

#[derive(Default)]
pub(super) struct State {
    context: Option<Context>,
}

impl State {
    pub fn clear(&mut self) {
        self.context = None;
    }
}

impl App {
    pub(super) fn prepare_formatting_request(&mut self, whole_document: bool) -> RequestKind {
        self.close_picker();
        let indentation = self.editor.indentation();
        self.formatting.context = Some(self.document_edit_context());
        RequestKind::Format {
            selection: (!whole_document).then(|| self.editor.selections().primary()),
            indentation,
        }
    }

    pub(super) fn receive_formatting(
        &mut self,
        edit: WorkspaceEdit,
        versions: Vec<SynchronizedDocument>,
    ) {
        let Some(context) = self.formatting.context.take() else {
            return;
        };
        if !context.current(self) {
            self.fail("formatting cancelled: original buffer, selection or settings changed");
            return;
        }
        if edit.documents.is_empty() {
            self.message = "no formatting changes".into();
        } else if let Err(error) = self.begin_workspace_edit(context, edit, versions) {
            self.fail(error);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crossterm::event::{Event as TerminalEvent, KeyCode, KeyEvent, KeyModifiers};
    use std::{
        fs,
        num::NonZeroUsize,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        sync::mpsc,
        time::{Duration, Instant},
    };
    use vex_core::{CharOffset, Selection, SelectionSet};
    use vex_editor::{IndentStyle, Indentation, Language, Mode};
    use vex_lsp::{Answer, Event, Service};

    const ORIGINAL: &str = "a🦀b  =1;\r\nc  =2;\r\nuntouched\r\n";
    const FORMATTED: &str = "a🦀b = 1;\r\nc = 2;\r\nuntouched\r\n";

    fn fixture(
        mode: &str,
    ) -> (
        tempfile::TempDir,
        App,
        Service,
        mpsc::Receiver<Event>,
        PathBuf,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let main = root.join("main.rs");
        fs::write(&main, ORIGINAL).unwrap();
        let path = root.join("format.py");
        fs::write(&path, r#"#!/usr/bin/env python3
import sys, json, os
mode = '#MODE#'
log = open(os.path.join(os.path.dirname(__file__), 'events.log'), 'w', buffering=1)
documents = {}
held = None
def send(value):
    value['jsonrpc'] = '2.0'
    body = json.dumps(value).encode()
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n' % len(body)).encode()+body)
    sys.stdout.buffer.flush()
def edit(line, start, end, text):
    return {'range':{'start':{'line':line,'character':start},'end':{'line':line,'character':end}},'newText':text}
while True:
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        if line == b'\r\n': break
        if line.lower().startswith(b'content-length:'): length = int(line.split(b':')[1])
    value = json.loads(sys.stdin.buffer.read(length))
    log.write(json.dumps(value) + '\n')
    method = value.get('method'); params = value.get('params', {})
    if method == 'initialize':
        assert 'formatting' in params['capabilities']['textDocument']
        assert 'rangeFormatting' in params['capabilities']['textDocument']
        send({'id':value['id'],'result':{'capabilities':{'textDocumentSync':2,
              'documentFormattingProvider': mode != 'unsupported',
              'documentRangeFormattingProvider': mode not in ['unsupported','document-only']}}})
    elif method == 'textDocument/didOpen':
        doc = params['textDocument']; documents[doc['uri']] = doc
    elif method == 'textDocument/didChange':
        doc = documents[params['textDocument']['uri']]
        assert params['textDocument']['version'] == doc['version']+1
        doc['version'] += 1; doc['text'] = params['contentChanges'][0]['text']
    elif method == 'textDocument/didClose': documents.pop(params['textDocument']['uri'])
    elif method in ['textDocument/rangeFormatting','textDocument/formatting']:
        assert len(documents) == 1  # Formatting does not open unrelated hidden buffers.
        text = documents[params['textDocument']['uri']]['text']
        edits = []
        for i, line in enumerate(text.split('\n')):
            if 'range' in params and not params['range']['start']['line'] <= i <= params['range']['end']['line']: continue
            if '  =' in line:
                at = len(line[:line.index('  =')].encode('utf-16-le'))//2
                edits += [edit(i, at+3, at+3, ' '), edit(i, at, at+1, '')]
        if mode == 'overlap': edits += [edits[-1]]
        if mode == 'invalid': edits[-1]['range']['end']['character'] = 9999
        if mode == 'malformed': edits[-1].pop('newText')
        if mode == 'null': edits = None
        if mode == 'noop':
            lines = text.split('\n')
            edits = [{'range':{'start':{'line':0,'character':0},'end':{'line':len(lines)-1,'character':len(lines[-1].encode('utf-16-le'))//2}},'newText':text}]
        if mode == 'error': send({'id':value['id'],'error':{'code':-32603,'message':'formatter failed'}})
        elif mode == 'hold': held = {'id':value['id'],'result':edits}
        else: send({'id':value['id'],'result':edits})
    elif method == '$/cancelRequest' and held is not None:
        assert params['id'] == held['id']
        send(held); held = None
    elif method == 'shutdown': send({'id':value['id'],'result':None})
    elif method == 'exit': break
"#.replace("#MODE#", mode)).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let hidden = root.join("hidden.rs");
        fs::write(&hidden, "hidden\n").unwrap();
        let mut app = App::open(Some(&main), (100, 24)).unwrap();
        app.open_window_file(&hidden).unwrap();
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("// unsaved\n").unwrap();
        app.editor.execute("normal_mode", 1).unwrap();
        app.open_window_file(&main).unwrap();
        app.enable_lsp();
        let (sender, receiver) = mpsc::channel();
        let service = Service::with_program(path, move |event| {
            let _ = sender.send(event);
        })
        .unwrap();
        flush(&mut app, &service);
        (directory, app, service, receiver, main)
    }
    fn flush(app: &mut App, service: &Service) {
        if let Some(update) = app.take_lsp_update() {
            service.update(update);
        }
        if let Some(update) = app.take_lsp_workspace_update() {
            service.update_workspace(update);
        }
    }
    fn receive(receiver: &mpsc::Receiver<Event>) -> Event {
        let event = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("missing formatting event");
        assert!(
            !matches!(&event, Event::Status { failed: true, .. }),
            "{event:?}"
        );
        event
    }
    fn reply(app: &mut App, receiver: &mpsc::Receiver<Event>) -> Event {
        loop {
            let event = receive(receiver);
            if matches!(&event, Event::Answer { .. }) {
                return event;
            }
            app.handle_lsp_event(event);
        }
    }
    fn complete(app: &mut App, service: &Service, receiver: &mpsc::Receiver<Event>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        flush(app, service);
        while app.input_waiting() {
            assert!(Instant::now() < deadline, "formatting did not finish");
            if let Some(job) = app.take_workspace_edit() {
                let result = job.run().unwrap();
                assert!(app.handle_workspace_edit(result));
            } else {
                app.handle_lsp_event(receive(receiver));
            }
            flush(app, service);
        }
    }
    fn key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
        app.handle(TerminalEvent::Key(KeyEvent::new(code, modifiers)));
    }

    #[test]
    fn range_formatting_preserves_direction_mode_other_lines_tabs_and_undo() {
        let (directory, mut app, service, receiver, main) = fixture("normal");
        app.editor.execute("select_mode", 1).unwrap();
        let selected = SelectionSet::single(Selection::new(CharOffset(3), CharOffset(1)));
        app.editor.set_selections(selected.clone()).unwrap();
        app.editor.set_indentation(Indentation {
            style: IndentStyle::Tabs,
            tab_width: NonZeroUsize::new(8).unwrap(),
        });
        key(&mut app, KeyCode::Char('='), KeyModifiers::NONE);
        complete(&mut app, &service, &receiver);
        assert!(!app.error, "{}", app.message);
        assert_eq!(
            app.editor.document().text(),
            "a🦀b = 1;\r\nc  =2;\r\nuntouched\r\n"
        );
        assert_eq!(app.editor.mode(), Mode::Select);
        assert_eq!(app.editor.selections(), &selected);
        assert_eq!(fs::read_to_string(&main).unwrap(), ORIGINAL);
        assert_eq!(
            app.snapshot_for_path(&main.with_file_name("hidden.rs"))
                .unwrap()
                .text(),
            "// unsaved\nhidden\n"
        );
        let log = fs::read_to_string(directory.path().join("events.log")).unwrap();
        let request = log
            .lines()
            .find(|line| line.contains("textDocument/rangeFormatting"))
            .unwrap();
        assert!(request.contains("\"tabSize\": 8") && request.contains("\"insertSpaces\": false"));
        assert!(request.contains("\"character\": 1") && request.contains("\"character\": 4"));
        app.editor.execute("undo", 1).unwrap();
        assert_eq!(app.editor.document().text(), ORIGINAL);
        assert_eq!(app.editor.selections(), &selected);
        app.editor.execute("redo", 1).unwrap();
        assert_eq!(
            app.editor.document().text(),
            "a🦀b = 1;\r\nc  =2;\r\nuntouched\r\n"
        );
    }

    #[test]
    fn document_formatting_updates_all_views_and_supports_multiple_selections() {
        let (_directory, mut app, service, receiver, main) = fixture("normal");
        let selections = SelectionSet::new(
            vec![
                Selection::new(CharOffset(0), CharOffset(1)),
                Selection::new(CharOffset(10), CharOffset(11)),
            ],
            0,
        )
        .unwrap();
        app.editor.set_selections(selections.clone()).unwrap();
        let original_view = app.editor.active_view();
        app.execute("vsplit").unwrap();
        app.editor.execute("goto_file_end", 1).unwrap();
        let second_selections = app.editor.selections().clone();
        app.execute("fmt").unwrap();
        complete(&mut app, &service, &receiver);
        assert!(!app.error, "{}", app.message);
        assert_eq!(app.editor.document().text(), FORMATTED);
        assert_eq!(app.editor.selections(), &second_selections);
        assert_eq!(
            app.editor
                .with_view(original_view, |editor| editor.selections().clone())
                .unwrap(),
            selections
        );
        assert_eq!(fs::read_to_string(&main).unwrap(), ORIGINAL);
        app.editor.execute("undo", 1).unwrap();
        assert_eq!(app.editor.document().text(), ORIGINAL);
        // The typed whole-document command accepts multiple selections, unlike =.
        app.editor.set_selections(selections).unwrap();
        assert!(app.editor.execute("format_selections", 1).is_err());
        app.execute("format").unwrap();
        complete(&mut app, &service, &receiver);
        assert!(!app.error, "{}", app.message);
        assert_eq!(app.editor.document().text(), FORMATTED);
        assert!(app.execute("format!").is_err());
        assert!(app.execute("fmt argument").is_err());
    }

    #[test]
    fn invalid_overlapping_failed_and_noop_formatting_never_partially_changes_text() {
        for mode in [
            "invalid",
            "overlap",
            "malformed",
            "error",
            "null",
            "noop",
            "unsupported",
        ] {
            let (_directory, mut app, service, receiver, _) = fixture(mode);
            let revision = app.editor.document().revision();
            let selections = app.editor.selections().clone();
            app.execute("format").unwrap();
            complete(&mut app, &service, &receiver);
            assert_eq!(app.editor.document().revision(), revision, "{mode}");
            assert_eq!(app.editor.document().text(), ORIGINAL, "{mode}");
            assert_eq!(app.editor.selections(), &selections, "{mode}");
            assert!(!app.is_dirty(), "{mode}");
            assert_eq!(
                app.error,
                !matches!(mode, "null" | "noop"),
                "{mode}: {}",
                app.message
            );
        }
    }

    #[test]
    fn missing_range_support_does_not_fall_back_to_formatting_the_whole_file() {
        let (directory, mut app, service, receiver, _) = fixture("document-only");
        key(&mut app, KeyCode::Char('='), KeyModifiers::NONE);
        complete(&mut app, &service, &receiver);
        assert!(
            app.error && app.message.contains("range formatting"),
            "{}",
            app.message
        );
        assert_eq!(app.editor.document().text(), ORIGINAL);
        let log = fs::read_to_string(directory.path().join("events.log")).unwrap();
        assert!(!log.contains("\"method\": \"textDocument/formatting\""));
        app.clear_message();
        app.execute("format").unwrap();
        complete(&mut app, &service, &receiver);
        assert!(!app.error, "{}", app.message);
        assert_eq!(app.editor.document().text(), FORMATTED);
    }

    #[test]
    fn cancelled_or_changed_origin_and_settings_reject_formatting_results() {
        for change in [
            "cancel",
            "selection",
            "settings",
            "revision",
            "language",
            "view",
            "prepared",
            "prepared-settings",
            "prepared-language",
        ] {
            let (_directory, mut app, service, receiver, _) = fixture("normal");
            app.execute("format").unwrap();
            flush(&mut app, &service);
            let answer = reply(&mut app, &receiver);
            assert!(matches!(
                &answer,
                Event::Answer {
                    result: Ok(Answer::Formatted { .. }),
                    ..
                }
            ));
            let mut prepared = None;
            if change.starts_with("prepared") {
                app.handle_lsp_event(answer);
                prepared = Some(app.take_workspace_edit().unwrap().run().unwrap());
                match change {
                    "prepared-settings" => app.editor.set_tab_width(NonZeroUsize::new(3).unwrap()),
                    "prepared-language" => app.editor.set_language(Some(Language::TypeScript)),
                    _ => key(&mut app, KeyCode::Esc, KeyModifiers::NONE),
                }
            } else {
                match change {
                    "cancel" => key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL),
                    "selection" => {
                        app.editor.execute("move_right", 1).unwrap();
                    }
                    "settings" => {
                        app.editor.set_tab_width(NonZeroUsize::new(3).unwrap());
                    }
                    "revision" => {
                        app.editor.execute("insert_mode", 1).unwrap();
                        app.editor.insert_text("x").unwrap();
                    }
                    "language" => app.editor.set_language(Some(Language::TypeScript)),
                    "view" => app.execute("vsplit").unwrap(),
                    _ => unreachable!(),
                }
                app.handle_lsp_event(answer);
            }
            if let Some(result) = prepared {
                app.handle_workspace_edit(result);
            }
            app.take_lsp_update();
            assert!(!app.input_waiting(), "{change}");
            assert!(app.take_workspace_edit().is_none(), "{change}");
            assert_eq!(
                app.editor.document().text().to_string(),
                if change == "revision" {
                    format!("x{ORIGINAL}")
                } else {
                    ORIGINAL.into()
                },
                "{change}"
            );
        }
    }

    #[test]
    fn formatting_without_a_named_file_or_language_service_reports_a_useful_error() {
        let mut app = App::from_document(vex_core::Document::from("text"), (80, 24));
        app.execute("format").unwrap();
        assert!(app.take_lsp_update().is_none());
        assert!(app.error && app.message.contains("not enabled"));
        app.enable_lsp();
        app.editor.set_language(Some(Language::Rust));
        app.clear_message();
        key(&mut app, KeyCode::Char('='), KeyModifiers::NONE);
        app.take_lsp_update();
        assert!(app.error && app.message.contains("named file"));
        assert!(!app.input_waiting());
        assert_eq!(app.editor.document().text(), "text");
    }

    #[test]
    fn cancelling_an_outstanding_format_request_sends_protocol_cancellation() {
        let (directory, mut app, service, receiver, _) = fixture("hold");
        app.execute("format").unwrap();
        flush(&mut app, &service);
        let log = directory.path().join("events.log");
        let wait_for = |needle: &str| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !fs::read_to_string(&log)
                .unwrap_or_default()
                .contains(needle)
            {
                assert!(Instant::now() < deadline, "missing {needle}");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        wait_for("textDocument/formatting");
        assert!(app.input_waiting());
        key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        flush(&mut app, &service);
        wait_for("$/cancelRequest");
        drop(service);
        for event in receiver.try_iter() {
            app.handle_lsp_event(event);
        }
        assert!(!app.input_waiting());
        assert!(app.take_workspace_edit().is_none());
        assert_eq!(app.editor.document().text(), ORIGINAL);
    }

    #[test]
    #[ignore = "manual release-mode formatting submission benchmark"]
    fn benchmark_formatting_submission() {
        use std::hint::black_box;
        for mib in [1usize, 8] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("buffer.txt");
            fs::write(&path, "foo \n".repeat((mib << 20) / 5)).unwrap();
            for count in [1usize, 1000] {
                let mut app = App::open(Some(&path), (100, 24)).unwrap();
                app.editor.set_background_syntax(true);
                app.editor.set_language(Some(Language::Rust));
                app.editor
                    .set_selections(
                        SelectionSet::new(
                            (0..count)
                                .map(|i| Selection::new(CharOffset(i * 5), CharOffset(i * 5 + 1)))
                                .collect(),
                            0,
                        )
                        .unwrap(),
                    )
                    .unwrap();
                app.editor.duplicate_view();
                app.editor.duplicate_view();
                app.enable_lsp();
                app.take_lsp_update();
                let mut samples = Vec::with_capacity(500);
                for _ in 0..500 {
                    let start = Instant::now();
                    app.editor.execute("format_document", 1).unwrap();
                    let update = black_box(app.take_lsp_update().unwrap());
                    samples.push(start.elapsed());
                    app.cancel_language_request();
                    drop(update);
                }
                samples.sort_unstable();
                eprintln!(
                    "format submission {mib}MiB, {count} selections/view, 3 views: median {:?}, p95 {:?}",
                    samples[250], samples[474]
                );
            }
        }
    }
}
