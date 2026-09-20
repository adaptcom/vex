//! Owned stdio transport. Blocking pipe reads/writes never run in Future::poll.
//! Replies resolve request futures; notifications feed the service inbox.

use crate::{
    executor::Executor,
    protocol::{read_message, write_message},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    future::Future,
    io::{self, BufReader, Read},
    path::Path,
    pin::Pin,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{self, SyncSender},
    },
    task::{Context, Poll, Waker},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

type Reply = Result<Value, String>;
#[derive(Default)]
struct Slot {
    result: Option<Reply>,
    waker: Option<Waker>,
}

#[derive(Default)]
struct Pending {
    replies: HashMap<u64, Arc<Mutex<Slot>>>,
    failure: Option<String>,
}

impl Pending {
    fn fail(&mut self, message: String) {
        self.failure = Some(message.clone());
        for (_, slot) in self.replies.drain() {
            let mut slot = slot.lock().unwrap();
            slot.result = Some(Err(message.clone()));
            if let Some(waker) = slot.waker.take() {
                waker.wake();
            }
        }
    }
}

pub(crate) struct Response {
    id: u64,
    slot: Arc<Mutex<Slot>>,
    pending: Arc<Mutex<Pending>>,
    writer: SyncSender<Value>,
    executor: Executor,
    deadline: Instant,
    complete: bool,
}

impl Future for Response {
    type Output = Reply;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Reply> {
        let this = self.get_mut();
        let mut slot = this.slot.lock().unwrap();
        if let Some(result) = slot.result.take() {
            this.complete = true;
            return Poll::Ready(result);
        }
        if Instant::now() >= this.deadline {
            return Poll::Ready(Err("language server request timed out".into()));
        }
        slot.waker = Some(cx.waker().clone());
        this.executor.deadline(this.deadline);
        Poll::Pending
    }
}

impl Drop for Response {
    fn drop(&mut self) {
        self.pending.lock().unwrap().replies.remove(&self.id);
        if !self.complete {
            let _ = self.writer.try_send(
                json!({"jsonrpc":"2.0", "method":"$/cancelRequest", "params":{"id":self.id}}),
            );
        }
    }
}

pub(crate) struct Transport {
    child: Child,
    writer: Option<SyncSender<Value>>,
    pending: Arc<Mutex<Pending>>,
    reader_thread: Option<JoinHandle<()>>,
    writer_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    next_id: u64,
}

impl Transport {
    pub fn start(
        program: &Path,
        arguments: &[&str],
        root: &Path,
        notify: impl Fn(Value) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let mut command = Command::new(program);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command
            .args(arguments)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut transport = Self {
            child,
            writer: None,
            pending: Arc::default(),
            reader_thread: None,
            writer_thread: None,
            stderr_thread: None,
            stderr: Arc::default(),
            next_id: 0,
        };
        let mut stdin = transport.child.stdin.take().unwrap();
        let stdout = transport.child.stdout.take().unwrap();
        let mut stderr = transport.child.stderr.take().unwrap();
        let notify = Arc::new(notify);
        let (writer, outgoing) = mpsc::sync_channel::<Value>(8);
        transport.writer = Some(writer.clone());
        let pending = transport.pending.clone();
        let failed = notify.clone();
        transport.writer_thread = Some(thread::Builder::new().name("vex-lsp-write".into()).spawn(
            move || {
                while let Ok(value) = outgoing.recv() {
                    if let Err(error) = write_message(&mut stdin, &value) {
                        pending.lock().unwrap().fail(error.to_string());
                        failed(json!({"transportError":error.to_string()}));
                        break;
                    }
                }
            },
        )?);
        let pending = transport.pending.clone();
        transport.reader_thread = Some(thread::Builder::new().name("vex-lsp-read".into()).spawn(move || {
            let result = (|| -> io::Result<()> {
                let mut reader = BufReader::new(stdout);
                while let Some(value) = read_message(&mut reader)? {
                    if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") { return Err(io::Error::other("invalid JSON-RPC version")) }
                    if let Some(method) = value["method"].as_str() {
                        if let Some(id) = value.get("id") {
                            let response = match method {
                                "workspace/configuration" => json!({"result": value["params"]["items"].as_array().map(|items| vec![Value::Null; items.len()]).unwrap_or_default()}),
                                "window/workDoneProgress/create" => json!({"result":null}),
                                "workspace/applyEdit" => json!({"result":{"applied":false,"failureReason":"workspace edits are not supported"}}),
                                _ => json!({"error":{"code":-32601,"message":"unsupported client method"}}),
                            };
                            let mut response = response;
                            response["jsonrpc"] = json!("2.0"); response["id"] = id.clone();
                            writer.try_send(response).map_err(io::Error::other)?;
                        } else if matches!(method, "textDocument/publishDiagnostics" | "window/showMessage") {
                            notify(value);
                        }
                    } else if let Some(id) = value["id"].as_u64()
                        && let Some(slot) = pending.lock().unwrap().replies.remove(&id) {
                            let mut slot = slot.lock().unwrap();
                            slot.result = Some(if value.get("error").is_some() {
                                Err(value["error"]["message"].as_str().unwrap_or("language server request failed").into())
                            } else { Ok(value.get("result").cloned().unwrap_or(Value::Null)) });
                            if let Some(waker) = slot.waker.take() { waker.wake(); }
                    }
                }
                Err(io::Error::other("language server disconnected"))
            })();
            if let Err(error) = result {
                pending.lock().unwrap().fail(error.to_string());
                notify(json!({"transportError":error.to_string()}));
            }
        })?);
        let tail = transport.stderr.clone();
        transport.stderr_thread = Some(
            thread::Builder::new()
                .name("vex-lsp-stderr".into())
                .spawn(move || {
                    let mut bytes = [0; 4096];
                    while let Ok(count) = stderr.read(&mut bytes) {
                        if count == 0 {
                            break;
                        }
                        let mut tail = tail.lock().unwrap();
                        tail.extend_from_slice(&bytes[..count]);
                        let excess = tail.len().saturating_sub(8192);
                        tail.drain(..excess);
                    }
                })?,
        );
        Ok(transport)
    }

