//! Modes, documented commands, and terminal-independent input dispatch for Vex.
//!
//! ```
//! use vex_core::Document;
//! use vex_editor::{Editor, Key, KeyHandler};
//!
//! let mut editor = Editor::new(Document::from("hello world"));
//! let mut keys = KeyHandler::default();
//! keys.handle(&mut editor, Key::Char('w'))?;
//! keys.handle(&mut editor, Key::Char('d'))?;
//! assert_eq!(editor.document().text(), "world");
//! editor.execute("undo", 1)?;
//! assert_eq!(editor.document().text(), "hello world");
//! # Ok::<(), vex_editor::Error>(())
//! ```

use std::fmt::Debug;
use std::{cell::RefCell, num::NonZeroUsize};
use vex_core::layout::LayoutCache;
use vex_core::{CharOffset, Document, Selection, SelectionSet, grapheme, motion};

pub mod background;
pub mod commands;
mod comments;
mod editing;
mod error;
mod keymap;
mod register;
mod repeat;
mod search;
mod selection;
mod surround;
mod syntax;
mod textobject;
mod views;

pub use commands::{Command, CommandContext, CommandInput};
pub use error::Error;
pub use keymap::{Binding, Dispatch, Key, KeyHandler, KeyHints, Keymap, Modifier, NamedKey};
pub use register::{Paste, PastePlan, RegisterValues, YankRegister};
pub use repeat::Session;
pub use search::{
    SearchCancellation, SearchCompletion, SearchJob, SearchPrompt, SearchResult, SearchStatus,
};
pub use syntax::{SyntaxJob, SyntaxResult, SyntaxWorker};
pub use vex_core::search::Direction as SearchDirection;
pub use vex_syntax::{Highlight, HighlightSpan, IndentStyle, Indentation, Language};
pub use views::ViewId;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mode {
    #[default]
    Normal,
    Select,
    Insert,
}

/// Frontend services requested by ordinary documented editing commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LanguageAction {
    Hover,
    Definition,
    Completion,
    NextDiagnostic(usize),
    PreviousDiagnostic(usize),
}

/// Application UI requested by documented commands without performing file I/O.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplicationAction {
    Clipboard(ClipboardKind, ClipboardAction, usize),
    ClipboardWrite(ClipboardKind, std::sync::Arc<str>, bool),
    SaveSelection,
    RecordJump(std::sync::Arc<SelectionSet>),
    Jump { forward: bool, count: usize },
    GitStatus,
    FilePicker,
    BufferPicker,
    JumpPicker,
    LastPicker,
    GlobalSearch(char),
    Buffer(BufferAction, usize),
    DocumentSymbols,
    WorkspaceSymbols,
    HalfPageUp(usize),
    HalfPageDown(usize),
    Window(WindowAction, usize),
}

/// System clipboard operations interpreted by a frontend's background service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardAction {
    Yank,
    YankMain,
    Delete,
    Change,
    Search { reverse: bool },
    SetSearch { register: char, activate: bool },
    Paste(Paste),
}

/// The system clipboard (+) and the separate primary selection (*) where supported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardKind {
    System,
    Primary,
}

impl ClipboardKind {
    pub fn from_register(name: char) -> Option<Self> {
        match name {
            '+' => Some(Self::System),
            '*' => Some(Self::Primary),
            _ => None,
        }
    }

    pub fn register(self) -> char {
        match self {
            Self::System => '+',
            Self::Primary => '*',
        }
    }
}

/// Buffer navigation interpreted by the frontend, without reading files here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferAction {
    LastAccessed,
    LastModified,
    Next,
    Previous,
    OpenSelected,
}

/// Window operations interpreted by the frontend's split layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowAction {
    SplitVertical,
    SplitHorizontal,
    Rotate,
    FocusLeft,
    FocusDown,
    FocusUp,
    FocusRight,
    SwapLeft,
    SwapDown,
    SwapUp,
    SwapRight,
    Close,
    Only,
    OpenHorizontal,
    OpenVertical,
}

