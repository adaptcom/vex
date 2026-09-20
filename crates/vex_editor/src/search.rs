//! Text search and selection scans carry immutable snapshots. Frontends may
//! execute them on a worker; only the editor applies results after validating
//! request and view state.

use crate::{CommandContext, Editor, Error, Mode};
use std::{num::NonZeroUsize, sync::Arc};
use vex_core::{
    DocumentId, Revision, SelectionSet, Snapshot,
    regex::{Options, Regex},
};

/// Preview state, including invalid patterns. Pending acceptance completes when
/// the matching result arrives; invalid and missing queries remain editable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchStatus {
    Invalid,
    Empty,
    Pending,
    Match,
    NoMatch,
}

/// The operation requested by a regex prompt. Search uses the primary selection;
/// selection operations restrict matching to the current ranges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchPrompt {
    Forward,
    Backward,
    Select,
    Split,
    Keep,
    Remove,
}

impl SearchPrompt {
    pub fn label(self) -> &'static str {
        match self {
            Self::Forward => "/",
            Self::Backward => "?",
            Self::Select => "select: ",
            Self::Split => "split: ",
            Self::Keep => "keep: ",
            Self::Remove => "remove: ",
        }
    }
}

mod matching;

/// Cancellation shared by the editor and its search worker.
pub type SearchCancellation = crate::background::Cancellation;

#[derive(Debug)]
enum Pattern {
    Text(Arc<str>),
    Selection,
    Compiled(Arc<CompiledPattern>),
}

#[derive(Debug)]
struct CompiledPattern {
    text: Arc<str>,
    regex: Regex,
    crlf: bool,
}

#[derive(Debug)]
enum Work {
    Surround {
        character: Option<char>,
        count: usize,
        replace: bool,
    },
    ReplaceSurround {
        pairs: Arc<[vex_core::pairs::Delimiters]>,
        replacement: char,
    },
    MatchBrackets {
        extend: bool,
    },
    Textobject {
        object: crate::textobject::Object,
        around: bool,
        count: usize,
    },
    Search {
        pattern: Pattern,
        operation: SearchPrompt,
        count: usize,
        extend: bool,
        crlf: bool,
    },
    Remember {
        boundaries: bool,
        crlf: bool,
    },
    CopyLines {
        count: usize,
        down: bool,
        tabs: NonZeroUsize,
    },
}

#[derive(Debug)]
enum Outcome {
    SurroundPreview(crate::surround::Resolved),
    Edit(vex_core::Transaction),
    Search(Arc<CompiledPattern>, Option<SelectionSet>),
    Selections(SelectionSet),
    Remember(Arc<CompiledPattern>),
}

/// Owned, Send text-search or selection-scan work. No mutable editor state
/// crosses the worker boundary. Both use the same ordered worker mailbox.
#[derive(Debug)]
pub struct SearchJob {
    snapshot: Snapshot,
    origins: SelectionSet,
    work: Work,
    cancellation: SearchCancellation,
    syntax: Option<vex_syntax::ParsedSyntax>,
    language: Option<crate::Language>,
}

/// An opaque completion, applicable only to the request that produced it.
#[derive(Debug)]
pub struct SearchResult {
    cancellation: SearchCancellation,
    outcome: Result<Outcome, Error>,
    syntax: Option<vex_syntax::ParsedSyntax>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchCompletion {
    Ignored,
    Preview,
    Accepted,
    Navigation,
}

impl SearchJob {
    pub fn cancellation(&self) -> SearchCancellation {
        self.cancellation.clone()
    }

    /// Compile and scan the snapshot. Cancelled jobs return no completion.
    /// Cancellation is checked before/after compilation and during scanning;
    /// grapheme-boundary and individual display-column lookups are not preemptible.
    pub fn run(mut self) -> Option<SearchResult> {
        let cancelled = || self.cancellation.is_cancelled();
        if cancelled() {
            return None;
        }
        if self.syntax.is_none()
            && let Some(language) = self.language
            && self.snapshot.text().len_bytes() <= vex_syntax::MAX_HIGHLIGHT_BYTES
        {
            self.syntax = Some(
                vex_syntax::Syntax::from_snapshot(language, self.snapshot.clone())
                    .parsed(&cancelled),
            );
        }
        let outcome = (|| match self.work {
            Work::Surround {
                character,
                count,
                replace,
            } => {
                let resolved = crate::surround::resolve(
                    &self.snapshot,
                    self.syntax.as_ref(),
                    &self.origins,
                    character,
                    count,
                    &cancelled,
                )?;
                if replace {
                    crate::surround::prepare(&self.snapshot, &self.origins, resolved, &cancelled)
                        .map(Outcome::SurroundPreview)
                } else {
                    crate::surround::transaction(
                        &self.snapshot,
                        &self.origins,
                        &resolved,
                        None,
                        &cancelled,
                    )
                    .map(Outcome::Edit)
                }
            }
            Work::ReplaceSurround { pairs, replacement } => crate::surround::transaction(
                &self.snapshot,
                &self.origins,
                &pairs,
                Some(replacement),
                &cancelled,
            )
            .map(Outcome::Edit),
            Work::MatchBrackets { extend } => crate::textobject::match_brackets(
                self.snapshot.text(),
                self.syntax.as_ref(),
                &self.origins,
                extend,
                &cancelled,
            )
            .map(Outcome::Selections),
            Work::Textobject {
                object,
                around,
                count,
            } => crate::textobject::select(
                self.snapshot.text(),
                &self.origins,
                object,
                around,
                count,
                self.syntax.as_ref(),
                &cancelled,
            )
            .map(Outcome::Selections),
            Work::Search {
                pattern,
                operation,
                count,
                extend,
                crlf,
            } => {
                let pattern = match pattern {
                    Pattern::Text(text) => compile(text, crlf)?,
                    Pattern::Selection => {
                        let selection = self.origins.ranges()[0];
                        let text = self
                            .snapshot
                            .text()
                            .slice(selection.start().0..selection.end().0);
                        if text.len_bytes() > vex_core::regex::MAX_PATTERN_BYTES {
                            return Err(Error::InvalidRegex("regex exceeds 64 KiB".into()));
                        }
                        compile(Arc::from(text.to_string()), crlf)?
                    }
                    Pattern::Compiled(pattern) => pattern,
                };
                matching::apply(
                    self.snapshot.text(),
                    &self.origins,
                    &pattern.regex,
                    operation,
                    count,
                    extend,
                    &cancelled,
                )
                .map(|selections| Outcome::Search(pattern, selections))
            }
            Work::Remember { boundaries, crlf } => {
                let query = matching::selection_pattern(
                    self.snapshot.text(),
                    &self.origins,
                    boundaries,
                    &cancelled,
                )?;
                compile(query.into(), crlf).map(Outcome::Remember)
            }
            Work::CopyLines { count, down, tabs } => {
                let document = vex_core::Document::from(self.snapshot.text().clone());
                crate::selection::copy_lines(
                    &document,
                    &self.origins,
                    count,
                    down,
                    tabs,
                    &cancelled,
                )
                .map(Outcome::Selections)
            }
        })();
        if cancelled() {
            return None;
        }
        Some(SearchResult {
            cancellation: self.cancellation,
            outcome,
            syntax: self.syntax,
        })
    }
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    SurroundPrepare,
    SurroundEdit,
    Preview { accept: bool },
    Repeat,
    CopyLines,
    Textobject,
    Remember(char),
}

#[derive(Debug)]
struct Pending {
    cancellation: SearchCancellation,
    document: DocumentId,
    revision: Revision,
    selections: SelectionSet,
    mode: Mode,
    kind: Kind,
    structural: bool,
}

#[derive(Debug, Default)]
pub(crate) struct Search {
    accepted: Option<Arc<CompiledPattern>>,
    pub preview: Option<Preview>,
    pub background: bool,
    pub outgoing: Option<SearchJob>,
    pending: Option<Pending>,
}

impl Search {
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }
    pub fn waiting(&self) -> bool {
        self.pending.as_ref().is_some_and(|p| {
            matches!(
                p.kind,
                Kind::Repeat
                    | Kind::SurroundPrepare
                    | Kind::SurroundEdit
                    | Kind::CopyLines
                    | Kind::Textobject
                    | Kind::Remember(_)
                    | Kind::Preview { accept: true }
            )
        })
    }
    pub fn progress(&self) -> Option<&'static str> {
        self.pending.as_ref().map(|pending| match pending.kind {
            Kind::SurroundPrepare | Kind::SurroundEdit => "matching surrounds...",
            Kind::CopyLines | Kind::Textobject => "selecting...",
            _ => "searching...",
        })
    }
    pub fn invalidate_columns(&mut self) {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| matches!(pending.kind, Kind::CopyLines))
        {
            self.cancel_jobs();
        }
    }
    pub fn invalidate_syntax(&mut self) {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.structural)
        {
            self.cancel_jobs();
        }
    }
    pub fn preparing_surround(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| matches!(pending.kind, Kind::SurroundPrepare))
    }
    pub fn invalidate_surround(&mut self) {
        if self.pending.as_ref().is_some_and(|pending| {
            matches!(pending.kind, Kind::SurroundPrepare | Kind::SurroundEdit)
        }) {
            self.cancel_jobs();
        }
    }
    fn cancel_jobs(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.cancellation.cancel();
        }
        self.outgoing = None;
    }
    pub fn invalidate(&mut self) {
        if self
            .pending
            .as_ref()
            .is_some_and(|p| matches!(p.kind, Kind::Preview { .. }))
        {
            self.preview = None;
        }
        self.cancel_jobs();
    }
}

