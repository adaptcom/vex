//! Bounded per-pane navigation history, independent of language services.
//! Checkpoints retain selections and document identities, never document text.

use super::App;
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use vex_core::{
    Bookmark, CharOffset, Document, DocumentId, PositionResolver, Selection, SelectionSet,
};
use vex_editor::{Mode, background::Cancellation};

const CAPACITY: usize = 32;

#[derive(Clone, Debug)]
pub(super) struct Jump {
    pub identity: u64,
    pub document: DocumentId,
    pub selections: Arc<SelectionSet>,
    pub bookmark: Bookmark,
}

impl Jump {
    pub fn new(document: &Document, selections: &SelectionSet) -> Self {
        Self::shared(document, Arc::new(selections.clone()))
    }

    fn shared(document: &Document, selections: Arc<SelectionSet>) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            identity: NEXT.fetch_add(1, Ordering::Relaxed),
            document: document.id(),
            bookmark: document.bookmark(),
            selections,
        }
    }

    fn matches(&self, other: &Self) -> bool {
        self.document == other.document
            && self.bookmark.revision() == other.bookmark.revision()
            && self.selections == other.selections
    }
}

#[derive(Clone, Debug)]
pub(super) struct History {
    entries: VecDeque<Jump>,
    // entries.len() means the live view is newer than the recorded history.
    cursor: usize,
}

impl History {
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Jump> {
        self.entries.iter()
    }

    pub fn new(initial: Jump) -> Self {
        Self {
            entries: VecDeque::from([initial]),
            cursor: 0,
        }
    }

    fn push(&mut self, jump: Jump) -> usize {
        self.entries.truncate(self.cursor);
        let mut removed = 0;
        if !self.entries.back().is_some_and(|last| last.matches(&jump)) {
            if self.entries.len() == CAPACITY {
                self.entries.pop_front();
                removed = 1;
            }
            self.entries.push_back(jump);
        }
        self.cursor = self.entries.len();
        removed
    }

    pub fn remove(&mut self, document: DocumentId) {
        let before = self
            .entries
            .iter()
            .take(self.cursor)
            .filter(|jump| jump.document == document)
            .count();
        self.entries.retain(|jump| jump.document != document);
        self.cursor = self.cursor.saturating_sub(before).min(self.entries.len());
    }
}

#[derive(Default)]
pub(super) struct State {
    background: bool,
    pending: Option<Pending>,
    job: Option<Job>,
}

struct Pending {
    origin: Jump,
    window: u64,
    mode: Mode,
    cancellation: Cancellation,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

pub(crate) struct Job {
    history: History,
    origin: Jump,
    forward: bool,
    count: usize,
    resolvers: BTreeMap<DocumentId, PositionResolver>,
    pub cancellation: Cancellation,
}

pub(crate) struct Result {
    history: History,
    destination: io::Result<Option<Jump>>,
    cancellation: Cancellation,
}

impl Job {
    pub fn run(mut self) -> Option<Result> {
        let cancelled = || self.cancellation.is_cancelled();
        let resolve = |jump: &mut Jump| -> io::Result<()> {
            let resolver = self
                .resolvers
                .get(&jump.document)
                .ok_or_else(|| io::Error::other("jump buffer is no longer open"))?;
            if resolver.bookmark().revision() != jump.bookmark.revision()
                && let Some(mapped) = resolver
                    .resolve(&jump.bookmark, &jump.selections, cancelled)
                    .map_err(io::Error::other)?
            {
                jump.selections = Arc::new(mapped);
                jump.bookmark = resolver.bookmark().clone();
            }
            Ok(())
        };
        let destination = (|| {
            let target = if self.forward {
                self.history
                    .cursor
                    .checked_add(self.count)
                    .filter(|&target| target < self.history.entries.len())
            } else {
                self.history.cursor.checked_sub(self.count)
            };
            let Some(mut target) = target else {
                return Ok(None);
            };
            if !self.forward && self.history.cursor == self.history.entries.len() {
                target = target.saturating_sub(self.history.push(self.origin.clone()));
            }
            let Some(jump) = self.history.entries.get_mut(target) else {
                return Ok(None);
            };
            resolve(jump)?;
            if !self.forward && jump.matches(&self.origin) {
                let Some(previous) = target.checked_sub(1) else {
                    return Ok(None);
                };
                target = previous;
                resolve(&mut self.history.entries[target])?;
            }
            self.history.cursor = target;
            Ok(Some(self.history.entries[target].clone()))
        })();
        (!cancelled()).then_some(Result {
            history: self.history,
            destination,
            cancellation: self.cancellation,
        })
    }
}

impl App {
    pub(super) fn current_jump(&self) -> Jump {
        Jump::new(self.editor.document(), self.editor.selections())
    }

