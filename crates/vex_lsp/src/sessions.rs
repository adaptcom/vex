//! Bounded workspace/server lifetimes, multiplexed on the existing executor.

use super::*;

const MAX_SESSIONS: usize = 8;

#[derive(Clone, PartialEq, Eq)]
struct Key {
    root: PathBuf,
    program: PathBuf,
    command: &'static str,
    arguments: &'static [&'static str],
    environment: &'static str,
}

impl Key {
    fn for_document(document: &Document, program: Option<&Path>) -> Option<Self> {
        let server = document.language.server()?;
        Some(Self {
            root: root(&document.path, document.language),
            program: program
                .map(PathBuf::from)
                .or_else(|| std::env::var_os(server.environment).map(PathBuf::from))
                .unwrap_or_else(|| server.command.into()),
            command: server.command,
            arguments: server.arguments,
            environment: server.environment,
        })
    }
}

type SessionFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + 'a>>;

struct Session<'a> {
    key: Key,
    inbox: Inbox,
    future: Option<SessionFuture<'a>>,
    failure: Option<String>,
    epoch: u64,
    stopping: bool,
    used: u64,
}

pub(super) async fn serve(
    program: Option<&Path>,
    inbox: &Inbox,
    executor: &Executor,
    emit: &impl Fn(Event),
) {
    let catalog = diagnostics::Catalog::default();
    emit(Event::DiagnosticCatalog(catalog.clone()));
    let mut sessions: Vec<Session<'_>> = Vec::new();
    let mut active: Option<Key> = None;
    let mut pending: Option<Update> = None;
    let mut workspace: Option<WorkspaceUpdate> = None;
    let mut restart = 0;
    let mut sequence = 0;
    let mut stopping = false;
    poll_fn(|cx| {
        // Route UI work before polling individual servers. A slow initialization
        // or request in one workspace cannot hold up routing to another.
        match inbox.poll(cx, executor, Instant::now()) {
            Poll::Ready(Input::Stop) => {
                stopping = true;
                pending = None;
                for session in &mut sessions {
                    session.stopping = true;
                    session.inbox.stop();
                }
            }
            Poll::Ready(Input::Update(update)) if !stopping => pending = Some(update),
            _ => {}
        }
        if let Some(update) = inbox.take_workspace()
            && workspace
                .as_ref()
                .is_none_or(|old| update.generation > old.generation)
        {
            workspace = Some(update.clone());
            for session in &sessions {
                session.inbox.update_workspace(update.clone());
            }
        }
        if let Some(update) = pending.take() {
            let oversized = update
                .document
                .as_ref()
                .is_some_and(|doc| doc.snapshot.text().len_bytes() > MAX_DOCUMENT_BYTES);
            let key = update
                .document
                .as_ref()
                .filter(|_| !oversized)
                .and_then(|document| {
                    // Ordinary edits never rediscover the project on disk.
                    active
                        .as_ref()
                        .filter(|key| {
                            document.restart == restart
                                && sessions.iter().any(|session| {
                                    &session.key == *key && session.epoch == document.epoch
                                })
                        })
                        .cloned()
                        .or_else(|| Key::for_document(document, program))
                });
            if active != key {
                for session in &sessions {
                    if Some(&session.key) == active.as_ref() {
                        session.inbox.route(Update {
                            document: None,
                            request: None,
                        });
                    }
                }
                active = key.clone();
            }
            if let Some(document) = &update.document
                && document.restart > restart
            {
                restart = document.restart;
                if let Some(session) = sessions
                    .iter_mut()
                    .find(|session| Some(&session.key) == key.as_ref())
                {
                    session.stopping = true;
                    session.inbox.stop();
                }
            }
            if oversized {
                let document = update.document.as_ref().unwrap();
                let message = "document exceeds the 8 MiB LSP limit".to_owned();
                emit(Event::Status {
                    epoch: document.epoch,
                    message: message.clone(),
                    failed: true,
                });
                if let Some(request) = update.request {
                    emit(Event::Answer {
                        epoch: document.epoch,
                        revision: document.snapshot.revision(),
                        id: request.id,
                        result: Err(message),
                    });
                }
                // Large files do not retire a healthy server for other buffers.
            } else if let (Some(key), Some(document)) = (key, update.document.as_ref()) {
                sequence += 1;
                if let Some(session) = sessions.iter_mut().find(|session| session.key == key) {
                    if session.stopping {
                        pending = Some(update);
                    } else {
                        let changed = session.epoch != document.epoch;
                        session.epoch = document.epoch;
                        session.used = sequence;
                        if let Some(error) = &session.failure {
                            if changed || update.request.is_some() {
                                emit(Event::Status {
                                    epoch: document.epoch,
                                    message: error.clone(),
                                    failed: true,
                                });
                            }
                            if let Some(request) = update.request {
                                emit(Event::Answer {
                                    epoch: document.epoch,
                                    revision: document.snapshot.revision(),
                                    id: request.id,
                                    result: Err(error.clone()),
                                });
                            }
                        } else {
                            session.inbox.route(update);
                        }
                    }
                } else if sessions.len() == MAX_SESSIONS {
                    if !sessions.iter().any(|session| session.stopping) {
                        let oldest = sessions
                            .iter_mut()
                            .min_by_key(|session| session.used)
                            .unwrap();
                        oldest.stopping = true;
                        oldest.inbox.stop();
                    }
                    pending = Some(update);
                } else {
                    let epoch = document.epoch;
                    emit(Event::Status {
                        epoch,
                        message: format!("{} starting", key.command),
                        failed: false,
                    });
                    let child = Inbox::default();
                    if let Some(workspace) = &workspace {
                        child.update_workspace(workspace.clone());
                    }
                    let shared = child.clone();
                    let program = key.program.clone();
                    let catalog = catalog.clone();
                    let future = Box::pin(async move {
                        session(
                            &program,
                            &shared,
                            executor,
                            update.document.unwrap(),
                            update.request,
                            &catalog,
                            emit,
                        )
                        .await
                    });
                    sessions.push(Session {
                        key,
                        inbox: child,
                        future: Some(future),
                        failure: None,
                        epoch,
                        stopping: false,
                        used: sequence,
                    });
                }
            }
        }
        let mut index = 0;
        while index < sessions.len() {
            let session = &mut sessions[index];
            if let Some(future) = &mut session.future
                && let Poll::Ready(result) = future.as_mut().poll(cx)
            {
                session.future = None;
                if let Err(error) = result {
                    let message = format!("{}: {error}", session.key.command);
                    emit(Event::Status {
                        epoch: session.epoch,
                        message: message.clone(),
                        failed: true,
                    });
                    session.failure = Some(message);
                }
            }
            if session.stopping && session.future.is_none() {
                sessions.remove(index);
                cx.waker().wake_by_ref();
            } else {
                index += 1;
            }
        }
        if stopping && sessions.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt, sync::mpsc};
    use vex_core::{CharOffset, Document as TextDocument, Selection, SelectionSet};

    fn mock(directory: &Path) -> PathBuf {
        let path = directory.join("persistent.py");
        fs::write(&path, r#"#!/usr/bin/env python3
import json, os, sys
log = open(os.path.join(os.path.dirname(__file__), 'sessions.jsonl'), 'a', buffering=1)
documents = {}
pid = os.getpid()
def record(value):
    log.write(json.dumps({'pid':pid,'root':os.getcwd(),**value}) + '\n')
def send(value):
    value['jsonrpc'] = '2.0'
    body = json.dumps(value).encode()
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n' % len(body)).encode() + body)
    sys.stdout.buffer.flush()
def diagnostics(uri, document):
    send({'method':'textDocument/publishDiagnostics','params':{'uri':uri,'version':document['version'],
        'diagnostics':[{'range':{'start':{'line':0,'character':0},'end':{'line':0,'character':1}},
            'severity':2,'message':document['text'],'data':{'owner':uri}}]}})
record({'method':'launch'})
while True:
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        if line == b'\r\n': break
        if line.lower().startswith(b'content-length:'): length = int(line.split(b':')[1])
    value = json.loads(sys.stdin.buffer.read(length))
    record(value)
    method = value.get('method'); params = value.get('params', {})
    if method == 'initialize':
        if os.path.exists('hold-initialize'): continue
        send({'id':value['id'],'result':{'capabilities':{'textDocumentSync':{'openClose':True,'change':2,'save':True},
            'hoverProvider':True,'completionProvider':{},'codeActionProvider':True}}})
    elif method == 'textDocument/didOpen':
        doc = params['textDocument']; uri = doc['uri']
        assert uri not in documents, 'duplicate didOpen'
        documents[uri] = doc
        diagnostics(uri, doc)
    elif method == 'textDocument/didChange':
        uri = params['textDocument']['uri']; doc = documents[uri]
        assert params['textDocument']['version'] > doc['version'], 'wire version went backwards'
        doc['version'] = params['textDocument']['version']; doc['text'] = params['contentChanges'][0]['text']
        diagnostics(uri, doc)
    elif method == 'textDocument/didClose':
        documents.pop(params['textDocument']['uri'], None)
    elif method == 'textDocument/hover':
        uri = params['textDocument']['uri']
        if params['position']['character'] == 2: continue
        if params['position']['character'] == 4: sys.exit(1)
        if params['position']['character'] == 3:
            for other, doc in documents.items(): diagnostics(other, doc)
        send({'id':value['id'],'result':{'contents':{'kind':'plaintext','value':json.dumps({'pid':pid,'documents':documents})}}})
    elif method == 'textDocument/codeAction':
        uri = params['textDocument']['uri']
        assert params['context']['diagnostics'][0]['data']['owner'] == uri, 'opaque diagnostic context lost'
        send({'id':value['id'],'result':[]})
    elif method == 'shutdown': send({'id':value['id'],'result':None})
    elif method == 'exit': sys.exit(0)
"#).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn document(directory: &Path, name: &str, text: &TextDocument, epoch: u64) -> Document {
        Document {
            epoch,
            restart: 0,
            language: Language::Rust,
            path: directory.join(name),
            snapshot: text.snapshot(),
            saved: 0,
            saved_snapshot: None,
        }
    }

    fn request(id: u64, kind: RequestKind, position: usize) -> Request {
        Request {
            id,
            kind,
            position: CharOffset(position),
            cancellation: Cancellation::default(),
        }
    }

    fn until(receiver: &mpsc::Receiver<Event>, accept: impl Fn(&Event) -> bool) -> Event {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let event = receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("LSP event timed out");
            if let Event::Status {
                failed: true,
                message,
                ..
            } = &event
            {
                panic!("{message}");
            }
            if accept(&event) {
                return event;
            }
        }
    }

    fn hover(service: &Service, receiver: &mpsc::Receiver<Event>, document: &Document) -> Value {
        service.update(Update {
            document: Some(document.clone()),
            request: Some(request(document.epoch, RequestKind::Hover, 1)),
        });
        let Event::Answer { result, .. } = until(
            receiver,
            |event| matches!(event, Event::Answer { id, .. } if *id == document.epoch),
        ) else {
            unreachable!()
        };
        let Answer::Hover(text) = result.unwrap() else {
            panic!("expected hover")
        };
        serde_json::from_str(&text.plain_text()).unwrap()
    }

    fn capture(service: &Service, generation: u64, documents: &[&Document]) {
        service.update_workspace(WorkspaceUpdate {
            epoch: generation,
            generation,
            documents: documents
                .iter()
                .map(|doc| WorkspaceDocument {
                    path: doc.path.clone(),
                    language: Some(doc.language),
                    snapshot: doc.snapshot.clone(),
                })
                .collect(),
        });
    }

    fn messages(directory: &Path) -> Vec<Value> {
        fs::read_to_string(directory.join("sessions.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn focus_changes_preserve_process_documents_versions_diagnostics_and_code_action_context() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (send, receive) = mpsc::channel();
        let service = Service::with_program(mock(&root), move |event| {
            let _ = send.send(event);
        })
        .unwrap();
        let mut a_text = TextDocument::from("alpha");
        let b_text = TextDocument::from("bravo");
        let mut a = document(&root, "a.rs", &a_text, 1);
        let mut b = document(&root, "b.rs", &b_text, 2);
        let first = hover(&service, &receive, &a);
        let b_state = hover(&service, &receive, &b);
        assert_eq!(first["pid"], b_state["pid"]);
        assert_eq!(b_state["documents"].as_object().unwrap().len(), 2);
        // Return without a didChange: cached diagnostics and opaque code-action
        // data must remain available even if the server does not republish.
        a.epoch = 3;
        service.update(Update {
            document: Some(a.clone()),
            request: None,
        });
        let Event::Diagnostics { diagnostics, .. } = until(&receive, |event| {
            matches!(event, Event::Diagnostics { epoch: 3, .. })
        }) else {
            unreachable!()
        };
        assert_eq!(diagnostics[0].message, "alpha");
        let docs = vec![
            WorkspaceDocument {
                path: a.path.clone(),
                language: Some(a.language),
                snapshot: a.snapshot.clone(),
            },
            WorkspaceDocument {
                path: b.path.clone(),
                language: Some(b.language),
                snapshot: b.snapshot.clone(),
            },
        ]
        .into();
        service.update(Update {
            document: Some(a.clone()),
            request: Some(request(
                10,
                RequestKind::CodeActions {
                    selection: Selection::new(CharOffset(0), CharOffset(1)),
                    documents: docs,
                },
                0,
            )),
        });
        assert!(matches!(
            until(&receive, |event| matches!(
                event,
                Event::Answer { id: 10, .. }
            )),
            Event::Answer {
                result: Ok(Answer::CodeActions(_)),
                ..
            }
        ));
        // The final edit can still be inside the debounce when focus leaves.
        let edit = a_text
            .transaction([vex_core::Edit::new(CharOffset(0)..CharOffset(5), "updated")])
            .unwrap();
        a_text
            .apply(
                edit,
                &mut SelectionSet::single(Selection::cursor(CharOffset(0))),
            )
            .unwrap();
        a.snapshot = a_text.snapshot();
        service.update(Update {
            document: Some(a.clone()),
            request: None,
        });
        capture(&service, 1, &[&a, &b]);
        b.epoch = 4;
        let state = hover(&service, &receive, &b);
        let a_uri = file_uri(&a.path).unwrap();
        assert_eq!(state["documents"][&a_uri]["text"], "updated");
        assert_eq!(state["documents"][&a_uri]["version"], 1);
        // Scratch/plain-text focus keeps both documents on the same process.
        service.update(Update {
            document: None,
            request: None,
        });
        a.epoch = 5;
        assert_eq!(hover(&service, &receive, &a)["pid"], first["pid"]);
        capture(&service, 2, &[&a]);
        a.epoch = 6;
        assert_eq!(
            hover(&service, &receive, &a)["documents"]
                .as_object()
                .unwrap()
                .len(),
            1
        );
        drop(service);
        let log = messages(&root);
        assert_eq!(
            log.iter().filter(|m| m["method"] == "initialize").count(),
            1
        );
        assert_eq!(
            log.iter()
                .filter(|m| m["method"] == "textDocument/didOpen")
                .count(),
            2
        );
        assert_eq!(log.iter().filter(|m| m["method"] == "shutdown").count(), 1);
        assert!(log.iter().any(|m| m["method"] == "textDocument/didClose"
            && m["params"]["textDocument"]["uri"] == file_uri(&b.path).unwrap()));
    }

    #[test]
    fn separate_roots_and_server_configurations_persist_and_restart_independently() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let other = root.join("other");
        fs::create_dir(&other).unwrap();
        let (send, receive) = mpsc::channel();
        let service = Service::with_program(mock(&root), move |event| {
            let _ = send.send(event);
        })
        .unwrap();
        let text = TextDocument::from("hello");
        let mut a = document(&root, "a.rs", &text, 1);
        let mut b = document(&other, "b.rs", &text, 2);
        let a_pid = hover(&service, &receive, &a)["pid"].clone();
        let b_pid = hover(&service, &receive, &b)["pid"].clone();
        assert_ne!(a_pid, b_pid);
        a.epoch = 3;
        assert_eq!(hover(&service, &receive, &a)["pid"], a_pid);
        a.epoch = 4;
        a.restart = 1;
        assert_ne!(hover(&service, &receive, &a)["pid"], a_pid);
        b.epoch = 5;
        b.restart = 1;
        assert_eq!(hover(&service, &receive, &b)["pid"], b_pid);
        let mut js = document(&root, "file.js", &text, 6);
        js.restart = 1;
        js.language = Language::JavaScript;
        let js_pid = hover(&service, &receive, &js)["pid"].clone();
        let mut ts = document(&root, "file.ts", &text, 7);
        ts.restart = 1;
        ts.language = Language::TypeScript;
        assert_eq!(hover(&service, &receive, &ts)["pid"], js_pid);
        assert_ne!(js_pid, a_pid);
        drop(service);
        let log = messages(&root);
        assert_eq!(
            log.iter().filter(|m| m["method"] == "initialize").count(),
            4
        );
        assert_eq!(log.iter().filter(|m| m["method"] == "shutdown").count(), 4);
    }

    #[test]
    fn stalled_initialization_and_requests_do_not_block_other_workspaces_or_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let slow = root.join("slow");
        fs::create_dir(&slow).unwrap();
        fs::write(slow.join("hold-initialize"), "").unwrap();
        let (send, receive) = mpsc::channel();
        let service = Service::with_program(mock(&root), move |event| {
            let _ = send.send(event);
        })
        .unwrap();
        let text = TextDocument::from("hello");
        let a = document(&slow, "a.rs", &text, 1);
        let mut b = document(&root, "b.rs", &text, 2);
        service.update(Update {
            document: Some(a),
            request: None,
        });
        until(&receive, |event| {
            matches!(event, Event::Status { epoch: 1, .. })
        });
        let initial = hover(&service, &receive, &b);
        service.update(Update {
            document: Some(b.clone()),
            request: Some(request(10, RequestKind::Hover, 2)),
        });
        b.epoch = 3;
        assert_eq!(hover(&service, &receive, &b)["pid"], initial["pid"]);
        let started = Instant::now();
        drop(service);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn session_pool_evicts_the_least_recently_used_server_at_its_bound() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (send, receive) = mpsc::channel();
        let service = Service::with_program(mock(&root), move |event| {
            let _ = send.send(event);
        })
        .unwrap();
        let text = TextDocument::from("hello");
        let mut documents = Vec::new();
        let mut pids = Vec::new();
        for index in 0..MAX_SESSIONS {
            let project = root.join(format!("project-{index}"));
            fs::create_dir(&project).unwrap();
            let doc = document(&project, "file.rs", &text, index as u64 + 1);
            pids.push(hover(&service, &receive, &doc)["pid"].clone());
            documents.push(doc);
        }
        documents[0].epoch = 20;
        assert_eq!(hover(&service, &receive, &documents[0])["pid"], pids[0]);
        let ninth = document(&root, "extra.rs", &text, 21);
        hover(&service, &receive, &ninth);
        documents[0].epoch = 22;
        assert_eq!(hover(&service, &receive, &documents[0])["pid"], pids[0]);
        documents[1].epoch = 23;
        assert_ne!(hover(&service, &receive, &documents[1])["pid"], pids[1]);
        drop(service);
        let log = messages(&root);
        assert_eq!(
            log.iter().filter(|m| m["method"] == "initialize").count(),
            MAX_SESSIONS + 2
        );
        assert_eq!(
            log.iter().filter(|m| m["method"] == "shutdown").count(),
            MAX_SESSIONS + 2
        );
    }

    #[test]
    fn closing_and_reopening_a_uri_advances_versions_and_oversized_buffers_keep_servers_alive() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (send, receive) = mpsc::channel();
        let service = Service::with_program(mock(&root), move |event| {
            let _ = send.send(event);
        })
        .unwrap();
        let text = TextDocument::from("hello");
        let mut a = document(&root, "a.rs", &text, 1);
        let mut b = document(&root, "b.rs", &text, 2);
        let original = hover(&service, &receive, &a);
        hover(&service, &receive, &b);
        capture(&service, 1, &[&b]);
        b.epoch = 3;
        assert_eq!(
            hover(&service, &receive, &b)["documents"]
                .as_object()
                .unwrap()
                .len(),
            1
        );
        a.epoch = 4;
        let reopened = hover(&service, &receive, &a);
        let uri = file_uri(&a.path).unwrap();
        assert_eq!(reopened["pid"], original["pid"]);
        assert!(
            reopened["documents"][&uri]["version"].as_i64().unwrap()
                > original["documents"][&uri]["version"].as_i64().unwrap()
        );
        let huge = TextDocument::from("x".repeat(MAX_DOCUMENT_BYTES + 1).as_str());
        let large = document(&root, "large.rs", &huge, 5);
        service.update(Update {
            document: Some(large.clone()),
            request: None,
        });
        loop {
            if matches!(
                receive.recv_timeout(Duration::from_secs(5)).unwrap(),
                Event::Status {
                    epoch: 5,
                    failed: true,
                    ..
                }
            ) {
                break;
            }
        }
        capture(&service, 2, &[&a, &b, &large]);
        a.epoch = 6;
        let state = hover(&service, &receive, &a);
        assert_eq!(state["pid"], original["pid"]);
        assert_eq!(state["documents"].as_object().unwrap().len(), 2);
    }

    #[test]
    fn a_failed_server_does_not_stop_other_workspaces_and_explicit_restart_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let other = root.join("other");
        fs::create_dir(&other).unwrap();
        let (send, receive) = mpsc::channel();
        let service = Service::with_program(mock(&root), move |event| {
            let _ = send.send(event);
        })
        .unwrap();
        let text = TextDocument::from("hello");
        let mut a = document(&root, "a.rs", &text, 1);
        let mut b = document(&other, "b.rs", &text, 2);
        let a_pid = hover(&service, &receive, &a)["pid"].clone();
        let b_pid = hover(&service, &receive, &b)["pid"].clone();
        a.epoch = 3;
        service.update(Update {
            document: Some(a.clone()),
            request: Some(request(10, RequestKind::Hover, 4)),
        });
        loop {
            if matches!(
                receive.recv_timeout(Duration::from_secs(5)).unwrap(),
                Event::Status {
                    epoch: 3,
                    failed: true,
                    ..
                }
            ) {
                break;
            }
        }
        b.epoch = 4;
        assert_eq!(hover(&service, &receive, &b)["pid"], b_pid);
        a.epoch = 5;
        a.restart = 1;
        assert_ne!(hover(&service, &receive, &a)["pid"], a_pid);
        b.epoch = 6;
        b.restart = 1;
        assert_eq!(hover(&service, &receive, &b)["pid"], b_pid);
    }
}
