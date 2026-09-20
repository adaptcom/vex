//! One-shot acknowledgement of a server-requested workspace edit.
//! Preparation can time out; claiming UI application excludes a timeout race
//! between the final validation and installing the already prepared text.

use crate::{Document, WorkspaceUpdate, executor::Executor};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};
use vex_editor::background::Cancellation;

/// Exact post-application snapshots, synchronized before replying `applied:true`.
#[derive(Debug)]
pub struct Applied {
    pub document: Document,
    pub workspace: WorkspaceUpdate,
}

#[derive(Default)]
struct State {
    claimed: bool,
    replied: bool,
    result: Option<Result<Applied, String>>,
    waker: Option<Waker>,
}

struct Shared {
    state: Mutex<State>,
    cancellation: Cancellation,
}

/// Frontend ownership of a request. Dropping it rejects the edit and wakes the
/// service; it never writes pipes or waits for the server on the UI thread.
pub struct ApplyReply(Arc<Shared>);

impl std::fmt::Debug for ApplyReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApplyReply")
            .field("cancelled", &self.0.cancellation.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl ApplyReply {
    pub fn cancellation(&self) -> Cancellation {
        self.0.cancellation.clone()
    }

    /// Call immediately before the synchronous application/preflight step.
    /// Once claimed, the service waits for `finish`; it cannot tell the server
    /// an edit failed while the UI is already installing it.
    pub fn claim(&mut self) -> bool {
        let mut state = self.0.state.lock().unwrap();
        if self.0.cancellation.is_cancelled() || state.replied || state.claimed {
            return false;
        }
        state.claimed = true;
        true
    }

    pub fn finish(self, result: Result<Applied, String>) {
        let mut state = self.0.state.lock().unwrap();
        if !state.replied && (!self.0.cancellation.is_cancelled() || state.claimed) {
            state.replied = true;
            state.result = Some(if result.is_ok() && !state.claimed {
                Err("workspace application was not claimed".into())
            } else {
                result
            });
            if let Some(waker) = state.waker.take() {
                waker.wake();
            }
        }
    }
}

impl Drop for ApplyReply {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        if !state.replied {
            state.replied = true;
            state.result = Some(Err("workspace edit was not applied".into()));
            if let Some(waker) = state.waker.take() {
                waker.wake();
            }
        }
    }
}

pub(crate) struct AwaitApply {
    shared: Arc<Shared>,
    executor: Executor,
    deadline: Instant,
    complete: bool,
}

pub(crate) fn channel(executor: &Executor) -> (ApplyReply, AwaitApply) {
    let shared = Arc::new(Shared {
        state: Mutex::default(),
        cancellation: Cancellation::default(),
    });
    (
        ApplyReply(shared.clone()),
        AwaitApply {
            shared,
            executor: executor.clone(),
            deadline: Instant::now() + Duration::from_secs(10),
            complete: false,
        },
    )
}

impl AwaitApply {
    pub fn cancellation(&self) -> Cancellation {
        self.shared.cancellation.clone()
    }
}

impl Future for AwaitApply {
    type Output = Result<Applied, String>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut state = this.shared.state.lock().unwrap();
        if let Some(result) = state.result.take() {
            this.complete = true;
            return Poll::Ready(result);
        }
        if !state.claimed {
            if this.shared.cancellation.is_cancelled() || Instant::now() >= this.deadline {
                this.shared.cancellation.cancel();
                state.replied = true;
                this.complete = true;
                return Poll::Ready(Err("workspace edit cancelled or timed out".into()));
            }
            this.executor.deadline(this.deadline);
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for AwaitApply {
    fn drop(&mut self) {
        if !self.complete {
            let _state = self.shared.state.lock().unwrap();
            self.shared.cancellation.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::poll_fn;

    #[test]
    fn dropping_or_expiring_a_reply_rejects_and_a_claim_prevents_timeout_during_application() {
        let executor = Executor::default();
        let (reply, future) = channel(&executor);
        drop(reply);
        assert!(executor.run(future).unwrap_err().contains("not applied"));
        let (mut reply, mut future) = channel(&executor);
        future.deadline = Instant::now();
        assert!(executor.run(future).is_err());
        assert!(!reply.claim());
        let (mut reply, mut future) = channel(&executor);
        assert!(reply.claim());
        future.deadline = Instant::now();
        let mut reply = Some(reply);
        let mut polls = 0;
        assert!(
            executor
                .run(poll_fn(|cx| {
                    polls += 1;
                    let result = Pin::new(&mut future).poll(cx);
                    if polls == 1 {
                        assert!(result.is_pending());
                        reply
                            .take()
                            .unwrap()
                            .finish(Err("preflight rejected stale destination".into()));
                    }
                    result
                }))
                .unwrap_err()
                .contains("preflight")
        );
        assert_eq!(polls, 2);
        let (mut reply, future) = channel(&executor);
        drop(future);
        assert!(!reply.claim());
    }
}
