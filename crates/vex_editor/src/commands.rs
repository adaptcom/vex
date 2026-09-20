//! Named command functions. Their Rust documentation also supplies runtime help.

use std::num::NonZeroUsize;
use vex_core::{Affinity, CharOffset, Edit, Selection, SelectionSet, grapheme, motion};

use crate::{Editor, Error, Mode};

pub struct CommandContext<'a> {
    pub editor: &'a mut Editor,
    pub count: NonZeroUsize,
    /// Distinguish an explicit 1 from an omitted count for commands such as G.
    pub count_given: bool,
    pub text: Option<&'a str>,
    pub character: Option<char>,
    /// Override the next-command register selected through the keymap.
    pub register: Option<char>,
}

impl<'a> CommandContext<'a> {
    pub fn new(editor: &'a mut Editor) -> Self {
        Self {
            editor,
            count: NonZeroUsize::MIN,
            count_given: false,
            text: None,
            character: None,
            register: None,
        }
    }
}

/// Input a keybinding must collect before invoking the command function.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CommandInput {
    #[default]
    None,
    Character,
    TextobjectInner,
    TextobjectAround,
    SurroundAdd,
    SurroundDelete,
    SurroundReplace,
    SurroundReplacement,
    RegisterSelect,
    RegisterInsert,
}