/// One shared document and its views, independent of a TTY.
/// All selections stay on grapheme boundaries. Insert mode owns zero-width
/// carets; other modes allow directional ranges and a zero-width EOF cursor.
#[derive(Debug)]
pub struct Editor {
    document: Document,
    yank_register: YankRegister,
    selected_register: Option<char>,
    display_name: std::sync::Arc<str>,
    recorder: repeat::Recorder,
    selections: SelectionSet,
    mode: Mode,
    preferred_columns: Option<Vec<usize>>,
    tab_width: NonZeroUsize,
    indent_style: IndentStyle,
    newline: &'static str,
    layout: RefCell<LayoutCache>,
    syntax: RefCell<syntax::Highlighting>,
    search: search::Search,
    replacement: Option<surround::Replacement>,
    language_action: Option<LanguageAction>,
    application_action: Option<ApplicationAction>,
    views: views::Views,
}

impl Editor {
    pub fn new(document: Document) -> Self {
        Self::with_yank_register(document, YankRegister::default())
    }

    /// Create a document editor using a session's shared internal yank register.
    /// Cloning the handle shares text across buffers without copying it.
    pub fn with_yank_register(document: Document, yank_register: YankRegister) -> Self {
        Self::with_session(document, Session::with_yank_register(yank_register))
    }

    /// Create a buffer sharing registers and insert history with other buffers.
    pub fn with_session(document: Document, session: Session) -> Self {
        let newline = line_ending(document.text());
        let selections = SelectionSet::single(
            motion::block(document.text(), CharOffset(0)).expect("BOF is valid"),
        );
        let views = views::Views::new(document.revision());
        Self {
            document,
            yank_register: session.yank.clone(),
            selected_register: None,
            display_name: "[scratch]".into(),
            recorder: repeat::Recorder::new(session),
            selections,
            mode: Mode::Normal,
            preferred_columns: None,
            tab_width: NonZeroUsize::new(4).unwrap(),
            indent_style: Indentation::default().style,
            newline,
            layout: RefCell::default(),
            syntax: RefCell::default(),
            search: search::Search::default(),
            replacement: None,
            language_action: None,
            application_action: None,
            views,
        }
    }

    pub fn document(&self) -> &Document {
        &self.document
    }