    fn send(&self, value: Value) -> Result<(), String> {
        if let Some(error) = &self.pending.lock().unwrap().failure {
            return Err(error.clone());
        }
        self.writer
            .as_ref()
            .unwrap()
            .try_send(value)
            .map_err(|_| "language server output queue is full or closed".into())
    }

    pub fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let mut message = json!({"jsonrpc":"2.0", "method":method});
        if !params.is_null() {
            message["params"] = params;
        }
        self.send(message)
    }

    pub fn request(
        &mut self,
        executor: &Executor,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Response, String> {
        self.next_id += 1;
        let slot = Arc::new(Mutex::new(Slot::default()));
        self.pending
            .lock()
            .unwrap()
            .replies
            .insert(self.next_id, slot.clone());
        let response = Response {
            id: self.next_id,
            slot,
            pending: self.pending.clone(),
            writer: self.writer.as_ref().unwrap().clone(),
            executor: executor.clone(),
            deadline: Instant::now() + timeout,
            complete: false,
        };
        let mut message = json!({"jsonrpc":"2.0", "id":self.next_id, "method":method});
        if !params.is_null() {
            message["params"] = params;
        }
        self.send(message)?;
        Ok(response)
    }

    pub async fn shutdown(&mut self, executor: &Executor, uri: Option<&str>) {
        if let Some(uri) = uri {
            let _ = self.notify("textDocument/didClose", json!({"textDocument":{"uri":uri}}));
        }
        if let Ok(response) = self.request(
            executor,
            "shutdown",
            Value::Null,
            Duration::from_millis(300),
        ) {
            let _ = response.await;
        }
        let _ = self.notify("exit", Value::Null);
        // The grace period runs on the service thread, after its last future.
        let deadline = Instant::now() + Duration::from_millis(200);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn error_context(&self, message: &str) -> String {
        let tail = self.stderr.lock().unwrap();
        if tail.is_empty() {
            message.into()
        } else {
            format!("{message}: {}", String::from_utf8_lossy(&tail).trim())
        }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // SAFETY: the child was spawned in its own process group. Stop that
            // owned group so descendants cannot retain pipes and block joins.
            unsafe {
                libc::killpg(self.child.id() as libc::pid_t, libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.pending
            .lock()
            .unwrap()
            .fail("language server stopped".into());
        if let Some(thread) = self.reader_thread.take() {
            let _ = thread.join();
        }
        self.writer.take();
        if let Some(thread) = self.writer_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
    }
}
