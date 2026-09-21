//! Owned stdio transport. Blocking pipe reads/writes never run in Future::poll.
//! Protocol packets enter the service inbox in wire order. The service resolves
//! request futures after processing preceding server requests and notifications.

use crate::{
    executor::Executor,
    protocol::{read_message, write_message},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    future::{Future, poll_fn},
    io::{self, BufReader, Read},
    path::Path,
    pin::Pin,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::TrySendError,
    },
    task::{Context, Poll, Waker},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

type Reply = Result<Value, String>;

#[derive(Default)]
struct Outgoing {
    messages: VecDeque<Value>,
    replies: VecDeque<Value>,
    closed: bool,
}

#[derive(Default)]
struct OutboxInner {
    queue: Mutex<Outgoing>,
    ready: Condvar,
    writable: Mutex<Option<Waker>>,
}

/// Server-request replies have separate bounded capacity and take precedence
/// over queued client messages. A batch of didOpen messages cannot prevent a
/// configuration reply or block the reader that resolves request futures.
#[derive(Clone, Default)]
struct Outbox(Arc<OutboxInner>);

impl Outbox {
    fn try_send(&self, value: Value) -> Result<(), TrySendError<Value>> {
        self.push(value, false)
    }
    fn reply(&self, value: Value) -> Result<(), TrySendError<Value>> {
        self.push(value, true)
    }
    fn push(&self, value: Value, reply: bool) -> Result<(), TrySendError<Value>> {
        let mut queue = self.0.queue.lock().unwrap();
        if queue.closed {
            return Err(TrySendError::Disconnected(value));
        }
        let (messages, limit) = if reply {
            (&mut queue.replies, 32)
        } else {
            (&mut queue.messages, 8)
        };
        if messages.len() == limit {
            return Err(TrySendError::Full(value));
        }
        messages.push_back(value);
        self.0.ready.notify_one();
        Ok(())
    }
    fn recv(&self) -> Option<Value> {
        let mut queue = self.0.queue.lock().unwrap();
        loop {
            if queue.closed {
                return None;
            }
            if let Some(value) = queue.replies.pop_front() {
                return Some(value);
            }
            if let Some(value) = queue.messages.pop_front() {
                if let Some(waker) = self.0.writable.lock().unwrap().take() {
                    waker.wake()
                }
                return Some(value);
            }
            queue = self.0.ready.wait(queue).unwrap();
        }
    }
    fn close(&self) {
        self.0.queue.lock().unwrap().closed = true;
        self.0.ready.notify_one();
        if let Some(waker) = self.0.writable.lock().unwrap().take() {
            waker.wake()
        }
    }
}
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
    writer: Outbox,
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