#[derive(Debug)]
pub struct Command {
    pub name: &'static str,
    pub documentation: &'static str,
    pub run: fn(&mut CommandContext<'_>) -> Result<(), Error>,
    pub input: CommandInput,
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
    ($($(#[doc = $doc:literal])+ fn $name:ident($ctx:ident) $([$input:ident])? $body:block)+) => {
        $(
            $(#[doc = $doc])+
            pub fn $name($ctx: &mut CommandContext<'_>) -> Result<(), Error> {
                static COMMAND: Command = Command {
                    name: stringify!($name), documentation: concat!($($doc, "\n",)+),
                    run: $name, input: commands!(@input $($input)?),
                };
                crate::repeat::invoke($ctx, &COMMAND, |$ctx| $body)
            }
        )+
        pub static COMMANDS: &[Command] = &[
            $(Command { name: stringify!($name), documentation: concat!($($doc, "\n",)+), run: $name, input: commands!(@input $($input)?) },)+
        ];
    };
    (@input $input:ident) => { CommandInput::$input };
    (@input) => { CommandInput::None };
}

fn replace_ranges(editor: &mut Editor, ranges: &SelectionSet, text: &str) -> Result<(), Error> {
    let transaction = editor.document.replace_selections(ranges, text)?;
    let after = transaction.map_selections(&editor.selections, Affinity::After)?;
    let transaction = transaction.with_selections(after)?;
    editor.apply(transaction, true)?;
    editor.preferred_columns = None;
    Ok(())
}

fn normalize(editor: &mut Editor) -> Result<(), Error> {
    editor.selections = editor.normalized(editor.selections.clone(), editor.mode)?;
    editor.preferred_columns = None;
    Ok(())
}

// Selection commands retain normal/select mode. Direct calls from insert mode
// enter normal mode so their ranges are not immediately collapsed to carets.
fn select_ranges(editor: &mut Editor, ranges: Vec<Selection>, primary: usize) -> Result<(), Error> {
    let mode = if editor.mode == Mode::Insert {
        Mode::Normal
    } else {
        editor.mode
    };
    let selections = editor.normalized(SelectionSet::new(ranges, primary)?, mode)?;
    editor.finish_undo_group();
    editor.selections = selections;
    editor.mode = mode;
    editor.preferred_columns = None;
    Ok(())
}

/// Full bounds of the lines touched by a half-open selection, plus the exclusive
/// ending line index. An endpoint at the next line's start excludes that line.
fn selected_line_bounds(text: &vex_core::Rope, selection: Selection) -> (Selection, usize) {
    let start = text.line_to_char(text.char_to_line(selection.start().0));
    let last = selection.end().0 - usize::from(!selection.is_empty());
    let end_line = text.char_to_line(last) + 1;
    let end = text.line_to_char(end_line);
    (Selection::new(CharOffset(start), CharOffset(end)), end_line)
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
    editor.finish_undo_group();
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
    editor.finish_undo_group();
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
    editor.finish_undo_group();
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
    editor.finish_undo_group();
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

fn find_character(
    ctx: &mut CommandContext<'_>,
    direction: vex_core::search::Direction,
    inclusive: bool,
) -> Result<(), Error> {
    let character = ctx.character.ok_or(Error::MissingCharacter)?;
    let editor = &mut *ctx.editor;
    editor.finish_undo_group();
    let text = editor.document.text();
    let ranges = editor
        .selections
        .ranges()
        .iter()
        .map(|&selection| {
            let origin = position(editor, selection)?;
            let Some(destination) = motion::find_char(
                text,
                origin,
                character,
                ctx.count.get(),
                direction,
                inclusive,
            )?
            else {
                return Ok(selection);
            };
            if editor.mode == Mode::Insert {
                return Ok(Selection::cursor(destination));
            }
            // Normal mode selects the traversed span. Select mode keeps the original
            // anchor, including when the destination crosses it.
            let from = if editor.mode == Mode::Select {
                selection
            } else {
                motion::block(text, origin)?
            };
            Ok(motion::put_cursor(text, from, destination, true)?)
        })
        .collect::<Result<Vec<_>, Error>>()?;
    editor.selections = SelectionSet::new(ranges, editor.selections.primary_index())?;
    editor.preferred_columns = None;
    Ok(())
}

fn goto_counted_line(ctx: &mut CommandContext<'_>) -> Result<(), Error> {
    move_to(ctx, |editor, _, count| {
        let text = editor.document.text();
        let mut last = text.len_lines() - 1;
        if last > 0 && text.line(last).len_chars() == 0 {
            last -= 1;
        }
        Ok(CharOffset(text.line_to_char((count - 1).min(last))))
    })
}

/// Preserve the exact whitespace prefix, independent of language or tab width.
fn leading_indent(text: vex_core::RopeSlice<'_>) -> String {
    text.chars()
        .take_while(|ch| matches!(ch, ' ' | '\t'))
        .collect()
}

fn open_lines(ctx: &mut CommandContext<'_>, below: bool) -> Result<(), Error> {
    let editor = &mut *ctx.editor;
    let text = editor.document.text();
    let count = ctx.count.get();
    // Use the selection's outer lines in either direction. A selection ending
    // at the next line's start still belongs to the preceding line.
    let lines = editor
        .selections
        .ranges()
        .iter()
        .map(|selection| {
            let edge = if below && !selection.is_empty() {
                grapheme::previous(text, selection.end(), 1)?
            } else {
                selection.start()
            };
            Ok(Selection::cursor(motion::line_start(text, edge)?))
        })
        .collect::<Result<Vec<_>, vex_core::Error>>()?;
    // Multiple selections on a line open it once, retaining the primary.
    let lines = SelectionSet::new(lines, editor.selections.primary_index())?;
    let capacity = lines
        .ranges()
        .len()
        .checked_mul(count)
        .ok_or(vex_core::Error::LengthOverflow)?;
    let mut carets = Vec::new();
    carets
        .try_reserve_exact(capacity)
        .map_err(|_| vex_core::Error::LengthOverflow)?;
    let edits = lines
        .ranges()
        .iter()
        .map(|line| {
            let indent = leading_indent(text.slice(line.head.0..));
            let unit = if below {
                format!("{}{indent}", editor.newline())
            } else {
                format!("{indent}{}", editor.newline())
            };
            let len = unit
                .len()
                .checked_mul(count)
                .ok_or(vex_core::Error::LengthOverflow)?;
            let mut inserted = String::new();
            inserted
                .try_reserve_exact(len)
                .map_err(|_| vex_core::Error::LengthOverflow)?;
            for _ in 0..count {
                inserted.push_str(&unit);
            }
            let at = if below {
                motion::line_end(text, line.head)?
            } else {
                line.head
            };
            Ok(Edit::insert(at, inserted))
        })
        .collect::<Result<Vec<_>, vex_core::Error>>()?;
    let transaction = editor.document.transaction(edits)?;
    for edit in transaction.edits() {
        // The inserted indentation and line endings are all ASCII.
        let width = edit.text().len() / count;
        let start = transaction
            .map_position(edit.range().start, Affinity::Before)?
            .0;
        let first = start + width - if below { 0 } else { editor.newline().len() };
        for index in 0..count {
            carets.push(Selection::cursor(CharOffset(first + index * width)));
        }
    }
    let selections = SelectionSet::new(carets, lines.primary_index() * count)?;
    let transaction = transaction.with_selections(selections)?;
    editor.finish_undo_group();
    editor.apply(transaction, true)?;
    editor.mode = Mode::Insert;
    normalize(editor)
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

fn erase_insert_range(
    ctx: &mut CommandContext<'_>,
    edge: impl Fn(&vex_core::Rope, CharOffset, usize) -> Result<CharOffset, vex_core::Error>,
) -> Result<(), Error> {
    let editor = &mut *ctx.editor;
    require_insert(editor)?;
    let text = editor.document.text();
    let ranges = editor
        .selections
        .ranges()
        .iter()
        .map(|selection| {
            Ok(Selection::new(
                selection.head,
                edge(text, selection.head, ctx.count.get())?,
            ))
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

fn insert(ctx: &mut CommandContext<'_>, grouped: bool) -> Result<(), Error> {
    let editor = &mut *ctx.editor;
    require_insert(editor)?;
    let text = ctx.text.ok_or(Error::MissingText)?;
    let transaction = editor
        .document
        .replace_selections(&editor.selections, text)?;
    editor.apply(transaction, grouped)?;
    normalize(editor)
}

fn clipboard(ctx: &mut CommandContext<'_>, action: crate::ClipboardAction) -> Result<(), Error> {
    if ctx.editor.mode == Mode::Insert {
        return Err(Error::WrongMode {
            expected: Mode::Normal,
            actual: Mode::Insert,
        });
    }
    ctx.editor.finish_undo_group();
    ctx.editor.mode = Mode::Normal;
    ctx.editor
        .request_clipboard(crate::ClipboardKind::System, action, ctx.count.get());
    Ok(())
}

commands! {
    /// Select a register for the next command. Names are literal, case-sensitive Unicode characters; a motion or failed command also consumes the selection. Escape cancels it.
    fn select_register(ctx) [RegisterSelect] {
        ctx.editor.selected_register = Some(ctx.character.ok_or(Error::MissingCharacter)?);
        Ok(())
    }

    /// Insert the following register's fragments at insertion carets, retaining insert mode. Fragments pair with selections and counts repeat each fragment; line endings follow the buffer. This is a separate undo step.
    fn insert_register(ctx) [RegisterInsert] {
        require_insert(ctx.editor)?;
        ctx.register = Some(ctx.character.ok_or(Error::MissingCharacter)?);
        crate::register::paste(ctx, crate::Paste::Cursor)
    }

    /// Copy all selections to the system clipboard, retaining fragment boundaries for later pastes while the clipboard is unchanged. Leaves select mode; counts are ignored. The frontend performs clipboard I/O in the background.
    fn yank_to_clipboard(ctx) { clipboard(ctx, crate::ClipboardAction::Yank) }

    /// Copy only the primary selection to the system clipboard and leave select mode. Does not change the internal yank register; ignores counts.
    fn yank_main_selection_to_clipboard(ctx) { clipboard(ctx, crate::ClipboardAction::YankMain) }

    /// Paste system clipboard fragments after selections in normal mode, honoring counts and the destination's line endings. Newline-terminated text pastes below selected lines. Clipboard reads and edit preparation run in the background.
    fn paste_clipboard_after(ctx) { clipboard(ctx, crate::ClipboardAction::Paste(crate::Paste::After)) }

    /// Paste system clipboard fragments before selections in normal mode, honoring counts and the destination's line endings. Newline-terminated text pastes above selected lines.
    fn paste_clipboard_before(ctx) { clipboard(ctx, crate::ClipboardAction::Paste(crate::Paste::Before)) }

    /// Replace selections with system clipboard fragments in one undo step, honoring counts and leaving the internal yank register unchanged.
    fn replace_selections_with_clipboard(ctx) { clipboard(ctx, crate::ClipboardAction::Paste(crate::Paste::Replace)) }

    /// Repeat the last completed insert session at the current selections. Replays its entry command, counts, text, motions, and accepted completion locally. A count repeats the whole session; ordinary normal-mode edits do not replace it.
    fn repeat_insert(ctx) { crate::repeat::start(ctx) }

    /// Surround each selection with a character or bracket pair, selecting the result and returning to normal mode. Either bracket chooses its matching pair; other characters repeat on both sides. Enter uses the buffer's line ending. Ignores counts, leaves registers unchanged, and records one undo step.
    fn surround_add(ctx) [SurroundAdd] { crate::surround::add(ctx) }

    /// Delete a surrounding pair at every cursor. Either bracket chooses its pair; m chooses the nearest pair. Counts seek outer pairs. Missing or overlapping pairs cancel all edits. Returns to normal mode with one undo step and leaves registers unchanged.
    fn surround_delete(ctx) [SurroundDelete] { crate::search::surround(ctx, false) }

    /// Find and preview surrounding delimiters before replacing them. Supply the old delimiter (or m for nearest); counts seek outer pairs. Then supply the replacement character with surround_replace_finish. Cancellation restores the original selections.
    fn surround_replace(ctx) [SurroundReplace] { crate::search::surround(ctx, true) }

    /// Replace the previewed surrounding pairs with a character or bracket pair, restoring the original selections before mapping them through one undoable edit. Returns to normal mode and leaves registers unchanged. Used as the second input step of mr.
    fn surround_replace_finish(ctx) [SurroundReplacement] { crate::surround::finish(ctx) }

    /// Move to the matching bracket, extending in select mode. Syntax also finds the enclosing scope from inside it. Counts are ignored.
    fn match_brackets(ctx) { crate::search::match_brackets(ctx) }

    /// Select inside a word (w), WORD (W), paragraph (p), specified delimiter, or closest pair (m), retaining normal/select mode. Counts select outer pairs or multiple paragraphs; word objects ignore counts. Scans support background cancellation.
    fn select_textobject_inner(ctx) [TextobjectInner] { crate::search::textobject(ctx, false) }

    /// Select around a word (w), WORD (W), paragraph (p), specified delimiter, or closest pair (m), including adjacent whitespace or delimiters. Counts select outer pairs or multiple paragraphs; word objects ignore counts.
    fn select_textobject_around(ctx) [TextobjectAround] { crate::search::textobject(ctx, true) }
    /// Add copies of each selection on following logical lines at the same display columns. Counts add copies, skipping lines that cannot fit both endpoints. Multi-line selections advance by their height; the primary follows its last copy. Uses the cancellable search worker when enabled.
    fn copy_selection_on_next_line(ctx) { crate::search::copy_lines(ctx, true) }

    /// Add copies of each selection on preceding logical lines at the same display columns. Counts add copies, skipping short lines; preserves selection direction and mode. Uses the cancellable search worker when enabled.
    fn copy_selection_on_prev_line(ctx) { crate::search::copy_lines(ctx, false) }

    /// Save the current selections as a jump checkpoint without writing the file.
    fn save_selection(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_application_action(crate::ApplicationAction::SaveSelection);
        Ok(())
    }

    /// Toggle comments on selected lines, preferring line comments and recognizing existing block comments. Uses language delimiters, skips blank lines, preserves selections and mode, and creates one undo step.
    fn toggle_comments(ctx) { crate::comments::toggle(ctx.editor, false) }

    /// Toggle block comments around selections, retaining their direction and selecting added delimiters. Languages with only line comments use those instead; plain text defaults to /* */. One undo step, with normal/select mode retained.
    fn toggle_block_comments(ctx) { crate::comments::toggle(ctx.editor, true) }

    /// Focus the next window in layout order. A count advances multiple windows.
    fn rotate_view(ctx) {
        ctx.editor.request_application_action(crate::ApplicationAction::Window(crate::WindowAction::Rotate, ctx.count.get()));
        Ok(())
    }

    /// Split the current window vertically, opening a shared view on the right.
    fn vsplit(ctx) {
        ctx.editor.request_application_action(crate::ApplicationAction::Window(crate::WindowAction::SplitVertical, ctx.count.get()));
        Ok(())
    }

    /// Split the current window horizontally, opening a shared view below.
    fn hsplit(ctx) {
        ctx.editor.request_application_action(crate::ApplicationAction::Window(crate::WindowAction::SplitHorizontal, ctx.count.get()));
        Ok(())
    }

    /// Focus the window to the left.
    fn jump_view_left(ctx) {
        ctx.editor.request_application_action(crate::ApplicationAction::Window(crate::WindowAction::FocusLeft, ctx.count.get()));
        Ok(())
    }

    /// Focus the window below.
    fn jump_view_down(ctx) {
        ctx.editor.request_application_action(crate::ApplicationAction::Window(crate::WindowAction::FocusDown, ctx.count.get()));
        Ok(())
    }

    /// Focus the window above.
    fn jump_view_up(ctx) {
        ctx.editor.request_application_action(crate::ApplicationAction::Window(crate::WindowAction::FocusUp, ctx.count.get()));
        Ok(())
    }

    /// Focus the window to the right.
    fn jump_view_right(ctx) {
        ctx.editor.request_application_action(crate::ApplicationAction::Window(crate::WindowAction::FocusRight, ctx.count.get()));
        Ok(())
    }

    /// Swap the current window with the window to the left.
    fn swap_view_left(ctx) {
        ctx.editor.request_application_action(crate::ApplicationAction::Window(crate::WindowAction::SwapLeft, ctx.count.get()));
        Ok(())
    }

    /// Swap the current window with the window below.
    fn swap_view_down(ctx) {
        ctx.editor.request_application_action(crate::ApplicationAction::Window(crate::WindowAction::SwapDown, ctx.count.get()));
        Ok(())
    }

    /// Swap the current window with the window above.
    fn swap_view_up(ctx) {
        ctx.editor.request_application_action(crate::ApplicationAction::Window(crate::WindowAction::SwapUp, ctx.count.get()));
        Ok(())
    }

    /// Swap the current window with the window to the right.
    fn swap_view_right(ctx) {
        ctx.editor.request_application_action(crate::ApplicationAction::Window(crate::WindowAction::SwapRight, ctx.count.get()));
        Ok(())
    }

    /// Close this window, protecting the last view of unsaved text. Exit when no windows remain.
    fn wclose(ctx) {
        ctx.editor.request_application_action(crate::ApplicationAction::Window(crate::WindowAction::Close, ctx.count.get()));
        Ok(())
    }

    /// Keep only this window, protecting unsaved text in other buffers.
    fn wonly(ctx) {
        ctx.editor.request_application_action(crate::ApplicationAction::Window(crate::WindowAction::Only, ctx.count.get()));
        Ok(())
    }

    /// Open filenames in the selections in horizontal splits. Paths are relative to the current file.
    fn goto_file_hsplit(ctx) {
        ctx.editor.request_application_action(crate::ApplicationAction::Window(crate::WindowAction::OpenHorizontal, ctx.count.get()));
        Ok(())
    }

    /// Open filenames in the selections in vertical splits. Paths are relative to the current file.
    fn goto_file_vsplit(ctx) {
        ctx.editor.request_application_action(crate::ApplicationAction::Window(crate::WindowAction::OpenVertical, ctx.count.get()));
        Ok(())
    }

    /// Request language-server completion at the insertion cursor.
    fn completion(ctx) {
        require_insert(ctx.editor)?;
        if ctx.editor.selections().ranges().len() != 1 {
            return Err(Error::InvalidCompletion);
        }
        ctx.editor.request_language_action(crate::LanguageAction::Completion);
        Ok(())
    }

    /// Open a fuzzy file picker at the current project root.
    fn file_picker(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_application_action(crate::ApplicationAction::FilePicker);
        Ok(())
    }

    /// Open a fuzzy picker of loaded buffers, including hidden and unsaved buffers.
    fn buffer_picker(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_application_action(crate::ApplicationAction::BufferPicker);
        Ok(())
    }

    /// Open a fuzzy picker of saved jump locations from every pane, restoring the full selection on acceptance.
    fn jumplist_picker(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_application_action(crate::ApplicationAction::JumpPicker);
        Ok(())
    }

    /// Reopen the last picker with its query, selected result, and scroll position.
    fn last_picker(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_application_action(crate::ApplicationAction::LastPicker);
        Ok(())
    }

    /// Search file contents below the working directory with a regular expression, including unsaved buffers. Accepted queries use the chosen search register (default /).
    fn global_search(ctx) {
        let register = ctx.register.unwrap_or('/');
        crate::search::writable_query(register)?;
        ctx.editor.finish_undo_group();
        ctx.editor.request_application_action(crate::ApplicationAction::GlobalSearch(register));
        Ok(())
    }

    /// Switch to the last buffer accessed in this pane, restoring its view.
    fn goto_last_accessed_file(ctx) {
        ctx.editor.request_application_action(crate::ApplicationAction::Buffer(crate::BufferAction::LastAccessed, 1));
        Ok(())
    }

    /// Go to the end of the latest undo group's change associated with its original primary selection. Select mode extends each selection; records a jump on success.
    fn goto_last_modification(ctx) { crate::search::last_modification(ctx) }

    /// Switch to the last other buffer modified in this pane.
    fn goto_last_modified_file(ctx) {
        ctx.editor.request_application_action(crate::ApplicationAction::Buffer(crate::BufferAction::LastModified, 1));
        Ok(())
    }

    /// Switch to the next loaded buffer in opening order, wrapping; accepts a count.
    fn goto_next_buffer(ctx) {
        ctx.editor.request_application_action(crate::ApplicationAction::Buffer(crate::BufferAction::Next, ctx.count.get()));
        Ok(())
    }

    /// Switch to the previous loaded buffer in opening order, wrapping; accepts a count.
    fn goto_previous_buffer(ctx) {
        ctx.editor.request_application_action(crate::ApplicationAction::Buffer(crate::BufferAction::Previous, ctx.count.get()));
        Ok(())
    }

    /// Open filenames in the selections in the current pane. Paths are relative to the current file; earlier buffers remain loaded.
    fn goto_file(ctx) {
        ctx.editor.request_application_action(crate::ApplicationAction::Buffer(crate::BufferAction::OpenSelected, 1));
        Ok(())
    }

    /// Open the repository status view with expandable staged and unstaged diffs.
    fn git_status(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_application_action(crate::ApplicationAction::GitStatus);
        Ok(())
    }

    /// Open a searchable picker of symbols in the current document using its language server.
    fn symbol_picker(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_application_action(crate::ApplicationAction::DocumentSymbols);
        Ok(())
    }

    /// Open a searchable picker of cached diagnostics for the current document.
    fn diagnostics_picker(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_application_action(crate::ApplicationAction::DiagnosticPicker(false));
        Ok(())
    }

    /// Open a searchable picker of cached diagnostics across files and language sessions.
    fn workspace_diagnostics_picker(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_application_action(crate::ApplicationAction::DiagnosticPicker(true));
        Ok(())
    }

    /// Search workspace symbols using the current document's language server.
    fn workspace_symbol_picker(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_application_action(crate::ApplicationAction::WorkspaceSymbols);
        Ok(())
    }

    /// Move backward through this pane's jump history, preserving a forward return path. Accepts a count.
    fn jump_backward(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_application_action(crate::ApplicationAction::Jump { forward: false, count: ctx.count.get() });
        Ok(())
    }

    /// Move forward through this pane's jump history. Accepts a count.
    fn jump_forward(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_application_action(crate::ApplicationAction::Jump { forward: true, count: ctx.count.get() });
        Ok(())
    }

    /// Alias for jump_backward; the default Ctrl-o binding uses the Helix command name.
    fn jump_back(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_application_action(crate::ApplicationAction::Jump { forward: false, count: ctx.count.get() });
        Ok(())
    }

    /// Show language-server documentation for the symbol at the primary cursor.
    fn hover(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_language_action(crate::LanguageAction::Hover);
        Ok(())
    }

    /// Show function signatures and the active parameter at the primary cursor. Normally opens automatically in insert mode; Alt-p/Alt-n cycle overloads. No default binding, matching Helix.
    fn signature_help(ctx) {
        ctx.editor.request_language_action(crate::LanguageAction::SignatureHelp);
        Ok(())
    }

    /// Jump to the definition of the symbol at the primary cursor.
    fn goto_definition(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_language_action(crate::LanguageAction::Definition);
        Ok(())
    }

    /// Jump to the type definition of the symbol at the primary cursor, or pick among destinations.
    fn goto_type_definition(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_language_action(crate::LanguageAction::TypeDefinition);
        Ok(())
    }

    /// Jump to an implementation of the symbol at the primary cursor, or pick among destinations.
    fn goto_implementation(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_language_action(crate::LanguageAction::Implementation);
        Ok(())
    }

    /// Find references to the symbol at the primary cursor, including its declaration.
    fn goto_reference(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_language_action(crate::LanguageAction::References);
        Ok(())
    }

    /// Select document highlights for the symbol under the primary cursor, retaining the primary occurrence.
    fn select_references_to_symbol_under_cursor(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_language_action(crate::LanguageAction::DocumentHighlights);
        Ok(())
    }

    /// Rename the symbol under the primary cursor through its language server.
    /// Opens a prompt prefilled with the current name. Workspace edits preserve
    /// unsaved buffers and create a separate undo step in each changed buffer.
    fn rename_symbol(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_language_action(crate::LanguageAction::Rename);
        Ok(())
    }

    /// Show language-server code actions for the primary selection.
    /// The frontend resolves the selected action and applies its edits before
    /// executing any accompanying server command.
    fn code_action(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_language_action(crate::LanguageAction::CodeAction);
        Ok(())
    }

    /// Format the current selection through the language server. Like Helix,
    /// this requires exactly one selection and range-formatting support.
    /// Text preparation runs in the background; changes remain unsaved and undoable.
    fn format_selections(ctx) {
        if ctx.editor.selections().ranges().len() != 1 { return Err(Error::FormatSelectionCount); }
        ctx.editor.finish_undo_group();
        ctx.editor.request_language_action(crate::LanguageAction::FormatSelections);
        Ok(())
    }

    /// Format the whole document through its language server, preserving all
    /// selections and creating one unsaved undo step. Used by :format and :fmt.
    fn format_document(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_language_action(crate::LanguageAction::FormatDocument);
        Ok(())
    }

    /// Select the next diagnostic range without wrapping. Numeric prefixes are
    /// ignored, matching Helix; the frontend records a jump-history entry.
    fn goto_next_diagnostic(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_language_action(crate::LanguageAction::NextDiagnostic);
        Ok(())
    }

    /// Select the previous diagnostic range with the cursor at its start, without
    /// wrapping. Numeric prefixes are ignored, matching Helix.
    fn goto_previous_diagnostic(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_language_action(crate::LanguageAction::PreviousDiagnostic);
        Ok(())
    }

    /// Select the first diagnostic range and record the origin in jump history.
    fn goto_first_diagnostic(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_language_action(crate::LanguageAction::FirstDiagnostic);
        Ok(())
    }

    /// Select the last diagnostic range and record the origin in jump history.
    fn goto_last_diagnostic(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.request_language_action(crate::LanguageAction::LastDiagnostic);
        Ok(())
    }

    /// Begin a forward regex search after the primary selection. Matches wrap; counts select successive matches, and select mode adds them. Acceptance stores the query in the chosen register (default: /) and makes it active for n/N across buffers.
    fn search_forward(ctx) { crate::search::begin(ctx, crate::SearchPrompt::Forward) }

    /// Begin a backward regex search before the primary selection, wrapping at document boundaries. Acceptance stores the query in the chosen register (default: /) and makes it active for n/N across buffers.
    fn search_backward(ctx) { crate::search::begin(ctx, crate::SearchPrompt::Backward) }

    /// Select regex matches inside the current selections; lowercase queries ignore case.
    fn select_regex(ctx) { crate::search::begin(ctx, crate::SearchPrompt::Select) }

    /// Split current selections on regex matches, excluding the matched separators.
    fn split_selection(ctx) { crate::search::begin(ctx, crate::SearchPrompt::Split) }

    /// Keep selections containing a regex match.
    fn keep_selections(ctx) { crate::search::begin(ctx, crate::SearchPrompt::Keep) }

    /// Remove selections containing a regex match, retaining at least one selection.
    fn remove_selections(ctx) { crate::search::begin(ctx, crate::SearchPrompt::Remove) }

    /// Remember selected text as a literal regex with detected word boundaries; use n/N to navigate.
    fn search_selection_detect_word_boundaries(ctx) { crate::search::remember(ctx, true) }

    /// Remember selected text as a literal regex, without adding word boundaries.
    fn search_selection(ctx) { crate::search::remember(ctx, false) }

    /// Preview the context's regex from the original selections. Empty, invalid, or unmatched input restores them.
    fn search_update(ctx) { crate::search::update(ctx.editor, ctx.text.ok_or(Error::MissingText)?) }

    /// Accept a matching preview and store its query in the chosen register, waiting for pending work if needed. Invalid or unmatched queries remain editable.
    fn search_accept(ctx) { crate::search::accept(ctx.editor) }

    /// Cancel a regex preview and restore its original selections and preferred columns.
    fn search_cancel(ctx) { crate::search::cancel(ctx.editor) }

    /// Search forward with the chosen register, or the last active search register. Select mode adds matches. Counts wrap and selection direction is preserved; changed register contents are compiled on the worker.
    fn search_next(ctx) { crate::search::repeat(ctx, false) }

    /// Search backward with the chosen register, or the last active search register. Select mode adds matches. This is independent of the last prompt direction.
    fn search_previous(ctx) { crate::search::repeat(ctx, true) }

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

    /// Move cursors and scroll up by half the visible text height, retaining desired columns and extending selections in select mode. Counts multiply the distance; the frontend supplies the current viewport size.
    fn page_cursor_half_up(ctx) {
        ctx.editor.request_application_action(crate::ApplicationAction::HalfPageUp(ctx.count.get()));
        Ok(())
    }

    /// Move cursors and scroll down by half the visible text height, retaining desired columns and extending selections in select mode. Counts multiply the distance; the frontend supplies the current viewport size.
    fn page_cursor_half_down(ctx) {
        ctx.editor.request_application_action(crate::ApplicationAction::HalfPageDown(ctx.count.get()));
        Ok(())
    }

    /// Select through the next word start; repeat counts span multiple words.
    fn move_word_forward(ctx) { word(ctx, motion::word_forward) }

    /// Select backward to a word start; repeat counts span multiple words.
    fn move_word_backward(ctx) { word(ctx, motion::word_backward) }

    /// Select through the next word end, excluding following whitespace.
    fn move_word_end(ctx) { word(ctx, motion::word_end) }

    /// Select through the next whitespace-separated WORD start, keeping punctuation within each WORD; accepts a count.
    fn move_long_word_forward(ctx) { word(ctx, motion::long_word_forward) }

    /// Select backward to a whitespace-separated WORD start; accepts a count.
    fn move_long_word_backward(ctx) { word(ctx, motion::long_word_backward) }

    /// Select through the next whitespace-separated WORD end, excluding following whitespace; accepts a count.
    fn move_long_word_end(ctx) { word(ctx, motion::long_word_end) }

    /// Select through the next occurrence of the supplied character, across line boundaries without wrapping; accepts a count. Enter targets a logical line ending; Tab targets a tab. Unmatched selections stay unchanged.
    fn find_next_char(ctx) [Character] { find_character(ctx, vex_core::search::Direction::Forward, true) }

    /// Select backward through the supplied character, across line boundaries without wrapping; accepts a count. Unmatched selections stay unchanged.
    fn find_prev_char(ctx) [Character] { find_character(ctx, vex_core::search::Direction::Backward, true) }

    /// Select until just before the next supplied character, skipping an adjacent match so repeated finds advance; accepts a count and crosses lines.
    fn find_till_char(ctx) [Character] { find_character(ctx, vex_core::search::Direction::Forward, false) }

    /// Select backward until just after the previous supplied character, skipping an adjacent match so repeated finds advance; accepts a count and crosses lines.
    fn till_prev_char(ctx) [Character] { find_character(ctx, vex_core::search::Direction::Backward, false) }

    /// Move to the first non-whitespace grapheme of each cursor's line. Whitespace-only lines keep their selections unchanged.
    fn goto_first_nonwhitespace(ctx) {
        let editor = &mut *ctx.editor;
        editor.finish_undo_group();
        let ranges = editor.selections.ranges().iter().map(|&selection| {
            match motion::first_nonwhitespace(editor.document.text(), position(editor, selection)?)? {
                Some(destination) => at_destination(editor, selection, destination),
                None => Ok(selection),
            }
        }).collect::<Result<Vec<_>, Error>>()?;
        editor.selections = SelectionSet::new(ranges, editor.selections.primary_index())?;
        editor.preferred_columns = None;
        Ok(())
    }

    /// Move to the counted one-based grapheme column (default 1), clamped to each cursor's logical line. Tabs and wide graphemes each count as one column.
    fn goto_column(ctx) {
        move_to(ctx, |editor, selection, count| Ok(motion::at_grapheme_column(editor.document.text(), position(editor, selection)?, count - 1)?))
    }

    /// Move to the explicitly counted one-based line, clamping to the last content line. With no count, do nothing; select mode extends to the destination.
    fn goto_line(ctx) {
        if !ctx.count_given { return Ok(()); }
        goto_counted_line(ctx)
    }

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

    /// Move to the start of the document, or to the counted one-based line, clamped to the last content line. Select mode extends to the destination.
    fn goto_file_start(ctx) { goto_counted_line(ctx) }

    /// Move to the end-of-file boundary.
    fn goto_file_end(ctx) { move_to(ctx, |editor, _, _| Ok(CharOffset(editor.document.text().len_chars()))) }

    /// Expand each selection to whole logical lines, including line endings, facing forward. If already line-aligned, extend below by the count; otherwise alignment counts as the first step.
    /// Repeated x keeps earlier lines selected. Counts clamp at EOF and overlapping ranges merge while retaining the primary selection.
    fn select_line(ctx) {
        let editor = &mut *ctx.editor;
        let text = editor.document.text();
        let ranges = editor.selections.ranges().iter().map(|&selection| {
            let (bounds, end_line) = selected_line_bounds(text, selection);
            let aligned = selection.range() == bounds.range();
            let extra = ctx.count.get() - usize::from(!aligned);
            let end_line = end_line.saturating_add(extra).min(text.len_lines());
            let end = CharOffset(text.line_to_char(end_line));
            Selection::new(bounds.start(), end)
        }).collect();
        let primary = editor.selections.primary_index();
        select_ranges(editor, ranges, primary)
    }

    /// Select the entire document as one forward range, retaining normal/select mode. Empty documents retain a single EOF cursor.
    fn select_all(ctx) {
        let end = CharOffset(ctx.editor.document.text().len_chars());
        select_ranges(ctx.editor, vec![Selection::new(CharOffset(0), end)], 0)
    }

    /// Collapse every selection to its displayed cursor, preserving multiple cursors and the primary. Normal/select cursors cover one whole grapheme, or remain empty at EOF.
    fn collapse_selection(ctx) {
        let editor = &mut *ctx.editor;
        let ranges = editor.selections.ranges().iter().map(|&selection| {
            Ok(Selection::cursor(position(editor, selection)?))
        }).collect::<Result<Vec<_>, Error>>()?;
        let primary = editor.selections.primary_index();
        select_ranges(editor, ranges, primary)
    }

    /// Keep only the primary selection, preserving its direction and normal/select mode.
    fn keep_primary_selection(ctx) {
        let primary = ctx.editor.selections.primary();
        select_ranges(ctx.editor, vec![primary], 0)
    }

    /// Expand selections to the full logical lines they touch, including line endings and preserving direction. Repeating this command does not add lines; a range ending at the next line's start excludes that line.
    fn extend_to_line_bounds(ctx) {
        let editor = &mut *ctx.editor;
        let ranges = editor.selections.ranges().iter().map(|&selection| {
            let (bounds, _) = selected_line_bounds(editor.document.text(), selection);
            if selection.is_backward() { Selection::new(bounds.end(), bounds.start()) } else { bounds }
        }).collect();
        let primary = editor.selections.primary_index();
        select_ranges(editor, ranges, primary)
    }

    /// Trim Unicode whitespace from selection edges without editing text or splitting graphemes. Remove empty/whitespace-only selections; retain the primary if it survives, otherwise use the last survivor.
    /// If no selection survives, keep a single cursor at the original primary's displayed position. Retains normal/select mode and leaves undo history and the yank register unchanged.
    fn trim_selections(ctx) {
        let editor = &mut *ctx.editor;
        let text = editor.document.text();
        let mut ranges = Vec::with_capacity(editor.selections.ranges().len());
        let mut primary = None;
        for (index, &selection) in editor.selections.ranges().iter().enumerate() {
            let slice = text.slice(selection.start().0..selection.end().0);
            let Some(leading) = slice.chars().position(|ch| !ch.is_whitespace()) else { continue };
            let trailing = slice.chars_at(slice.len_chars()).reversed().take_while(|ch| ch.is_whitespace()).count();
            // A whitespace scalar can belong to a cluster with non-whitespace
            // combining marks. Keep that whole cluster rather than cutting it.
            let start = grapheme::floor(text, CharOffset(selection.start().0 + leading))?;
            let end = grapheme::ceil(text, CharOffset(selection.end().0 - trailing))?;
            if index == editor.selections.primary_index() { primary = Some(ranges.len()); }
            ranges.push(if selection.is_backward() { Selection::new(end, start) } else { Selection::new(start, end) });
        }
        if ranges.is_empty() {
            ranges.push(Selection::cursor(position(editor, editor.selections.primary())?));
        }
        let primary = primary.unwrap_or(ranges.len() - 1);
        select_ranges(editor, ranges, primary)
    }

    /// Toggle select mode; movements in select mode retain the anchor grapheme.
    fn select_mode(ctx) {
        ctx.editor.finish_undo_group();
        ctx.editor.mode = if ctx.editor.mode == Mode::Select { Mode::Normal } else { Mode::Select };
        normalize(ctx.editor)
    }

    /// Enter normal mode. Leaving insert mode places the cursor on the preceding grapheme in the same line.
    fn normal_mode(ctx) {
        let editor = &mut *ctx.editor;
        editor.finish_undo_group();
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

    /// Enter insert mode at the first non-whitespace character on each cursor's line, or its start if blank. Carets on the same line merge; does not infer indentation or use a count.
    fn insert_at_line_start(ctx) { crate::editing::insert_at_line_edge(ctx.editor, false) }

    /// Enter insert mode before the line ending on each cursor's line. Carets on the same line merge; does not infer indentation or use a count.
    fn insert_at_line_end(ctx) { crate::editing::insert_at_line_edge(ctx.editor, true) }

    /// Replace each selected grapheme with the following character in one undo step, retaining selection direction and returning to normal mode. Enter uses the buffer's line ending; Tab inserts a literal tab. Empty EOF cursors do nothing; ignores counts and leaves the yank register unchanged.
    fn replace(ctx) [Character] { crate::editing::replace(ctx) }

    /// Indent each selected nonblank line once, using the buffer's language indentation settings. Counts add levels; spaces advance to an indent boundary. Retains selected text and returns to normal mode in one undo step.
    fn indent(ctx) { crate::editing::indent(ctx, false) }

    /// Remove up to a counted number of indentation levels from each selected line, measuring tabs at the buffer's tab stops. Retains selected text and returns to normal mode in one undo step.
    fn unindent(ctx) { crate::editing::indent(ctx, true) }

    /// Join lines within each selection; a single-line selection joins the next line. Remove the line break and following indentation, adding a separating space if needed. Shared joins happen once; counts are ignored, selections and normal/select mode are retained. Comment prefixes are kept literally.
    fn join_selections(ctx) { crate::editing::join(ctx.editor) }

    /// Add a counted number of empty lines above each selection without entering insert mode. Shared insertion points are handled once; uses the buffer's line ending and retains selections on the original text in one undo step.
    fn add_newline_above(ctx) { crate::editing::add_newlines(ctx, false) }

    /// Add a counted number of empty lines below each selection without entering insert mode. Shared insertion points are handled once; uses the buffer's line ending and retains selections on the original text in one undo step.
    fn add_newline_below(ctx) { crate::editing::add_newlines(ctx, true) }

    /// Open lines below each selection and enter insert mode, copying indentation. A count creates that many lines and carets; opening and subsequent typing share one undo step.
    /// Uses the loaded line ending; multiple selections ending on the same line share the new lines.
    fn open_below(ctx) { open_lines(ctx, true) }

    /// Open lines above each selection and enter insert mode, copying indentation. A count creates that many lines and carets; opening and subsequent typing share one undo step.
    /// Uses the loaded line ending; multiple selections starting on the same line share the new lines.
    fn open_above(ctx) { open_lines(ctx, false) }

    /// Insert a newline at every insert caret, copying leading tabs and spaces before that caret. Uses the loaded line ending and continues the typing undo group; requires insert mode.
    /// Indentation is copied literally, without language-specific increases or decreases. Pasted and directly inserted text remains unchanged.
    fn insert_newline(ctx) {
        let editor = &mut *ctx.editor;
        require_insert(editor)?;
        let text = editor.document.text();
        let edits = editor.selections.ranges().iter().map(|selection| {
            let head = selection.head;
            let start = motion::line_start(text, head)?;
            // If the caret splits indentation, only copy the prefix before it:
            // the remaining whitespace already follows the caret on the new line.
            let indent = leading_indent(text.slice(start.0..head.0));
            Ok(Edit::insert(head, format!("{}{indent}", editor.newline())))
        }).collect::<Result<Vec<_>, vex_core::Error>>()?;
        let transaction = editor.document.transaction(edits)?;
        editor.apply(transaction, true)?;
        normalize(editor)
    }

    /// Insert the context's text at all carets, continuing the typing undo group; requires insert mode.
    fn insert_text(ctx) { insert(ctx, true) }

    /// Start a new undo checkpoint without saving or leaving insert mode. Subsequent typing and deletion form a new undo group.
    fn commit_undo_checkpoint(ctx) {
        require_insert(ctx.editor)?;
        ctx.editor.finish_undo_group();
        ctx.editor.document.finish_undo_group();
        Ok(())
    }

    /// Delete backward to the previous word start at every insert caret, including intervening whitespace. Counts repeat the word motion; leaves registers unchanged and continues the typing undo group.
    fn delete_word_backward(ctx) {
        erase_insert_range(ctx, |text, head, count| Ok(motion::word_backward(text, Selection::cursor(head), count)?.head))
    }

    /// Delete from each insert caret back to the first non-whitespace character, or the line start when within indentation. At line start, remove the preceding line ending. Continues the typing undo group and leaves registers unchanged.
    fn kill_to_line_start(ctx) {
        erase_insert_range(ctx, |text, head, _| {
            let start = motion::line_start(text, head)?;
            if head == start && head.0 > 0 {
                grapheme::previous(text, head, 1)
            } else {
                // Only text before the caret affects this deletion. In
                // particular, a huge indentation suffix must not slow Ctrl-u
                // near the start of a line.
                let first = text.slice(start.0..head.0).chars().position(|ch| !ch.is_whitespace());
                match first {
                    Some(offset) => grapheme::floor(text, CharOffset(start.0 + offset)),
                    None => Ok(start),
                }
            }
        })
    }

    /// Delete from every insert caret to the line end; at the line ending, delete that entire ending instead. Continues the typing undo group and leaves registers unchanged.
    fn kill_to_line_end(ctx) {
        erase_insert_range(ctx, |text, head, _| {
            let end = motion::line_end(text, head)?;
            if head == end { grapheme::next(text, head, 1) } else { Ok(end) }
        })
    }

    /// Insert the context's pasted text at all carets as a separate undo step; requires insert mode.
    fn insert_paste(ctx) { insert(ctx, false) }

    /// Copy selections to the chosen register (default: last-yanked text), retaining their order and leaving select mode. Registers are shared across buffers; undo history is unchanged.
    fn yank(ctx) {
        let name = ctx.register.unwrap_or('"');
        if let Some(kind) = crate::ClipboardKind::from_register(name) {
            ctx.editor.request_clipboard(kind, crate::ClipboardAction::Yank, ctx.count.get());
            return Ok(());
        }
        let values = crate::register::capture_for(ctx.editor, name)?;
        ctx.editor.set_register(name, values)?;
        ctx.editor.finish_undo_group();
        ctx.editor.mode = Mode::Normal;
        normalize(ctx.editor)
    }

    /// Paste the chosen register (default: last-yanked text) after selections, selecting the inserted text in normal mode. Newline-terminated yanks paste below the selected lines; counts repeat each fragment in one undo step.
    /// Fragments pair with selections in document order; extra destinations repeat the last fragment. Uses the destination's line endings without changing the register.
    fn paste_after(ctx) { crate::register::paste(ctx, crate::register::Paste::After) }

    /// Paste the chosen register (default: last-yanked text) before selections, selecting the inserted text in normal mode. Newline-terminated yanks paste above the selected lines; counts repeat each fragment in one undo step.
    /// Fragments pair with selections in document order; extra destinations repeat the last fragment. Uses the destination's line endings without changing the register.
    fn paste_before(ctx) { crate::register::paste(ctx, crate::register::Paste::Before) }

    /// Replace selections with the chosen register (default: last-yanked text) in one undo step, selecting the replacements in normal mode. Counts repeat each fragment; replacement leaves the register unchanged.
    /// Fragments pair in document order, repeating the last for extra destinations. Replaces the exact ranges even for linewise yanks, using the destination's line endings.
    fn replace_with_yanked(ctx) { crate::register::paste(ctx, crate::register::Paste::Replace) }

    /// Cut selections into the chosen register (default: last-yanked text) and delete them atomically, leaving normal-mode cursors at the edit locations. The discard register avoids copying selected text.
    fn delete_selection(ctx) {
        let name = ctx.register.unwrap_or('"');
        if let Some(kind) = crate::ClipboardKind::from_register(name) {
            ctx.editor.request_clipboard(kind, crate::ClipboardAction::Delete, ctx.count.get());
            return Ok(());
        }
        let values = crate::register::capture_for(ctx.editor, name)?;
        delete_selection_without_yank(ctx)?;
        ctx.editor.set_register(name, values)?;
        Ok(())
    }

    /// Delete selections without changing the yank register. Used for internal buffer cleanup; leaves normal-mode cursors at the edit locations.
    fn delete_selection_without_yank(ctx) {
        let editor = &mut *ctx.editor;
        let transaction = editor.document.replace_selections(&editor.selections, "")?;
        editor.apply(transaction, false)?;
        editor.mode = Mode::Normal;
        normalize(editor)
    }

    /// Cut selections into the chosen register (default: last-yanked text) and enter insert mode; the deletion and subsequent typing share one undo step. The discard register avoids copying selected text.
    fn change_selection(ctx) {
        let name = ctx.register.unwrap_or('"');
        if let Some(kind) = crate::ClipboardKind::from_register(name) {
            ctx.editor.request_clipboard(kind, crate::ClipboardAction::Change, ctx.count.get());
            return Ok(());
        }
        let values = crate::register::capture_for(ctx.editor, name)?;
        let editor = &mut *ctx.editor;
        editor.finish_undo_group();
        let transaction = editor.document.replace_selections(&editor.selections, "")?;
        editor.apply(transaction, true)?;
        editor.set_register(name, values)?;
        editor.mode = Mode::Insert;
        normalize(editor)
    }

    /// Delete preceding graphemes at all insert carets, continuing the typing undo group; accepts a count.
    fn delete_backward(ctx) { erase(ctx, true) }

    /// Delete following graphemes at all insert carets, continuing the typing undo group; accepts a count.
    fn delete_forward(ctx) { erase(ctx, false) }

    /// Undo edit groups and restore their selections; accepts a count of groups.
    fn undo(ctx) {
        ctx.editor.finish_undo_group();
        for _ in 0..ctx.count.get() {
            if !ctx.editor.document.undo(&mut ctx.editor.selections)? { break; }
            ctx.editor.synchronize_caches();
        }
        normalize(ctx.editor)
    }

    /// Redo edit groups and restore their selections; accepts a count of groups.
    fn redo(ctx) {
        ctx.editor.finish_undo_group();
        for _ in 0..ctx.count.get() {
            if !ctx.editor.document.redo(&mut ctx.editor.selections)? { break; }
            ctx.editor.synchronize_caches();
        }
        normalize(ctx.editor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vex_core::Document;

    #[test]
    fn insert_line_kills_preserve_indent_then_remove_it_and_whole_line_endings() {
        let mut editor = Editor::new(Document::from("prev\r\n  hello tail"));
        editor.execute("insert_mode", 1).unwrap();
        editor
            .set_selections(SelectionSet::single(Selection::cursor(CharOffset(13))))
            .unwrap();
        editor.execute("kill_to_line_start", 1).unwrap();
        assert_eq!(editor.document().text(), "prev\r\n   tail");
        assert_eq!(editor.selections().primary().head, CharOffset(8));
        editor.execute("kill_to_line_start", 1).unwrap();
        assert_eq!(editor.document().text(), "prev\r\n tail");
        editor.execute("kill_to_line_start", 1).unwrap();
        assert_eq!(editor.document().text(), "prev tail");
        assert_eq!(editor.document().undo_depth(), 1);
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document().text(), "prev\r\n  hello tail");
        editor.execute("kill_to_line_end", 1).unwrap();
        assert_eq!(editor.document().text(), "prev\r\n  hello");
        editor.execute("undo", 1).unwrap();
        editor
            .set_selections(SelectionSet::single(Selection::cursor(CharOffset(4))))
            .unwrap();
        editor.execute("kill_to_line_end", 1).unwrap();
        assert_eq!(editor.document().text(), "prev  hello tail");
    }

    #[test]
    fn insert_word_deletion_handles_counts_unicode_and_overlapping_carets() {
        let mut editor = Editor::new(Document::from("one e\u{301}界"));
        editor.execute("insert_at_line_end", 1).unwrap();
        editor.execute("delete_word_backward", 1).unwrap();
        assert_eq!(editor.document().text(), "one ");
        editor.execute("delete_word_backward", 1).unwrap();
        assert_eq!(editor.document().text(), "");
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document().text(), "one e\u{301}界");
        editor.execute("delete_word_backward", 2).unwrap();
        assert_eq!(editor.document().text(), "");
        editor.execute("undo", 1).unwrap();
        editor
            .set_selections(
                SelectionSet::new(
                    vec![
                        Selection::cursor(CharOffset(5)),
                        Selection::cursor(CharOffset(7)),
                    ],
                    1,
                )
                .unwrap(),
            )
            .unwrap();
        editor.execute("kill_to_line_start", 1).unwrap();
        assert_eq!(editor.document().text(), "");
        assert_eq!(
            editor.selections().ranges(),
            &[Selection::cursor(CharOffset(0))]
        );
        assert_eq!(editor.selections().primary_index(), 0);
    }

    #[test]
    fn insert_kills_require_insert_mode_and_checkpoints_split_undo_without_edits() {
        for command in [
            "delete_word_backward",
            "kill_to_line_start",
            "kill_to_line_end",
            "commit_undo_checkpoint",
        ] {
            let mut editor = Editor::new(Document::default());
            assert!(matches!(
                editor.execute(command, 1),
                Err(Error::WrongMode { .. })
            ));
            editor.execute("insert_mode", 1).unwrap();
            editor.execute(command, 1).unwrap();
            assert_eq!(editor.document().undo_depth(), 0);
        }
        let mut editor = Editor::new(Document::default());
        editor.execute("insert_mode", 1).unwrap();
        editor.insert_text("first").unwrap();
        let revision = editor.document().revision();
        editor.execute("commit_undo_checkpoint", 1).unwrap();
        assert_eq!(editor.document().revision(), revision);
        editor.insert_text(" second").unwrap();
        editor.execute("delete_word_backward", 1).unwrap();
        editor.insert_text("third").unwrap();
        assert_eq!(editor.document().undo_depth(), 2);
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document().text(), "first");
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document().text(), "");
    }

    #[test]
    fn find_commands_are_callable_with_character_context_and_preserve_unmatched_selections() {
        let mut editor = Editor::new(Document::from("a:b\nc:d\nlast"));
        editor
            .set_selections(
                SelectionSet::new(
                    vec![
                        Selection::cursor(CharOffset(0)),
                        Selection::cursor(CharOffset(4)),
                        Selection::cursor(CharOffset(8)),
                    ],
                    2,
                )
                .unwrap(),
            )
            .unwrap();
        let before = editor.selections().clone();
        assert_eq!(
            find_next_char(&mut CommandContext::new(&mut editor)),
            Err(Error::MissingCharacter)
        );
        assert_eq!(editor.selections(), &before);
        let mut context = CommandContext::new(&mut editor);
        context.character = Some(':');
        find_next_char(&mut context).unwrap();
        assert_eq!(
            editor.selections().ranges(),
            &[
                Selection::new(CharOffset(0), CharOffset(2)),
                Selection::new(CharOffset(4), CharOffset(6)),
                Selection::new(CharOffset(8), CharOffset(9)),
            ]
        );
        assert_eq!(editor.selections().primary_index(), 2);
        assert_eq!(editor.document().undo_depth(), 0);
    }

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
    fn select_all_replaces_multiple_ranges_and_preserves_select_mode() {
        for mode in [Mode::Normal, Mode::Select] {
            let mut editor = Editor::new(Document::from("ae\u{301}🦀\r\nz"));
            editor
                .set_selections(SelectionSet::new(vec![range(0, 1), range(4, 3)], 1).unwrap())
                .unwrap();
            if mode == Mode::Select {
                editor.execute("select_mode", 1).unwrap();
            }
            editor.execute("select_all", 99).unwrap();
            assert_eq!(editor.selections(), &SelectionSet::single(range(0, 7)));
            assert_eq!(editor.mode(), mode);
            assert_eq!(editor.document().revision().get(), 0);
        }
    }

    #[test]
    fn collapse_uses_displayed_cursors_and_retains_multiple_cursors_and_primary() {
        let mut editor = Editor::new(Document::from("ae\u{301}🦀\r\nz"));
        editor
            .set_selections(
                SelectionSet::new(vec![range(0, 3), range(6, 3), range(7, 7)], 1).unwrap(),
            )
            .unwrap();
        editor.execute("select_mode", 1).unwrap();
        editor.execute("collapse_selection", 1).unwrap();
        assert_eq!(
            editor.selections().ranges(),
            &[range(1, 3), range(3, 4), range(7, 7)]
        );
        assert_eq!(editor.selections().primary_index(), 1);
        assert_eq!(editor.mode(), Mode::Select);
        editor
            .set_selections(SelectionSet::single(range(3, 6)))
            .unwrap();
        editor.execute("collapse_selection", 1).unwrap();
        assert_eq!(editor.selections().primary(), range(4, 6));
    }

    #[test]
    fn keeping_primary_preserves_its_full_range_direction_and_mode() {
        let mut editor = Editor::new(Document::from("one two three"));
        editor
            .set_selections(
                SelectionSet::new(vec![range(0, 3), range(7, 4), range(8, 13)], 1).unwrap(),
            )
            .unwrap();
        editor.execute("select_mode", 1).unwrap();
        editor.execute("keep_primary_selection", 10).unwrap();
        assert_eq!(editor.selections(), &SelectionSet::single(range(7, 4)));
        assert_eq!(editor.mode(), Mode::Select);
    }

    #[test]
    fn line_bounds_preserve_direction_and_do_not_include_the_next_line_or_repeat() {
        for (before, after) in [
            (range(1, 5), range(0, 8)),
            (range(5, 1), range(8, 0)),
            (range(0, 4), range(0, 4)),
            (range(4, 0), range(4, 0)),
            (range(2, 4), range(0, 4)),
            (range(10, 10), range(8, 10)),
        ] {
            let mut editor = Editor::new(Document::from("aa\r\nbb\r\ncc"));
            editor.set_selections(SelectionSet::single(before)).unwrap();
            editor.execute("extend_to_line_bounds", 1).unwrap();
            assert_eq!(editor.selections().primary(), after);
            editor.execute("extend_to_line_bounds", usize::MAX).unwrap();
            assert_eq!(editor.selections().primary(), after);
        }
    }

    #[test]
    fn repeated_line_selection_extends_counts_without_losing_earlier_lines() {
        let mut editor = Editor::new(Document::from("one\ntwo\nthree\nfour\nlast"));
        editor.execute("select_line", 1).unwrap();
        assert_eq!(editor.selections().primary(), range(0, 4));
        editor.execute("select_line", 2).unwrap();
        assert_eq!(editor.selections().primary(), range(0, 14));
        editor.execute("select_line", 1).unwrap();
        assert_eq!(editor.selections().primary(), range(0, 19));
        editor.execute("select_line", usize::MAX).unwrap();
        assert_eq!(editor.selections().primary(), range(0, 23));
        editor.execute("select_line", usize::MAX).unwrap();
        assert_eq!(editor.selections().primary(), range(0, 23));
        for selection in [range(1, 6), range(6, 1)] {
            editor
                .set_selections(SelectionSet::single(selection))
                .unwrap();
            editor.execute("select_line", 2).unwrap();
            assert_eq!(editor.selections().primary(), range(0, 14));
        }
    }

    #[test]
    fn line_counts_apply_independently_and_merging_tracks_the_primary() {
        let mut editor = Editor::new(Document::from("aa\nbb\ncc\ndd\nee"));
        editor
            .set_selections(
                SelectionSet::new(vec![range(0, 3), range(4, 5), range(14, 12)], 2).unwrap(),
            )
            .unwrap();
        editor.execute("select_line", 1).unwrap();
        // The already-aligned first range adds a line; the second only aligns.
        assert_eq!(editor.selections().ranges(), &[range(0, 6), range(12, 14)]);
        assert_eq!(editor.selections().primary_index(), 1);
        editor.execute("select_line", 2).unwrap();
        assert_eq!(editor.selections().ranges(), &[range(0, 12), range(12, 14)]);
        editor.execute("select_line", 1).unwrap();
        assert_eq!(editor.selections(), &SelectionSet::single(range(0, 14)));

        editor
            .set_selections(SelectionSet::new(vec![range(0, 1), range(2, 1)], 1).unwrap())
            .unwrap();
        editor.execute("extend_to_line_bounds", 1).unwrap();
        assert_eq!(editor.selections(), &SelectionSet::single(range(3, 0)));
    }

    #[test]
    fn trimming_removes_blank_ranges_and_keeps_or_reassigns_the_primary() {
        for primary in 0..3 {
            let mut editor = Editor::new(Document::from("  one \t two  \n   "));
            editor
                .set_selections(
                    SelectionSet::new(vec![range(0, 7), range(13, 7), range(14, 17)], primary)
                        .unwrap(),
                )
                .unwrap();
            editor.execute("select_mode", 1).unwrap();
            editor.execute("trim_selections", 1).unwrap();
            assert_eq!(editor.selections().ranges(), &[range(2, 5), range(11, 8)]);
            assert_eq!(editor.selections().primary_index(), primary.min(1));
            assert_eq!(editor.mode(), Mode::Select);
        }
    }

    #[test]
    fn trimming_all_whitespace_keeps_only_the_original_primary_cursor() {
        for (primary, expected) in [(0, range(1, 2)), (1, range(2, 4)), (2, range(5, 5))] {
            let mut editor = Editor::new(Document::from(" \t\r\n "));
            editor
                .set_selections(
                    SelectionSet::new(vec![range(0, 2), range(4, 2), range(5, 5)], primary)
                        .unwrap(),
                )
                .unwrap();
            editor.execute("select_mode", 1).unwrap();
            editor.execute("trim_selections", 1).unwrap();
            assert_eq!(editor.selections(), &SelectionSet::single(expected));
            assert_eq!(editor.mode(), Mode::Select);
        }
    }

    #[test]
    fn trimming_unicode_whitespace_keeps_combining_clusters_intact() {
        for (text, expected) in [
            (" \u{a0}e\u{301}🦀\u{2003}\r\n", range(5, 2)),
            (" \u{301} e\u{301} \u{2003}", range(5, 0)),
            ("hi \u{301} ", range(4, 0)),
        ] {
            let mut editor = Editor::new(Document::from(text));
            let len = editor.document().text().len_chars();
            editor
                .set_selections(SelectionSet::single(range(len, 0)))
                .unwrap();
            editor.execute("trim_selections", 1).unwrap();
            assert_eq!(editor.selections().primary(), expected);
            assert!(grapheme::is_boundary(editor.document().text(), expected.head).unwrap());
            assert!(grapheme::is_boundary(editor.document().text(), expected.anchor).unwrap());
        }
        let source = format!("{}e\u{301}{}", " \t".repeat(2000), "\r\n".repeat(2000));
        let mut editor = Editor::new(Document::from(source.as_str()));
        editor.execute("select_all", 1).unwrap();
        let snapshot = editor.document().snapshot();
        editor.execute("trim_selections", 1).unwrap();
        assert_eq!(editor.selections().primary(), range(4000, 4002));
        assert!(editor.document().text().is_instance(snapshot.text()));
    }

    #[test]
    fn selection_controls_handle_empty_buffers_without_changes() {
        let mut editor = Editor::new(Document::default());
        editor.execute("select_mode", 1).unwrap();
        for command in [
            "select_all",
            "collapse_selection",
            "keep_primary_selection",
            "extend_to_line_bounds",
            "trim_selections",
            "select_line",
        ] {
            editor.execute(command, usize::MAX).unwrap();
            assert_eq!(editor.selections(), &SelectionSet::single(range(0, 0)));
            assert_eq!(editor.mode(), Mode::Select);
            assert_eq!(editor.document().revision().get(), 0);
            assert_eq!(editor.document().undo_depth(), 0);
        }
    }

    #[test]
    fn selection_controls_preserve_redo_and_yanked_text() {
        for command in [
            "select_all",
            "collapse_selection",
            "keep_primary_selection",
            "extend_to_line_bounds",
            "trim_selections",
            "select_line",
        ] {
            let mut editor = Editor::new(Document::from(" one\n two\n"));
            editor.execute("select_all", 1).unwrap();
            editor.execute("yank", 1).unwrap();
            editor.execute("insert_mode", 1).unwrap();
            editor.insert_text("edit").unwrap();
            editor.execute("normal_mode", 1).unwrap();
            editor.execute("undo", 1).unwrap();
            let revision = editor.document().revision();
            editor.execute(command, 1).unwrap();
            assert_eq!(editor.document().revision(), revision);
            assert_eq!(editor.document().redo_depth(), 1);
            assert_eq!(editor.document().undo_depth(), 0);
            let mut other = Editor::with_yank_register(Document::default(), editor.yank_register());
            other.execute("paste_before", 1).unwrap();
            assert_eq!(other.document().text(), " one\n two\n");
        }
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

    #[test]
    fn directly_called_movements_and_mode_commands_separate_typing() {
        let boundaries = [
            move_left,
            move_right,
            move_up,
            move_down,
            move_word_forward,
            move_word_backward,
            move_word_end,
            move_long_word_forward,
            move_long_word_backward,
            move_long_word_end,
            goto_first_nonwhitespace,
            goto_column,
            goto_line_start,
            goto_line_end,
            goto_file_start,
            goto_file_end,
            select_line,
            select_all,
            collapse_selection,
            keep_primary_selection,
            extend_to_line_bounds,
            trim_selections,
            select_mode,
            normal_mode,
            insert_mode,
            append_mode,
        ];
        for boundary in boundaries {
            let mut editor = Editor::new(Document::from("one\ntwo"));
            insert_mode(&mut CommandContext::new(&mut editor)).unwrap();
            editor.insert_text("a").unwrap();
            editor.insert_text("b").unwrap();
            let before = editor.document().snapshot();
            boundary(&mut CommandContext::new(&mut editor)).unwrap();
            if editor.mode() != Mode::Insert {
                insert_mode(&mut CommandContext::new(&mut editor)).unwrap();
            }
            let selections = editor.selections().clone();
            editor.insert_text("c").unwrap();
            editor.insert_text("d").unwrap();
            assert_eq!(editor.document().undo_depth(), 2);
            undo(&mut CommandContext::new(&mut editor)).unwrap();
            assert!(editor.document().text().is_instance(before.text()));
            assert_eq!(editor.selections(), &selections);
        }
    }

    #[test]
    fn changing_multiple_selections_and_typing_is_one_undo_group() {
        let mut editor = Editor::new(Document::from("one\ntwo"));
        let before = SelectionSet::new(vec![range(3, 0), range(4, 7)], 1).unwrap();
        editor.set_selections(before.clone()).unwrap();
        change_selection(&mut CommandContext::new(&mut editor)).unwrap();
        for text in ["e", "\u{301}", "🦀"] {
            editor.insert_text(text).unwrap();
        }
        assert_eq!(editor.document().text(), "e\u{301}🦀\ne\u{301}🦀");
        assert_eq!(editor.document().undo_depth(), 1);
        normal_mode(&mut CommandContext::new(&mut editor)).unwrap();
        undo(&mut CommandContext::new(&mut editor)).unwrap();
        assert_eq!(editor.document().text(), "one\ntwo");
        assert_eq!(editor.selections(), &before);
        redo(&mut CommandContext::new(&mut editor)).unwrap();
        assert_eq!(editor.document().text(), "e\u{301}🦀\ne\u{301}🦀");
        assert_eq!(editor.selections().ranges(), &[range(3, 4), range(7, 7)]);
        assert_eq!(editor.selections().primary_index(), 1);
    }

    #[test]
    fn opening_lines_handles_empty_files_eof_and_copies_indentation() {
        for (source, command, expected) in [
            ("", "open_below", "\nx"),
            ("", "open_above", "x\n"),
            ("one", "open_below", "one\nx"),
            ("one", "open_above", "x\none"),
            ("one\n", "open_below", "one\nx\n"),
            ("one\n", "open_above", "x\none\n"),
            ("\t  one\nlast", "open_below", "\t  one\n\t  x\nlast"),
            ("\t  one\nlast", "open_above", "\t  x\n\t  one\nlast"),
            ("  ", "open_below", "  \n  x"),
            ("  ", "open_above", "  x\n  "),
        ] {
            let mut editor = Editor::new(Document::from(source));
            editor.execute(command, 1).unwrap();
            assert_eq!(editor.mode(), Mode::Insert);
            editor.insert_text("x").unwrap();
            assert_eq!(editor.document().text(), expected, "{command}: {source:?}");
        }
        for command in ["open_below", "open_above"] {
            let mut editor = Editor::new(Document::from("one\n"));
            editor.execute("goto_file_end", 1).unwrap();
            editor.execute(command, 1).unwrap();
            editor.insert_text("x").unwrap();
            let expected = if command == "open_below" {
                "one\n\nx"
            } else {
                "one\nx\n"
            };
            assert_eq!(editor.document().text(), expected);
        }
    }

    #[test]
    fn newline_copies_indentation_without_duplicating_whitespace_after_the_caret() {
        for (source, expected) in [
            ("|", "\n|"),
            ("plain|", "plain\n|"),
            ("  item|", "  item\n  |"),
            ("    item|", "    item\n    |"),
            ("\titem|", "\titem\n\t|"),
            ("\t  a|b", "\t  a\n\t  |b"),
            ("|    item", "\n|    item"),
            ("  |  item", "  \n  |  item"),
            ("    |item", "    \n    |item"),
            ("\t | item", "\t \n\t | item"),
            ("  |  ", "  \n  |  "),
            ("   |", "   \n   |"),
            ("\t界e\u{301}|🦀", "\t界e\u{301}\n\t|🦀"),
            // Copying indentation does not infer another level from syntax.
            ("  if ready {|", "  if ready {\n  |"),
            ("  if ready:|", "  if ready:\n  |"),
        ] {
            let (before, after) = source.split_once('|').unwrap();
            let document = Document::from(format!("{before}{after}").as_str());
            let mut editor = Editor::new(document);
            editor.execute("insert_mode", 1).unwrap();
            let pos = before.chars().count();
            editor
                .set_selections(SelectionSet::single(range(pos, pos)))
                .unwrap();
            insert_newline(&mut CommandContext::new(&mut editor)).unwrap();
            let (before, after) = expected.split_once('|').unwrap();
            assert_eq!(
                editor.document().text(),
                format!("{before}{after}").as_str(),
                "{source:?}"
            );
            let pos = before.chars().count();
            assert_eq!(editor.selections().primary(), range(pos, pos), "{source:?}");
        }
    }

    #[test]
    fn newline_preserves_line_endings_and_groups_with_typing() {
        for newline in ["\n", "\r\n", "\r"] {
            let source = format!("  one{newline}\t two");
            let mut editor = Editor::new(Document::from(source.as_str()));
            editor.execute("goto_file_end", 1).unwrap();
            editor.execute("insert_mode", 1).unwrap();
            let selections = editor.selections().clone();
            editor.insert_text("A").unwrap();
            editor.execute("insert_newline", 1).unwrap();
            editor.insert_text("B").unwrap();
            let expected = format!("{source}A{newline}\t B");
            assert_eq!(editor.document().text(), expected.as_str());
            assert_eq!(editor.document().undo_depth(), 1);
            editor.execute("undo", 1).unwrap();
            assert_eq!(editor.document().text(), source.as_str());
            assert_eq!(editor.selections(), &selections);
            editor.execute("redo", 1).unwrap();
            assert_eq!(editor.document().text(), expected.as_str());
        }
    }

    #[test]
    fn newline_handles_multiple_carets_on_the_same_and_different_lines_atomically() {
        let mut editor = Editor::new(Document::from("  ab\n\tcd"));
        editor.execute("insert_mode", 1).unwrap();
        let before = SelectionSet::new(vec![range(3, 3), range(4, 4), range(8, 8)], 2).unwrap();
        editor.set_selections(before.clone()).unwrap();
        editor.execute("insert_newline", 1).unwrap();
        assert_eq!(editor.document().text(), "  a\n  b\n  \n\tcd\n\t");
        assert_eq!(
            editor.selections().ranges(),
            &[range(6, 6), range(10, 10), range(16, 16)]
        );
        assert_eq!(editor.selections().primary_index(), 2);
        assert_eq!(editor.document().revision().get(), 1);
        editor.insert_text("x").unwrap();
        assert_eq!(editor.document().text(), "  a\n  xb\n  x\n\tcd\n\tx");
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document().text(), "  ab\n\tcd");
        assert_eq!(editor.selections(), &before);
    }

    #[test]
    fn newline_requires_insert_mode_and_raw_text_and_paste_keep_literal_newlines() {
        let mut editor = Editor::new(Document::from("  one"));
        for mode in [Mode::Normal, Mode::Select] {
            assert_eq!(editor.mode(), mode);
            assert_eq!(
                editor.execute("insert_newline", 1),
                Err(Error::WrongMode {
                    expected: Mode::Insert,
                    actual: mode
                })
            );
            assert_eq!(editor.document().text(), "  one");
            assert_eq!(editor.document().undo_depth(), 0);
            editor.execute("select_mode", 1).unwrap();
        }
        editor.execute("goto_file_end", 1).unwrap();
        editor.execute("insert_mode", 1).unwrap();
        editor.insert_text("\n  raw").unwrap();
        editor.insert_paste("\n  pasted\nend").unwrap();
        assert_eq!(editor.document().text(), "  one\n  raw\n  pasted\nend");
    }

    #[test]
    fn opening_uses_selection_edges_in_either_direction_and_preserves_line_endings() {
        for newline in ["\n", "\r\n", "\r"] {
            let source = format!("one{newline}\t two{newline}three");
            let end = 8 + 2 * newline.len();
            for selection in [range(0, end), range(end, 0)] {
                for command in ["open_above", "open_below"] {
                    let mut editor = Editor::new(Document::from(source.as_str()));
                    editor
                        .set_selections(SelectionSet::single(selection))
                        .unwrap();
                    editor.execute("select_mode", 1).unwrap();
                    editor.execute(command, 1).unwrap();
                    editor.insert_text("x").unwrap();
                    let expected = if command == "open_above" {
                        format!("x{newline}{source}")
                    } else {
                        format!("one{newline}\t two{newline}\t x{newline}three")
                    };
                    assert_eq!(editor.document().text(), expected.as_str());
                    editor.execute("normal_mode", 1).unwrap();
                    editor.execute("undo", 1).unwrap();
                    assert_eq!(editor.document().text(), source.as_str());
                    assert_eq!(editor.selections().primary(), selection);
                }
            }
        }
    }

    #[test]
    fn opening_counted_lines_merges_duplicate_lines_preserves_primary_and_undo() {
        for command in ["open_below", "open_above"] {
            for primary in [1, 2] {
                let mut editor = Editor::new(Document::from("ab\ncd"));
                let before =
                    SelectionSet::new(vec![range(0, 1), range(2, 1), range(3, 4)], primary)
                        .unwrap();
                editor.set_selections(before.clone()).unwrap();
                editor.execute(command, 2).unwrap();
                assert_eq!(editor.selections().ranges().len(), 4);
                assert_eq!(
                    editor.selections().primary_index(),
                    if primary == 1 { 0 } else { 2 }
                );
                for text in ["e", "\u{301}", "🦀"] {
                    editor.insert_text(text).unwrap();
                }
                let typed = "e\u{301}🦀";
                let expected = if command == "open_below" {
                    format!("ab\n{typed}\n{typed}\ncd\n{typed}\n{typed}")
                } else {
                    format!("{typed}\n{typed}\nab\n{typed}\n{typed}\ncd")
                };
                assert_eq!(editor.document().text(), expected.as_str());
                assert_eq!(editor.document().undo_depth(), 1);
                editor.execute("normal_mode", 1).unwrap();
                editor.execute("undo", 1).unwrap();
                assert_eq!(editor.document().text(), "ab\ncd");
                assert_eq!(editor.selections(), &before);
                editor.execute("redo", 1).unwrap();
                assert_eq!(editor.document().text(), expected.as_str());
                assert_eq!(
                    editor.selections().primary_index(),
                    if primary == 1 { 0 } else { 2 }
                );
            }
        }
    }

    #[test]
    fn opening_lines_separates_prior_typing_and_rejects_impossible_counts_atomically() {
        for command in [open_below, open_above] {
            let mut editor = Editor::new(Document::from("one"));
            insert_mode(&mut CommandContext::new(&mut editor)).unwrap();
            editor.insert_text("ab").unwrap();
            let before = editor.selections().clone();
            let mut context = CommandContext::new(&mut editor);
            context.count = NonZeroUsize::new(usize::MAX).unwrap();
            assert_eq!(
                command(&mut context),
                Err(Error::Core(vex_core::Error::LengthOverflow))
            );
            assert_eq!(editor.document().text(), "abone");
            assert_eq!(editor.selections(), &before);
            assert_eq!(editor.mode(), Mode::Insert);
            command(&mut CommandContext::new(&mut editor)).unwrap();
            editor.insert_text("xy").unwrap();
            assert_eq!(editor.document().undo_depth(), 2);
            editor.execute("undo", 1).unwrap();
            assert_eq!(editor.document().text(), "abone");
            assert_eq!(editor.selections(), &before);
            editor.execute("undo", 1).unwrap();
            assert_eq!(editor.document().text(), "one");
        }
    }

    #[test]
    fn backspace_and_delete_remain_in_the_typing_undo_group() {
        for command in [delete_backward, delete_forward] {
            let mut editor = Editor::new(Document::from("xyz"));
            insert_mode(&mut CommandContext::new(&mut editor)).unwrap();
            editor.insert_text("a").unwrap();
            editor.insert_text("b").unwrap();
            command(&mut CommandContext::new(&mut editor)).unwrap();
            editor.insert_text("c").unwrap();
            editor.insert_text("d").unwrap();
            assert_eq!(editor.document().undo_depth(), 1);
            editor.execute("undo", 1).unwrap();
            assert_eq!(editor.document().text(), "xyz");
        }
    }

    #[test]
    fn paste_separates_typing_and_restores_multiple_carets() {
        let mut editor = Editor::new(Document::from("a\nb"));
        insert_mode(&mut CommandContext::new(&mut editor)).unwrap();
        editor
            .set_selections(SelectionSet::new(vec![range(0, 0), range(2, 2)], 1).unwrap())
            .unwrap();
        editor.insert_text("x").unwrap();
        editor.insert_text("y").unwrap();
        let before = editor.document().snapshot();
        let before_carets = editor.selections().clone();
        editor.insert_paste("e\u{301}🦀\r\n").unwrap();
        let pasted = editor.document().snapshot();
        let pasted_carets = editor.selections().clone();
        editor.insert_text("z").unwrap();
        editor.insert_text("!").unwrap();
        assert_eq!(editor.document().undo_depth(), 3);
        editor.execute("undo", 1).unwrap();
        assert!(editor.document().text().is_instance(pasted.text()));
        assert_eq!(editor.selections(), &pasted_carets);
        editor.execute("undo", 1).unwrap();
        assert!(editor.document().text().is_instance(before.text()));
        assert_eq!(editor.selections(), &before_carets);
        editor.execute("redo", 1).unwrap();
        assert_eq!(editor.selections(), &pasted_carets);
        // Editing after undo abandons the remaining redo branch.
        editor.set_selections(before_carets).unwrap();
        editor.insert_text("new").unwrap();
        assert_eq!(editor.document().redo_depth(), 0);
        editor.execute("undo", 1).unwrap();
        assert!(editor.document().text().is_instance(pasted.text()));
    }
}