impl Drop for Search {
    fn drop(&mut self) {
        self.cancel_jobs();
    }
}

#[derive(Debug)]
pub(crate) struct Preview {
    origin: SelectionSet,
    columns: Option<Vec<usize>>,
    revision: Revision,
    mode: Mode,
    count: usize,
    pattern: Option<Arc<CompiledPattern>>,
    register: char,
    pub operation: SearchPrompt,
    pub error: Option<Error>,
    pub status: SearchStatus,
}

pub(crate) fn begin(ctx: &mut CommandContext<'_>, operation: SearchPrompt) -> Result<(), Error> {
    let editor = &mut *ctx.editor;
    if editor.search.preview.is_some() {
        return Err(Error::SearchActive);
    }
    require_normal_or_select(editor)?;
    let register = ctx.register.unwrap_or('/');
    writable_query(register)?;
    editor.finish_undo_group();
    editor.search.preview = Some(Preview {
        origin: editor.selections.clone(),
        columns: editor.preferred_columns.clone(),
        revision: editor.document.revision(),
        mode: editor.mode,
        count: ctx.count.get(),
        pattern: None,
        register,
        operation,
        error: None,
        status: SearchStatus::Empty,
    });
    Ok(())
}

fn require_normal_or_select(editor: &Editor) -> Result<(), Error> {
    if editor.mode == Mode::Insert {
        return Err(Error::WrongMode {
            expected: Mode::Normal,
            actual: editor.mode,
        });
    }
    Ok(())
}

fn check_preview(editor: &mut Editor) -> Result<(), Error> {
    let preview = editor
        .search
        .preview
        .as_ref()
        .ok_or(Error::NoSearchPreview)?;
    if preview.revision != editor.document.revision() || preview.mode != editor.mode {
        editor.search.cancel_jobs();
        editor.search.preview = None;
        return Err(Error::SearchChanged);
    }
    Ok(())
}

pub(crate) fn update(editor: &mut Editor, query: &str) -> Result<(), Error> {
    check_preview(editor)?;
    editor.search.cancel_jobs();
    let preview = editor.search.preview.as_mut().unwrap();
    editor.selections = preview.origin.clone();
    editor.preferred_columns = preview.columns.clone();
    preview.pattern = None;
    preview.error = None;
    preview.status = if query.is_empty() {
        SearchStatus::Empty
    } else {
        SearchStatus::Pending
    };
    if query.is_empty() {
        return Ok(());
    }
    let (operation, count) = (preview.operation, preview.count);
    schedule(
        editor,
        Pattern::Text(Arc::from(query)),
        operation,
        count,
        Kind::Preview { accept: false },
    )
}

fn schedule(
    editor: &mut Editor,
    pattern: Pattern,
    operation: SearchPrompt,
    count: usize,
    kind: Kind,
) -> Result<(), Error> {
    dispatch(
        editor,
        Work::Search {
            pattern,
            operation,
            count,
            extend: editor.mode == Mode::Select,
            crlf: editor.newline == "\r\n",
        },
        kind,
    )
}

pub(crate) fn copy_lines(ctx: &mut CommandContext<'_>, down: bool) -> Result<(), Error> {
    let editor = &mut *ctx.editor;
    require_normal_or_select(editor)?;
    editor.finish_undo_group();
    dispatch(
        editor,
        Work::CopyLines {
            count: ctx.count.get(),
            down,
            tabs: editor.tab_width,
        },
        Kind::CopyLines,
    )
}

pub(crate) fn textobject(ctx: &mut CommandContext<'_>, around: bool) -> Result<(), Error> {
    let object = match ctx.character.ok_or(Error::MissingCharacter)? {
        'w' => crate::textobject::Object::Word,
        'W' => crate::textobject::Object::LongWord,
        'p' => crate::textobject::Object::Paragraph,
        'm' => crate::textobject::Object::Pair(None),
        ch if !ch.is_ascii_alphanumeric() => crate::textobject::Object::Pair(Some(ch)),
        _ => return Ok(()),
    };
    require_normal_or_select(ctx.editor)?;
    ctx.editor.finish_undo_group();
    dispatch(
        ctx.editor,
        Work::Textobject {
            object,
            around,
            count: ctx.count.get(),
        },
        Kind::Textobject,
    )
}

pub(crate) fn match_brackets(ctx: &mut CommandContext<'_>) -> Result<(), Error> {
    require_normal_or_select(ctx.editor)?;
    ctx.editor.finish_undo_group();
    dispatch(
        ctx.editor,
        Work::MatchBrackets {
            extend: ctx.editor.mode == Mode::Select,
        },
        Kind::Textobject,
    )
}

