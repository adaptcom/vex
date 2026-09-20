//! Wakeable event inbox and owned producer threads. Terminal input is bounded;
//! independent services have typed completions and explicit delivery policies.

use crate::picker::buffers::{BufferJob, BufferResult};
use crate::picker::files::{FileJob, FileResult, FileWorker, PreviewJob, PreviewResult};
use crate::picker::symbols::{SymbolJob, SymbolResult};
use crossterm::event::{self, Event, MouseEvent, MouseEventKind};
use std::{
    collections::{HashMap, VecDeque},
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
    MouseScroll(MouseEvent, usize),
    Background(BackgroundEvent),
    Lsp(vex_lsp::Event),
    GitWrite(vex_git::write::Result),
    Failed(io::Error),
}

/// Snapshot services keep only their latest completion. Clipboard requests admit
/// one operation at a time; replacement requires explicit cancellation. Services
/// with ordered protocol messages must use a FIFO rather than these slots.
pub(crate) enum BackgroundEvent {
    Search(SearchResult),
    Syntax(Vec<SyntaxResult>),
    Files(FileResult),
    Symbols(SymbolResult),
    Locations(crate::picker::locations::Result),
    Diagnostics(crate::picker::diagnostics::Result),
    LocationNavigation(crate::app::LocationNavigationResult),
    WorkspaceEdit(crate::app::WorkspaceEditResult),
    Buffers(BufferResult),
    Jumps(crate::picker::jumps::Result),
    JumpNavigation(crate::app::JumpNavigationResult),
    Prompt(crate::prompt::Result),
    WorkspaceSearch(crate::picker::search::Result),
    Preview(PreviewResult),
    Git(vex_git::Result),
    GitStatus(vex_git::status::Result),
    FilePoll(crate::files::watch::Result),
    Clipboard(crate::clipboard::Result),
}

#[derive(Default)]
struct Inbox {
    input: VecDeque<Event>,
    lsp: VecDeque<vex_lsp::Event>,
    git_writes: VecDeque<vex_git::write::Result>,
    background: [Option<BackgroundEvent>; 8],
    next_background: usize,
    prefer_input: bool,
    failure: Option<io::Error>,
    closed: bool,
}

impl Inbox {
    fn pop_input(&mut self) -> Option<AppEvent> {
        let event = self.input.pop_front()?;
        if let Event::Mouse(mouse) = event
            && matches!(
                mouse.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            )
        {
            let mut count = 1;
            // Never cross a key, release, resize, direction or pointer change.
            // The queue is bounded, so one batch has at most INPUT_CAPACITY steps.
            while self.input.front() == Some(&event) {
                self.input.pop_front();
                count += 1;
            }
            return Some(AppEvent::MouseScroll(mouse, count));
        }
        Some(AppEvent::Terminal(event))
    }
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
    fn git_write(&self, result: vex_git::write::Result) {
        let mut state = self.0.state.lock().unwrap();
        while state.git_writes.len() == 32 && !state.closed {
            state = self.0.space.wait(state).unwrap();
        }
        if !state.closed {
            state.git_writes.push_back(result);
            self.0.ready.notify_one();
        }
    }
    /// Protocol results use a bounded FIFO. Only service threads call this;
    /// backpressure never blocks editing, and closing releases blocked senders.
    fn lsp(&self, event: vex_lsp::Event) {
        let mut state = self.0.state.lock().unwrap();
        if matches!(event, vex_lsp::Event::DiagnosticCatalog(_))
            && matches!(state.lsp.back(), Some(vex_lsp::Event::DiagnosticCatalog(_)))
        {
            state.lsp.pop_back();
        }
        while state.lsp.len() == 128 && !state.closed {
            state = self.0.space.wait(state).unwrap();
        }
        if !state.closed {
            state.lsp.push_back(event);
            self.0.ready.notify_one();
        }
    }
    fn terminal(&self, event: Event) -> bool {
        let mut state = self.0.state.lock().unwrap();
        if state.closed {
            return false;
        }
        if matches!(
            event,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                ..
            })
        ) {
            return true;
        }
        if let Event::Mouse(mouse) = event
            && matches!(mouse.kind, MouseEventKind::Drag(_))
            && let Some(Event::Mouse(previous)) = state.input.back_mut()
            && previous.kind == mouse.kind
            && previous.modifiers == mouse.modifiers
        {
            *previous = mouse;
            return true;
        }
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
                BackgroundEvent::Files(_)
                | BackgroundEvent::Symbols(_)
                | BackgroundEvent::Locations(_)
                | BackgroundEvent::Diagnostics(_)
                | BackgroundEvent::LocationNavigation(_)
                | BackgroundEvent::WorkspaceEdit(_)
                | BackgroundEvent::Buffers(_)
                | BackgroundEvent::Jumps(_)
                | BackgroundEvent::JumpNavigation(_)
                | BackgroundEvent::Prompt(_)
                | BackgroundEvent::WorkspaceSearch(_) => 2,
                BackgroundEvent::Preview(_) => 3,
                BackgroundEvent::Git(_) => 4,
                BackgroundEvent::GitStatus(_) => 5,
                BackgroundEvent::FilePoll(_) => 6,
                BackgroundEvent::Clipboard(_) => 7,
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
        state.lsp.clear();
        state.git_writes.clear();
        state.background = [None, None, None, None, None, None, None, None];
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
                self.0.space.notify_all();
                return Some(AppEvent::Terminal(event));
            }
            if !waiting
                && state.prefer_input
                && let Some(event) = state.pop_input()
            {
                state.prefer_input = false;
                self.0.space.notify_all();
                return Some(event);
            }
            let services = state.background.len() + 2;
            for offset in 0..services {
                let index = (state.next_background + offset) % services;
                if index == state.background.len() + 1 {
                    if let Some(result) = state.git_writes.pop_front() {
                        state.next_background = 0;
                        state.prefer_input = true;
                        self.0.space.notify_all();
                        return Some(AppEvent::GitWrite(result));
                    }
                    continue;
                }
                if index == state.background.len() {
                    if let Some(event) = state.lsp.pop_front() {
                        state.next_background = index + 1;
                        state.prefer_input = true;
                        self.0.space.notify_all();
                        return Some(AppEvent::Lsp(event));
                    }
                    continue;
                }
                if let Some(result) = state.background[index].take() {
                    state.next_background = (index + 1) % services;
                    state.prefer_input = true;
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
                if matches!(event, Event::Resize(..) | Event::FocusLost) {
                    // These notifications can pass input held for a worker.
                    // Stale pointer coordinates/presses must not restart a drag.
                    let mut position = 0;
                    state.input.retain(|queued| {
                        let keep = position >= index || !matches!(queued, Event::Mouse(_));
                        position += 1;
                        keep
                    });
                }
                self.0.space.notify_all();
                return Some(AppEvent::Terminal(event));
            }
            if !waiting && let Some(event) = state.pop_input() {
                self.0.space.notify_all();
                return Some(event);
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

impl Job for crate::files::watch::Batch {
    fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }
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

/// All visible buffers travel together through the latest-result slot, so one
/// buffer's highlight completion cannot replace another buffer's result.
pub(crate) struct SyntaxBatch {
    pub documents: Vec<vex_core::DocumentId>,
    pub jobs: Vec<SyntaxJob>,
    pub cancellation: Cancellation,
}

impl Job for SyntaxBatch {
    fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }
}

#[derive(Default)]
struct SyntaxBuffers(HashMap<vex_core::DocumentId, SyntaxWorker>);

impl SyntaxBuffers {
    fn run(&mut self, batch: SyntaxBatch) -> Option<BackgroundEvent> {
        self.0.retain(|id, _| batch.documents.contains(id));
        let mut results = Vec::new();
        for job in batch.jobs {
            if batch.cancellation.is_cancelled() {
                return None;
            }
            let worker = self.0.entry(job.document_id()).or_default();
            if let Some(result) = worker.run_cancellable(job, || batch.cancellation.is_cancelled())
            {
                results.push(result);
            }
        }
        (!batch.cancellation.is_cancelled()).then_some(BackgroundEvent::Syntax(results))
    }
}

enum PickerJob {
    Files(FileJob),
    Symbols(SymbolJob),
    Locations(crate::picker::locations::Job),
    Diagnostics(crate::picker::diagnostics::Job),
    LocationNavigation(crate::app::LocationNavigationJob),
    WorkspaceEdit(crate::app::WorkspaceEditJob),
    Buffers(BufferJob),
    Jumps(crate::picker::jumps::Job),
    JumpNavigation(crate::app::JumpNavigationJob),
    Prompt(crate::prompt::Job),
    WorkspaceSearch(crate::picker::search::Job),
}

impl Job for PickerJob {
    fn cancellation(&self) -> Cancellation {
        match self {
            Self::Files(job) => job.cancellation.clone(),
            Self::Symbols(job) => job.cancellation.clone(),
            Self::Locations(job) => job.cancellation.clone(),
            Self::Diagnostics(job) => job.cancellation.clone(),
            Self::LocationNavigation(job) => job.cancellation.clone(),
            Self::WorkspaceEdit(job) => job.cancellation.clone(),
            Self::Buffers(job) => job.cancellation.clone(),
            Self::Jumps(job) => job.cancellation.clone(),
            Self::JumpNavigation(job) => job.cancellation.clone(),
            Self::Prompt(job) => job.cancellation.clone(),
            Self::WorkspaceSearch(job) => job.cancellation.clone(),
        }
    }
}
impl Job for PreviewJob {
    fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }
}

