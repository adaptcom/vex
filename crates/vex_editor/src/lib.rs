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

use std::{cell::RefCell, num::NonZeroUsize};
use vex_core::layout::LayoutCache;
use vex_core::{CharOffset, Document, Selection, SelectionSet, grapheme, motion};

pub mod background;
pub mod commands;
mod error;
mod keymap;
mod search;
mod syntax;

pub use commands::{Command, CommandContext};
pub use error::Error;
pub use keymap::{Binding, Dispatch, Key, KeyHandler, KeyHints, Keymap};
pub use search::{SearchCancellation, SearchCompletion, SearchJob, SearchResult, SearchStatus};
pub use syntax::{SyntaxJob, SyntaxResult, SyntaxWorker};
pub use vex_core::search::Direction as SearchDirection;
pub use vex_syntax::{Highlight, HighlightSpan, Language};

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
    JumpBack,
    NextDiagnostic(usize),
    PreviousDiagnostic(usize),
}

/// Application UI requested by documented commands without performing file I/O.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplicationAction {
    FilePicker,
}

/// The first editor model: one document and its active view, independent of a TTY.
/// All selections stay on grapheme boundaries. Insert mode owns zero-width
/// carets; other modes allow directional ranges and a zero-width EOF cursor.
#[derive(Debug)]
pub struct Editor {
    document: Document,
    selections: SelectionSet,
    mode: Mode,
    preferred_columns: Option<Vec<usize>>,
    tab_width: NonZeroUsize,
    layout: RefCell<LayoutCache>,
    syntax: RefCell<syntax::Highlighting>,
    search: search::Search,
    language_action: Option<LanguageAction>,
    application_action: Option<ApplicationAction>,
}

impl Editor {
    pub fn new(document: Document) -> Self {
        let selections = SelectionSet::single(
            motion::block(document.text(), CharOffset(0)).expect("BOF is valid"),
        );
        Self {
            document,
            selections,
            mode: Mode::Normal,
            preferred_columns: None,
            tab_width: NonZeroUsize::new(4).unwrap(),
            layout: RefCell::default(),
            syntax: RefCell::default(),
            search: search::Search::default(),
            language_action: None,
            application_action: None,
        }
    }

    pub fn document(&self) -> &Document {
        &self.document
    }
    pub fn selections(&self) -> &SelectionSet {
        &self.selections
    }
    pub fn mode(&self) -> Mode {
        self.mode
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

    /// Direction of an active search prompt, if a search command requested one.
    pub fn search_direction(&self) -> Option<SearchDirection> {
        self.search
            .preview
            .as_ref()
            .map(|preview| preview.direction)
    }

    pub fn search_status(&self) -> Option<SearchStatus> {
        self.search.preview.as_ref().map(|preview| preview.status)
    }

    /// Use deferred search commands. The frontend must take each job, run it
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

    /// An accepted preview or repeat is waiting for its destination. Frontends
    /// should defer subsequent editing keys, but keep cancellation responsive.
    pub fn search_waiting(&self) -> bool {
        self.search.waiting()
    }

    /// Preview literal matches from the selections saved by search_forward or
    /// search_backward. The empty query restores those selections.
    pub fn update_search(&mut self, text: &str) -> Result<(), Error> {
        let mut context = CommandContext::new(self);
        context.text = Some(text);
        commands::search_update(&mut context)
    }
    pub fn tab_width(&self) -> NonZeroUsize {
        self.tab_width
    }

    pub fn set_tab_width(&mut self, width: NonZeroUsize) {
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
        self.search.invalidate();
        self.layout.get_mut().synchronize(&self.document);
        self.syntax.get_mut().synchronize(&self.document);
    }

    // Every text command goes through here, including each event in a batch.
    // Otherwise several edits before drawing would skip cache revisions.
    fn apply(&mut self, transaction: vex_core::Transaction, grouped: bool) -> Result<(), Error> {
        if grouped {
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
    pub fn finish_undo_group(&mut self) {
        self.search.invalidate();
        self.document.finish_undo_group();
    }

    /// Install selections after checking bounds and snapping endpoints outward
    /// to whole graphemes. Insert mode collapses them to carets at their heads.
    pub fn set_selections(&mut self, selections: SelectionSet) -> Result<(), Error> {
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
    /// A zero count uses the default count of one.
    pub fn execute(&mut self, name: &str, count: usize) -> Result<(), Error> {
        let command = commands::find(name).ok_or_else(|| Error::UnknownCommand(name.into()))?;
        let mut context = CommandContext::new(self);
        context.count = NonZeroUsize::new(count).unwrap_or(NonZeroUsize::MIN);
        (command.run)(&mut context)
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

#[cfg(test)]
mod tests {
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