pub(crate) fn surround(ctx: &mut CommandContext<'_>, replace: bool) -> Result<(), Error> {
    let character = ctx.character.ok_or(Error::MissingCharacter)?;
    require_normal_or_select(ctx.editor)?;
    ctx.editor.finish_undo_group();
    dispatch(
        ctx.editor,
        Work::Surround {
            character: (character != 'm').then_some(character),
            count: ctx.count.get(),
            replace,
        },
        if replace {
            Kind::SurroundPrepare
        } else {
            Kind::SurroundEdit
        },
    )
}

pub(crate) fn replace_surround(
    editor: &mut Editor,
    pairs: Arc<[vex_core::pairs::Delimiters]>,
    replacement: char,
) -> Result<(), Error> {
    require_normal_or_select(editor)?;
    editor.finish_undo_group();
    dispatch(
        editor,
        Work::ReplaceSurround { pairs, replacement },
        Kind::SurroundEdit,
    )
}

fn dispatch(editor: &mut Editor, work: Work, kind: Kind) -> Result<(), Error> {
    let cancellation = SearchCancellation::default();
    let snapshot = editor.document.snapshot();
    // Explicit asymmetric delimiters balance their own kind without syntax.
    // Avoid initializing a grammar or parsing a cold file for md( / mi[, etc.
    let structural = match &work {
        Work::MatchBrackets { .. } => true,
        Work::Surround { character, .. }
        | Work::Textobject {
            object: crate::textobject::Object::Pair(character),
            ..
        } => character.is_none_or(|ch| {
            let (open, close) = vex_core::pairs::pair(ch);
            open == close
        }),
        _ => false,
    };
    editor.search.pending = Some(Pending {
        cancellation: cancellation.clone(),
        document: snapshot.id(),
        revision: snapshot.revision(),
        selections: editor.selections.clone(),
        mode: editor.mode,
        kind,
        structural,
    });
    let job = SearchJob {
        snapshot,
        origins: editor.selections.clone(),
        work,
        cancellation,
        syntax: if structural {
            editor.parsed_syntax()
        } else {
            None
        },
        language: if structural { editor.language() } else { None },
    };
    if editor.search.background {
        editor.search.outgoing = Some(job);
        Ok(())
    } else {
        apply_result(
            editor,
            job.run().expect("synchronous request cannot be cancelled"),
        )?;
        Ok(())
    }
}

pub(crate) fn apply_result(
    editor: &mut Editor,
    result: SearchResult,
) -> Result<SearchCompletion, Error> {
    let Some(pending) = editor.search.pending.as_ref() else {
        return Ok(SearchCompletion::Ignored);
    };
    if !pending.cancellation.same_request(&result.cancellation)
        || result.cancellation.is_cancelled()
    {
        return Ok(SearchCompletion::Ignored);
    }
    let pending = editor.search.pending.take().unwrap();
    editor.search.outgoing = None;
    if pending.document != editor.document.id()
        || pending.revision != editor.document.revision()
        || pending.selections != editor.selections
        || pending.mode != editor.mode
    {
        if matches!(pending.kind, Kind::Preview { .. }) {
            editor.search.preview = None;
        }
        return Ok(SearchCompletion::Ignored);
    }
    if let Some(syntax) = result.syntax {
        editor.cache_parsed_syntax(syntax);
    }
    let (pattern, selections) = match result.outcome {
        Ok(Outcome::SurroundPreview(resolved)) => {
            crate::surround::preview(editor, resolved);
            return Ok(SearchCompletion::Navigation);
        }
        Ok(Outcome::Edit(transaction)) => {
            crate::surround::apply(editor, transaction)?;
            return Ok(SearchCompletion::Navigation);
        }
        Ok(Outcome::Search(pattern, selections)) => (pattern, selections),
        Ok(Outcome::Remember(pattern)) => {
            let Kind::Remember(register) = pending.kind else {
                unreachable!("remember request")
            };
            publish(editor, register, pattern, true)?;
            return Ok(SearchCompletion::Navigation);
        }
        Ok(Outcome::Selections(selections)) => {
            editor.selections = selections;
            editor.preferred_columns = None;
            return Ok(SearchCompletion::Navigation);
        }
        Err(error) => {
            if let Some(preview) = editor.search.preview.as_mut() {
                preview.status = SearchStatus::Invalid;
                preview.error = Some(error.clone());
            }
            return Err(error);
        }
    };
    match pending.kind {
        Kind::Preview {
            accept: accept_requested,
        } => {
            let preview = editor
                .search
                .preview
                .as_mut()
                .expect("active preview request");
            preview.status = if selections.is_some() {
                SearchStatus::Match
            } else {
                SearchStatus::NoMatch
            };
            preview.pattern = Some(pattern);
            if let Some(selections) = selections {
                editor.selections = selections;
                editor.preferred_columns = None;
                if accept_requested {
                    accept(editor)?;
                    return Ok(SearchCompletion::Accepted);
                }
            }
            Ok(SearchCompletion::Preview)
        }
        Kind::Repeat => {
            editor.search.accepted = Some(pattern);
            editor.selections = selections.ok_or(Error::NoMatch)?;
            editor.preferred_columns = None;
            Ok(SearchCompletion::Navigation)
        }
        Kind::CopyLines
        | Kind::Textobject
        | Kind::Remember(_)
        | Kind::SurroundPrepare
        | Kind::SurroundEdit => unreachable!("handled above"),
    }
}

pub(crate) fn accept(editor: &mut Editor) -> Result<(), Error> {
    check_preview(editor)?;
    let preview = editor.search.preview.as_ref().unwrap();
    match preview.status {
        SearchStatus::Empty => cancel(editor),
        SearchStatus::Pending => {
            editor
                .search
                .pending
                .as_mut()
                .expect("pending preview")
                .kind = Kind::Preview { accept: true };
            Ok(())
        }
        SearchStatus::Invalid => Err(preview.error.clone().expect("invalid preview error")),
        SearchStatus::NoMatch => Err(Error::NoMatch),
        SearchStatus::Match => {
            let preview = editor.search.preview.take().unwrap();
            let pattern = preview.pattern.as_ref().expect("matched pattern");
            publish(
                editor,
                preview.register,
                pattern.clone(),
                matches!(
                    preview.operation,
                    SearchPrompt::Forward | SearchPrompt::Backward
                ),
            )?;
            Ok(())
        }
    }
}

pub(crate) fn cancel(editor: &mut Editor) -> Result<(), Error> {
    check_preview(editor)?;
    editor.search.cancel_jobs();
    let preview = editor.search.preview.take().unwrap();
    editor.selections = preview.origin;
    editor.preferred_columns = preview.columns;
    Ok(())
}