    pub(super) fn push_jump(&mut self, jump: Jump) {
        self.jump_history_mut().push(jump);
    }

    pub(super) fn record_jump(&mut self) {
        self.push_jump(self.current_jump());
    }

    pub(super) fn record_jump_at(&mut self, selections: Arc<SelectionSet>) {
        self.push_jump(Jump::shared(self.editor.document(), selections));
    }

    pub(super) fn navigate_jump(&mut self, forward: bool, count: usize) -> io::Result<()> {
        self.cancel_jump_navigation();
        let history = self.jump_history().clone();
        let origin = self.current_jump();
        let mut resolvers = BTreeMap::new();
        for jump in history.iter() {
            if let std::collections::btree_map::Entry::Vacant(entry) =
                resolvers.entry(jump.document)
                && let Some(resolver) = self.resolver_for_buffer(jump.document)
            {
                entry.insert(resolver);
            }
        }
        let cancellation = Cancellation::default();
        self.jump_navigation.pending = Some(Pending {
            origin: origin.clone(),
            window: self.focused_window_id(),
            mode: self.editor.mode(),
            cancellation: cancellation.clone(),
        });
        let job = Job {
            history,
            origin,
            resolvers,
            forward,
            count,
            cancellation,
        };
        // Unchanged checkpoints need no scan. Keep ordinary navigation inline
        // and wake the worker only when a retained revision needs remapping.
        let background = self.jump_navigation.background
            && job.history.iter().any(|jump| {
                job.resolvers.get(&jump.document).is_some_and(|resolver| {
                    resolver.bookmark().revision() != jump.bookmark.revision()
                })
            });
        if background {
            self.jump_navigation.job = Some(job);
            self.message = "locating jump…".into();
            Ok(())
        } else {
            self.apply_jump_navigation(job.run().expect("synchronous navigation"))
        }
    }

    pub(crate) fn enable_jump_navigation(&mut self) {
        self.jump_navigation.background = true;
    }
    pub(crate) fn take_jump_navigation(&mut self) -> Option<Job> {
        self.jump_navigation.job.take()
    }
    pub(in crate::app) fn jump_navigation_waiting(&self) -> bool {
        self.jump_navigation.pending.is_some()
    }
    pub(in crate::app) fn cancel_jump_navigation(&mut self) {
        self.jump_navigation.pending = None;
        self.jump_navigation.job = None;
    }
    pub(crate) fn handle_jump_navigation(&mut self, result: Result) -> bool {
        if let Err(error) = self.apply_jump_navigation(result) {
            self.fail(error);
        }
        true
    }
    fn apply_jump_navigation(&mut self, result: Result) -> io::Result<()> {
        let Some(pending) = &self.jump_navigation.pending else {
            return Ok(());
        };
        if !pending.cancellation.same_request(&result.cancellation)
            || result.cancellation.is_cancelled()
        {
            return Ok(());
        }
        let pending = self.jump_navigation.pending.take().unwrap();
        self.jump_navigation.job = None;
        if pending.window != self.focused_window_id()
            || pending.mode != self.editor.mode()
            || pending.origin.document != self.editor.document().id()
            || pending.origin.bookmark.revision() != self.editor.document().revision()
            || pending.origin.selections.as_ref() != self.editor.selections()
        {
            self.clear_message();
            return Ok(());
        }
        let Some(jump) = result.destination? else {
            self.clear_message();
            return Ok(());
        };
        if self
            .snapshot_for_buffer(jump.document)
            .is_none_or(|snapshot| snapshot.revision() != jump.bookmark.revision())
        {
            return Err(io::Error::other(
                "jump buffer changed while locating the destination",
            ));
        }
        self.restore_jump(jump.document, &jump.selections)?;
        *self.jump_history_mut() = result.history;
        self.clear_message();
        Ok(())
    }

