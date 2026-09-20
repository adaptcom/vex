//! Per-session synchronization for requests that can edit multiple buffers.

use crate::{
    Document, Executor, file_uri, transport::Transport, workspace_edit::SynchronizedDocument,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    task::Context,
};
use vex_core::Snapshot;
use vex_editor::Language;

/// A cheap snapshot of an open buffer, including hidden, unsaved buffers.
#[derive(Clone, Debug)]
pub struct WorkspaceDocument {
    pub path: PathBuf,
    pub language: Option<Language>,
    pub snapshot: Snapshot,
}

/// Latest post-application buffer state for an existing server session. It is
/// delivered before subsequent requests and coalesced independently of cursor
/// requests, so movement or cancellation cannot discard synchronization.
#[derive(Clone, Debug)]
pub struct WorkspaceUpdate {
    pub epoch: u64,
    /// Monotonic frontend capture sequence, shared with application replies.
    pub generation: u64,
    pub documents: std::sync::Arc<[WorkspaceDocument]>,
}

pub(crate) fn validate_origin(
    active: &Document,
    captured: &[WorkspaceDocument],
) -> Result<(), String> {
    if !captured.iter().any(|doc| {
        doc.path == active.path
            && doc.snapshot.id() == active.snapshot.id()
            && doc.snapshot.revision() == active.snapshot.revision()
    }) {
        return Err("workspace capture is out of date or missing the active buffer".into());
    }
    Ok(())
}

struct OpenDocument {
    snapshot: Snapshot,
    version: i32,
    language: Language,
}

#[derive(Default)]
pub(crate) struct Workspace {
    documents: BTreeMap<PathBuf, OpenDocument>,
}

