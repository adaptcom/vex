//! Wakeable event inbox and owned producer threads. Terminal input is bounded;
//! independent services have typed completions and explicit delivery policies.

use crossterm::event::{self, Event};
use std::{
    collections::VecDeque,
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use vex_editor::{
    Key, SearchJob, SearchResult, SyntaxJob, SyntaxResult, SyntaxWorker, background::Cancellation,
};

const INPUT_CAPACITY: usize = 256;

pub(crate) enum AppEvent {
    Terminal(Event),
    Background(BackgroundEvent),
    Failed(io::Error),
}

/// These snapshot services keep only their latest completion. Future services
/// with ordered protocol messages must get a FIFO policy, not share these slots.
pub(crate) enum BackgroundEvent {
    Search(SearchResult),
    Syntax(SyntaxResult),
}

#[derive(Default)]
struct Inbox {
    input: VecDeque<Event>,
    background: [Option<BackgroundEvent>; 2],
    next_background: usize,
    failure: Option<io::Error>,
    closed: bool,
}

#[derive(Default)]
struct SharedInbox {
    state: Mutex<Inbox>,
    ready: Condvar,
    space: Condvar,
}

#[derive(Clone, Default)]
pub(crate) struct EventQueue(Arc<SharedInbox>);

impl EventQueue {
    fn terminal(&self, event: Event) -> bool {
        let mut state = self.0.state.lock().unwrap();
        while state.input.len() == INPUT_CAPACITY && !state.closed {
            state = self.0.space.wait(state).unwrap();
        }
        if state.closed {
            return false;
        }
        state.input.push_back(event);
        self.0.ready.notify_one();
        true
    }

    fn background(&self, result: BackgroundEvent) {
        let mut state = self.0.state.lock().unwrap();
        if !state.closed {
            let slot = match &result {
                BackgroundEvent::Search(_) => 0,
                BackgroundEvent::Syntax(_) => 1,
            };
            state.background[slot] = Some(result);
            self.0.ready.notify_one();
        }
    }

    fn fail(&self, error: io::Error) {
        let mut state = self.0.state.lock().unwrap();
        state.failure = Some(error);
        self.0.ready.notify_one();
    }

    fn close(&self) {
        let mut state = self.0.state.lock().unwrap();
        state.closed = true;
        state.input.clear();
        state.background = [None, None];
        self.0.ready.notify_all();
        self.0.space.notify_all();
    }

    fn closed(&self) -> bool {
        self.0.state.lock().unwrap().closed
    }

    /// Wait for input or worker completion. While a search destination is needed
    /// by subsequent keys, hold those keys in FIFO order. Resize/focus events
    /// can pass them; Escape/Ctrl-c cancel when next in key order. An Escape
    /// after a queued change command must not discard that edit. The completion
    /// slots cannot be blocked by input, and alternate when both are ready.
    pub(crate) fn next(&self, timeout: Duration, waiting: bool) -> Option<AppEvent> {
        let deadline = Instant::now() + timeout;
        let mut state = self.0.state.lock().unwrap();
        loop {
            if let Some(error) = state.failure.take() {
                return Some(AppEvent::Failed(error));
            }
            if state.closed {
                return None;
            }
            if waiting && state.input.front().is_some_and(cancels_search) {
                let event = state.input.pop_front().unwrap();
                self.0.space.notify_one();
                return Some(AppEvent::Terminal(event));
            }
            for offset in 0..state.background.len() {
                let index = (state.next_background + offset) % state.background.len();
                if let Some(result) = state.background[index].take() {
                    state.next_background = (index + 1) % state.background.len();
                    return Some(AppEvent::Background(result));
                }
            }
            if waiting
                && let Some(index) = state.input.iter().position(|event| {
                    matches!(
                        event,
                        Event::Resize(..) | Event::FocusGained | Event::FocusLost
                    )
                })
            {
                let event = state.input.remove(index).unwrap();
                self.0.space.notify_one();
                return Some(AppEvent::Terminal(event));
            }
            if !waiting && let Some(event) = state.input.pop_front() {
                self.0.space.notify_one();
                return Some(AppEvent::Terminal(event));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            state = self.0.ready.wait_timeout(state, remaining).unwrap().0;
        }
    }
}

fn cancels_search(event: &Event) -> bool {
    matches!(event, Event::Key(key) if matches!(crate::input::key(*key), Some(Key::Escape | Key::Ctrl('c'))))
}

trait Job: Send + 'static {
    fn cancellation(&self) -> Cancellation;
}

impl Job for SearchJob {
    fn cancellation(&self) -> Cancellation {
        self.cancellation()
    }
}

impl Job for SyntaxJob {
    fn cancellation(&self) -> Cancellation {
        self.cancellation()
    }
}

struct Work<J> {
    pending: Option<J>,
    running: Option<Cancellation>,
    closed: bool,
}

struct Mailbox<J> {
    state: Mutex<Work<J>>,
    ready: Condvar,
}

struct LatestWorker<J: Job> {
    mailbox: Arc<Mailbox<J>>,
    thread: Option<JoinHandle<()>>,
}

type SearchWorker = LatestWorker<SearchJob>;

impl LatestWorker<SearchJob> {
    fn start(events: EventQueue) -> io::Result<Self> {
        Self::with_runner(events, SearchJob::run)
    }

    fn with_runner(
        events: EventQueue,
        run: impl Fn(SearchJob) -> Option<SearchResult> + Send + 'static,
    ) -> io::Result<Self> {
        Self::spawn("vex-search", events, move |job| {
            run(job).map(BackgroundEvent::Search)
        })
    }
}

impl<J: Job> LatestWorker<J> {
    fn spawn(
        name: &'static str,
        events: EventQueue,
        mut run: impl FnMut(J) -> Option<BackgroundEvent> + Send + 'static,
    ) -> io::Result<Self> {
        let mailbox = Arc::new(Mailbox {
            state: Mutex::new(Work::<J> {
                pending: None,
                running: None,
                closed: false,
            }),
            ready: Condvar::new(),
        });
        let shared = Arc::clone(&mailbox);
        let thread = thread::Builder::new().name(name.into()).spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                loop {
                    let job = {
                        let mut state = shared.state.lock().unwrap();
                        while state.pending.is_none() && !state.closed {
                            state = shared.ready.wait(state).unwrap();
                        }
                        if state.closed {
                            break;
                        }
                        let job = state.pending.take().unwrap();
                        state.running = Some(job.cancellation());
                        job
                    };
                    if let Some(result) = run(job) {
                        events.background(result);
                    }
                    shared.state.lock().unwrap().running = None;
                }
            }));
            if result.is_err() {
                events.fail(io::Error::other(format!("{name} worker panicked")));
            }
        })?;
        Ok(Self {
            mailbox,
            thread: Some(thread),
        })
    }

    fn submit(&self, job: J) {
        let mut state = self.mailbox.state.lock().unwrap();
        if let Some(running) = &state.running {
            running.cancel();
        }
        if let Some(previous) = state.pending.replace(job) {
            previous.cancellation().cancel();
        }
        self.mailbox.ready.notify_one();
    }

    fn stop(&self) {
        let mut state = self.mailbox.state.lock().unwrap();
        state.closed = true;
        if let Some(running) = &state.running {
            running.cancel();
        }
        if let Some(job) = state.pending.take() {
            job.cancellation().cancel();
        }
        self.mailbox.ready.notify_all();
    }
}