impl Response {
    pub fn resume_timeout(&mut self) {
        self.deadline = Instant::now() + Duration::from_secs(10);
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
    writer: Option<Outbox>,
    pending: Arc<Mutex<Pending>>,
    reader_thread: Option<JoinHandle<()>>,
    writer_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    next_id: u64,
    accept_edits: Arc<AtomicBool>,
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
            accept_edits: Arc::default(),
        };
        let mut stdin = transport.child.stdin.take().unwrap();
        let stdout = transport.child.stdout.take().unwrap();
        let mut stderr = transport.child.stderr.take().unwrap();
        let notify = Arc::new(notify);
        let writer = Outbox::default();
        let outgoing = writer.clone();
        transport.writer = Some(writer.clone());
        let pending = transport.pending.clone();
        let failed = notify.clone();
        transport.writer_thread = Some(thread::Builder::new().name("vex-lsp-write".into()).spawn(
            move || {
                while let Some(value) = outgoing.recv() {
                    if let Err(error) = write_message(&mut stdin, &value) {
                        pending.lock().unwrap().fail(error.to_string());
                        failed(json!({"transportError":error.to_string()}));
                        outgoing.close();
                        break;
                    }
                }
            },
        )?);
        let pending = transport.pending.clone();
        let accept_edits = transport.accept_edits.clone();
        transport.reader_thread = Some(thread::Builder::new().name("vex-lsp-read".into()).spawn(move || {
            let result = (|| -> io::Result<()> {
                let mut reader = BufReader::new(stdout);
                while let Some(value) = read_message(&mut reader)? {
                    if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") { return Err(io::Error::other("invalid JSON-RPC version")) }
                    if let Some(method) = value["method"].as_str() {
                        if let Some(id) = value.get("id") {
                            if method == "workspace/applyEdit" && accept_edits.load(Ordering::Acquire) {
                                notify(value);
                                continue;
                            }
                            let response = match method {
                                "workspace/configuration" => json!({"result": value["params"]["items"].as_array().map(|items| vec![Value::Null; items.len()]).unwrap_or_default()}),
                                "window/workDoneProgress/create" => json!({"result":null}),
                                "workspace/applyEdit" => json!({"result":{"applied":false,"failureReason":"workspace edits require an active server command"}}),
                                _ => json!({"error":{"code":-32601,"message":"unsupported client method"}}),
                            };
                            let mut response = response;
                            response["jsonrpc"] = json!("2.0"); response["id"] = id.clone();
                            writer.reply(response).map_err(io::Error::other)?;
                        } else if matches!(method, "textDocument/publishDiagnostics" | "window/showMessage") {
                            notify(value);
                        }
                    } else {
                        notify(value);
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

    /// Resolve only when the service reaches this response in its ordered inbox.
    /// Resolving on the reader thread would let an executeCommand response pass
    /// an earlier workspace/applyEdit request that the service has not seen yet.
    pub fn receive_response(&self, value: &Value) -> bool {
        if value.get("method").is_some() {
            return false;
        }
        if let Some(id) = value["id"].as_u64()
            && let Some(slot) = self.pending.lock().unwrap().replies.remove(&id)
        {
            let mut slot = slot.lock().unwrap();
            slot.result = Some(if value.get("error").is_some() {
                Err(value["error"]["message"]
                    .as_str()
                    .unwrap_or("language server request failed")
                    .into())
            } else {
                Ok(value.get("result").cloned().unwrap_or(Value::Null))
            });
            if let Some(waker) = slot.waker.take() {
                waker.wake();
            }
        }
        true
    }

    pub fn enable_edits(&self, enabled: bool) {
        self.accept_edits.store(enabled, Ordering::Release);
    }

    pub async fn reply_edit(
        &self,
        executor: &Executor,
        inbox: &crate::Inbox,
        id: Value,
        result: &Result<(), String>,
    ) -> Result<(), String> {
        let result = match result {
            Ok(()) => json!({"applied":true}),
            Err(error) => json!({"applied":false,"failureReason":error}),
        };
        // This reply must follow any didChange messages for the applied text.
        self.send_wait(
            executor,
            json!({"jsonrpc":"2.0","id":id,"result":result}),
            |cx| inbox.stopped(cx),
        )
        .await
    }

    /// Initialization and shutdown also use the ordered reader path. Neither
    /// phase accepts application requests; the reader rejects those directly.
    pub fn poll_lifecycle_response(
        &self,
        inbox: &crate::Inbox,
        response: &mut Response,
        cx: &mut Context<'_>,
    ) -> Poll<Reply> {
        inbox.stopped(cx); // Register even during the short shutdown grace period.
        for index in 0..128 {
            let Some(value) = inbox.take_wire() else {
                break;
            };
            if !self.receive_response(&value) && value["method"] == "workspace/applyEdit" {
                let _ = self.writer.as_ref().unwrap().reply(json!({"jsonrpc":"2.0","id":value["id"],"result":{"applied":false,"failureReason":"language server session is closing"}}));
            }
            if index == 127 {
                cx.waker().wake_by_ref();
            }
        }
        Pin::new(response).poll(cx)
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

    /// Wait for bounded queue capacity without blocking the service executor.
    /// Register before trying the queue so a concurrent dequeue cannot lose a wake.
    async fn send_wait(
        &self,
        executor: &Executor,
        message: Value,
        interrupted: impl Fn(&Context<'_>) -> bool,
    ) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut message = Some(message);
        poll_fn(|cx| {
            if interrupted(cx) {
                return Poll::Ready(Err("language request superseded or cancelled".into()));
            }
            if let Some(error) = &self.pending.lock().unwrap().failure {
                return Poll::Ready(Err(error.clone()));
            }
            if Instant::now() >= deadline {
                return Poll::Ready(Err("language server output queue timed out".into()));
            }
            executor.deadline(deadline);
            enqueue(self.writer.as_ref().unwrap(), &mut message, cx)
        })
        .await
    }

    pub async fn notify_wait(
        &self,
        executor: &Executor,
        method: &str,
        params: Value,
        interrupted: impl Fn(&Context<'_>) -> bool,
    ) -> Result<(), String> {
        self.send_wait(
            executor,
            json!({"jsonrpc":"2.0", "method":method,"params":params}),
            interrupted,
        )
        .await
    }

    pub async fn request_wait(
        &mut self,
        executor: &Executor,
        method: &str,
        params: Value,
        interrupted: impl Fn(&Context<'_>) -> bool,
    ) -> Result<Response, String> {
        let (mut response, message) =
            self.prepare_request(executor, method, params, Duration::from_secs(10));
        if let Err(error) = self.send_wait(executor, message, interrupted).await {
            response.complete = true; // The request was never enqueued.
            return Err(error);
        }
        response.deadline = Instant::now() + Duration::from_secs(10);
        Ok(response)
    }

    pub fn request(
        &mut self,
        executor: &Executor,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Response, String> {
        let (mut response, message) = self.prepare_request(executor, method, params, timeout);
        if let Err(error) = self.send(message) {
            response.complete = true;
            return Err(error);
        }
        Ok(response)
    }

    fn prepare_request(
        &mut self,
        executor: &Executor,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> (Response, Value) {
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
        (response, message)
    }

    pub async fn shutdown(&mut self, executor: &Executor, inbox: &crate::Inbox, uri: Option<&str>) {
        self.accept_edits.store(false, Ordering::Release);
        if let Some(uri) = uri {
            let _ = self.notify("textDocument/didClose", json!({"textDocument":{"uri":uri}}));
        }
        if let Ok(mut response) = self.request(
            executor,
            "shutdown",
            Value::Null,
            Duration::from_millis(300),
        ) {
            let _ = poll_fn(|cx| self.poll_lifecycle_response(inbox, &mut response, cx)).await;
        }
        let _ = self.notify("exit", Value::Null);
        // Other workspace sessions continue while this child exits.
        let deadline = Instant::now() + Duration::from_millis(200);
        poll_fn(|_| {
            let now = Instant::now();
            if now >= deadline || self.child.try_wait().ok().flatten().is_some() {
                Poll::Ready(())
            } else {
                executor.deadline(deadline.min(now + Duration::from_millis(5)));
                Poll::Pending
            }
        })
        .await;
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
        if let Some(writer) = self.writer.take() {
            writer.close();
        }
        if let Some(thread) = self.writer_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
    }
}

fn enqueue(
    writer: &Outbox,
    message: &mut Option<Value>,
    cx: &Context<'_>,
) -> Poll<Result<(), String>> {
    *writer.0.writable.lock().unwrap() = Some(cx.waker().clone());
    match writer.try_send(message.take().expect("polled completed send")) {
        Ok(()) => Poll::Ready(Ok(())),
        Err(TrySendError::Full(value)) => {
            *message = Some(value);
            Poll::Pending
        }
        Err(TrySendError::Disconnected(_)) => {
            Poll::Ready(Err("language server output queue closed".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_queue_yields_and_wakes_without_losing_or_reordering_messages() {
        let writer = Outbox::default();
        for i in 0..8 {
            writer.try_send(json!(i)).unwrap();
        }
        // Replies must remain available even when client messages fill the queue.
        writer.reply(json!("configuration reply")).unwrap();
        assert_eq!(writer.recv().unwrap(), json!("configuration reply"));
        let executor = Executor::default();
        let mut message = Some(json!(8));
        let mut polls = 0;
        executor
            .run(poll_fn(|cx| {
                polls += 1;
                let result = enqueue(&writer, &mut message, cx);
                if polls == 1 {
                    assert!(result.is_pending());
                    // Dequeue and wake during poll to exercise the missed-wake race.
                    assert_eq!(writer.recv().unwrap(), json!(0));
                }
                result
            }))
            .unwrap();
        assert_eq!(polls, 2);
        for i in 1..9 {
            assert_eq!(writer.recv().unwrap(), json!(i));
        }
        for _ in 0..32 {
            writer.reply(json!(null)).unwrap();
        }
        assert!(matches!(
            writer.reply(json!(null)),
            Err(TrySendError::Full(_))
        ));
        writer.close();
        let mut message = Some(json!(9));
        assert!(
            executor
                .run(poll_fn(|cx| enqueue(&writer, &mut message, cx)))
                .is_err()
        );
    }
}