    /// Share this editor's register with another buffer in the same session.
    pub fn yank_register(&self) -> YankRegister {
        self.yank_register.clone()
    }
    pub fn selections(&self) -> &SelectionSet {
        &self.selections
    }
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Line ending detected when the document was loaded, defaulting to LF.
    /// Retained through edits so removing all line breaks does not change it.
    pub fn newline(&self) -> &'static str {
        self.newline
    }

    /// Take a language-service action after command dispatch. The frontend owns
    /// transport and applies results; commands remain independent of LSP and I/O.
    pub fn take_language_action(&mut self) -> Option<LanguageAction> {
        self.language_action.take()
    }

    /// Take an application action after dispatch; the frontend owns its UI.
    pub fn take_application_action(&mut self) -> Option<ApplicationAction> {
        self.application_action.take()
    }

    fn request_language_action(&mut self, action: LanguageAction) {
        self.recorder.service = true;
        self.language_action = Some(action);
    }

    fn request_application_action(&mut self, action: ApplicationAction) {
        self.recorder.service = true;
        self.application_action = Some(action);
    }

    pub(crate) fn request_clipboard(
        &mut self,
        kind: ClipboardKind,
        action: ClipboardAction,
        count: usize,
    ) {
        self.request_application_action(ApplicationAction::Clipboard(kind, action, count));
        repeat::request_clipboard(self, action, count);
    }

    pub(crate) fn request_clipboard_search_write(
        &mut self,
        kind: ClipboardKind,
        text: std::sync::Arc<str>,
        activate: bool,
    ) {
        self.request_application_action(ApplicationAction::ClipboardWrite(kind, text, activate));
        repeat::request_clipboard(
            self,
            ClipboardAction::SetSearch {
                register: kind.register(),
                activate,
            },
            1,
        );
    }

    /// The active regex prompt requested by an editor command.
    pub fn search_prompt(&self) -> Option<SearchPrompt> {
        self.search
            .preview
            .as_ref()
            .map(|preview| preview.operation)
    }

    pub fn search_direction(&self) -> Option<SearchDirection> {
        match self.search_prompt()? {
            SearchPrompt::Forward => Some(SearchDirection::Forward),
            SearchPrompt::Backward => Some(SearchDirection::Backward),
            _ => None,
        }
    }

    /// Register selected for the active search prompt, including selection filters.
    pub fn search_prompt_register(&self) -> Option<char> {
        self.search.preview.as_ref().map(|preview| preview.register)
    }

    /// Remember a validated workspace query for subsequent n/N navigation.
    /// Clipboard registers request the frontend's guarded background write.
    pub fn remember_search_query(
        &mut self,
        register: char,
        query: std::sync::Arc<str>,
    ) -> Result<(), Error> {
        search::writable_query(register)?;
        if let Some(kind) = ClipboardKind::from_register(register) {
            self.request_clipboard_search_write(kind, query, true);
            Ok(())
        } else {
            self.yank_register.remember_search(register, query, true)
        }
    }

    pub fn search_status(&self) -> Option<SearchStatus> {
        self.search.preview.as_ref().map(|preview| preview.status)
    }

    pub fn search_error(&self) -> Option<&Error> {
        self.search.preview.as_ref()?.error.as_ref()
    }

    /// Use deferred search and selection-scan commands. The frontend must take each job, run it
    /// off the UI thread, and deliver its result via apply_search_result.
    /// The default remains synchronous for standalone editor integrations.
    pub fn set_background_search(&mut self, enabled: bool) {
        self.search.invalidate();
        self.search.background = enabled;
    }

    /// Take the latest deferred request after command dispatch. Superseded jobs
    /// already handed to a worker observe cancellation through their token.
    pub fn take_search_job(&mut self) -> Option<SearchJob> {
        self.search.outgoing.take()
    }

    /// Apply a completion on the editor's owning thread, ignoring stale work.
    pub fn apply_search_result(&mut self, result: SearchResult) -> Result<SearchCompletion, Error> {
        search::apply_result(self, result)
    }

    /// Whether the latest request is queued or running.
    pub fn search_pending(&self) -> bool {
        self.search.pending()
    }

    /// Short status for the pending text-search or selection scan.
    pub fn search_progress(&self) -> Option<&'static str> {
        self.search.progress()
    }

    /// An accepted preview, repeat, or selection scan is waiting for its result. Frontends
    /// should defer subsequent editing keys, but keep cancellation responsive.
    pub fn search_waiting(&self) -> bool {
        self.search.waiting()
    }

    /// Preview a regex operation from its saved selections. An empty query
    /// restores those selections.
    pub fn update_search(&mut self, text: &str) -> Result<(), Error> {
        let mut context = CommandContext::new(self);
        context.text = Some(text);
        commands::search_update(&mut context)
    }
    pub fn tab_width(&self) -> NonZeroUsize {
        self.tab_width
    }

    pub fn indentation(&self) -> Indentation {
        Indentation {
            style: self.indent_style,
            tab_width: self.tab_width,
        }
    }

    /// Override this buffer's indentation and tab display width. Changing to a
    /// different language restores that language's defaults; text is untouched.
    pub fn set_indentation(&mut self, indentation: Indentation) {
        self.indent_style = indentation.style;
        self.set_tab_width(indentation.tab_width);
    }

    pub fn set_tab_width(&mut self, width: NonZeroUsize) {
        if self.tab_width != width {
            self.search.invalidate_columns();
        }
        self.tab_width = width;
        self.preferred_columns = None;
    }

    /// Cached display column, using the same width conventions as vertical motion.
    /// Filling derived layout data does not mutate the document or selections.
    pub fn display_column(&self, position: CharOffset) -> Result<usize, vex_core::Error> {
        self.layout
            .borrow_mut()
            .column(&self.document, position, self.tab_width)
    }

    /// Locate a display column in the logical line containing `position`.
    /// Returns its grapheme boundary and actual column, clamping at line end.
    pub fn position_at_column(
        &self,
        position: CharOffset,
        column: usize,
    ) -> Result<(CharOffset, usize), vex_core::Error> {
        self.layout
            .borrow_mut()
            .at_column(&self.document, position, column, self.tab_width)
    }

    fn synchronize_caches(&mut self) {
        self.replacement = None;
        self.synchronize_views();
        self.search.invalidate();
        self.layout.get_mut().synchronize(&self.document);
        self.syntax.get_mut().synchronize(&self.document);
    }

    // Every text command goes through here, including each event in a batch.
    // Otherwise several edits before drawing would skip cache revisions.
    fn apply(&mut self, transaction: vex_core::Transaction, grouped: bool) -> Result<(), Error> {
        if grouped || self.recorder.stepping {
            self.document
                .apply_grouped(transaction, &mut self.selections)?;
        } else {
            self.document.apply(transaction, &mut self.selections)?;
        }
        self.synchronize_caches();
        Ok(())
    }

    /// Separate subsequent typing from the current undo step. Integrations must
    /// call this at savepoints so undo can return to the saved text. Movements,
    /// mode/selection changes, explicit edits, paste, and undo/redo do so already.
    /// Outside a replayed action, this also cancels pending clipboard commands
    /// and insert playback, even when a later view/cursor change returns here.
    pub fn finish_undo_group(&mut self) {
        self.selected_register = None;
        if !self.recorder.stepping {
            self.cancel_clipboard_command();
        }
        surround::cancel(self);
        self.search.invalidate();
        if !self.recorder.stepping {
            self.document.finish_undo_group();
        }
    }

    /// Accept a completion and its additional edits as one undo step. All edits
    /// use the current revision's coordinates; overlaps fail before any change.
    /// Initial completion supports a single insertion caret.
    pub fn apply_completion(
        &mut self,
        edit: vex_core::Edit,
        additional: Vec<vex_core::Edit>,
    ) -> Result<(), Error> {
        if self.mode != Mode::Insert {
            return Err(Error::WrongMode {
                expected: Mode::Insert,
                actual: self.mode,
            });
        }
        let cursor = self.selections.primary().head;
        if self.selections.ranges().len() != 1
            || edit.range().start > cursor
            || edit.range().end < cursor
        {
            return Err(Error::InvalidCompletion);
        }
        let start = edit.range().start;
        let recorded = edit.clone();
        let transaction = self
            .document
            .transaction(std::iter::once(edit).chain(additional))?;
        let caret = transaction.map_position(start, vex_core::Affinity::After)?;
        let transaction =
            transaction.with_selections(SelectionSet::single(Selection::cursor(caret)))?;
        self.apply(transaction, false)?;
        self.selections = self.normalized(self.selections.clone(), self.mode)?;
        self.preferred_columns = None;
        self.record_completion(cursor, &recorded);
        Ok(())
    }

    /// Install selections after checking bounds and snapping endpoints outward
    /// to whole graphemes. Insert mode collapses them to carets at their heads.
    pub fn set_selections(&mut self, selections: SelectionSet) -> Result<(), Error> {
        self.cancel_repeat();
        let selections = self.normalized(selections, self.mode)?;
        self.finish_undo_group();
        self.selections = selections;
        self.preferred_columns = None;
        Ok(())
    }

    pub(crate) fn normalized(
        &self,
        selections: SelectionSet,
        mode: Mode,
    ) -> Result<SelectionSet, Error> {
        let text = self.document.text();
        selections.validate(text.len_chars())?;
        let ranges = selections
            .ranges()
            .iter()
            .map(|&selection| {
                if mode == Mode::Insert {
                    Ok(Selection::cursor(grapheme::ceil(text, selection.head)?))
                } else if selection.is_empty() {
                    motion::block(text, selection.head)
                } else {
                    let start = grapheme::floor(text, selection.start())?;
                    let end = grapheme::ceil(text, selection.end())?;
                    Ok(if selection.is_backward() {
                        Selection::new(end, start)
                    } else {
                        Selection::new(start, end)
                    })
                }
            })
            .collect::<Result<Vec<_>, vex_core::Error>>()?;
        Ok(SelectionSet::new(ranges, selections.primary_index())?)
    }

    /// Invoke a documented command by its stable name, without keyboard input.
    /// A zero count is omitted and uses the command's default, usually one.
    pub fn execute(&mut self, name: &str, count: usize) -> Result<(), Error> {
        let command = commands::find(name).ok_or_else(|| Error::UnknownCommand(name.into()))?;
        if command.input != CommandInput::SurroundReplacement {
            self.cancel_surround();
        }
        let mut context = CommandContext::new(self);
        context.count = NonZeroUsize::new(count).unwrap_or(NonZeroUsize::MIN);
        context.count_given = count != 0;
        (command.run)(&mut context)
    }

    /// Cancel a staged surround operation and restore its original selections.
    /// Other text-search work is unaffected.
    pub fn cancel_surround(&mut self) {
        surround::cancel(self);
        self.search.invalidate_surround();
    }

    pub(crate) fn replacing_surround(&self) -> bool {
        self.replacement.is_some() || self.search.preparing_surround()
    }

    /// Insert a text event at every insert caret, continuing the current typing
    /// group. Use [`Self::insert_paste`] for a separate undo step.
    pub fn insert_text(&mut self, text: &str) -> Result<(), Error> {
        let mut context = CommandContext::new(self);
        context.text = Some(text);
        commands::insert_text(&mut context)
    }

    /// Insert pasted text at every insert caret as its own undo step, separating
    /// it from typing on either side. Requires insert mode and preserves the text.
    pub fn insert_paste(&mut self, text: &str) -> Result<(), Error> {
        let mut context = CommandContext::new(self);
        context.text = Some(text);
        commands::insert_paste(&mut context)
    }
}