impl Job for crate::clipboard::Job {
    fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }
}

impl Job for vex_git::Batch {
    fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }
}

impl Job for vex_git::status::Batch {
    fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
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

/// Ordered writes drain on shutdown. Only read workers discard pending work.
struct WriteWorker {
    sender: Option<std::sync::mpsc::Sender<vex_git::write::Job>>,
    thread: Option<JoinHandle<()>>,
}

impl WriteWorker {
    fn start(events: EventQueue) -> io::Result<Self> {
        Self::with_runner(events, vex_git::write::Job::run)
    }

    fn with_runner(
        events: EventQueue,
        run: impl Fn(vex_git::write::Job) -> vex_git::write::Result + Send + 'static,
    ) -> io::Result<Self> {
        let (sender, receiver) = std::sync::mpsc::channel();
        let thread = thread::Builder::new()
            .name("vex-git-write".into())
            .spawn(move || {
                if catch_unwind(AssertUnwindSafe(|| {
                    for job in receiver {
                        events.git_write(run(job));
                    }
                }))
                .is_err()
                {
                    events.fail(io::Error::other("Git write worker panicked"));
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            thread: Some(thread),
        })
    }

    fn submit(&self, job: vex_git::write::Job) {
        // The UI admits one operation per repository, at most 16 overall.
        let _ = self.sender.as_ref().unwrap().send(job);
    }
}

impl Drop for WriteWorker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The runtime owns every producer and joins them before terminal restoration.
pub(crate) struct Runtime {
    pub events: EventQueue,
    search: Option<SearchWorker>,
    syntax: Option<LatestWorker<SyntaxBatch>>,
    lsp: Option<vex_lsp::Service>,
    files: Option<LatestWorker<PickerJob>>,
    preview: Option<LatestWorker<PreviewJob>>,
    git: Option<LatestWorker<vex_git::Batch>>,
    git_status: Option<LatestWorker<vex_git::status::Batch>>,
    git_write: Option<WriteWorker>,
    file_poll: Option<LatestWorker<crate::files::watch::Batch>>,
    clipboard: Option<LatestWorker<crate::clipboard::Job>>,
    input: Option<JoinHandle<()>>,
}

impl Runtime {
    pub(crate) fn start() -> io::Result<Self> {
        let events = EventQueue::default();
        let mut clipboard_state = crate::clipboard::Worker::default();
        let clipboard = LatestWorker::spawn("vex-clipboard", events.clone(), move |job| {
            clipboard_state.run(job).map(BackgroundEvent::Clipboard)
        })?;
        let git_write = WriteWorker::start(events.clone())?;
        let mut poll_state = crate::files::watch::Worker::default();
        let file_poll = LatestWorker::spawn("vex-file-poll", events.clone(), move |batch| {
            poll_state.run(batch).map(BackgroundEvent::FilePoll)
        })?;
        let search = SearchWorker::start(events.clone())?;
        let mut syntax_state = SyntaxBuffers::default();
        let syntax = LatestWorker::spawn("vex-syntax", events.clone(), move |job| {
            syntax_state.run(job)
        })?;
        let queue = events.clone();
        let lsp = vex_lsp::Service::start(move |event| queue.lsp(event))?;
        let queue = events.clone();
        let mut file_state = FileWorker::default();
        let mut workspace_search = crate::picker::search::Worker::default();
        let mut diagnostic_state = crate::picker::diagnostics::Worker::default();
        let files = LatestWorker::spawn("vex-picker", events.clone(), move |job| match job {
            PickerJob::Diagnostics(job) => {
                workspace_search = crate::picker::search::Worker::default();
                file_state = FileWorker::default();
                diagnostic_state.run(job).map(BackgroundEvent::Diagnostics)
            }
            PickerJob::Files(job) => {
                diagnostic_state = crate::picker::diagnostics::Worker::default();
                workspace_search = crate::picker::search::Worker::default();
                file_state.run(job, |result| {
                    queue.background(BackgroundEvent::Files(result))
                });
                None
            }
            PickerJob::Locations(job) => {
                diagnostic_state = crate::picker::diagnostics::Worker::default();
                workspace_search = crate::picker::search::Worker::default();
                file_state = FileWorker::default();
                job.run().map(BackgroundEvent::Locations)
            }
            PickerJob::WorkspaceEdit(job) => {
                diagnostic_state = crate::picker::diagnostics::Worker::default();
                workspace_search = crate::picker::search::Worker::default();
                file_state = FileWorker::default();
                job.run().map(BackgroundEvent::WorkspaceEdit)
            }
            PickerJob::LocationNavigation(job) => {
                diagnostic_state = crate::picker::diagnostics::Worker::default();
                workspace_search = crate::picker::search::Worker::default();
                file_state = FileWorker::default();
                job.run().map(BackgroundEvent::LocationNavigation)
            }
            PickerJob::Symbols(job) => {
                diagnostic_state = crate::picker::diagnostics::Worker::default();
                workspace_search = crate::picker::search::Worker::default();
                file_state = FileWorker::default();
                job.run().map(BackgroundEvent::Symbols)
            }
            PickerJob::Buffers(job) => {
                diagnostic_state = crate::picker::diagnostics::Worker::default();
                workspace_search = crate::picker::search::Worker::default();
                file_state = FileWorker::default();
                job.run().map(BackgroundEvent::Buffers)
            }
            PickerJob::Jumps(job) => {
                diagnostic_state = crate::picker::diagnostics::Worker::default();
                workspace_search = crate::picker::search::Worker::default();
                file_state = FileWorker::default();
                job.run().map(BackgroundEvent::Jumps)
            }
            PickerJob::JumpNavigation(job) => {
                diagnostic_state = crate::picker::diagnostics::Worker::default();
                workspace_search = crate::picker::search::Worker::default();
                file_state = FileWorker::default();
                job.run().map(BackgroundEvent::JumpNavigation)
            }
            PickerJob::Prompt(job) => {
                diagnostic_state = crate::picker::diagnostics::Worker::default();
                workspace_search = crate::picker::search::Worker::default();
                job.run().map(BackgroundEvent::Prompt)
            }
            PickerJob::WorkspaceSearch(job) => {
                diagnostic_state = crate::picker::diagnostics::Worker::default();
                file_state = FileWorker::default();
                workspace_search.run(job, |result| {
                    queue.background(BackgroundEvent::WorkspaceSearch(result))
                });
                None
            }
        })?;
        let preview = LatestWorker::spawn("vex-preview", events.clone(), |job: PreviewJob| {
            job.run().map(BackgroundEvent::Preview)
        })?;
        let mut git_state = vex_git::Worker::default();
        let git = LatestWorker::spawn("vex-git", events.clone(), move |job| {
            git_state.run(job).map(BackgroundEvent::Git)
        })?;
        let git_status = LatestWorker::spawn(
            "vex-git-status",
            events.clone(),
            |job: vex_git::status::Batch| {
                let cancellation = job.cancellation.clone();
                job.run()
                    .and_then(|result| crate::git_status::highlight(result, &cancellation))
                    .map(BackgroundEvent::GitStatus)
            },
        )?;
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
            lsp: Some(lsp),
            files: Some(files),
            preview: Some(preview),
            git: Some(git),
            git_status: Some(git_status),
            git_write: Some(git_write),
            file_poll: Some(file_poll),
            clipboard: Some(clipboard),
            input: Some(input),
        })
    }

