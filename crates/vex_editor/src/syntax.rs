//! Revision-checked syntax requests. The UI keeps only snapshots, small edit
//! metadata, completed spans, and an immutable tree clone; parsing and querying
//! belong to workers. Match commands share the tree without reparsing on the UI.

use crate::{Editor, HighlightSpan, Language, background::Cancellation};
use std::{collections::VecDeque, ops::Range, sync::Arc};
use vex_core::{Affinity, ByteOffset, ChangeExtent, CharOffset, Document, Revision, Snapshot};
use vex_syntax::{MAX_HIGHLIGHT_BYTES, ParsedSyntax, Syntax};

const MAX_RANGES: usize = 128;
const MAX_CHANGES: usize = 256;

#[derive(Clone, Debug)]
struct Change {
    before: Revision,
    after: Revision,
    old_len: usize,
    extent: ChangeExtent,
}

#[derive(Debug)]
struct CachedRange {
    range: Range<ByteOffset>,
    spans: Arc<[HighlightSpan]>,
    fresh: bool,
}

impl CachedRange {
    /// Keep display colors aligned with surviving text while parsing catches up.
    /// Only positions inside the conservative extent need the exact edit maps;
    /// the unchanged prefix/suffix use byte offsets without scanning the buffer.
    fn remap(&mut self, previous: &Snapshot, document: &Document, extent: ChangeExtent) {
        let start = previous.text().char_to_byte(extent.start.0);
        let old_end = previous.text().char_to_byte(extent.old_end.0);
        let new_end = document.text().char_to_byte(extent.new_end.0);
        let map = |byte: ByteOffset, affinity| {
            if byte.0 < start {
                byte
            } else if byte.0 > old_end {
                ByteOffset(new_end + (byte.0 - old_end))
            } else {
                let position = document
                    .map_position_since(
                        previous,
                        CharOffset(previous.text().byte_to_char(byte.0)),
                        affinity,
                    )
                    .expect("consecutive syntax snapshots and valid cached offsets");
                ByteOffset(document.text().char_to_byte(position.0))
            }
        };
        // Coverage includes boundary insertions so a growing line still hits its
        // cached range. Tokens exclude them; only typing inside a token inherits
        // its color. Fully replaced tokens disappear until the worker recolors.
        self.range = map(self.range.start, Affinity::Before)..map(self.range.end, Affinity::After);
        if self
            .spans
            .last()
            .is_some_and(|span| span.range.end.0 > start)
        {
            self.spans = self
                .spans
                .iter()
                .filter_map(|span| {
                    let range = map(span.range.start, Affinity::After)
                        ..map(span.range.end, Affinity::Before);
                    (range.start < range.end).then_some(HighlightSpan {
                        range,
                        highlight: span.highlight,
                    })
                })
                .collect();
        }
        self.fresh = false;
    }
}

#[derive(Debug)]
struct Pending {
    cancellation: Cancellation,
    ranges: Vec<Range<ByteOffset>>,
}

#[derive(Debug, Default)]
pub(crate) struct Highlighting {
    language: Option<Language>,
    background: bool,
    synchronous: Option<Syntax>,
    parsed: Option<ParsedSyntax>,
    snapshot: Option<Snapshot>,
    session: Cancellation,
    changes: VecDeque<Change>,
    requested: Vec<Range<ByteOffset>>,
    cache: VecDeque<CachedRange>,
    pending: Option<Pending>,
}