    pub(super) fn restore_jump(
        &mut self,
        document: DocumentId,
        saved: &SelectionSet,
    ) -> io::Result<()> {
        let mode = self.editor.mode();
        self.open_buffer(document)?;
        self.editor
            .execute("normal_mode", 1)
            .map_err(io::Error::other)?;
        if mode == Mode::Select {
            self.editor
                .execute("select_mode", 1)
                .map_err(io::Error::other)?;
        }
        let length = self.editor.document().text().len_chars();
        let selections = if saved
            .ranges()
            .iter()
            .all(|selection| selection.end().0 <= length)
        {
            saved.clone()
        } else {
            SelectionSet::new(
                saved
                    .ranges()
                    .iter()
                    .map(|selection| {
                        Selection::new(
                            CharOffset(selection.anchor.0.min(length)),
                            CharOffset(selection.head.0.min(length)),
                        )
                    })
                    .collect(),
                saved.primary_index(),
            )
            .map_err(io::Error::other)?
        };
        self.editor
            .set_selections(selections)
            .map_err(io::Error::other)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use vex_core::Document;

    fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
        app.handle(Event::Key(KeyEvent::new(code, modifiers)));
    }
    fn at(app: &mut App, position: usize) {
        app.editor
            .set_selections(SelectionSet::single(Selection::new(
                CharOffset(position),
                CharOffset(position + 1),
            )))
            .unwrap();
    }
    fn cursor(app: &App) -> usize {
        app.editor.selections().primary().start().0
    }

    #[test]
    fn saved_ranges_follow_edits_grouped_undo_and_redo_with_direction_and_primary_intact() {
        let mut app = App::from_document(Document::from("abcdefghijklmnop"), (80, 24));
        let saved = SelectionSet::new(
            vec![
                Selection::new(CharOffset(1), CharOffset(3)),
                Selection::new(CharOffset(8), CharOffset(6)),
            ],
            1,
        )
        .unwrap();
        app.editor.set_selections(saved.clone()).unwrap();
        app.record_jump();
        at(&mut app, 0);
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("X").unwrap();
        app.editor.insert_text("Y").unwrap();
        app.editor.execute("normal_mode", 1).unwrap();
        at(&mut app, 14);
        app.editor.execute("select_mode", 1).unwrap();
        app.navigate_jump(false, 1).unwrap();
        assert_eq!(
            app.editor.selections(),
            &SelectionSet::new(
                vec![
                    Selection::new(CharOffset(3), CharOffset(5)),
                    Selection::new(CharOffset(10), CharOffset(8)),
                ],
                1
            )
            .unwrap()
        );
        assert_eq!(app.editor.mode(), Mode::Select);
        app.editor.execute("undo", 1).unwrap();
        app.navigate_jump(true, 1).unwrap();
        assert_eq!(cursor(&app), 12);
        app.navigate_jump(false, 1).unwrap();
        assert_eq!(app.editor.selections(), &saved);
        app.editor.execute("redo", 1).unwrap();
        app.navigate_jump(true, 1).unwrap();
        assert_eq!(cursor(&app), 14);
    }

    #[test]
    fn background_remapping_waits_cancels_and_rejects_stale_edits_without_advancing_history() {
        let mut app = App::from_document(Document::from("abcdefghij"), (80, 24));
        at(&mut app, 4);
        app.record_jump();
        at(&mut app, 0);
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("X").unwrap();
        app.editor.execute("normal_mode", 1).unwrap();
        let position = app.jump_history().cursor;
        let origin = app.editor.selections().clone();
        app.enable_jump_navigation();
        app.navigate_jump(false, 1).unwrap();
        assert!(app.input_waiting());
        let cancelled = app.take_jump_navigation().unwrap();
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(cancelled.run().is_none());
        assert!(!app.input_waiting());
        assert_eq!(app.editor.selections(), &origin);
        assert_eq!(app.jump_history().cursor, position);
        app.navigate_jump(false, 1).unwrap();
        let stale = app.take_jump_navigation().unwrap().run().unwrap();
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("Y").unwrap();
        app.editor.execute("normal_mode", 1).unwrap();
        let origin = app.editor.selections().clone();
        app.handle_jump_navigation(stale);
        assert!(!app.input_waiting());
        assert_eq!(app.editor.selections(), &origin);
        assert_eq!(app.jump_history().cursor, position);
        app.navigate_jump(false, 1).unwrap();
        let result = app.take_jump_navigation().unwrap().run().unwrap();
        app.handle_jump_navigation(result);
        assert_eq!(cursor(&app), 6);
        assert!(!app.input_waiting());
        app.navigate_jump(true, 1).unwrap();
        assert!(!app.input_waiting()); // Current revisions need no worker round trip.
        assert_eq!(app.editor.selections(), &origin);
    }