    pub(crate) fn submit(&self, job: SearchJob) {
        self.search.as_ref().unwrap().submit(job);
    }

    pub(crate) fn submit_syntax(&self, job: SyntaxBatch) {
        self.syntax.as_ref().unwrap().submit(job);
    }

    pub(crate) fn update_lsp(&self, update: vex_lsp::Update) {
        self.lsp.as_ref().unwrap().update(update);
    }
    pub(crate) fn update_lsp_workspace(&self, update: vex_lsp::WorkspaceUpdate) {
        self.lsp.as_ref().unwrap().update_workspace(update);
    }

    pub(crate) fn submit_picker(&self, job: FileJob) {
        self.files.as_ref().unwrap().submit(PickerJob::Files(job));
    }
    pub(crate) fn submit_diagnostics(&self, job: crate::picker::diagnostics::Job) {
        self.files
            .as_ref()
            .unwrap()
            .submit(PickerJob::Diagnostics(job));
    }
    pub(crate) fn submit_locations(&self, job: crate::picker::locations::Job) {
        self.files
            .as_ref()
            .unwrap()
            .submit(PickerJob::Locations(job));
    }
    pub(crate) fn submit_workspace_edit(&self, job: crate::app::WorkspaceEditJob) {
        self.files
            .as_ref()
            .unwrap()
            .submit(PickerJob::WorkspaceEdit(job));
    }
    pub(crate) fn submit_location_navigation(&self, job: crate::app::LocationNavigationJob) {
        self.files
            .as_ref()
            .unwrap()
            .submit(PickerJob::LocationNavigation(job));
    }
    pub(crate) fn submit_symbols(&self, job: SymbolJob) {
        self.files.as_ref().unwrap().submit(PickerJob::Symbols(job));
    }
    pub(crate) fn submit_buffers(&self, job: BufferJob) {
        self.files.as_ref().unwrap().submit(PickerJob::Buffers(job));
    }
    pub(crate) fn submit_jumps(&self, job: crate::picker::jumps::Job) {
        self.files.as_ref().unwrap().submit(PickerJob::Jumps(job));
    }
    pub(crate) fn submit_jump_navigation(&self, job: crate::app::JumpNavigationJob) {
        self.files
            .as_ref()
            .unwrap()
            .submit(PickerJob::JumpNavigation(job));
    }
    pub(crate) fn submit_prompt(&self, job: crate::prompt::Job) {
        self.files.as_ref().unwrap().submit(PickerJob::Prompt(job));
    }
    pub(crate) fn submit_workspace_search(&self, job: crate::picker::search::Job) {
        self.files
            .as_ref()
            .unwrap()
            .submit(PickerJob::WorkspaceSearch(job));
    }
    pub(crate) fn submit_preview(&self, job: PreviewJob) {
        self.preview.as_ref().unwrap().submit(job);
    }
    pub(crate) fn submit_git(&self, job: vex_git::Batch) {
        self.git.as_ref().unwrap().submit(job);
    }
    pub(crate) fn submit_status(&self, job: vex_git::status::Batch) {
        self.git_status.as_ref().unwrap().submit(job);
    }
    pub(crate) fn submit_git_write(&self, job: vex_git::write::Job) {
        self.git_write.as_ref().unwrap().submit(job);
    }
    pub(crate) fn submit_file_poll(&self, batch: crate::files::watch::Batch) {
        self.file_poll.as_ref().unwrap().submit(batch);
    }