impl<J: Job> Drop for LatestWorker<J> {
    fn drop(&mut self) {
        self.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The runtime owns every producer and joins them before terminal restoration.
pub(crate) struct Runtime {
    pub events: EventQueue,
    search: Option<SearchWorker>,
    syntax: Option<LatestWorker<SyntaxJob>>,
    input: Option<JoinHandle<()>>,
}

impl Runtime {
    pub(crate) fn start() -> io::Result<Self> {
        let events = EventQueue::default();
        let search = SearchWorker::start(events.clone())?;
        let mut syntax_state = SyntaxWorker::default();
        let syntax = LatestWorker::spawn("vex-syntax", events.clone(), move |job| {
            syntax_state.run(job).map(BackgroundEvent::Syntax)
        })?;
        let queue = events.clone();
        let input = thread::Builder::new()
            .name("vex-input".into())
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| -> io::Result<()> {
                    // Crossterm poll/read are owned exclusively by this thread.
                    while !queue.closed() {
                        if event::poll(Duration::from_millis(50))?
                            && !queue.terminal(event::read()?)
                        {
                            break;
                        }
                    }
                    Ok(())
                }));
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => queue.fail(error),
                    Err(_) => queue.fail(io::Error::other("terminal input worker panicked")),
                }
            })?;
        Ok(Self {
            events,
            search: Some(search),
            syntax: Some(syntax),
            input: Some(input),
        })
    }

    pub(crate) fn submit(&self, job: SearchJob) {
        self.search.as_ref().unwrap().submit(job);
    }

    pub(crate) fn submit_syntax(&self, job: SyntaxJob) {
        self.syntax.as_ref().unwrap().submit(job);
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.events.close();
        self.search.as_ref().unwrap().stop();
        self.syntax.as_ref().unwrap().stop();
        self.search.take();
        self.syntax.take();
        if let Some(thread) = self.input.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };
    use vex_core::Document;
    use vex_editor::{Editor, SearchCompletion, SearchStatus};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }
    fn press(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.handle(key(KeyCode::Char(ch)));
        }
    }
    fn deliver(app: &mut App, event: AppEvent) {
        match event {
            AppEvent::Terminal(event) => {
                app.handle(event);
            }
            AppEvent::Background(BackgroundEvent::Search(result)) => {
                app.handle_search_result(result);
            }
            AppEvent::Background(BackgroundEvent::Syntax(result)) => {
                app.editor.apply_syntax_result(result);
            }
            AppEvent::Failed(error) => panic!("{error}"),
        }
    }
    fn searching() -> Editor {
        let mut editor = Editor::new(Document::from("x last"));
        editor.set_background_search(true);
        editor.execute("search_forward", 1).unwrap();
        editor
    }

    fn syntax_job(editor: &mut Editor) -> SyntaxJob {
        editor.begin_syntax_frame();
        editor.syntax_highlights(
            vex_core::ByteOffset(0)..vex_core::ByteOffset(editor.document().text().len_bytes()),
        );
        editor.take_syntax_job().unwrap()
    }

    #[test]
    fn services_keep_independent_latest_completions_and_alternate_when_ready() {
        let events = EventQueue::default();
        for _ in 0..INPUT_CAPACITY {
            events.terminal(key(KeyCode::Char('x')));
        }
        let mut editor = Editor::new(Document::from("fn main() {}\n"));
        editor.set_language(Some(vex_editor::Language::Rust));
        editor.set_background_syntax(true);
        let mut syntax = SyntaxWorker::default();
        events.background(BackgroundEvent::Syntax(
            syntax.run(syntax_job(&mut editor)).unwrap(),
        ));
        editor.set_language(Some(vex_editor::Language::Rust));
        events.background(BackgroundEvent::Syntax(
            syntax.run(syntax_job(&mut editor)).unwrap(),
        ));
        let mut search = searching();
        search.update_search("last").unwrap();
        events.background(BackgroundEvent::Search(
            search.take_search_job().unwrap().run().unwrap(),
        ));
        assert!(matches!(
            events.next(Duration::ZERO, true),
            Some(AppEvent::Background(BackgroundEvent::Search(_)))
        ));
        // Replenishing search cannot starve syntax, or overwrite its latest result.
        search.update_search("last").unwrap();
        events.background(BackgroundEvent::Search(
            search.take_search_job().unwrap().run().unwrap(),
        ));
        let Some(AppEvent::Background(BackgroundEvent::Syntax(result))) =
            events.next(Duration::ZERO, true)
        else {
            panic!("syntax completion was lost or starved");
        };
        assert!(editor.apply_syntax_result(result));
        assert!(matches!(
            events.next(Duration::ZERO, true),
            Some(AppEvent::Background(BackgroundEvent::Search(_)))
        ));
        assert!(events.next(Duration::ZERO, true).is_none());
        assert_eq!(events.0.state.lock().unwrap().input.len(), INPUT_CAPACITY);
    }

    #[test]
    fn syntax_wakes_the_ui_while_search_is_still_running() {
        let events = EventQueue::default();
        let (started, observed) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let search = SearchWorker::with_runner(events.clone(), move |job| {
            started.send(()).unwrap();
            gate.recv_timeout(Duration::from_secs(3)).unwrap();
            job.run()
        })
        .unwrap();
        let mut editor = searching();
        editor.update_search("last").unwrap();
        search.submit(editor.take_search_job().unwrap());
        observed.recv_timeout(Duration::from_secs(3)).unwrap();
        editor.set_language(Some(vex_editor::Language::Rust));
        editor.set_background_syntax(true);
        let mut state = SyntaxWorker::default();
        let syntax = LatestWorker::spawn("test-syntax", events.clone(), move |job| {
            state.run(job).map(BackgroundEvent::Syntax)
        })
        .unwrap();
        syntax.submit(syntax_job(&mut editor));
        let Some(AppEvent::Background(BackgroundEvent::Syntax(result))) =
            events.next(Duration::from_secs(3), true)
        else {
            panic!("syntax must complete independently of search");
        };
        assert!(editor.apply_syntax_result(result));
        assert!(editor.search_pending());
        release.send(()).unwrap();
        let Some(AppEvent::Background(BackgroundEvent::Search(result))) =
            events.next(Duration::from_secs(3), true)
        else {
            panic!("search completion missing");
        };
        editor.apply_search_result(result).unwrap();
    }

    #[test]
    fn full_input_queue_cannot_block_worker_completion_or_failure() {
        let events = EventQueue::default();
        for _ in 0..INPUT_CAPACITY {
            assert!(events.terminal(key(KeyCode::Char('x'))));
        }
        let worker = SearchWorker::start(events.clone()).unwrap();
        let mut editor = searching();
        editor.update_search("last").unwrap();
        worker.submit(editor.take_search_job().unwrap());
        let Some(AppEvent::Background(BackgroundEvent::Search(result))) =
            events.next(Duration::from_secs(3), true)
        else {
            panic!("completion must wake an idle UI even with full input");
        };
        assert_eq!(
            editor.apply_search_result(result).unwrap(),
            SearchCompletion::Preview
        );
        assert_eq!(editor.search_status(), Some(SearchStatus::Match));
        assert_eq!(events.0.state.lock().unwrap().input.len(), INPUT_CAPACITY);
        events.fail(io::Error::other("input failed"));
        assert!(matches!(
            events.next(Duration::ZERO, true),
            Some(AppEvent::Failed(_))
        ));
    }

    #[test]
    fn closing_the_queue_releases_a_blocked_input_producer() {
        let events = EventQueue::default();
        for _ in 0..INPUT_CAPACITY {
            events.terminal(key(KeyCode::Char('x')));
        }
        let sender = events.clone();
        let thread = thread::spawn(move || sender.terminal(key(KeyCode::Char('y'))));
        events.close();
        assert!(!thread.join().unwrap());
        assert!(events.next(Duration::ZERO, false).is_none());
    }

    #[test]
    fn worker_failure_wakes_the_ui_instead_of_leaving_search_pending_forever() {
        let events = EventQueue::default();
        let worker =
            SearchWorker::with_runner(events.clone(), |_| panic!("injected search failure"))
                .unwrap();
        let mut editor = searching();
        editor.update_search("last").unwrap();
        worker.submit(editor.take_search_job().unwrap());
        let Some(AppEvent::Failed(error)) = events.next(Duration::from_secs(3), true) else {
            panic!("missing worker failure");
        };
        assert!(error.to_string().contains("search worker panicked"));
        drop(worker);
    }

    #[test]
    fn rapid_queries_cancel_running_work_and_keep_only_the_latest_pending_job() {
        let events = EventQueue::default();
        let (started, observed) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&calls);
        let worker = SearchWorker::with_runner(events.clone(), move |job| {
            if count.fetch_add(1, Ordering::Relaxed) == 0 {
                started.send(()).unwrap();
                gate.recv_timeout(Duration::from_secs(3)).unwrap();
            }
            job.run()
        })
        .unwrap();
        let mut editor = searching();
        editor.update_search("first").unwrap();
        let job = editor.take_search_job().unwrap();
        let first = job.cancellation();
        worker.submit(job);
        observed.recv_timeout(Duration::from_secs(3)).unwrap();
        let mut superseded = vec![];
        for _ in 0..100 {
            editor.update_search("last").unwrap();
            let job = editor.take_search_job().unwrap();
            superseded.push(job.cancellation());
            worker.submit(job);
        }
        assert!(first.is_cancelled());
        assert!(superseded[..99].iter().all(Cancellation::is_cancelled));
        assert!(!superseded[99].is_cancelled());
        assert!(worker.mailbox.state.lock().unwrap().pending.is_some());
        release.send(()).unwrap();
        let Some(AppEvent::Background(BackgroundEvent::Search(result))) =
            events.next(Duration::from_secs(3), false)
        else {
            panic!("missing latest result");
        };
        assert_eq!(
            editor.apply_search_result(result).unwrap(),
            SearchCompletion::Preview
        );
        assert_eq!(editor.search_status(), Some(SearchStatus::Match));
        drop(worker);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert!(events.next(Duration::ZERO, false).is_none());
    }

    #[test]
    fn worker_shutdown_cancels_active_work_and_joins_it() {
        let events = EventQueue::default();
        let (started, observed) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let worker = SearchWorker::with_runner(events, move |job| {
            started.send(()).unwrap();
            gate.recv_timeout(Duration::from_secs(3)).unwrap();
            assert!(job.cancellation().is_cancelled());
            job.run()
        })
        .unwrap();
        let mut editor = searching();
        editor.update_search("last").unwrap();
        worker.submit(editor.take_search_job().unwrap());
        observed.recv_timeout(Duration::from_secs(3)).unwrap();
        let mailbox = Arc::clone(&worker.mailbox);
        let joining = thread::spawn(move || drop(worker));
        let state = mailbox.state.lock().unwrap();
        let (state, timeout) = mailbox
            .ready
            .wait_timeout_while(state, Duration::from_secs(3), |s| !s.closed)
            .unwrap();
        assert!(!timeout.timed_out());
        assert!(state.running.as_ref().unwrap().is_cancelled());
        drop(state);
        release.send(()).unwrap();
        joining.join().unwrap();
    }

    #[test]
    fn early_enter_defers_edits_preserves_escape_order_and_allows_resize() {
        let mut app = App::from_document(Document::from("x cat"), (40, 8));
        app.editor.set_background_search(true);
        press(&mut app, "/cat");
        let result = app.editor.take_search_job().unwrap().run().unwrap();
        app.handle(key(KeyCode::Enter));
        assert!(app.editor.search_waiting());
        let events = EventQueue::default();
        for code in [KeyCode::Char('c'), KeyCode::Char('X'), KeyCode::Esc] {
            events.terminal(key(code));
        }
        events.terminal(Event::Resize(60, 10));
        deliver(&mut app, events.next(Duration::ZERO, true).unwrap());
        assert_eq!(app.size(), (60, 10));
        assert!(events.next(Duration::ZERO, true).is_none());
        assert_eq!(app.editor.document().text(), "x cat");
        events.background(BackgroundEvent::Search(result));
        while let Some(event) = events.next(Duration::ZERO, app.editor.search_waiting()) {
            deliver(&mut app, event);
        }
        assert_eq!(app.editor.document().text(), "x X");
        assert_eq!(app.editor.mode(), vex_editor::Mode::Normal);
        assert_eq!(app.editor.search_direction(), None);
        assert_eq!(app.editor.document().undo_depth(), 1);
    }

    #[test]
    fn escape_cancels_an_accepted_pending_search_before_its_ready_result() {
        let mut app = App::from_document(Document::from("x cat"), (40, 8));
        app.editor.set_background_search(true);
        let original = app.editor.selections().clone();
        press(&mut app, "/cat");
        let result = app.editor.take_search_job().unwrap().run().unwrap();
        app.handle(key(KeyCode::Enter));
        let events = EventQueue::default();
        events.background(BackgroundEvent::Search(result));
        events.terminal(key(KeyCode::Esc));
        deliver(&mut app, events.next(Duration::ZERO, true).unwrap());
        assert!(!app.editor.search_waiting());
        assert_eq!(app.editor.search_direction(), None);
        let Some(AppEvent::Background(BackgroundEvent::Search(result))) =
            events.next(Duration::ZERO, false)
        else {
            panic!("queued result");
        };
        assert!(!app.handle_search_result(result));
        assert_eq!(app.editor.selections(), &original);
    }
}
