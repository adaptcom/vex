//! A single-root-future executor with wakeups and deadlines. I/O threads wake
//! response and inbox futures; the root polls those concurrently. No I/O reactor,
//! task pool, unsafe wakers, busy polling, or runtime-specific futures are needed.

use std::{
    future::Future,
    pin::pin,
    sync::{Arc, Condvar, Mutex},
    task::{Context, Poll, Wake, Waker},
    time::Instant,
};

#[derive(Default)]
struct State {
    notified: bool,
    deadline: Option<Instant>,
}

#[derive(Default)]
struct Parker {
    state: Mutex<State>,
    ready: Condvar,
}

impl Wake for Parker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.state.lock().unwrap().notified = true;
        self.ready.notify_one();
    }
}

#[derive(Clone, Default)]
pub(crate) struct Executor(Arc<Parker>);

impl Executor {
    /// Register a deadline during poll. Registration is rebuilt each poll, so
    /// dropping a timeout future cannot leave an ever-growing timer collection.
    pub fn deadline(&self, deadline: Instant) {
        let mut state = self.0.state.lock().unwrap();
        state.deadline = Some(state.deadline.map_or(deadline, |old| old.min(deadline)));
    }

    pub fn run<F: Future>(&self, future: F) -> F::Output {
        let waker = Waker::from(self.0.clone());
        let mut context = Context::from_waker(&waker);
        let mut future = pin!(future);
        loop {
            {
                let mut state = self.0.state.lock().unwrap();
                state.notified = false;
                state.deadline = None;
            }
            if let Poll::Ready(result) = future.as_mut().poll(&mut context) {
                return result;
            }
            let mut state = self.0.state.lock().unwrap();
            while !state.notified {
                if let Some(deadline) = state.deadline {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    state = self.0.ready.wait_timeout(state, remaining).unwrap().0;
                } else {
                    state = self.0.ready.wait(state).unwrap();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::poll_fn,
        sync::atomic::{AtomicBool, Ordering},
        thread,
        time::Duration,
    };

    #[test]
    fn wake_during_poll_is_not_lost_and_finished_futures_are_not_polled_again() {
        let executor = Executor::default();
        let mut polls = 0;
        let result = executor.run(poll_fn(|cx| {
            polls += 1;
            match polls {
                1 => {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
                2 => Poll::Ready(42),
                _ => panic!("polled a completed future"),
            }
        }));
        assert_eq!(result, 42);
    }

    #[test]
    fn deadlines_and_external_wakes_drive_progress() {
        let executor = Executor::default();
        let deadline = Instant::now() + Duration::from_millis(10);
        executor.run(poll_fn(|_| {
            if Instant::now() >= deadline {
                Poll::Ready(())
            } else {
                executor.deadline(deadline);
                Poll::Pending
            }
        }));
        let ready = Arc::new(AtomicBool::new(false));
        let mut thread = None;
        executor.run(poll_fn(|cx| {
            if ready.load(Ordering::Acquire) {
                return Poll::Ready(());
            }
            if thread.is_none() {
                let ready = ready.clone();
                let waker = cx.waker().clone();
                thread = Some(thread::spawn(move || {
                    ready.store(true, Ordering::Release);
                    waker.wake();
                }));
            }
            Poll::Pending
        }));
        thread.unwrap().join().unwrap();
    }
}