    pub(crate) fn submit_clipboard(&self, job: crate::clipboard::Job) {
        self.clipboard.as_ref().unwrap().submit(job);
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.events.close();
        self.search.as_ref().unwrap().stop();
        self.syntax.as_ref().unwrap().stop();
        self.lsp.as_ref().unwrap().stop();
        self.files.as_ref().unwrap().stop();
        self.preview.as_ref().unwrap().stop();
        self.git.as_ref().unwrap().stop();
        self.git_status.as_ref().unwrap().stop();
        self.file_poll.as_ref().unwrap().stop();
        self.clipboard.as_ref().unwrap().stop();
        self.search.take();
        self.syntax.take();
        self.lsp.take();
        self.files.take();
        self.preview.take();
        self.git.take();
        self.git_status.take();
        self.git_write.take();
        self.file_poll.take();
        self.clipboard.take();
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

    #[test]
    fn mouse_bursts_coalesce_without_crossing_input_or_position_boundaries() {
        use crossterm::event::MouseButton;
        let queue = EventQueue::default();
        let mouse = |kind, column| MouseEvent {
            kind,
            column,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        let wheel = mouse(MouseEventKind::ScrollDown, 2);
        for _ in 0..50 {
            queue.terminal(Event::Mouse(wheel));
        }
        let other_pane = mouse(MouseEventKind::ScrollDown, 40);
        queue.terminal(Event::Mouse(other_pane));
        queue.terminal(key(KeyCode::Char('j')));
        queue.terminal(Event::Mouse(wheel));
        let up = mouse(MouseEventKind::ScrollUp, 2);
        queue.terminal(Event::Mouse(up));
        assert!(
            matches!(queue.next(Duration::ZERO, false), Some(AppEvent::MouseScroll(event, 50)) if event == wheel)
        );
        assert!(
            matches!(queue.next(Duration::ZERO, false), Some(AppEvent::MouseScroll(event, 1)) if event == other_pane)
        );
        assert!(matches!(
            queue.next(Duration::ZERO, false),
            Some(AppEvent::Terminal(Event::Key(_)))
        ));
        assert!(
            matches!(queue.next(Duration::ZERO, false), Some(AppEvent::MouseScroll(event, 1)) if event == wheel)
        );
        assert!(
            matches!(queue.next(Duration::ZERO, false), Some(AppEvent::MouseScroll(event, 1)) if event == up)
        );
        let down = Event::Mouse(mouse(MouseEventKind::Down(MouseButton::Left), 10));
        queue.terminal(down.clone());
        for column in 0..1000 {
            queue.terminal(Event::Mouse(mouse(
                MouseEventKind::Drag(MouseButton::Left),
                column,
            )));
        }
        let release = Event::Mouse(mouse(MouseEventKind::Up(MouseButton::Left), 999));
        queue.terminal(release.clone());
        queue.terminal(Event::Mouse(mouse(MouseEventKind::Moved, 3)));
        assert!(
            matches!(queue.next(Duration::ZERO, false), Some(AppEvent::Terminal(event)) if event == down)
        );
        assert!(
            matches!(queue.next(Duration::ZERO, false), Some(AppEvent::Terminal(Event::Mouse(event))) if event.column == 999 && event.kind == MouseEventKind::Drag(MouseButton::Left))
        );
        assert!(
            matches!(queue.next(Duration::ZERO, false), Some(AppEvent::Terminal(event)) if event == release)
        );
        assert!(queue.next(Duration::ZERO, false).is_none());
    }

    #[test]
    fn focus_loss_and_resize_do_not_replay_stale_pointer_presses_after_a_worker() {
        for notification in [Event::FocusLost, Event::Resize(80, 20)] {
            let queue = EventQueue::default();
            queue.terminal(Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 40,
                row: 4,
                modifiers: KeyModifiers::NONE,
            }));
            queue.terminal(key(KeyCode::Char('j')));
            queue.terminal(notification.clone());
            assert!(
                matches!(queue.next(Duration::ZERO, true), Some(AppEvent::Terminal(event)) if event == notification)
            );
            assert!(matches!(
                queue.next(Duration::ZERO, false),
                Some(AppEvent::Terminal(Event::Key(_)))
            ));
            assert!(queue.next(Duration::ZERO, false).is_none());
        }
    }