pub(crate) fn repeat(ctx: &mut CommandContext<'_>, reverse: bool) -> Result<(), Error> {
    let editor = &mut *ctx.editor;
    if editor.search.preview.is_some() {
        return Err(Error::SearchActive);
    }
    require_normal_or_select(editor)?;
    let register = ctx
        .register
        .unwrap_or_else(|| editor.yank_register.last_search());
    if let Some(kind) = crate::ClipboardKind::from_register(register) {
        editor.finish_undo_group();
        editor.request_clipboard(
            kind,
            crate::ClipboardAction::Search { reverse },
            ctx.count.get(),
        );
        return Ok(());
    }
    let pattern = if register == '.' {
        // Capturing a potentially large dynamic register belongs on the worker.
        Pattern::Selection
    } else {
        let text = editor.register_first(register)?.ok_or(Error::NoSearch)?;
        cached_pattern(editor, text)
    };
    let operation = if reverse {
        SearchPrompt::Backward
    } else {
        SearchPrompt::Forward
    };
    editor.finish_undo_group();
    schedule(editor, pattern, operation, ctx.count.get(), Kind::Repeat)
}

fn writable_query(name: char) -> Result<(), Error> {
    if crate::ClipboardKind::from_register(name).is_some() {
        Ok(())
    } else {
        crate::register::writable(name)
    }
}

fn publish(
    editor: &mut Editor,
    register: char,
    pattern: Arc<CompiledPattern>,
    activate: bool,
) -> Result<(), Error> {
    if let Some(kind) = crate::ClipboardKind::from_register(register) {
        editor.request_clipboard_search_write(kind, pattern.text.clone(), activate);
    } else {
        editor
            .yank_register
            .remember_search(register, pattern.text.clone(), activate)?;
    }
    editor.search.accepted = Some(pattern);
    Ok(())
}

fn cached_pattern(editor: &Editor, text: Arc<str>) -> Pattern {
    match editor.search.accepted.as_ref() {
        Some(pattern)
            if Arc::ptr_eq(&pattern.text, &text) && pattern.crlf == (editor.newline == "\r\n") =>
        {
            Pattern::Compiled(pattern.clone())
        }
        _ => Pattern::Text(text),
    }
}

pub(crate) fn from_clipboard(
    editor: &mut Editor,
    text: Arc<str>,
    reverse: bool,
    count: usize,
) -> Result<(), Error> {
    require_normal_or_select(editor)?;
    if editor.search.preview.is_some() {
        return Err(Error::SearchActive);
    }
    let pattern = cached_pattern(editor, text);
    editor.finish_undo_group();
    schedule(
        editor,
        pattern,
        if reverse {
            SearchPrompt::Backward
        } else {
            SearchPrompt::Forward
        },
        count,
        Kind::Repeat,
    )
}

fn compile(query: Arc<str>, crlf: bool) -> Result<Arc<CompiledPattern>, Error> {
    if query.len() > vex_core::regex::MAX_PATTERN_BYTES {
        return Err(Error::InvalidRegex("regex exceeds 64 KiB".into()));
    }
    let regex = Regex::new(
        &query,
        Options {
            case_insensitive: !query.chars().any(char::is_uppercase),
            multi_line: true,
            crlf,
        },
    )
    .map_err(|error| Error::InvalidRegex(error.to_string()))?;
    Ok(Arc::new(CompiledPattern {
        text: query,
        regex,
        crlf,
    }))
}