impl Highlighting {
    fn cancel(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.cancellation.cancel();
        }
    }

    fn reset(&mut self, document: &Document, language: Option<Language>, background: bool) {
        self.cancel();
        *self = Self {
            language,
            background,
            snapshot: language.map(|_| document.snapshot()),
            synchronous: None,
            parsed: None,
            session: Cancellation::default(),
            changes: VecDeque::new(),
            requested: Vec::new(),
            cache: VecDeque::new(),
            pending: None,
        };
    }

    pub(crate) fn synchronize(&mut self, document: &Document) {
        let Some(previous) = &self.snapshot else {
            return;
        };
        if previous.id() == document.id() && previous.revision() == document.revision() {
            return;
        }
        if let Some(extent) = document.change_since(previous) {
            if self.changes.len() == MAX_CHANGES {
                self.changes.pop_front();
            }
            self.changes.push_back(Change {
                before: previous.revision(),
                after: document.revision(),
                old_len: previous.text().len_chars(),
                extent,
            });
            if document.text().len_bytes() <= MAX_HIGHLIGHT_BYTES {
                for entry in &mut self.cache {
                    entry.remap(previous, document, extent);
                }
                self.cache.retain(|entry| !entry.range.is_empty());
            } else {
                self.cache.clear();
            }
        } else {
            self.changes.clear();
            self.cache.clear();
        }
        self.cancel();
        self.requested.clear();
        self.parsed = None;
        self.snapshot = Some(document.snapshot());
        if let Some(syntax) = &mut self.synchronous {
            syntax.synchronize(document);
        }
    }
}

impl Drop for Highlighting {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl Editor {
    /// Select a language lazily. Grammar initialization happens with the first
    /// query on the syntax worker (or at first draw for synchronous integrations).
    /// Selecting the same language again also invalidates pending results.
    pub fn set_language(&mut self, language: Option<Language>) {
        self.cancel_surround();
        self.search.invalidate_syntax();
        if self.language() != language {
            self.indentation_state.overridden = false;
            self.apply_indentation(self.indentation_state.resolve(language));
        }
        let syntax = self.syntax.get_mut();
        syntax.reset(&self.document, language, syntax.background);
    }

    pub fn language(&self) -> Option<Language> {
        self.syntax.borrow().language
    }

    /// Reuse only a parse for the current document revision and language.
    pub(crate) fn parsed_syntax(&self) -> Option<ParsedSyntax> {
        let state = self.syntax.borrow();
        state
            .parsed
            .clone()
            .or_else(|| state.synchronous.as_ref()?.parsed_if_ready())
            .filter(|parsed| {
                parsed.snapshot().id() == self.document.id()
                    && parsed.snapshot().revision() == self.document.revision()
                    && Some(parsed.language()) == state.language
            })
    }

    pub(crate) fn cache_parsed_syntax(&mut self, parsed: ParsedSyntax) {
        if parsed.snapshot().id() == self.document.id()
            && parsed.snapshot().revision() == self.document.revision()
            && Some(parsed.language()) == self.language()
        {
            self.syntax.get_mut().parsed = Some(parsed);
        }
    }

    /// Enable asynchronous syntax. Frontends call begin_syntax_frame before
    /// drawing, take_syntax_job afterward, and apply_syntax_result on completion.
    /// Standalone integrations remain synchronous by default.
    pub fn set_background_syntax(&mut self, enabled: bool) {
        let syntax = self.syntax.get_mut();
        syntax.reset(&self.document, syntax.language, enabled);
    }

    /// Begin collecting visible ranges, including frames with no visible text.
    pub fn begin_syntax_frame(&self) {
        self.syntax.borrow_mut().requested.clear();
    }

    /// Cancel a request before replacing a multi-document worker batch. Cached
    /// ranges stay available and missing ranges can immediately be reissued.
    pub fn cancel_syntax_request(&mut self) {
        self.syntax.get_mut().cancel();
    }

    /// Release structural caches when a buffer stops being visible. Cached
    /// viewport colors remain available; the next structural job may parse again.
    pub fn release_syntax_tree(&mut self) {
        let state = self.syntax.get_mut();
        state.parsed = None;
        state.synchronous = None;
    }