    #[test]
    fn git_writes_finish_in_order_and_every_result_survives_worker_shutdown() {
        let events = EventQueue::default();
        let ran = Arc::new(Mutex::new(Vec::new()));
        let log = ran.clone();
        let worker = WriteWorker::with_runner(events.clone(), move |job| {
            log.lock().unwrap().push(job.id);
            vex_git::write::Result {
                id: job.id,
                root: job.root,
                operation: job.operation,
                outcome: if job.id == 2 {
                    Err("injected failure".into())
                } else {
                    Ok(String::new())
                },
            }
        })
        .unwrap();
        for id in 1..=16 {
            worker.submit(vex_git::write::Job {
                id,
                root: "/repo".into(),
                operation: vex_git::write::Operation::Commit {
                    message: format!("{id}"),
                },
            });
        }
        drop(worker);
        assert_eq!(*ran.lock().unwrap(), (1..=16).collect::<Vec<_>>());
        for id in 1..=16 {
            let Some(AppEvent::GitWrite(result)) = events.next(Duration::ZERO, false) else {
                panic!("missing write completion");
            };
            assert_eq!(result.id, id);
            assert_eq!(result.outcome.is_err(), id == 2);
        }
        assert!(events.next(Duration::ZERO, false).is_none());
    }

    #[test]
    fn closing_the_inbox_releases_a_blocked_write_producer_without_dropping_jobs() {
        let events = EventQueue::default();
        let ran = Arc::new(AtomicUsize::new(0));
        let log = ran.clone();
        let worker = WriteWorker::with_runner(events.clone(), move |job| {
            log.fetch_add(1, Ordering::SeqCst);
            vex_git::write::Result {
                id: job.id,
                root: job.root,
                operation: job.operation,
                outcome: Ok(String::new()),
            }
        })
        .unwrap();
        for id in 0..64 {
            worker.submit(vex_git::write::Job {
                id,
                root: "/repo".into(),
                operation: vex_git::write::Operation::Commit {
                    message: String::new(),
                },
            });
        }
        events.close();
        drop(worker);
        assert_eq!(ran.load(Ordering::SeqCst), 64);
    }

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
            AppEvent::MouseScroll(event, count) => {
                app.handle_mouse(event, count);
            }
            AppEvent::Terminal(event) => {
                app.handle(event);
            }
            AppEvent::Background(BackgroundEvent::Search(result)) => {
                app.handle_search_result(result);
            }
            AppEvent::Background(BackgroundEvent::Syntax(result)) => {
                app.handle_syntax_results(result);
            }
            AppEvent::Background(BackgroundEvent::Files(result)) => {
                app.handle_picker_result(result);
            }
            AppEvent::Background(BackgroundEvent::Diagnostics(result)) => {
                app.handle_diagnostic_result(result);
            }
            AppEvent::Background(BackgroundEvent::Locations(result)) => {
                app.handle_location_result(result);
            }
            AppEvent::Background(BackgroundEvent::WorkspaceEdit(result)) => {
                app.handle_workspace_edit(result);
            }
            AppEvent::Background(BackgroundEvent::LocationNavigation(result)) => {
                app.handle_location_navigation(result);
            }
            AppEvent::Background(BackgroundEvent::Symbols(result)) => {
                app.handle_symbol_result(result);
            }
            AppEvent::Background(BackgroundEvent::Buffers(result)) => {
                app.handle_buffer_result(result);
            }
            AppEvent::Background(BackgroundEvent::Jumps(result)) => {
                app.handle_jump_result(result);
            }
            AppEvent::Background(BackgroundEvent::JumpNavigation(result)) => {
                app.handle_jump_navigation(result);
            }
            AppEvent::Background(BackgroundEvent::Prompt(result)) => {
                app.handle_prompt_completion(result);
            }
            AppEvent::Background(BackgroundEvent::WorkspaceSearch(result)) => {
                app.handle_workspace_search_result(result);
            }
            AppEvent::Background(BackgroundEvent::Preview(result)) => {
                app.handle_preview_result(result);
            }
            AppEvent::Background(BackgroundEvent::Git(result)) => {
                app.handle_git_result(result);
            }
            AppEvent::Background(BackgroundEvent::GitStatus(result)) => {
                app.handle_status_result(result);
            }
            AppEvent::Lsp(event) => {
                app.handle_lsp_event(event);
            }
            AppEvent::GitWrite(result) => {
                app.handle_git_write(result);
            }
            AppEvent::Background(BackgroundEvent::FilePoll(result)) => {
                app.handle_file_poll(result, Instant::now());
            }
            AppEvent::Background(BackgroundEvent::Clipboard(result)) => {
                app.handle_clipboard_result(result);
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
    fn workspace_edits_keep_later_undo_queued_and_escape_cancels_preparation() {
        use vex_lsp::{
            Position, Range,
            workspace_edit::{DocumentEdit, TextEdit, WorkspaceEdit},
        };
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.rs");
        std::fs::write(&path, "foo\n").unwrap();
        let mut app = App::open(Some(&path), (80, 24)).unwrap();
        let edit = WorkspaceEdit {
            documents: vec![DocumentEdit {
                path,
                version: None,
                edits: vec![TextEdit {
                    range: Range {
                        start: Position {
                            line: 0,
                            character: 0,
                        },
                        end: Position {
                            line: 0,
                            character: 3,
                        },
                    },
                    new_text: "bar".into(),
                }],
            }],
        };
        app.begin_workspace_edit(app.workspace_edit_context(), edit.clone(), Vec::new())
            .unwrap();
        let result = app.take_workspace_edit().unwrap().run().unwrap();
        let events = EventQueue::default();
        events.terminal(key(KeyCode::Char('u')));
        assert!(events.next(Duration::ZERO, app.input_waiting()).is_none());
        events.background(BackgroundEvent::WorkspaceEdit(result));
        let event = events.next(Duration::ZERO, app.input_waiting()).unwrap();
        deliver(&mut app, event);
        assert_eq!(app.editor.document().text(), "bar\n");
        let event = events.next(Duration::ZERO, app.input_waiting()).unwrap();
        deliver(&mut app, event);
        assert_eq!(app.editor.document().text(), "foo\n");
        app.begin_workspace_edit(app.workspace_edit_context(), edit, Vec::new())
            .unwrap();
        let job = app.take_workspace_edit().unwrap();
        events.terminal(key(KeyCode::Esc));
        let event = events.next(Duration::ZERO, app.input_waiting()).unwrap();
        deliver(&mut app, event);
        assert!(job.run().is_none());
        assert!(!app.input_waiting());
        assert_eq!(app.editor.document().text(), "foo\n");
    }

    #[test]
    fn rename_preparation_prompt_submission_and_workspace_delivery_preserve_queued_key_order() {
        use vex_lsp::{
            Answer, Position, Range, RequestKind,
            workspace_edit::{DocumentEdit, SynchronizedDocument, TextEdit, WorkspaceEdit},
        };
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.rs");
        std::fs::write(&path, "foo\n").unwrap();
        let mut app = App::open(Some(&path), (100, 24)).unwrap();
        app.enable_lsp();
        app.take_lsp_update();
        press(&mut app, " r");
        let update = app.take_lsp_update().unwrap();
        let document = update.document.unwrap();
        let events = EventQueue::default();
        events.terminal(Event::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        for ch in "bar".chars() {
            events.terminal(key(KeyCode::Char(ch)));
        }
        events.terminal(key(KeyCode::Enter));
        events.terminal(key(KeyCode::Char('u')));
        assert!(events.next(Duration::ZERO, app.input_waiting()).is_none());
        events.lsp(vex_lsp::Event::Answer {
            epoch: document.epoch,
            revision: document.snapshot.revision(),
            id: update.request.unwrap().id,
            result: Ok(Answer::RenamePrepared("foo".into())),
        });
        let mut rename = None;
        while let Some(event) = events.next(Duration::ZERO, app.input_waiting()) {
            deliver(&mut app, event);
            if let Some(update) = app.take_lsp_update()
                && let Some(request) = update.request
            {
                rename = Some(request);
            }
        }
        let request = rename.unwrap();
        assert!(matches!(&request.kind,RequestKind::Rename { name, .. } if name == "bar"));
        assert!(app.input_waiting());
        assert_eq!(app.editor.document().text(), "foo\n");
        events.lsp(vex_lsp::Event::Answer {
            epoch: document.epoch,
            revision: document.snapshot.revision(),
            id: request.id,
            result: Ok(Answer::WorkspaceEdit {
                edit: WorkspaceEdit {
                    documents: vec![DocumentEdit {
                        path: document.path.clone(),
                        version: Some(0),
                        edits: vec![TextEdit {
                            range: Range {
                                start: Position {
                                    line: 0,
                                    character: 0,
                                },
                                end: Position {
                                    line: 0,
                                    character: 3,
                                },
                            },
                            new_text: "bar".into(),
                        }],
                    }],
                },
                versions: vec![SynchronizedDocument {
                    path: document.path,
                    version: 0,
                    document: document.snapshot.id(),
                    revision: document.snapshot.revision(),
                }],
            }),
        });
        let event = events.next(Duration::ZERO, app.input_waiting()).unwrap();
        deliver(&mut app, event);
        assert!(events.next(Duration::ZERO, app.input_waiting()).is_none());
        events.background(BackgroundEvent::WorkspaceEdit(
            app.take_workspace_edit().unwrap().run().unwrap(),
        ));
        let event = events.next(Duration::ZERO, app.input_waiting()).unwrap();
        deliver(&mut app, event);
        assert_eq!(app.editor.document().text(), "bar\n");
        let event = events.next(Duration::ZERO, app.input_waiting()).unwrap();
        deliver(&mut app, event);
        assert_eq!(app.editor.document().text(), "foo\n");
        assert!(!app.input_waiting());
    }

    #[test]
    fn document_highlights_and_destination_loads_finish_before_queued_edits() {
        use vex_lsp::{Answer, Destination, Locations, Navigation, Position, Range};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.rs");
        std::fs::write(&path, "foo foo\n").unwrap();
        let mut app = App::open(Some(&path), (80, 24)).unwrap();
        app.enable_lsp();
        app.take_lsp_update();
        press(&mut app, " h");
        let update = app.take_lsp_update().unwrap();
        let document = update.document.unwrap();
        let selections = vex_editor::PreparedSelections::new(
            &document.snapshot,
            vex_core::SelectionSet::new(
                vec![
                    vex_core::Selection::new(vex_core::CharOffset(0), vex_core::CharOffset(3)),
                    vex_core::Selection::new(vex_core::CharOffset(4), vex_core::CharOffset(7)),
                ],
                0,
            )
            .unwrap(),
            vex_editor::Mode::Normal,
            &Cancellation::default(),
        )
        .unwrap();
        let events = EventQueue::default();
        for ch in "cx".chars() {
            events.terminal(key(KeyCode::Char(ch)));
        }
        events.terminal(key(KeyCode::Esc));
        events.terminal(Event::Resize(100, 30));
        let event = events.next(Duration::ZERO, app.input_waiting()).unwrap();
        deliver(&mut app, event);
        assert_eq!(app.size(), (100, 30));
        assert!(events.next(Duration::ZERO, app.input_waiting()).is_none());
        events.lsp(vex_lsp::Event::Answer {
            epoch: document.epoch,
            revision: document.snapshot.revision(),
            id: update.request.unwrap().id,
            result: Ok(Answer::DocumentHighlights(Some(selections))),
        });
        while let Some(event) = events.next(Duration::ZERO, app.input_waiting()) {
            deliver(&mut app, event);
        }
        assert_eq!(app.editor.document().text(), "x x\n");
        app.take_lsp_update();
        press(&mut app, "gy");
        let update = app.take_lsp_update().unwrap();
        let document = update.document.unwrap();
        events.terminal(key(KeyCode::Char('d')));
        assert!(events.next(Duration::ZERO, app.input_waiting()).is_none());
        events.lsp(vex_lsp::Event::Answer {
            epoch: document.epoch,
            revision: document.snapshot.revision(),
            id: update.request.unwrap().id,
            result: Ok(Answer::Locations(
                Navigation::TypeDefinition,
                Locations {
                    items: vec![Destination {
                        path: document.path.clone(),
                        range: Range {
                            start: Position {
                                line: 0,
                                character: 2,
                            },
                            end: Position {
                                line: 0,
                                character: 3,
                            },
                        },
                    }],
                    ..Default::default()
                },
            )),
        });
        let event = events.next(Duration::ZERO, app.input_waiting()).unwrap();
        deliver(&mut app, event);
        assert!(app.input_waiting());
        assert!(events.next(Duration::ZERO, app.input_waiting()).is_none());
        events.background(BackgroundEvent::LocationNavigation(
            app.take_location_navigation().unwrap().run().unwrap(),
        ));
        while let Some(event) = events.next(Duration::ZERO, app.input_waiting()) {
            deliver(&mut app, event);
        }
        assert_eq!(app.editor.document().text(), "x \n");
    }

    #[test]
    fn diagnostic_catalog_notifications_coalesce_without_reordering_protocol_answers() {
        let events = EventQueue::default();
        let catalog = vex_lsp::diagnostics::Catalog::default();
        for _ in 0..1000 {
            events.lsp(vex_lsp::Event::DiagnosticCatalog(catalog.clone()));
        }
        events.lsp(vex_lsp::Event::Answer {
            epoch: 1,
            revision: vex_core::Document::from("").revision(),
            id: 1,
            result: Ok(vex_lsp::Answer::CommandExecuted),
        });
        for _ in 0..1000 {
            events.lsp(vex_lsp::Event::DiagnosticCatalog(catalog.clone()));
        }
        assert_eq!(events.0.state.lock().unwrap().lsp.len(), 3);
        assert!(matches!(
            events.next(Duration::ZERO, false),
            Some(AppEvent::Lsp(vex_lsp::Event::DiagnosticCatalog(_)))
        ));
        assert!(matches!(
            events.next(Duration::ZERO, false),
            Some(AppEvent::Lsp(vex_lsp::Event::Answer { id: 1, .. }))
        ));
        assert!(matches!(
            events.next(Duration::ZERO, false),
            Some(AppEvent::Lsp(vex_lsp::Event::DiagnosticCatalog(_)))
        ));
    }

    #[test]
    fn lsp_results_stay_ordered_and_cannot_starve_terminal_input() {
        let events = EventQueue::default();
        for index in 0..128 {
            events.lsp(vex_lsp::Event::Status {
                epoch: index,
                message: String::new(),
                failed: false,
            });
        }
        events.terminal(key(KeyCode::Char('x')));
        assert!(matches!(
            events.next(Duration::ZERO, false),
            Some(AppEvent::Lsp(vex_lsp::Event::Status { epoch: 0, .. }))
        ));
        assert!(matches!(
            events.next(Duration::ZERO, false),
            Some(AppEvent::Terminal(_))
        ));
        for index in 1..128 {
            assert!(
                matches!(events.next(Duration::ZERO, true), Some(AppEvent::Lsp(vex_lsp::Event::Status { epoch, .. })) if epoch == index)
            );
        }
        for _ in 0..128 {
            events.lsp(vex_lsp::Event::Status {
                epoch: 0,
                message: String::new(),
                failed: false,
            });
        }
        let producer = events.clone();
        let thread = thread::spawn(move || {
            producer.lsp(vex_lsp::Event::Status {
                epoch: 0,
                message: String::new(),
                failed: false,
            })
        });
        events.close();
        thread.join().unwrap();
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
        events.background(BackgroundEvent::Syntax(vec![
            syntax.run(syntax_job(&mut editor)).unwrap(),
        ]));
        editor.set_language(Some(vex_editor::Language::Rust));
        events.background(BackgroundEvent::Syntax(vec![
            syntax.run(syntax_job(&mut editor)).unwrap(),
        ]));
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
        assert!(editor.apply_syntax_result(result.into_iter().next().unwrap()));
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
            state
                .run(job)
                .map(|result| BackgroundEvent::Syntax(vec![result]))
        })
        .unwrap();
        syntax.submit(syntax_job(&mut editor));
        let Some(AppEvent::Background(BackgroundEvent::Syntax(result))) =
            events.next(Duration::from_secs(3), true)
        else {
            panic!("syntax must complete independently of search");
        };
        assert!(editor.apply_syntax_result(result.into_iter().next().unwrap()));
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
    fn early_workspace_acceptance_selects_matching_lines_before_queued_edits() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("result.txt"), "needle\nrest\n").unwrap();
        let mut app = App::from_document(Document::from("origin"), (80, 24));
        press(&mut app, " /needle");
        app.handle(key(KeyCode::Enter));
        let mut job = app
            .take_workspace_search_job(Instant::now() + Duration::from_secs(1))
            .unwrap();
        job.root = root.path().into();
        let events = EventQueue::default();
        for code in [
            KeyCode::Char('d'),
            KeyCode::Char('i'),
            KeyCode::Char('X'),
            KeyCode::Esc,
        ] {
            events.terminal(key(code));
        }
        assert!(events.next(Duration::ZERO, app.input_waiting()).is_none());
        let mut worker = crate::picker::search::Worker::default();
        worker.run(job, |result| {
            events.background(BackgroundEvent::WorkspaceSearch(result))
        });
        while let Some(event) = events.next(Duration::ZERO, app.input_waiting()) {
            deliver(&mut app, event);
        }
        assert_eq!(app.editor.document().text().to_string(), "Xrest\n");
        assert!(!app.input_waiting());
    }

    #[test]
    fn early_prompt_tab_waits_before_queued_enter_and_document_edits() {
        let mut app = App::from_document(Document::from("source"), (80, 24));
        press(&mut app, ":language rus");
        app.handle(key(KeyCode::Tab));
        let job = app.take_prompt_completion_job().unwrap();
        let events = EventQueue::default();
        for code in [
            KeyCode::Enter,
            KeyCode::Char('i'),
            KeyCode::Char('X'),
            KeyCode::Esc,
        ] {
            events.terminal(key(code));
        }
        assert!(app.input_waiting());
        assert!(events.next(Duration::ZERO, app.input_waiting()).is_none());
        events.background(BackgroundEvent::Prompt(job.run().unwrap()));
        while let Some(event) = events.next(Duration::ZERO, app.input_waiting()) {
            deliver(&mut app, event);
        }
        assert_eq!(app.editor.language(), Some(vex_editor::Language::Rust));
        assert_eq!(app.editor.document().text().to_string(), "Xsource");
        assert!(!app.input_waiting());
    }

    #[test]
    fn surround_replacement_defers_each_input_until_its_worker_stage_completes() {
        let mut app = App::from_document(Document::from("(abc)"), (50, 12));
        app.editor.set_background_search(true);
        press(&mut app, "lmr(");
        let prepare = app.editor.take_search_job().unwrap();
        let events = EventQueue::default();
        events.terminal(key(KeyCode::Char(']')));
        events.terminal(key(KeyCode::Char('d')));
        assert!(events.next(Duration::ZERO, app.input_waiting()).is_none());
        events.background(BackgroundEvent::Search(prepare.run().unwrap()));
        deliver(&mut app, events.next(Duration::ZERO, true).unwrap());
        deliver(&mut app, events.next(Duration::ZERO, false).unwrap());
        assert!(app.input_waiting());
        assert_eq!(app.editor.document().text(), "(abc)");
        assert!(events.next(Duration::ZERO, true).is_none());
        let edit = app.editor.take_search_job().unwrap();
        events.background(BackgroundEvent::Search(edit.run().unwrap()));
        while let Some(event) = events.next(Duration::ZERO, app.input_waiting()) {
            deliver(&mut app, event);
        }
        assert_eq!(app.editor.document().text(), "[bc]");
        assert_eq!(app.editor.document().undo_depth(), 2);
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
    fn copied_selections_finish_before_queued_edits_and_can_be_cancelled() {
        let mut app = App::from_document(Document::from("abc\ndef\nghi"), (40, 8));
        app.editor.set_background_search(true);
        press(&mut app, "2C");
        let result = app.editor.take_search_job().unwrap().run().unwrap();
        let events = EventQueue::default();
        events.terminal(key(KeyCode::Char('d')));
        events.terminal(Event::Resize(60, 10));
        let event = events.next(Duration::ZERO, app.input_waiting()).unwrap();
        deliver(&mut app, event);
        assert_eq!(app.size(), (60, 10));
        assert!(events.next(Duration::ZERO, app.input_waiting()).is_none());
        assert_eq!(app.editor.document().text(), "abc\ndef\nghi");
        events.background(BackgroundEvent::Search(result));
        while let Some(event) = events.next(Duration::ZERO, app.input_waiting()) {
            deliver(&mut app, event);
        }
        assert_eq!(app.editor.document().text(), "bc\nef\nhi");
        app.editor.execute("undo", 1).unwrap();
        let original = app.editor.selections().clone();
        press(&mut app, "C");
        let result = app.editor.take_search_job().unwrap().run().unwrap();
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
        assert!(!app.input_waiting());
        assert!(!app.handle_search_result(result));
        assert_eq!(app.editor.selections(), &original);
        assert_eq!(app.editor.document().text(), "abc\ndef\nghi");
    }

    #[test]
    fn regex_selection_and_star_finish_before_queued_edits() {
        for keys in ["%s[a-z]+", "%S +"] {
            let mut app = App::from_document(Document::from("one two three"), (40, 8));
            app.editor.set_background_search(true);
            press(&mut app, keys);
            let result = app.editor.take_search_job().unwrap().run().unwrap();
            app.handle(key(KeyCode::Enter));
            let events = EventQueue::default();
            events.terminal(key(KeyCode::Char('d')));
            assert!(events.next(Duration::ZERO, app.input_waiting()).is_none());
            events.background(BackgroundEvent::Search(result));
            while let Some(event) = events.next(Duration::ZERO, app.input_waiting()) {
                deliver(&mut app, event);
            }
            assert_eq!(app.editor.document().text(), "  ");
        }
        let mut app = App::from_document(Document::from("a a"), (40, 8));
        app.editor.set_background_search(true);
        press(&mut app, "*");
        assert!(app.input_waiting());
        let result = app.editor.take_search_job().unwrap().run().unwrap();
        let events = EventQueue::default();
        events.terminal(key(KeyCode::Char('n')));
        events.terminal(key(KeyCode::Char('d')));
        assert!(events.next(Duration::ZERO, true).is_none());
        events.background(BackgroundEvent::Search(result));
        while let Some(event) = events.next(Duration::ZERO, app.input_waiting()) {
            deliver(&mut app, event);
        }
        assert_eq!(app.editor.document().text(), "a a");
        let result = app.editor.take_search_job().unwrap().run().unwrap();
        events.background(BackgroundEvent::Search(result));
        while let Some(event) = events.next(Duration::ZERO, app.input_waiting()) {
            deliver(&mut app, event);
        }
        assert_eq!(app.editor.document().text(), "a ");
    }

    #[test]
    fn picker_enter_holds_following_edits_until_the_selected_file_opens() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".git")).unwrap();
        let origin = directory.path().join("origin.txt");
        std::fs::write(&origin, "origin").unwrap();
        std::fs::write(directory.path().join("destination.txt"), "target").unwrap();
        let mut app = App::open(Some(&origin), (60, 12)).unwrap();
        press(&mut app, " fdestination");
        app.handle(key(KeyCode::Enter));
        assert!(app.input_waiting());
        let events = EventQueue::default();
        for ch in "iX".chars() {
            events.terminal(key(KeyCode::Char(ch)));
        }
        events.terminal(Event::Resize(80, 16));
        let event = events.next(Duration::ZERO, app.input_waiting()).unwrap();
        deliver(&mut app, event);
        assert_eq!(app.size(), (80, 16));
        assert!(events.next(Duration::ZERO, app.input_waiting()).is_none());
        FileWorker::default().run(app.take_picker_job().unwrap(), |result| {
            events.background(BackgroundEvent::Files(result))
        });
        while let Some(event) = events.next(Duration::ZERO, app.input_waiting()) {
            deliver(&mut app, event);
        }
        assert_eq!(app.editor.document().text(), "Xtarget");
        assert_eq!(std::fs::read_to_string(origin).unwrap(), "origin");
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