pub(crate) fn remember(ctx: &mut CommandContext<'_>, boundaries: bool) -> Result<(), Error> {
    let editor = &mut *ctx.editor;
    require_normal_or_select(editor)?;
    if editor.search.preview.is_some() {
        return Err(Error::SearchActive);
    }
    let register = ctx.register.unwrap_or('/');
    writable_query(register)?;
    editor.finish_undo_group();
    dispatch(
        editor,
        Work::Remember {
            boundaries,
            crlf: editor.newline == "\r\n",
        },
        Kind::Remember(register),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SearchDirection;
    use crate::{Key, KeyHandler, Keymap, commands};
    use proptest::prelude::*;
    use vex_core::{CharOffset, Document, Selection};

    fn range(start: usize, end: usize) -> Selection {
        Selection::new(CharOffset(start), CharOffset(end))
    }

    fn deferred(source: &str) -> Editor {
        let mut editor = Editor::new(Document::from(source));
        editor.set_background_search(true);
        editor.execute("search_forward", 1).unwrap();
        editor
    }

    fn registered(editor: &mut Editor, name: char, command: &str) -> Result<(), Error> {
        let mut context = CommandContext::new(editor);
        context.register = Some(name);
        (commands::find(command).unwrap().run)(&mut context)
    }

    #[test]
    fn search_registers_share_queries_and_follow_later_writes_without_reusing_stale_patterns() {
        let mut source = Editor::new(Document::from("cat dog cat dog"));
        source.execute("search_forward", 1).unwrap();
        source.update_search("dog").unwrap();
        source.execute("search_accept", 1).unwrap();
        assert_eq!(source.register_first('/').unwrap().as_deref(), Some("dog"));
        registered(&mut source, 'a', "search_forward").unwrap();
        source.update_search("cat").unwrap();
        source.execute("search_accept", 1).unwrap();
        assert_eq!(source.register_first('/').unwrap().as_deref(), Some("dog"));
        assert_eq!(source.register_first('a').unwrap().as_deref(), Some("cat"));

        let mut target =
            Editor::with_session(Document::from("x cat dog cat dog"), source.session());
        target.execute("search_next", 1).unwrap();
        assert_eq!(target.selections.primary(), range(2, 5));
        let cached = target.search.accepted.clone().unwrap();
        target.execute("search_next", 1).unwrap();
        assert!(Arc::ptr_eq(
            &cached,
            target.search.accepted.as_ref().unwrap()
        ));
        assert_eq!(target.selections.primary(), range(10, 13));

        registered(&mut target, '/', "search_next").unwrap();
        assert_eq!(target.selections.primary(), range(14, 17));
        target.execute("search_next", 1).unwrap(); // Still uses a.
        assert_eq!(target.selections.primary(), range(2, 5));
        source
            .set_register('a', Arc::from([Arc::from("dog")]))
            .unwrap();
        target.execute("search_next", 1).unwrap();
        assert_eq!(target.selections.primary(), range(6, 9));
        assert!(!Arc::ptr_eq(
            &cached,
            target.search.accepted.as_ref().unwrap()
        ));
    }

    #[test]
    fn accepting_and_cancelling_worker_previews_update_only_the_chosen_register() {
        let mut editor = Editor::new(Document::from("x cat dog cat"));
        editor.set_background_search(true);
        editor
            .set_register('a', Arc::from([Arc::from("old")]))
            .unwrap();
        registered(&mut editor, 'a', "search_forward").unwrap();
        editor.update_search("cat").unwrap();
        let stale = editor.take_search_job().unwrap().run().unwrap();
        editor.execute("search_cancel", 1).unwrap();
        assert_eq!(
            editor.apply_search_result(stale).unwrap(),
            SearchCompletion::Ignored
        );
        assert_eq!(editor.register_first('a').unwrap().as_deref(), Some("old"));
        registered(&mut editor, 'a', "search_forward").unwrap();
        editor.update_search("cat").unwrap();
        editor.execute("search_accept", 1).unwrap();
        assert_eq!(editor.register_first('a').unwrap().as_deref(), Some("old"));
        let result = editor.take_search_job().unwrap().run().unwrap();
        assert_eq!(
            editor.apply_search_result(result).unwrap(),
            SearchCompletion::Accepted
        );
        assert_eq!(editor.register_first('a').unwrap().as_deref(), Some("cat"));
        assert!(editor.register_first('/').unwrap().is_none());
        registered(&mut editor, 'a', "search_forward").unwrap();
        editor.update_search("[").unwrap();
        let result = editor.take_search_job().unwrap().run().unwrap();
        assert!(editor.apply_search_result(result).is_err());
        assert_eq!(editor.register_first('a').unwrap().as_deref(), Some("cat"));
        editor.execute("search_cancel", 1).unwrap();
        for name in ['#', '.', '%'] {
            assert_eq!(
                registered(&mut editor, name, "search_forward"),
                Err(Error::ReadOnlyRegister(name))
            );
            assert_eq!(
                registered(&mut editor, name, "search_selection"),
                Err(Error::ReadOnlyRegister(name))
            );
            assert!(editor.search.preview.is_none());
            assert!(editor.take_search_job().is_none());
        }
    }

    #[test]
    fn selection_queries_keep_custom_search_choice_and_star_can_activate_it() {
        let mut editor = Editor::new(Document::from("cat a.b cat a.b"));
        registered(&mut editor, 'a', "search_forward").unwrap();
        editor.update_search("cat").unwrap();
        editor.execute("search_accept", 1).unwrap();
        editor.execute("select_all", 1).unwrap();
        registered(&mut editor, 'b', "select_regex").unwrap();
        editor.update_search(r"a\.b").unwrap();
        editor.execute("search_accept", 1).unwrap();
        assert_eq!(
            editor.register_first('b').unwrap().as_deref(),
            Some(r"a\.b")
        );
        assert_eq!(editor.yank_register.last_search(), 'a');
        registered(&mut editor, 'c', "search_selection").unwrap();
        assert_eq!(editor.yank_register.last_search(), 'c');
        assert!(
            editor
                .register_first('c')
                .unwrap()
                .unwrap()
                .contains(r"a\.b")
        );
        assert_eq!(editor.register_first('a').unwrap().as_deref(), Some("cat"));
    }

    #[test]
    fn register_queries_compile_on_worker_with_destination_line_endings_and_cancel() {
        let mut editor = Editor::new(Document::from("x\r\ncat\r\ncat\r\n"));
        editor.set_background_search(true);
        editor
            .set_register('a', Arc::from([Arc::from("cat$")]))
            .unwrap();
        registered(&mut editor, 'a', "search_next").unwrap();
        let job = editor.take_search_job().unwrap();
        assert!(matches!(
            &job.work,
            Work::Search {
                pattern: Pattern::Text(_),
                crlf: true,
                ..
            }
        ));
        editor
            .set_register('a', Arc::from([Arc::from("x")]))
            .unwrap();
        editor.apply_search_result(job.run().unwrap()).unwrap();
        assert_eq!(editor.selections.primary(), range(3, 6));
        registered(&mut editor, 'a', "search_next").unwrap();
        let job = editor.take_search_job().unwrap();
        editor.execute("move_left", 1).unwrap();
        assert!(job.run().is_none());

        editor
            .set_selections(SelectionSet::single(range(3, 6)))
            .unwrap();
        registered(&mut editor, '.', "search_next").unwrap();
        let job = editor.take_search_job().unwrap();
        assert!(matches!(
            &job.work,
            Work::Search {
                pattern: Pattern::Selection,
                ..
            }
        ));
        editor.apply_search_result(job.run().unwrap()).unwrap();
        assert_eq!(editor.selections.primary(), range(8, 11));
    }

    #[test]
    fn oversized_dynamic_queries_reject_on_worker_before_capturing_selection_text() {
        let mut editor = Editor::new(Document::from("x".repeat(1 << 20).as_str()));
        editor.set_background_search(true);
        editor.execute("select_all", 1).unwrap();
        registered(&mut editor, '.', "search_next").unwrap();
        let origin = editor.selections.clone();
        let job = editor.take_search_job().unwrap();
        assert!(matches!(
            &job.work,
            Work::Search {
                pattern: Pattern::Selection,
                ..
            }
        ));
        assert_eq!(
            editor.apply_search_result(job.run().unwrap()),
            Err(Error::InvalidRegex("regex exceeds 64 KiB".into()))
        );
        assert_eq!(editor.selections, origin);
        assert!(!editor.search_waiting());
    }

    #[test]
    fn structural_jobs_reuse_highlight_trees_and_reject_edits_and_language_changes() {
        use vex_core::ByteOffset;
        let mut editor = Editor::new(Document::from("fn f() { f(1); }"));
        editor.set_language(Some(crate::Language::Rust));
        editor.set_background_search(true);
        editor.set_background_syntax(true);
        editor.syntax_highlights(ByteOffset(0)..ByteOffset(editor.document.text().len_bytes()));
        let result = crate::SyntaxWorker::default()
            .run(editor.take_syntax_job().unwrap())
            .unwrap();
        assert!(editor.apply_syntax_result(result));
        editor
            .set_selections(SelectionSet::single(range(11, 12)))
            .unwrap();
        editor.execute("match_brackets", 1).unwrap();
        let job = editor.take_search_job().unwrap();
        assert!(job.syntax.as_ref().unwrap().available());
        let result = job.run().unwrap();
        assert_eq!(
            editor.apply_search_result(result).unwrap(),
            SearchCompletion::Navigation
        );
        assert_eq!(editor.selections.primary(), range(12, 13));
        editor.execute("match_brackets", 1).unwrap();
        let result = editor.take_search_job().unwrap().run().unwrap();
        editor.set_language(None);
        assert_eq!(
            editor.apply_search_result(result).unwrap(),
            SearchCompletion::Ignored
        );
        assert!(editor.parsed_syntax().is_none());
        editor.set_language(Some(crate::Language::Rust));
        editor.execute("match_brackets", 1).unwrap();
        let job = editor.take_search_job().unwrap();
        assert!(job.syntax.is_none());
        editor.apply_search_result(job.run().unwrap()).unwrap();
        assert!(editor.parsed_syntax().unwrap().available());
        editor.execute("insert_mode", 1).unwrap();
        editor.insert_text("x").unwrap();
        assert!(editor.parsed_syntax().is_none());
        editor.execute("normal_mode", 1).unwrap();
        editor.execute("match_brackets", 1).unwrap();
        let result = editor.take_search_job().unwrap().run().unwrap();
        editor.execute("move_left", 1).unwrap();
        assert_eq!(
            editor.apply_search_result(result).unwrap(),
            SearchCompletion::Ignored
        );
    }

    #[test]
    fn deferred_preview_rejects_out_of_order_results_and_accepts_when_ready() {
        fn is_send<T: Send>() {}
        is_send::<SearchJob>();
        is_send::<SearchResult>();
        let mut editor = deferred("x cat dog");
        let origin = editor.selections.clone();
        editor.update_search("cat").unwrap();
        let old = editor.take_search_job().unwrap().run().unwrap();
        editor.update_search("dog").unwrap();
        let newest = editor.take_search_job().unwrap();
        editor.execute("search_accept", 1).unwrap();
        assert!(editor.search_waiting());
        assert_eq!(editor.search_status(), Some(SearchStatus::Pending));
        assert_eq!(editor.selections, origin);
        assert_eq!(
            editor.apply_search_result(old).unwrap(),
            SearchCompletion::Ignored
        );
        assert!(editor.search_waiting());
        assert_eq!(
            editor.apply_search_result(newest.run().unwrap()).unwrap(),
            SearchCompletion::Accepted
        );
        assert_eq!(editor.selections.primary(), range(6, 9));
        assert!(!editor.search_pending());
        assert_eq!(editor.search_direction(), None);
    }

    #[test]
    fn empty_queries_and_cancellation_invalidate_jobs_and_late_completions() {
        let mut editor = deferred("x cat");
        let origin = editor.selections.clone();
        editor.update_search("cat").unwrap();
        let job = editor.take_search_job().unwrap();
        editor.update_search("").unwrap();
        assert!(job.cancellation().is_cancelled());
        assert!(job.run().is_none());
        assert_eq!(editor.search_status(), Some(SearchStatus::Empty));
        editor.update_search("cat").unwrap();
        let result = editor.take_search_job().unwrap().run().unwrap();
        editor.execute("search_accept", 1).unwrap();
        editor.execute("search_cancel", 1).unwrap();
        assert_eq!(
            editor.apply_search_result(result).unwrap(),
            SearchCompletion::Ignored
        );
        assert_eq!(editor.selections, origin);
        assert!(!editor.search_waiting());
    }

    #[test]
    fn deferred_missing_queries_keep_the_prompt_open_after_early_enter() {
        let mut editor = deferred("cat");
        editor.update_search("missing").unwrap();
        editor.execute("search_accept", 1).unwrap();
        let result = editor.take_search_job().unwrap().run().unwrap();
        assert_eq!(
            editor.apply_search_result(result).unwrap(),
            SearchCompletion::Preview
        );
        assert_eq!(editor.search_status(), Some(SearchStatus::NoMatch));
        assert!(!editor.search_waiting());
        assert_eq!(editor.execute("search_accept", 1), Err(Error::NoMatch));
    }

    #[test]
    fn movement_edit_mode_and_history_commands_cancel_pending_navigation() {
        for action in [
            "move_right",
            "insert_mode",
            "delete_selection",
            "undo",
            "redo",
        ] {
            let mut editor = Editor::new(Document::from("cat cat"));
            accept(&mut editor, "cat", false);
            editor.set_background_search(true);
            commands::search_next(&mut CommandContext::new(&mut editor)).unwrap();
            let job = editor.take_search_job().unwrap();
            let token = job.cancellation();
            let result = job.run().unwrap();
            editor.execute(action, 1).unwrap();
            let after = editor.selections.clone();
            assert!(token.is_cancelled(), "{action}");
            assert_eq!(
                editor.apply_search_result(result).unwrap(),
                SearchCompletion::Ignored
            );
            assert_eq!(editor.selections, after);
            assert!(!editor.search_pending());
        }
    }

    #[test]
    fn stale_preview_cannot_apply_after_edit_undo_or_cursor_round_trip() {
        for edit in [true, false] {
            let mut editor = deferred("cat cat");
            editor.update_search("cat").unwrap();
            let result = editor.take_search_job().unwrap().run().unwrap();
            if edit {
                editor.execute("delete_selection", 1).unwrap();
                editor.execute("undo", 1).unwrap();
            } else {
                editor.execute("move_right", 1).unwrap();
                editor.execute("move_left", 1).unwrap();
            }
            let after = editor.selections.clone();
            assert_eq!(
                editor.apply_search_result(result).unwrap(),
                SearchCompletion::Ignored
            );
            assert_eq!(editor.selections, after);
            assert!(!editor.search_pending());
        }
    }

    fn accept(editor: &mut Editor, query: &str, backward: bool) {
        editor
            .execute(
                if backward {
                    "search_backward"
                } else {
                    "search_forward"
                },
                1,
            )
            .unwrap();
        editor.update_search(query).unwrap();
        editor.execute("search_accept", 1).unwrap();
    }

    #[test]
    fn select_split_and_filter_preview_the_original_ranges_and_keep_text_untouched() {
        let mut editor = Editor::new(Document::from("one, TWO; three\r\nfour"));
        editor.execute("select_all", 1).unwrap();
        let original = editor.selections.clone();
        editor.execute("select_regex", 1).unwrap();
        editor.update_search(r"\w+").unwrap();
        assert_eq!(
            editor.selections.ranges(),
            &[range(0, 3), range(5, 8), range(10, 15), range(17, 21)]
        );
        editor.update_search("two").unwrap();
        assert_eq!(editor.selections.ranges(), &[range(5, 8)]);
        editor.update_search("Two").unwrap();
        assert_eq!(editor.search_status(), Some(SearchStatus::NoMatch));
        assert_eq!(editor.selections, original);
        editor.execute("search_cancel", 1).unwrap();
        editor.execute("split_selection", 1).unwrap();
        editor.update_search(r"[,;\s]+").unwrap();
        editor.execute("search_accept", 1).unwrap();
        let words = editor.selections.clone();
        assert_eq!(words.ranges().len(), 4);
        editor.execute("keep_selections", 1).unwrap();
        editor.update_search("o").unwrap();
        assert_eq!(
            editor.selections.ranges(),
            &[range(0, 3), range(5, 8), range(17, 21)]
        );
        editor.update_search("z").unwrap();
        assert_eq!(editor.selections, words);
        assert_eq!(editor.execute("search_accept", 1), Err(Error::NoMatch));
        editor.execute("search_cancel", 1).unwrap();
        editor.execute("remove_selections", 1).unwrap();
        editor.update_search("o").unwrap();
        assert_eq!(editor.selections.ranges(), &[range(10, 15)]);
        editor.execute("search_accept", 1).unwrap();
        assert_eq!(editor.document.revision().get(), 0);
        assert_eq!(editor.document.undo_depth(), 0);
    }

    #[test]
    fn selection_regex_anchors_see_document_context_and_normalize_unicode() {
        let mut editor = Editor::new(Document::from("xx e\u{301} yy\r\nlast\r\n"));
        editor
            .set_selections(SelectionSet::single(range(3, 8)))
            .unwrap();
        editor.execute("select_regex", 1).unwrap();
        editor.update_search("^|$").unwrap();
        assert_eq!(editor.search_status(), Some(SearchStatus::NoMatch));
        editor.update_search(r"\p{M}").unwrap();
        assert_eq!(editor.selections.primary(), range(3, 5));
        editor.execute("search_cancel", 1).unwrap();
        editor.execute("select_all", 1).unwrap();
        editor.execute("select_regex", 1).unwrap();
        editor.update_search("^.*$").unwrap();
        assert_eq!(editor.selections.ranges(), &[range(0, 8), range(10, 14)]);
        editor.execute("search_cancel", 1).unwrap();
        editor.execute("split_selection", 1).unwrap();
        editor.update_search(r"\r\n").unwrap();
        assert_eq!(editor.selections.ranges(), &[range(0, 8), range(10, 14)]);
    }

    #[test]
    fn invalid_patterns_keep_the_prompt_editable_even_after_early_acceptance() {
        let mut editor = Editor::new(Document::from("one two one"));
        accept(&mut editor, "one", false);
        let original = editor.selections.clone();
        editor.set_background_search(true);
        editor.execute("select_regex", 1).unwrap();
        editor.update_search("[").unwrap();
        editor.execute("search_accept", 1).unwrap();
        let result = editor.take_search_job().unwrap().run().unwrap();
        assert!(matches!(
            editor.apply_search_result(result),
            Err(Error::InvalidRegex(_))
        ));
        assert_eq!(editor.search_status(), Some(SearchStatus::Invalid));
        assert_eq!(editor.search_prompt(), Some(SearchPrompt::Select));
        assert_eq!(editor.selections, original);
        assert!(!editor.search_waiting());
        editor.update_search("[o]").unwrap();
        editor.execute("search_accept", 1).unwrap();
        let result = editor.take_search_job().unwrap().run().unwrap();
        assert_eq!(
            editor.apply_search_result(result).unwrap(),
            SearchCompletion::Accepted
        );
        assert_eq!(editor.selections.primary(), range(8, 9));
    }

    #[test]
    fn star_remembers_escaped_fragments_with_boundaries_without_moving() {
        let mut editor = Editor::new(Document::from("cat catfish CAT cat a.b axb a.b"));
        editor
            .set_selections(SelectionSet::single(range(0, 3)))
            .unwrap();
        let original = editor.selections.clone();
        let mut keys = KeyHandler::default();
        keys.handle(&mut editor, Key::Char('*')).unwrap();
        assert_eq!(editor.selections, original);
        editor.execute("search_next", 1).unwrap();
        assert_eq!(editor.selections.primary(), range(12, 15));
        editor.execute("search_next", 1).unwrap();
        assert_eq!(editor.selections.primary(), range(16, 19));
        editor
            .set_selections(SelectionSet::single(range(20, 23)))
            .unwrap();
        keys.handle(&mut editor, Key::Char('*')).unwrap();
        editor.execute("search_next", 1).unwrap();
        assert_eq!(editor.selections.primary(), range(28, 31));
    }

    #[test]
    fn selection_queries_obey_the_same_mailbox_and_stale_result_rules() {
        for command in ["select_regex", "split_selection", "keep_selections"] {
            let mut editor = Editor::new(Document::from("one two three"));
            editor.execute("select_all", 1).unwrap();
            editor.set_background_search(true);
            editor.execute(command, 1).unwrap();
            editor.update_search(r"\w+").unwrap();
            let old = editor.take_search_job().unwrap().run().unwrap();
            editor.update_search("o").unwrap();
            assert_eq!(
                editor.apply_search_result(old).unwrap(),
                SearchCompletion::Ignored
            );
            let job = editor.take_search_job().unwrap();
            editor.execute("search_cancel", 1).unwrap();
            assert!(job.run().is_none());
            assert_eq!(editor.selections.primary(), range(0, 13));
        }
    }

    #[test]
    fn previews_always_start_at_the_origin_and_cancel_restores_columns() {
        let mut editor = Editor::new(Document::from("x cat cater"));
        editor.preferred_columns = Some(vec![12]);
        let origin = editor.selections.clone();
        commands::search_forward(&mut CommandContext::new(&mut editor)).unwrap();
        for (query, expected) in [
            ("c", range(2, 3)),
            ("ca", range(2, 4)),
            ("cater", range(6, 11)),
            ("cat", range(2, 5)),
        ] {
            editor.update_search(query).unwrap();
            assert_eq!(editor.selections.primary(), expected);
        }
        editor.update_search("missing").unwrap();
        assert_eq!(editor.search_status(), Some(SearchStatus::NoMatch));
        assert_eq!(editor.selections, origin);
        assert_eq!(editor.execute("search_accept", 1), Err(Error::NoMatch));
        assert!(editor.search_direction().is_some());
        editor.update_search("cat").unwrap();
        commands::search_cancel(&mut CommandContext::new(&mut editor)).unwrap();
        assert_eq!(editor.selections, origin);
        assert_eq!(editor.preferred_columns, Some(vec![12]));
        assert_eq!(editor.execute("search_next", 1), Err(Error::NoSearch));
    }

    #[test]
    fn searches_start_past_selections_and_huge_counts_wrap_without_count_loops() {
        let mut editor = Editor::new(Document::from("ababa"));
        accept(&mut editor, "aba", false);
        assert_eq!(editor.selections.primary(), range(2, 5));
        editor.execute("search_next", 1).unwrap();
        assert_eq!(editor.selections.primary(), range(0, 3));
        editor.execute("search_next", usize::MAX).unwrap();
        assert_eq!(editor.selections.primary(), range(0, 3));
        editor.execute("search_previous", usize::MAX).unwrap();
        assert_eq!(editor.selections.primary(), range(0, 3));
        assert_eq!(editor.document.revision().get(), 0);
        assert_eq!(editor.document.undo_depth(), 0);
    }

    #[test]
    fn n_always_searches_forward_and_n_uppercase_backward_preserving_direction() {
        let mut editor = Editor::new(Document::from("cat x cat x cat"));
        editor.execute("goto_file_end", 1).unwrap();
        accept(&mut editor, "cat", true);
        assert_eq!(editor.selections.primary(), range(12, 15));
        editor.execute("search_next", 1).unwrap();
        assert_eq!(editor.selections.primary(), range(0, 3));
        editor.execute("search_previous", 1).unwrap();
        assert_eq!(editor.selections.primary(), range(12, 15));
        editor
            .set_selections(SelectionSet::single(range(15, 12)))
            .unwrap();
        editor.execute("search_previous", 2).unwrap();
        assert_eq!(editor.selections.primary(), range(3, 0));
        editor.execute("search_next", 3).unwrap();
        assert_eq!(editor.selections.primary(), range(3, 0));
    }

    #[test]
    fn substring_matches_expand_to_graphemes_and_repeats_make_progress() {
        let mut editor = Editor::new(Document::from("e\u{301}\u{301} e\u{301} 🦀"));
        accept(&mut editor, "\u{301}", false);
        assert_eq!(editor.selections.primary(), range(4, 6));
        editor.execute("search_next", 1).unwrap();
        assert_eq!(editor.selections.primary(), range(0, 3));
        editor.execute("search_next", 3).unwrap();
        assert_eq!(editor.selections.primary(), range(4, 6));
        editor.execute("search_previous", 1).unwrap();
        assert_eq!(editor.selections.primary(), range(0, 3));
        accept(&mut editor, "🦀", false);
        assert_eq!(editor.selections.primary(), range(7, 8));
        editor.execute("search_next", usize::MAX).unwrap();
        assert_eq!(editor.selections.primary(), range(7, 8));
    }

    #[test]
    fn normal_search_moves_only_primary_and_select_mode_adds_matches() {
        let mut editor = Editor::new(Document::from("x aa x aa x aa"));
        let original = SelectionSet::new(vec![range(1, 0), range(7, 8)], 1).unwrap();
        editor.set_selections(original).unwrap();
        accept(&mut editor, "aa", false);
        assert_eq!(editor.selections.ranges(), &[range(1, 0), range(12, 14)]);
        editor.execute("select_mode", 1).unwrap();
        editor.execute("search_next", 2).unwrap();
        assert_eq!(
            editor.selections.ranges(),
            &[range(1, 0), range(2, 4), range(7, 9), range(12, 14)]
        );
        assert_eq!(editor.selections.primary(), range(7, 9));
        editor.execute("search_previous", 1).unwrap();
        assert_eq!(editor.selections.ranges().len(), 4);
        assert_eq!(editor.selections.primary(), range(2, 4));
        assert_eq!(editor.mode, Mode::Select);
        editor.execute("search_next", usize::MAX).unwrap();
        assert_eq!(editor.selections.ranges().len(), 4);
        assert_eq!(editor.selections.primary(), range(2, 4));
    }

    #[test]
    fn empty_and_cancelled_searches_preserve_the_accepted_pattern() {
        let mut editor = Editor::new(Document::from("a b a"));
        accept(&mut editor, "a", false);
        editor.execute("search_backward", 1).unwrap();
        editor.update_search("b").unwrap();
        editor.update_search("").unwrap();
        assert_eq!(editor.search_status(), Some(SearchStatus::Empty));
        editor.execute("search_accept", 1).unwrap();
        editor.execute("search_next", 1).unwrap();
        assert_eq!(editor.selections.primary(), range(0, 1));
        editor.execute("search_backward", 1).unwrap();
        editor.update_search("b").unwrap();
        editor.execute("search_cancel", 1).unwrap();
        editor.execute("search_next", 1).unwrap();
        assert_eq!(editor.selections.primary(), range(4, 5));
        let mut empty = Editor::new(Document::default());
        empty.execute("search_forward", 1).unwrap();
        empty.update_search("x").unwrap();
        assert_eq!(empty.search_status(), Some(SearchStatus::NoMatch));
        empty.execute("search_cancel", 1).unwrap();
    }

    #[test]
    fn accepted_searches_follow_edits_undo_redo_without_stale_match_caches() {
        let mut editor = Editor::new(Document::from("cat cat"));
        accept(&mut editor, "cat", false);
        editor.execute("delete_selection", 1).unwrap();
        editor.execute("search_next", 1).unwrap();
        assert_eq!(editor.selections.primary(), range(0, 3));
        editor.execute("delete_selection", 1).unwrap();
        let selections = editor.selections.clone();
        assert_eq!(editor.execute("search_next", 1), Err(Error::NoMatch));
        assert_eq!(editor.selections, selections);
        editor.execute("undo", 2).unwrap();
        let snapshot = editor.document.snapshot();
        editor.execute("search_next", 1).unwrap();
        assert_eq!(editor.selections.primary(), range(0, 3));
        assert_eq!(editor.document.revision(), snapshot.revision());
        assert!(editor.document.text().is_instance(snapshot.text()));
        editor.execute("redo", 2).unwrap();
        assert_eq!(editor.document.text(), " ");
    }

    #[test]
    fn stale_previews_never_restore_coordinates_after_an_external_edit() {
        for action in ["search_update", "search_accept", "search_cancel"] {
            let mut editor = Editor::new(Document::from("abc abc"));
            editor.execute("search_forward", 1).unwrap();
            editor.update_search("abc").unwrap();
            editor.execute("delete_selection", 1).unwrap();
            let before = editor.selections.clone();
            let mut context = CommandContext::new(&mut editor);
            context.text = Some("abc");
            assert_eq!(
                (commands::find(action).unwrap().run)(&mut context),
                Err(Error::SearchChanged)
            );
            assert_eq!(editor.selections, before);
            assert_eq!(editor.search_direction(), None);
        }
    }

    #[test]
    fn search_bindings_use_documented_functions_and_preserve_counts() {
        let mut editor = Editor::new(Document::from("a b a b a"));
        let mut keys = KeyHandler::default();
        for key in "2/".chars() {
            keys.handle(&mut editor, Key::Char(key)).unwrap();
        }
        editor.update_search("a").unwrap();
        assert_eq!(editor.selections.primary(), range(8, 9));
        editor.execute("search_accept", 1).unwrap();
        for key in "2n".chars() {
            keys.handle(&mut editor, Key::Char(key)).unwrap();
        }
        assert_eq!(editor.selections.primary(), range(4, 5));
        let mut map = Keymap::empty();
        map.bind(Mode::Normal, vec![Key::Char('s')], "search_backward")
            .unwrap();
        KeyHandler::new(map)
            .handle(&mut editor, Key::Char('s'))
            .unwrap();
        assert_eq!(editor.search_direction(), Some(SearchDirection::Backward));
        assert!(
            commands::find("search_backward")
                .unwrap()
                .description()
                .contains("regex")
        );
        editor.execute("search_cancel", 1).unwrap();
        editor.execute("goto_file_start", 1).unwrap();
        editor.execute("insert_mode", 1).unwrap();
        for key in "/?nN".chars() {
            keys.handle(&mut editor, Key::Char(key)).unwrap();
        }
        assert!(editor.document.text().to_string().starts_with("/?nN"));
    }

    proptest! {
        #[test]
        fn counted_navigation_agrees_with_repeated_flat_searches(
            source in "[abc ]{0,100}", query in "[abc ]{1,5}",
            position in any::<usize>(), count in 1usize..200, backward in any::<bool>(),
        ) {
            let mut editor = Editor::new(Document::from(source.as_str()));
            let position = position % (source.len() + 1);
            editor.set_selections(SelectionSet::single(Selection::cursor(CharOffset(position)))).unwrap();
            let original = editor.selections.clone();
            let mut expected = original.primary();
            let mut matched = false;
            for _ in 0..count {
                let found = if backward {
                    source[..expected.start().0].match_indices(&query).last()
                        .or_else(|| source.match_indices(&query).last())
                        .map(|(i, _)| i)
                } else {
                    source[expected.end().0..].find(&query).map(|i| i + expected.end().0)
                        .or_else(|| source.find(&query))
                };
                if let Some(at) = found {
                    matched = true;
                    expected = range(at, at + query.len());
                } else { break; }
            }
            editor.execute(if backward { "search_backward" } else { "search_forward" }, count).unwrap();
            editor.update_search(&query).unwrap();
            if matched {
                prop_assert_eq!(editor.selections.primary(), expected);
            } else {
                prop_assert_eq!(editor.search_status(), Some(SearchStatus::NoMatch));
                prop_assert_eq!(editor.selections(), &original);
            }
        }
    }
}
