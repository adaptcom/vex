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

use std::num::NonZeroUsize;
use vex_core::{CharOffset, Document, Selection, SelectionSet, grapheme, motion};

pub mod commands;
mod error;
mod keymap;

pub use commands::{Command, CommandContext};
pub use error::Error;
pub use keymap::{Binding, Dispatch, Key, KeyHandler, Keymap};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mode {
    #[default]
    Normal,
    Select,
    Insert,
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
    pub fn tab_width(&self) -> NonZeroUsize {
        self.tab_width
    }

    pub fn set_tab_width(&mut self, width: NonZeroUsize) {
        self.tab_width = width;
        self.preferred_columns = None;
    }

    /// Install selections after checking bounds and snapping endpoints outward
    /// to whole graphemes. Insert mode collapses them to carets at their heads.
    pub fn set_selections(&mut self, selections: SelectionSet) -> Result<(), Error> {
        let selections = self.normalized(selections, self.mode)?;
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

    /// Insert a text event or paste as one transaction at every insert caret.
    pub fn insert_text(&mut self, text: &str) -> Result<(), Error> {
        let mut context = CommandContext::new(self);
        context.text = Some(text);
        commands::insert_text(&mut context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