    /// Return cached colors while requesting background work. After an edit,
    /// previous colors follow surviving text until a current result replaces them.
    /// Text without cached colors stays plain until its first result arrives.
    /// At most 128 distinct ranges per frame are highlighted in background mode.
    pub fn syntax_highlights(&self, range: Range<ByteOffset>) -> Arc<[HighlightSpan]> {
        let mut state = self.syntax.borrow_mut();
        let Some(language) = state.language else {
            return Arc::from([]);
        };
        if !state.background {
            return state
                .synchronous
                .get_or_insert_with(|| Syntax::new(language, &self.document))
                .highlights(&self.document, range);
        }
        if range.start >= range.end
            || range.end.0 > self.document.text().len_bytes()
            || self.document.text().len_bytes() > MAX_HIGHLIGHT_BYTES
        {
            return Arc::from([]);
        }
        if !state.requested.contains(&range) {
            if state.requested.len() == MAX_RANGES {
                return Arc::from([]);
            }
            state.requested.push(range.clone());
        }
        if let Some(index) = state.cache.iter().position(|entry| entry.range == range) {
            let entry = state.cache.remove(index).unwrap();
            let spans = Arc::clone(&entry.spans);
            state.cache.push_back(entry);
            spans
        } else {
            // Newlines, joined lines, horizontal clipping, and split views can
            // change query boundaries before parsing finishes. Reuse overlapping
            // colors too, clipped and merged into sorted, disjoint spans.
            let mut spans: Vec<HighlightSpan> = Vec::new();
            for entry in &state.cache {
                if entry.range.end <= range.start || entry.range.start >= range.end {
                    continue;
                }
                let start = entry
                    .spans
                    .partition_point(|span| span.range.end <= range.start);
                for span in entry.spans[start..]
                    .iter()
                    .take_while(|span| span.range.start < range.end)
                {
                    spans.push(HighlightSpan {
                        range: span.range.start.max(range.start)..span.range.end.min(range.end),
                        highlight: span.highlight,
                    });
                }
            }
            spans.sort_unstable_by_key(|span| (span.range.start, span.range.end));
            let mut merged: Vec<HighlightSpan> = Vec::with_capacity(spans.len());
            for mut span in spans {
                if let Some(previous) = merged.last_mut() {
                    if previous.highlight == span.highlight
                        && previous.range.end >= span.range.start
                    {
                        previous.range.end = previous.range.end.max(span.range.end);
                        continue;
                    }
                    span.range.start = span.range.start.max(previous.range.end);
                }
                if !span.range.is_empty() {
                    merged.push(span);
                }
            }
            merged.into()
        }
    }

    /// Coalesce all missing visible ranges into one immutable worker request.
    /// Repeated draws of an unchanged viewport do not restart pending work.
    pub fn take_syntax_job(&mut self) -> Option<SyntaxJob> {
        let state = self.syntax.get_mut();
        let language = state.language?;
        if !state.background {
            return None;
        }
        let ranges: Vec<_> = state
            .requested
            .iter()
            .filter(|range| {
                !state
                    .cache
                    .iter()
                    .any(|entry| entry.fresh && entry.range == **range)
            })
            .cloned()
            .collect();
        if ranges.is_empty() {
            state.cancel();
            return None;
        }
        if state
            .pending
            .as_ref()
            .is_some_and(|pending| ranges.iter().all(|range| pending.ranges.contains(range)))
        {
            return None;
        }
        state.cancel();
        let cancellation = Cancellation::default();
        state.pending = Some(Pending {
            cancellation: cancellation.clone(),
            ranges: ranges.clone(),
        });
        Some(SyntaxJob {
            snapshot: self.document.snapshot(),
            language,
            session: state.session.clone(),
            cancellation,
            ranges,
            changes: state.changes.iter().cloned().collect(),
        })
    }

    /// Accept only the active request for this document revision and language
    /// session. Empty completions are cached too, preventing timeout retry loops.
    /// Returns whether a redraw is needed.
    pub fn apply_syntax_result(&mut self, result: SyntaxResult) -> bool {
        let state = self.syntax.get_mut();
        if result.cancellation.is_cancelled()
            || result.document != self.document.id()
            || result.revision != self.document.revision()
            || !result.session.same_request(&state.session)
            || !state
                .pending
                .as_ref()
                .is_some_and(|pending| pending.cancellation.same_request(&result.cancellation))
        {
            return false;
        }
        state.pending = None;
        state.parsed = Some(result.parsed);
        // The batch covers the current viewport. Discard provisional colors,
        // including when the worker reports plain text after a budget limit.
        state.cache.retain(|entry| entry.fresh);
        for entry in result.ranges {
            if state.cache.len() == MAX_RANGES {
                state.cache.pop_front();
            }
            state.cache.push_back(entry);
        }
        true
    }
}

/// Sendable syntax work containing a cheap rope snapshot and bounded metadata.
#[derive(Debug)]
pub struct SyntaxJob {
    snapshot: Snapshot,
    language: Language,
    session: Cancellation,
    cancellation: Cancellation,
    ranges: Vec<Range<ByteOffset>>,
    changes: Vec<Change>,
}

impl SyntaxJob {
    pub fn document_id(&self) -> vex_core::DocumentId {
        self.snapshot.id()
    }
    pub fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }

    /// Compose conservative edits since the worker's snapshot. Keeping only
    /// coordinates lets queued work coalesce without retaining old rope versions.
    fn change_since(&self, previous: &Snapshot) -> Option<ChangeExtent> {
        if previous.id() != self.snapshot.id() {
            return None;
        }
        let first = self
            .changes
            .iter()
            .position(|change| change.before == previous.revision())?;
        let mut revision = previous.revision();
        let mut prefix = previous.text().len_chars();
        let mut suffix = previous.text().len_chars();
        for change in &self.changes[first..] {
            if change.before != revision {
                return None;
            }
            revision = change.after;
            prefix = prefix.min(change.extent.start.0);
            suffix = suffix.min(change.old_len - change.extent.old_end.0);
        }
        (revision == self.snapshot.revision()).then(|| ChangeExtent {
            start: CharOffset(prefix),
            old_end: CharOffset(previous.text().len_chars() - suffix),
            new_end: CharOffset(self.snapshot.text().len_chars() - suffix),
        })
    }
}

/// Completed spans, applicable only to the job's original revision and session.
#[derive(Debug)]
pub struct SyntaxResult {
    document: vex_core::DocumentId,
    revision: Revision,
    session: Cancellation,
    cancellation: Cancellation,
    ranges: Vec<CachedRange>,
    parsed: ParsedSyntax,
}

impl SyntaxResult {
    pub fn document_id(&self) -> vex_core::DocumentId {
        self.document
    }
}

/// Persistent, runtime-independent worker state. Construct and run on a worker
/// thread; mutable parse state, query cursor, and grammar initialization stay
/// there. Results may share an immutable tree clone with selection workers.
#[derive(Default)]
pub struct SyntaxWorker {
    syntax: Option<Syntax>,
    session: Option<Cancellation>,
}

impl SyntaxWorker {
    pub fn run(&mut self, job: SyntaxJob) -> Option<SyntaxResult> {
        self.run_cancellable(job, || false)
    }