    #[test]
    fn pending_cross_buffer_destination_is_revision_checked_before_switching() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("other.txt");
        std::fs::write(&path, "other buffer").unwrap();
        let mut app = App::from_document(Document::from("abcdefghij"), (120, 24));
        let target = app.editor.document().id();
        at(&mut app, 4);
        app.record_jump();
        at(&mut app, 0);
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("Z").unwrap();
        app.editor.execute("normal_mode", 1).unwrap();
        app.open_window_file(&path).unwrap();
        let origin = app.editor.document().id();
        let position = app.jump_history().cursor;
        app.enable_jump_navigation();
        app.navigate_jump(false, 1).unwrap();
        let result = app.take_jump_navigation().unwrap().run().unwrap();
        app.with_file_buffer_mut(target, |editor, _, _| {
            editor.execute("insert_mode", 1).unwrap();
            editor.insert_text("X").unwrap();
            editor.execute("normal_mode", 1).unwrap();
        })
        .unwrap();
        app.handle_jump_navigation(result);
        assert_eq!(app.editor.document().id(), origin);
        assert_eq!(app.jump_history().cursor, position);
        assert!(!app.input_waiting());
        assert!(app.message.contains("jump buffer changed"));
    }

    #[test]
    fn backward_skips_the_current_checkpoint_only_after_remapping_it() {
        for (current, expected) in [(4, 5), (5, 3)] {
            let mut app = App::from_document(Document::from("abcdefghij"), (80, 24));
            for start in [2, 4] {
                at(&mut app, start);
                app.record_jump();
            }
            at(&mut app, 0);
            app.editor.execute("insert_mode", 1).unwrap();
            app.editor.insert_text("X").unwrap();
            app.editor.execute("normal_mode", 1).unwrap();
            at(&mut app, current);
            app.navigate_jump(false, 1).unwrap();
            assert_eq!(cursor(&app), expected);
        }
    }