fn line_ending(text: &vex_core::Rope) -> &'static str {
    let first = text.line(0);
    let len = first.len_chars();
    if len >= 2 && first.char(len - 2) == '\r' && first.char(len - 1) == '\n' {
        "\r\n"
    } else if len >= 1 && first.char(len - 1) == '\r' {
        "\r"
    } else {
        "\n"
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn completion_imports_and_replacement_are_atomic_and_separate_from_typing() {
        use super::*;
        use vex_core::Edit;
        let mut editor = Editor::new(Document::from("// 界\r\nans"));
        editor.execute("insert_mode", 1).unwrap();
        editor
            .set_selections(SelectionSet::single(Selection::cursor(CharOffset(9))))
            .unwrap();
        let before = editor.selections().clone();
        let edit = Edit::new(CharOffset(6)..CharOffset(9), "answer()");
        let import = Edit::insert(CharOffset(0), "use demo::answer;\r\n");
        editor.apply_completion(edit.clone(), vec![import]).unwrap();
        assert_eq!(
            editor.document().text(),
            "use demo::answer;\r\n// 界\r\nanswer()"
        );
        assert_eq!(
            editor.selections().primary().head.0,
            editor.document().text().len_chars()
        );
        editor.insert_text(";").unwrap();
        editor.execute("undo", 1).unwrap();
        assert!(editor.document().text().to_string().ends_with("answer()"));
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document().text(), "// 界\r\nans");
        assert_eq!(editor.selections(), &before);
        editor.execute("redo", 1).unwrap();
        assert!(editor.document().text().to_string().starts_with("use demo"));
        editor.execute("undo", 1).unwrap();
        let revision = editor.document().revision();
        assert!(
            editor
                .apply_completion(edit, vec![Edit::delete(CharOffset(7)..CharOffset(9))])
                .is_err()
        );
        assert_eq!(editor.document().revision(), revision);
        assert_eq!(editor.document().text(), "// 界\r\nans");
    }

    use super::*;
    use vex_core::ByteOffset;

    #[test]
    fn external_selections_snap_outward_and_invalid_ones_do_not_mutate_state() {
        let mut editor = Editor::new(Document::from("ae\u{301}🦀"));
        editor
            .set_selections(SelectionSet::single(Selection::new(
                CharOffset(3),
                CharOffset(2),
            )))
            .unwrap();
        assert_eq!(
            editor.selections().primary(),
            Selection::new(CharOffset(3), CharOffset(1))
        );
        let before = editor.selections().clone();
        assert!(
            editor
                .set_selections(SelectionSet::single(Selection::cursor(CharOffset(5))))
                .is_err()
        );
        assert_eq!(editor.selections(), &before);
        assert!(matches!(
            editor.execute("missing", 1),
            Err(Error::UnknownCommand(_))
        ));
        assert_eq!(editor.document().revision().get(), 0);
    }

    #[test]
    fn insert_text_requires_insert_mode_and_snaps_joined_cluster_carets() {
        let mut editor = Editor::new(Document::from("👩💻"));
        assert!(matches!(
            editor.insert_text("x"),
            Err(Error::WrongMode { .. })
        ));
        editor.execute("append_mode", 1).unwrap();
        editor.insert_text("\u{200d}").unwrap();
        assert_eq!(editor.document().text(), "👩\u{200d}💻");
        assert_eq!(
            editor.selections().primary(),
            Selection::cursor(CharOffset(3))
        );
        editor.execute("commit_undo_checkpoint", 1).unwrap();
        editor.execute("delete_backward", 1).unwrap();
        assert_eq!(editor.document().text(), "");
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document().text(), "👩\u{200d}💻");
        assert_eq!(
            editor.selections().primary(),
            Selection::cursor(CharOffset(3))
        );
    }

    #[test]
    fn external_selection_changes_separate_typing() {
        let mut editor = Editor::new(Document::default());
        editor.execute("insert_mode", 1).unwrap();
        editor.insert_text("a").unwrap();
        editor.insert_text("b").unwrap();
        let before = editor.document().snapshot();
        editor.set_selections(SelectionSet::default()).unwrap();
        editor.insert_text("c").unwrap();
        assert!(
            editor
                .set_selections(SelectionSet::single(Selection::cursor(CharOffset(999))))
                .is_err()
        );
        editor.insert_text("d").unwrap();
        assert_eq!(editor.document().text(), "cdab");
        assert_eq!(editor.document().undo_depth(), 2);
        editor.execute("undo", 1).unwrap();
        assert!(editor.document().text().is_instance(before.text()));
        assert_eq!(
            editor.selections().primary(),
            Selection::cursor(CharOffset(0))
        );
    }

    #[test]
    fn syntax_tracks_batched_typing_counted_history_and_language_changes() {
        let mut editor = Editor::new(Document::from("fn original() {}"));
        editor.set_language(Some(Language::Rust));
        let highlights = |editor: &Editor| {
            editor
                .syntax_highlights(ByteOffset(0)..ByteOffset(editor.document().text().len_bytes()))
        };
        let before = highlights(&editor);
        assert!(!before.is_empty());
        editor.execute("insert_mode", 1).unwrap();
        for text in ["/", "*", "界", "*/"] {
            editor.insert_text(text).unwrap();
        }
        editor.insert_paste("\n").unwrap();
        let after = highlights(&editor);
        assert_eq!(after[0].highlight, Highlight::Comment);
        editor.execute("undo", 2).unwrap();
        assert_eq!(highlights(&editor), before);
        editor.execute("redo", 2).unwrap();
        assert_eq!(highlights(&editor), after);
        let revision = editor.document().revision();
        editor.set_language(None);
        assert!(highlights(&editor).is_empty());
        editor.set_language(Some(Language::Rust));
        assert_eq!(highlights(&editor), after);
        assert_eq!(editor.document().revision(), revision);
    }

    #[test]
    fn cached_motion_tracks_batched_typing_history_tab_width_and_multiple_cursors() {
        let mut editor = Editor::new(Document::from("a\t界e\u{301}\r\n123456789\r\nx\tend"));
        editor.execute("insert_mode", 1).unwrap();
        editor
            .set_selections(
                SelectionSet::new(
                    vec![
                        Selection::cursor(CharOffset(3)),
                        Selection::cursor(CharOffset(13)),
                    ],
                    1,
                )
                .unwrap(),
            )
            .unwrap();
        let verify = |editor: &Editor| {
            for position in 0..=editor.document().text().len_chars() {
                assert_eq!(
                    editor.display_column(CharOffset(position)).unwrap(),
                    motion::column(
                        editor.document().text(),
                        CharOffset(position),
                        editor.tab_width()
                    )
                    .unwrap()
                );
            }
        };
        verify(&editor);
        // Several changes can arrive in a terminal event batch before drawing.
        for text in ["x", "\u{301}", "\n", "\t"] {
            editor.insert_text(text).unwrap();
        }
        verify(&editor);
        assert_eq!(editor.document().undo_depth(), 1);
        editor.execute("undo", 1).unwrap();
        verify(&editor);
        editor.execute("redo", 1).unwrap();
        verify(&editor);
        editor.set_tab_width(NonZeroUsize::new(8).unwrap());
        verify(&editor);
        let before = editor.selections().clone();
        let text = editor.document().text();
        let expected = SelectionSet::new(
            before
                .ranges()
                .iter()
                .map(|s| {
                    let column = motion::column(text, s.head, editor.tab_width()).unwrap();
                    Selection::cursor(
                        motion::vertical(text, s.head, 1, true, column, editor.tab_width())
                            .unwrap(),
                    )
                })
                .collect(),
            before.primary_index(),
        )
        .unwrap();
        editor.execute("move_down", 1).unwrap();
        assert_eq!(editor.selections(), &expected);
    }
}