impl Workspace {
    pub(crate) fn versions(&self, active: &Document, version: i32) -> Vec<SynchronizedDocument> {
        std::iter::once(SynchronizedDocument {
            path: active.path.clone(),
            version,
            document: active.snapshot.id(),
            revision: active.snapshot.revision(),
        })
        .chain(
            self.documents
                .iter()
                .map(|(path, doc)| SynchronizedDocument {
                    path: path.clone(),
                    version: doc.version,
                    document: doc.snapshot.id(),
                    revision: doc.snapshot.revision(),
                }),
        )
        .collect()
    }
    /// Synchronize only this server's languages inside its workspace. Each
    /// queued message is bounded by the normal document limit; capacity wakes
    /// the executor, so a slow server never causes a busy loop or unbounded queue.
    #[allow(clippy::too_many_arguments)]
    pub async fn synchronize(
        &mut self,
        transport: &Transport,
        executor: &Executor,
        active: &Document,
        active_version: i32,
        root: &Path,
        captured: &[WorkspaceDocument],
        interrupted: impl Fn(&Context<'_>) -> bool,
    ) -> Result<Vec<SynchronizedDocument>, String> {
        let server = active.language.server().unwrap();
        if captured.len() > 4096 {
            return Err("workspace synchronization exceeds 4096 buffers".into());
        }
        let mut bytes = 0usize;
        let mut relevant = BTreeMap::new();
        // Validate the entire capture before sending. Unrelated dirty buffers
        // remain protected by the frontend's workspace-edit validation.
        // Active edits may have advanced while an applied workspace update was
        // queued. The session already synchronized that document separately.
        let origin = WorkspaceDocument {
            path: active.path.clone(),
            language: Some(active.language),
            snapshot: active.snapshot.clone(),
        };
        for doc in captured
            .iter()
            .filter(|doc| doc.path != active.path)
            .chain(std::iter::once(&origin))
        {
            let Some(language) = doc.language else {
                continue;
            };
            let Some(other) = language.server() else {
                continue;
            };
            if !doc.path.starts_with(root)
                || server.command != other.command
                || server.arguments != other.arguments
                || server.environment != other.environment
            {
                continue;
            }
            if doc.snapshot.text().len_bytes() > crate::MAX_DOCUMENT_BYTES {
                return Err(format!(
                    "buffer exceeds the 8 MiB LSP limit: {}",
                    doc.path.display()
                ));
            }
            if self.documents.get(&doc.path).is_some_and(|current| {
                current.snapshot.id() == doc.snapshot.id()
                    && current.snapshot.revision() > doc.snapshot.revision()
            }) {
                return Err(format!(
                    "workspace capture is out of date: {}",
                    doc.path.display()
                ));
            }
            bytes = bytes.saturating_add(doc.snapshot.text().len_bytes());
            if bytes > 64 << 20 {
                return Err("workspace synchronization exceeds 64 MiB".into());
            }
            if relevant.insert(doc.path.clone(), (doc, language)).is_some() {
                return Err("duplicate workspace buffer path".into());
            }
        }
        let closed: Vec<_> = self
            .documents
            .iter()
            .filter(|(path, old)| {
                !relevant
                    .get(*path)
                    .is_some_and(|(_, language)| *language == old.language)
            })
            .map(|(path, _)| path.clone())
            .collect();
        for path in closed {
            transport
                .notify_wait(
                    executor,
                    "textDocument/didClose",
                    json!({"textDocument":{"uri":file_uri(&path).map_err(|e| e.to_string())?}}),
                    &interrupted,
                )
                .await?;
            self.documents.remove(&path);
        }
        let mut versions = Vec::with_capacity(relevant.len());
        for (path, (doc, language)) in relevant {
            let version = if path == active.path {
                active_version
            } else {
                let uri = file_uri(&path).map_err(|e| e.to_string())?;
                match self.documents.get_mut(&path) {
                    Some(old) => {
                        if old.snapshot.id() != doc.snapshot.id()
                            || old.snapshot.revision() != doc.snapshot.revision()
                        {
                            let version = old
                                .version
                                .checked_add(1)
                                .ok_or("LSP document version exhausted")?;
                            transport
                                .notify_wait(
                                    executor,
                                    "textDocument/didChange",
                                    json!({
                                        "textDocument":{"uri":uri,"version":version},
                                        "contentChanges":[{"text":doc.snapshot.text().to_string()}]
                                    }),
                                    &interrupted,
                                )
                                .await?;
                            old.snapshot = doc.snapshot.clone();
                            old.version = version;
                        }
                        old.version
                    }
                    None => {
                        transport.notify_wait(executor, "textDocument/didOpen", json!({"textDocument":{
                            "uri":uri,"languageId":language.language_id(),"version":0,"text":doc.snapshot.text().to_string()
                        }}), &interrupted).await?;
                        self.documents.insert(
                            path.clone(),
                            OpenDocument {
                                snapshot: doc.snapshot.clone(),
                                version: 0,
                                language,
                            },
                        );
                        0
                    }
                }
            };
            versions.push(SynchronizedDocument {
                path,
                version,
                document: doc.snapshot.id(),
                revision: doc.snapshot.revision(),
            });
        }
        Ok(versions)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{Answer, Event, Request, RequestKind, Service, Update};
    use serde_json::Value;
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        sync::{Arc, mpsc},
        time::Duration,
    };
    use vex_core::{CharOffset, Document as TextDocument, Selection, SelectionSet};
    use vex_editor::background::Cancellation;

    fn server(directory: &Path, prepare: bool) -> PathBuf {
        let path = directory.join("rename.py");
        fs::write(&path, r#"#!/usr/bin/env python3
import json, sys, os
documents = {}
log = open(os.path.join(os.path.dirname(__file__), 'wire.jsonl'), 'w', buffering=1)
def send(value):
    value['jsonrpc'] = '2.0'
    body = json.dumps(value).encode()
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n' % len(body)).encode() + body)
    sys.stdout.buffer.flush()
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
        assert params['capabilities']['textDocument']['rename']['prepareSupport']
        assert params['capabilities']['workspace']['workspaceEdit']['documentChanges']
        assert 'resourceOperations' not in params['capabilities']['workspace']['workspaceEdit']
        send({'id':value['id'], 'result':{'capabilities':{'textDocumentSync':2, 'hoverProvider':True, 'renameProvider':{'prepareProvider':PREPARE}}}})
    elif method == 'textDocument/didOpen':
        doc = params['textDocument']; assert doc['uri'] not in documents
        documents[doc['uri']] = doc
        send({'id':doc['uri'],'method':'workspace/configuration','params':{'items':[]}})
        send({'method':'textDocument/publishDiagnostics','params':{'uri':doc['uri'],'version':doc['version'],'diagnostics':[]}})
    elif method == 'textDocument/didChange':
        doc = documents[params['textDocument']['uri']]
        assert params['textDocument']['version'] == doc['version'] + 1
        doc['version'] += 1; doc['text'] = params['contentChanges'][0]['text']
    elif method == 'textDocument/didClose': documents.pop(params['textDocument']['uri'])
    elif method == 'textDocument/prepareRename':
        assert PREPARE
        if params['position']['character'] == 1:
            send({'id':value['id'], 'result':None})
        else:
            send({'id':value['id'], 'result':{'range':{'start':{'line':0,'character':0},'end':{'line':0,'character':3}},'placeholder':'foo'}})
    elif method == 'textDocument/hover':
        send({'id':value['id'],'result':{'contents':'|'.join(doc['text'] for doc in documents.values())}})
    elif method == 'textDocument/rename':
        if params['newName'] == 'error':
            send({'id':value['id'],'error':{'code':-32602,'message':'invalid name'}})
            continue
        edits = []
        for uri, doc in documents.items():
            start = doc['text'].index('foo'); prefix = doc['text'][:start]
            line = prefix.count('\n'); column = len(prefix.split('\n')[-1].encode('utf-16-le')) // 2
            edits.append({'textDocument':{'uri':uri,'version':doc['version']}, 'edits':[{'range':{'start':{'line':line,'character':column},'end':{'line':line,'character':column + 3}},'newText':params['newName']}]})
        send({'id':value['id'],'result':{'documentChanges':edits}})
    elif method == 'shutdown': send({'id':value['id'],'result':None})
    elif method == 'exit': break
"#.replace("PREPARE", if prepare { "True" } else { "False" })).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn answer(receiver: &mpsc::Receiver<Event>, id: u64) -> Result<Answer, String> {
        loop {
            match receiver
                .recv_timeout(Duration::from_secs(10))
                .expect("missing language reply")
            {
                Event::Answer {
                    id: found, result, ..
                } if found == id => return result,
                Event::Status {
                    failed: true,
                    message,
                    ..
                } => panic!("{message}"),
                _ => {}
            }
        }
    }

    #[test]
    fn rename_syncs_many_hidden_snapshots_tracks_versions_closes_missing_buffers_and_reuses_unchanged_text()
     {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (sender, receiver) = mpsc::channel();
        let service = Service::with_program(server(&root, true), move |event| {
            let _ = sender.send(event);
        })
        .unwrap();
        let mut texts: Vec<_> = (0..160).map(|_| TextDocument::from("foo\n")).collect();
        let mut capture: Vec<_> = texts
            .iter()
            .enumerate()
            .map(|(i, text)| WorkspaceDocument {
                path: root.join(format!("{i}.rs")),
                language: Some(Language::Rust),
                snapshot: text.snapshot(),
            })
            .collect();
        // Unrelated languages and other workspaces are not opened in this session.
        capture.push(WorkspaceDocument {
            path: root.join("script.sh"),
            language: Some(Language::Bash),
            snapshot: TextDocument::from("foo").snapshot(),
        });
        capture.push(WorkspaceDocument {
            path: root.with_extension("outside").join("other.rs"),
            language: Some(Language::Rust),
            snapshot: TextDocument::from("foo").snapshot(),
        });
        let document = Document {
            epoch: 1,
            language: Language::Rust,
            path: capture[0].path.clone(),
            snapshot: texts[0].snapshot(),
            saved: 0,
            saved_snapshot: None,
        };
        let submit = |id, kind, position| {
            service.update(Update {
                document: Some(document.clone()),
                request: Some(Request {
                    id,
                    kind,
                    position: CharOffset(position),
                    cancellation: Cancellation::default(),
                }),
            })
        };
        submit(
            1,
            RequestKind::PrepareRename {
                selection: Selection::new(CharOffset(0), CharOffset(1)),
                documents: capture.clone().into(),
            },
            0,
        );
        assert!(
            matches!(answer(&receiver, 1).unwrap(), Answer::RenamePrepared(name) if name == "foo")
        );
        let mut selections = SelectionSet::default();
        let change = texts[1]
            .replace_selections(&selections, "// unsaved 😀\n")
            .unwrap();
        texts[1].apply(change, &mut selections).unwrap();
        capture[1].snapshot = texts[1].snapshot();
        let closed = capture.remove(159).path;
        submit(
            2,
            RequestKind::Rename {
                name: "bar".into(),
                documents: capture.clone().into(),
            },
            0,
        );
        let Answer::WorkspaceEdit { edit, versions } = answer(&receiver, 2).unwrap() else {
            panic!("wrong answer")
        };
        assert_eq!(edit.documents.len(), 159);
        assert_eq!(versions.len(), 159);
        let hidden = versions.iter().find(|v| v.path == capture[1].path).unwrap();
        assert_eq!(hidden.version, 1);
        assert_eq!(hidden.revision, texts[1].revision());
        assert_eq!(hidden.document, texts[1].id());
        for (id, position, kind) in [
            (
                3,
                0,
                RequestKind::Rename {
                    name: "error".into(),
                    documents: capture.clone().into(),
                },
            ),
            (
                4,
                1,
                RequestKind::PrepareRename {
                    selection: Selection::new(CharOffset(1), CharOffset(2)),
                    documents: capture.into(),
                },
            ),
        ] {
            submit(id, kind, position);
            assert!(answer(&receiver, id).is_err());
        }
        drop(service);
        let wire: Vec<Value> = fs::read_to_string(root.join("wire.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            wire.iter()
                .filter(|v| v["method"] == "textDocument/didOpen")
                .count(),
            160
        );
        assert_eq!(
            wire.iter()
                .filter(|v| v["method"] == "textDocument/didChange")
                .count(),
            1
        );
        let change = wire
            .iter()
            .position(|v| v["method"] == "textDocument/didChange")
            .unwrap();
        let rename = wire
            .iter()
            .position(|v| v["method"] == "textDocument/rename")
            .unwrap();
        assert!(change < rename);
        assert!(
            wire[..rename]
                .iter()
                .any(|v| v["method"] == "textDocument/didClose"
                    && v["params"]["textDocument"]["uri"] == file_uri(&closed).unwrap())
        );
    }

    #[test]
    fn rename_without_prepare_provider_uses_local_placeholder_without_sending_unsupported_method() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (sender, receiver) = mpsc::channel();
        let service = Service::with_program(server(&root, false), move |event| {
            let _ = sender.send(event);
        })
        .unwrap();
        let text = TextDocument::from("foo");
        let path = root.join("main.rs");
        let documents: Arc<[WorkspaceDocument]> = vec![WorkspaceDocument {
            path: path.clone(),
            language: Some(Language::Rust),
            snapshot: text.snapshot(),
        }]
        .into();
        service.update(Update {
            document: Some(Document {
                epoch: 1,
                language: Language::Rust,
                path,
                snapshot: text.snapshot(),
                saved: 0,
                saved_snapshot: None,
            }),
            request: Some(Request {
                id: 1,
                kind: RequestKind::PrepareRename {
                    selection: Selection::new(CharOffset(1), CharOffset(2)),
                    documents,
                },
                position: CharOffset(1),
                cancellation: Cancellation::default(),
            }),
        });
        assert!(
            matches!(answer(&receiver, 1).unwrap(), Answer::RenamePrepared(name) if name == "foo")
        );
        drop(service);
        assert!(
            !fs::read_to_string(root.join("wire.jsonl"))
                .unwrap()
                .contains("textDocument/prepareRename")
        );
    }

    #[test]
    fn shutdown_interrupts_workspace_sync_when_the_server_stops_reading_a_full_output_queue() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let program = server(&root, true);
        let script = fs::read_to_string(&program).unwrap().replace(
            "        documents[doc['uri']] = doc",
            "        if doc['uri'].endswith('01.rs'):\n            open(os.path.join(os.path.dirname(__file__), 'blocked'), 'w').close()\n            import time; time.sleep(30)\n        documents[doc['uri']] = doc",
        );
        fs::write(&program, script).unwrap();
        let service = Service::with_program(program, |_| {}).unwrap();
        let large = TextDocument::from(format!("foo{}", " ".repeat(1 << 20)).as_str());
        let capture: Arc<[WorkspaceDocument]> = (0..20)
            .map(|i| WorkspaceDocument {
                path: root.join(format!("{i:02}.rs")),
                language: Some(Language::Rust),
                snapshot: large.snapshot(),
            })
            .collect();
        service.update(Update {
            document: Some(Document {
                epoch: 1,
                language: Language::Rust,
                path: capture[0].path.clone(),
                snapshot: large.snapshot(),
                saved: 0,
                saved_snapshot: None,
            }),
            request: Some(Request {
                id: 1,
                kind: RequestKind::PrepareRename {
                    selection: Selection::new(CharOffset(0), CharOffset(1)),
                    documents: capture,
                },
                position: CharOffset(0),
                cancellation: Cancellation::default(),
            }),
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !root.join("blocked").exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "server never received workspace documents"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // Give the producer enough time to reach the bounded queue's capacity.
        std::thread::sleep(Duration::from_millis(100));
        let start = std::time::Instant::now();
        drop(service);
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn applied_workspace_snapshots_survive_cursor_request_coalescing_and_sync_before_following_requests()
     {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (sender, receiver) = mpsc::channel();
        let service = Service::with_program(server(&root, true), move |event| {
            let _ = sender.send(event);
        })
        .unwrap();
        let mut active = TextDocument::from("foo");
        let mut hidden = TextDocument::from("foo");
        let mut document = Document {
            epoch: 1,
            language: Language::Rust,
            path: root.join("main.rs"),
            snapshot: active.snapshot(),
            saved: 0,
            saved_snapshot: None,
        };
        let mut captured = vec![
            WorkspaceDocument {
                path: document.path.clone(),
                snapshot: active.snapshot(),
                language: Some(Language::Rust),
            },
            WorkspaceDocument {
                path: root.join("hidden.rs"),
                snapshot: hidden.snapshot(),
                language: Some(Language::Rust),
            },
        ];
        service.update(Update {
            document: Some(document.clone()),
            request: Some(Request {
                id: 1,
                kind: RequestKind::PrepareRename {
                    selection: Selection::new(CharOffset(0), CharOffset(1)),
                    documents: captured.clone().into(),
                },
                position: CharOffset(0),
                cancellation: Cancellation::default(),
            }),
        });
        answer(&receiver, 1).unwrap();
        for doc in [&mut active, &mut hidden] {
            let mut selections = SelectionSet::single(Selection::new(CharOffset(0), CharOffset(3)));
            let edit = doc.replace_selections(&selections, "bar").unwrap();
            doc.apply(edit, &mut selections).unwrap();
        }
        document.snapshot = active.snapshot();
        captured[1].snapshot = hidden.snapshot();
        captured.push(WorkspaceDocument {
            path: root.join("newly_loaded.rs"),
            snapshot: TextDocument::from("bar").snapshot(),
            language: Some(Language::Rust),
        });
        // Deliberately leave the active entry old: its ordinary update is the
        // authority, even if typing advances after a workspace capture.
        service.update_workspace(WorkspaceUpdate {
            epoch: 1,
            generation: 1,
            documents: captured.into(),
        });
        let cancelled = Cancellation::default();
        cancelled.cancel();
        service.update(Update {
            document: Some(document.clone()),
            request: Some(Request {
                id: 2,
                kind: RequestKind::Hover,
                position: CharOffset(1),
                cancellation: cancelled,
            }),
        });
        service.update(Update {
            document: Some(document),
            request: Some(Request {
                id: 3,
                kind: RequestKind::Hover,
                position: CharOffset(1),
                cancellation: Cancellation::default(),
            }),
        });
        assert!(
            matches!(answer(&receiver,3).unwrap(),Answer::Hover(text) if text == "bar|bar|bar")
        );
    }
}