    #[test]
    fn last_modification_records_the_origin_only_after_the_worker_succeeds() {
        let mut app = App::from_document(Document::from("abcdefghij"), (80, 24));
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("XY").unwrap();
        app.editor.execute("normal_mode", 1).unwrap();
        at(&mut app, 9);
        app.editor.set_background_search(true);
        for ch in ['g', '.'] {
            press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        assert!(app.input_waiting());
        let result = app.editor.take_search_job().unwrap().run().unwrap();
        assert_eq!(cursor(&app), 9);
        app.handle_search_result(result);
        assert!(!app.input_waiting());
        assert_eq!(cursor(&app), 2);
        app.execute("jump_backward").unwrap();
        assert_eq!(cursor(&app), 9);
        app.execute("jump_forward").unwrap();
        assert_eq!(cursor(&app), 2);
    }

    #[test]
    fn backward_forward_counts_duplicates_and_branching_follow_live_selection() {
        let mut app = App::from_document(Document::from("abcdefghijk"), (80, 24));
        for position in [0, 3, 6] {
            at(&mut app, position);
            app.execute("save_selection").unwrap();
        }
        // The most recent saved selection is also the live position: skip it.
        press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_eq!(cursor(&app), 3);
        press(&mut app, KeyCode::Char('i'), KeyModifiers::CONTROL);
        assert_eq!(cursor(&app), 6);
        app.execute("jump_backward").unwrap();
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(cursor(&app), 6);
        press(&mut app, KeyCode::Char('2'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_eq!(cursor(&app), 0);
        press(&mut app, KeyCode::Char('2'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('i'), KeyModifiers::CONTROL);
        assert_eq!(cursor(&app), 6);
        app.navigate_jump(false, usize::MAX).unwrap();
        app.navigate_jump(true, usize::MAX).unwrap();
        assert_eq!(cursor(&app), 6);
        app.execute("jump_backward").unwrap();
        at(&mut app, 4);
        app.execute("save_selection").unwrap();
        at(&mut app, 9);
        app.execute("jump_forward").unwrap(); // Old forward branch was discarded.
        assert_eq!(cursor(&app), 9);
        app.execute("jump_backward").unwrap();
        assert_eq!(cursor(&app), 4);
        app.execute("jump_forward").unwrap();
        assert_eq!(cursor(&app), 9);
    }

    #[test]
    fn jumps_restore_direction_primary_and_select_mode_across_hidden_scratch_and_files() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("target.txt");
        std::fs::write(&path, "target text").unwrap();
        let mut app = App::from_document(Document::from("abcdefghi"), (80, 24));
        let initial = app.editor.document().id();
        let selections = SelectionSet::new(
            vec![
                Selection::new(CharOffset(3), CharOffset(1)),
                Selection::new(CharOffset(5), CharOffset(8)),
            ],
            1,
        )
        .unwrap();
        app.editor.set_selections(selections.clone()).unwrap();
        app.open_window_from_picker(&path).unwrap();
        let target = app.editor.document().id();
        at(&mut app, 4);
        app.execute("select_mode").unwrap();
        app.execute("jump_backward").unwrap();
        assert_eq!(app.editor.document().id(), initial);
        assert_eq!(app.editor.mode(), Mode::Select);
        assert_eq!(app.editor.selections(), &selections);
        app.execute("jump_forward").unwrap();
        assert_eq!(app.editor.document().id(), target);
        assert_eq!(cursor(&app), 4);
        assert_eq!(app.editor.mode(), Mode::Select);
        // Tab retains its editing meaning in insert mode.
        app.execute("insert_mode").unwrap();
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert!(app.editor.document().text().to_string().contains('\t'));
    }

    #[test]
    fn each_pane_has_its_own_history_and_closed_buffers_leave_no_dangling_entries() {
        let mut app = App::from_document(Document::from("abcdefghij"), (100, 24));
        app.execute("save_selection").unwrap();
        at(&mut app, 3);
        app.execute("vsplit").unwrap();
        app.execute("jump_backward").unwrap();
        assert_eq!(cursor(&app), 3); // New pane cannot consume the other's history.
        app.execute("save_selection").unwrap();
        at(&mut app, 6);
        app.execute("jump_backward").unwrap();
        assert_eq!(cursor(&app), 3);
        app.execute("jump_view_left").unwrap();
        app.execute("jump_backward").unwrap();
        assert_eq!(cursor(&app), 0);
        app.execute("jump_view_right").unwrap();
        app.execute("jump_forward").unwrap();
        assert_eq!(cursor(&app), 6);

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("target.txt");
        std::fs::write(&path, "target").unwrap();
        let original = app.editor.document().id();
        app.open_window_from_picker(&path).unwrap();
        let target = app.editor.document().id();
        app.execute("jump_backward").unwrap();
        app.execute("bc").unwrap();
        assert_eq!(app.editor.document().id(), target);
        assert!(
            app.jump_history()
                .entries
                .iter()
                .all(|jump| jump.document != original)
        );
        app.execute("jump_forward").unwrap();
        assert_eq!(app.editor.document().id(), target);
    }

    #[test]
    fn history_capacity_shares_selection_storage_and_failed_destinations_preserve_history() {
        let mut app = App::from_document(Document::from("x".repeat(100).as_str()), (80, 24));
        for position in 0..80 {
            at(&mut app, position);
            app.record_jump();
        }
        assert_eq!(app.jump_history().entries.len(), CAPACITY);
        at(&mut app, 90);
        app.navigate_jump(false, CAPACITY).unwrap();
        assert_eq!(cursor(&app), 49); // Saving the live return location evicts 48.
        app.navigate_jump(true, CAPACITY - 1).unwrap();
        assert_eq!(cursor(&app), 90);
        let history = app.jump_history().clone();
        assert!(Arc::ptr_eq(
            &history.entries[0].selections,
            &app.jump_history().entries[0].selections
        ));
        let missing = Document::from("missing");
        app.push_jump(Jump::new(
            &missing,
            &SelectionSet::single(Selection::cursor(CharOffset(0))),
        ));
        let position = app.jump_history().cursor;
        let length = app.jump_history().entries.len();
        assert!(app.navigate_jump(false, 1).is_err());
        assert_eq!(app.jump_history().cursor, position);
        assert_eq!(app.jump_history().entries.len(), length);
        assert_eq!(cursor(&app), 90);
    }
}
