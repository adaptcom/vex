//! Named command functions. Their Rust documentation also supplies runtime help.

use std::num::NonZeroUsize;
use vex_core::{Affinity, CharOffset, Selection, SelectionSet, grapheme, motion};

use crate::{Editor, Error, Mode};

pub struct CommandContext<'a> {
    pub editor: &'a mut Editor,
    pub count: NonZeroUsize,
    pub text: Option<&'a str>,
}

impl<'a> CommandContext<'a> {
    pub fn new(editor: &'a mut Editor) -> Self {
        Self {
            editor,
            count: NonZeroUsize::MIN,
            text: None,
        }
    }
}

#[derive(Debug)]
pub struct Command {
    pub name: &'static str,
    pub documentation: &'static str,
    pub run: fn(&mut CommandContext<'_>) -> Result<(), Error>,
}

impl Command {
    pub fn description(&self) -> &'static str {
        self.documentation.trim()
    }
}

/// Return a command's documentation and callable function by its stable name.
pub fn find(name: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|command| command.name == name)
}

// Each doc comment is emitted both as Rustdoc and as command metadata. Adding a
// command here creates an ordinary public function and registers it for help.
macro_rules! commands {
    ($($(#[doc = $doc:literal])+ fn $name:ident($ctx:ident) $body:block)+) => {
        $(
            $(#[doc = $doc])+
            pub fn $name($ctx: &mut CommandContext<'_>) -> Result<(), Error> $body
        )+
        pub static COMMANDS: &[Command] = &[
            $(Command { name: stringify!($name), documentation: concat!($($doc, "\n",)+), run: $name },)+
        ];
    };
}

fn replace_ranges(editor: &mut Editor, ranges: &SelectionSet, text: &str) -> Result<(), Error> {
    let transaction = editor.document.replace_selections(ranges, text)?;
    let after = transaction.map_selections(&editor.selections, Affinity::After)?;
    let transaction = transaction.with_selections(after)?;
    editor.apply(transaction)?;
    editor.preferred_columns = None;
    Ok(())
}

fn normalize(editor: &mut Editor) -> Result<(), Error> {
    editor.selections = editor.normalized(editor.selections.clone(), editor.mode)?;
    editor.preferred_columns = None;
    Ok(())
}

fn at_destination(
    editor: &Editor,
    selection: Selection,
    destination: CharOffset,
) -> Result<Selection, Error> {
    if editor.mode == Mode::Insert {
        Ok(Selection::cursor(destination))
    } else {
        Ok(motion::put_cursor(
            editor.document.text(),
            selection,
            destination,
            editor.mode == Mode::Select,
        )?)
    }
}

fn move_to(
    ctx: &mut CommandContext<'_>,
    destination: impl Fn(&Editor, Selection, usize) -> Result<CharOffset, Error>,
) -> Result<(), Error> {
    let editor = &mut *ctx.editor;
    let ranges = editor
        .selections
        .ranges()
        .iter()
        .map(|&selection| {
            at_destination(
                editor,
                selection,
                destination(editor, selection, ctx.count.get())?,
            )
        })
        .collect::<Result<Vec<_>, Error>>()?;
    editor.selections = SelectionSet::new(ranges, editor.selections.primary_index())?;
    editor.preferred_columns = None;
    Ok(())
}

fn position(editor: &Editor, selection: Selection) -> Result<CharOffset, Error> {
    Ok(if editor.mode == Mode::Insert {
        selection.head
    } else {
        motion::cursor(editor.document.text(), selection)?
    })
}

fn vertical(ctx: &mut CommandContext<'_>, down: bool) -> Result<(), Error> {
    let editor = &mut *ctx.editor;
    let text = editor.document.text();
    let mut columns = Vec::with_capacity(editor.selections.ranges().len());
    let mut ranges = Vec::with_capacity(editor.selections.ranges().len());
    for (index, &selection) in editor.selections.ranges().iter().enumerate() {
        let position = position(editor, selection)?;
        let column = match &editor.preferred_columns {
            Some(columns) => columns[index],
            None => editor.display_column(position)?,
        };
        let line = text.char_to_line(position.0);
        let target = if down {
            line.saturating_add(ctx.count.get())
                .min(text.len_lines() - 1)
        } else {
            line.saturating_sub(ctx.count.get())
        };
        let destination = if line == target {
            position
        } else {
            editor
                .position_at_column(CharOffset(text.line_to_char(target)), column)?
                .0
        };
        columns.push(column);
        ranges.push(at_destination(editor, selection, destination)?);
    }
    let selections = SelectionSet::new(ranges.clone(), editor.selections.primary_index())?;
    // Merged or reordered selections invalidate per-selection desired columns.
    editor.preferred_columns = (selections.ranges() == ranges).then_some(columns);
    editor.selections = selections;
    Ok(())
}

fn word(
    ctx: &mut CommandContext<'_>,
    movement: fn(&vex_core::Rope, Selection, usize) -> Result<Selection, vex_core::Error>,
) -> Result<(), Error> {
    let editor = &mut *ctx.editor;
    let text = editor.document.text();
    let ranges = editor
        .selections
        .ranges()
        .iter()
        .map(|&selection| {
            let moved = movement(text, selection, ctx.count.get())?;
            if editor.mode == Mode::Select {
                Ok(motion::put_cursor(
                    text,
                    selection,
                    motion::cursor(text, moved)?,
                    true,
                )?)
            } else if editor.mode == Mode::Insert {
                Ok(Selection::cursor(moved.head))
            } else {
                Ok(moved)
            }
        })
        .collect::<Result<Vec<_>, Error>>()?;
    editor.selections = SelectionSet::new(ranges, editor.selections.primary_index())?;
    editor.preferred_columns = None;
    Ok(())
}

fn enter_insert(editor: &mut Editor, append: bool) -> Result<(), Error> {
    let ranges = editor
        .selections
        .ranges()
        .iter()
        .map(|s| Selection::cursor(if append { s.end() } else { s.start() }))
        .collect();
    editor.selections = SelectionSet::new(ranges, editor.selections.primary_index())?;
    editor.mode = Mode::Insert;
    editor.preferred_columns = None;
    Ok(())
}

fn erase(ctx: &mut CommandContext<'_>, backward: bool) -> Result<(), Error> {
    let editor = &mut *ctx.editor;
    require_insert(editor)?;
    let ranges = editor
        .selections
        .ranges()
        .iter()
        .map(|selection| {
            let head = selection.head;
            let edge = if backward {
                grapheme::previous(editor.document.text(), head, ctx.count.get())?
            } else {
                grapheme::next(editor.document.text(), head, ctx.count.get())?
            };
            Ok(Selection::new(head, edge))
        })
        .collect::<Result<Vec<_>, vex_core::Error>>()?;
    let ranges = SelectionSet::new(ranges, editor.selections.primary_index())?;
    replace_ranges(editor, &ranges, "")?;
    normalize(editor)
}

fn require_insert(editor: &Editor) -> Result<(), Error> {
    if editor.mode != Mode::Insert {
        return Err(Error::WrongMode {
            expected: Mode::Insert,
            actual: editor.mode,
        });
    }
    Ok(())
}

commands! {
    /// Move right by graphemes, extending the selection in select mode.
    fn move_right(ctx) {
        move_to(ctx, |editor, selection, count| Ok(grapheme::next(editor.document.text(), position(editor, selection)?, count)?))
    }

    /// Move left by graphemes, extending the selection in select mode.
    fn move_left(ctx) {
        move_to(ctx, |editor, selection, count| Ok(grapheme::previous(editor.document.text(), position(editor, selection)?, count)?))
    }

    /// Move down by logical lines, retaining each cursor's desired display column.
    fn move_down(ctx) { vertical(ctx, true) }

    /// Move up by logical lines, retaining each cursor's desired display column.
    fn move_up(ctx) { vertical(ctx, false) }

    /// Select through the next word start; repeat counts span multiple words.
    fn move_word_forward(ctx) { word(ctx, motion::word_forward) }

    /// Select backward to a word start; repeat counts span multiple words.
    fn move_word_backward(ctx) { word(ctx, motion::word_backward) }

    /// Select through the next word end, excluding following whitespace.
    fn move_word_end(ctx) { word(ctx, motion::word_end) }

    /// Move to the beginning of the current logical line.
    fn goto_line_start(ctx) {
        move_to(ctx, |editor, selection, _| Ok(motion::line_start(editor.document.text(), position(editor, selection)?)?))
    }

    /// Move to the last grapheme of the line, or its end boundary in insert mode.
    fn goto_line_end(ctx) {
        move_to(ctx, |editor, selection, _| {
            let text = editor.document.text();
            let pos = position(editor, selection)?;
            let end = motion::line_end(text, pos)?;
            Ok(if editor.mode != Mode::Insert && end > motion::line_start(text, pos)? {
                grapheme::previous(text, end, 1)?
            } else { end })
        })
    }

    /// Move to the start of the document.
    fn goto_file_start(ctx) { move_to(ctx, |_, _, _| Ok(CharOffset(0))) }

    /// Move to the end-of-file boundary.
    fn goto_file_end(ctx) { move_to(ctx, |editor, _, _| Ok(CharOffset(editor.document.text().len_chars()))) }

    /// Select logical lines from each cursor, including line endings; accepts a count.
    fn select_line(ctx) {
        let editor = &mut *ctx.editor;
        let text = editor.document.text();
        let ranges = editor.selections.ranges().iter().map(|&selection| {
            let pos = position(editor, selection)?;
            let line = text.char_to_line(pos.0);
            let end_line = line.saturating_add(ctx.count.get()).min(text.len_lines());
            let start = CharOffset(text.line_to_char(line));
            let end = CharOffset(text.line_to_char(end_line));
            Ok(Selection::new(start, end))
        }).collect::<Result<Vec<_>, Error>>()?;
        editor.selections = SelectionSet::new(ranges, editor.selections.primary_index())?;
        if editor.mode == Mode::Insert { editor.mode = Mode::Normal; }
        editor.preferred_columns = None;
        Ok(())
    }

    /// Toggle select mode; movements in select mode retain the anchor grapheme.
    fn select_mode(ctx) {
        ctx.editor.mode = if ctx.editor.mode == Mode::Select { Mode::Normal } else { Mode::Select };
        normalize(ctx.editor)
    }

    /// Enter normal mode. Leaving insert mode places the cursor on the preceding grapheme in the same line.
    fn normal_mode(ctx) {
        let editor = &mut *ctx.editor;
        if editor.mode == Mode::Insert {
            let text = editor.document.text();
            let ranges = editor.selections.ranges().iter().map(|selection| {
                let head = selection.head;
                let pos = if head > motion::line_start(text, head)? { grapheme::previous(text, head, 1)? } else { head };
                motion::block(text, pos)
            }).collect::<Result<Vec<_>, vex_core::Error>>()?;
            editor.selections = SelectionSet::new(ranges, editor.selections.primary_index())?;
        }
        editor.mode = Mode::Normal;
        normalize(editor)
    }

    /// Enter insert mode with a caret before every selection.
    fn insert_mode(ctx) { enter_insert(ctx.editor, false) }

    /// Enter insert mode with a caret after every selection.
    fn append_mode(ctx) { enter_insert(ctx.editor, true) }

    /// Insert the context's text at all carets as one undo step; requires insert mode.
    fn insert_text(ctx) {
        let editor = &mut *ctx.editor;
        require_insert(editor)?;
        let text = ctx.text.ok_or(Error::MissingText)?;
        let transaction = editor.document.replace_selections(&editor.selections, text)?;
        editor.apply(transaction)?;
        normalize(editor)
    }

    /// Delete selected text atomically, leaving normal-mode cursors at the edit locations.
    fn delete_selection(ctx) {
        let editor = &mut *ctx.editor;
        let transaction = editor.document.replace_selections(&editor.selections, "")?;
        editor.apply(transaction)?;
        editor.mode = Mode::Normal;
        normalize(editor)
    }

    /// Delete selected text and enter insert mode at all edit locations.
    fn change_selection(ctx) {
        let editor = &mut *ctx.editor;
        let transaction = editor.document.replace_selections(&editor.selections, "")?;
        editor.apply(transaction)?;
        editor.mode = Mode::Insert;
        normalize(editor)
    }

    /// Delete preceding graphemes at all insert carets; accepts a count.
    fn delete_backward(ctx) { erase(ctx, true) }

    /// Delete following graphemes at all insert carets; accepts a count.
    fn delete_forward(ctx) { erase(ctx, false) }

    /// Undo transactions and restore their selections; accepts a count.
    fn undo(ctx) {
        for _ in 0..ctx.count.get() {
            if !ctx.editor.document.undo(&mut ctx.editor.selections)? { break; }
            ctx.editor.synchronize_layout();
        }
        normalize(ctx.editor)
    }

    /// Redo transactions and restore their selections; accepts a count.
    fn redo(ctx) {
        for _ in 0..ctx.count.get() {
            if !ctx.editor.document.redo(&mut ctx.editor.selections)? { break; }
            ctx.editor.synchronize_layout();
        }
        normalize(ctx.editor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vex_core::Document;

    fn range(anchor: usize, head: usize) -> Selection {
        Selection::new(CharOffset(anchor), CharOffset(head))
    }

    #[test]
    fn functions_are_directly_callable_and_share_registry_documentation() {
        let mut editor = Editor::new(Document::from("hello world"));
        move_word_forward(&mut CommandContext::new(&mut editor)).unwrap();
        assert_eq!(editor.selections().primary(), range(0, 6));
        let command = find("move_word_forward").unwrap();
        assert_eq!(
            command.description(),
            "Select through the next word start; repeat counts span multiple words."
        );
        (command.run)(&mut CommandContext::new(&mut editor)).unwrap();
        assert_eq!(editor.selections().primary(), range(6, 11));
    }

    #[test]
    fn select_mode_crosses_the_anchor_without_splitting_clusters() {
        let mut editor = Editor::new(Document::from("ae\u{301}🦀z"));
        editor.execute("move_right", 1).unwrap();
        assert_eq!(editor.selections().primary(), range(1, 3));
        editor.execute("select_mode", 1).unwrap();
        editor.execute("move_left", 1).unwrap();
        assert_eq!(editor.selections().primary(), range(3, 0));
        editor.execute("move_right", 2).unwrap();
        assert_eq!(editor.selections().primary(), range(1, 4));
        assert_eq!(editor.document().revision().get(), 0);
    }

    #[test]
    fn word_extension_retains_the_original_anchor() {
        let mut editor = Editor::new(Document::from("one two three"));
        editor.execute("select_mode", 1).unwrap();
        editor.execute("move_word_forward", 1).unwrap();
        editor.execute("move_word_forward", 1).unwrap();
        assert_eq!(editor.selections().primary(), range(0, 8));
        editor.execute("move_word_backward", 1).unwrap();
        assert_eq!(editor.selections().primary(), range(0, 5));
    }

    #[test]
    fn multiple_selections_move_edit_and_restore_together() {
        let mut editor = Editor::new(Document::from("one two\nthree four"));
        editor
            .set_selections(SelectionSet::new(vec![range(0, 1), range(8, 9)], 1).unwrap())
            .unwrap();
        editor.execute("move_word_forward", 1).unwrap();
        let selected = editor.selections().clone();
        assert_eq!(selected.ranges(), &[range(0, 4), range(8, 14)]);
        editor.execute("delete_selection", 1).unwrap();
        assert_eq!(editor.document().text(), "two\nfour");
        assert_eq!(editor.document().undo_depth(), 1);
        assert_eq!(editor.selections().primary_index(), 1);
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document().text(), "one two\nthree four");
        assert_eq!(editor.selections(), &selected);
        editor.execute("redo", 1).unwrap();
        assert_eq!(editor.document().text(), "two\nfour");
    }

    #[test]
    fn vertical_motion_keeps_desired_columns_through_short_lines() {
        let mut editor = Editor::new(Document::from("a\t界z\r\nx\r\n123456z"));
        editor
            .set_selections(SelectionSet::single(range(3, 4)))
            .unwrap();
        editor.execute("move_down", 1).unwrap();
        assert_eq!(editor.selections().primary(), range(7, 9));
        editor.execute("move_down", 1).unwrap();
        assert_eq!(editor.selections().primary(), range(15, 16));
        editor.execute("move_up", 2).unwrap();
        assert_eq!(editor.selections().primary(), range(3, 4));
        editor.execute("move_left", 1).unwrap();
        editor.execute("move_down", 2).unwrap();
        assert_eq!(editor.selections().primary(), range(13, 14));
    }

    #[test]
    fn overlapping_multi_caret_deletions_merge_before_the_transaction() {
        let mut editor = Editor::new(Document::from("abcd"));
        editor.execute("insert_mode", 1).unwrap();
        let before = SelectionSet::new(vec![range(2, 2), range(3, 3)], 1).unwrap();
        editor.set_selections(before.clone()).unwrap();
        editor.execute("delete_backward", 2).unwrap();
        assert_eq!(editor.document().text(), "d");
        assert_eq!(editor.selections().ranges(), &[range(0, 0)]);
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document().text(), "abcd");
        assert_eq!(editor.selections(), &before);
    }

    #[test]
    fn deleting_a_separator_can_join_graphemes_without_leaving_an_interior_cursor() {
        let mut editor = Editor::new(Document::from("a\n\u{301}"));
        editor.execute("move_right", 1).unwrap();
        editor.execute("delete_selection", 1).unwrap();
        assert_eq!(editor.document().text(), "a\u{301}");
        assert_eq!(editor.selections().primary(), range(0, 2));
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.selections().primary(), range(1, 2));
    }

    #[test]
    fn line_selection_includes_crlf_and_handles_final_empty_line() {
        let mut editor = Editor::new(Document::from("one\r\ntwo\r\n"));
        editor.execute("select_line", 2).unwrap();
        assert_eq!(editor.selections().primary(), range(0, 10));
        editor.execute("goto_file_end", 1).unwrap();
        editor.execute("select_line", usize::MAX).unwrap();
        assert_eq!(editor.selections().primary(), range(10, 10));
        editor.execute("delete_selection", 1).unwrap();
        assert_eq!(editor.document().revision().get(), 0);
    }

    #[test]
    fn insert_mode_arrows_preserve_carets_and_escape_stays_on_the_same_line() {
        let mut editor = Editor::new(Document::from("a\r\ne\u{301}"));
        editor.execute("insert_mode", 1).unwrap();
        editor.execute("move_right", 2).unwrap();
        assert_eq!(editor.selections().primary(), range(3, 3));
        editor.execute("normal_mode", 1).unwrap();
        assert_eq!(editor.selections().primary(), range(3, 5));
        editor.execute("append_mode", 1).unwrap();
        editor.execute("normal_mode", 1).unwrap();
        assert_eq!(editor.selections().primary(), range(3, 5));
    }

    #[test]
    fn text_event_is_one_transaction_for_multiple_carets() {
        let mut editor = Editor::new(Document::from("ab"));
        editor.execute("insert_mode", 1).unwrap();
        editor
            .set_selections(SelectionSet::new(vec![range(0, 0), range(2, 2)], 1).unwrap())
            .unwrap();
        editor.insert_text("e\u{301}🦀\r\n").unwrap();
        assert_eq!(editor.document().text(), "e\u{301}🦀\r\nabe\u{301}🦀\r\n");
        assert_eq!(editor.document().undo_depth(), 1);
        assert_eq!(editor.selections().ranges(), &[range(5, 5), range(12, 12)]);
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document().text(), "ab");
        assert_eq!(editor.selections().ranges(), &[range(0, 0), range(2, 2)]);
    }
}