    /// Also observe the owning batch's cancellation, including during parsing.
    pub fn run_cancellable(
        &mut self,
        job: SyntaxJob,
        abort: impl Fn() -> bool,
    ) -> Option<SyntaxResult> {
        let cancelled = || job.cancellation.is_cancelled() || abort();
        if cancelled() {
            return None;
        }
        if !self
            .session
            .as_ref()
            .is_some_and(|session| session.same_request(&job.session))
        {
            self.syntax = Some(Syntax::from_snapshot(job.language, job.snapshot.clone()));
            self.session = Some(job.session.clone());
        }
        if cancelled() {
            return None;
        }
        let syntax = self.syntax.as_mut().unwrap();
        let change = job.change_since(syntax.snapshot());
        syntax.synchronize_snapshot(job.snapshot.clone(), change);
        let mut ranges = Vec::with_capacity(job.ranges.len());
        for range in job.ranges {
            if cancelled() {
                return None;
            }
            let spans = syntax.highlights_current(range.clone(), cancelled);
            ranges.push(CachedRange {
                range,
                spans,
                fresh: true,
            });
        }
        if cancelled() {
            return None;
        }
        let parsed = syntax.parsed(&cancelled);
        if cancelled() {
            return None;
        }
        Some(SyntaxResult {
            document: job.snapshot.id(),
            revision: job.snapshot.revision(),
            session: job.session,
            cancellation: job.cancellation,
            ranges,
            parsed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use vex_core::Edit;

    fn editor(source: &str) -> Editor {
        let mut editor = Editor::new(Document::from(source));
        editor.set_language(Some(Language::Rust));
        editor.set_background_syntax(true);
        editor
    }

    fn request(editor: &mut Editor) -> SyntaxJob {
        editor.begin_syntax_frame();
        editor.syntax_highlights(ByteOffset(0)..ByteOffset(editor.document.text().len_bytes()));
        editor.take_syntax_job().unwrap()
    }

    fn finish(editor: &mut Editor, worker: &mut SyntaxWorker) -> Arc<[HighlightSpan]> {
        let result = worker.run(request(editor)).unwrap();
        assert!(editor.apply_syntax_result(result));
        editor.begin_syntax_frame();
        let spans =
            editor.syntax_highlights(ByteOffset(0)..ByteOffset(editor.document.text().len_bytes()));
        assert!(editor.take_syntax_job().is_none());
        spans
    }

    #[test]
    fn drawing_is_deferred_and_pending_and_completed_ranges_are_reused() {
        let mut editor = editor("fn main() {}\n");
        let mut worker = SyntaxWorker::default();
        let job = request(&mut editor);
        assert!(editor.syntax.borrow().synchronous.is_none());
        assert!(
            editor
                .syntax_highlights(ByteOffset(0)..ByteOffset(13))
                .is_empty()
        );
        assert!(editor.take_syntax_job().is_none());
        assert!(!job.cancellation().is_cancelled());
        assert!(editor.apply_syntax_result(worker.run(job).unwrap()));
        let spans = editor.syntax_highlights(ByteOffset(0)..ByteOffset(13));
        assert!(!spans.is_empty());
        assert!(Arc::ptr_eq(
            &spans,
            &editor.syntax_highlights(ByteOffset(0)..ByteOffset(13))
        ));
        assert!(editor.take_syntax_job().is_none());
        assert!(editor.syntax.borrow().synchronous.is_none());
    }

    #[test]
    fn edits_retain_colors_but_still_request_and_apply_current_syntax() {
        let mut editor = editor("fn main() {}\nfn other() {}\n");
        let mut worker = SyntaxWorker::default();
        let before = finish(&mut editor, &mut worker);
        editor.execute("insert_mode", 1).unwrap();
        editor.insert_text("//界 ").unwrap();
        assert!(editor.parsed_syntax().is_none());
        let range = ByteOffset(0)..ByteOffset(editor.document.text().len_bytes());
        let retained = editor.syntax_highlights(range.clone());
        let shifted: Vec<_> = before
            .iter()
            .map(|span| HighlightSpan {
                range: ByteOffset(span.range.start.0 + 6)..ByteOffset(span.range.end.0 + 6),
                highlight: span.highlight,
            })
            .collect();
        assert_eq!(&*retained, shifted);
        let job = editor
            .take_syntax_job()
            .expect("retained colors are provisional");
        editor.begin_syntax_frame();
        assert!(Arc::ptr_eq(
            &retained,
            &editor.syntax_highlights(range.clone())
        ));
        assert!(editor.take_syntax_job().is_none());
        assert!(!job.cancellation().is_cancelled());
        assert!(editor.apply_syntax_result(worker.run(job).unwrap()));
        let current = editor.syntax_highlights(range.clone());
        assert_ne!(retained, current);
        assert!(
            current.iter().any(|span| span.range.start == ByteOffset(0)
                && span.highlight == crate::Highlight::Comment)
        );
        assert_eq!(
            current,
            Syntax::new(Language::Rust, &editor.document).highlights(&editor.document, range)
        );
        assert!(editor.take_syntax_job().is_none());
    }

    #[test]
    fn disjoint_edits_and_grouped_undo_keep_colors_between_carets() {
        let mut editor = editor("fn one() {}\nfn two() {}\nfn three() {}\n");
        let mut worker = SyntaxWorker::default();
        let before = finish(&mut editor, &mut worker);
        let transaction = editor
            .document
            .transaction([
                Edit::insert(CharOffset(0), "// "),
                Edit::insert(CharOffset(24), "界"),
            ])
            .unwrap();
        editor.apply(transaction, true).unwrap();
        let transaction = editor
            .document
            .transaction([Edit::insert(
                CharOffset(editor.document.text().len_chars()),
                "\n",
            )])
            .unwrap();
        editor.apply(transaction, true).unwrap();
        let range = ByteOffset(0)..ByteOffset(editor.document.text().len_bytes());
        let retained = editor.syntax_highlights(range.clone());
        assert!(
            retained
                .iter()
                .any(|span| span.range == (ByteOffset(15)..ByteOffset(17))
                    && span.highlight == crate::Highlight::Keyword)
        );
        assert!(editor.take_syntax_job().is_some());
        editor.execute("undo", 1).unwrap();
        assert_eq!(
            editor.syntax_highlights(ByteOffset(0)..ByteOffset(editor.document.text().len_bytes())),
            before
        );
        editor.execute("redo", 1).unwrap();
        assert_eq!(editor.syntax_highlights(range), retained);
    }

    #[test]
    fn changed_viewport_boundaries_reuse_clipped_disjoint_colors_until_completion() {
        let mut editor = editor("fn main() {}\n");
        let mut worker = SyntaxWorker::default();
        // Overlapping views must not duplicate spans in the fallback.
        editor.syntax_highlights(ByteOffset(0)..ByteOffset(13));
        editor.syntax_highlights(ByteOffset(1)..ByteOffset(10));
        let job = editor.take_syntax_job().unwrap();
        assert!(editor.apply_syntax_result(worker.run(job).unwrap()));
        let transaction = editor
            .document
            .transaction([Edit::insert(CharOffset(3), "\n")])
            .unwrap();
        editor.apply(transaction, true).unwrap();
        editor.begin_syntax_frame();
        let first = editor.syntax_highlights(ByteOffset(0)..ByteOffset(3));
        assert_eq!(
            &*first,
            &[HighlightSpan {
                range: ByteOffset(0)..ByteOffset(2),
                highlight: crate::Highlight::Keyword
            }]
        );
        let clipped = editor.syntax_highlights(ByteOffset(5)..ByteOffset(7));
        assert_eq!(
            &*clipped,
            &[HighlightSpan {
                range: ByteOffset(5)..ByteOffset(7),
                highlight: crate::Highlight::Function
            }]
        );
        assert!(editor.take_syntax_job().is_some());
        // Joining lines also reuses both source ranges before another parse.
        let transaction = editor
            .document
            .transaction([Edit::delete(CharOffset(3)..CharOffset(4))])
            .unwrap();
        editor.apply(transaction, true).unwrap();
        let joined = editor.syntax_highlights(ByteOffset(0)..ByteOffset(12));
        assert!(
            joined
                .iter()
                .any(|span| span.highlight == crate::Highlight::Keyword)
        );
        assert!(
            joined
                .iter()
                .any(|span| span.highlight == crate::Highlight::Function)
        );
        assert!(
            joined
                .windows(2)
                .all(|pair| pair[0].range.end <= pair[1].range.start)
        );
    }

    #[test]
    fn replaced_tokens_and_empty_results_do_not_leave_old_colors_behind() {
        let mut editor = editor("fn main() {}\n");
        let mut worker = SyntaxWorker::default();
        finish(&mut editor, &mut worker);
        let transaction = editor
            .document
            .transaction([Edit::new(CharOffset(0)..CharOffset(2), "  ")])
            .unwrap();
        editor.apply(transaction, false).unwrap();
        let range = ByteOffset(0)..ByteOffset(editor.document.text().len_bytes());
        let retained = editor.syntax_highlights(range.clone());
        assert!(!retained.is_empty());
        assert!(
            retained
                .iter()
                .all(|span| span.range.start >= ByteOffset(2))
        );
        let mut result = worker.run(editor.take_syntax_job().unwrap()).unwrap();
        // Simulate a query budget fallback, even though colors existed before.
        for entry in &mut result.ranges {
            entry.spans = Arc::from([]);
        }
        assert!(editor.apply_syntax_result(result));
        assert!(editor.syntax_highlights(range).is_empty());
        assert!(editor.take_syntax_job().is_none());
    }

    #[test]
    fn provisional_colors_are_cleared_on_language_reset_size_limit_or_revision_gap() {
        let mut editor = editor("fn main() {}\n");
        let mut worker = SyntaxWorker::default();
        finish(&mut editor, &mut worker);
        editor.execute("insert_mode", 1).unwrap();
        editor.insert_text(" ").unwrap();
        editor.set_language(Some(Language::Bash));
        assert!(
            editor
                .syntax_highlights(ByteOffset(0)..ByteOffset(13))
                .is_empty()
        );
        editor.set_language(Some(Language::Rust));
        finish(&mut editor, &mut worker);
        editor
            .insert_text(&" ".repeat(MAX_HIGHLIGHT_BYTES))
            .unwrap();
        assert!(editor.syntax.borrow().cache.is_empty());
        assert!(
            editor
                .syntax_highlights(ByteOffset(0)..ByteOffset(13))
                .is_empty()
        );
        assert!(editor.take_syntax_job().is_none());
        editor.execute("undo", 1).unwrap();
        finish(&mut editor, &mut worker);
        for _ in 0..2 {
            let transaction = editor
                .document
                .transaction([Edit::insert(CharOffset(0), " ")])
                .unwrap();
            editor
                .document
                .apply(transaction, &mut editor.selections)
                .unwrap();
        }
        editor.synchronize_caches();
        assert!(editor.syntax.borrow().cache.is_empty());
    }

    #[test]
    fn synchronous_highlighting_lends_its_tree_without_another_parse() {
        let mut editor = editor("fn f() { f(1); }");
        editor.set_background_syntax(false);
        assert!(editor.parsed_syntax().is_none());
        editor.syntax_highlights(ByteOffset(0)..ByteOffset(editor.document.text().len_bytes()));
        assert!(editor.parsed_syntax().unwrap().available());
        editor.release_syntax_tree();
        assert!(editor.parsed_syntax().is_none());
    }

    #[test]
    fn late_completions_cannot_survive_edits_history_or_language_resets() {
        let mut editor = editor("fn main() {}\n");
        let mut worker = SyntaxWorker::default();
        for operation in 0..5 {
            let result = worker.run(request(&mut editor)).unwrap();
            match operation {
                0 => {
                    editor.execute("insert_mode", 1).unwrap();
                    editor.insert_text("//").unwrap();
                }
                1 => {
                    editor.execute("undo", 1).unwrap();
                }
                2 => {
                    editor.execute("redo", 1).unwrap();
                }
                3 => editor.set_language(Some(Language::Rust)),
                _ => {
                    editor.set_language(None);
                    editor.set_language(Some(Language::Rust));
                }
            }
            assert!(!editor.apply_syntax_result(result));
        }
        let result = worker.run(request(&mut editor)).unwrap();
        editor.set_background_syntax(false);
        assert!(!editor.apply_syntax_result(result));
    }

    #[test]
    fn viewport_changes_cancel_obsolete_jobs_and_zero_sized_frames_cancel_work() {
        let mut editor = editor("fn a() {}\nfn b() {}\n");
        let mut worker = SyntaxWorker::default();
        editor.syntax_highlights(ByteOffset(0)..ByteOffset(9));
        let first = worker.run(editor.take_syntax_job().unwrap()).unwrap();
        editor.begin_syntax_frame();
        editor.syntax_highlights(ByteOffset(10)..ByteOffset(19));
        let second = editor.take_syntax_job().unwrap();
        assert!(!editor.apply_syntax_result(first));
        editor.begin_syntax_frame();
        assert!(editor.take_syntax_job().is_none());
        assert!(second.cancellation().is_cancelled());
        assert!(worker.run(second).is_none());
    }

    #[test]
    fn empty_results_and_large_or_tall_documents_do_not_reschedule_forever() {
        let mut editor = editor(" \n".repeat(MAX_RANGES + 1).as_str());
        let mut worker = SyntaxWorker::default();
        for _ in 0..3 {
            editor.begin_syntax_frame();
            for index in 0..=MAX_RANGES {
                assert!(
                    editor
                        .syntax_highlights(ByteOffset(index * 2)..ByteOffset(index * 2 + 1))
                        .is_empty()
                );
            }
            if let Some(job) = editor.take_syntax_job() {
                assert_eq!(job.ranges.len(), MAX_RANGES);
                assert!(editor.apply_syntax_result(worker.run(job).unwrap()));
            } else {
                assert_eq!(editor.syntax.borrow().cache.len(), MAX_RANGES);
            }
        }
        assert!(editor.take_syntax_job().is_none());
        let mut huge = self::editor(&"x".repeat(MAX_HIGHLIGHT_BYTES + 1));
        assert!(
            huge.syntax_highlights(ByteOffset(0)..ByteOffset(80))
                .is_empty()
        );
        assert!(huge.take_syntax_job().is_none());
    }

    #[test]
    fn coalesced_edits_and_counted_history_keep_a_conservative_incremental_extent() {
        for ending in ["\n", "\r\n", "\r", "\u{2028}"] {
            let source = format!("fn one() {{}}{ending}fn two() {{}}{ending}");
            let mut editor = editor(&source);
            let mut worker = SyntaxWorker::default();
            let before = finish(&mut editor, &mut worker);
            editor.execute("insert_mode", 1).unwrap();
            for text in ["/", "/", "界"] {
                editor.insert_text(text).unwrap();
            }
            editor.insert_paste(ending).unwrap();
            let job = request(&mut editor);
            let extent = job
                .change_since(worker.syntax.as_ref().unwrap().snapshot())
                .unwrap();
            assert_eq!(extent.start, CharOffset(0));
            assert_eq!(extent.old_end, CharOffset(0));
            assert_eq!(extent.new_end, CharOffset(3 + ending.chars().count()));
            assert!(editor.apply_syntax_result(worker.run(job).unwrap()));
            editor.execute("undo", 2).unwrap();
            assert_eq!(finish(&mut editor, &mut worker), before);
            editor.execute("redo", 2).unwrap();
            let after = finish(&mut editor, &mut worker);
            let mut fresh = Syntax::new(Language::Rust, editor.document());
            assert_eq!(
                after,
                fresh.highlights(
                    editor.document(),
                    ByteOffset(0)..ByteOffset(editor.document.text().len_bytes())
                )
            );
        }
    }

    #[test]
    fn change_log_is_bounded_and_falls_back_when_the_worker_is_too_far_behind() {
        let mut editor = editor("fn main() {}\n");
        let mut worker = SyntaxWorker::default();
        finish(&mut editor, &mut worker);
        editor.execute("insert_mode", 1).unwrap();
        for _ in 0..=MAX_CHANGES {
            editor.insert_text(" ").unwrap();
        }
        let job = request(&mut editor);
        assert_eq!(job.changes.len(), MAX_CHANGES);
        assert!(
            job.change_since(worker.syntax.as_ref().unwrap().snapshot())
                .is_none()
        );
        assert!(editor.apply_syntax_result(worker.run(job).unwrap()));
        assert!(
            !editor
                .syntax_highlights(ByteOffset(0)..ByteOffset(editor.document.text().len_bytes()))
                .is_empty()
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn composed_changes_preserve_unchanged_prefix_and_suffix(
            operations in prop::collection::vec((any::<usize>(), any::<usize>(), prop::sample::select(vec!["", "界", "\r\n", "abc", "e\u{301}"])), 1..80)
        ) {
            let mut editor = editor("fn main() { let name = \"界\"; }\r\n");
            let previous = editor.document.snapshot();
            for (start, end, inserted) in operations {
                let len = editor.document.text().len_chars();
                let start = start % (len + 1);
                let end = start + end % (len - start + 1);
                let transaction = editor.document.transaction([Edit::new(CharOffset(start)..CharOffset(end), inserted)]).unwrap();
                editor.apply(transaction, true).unwrap();
                let job = {
                    // Metadata composition also applies when the entire text is deleted.
                    let state = editor.syntax.borrow();
                    SyntaxJob {
                        snapshot: editor.document.snapshot(), language: Language::Rust,
                        session: state.session.clone(), cancellation: Cancellation::default(),
                        ranges: vec![], changes: state.changes.iter().cloned().collect(),
                    }
                };
                if previous.revision() != editor.document.revision() {
                    let extent = job.change_since(&previous).unwrap();
                    prop_assert!(extent.start <= extent.old_end && extent.start <= extent.new_end);
                    prop_assert_eq!(previous.text().slice(..extent.start.0), editor.document.text().slice(..extent.start.0));
                    prop_assert_eq!(previous.text().slice(extent.old_end.0..), editor.document.text().slice(extent.new_end.0..));
                }
            }
        }
    }
}
