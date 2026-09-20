//! Session-shared yank storage and atomic, selection-aware paste operations.

use std::{
    borrow::Cow,
    ops::Range,
    sync::{Arc, Mutex},
};
use vex_core::{
    Affinity, CharOffset, Edit, Selection, SelectionSet, Snapshot, Transaction, grapheme, motion,
};

use crate::{CommandContext, Editor, Error, Mode};

type Fragments = Arc<[Arc<str>]>;

/// An internal yank register shared by all buffers in an editor session.
/// Clones share immutable text; undo/redo and buffer lifetimes do not own it.
/// A standalone [`Editor::new`] starts with its own empty register.
#[derive(Clone, Debug, Default)]
pub struct YankRegister {
    values: Arc<Mutex<Fragments>>,
}

impl YankRegister {
    fn read(&self) -> Fragments {
        self.values.lock().expect("yank register lock").clone()
    }

    pub(crate) fn write(&self, values: Fragments) {
        *self.values.lock().expect("yank register lock") = values;
    }
}

pub(crate) fn capture(editor: &Editor) -> Fragments {
    editor
        .selections
        .ranges()
        .iter()
        .map(|selection| {
            Arc::from(
                editor
                    .document
                    .text()
                    .slice(selection.start().0..selection.end().0)
                    .to_string(),
            )
        })
        .collect()
}

/// Where supplied register fragments are inserted relative to selections.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Paste {
    Before,
    After,
    Replace,
}

/// Immutable preparation inputs for clipboard/register edits on a worker.
/// Cloning the snapshot shares rope storage rather than copying document text.
pub struct PastePlan {
    snapshot: Snapshot,
    selections: SelectionSet,
    newline: &'static str,
}

impl Editor {
    pub fn paste_plan(&self) -> PastePlan {
        PastePlan {
            snapshot: self.document.snapshot(),
            selections: self.selections.clone(),
            newline: self.newline(),
        }
    }

    /// Apply a prepared paste as one undo step without changing any register.
    /// The frontend must check view and selection identity before delivery;
    /// transaction validation rejects changed documents and revisions.
    pub fn apply_paste(&mut self, transaction: Transaction) -> Result<(), Error> {
        self.apply(transaction, false)?;
        self.mode = Mode::Normal;
        self.selections = self.normalized(self.selections.clone(), self.mode)?;
        self.preferred_columns = None;
        Ok(())
    }
}

fn check(cancelled: &impl Fn() -> bool) -> Result<(), Error> {
    if cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

fn overflow() -> Error {
    vex_core::Error::LengthOverflow.into()
}

fn string_with_capacity(capacity: usize) -> Result<String, Error> {
    let mut text = String::new();
    text.try_reserve_exact(capacity).map_err(|_| overflow())?;
    Ok(text)
}

/// Normalize only line endings; share an unchanged, uncounted fragment directly.
fn prepare(
    value: &Arc<str>,
    newline: &str,
    count: usize,
    cancelled: &impl Fn() -> bool,
) -> Result<Arc<str>, Error> {
    let bytes = value.as_bytes();
    let convert = match newline {
        "\n" => value.contains('\r'),
        "\r" => value.contains('\n'),
        "\r\n" => bytes.iter().enumerate().any(|(i, &byte)| match byte {
            b'\r' => bytes.get(i + 1) != Some(&b'\n'),
            b'\n' => i == 0 || bytes[i - 1] != b'\r',
            _ => false,
        }),
        _ => unreachable!("supported document line ending"),
    };
    let text = if convert {
        let mut normalized = string_with_capacity(
            value
                .len()
                .checked_mul(newline.len())
                .ok_or_else(overflow)?,
        )?;
        let mut chars = value.chars().enumerate().peekable();
        while let Some((index, ch)) = chars.next() {
            if index % 1024 == 0 {
                check(cancelled)?;
            }
            match ch {
                '\r' => {
                    if chars.peek().is_some_and(|(_, ch)| *ch == '\n') {
                        chars.next();
                    }
                    normalized.push_str(newline);
                }
                '\n' => normalized.push_str(newline),
                _ => normalized.push(ch),
            }
        }
        Cow::Owned(normalized)
    } else {
        Cow::Borrowed(value.as_ref())
    };
    if count == 1 || text.is_empty() {
        return Ok(match text {
            Cow::Borrowed(_) => value.clone(),
            Cow::Owned(text) => Arc::from(text),
        });
    }
    let mut repeated = string_with_capacity(text.len().checked_mul(count).ok_or_else(overflow)?)?;
    for index in 0..count {
        if index % 1024 == 0 {
            check(cancelled)?;
        }
        repeated.push_str(&text);
    }
    Ok(repeated.into())
}

// Several selections on one line can target the same insertion boundary.
// Coalesce those edits, but retain a separate resulting range for each fragment.
struct Group {
    range: Range<CharOffset>,
    parts: Vec<Arc<str>>,
    chars: usize,
}

impl Group {
    fn text(&self) -> Result<Arc<str>, Error> {
        if self.parts.len() == 1 {
            return Ok(self.parts[0].clone());
        }
        let capacity = self.parts.iter().try_fold(0usize, |len, part| {
            len.checked_add(part.len()).ok_or_else(overflow)
        })?;
        let mut joined = string_with_capacity(capacity)?;
        for part in &self.parts {
            joined.push_str(part);
        }
        Ok(joined.into())
    }
}

pub(crate) fn paste(ctx: &mut CommandContext<'_>, action: Paste) -> Result<(), Error> {
    let editor = &mut *ctx.editor;
    let values = editor.yank_register.read();
    let transaction = editor
        .paste_plan()
        .prepare(&values, action, ctx.count, &|| false)?;
    editor.apply_paste(transaction)
}

impl PastePlan {
    /// Prepare edits without touching a live editor. Values pair with selections
    /// in document order; extra destinations repeat the last value.
    pub fn prepare(
        &self,
        values: &[Arc<str>],
        action: Paste,
        count: std::num::NonZeroUsize,
        cancelled: &impl Fn() -> bool,
    ) -> Result<Transaction, Error> {
        check(cancelled)?;
        if values.is_empty() {
            return Err(Error::EmptyYankRegister);
        }
        let linewise =
            action != Paste::Replace && values.iter().any(|value| value.ends_with(['\r', '\n']));
        let values = values
            .iter()
            .take(self.selections.ranges().len())
            .map(|value| {
                check(cancelled)?;
                let text = prepare(value, self.newline, count.get(), cancelled)?;
                let chars = text.chars().count();
                Ok((text, chars))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let text = self.snapshot.text();
        let mut groups: Vec<Group> = Vec::new();
        let mut spans = Vec::with_capacity(self.selections.ranges().len());
        for (index, &selection) in self.selections.ranges().iter().enumerate() {
            check(cancelled)?;
            let (value, chars) = &values[index.min(values.len() - 1)];
            let range = match action {
                Paste::Replace => selection.range(),
                Paste::Before | Paste::After => {
                    let at = if linewise {
                        if action == Paste::Before {
                            motion::line_start(text, selection.start())?
                        } else {
                            let edge = if selection.is_empty() {
                                selection.end()
                            } else {
                                grapheme::previous(text, selection.end(), 1)?
                            };
                            CharOffset(text.line_to_char(
                                (text.char_to_line(edge.0) + 1).min(text.len_lines()),
                            ))
                        }
                    } else if action == Paste::Before {
                        selection.start()
                    } else {
                        selection.end()
                    };
                    at..at
                }
            };
            if !groups
                .last()
                .is_some_and(|group| range.is_empty() && group.range == range)
            {
                let mut group = Group {
                    range,
                    parts: Vec::new(),
                    chars: 0,
                };
                // A linewise append to an unterminated final line needs a separator.
                if linewise
                    && action == Paste::After
                    && group.range.start.0 == text.len_chars()
                    && text.len_chars() > 0
                    && !matches!(text.char(text.len_chars() - 1), '\r' | '\n')
                {
                    group.parts.push(Arc::from(self.newline));
                    group.chars = self.newline.len();
                }
                groups.push(group);
            }
            let group_index = groups.len() - 1;
            let group = &mut groups[group_index];
            spans.push((group_index, group.chars, *chars, selection.is_backward()));
            group.chars = group.chars.checked_add(*chars).ok_or_else(overflow)?;
            group.parts.push(value.clone());
        }
        let edits = groups
            .iter()
            .map(|group| {
                check(cancelled)?;
                Ok(Edit::new(group.range.clone(), group.text()?))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let transaction = self.snapshot.transaction(edits)?;
        let selections = spans
            .into_iter()
            .map(|(group, offset, len, backward)| {
                check(cancelled)?;
                let at = transaction
                    .map_position(groups[group].range.start, Affinity::Before)?
                    .0;
                let start = CharOffset(at.checked_add(offset).ok_or_else(overflow)?);
                let end = CharOffset(start.0.checked_add(len).ok_or_else(overflow)?);
                Ok(if backward {
                    Selection::new(end, start)
                } else {
                    Selection::new(start, end)
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let selections = SelectionSet::new(selections, self.selections.primary_index())?;
        let transaction = transaction.with_selections(selections)?;
        check(cancelled)?;
        Ok(transaction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use vex_core::Document;

    #[test]
    fn prepared_pastes_reject_stale_revisions_and_other_documents() {
        let mut source = Editor::new(Document::from("abc"));
        let transaction = source
            .paste_plan()
            .prepare(
                &[Arc::from("X")],
                Paste::After,
                std::num::NonZeroUsize::MIN,
                &|| false,
            )
            .unwrap();
        let mut other = Editor::new(Document::from("abc"));
        assert!(other.apply_paste(transaction.clone()).is_err());
        source.execute("insert_mode", 1).unwrap();
        source.insert_text("z").unwrap();
        source.execute("normal_mode", 1).unwrap();
        assert!(source.apply_paste(transaction).is_err());
        assert_eq!(source.document().text(), "zabc");
        assert_eq!(other.document().text(), "abc");
    }

    #[test]
    fn counted_paste_preparation_observes_cancellation_before_finishing() {
        let source = Editor::new(Document::from("abc"));
        let calls = std::cell::Cell::new(0);
        let result = source.paste_plan().prepare(
            &[Arc::from("x")],
            Paste::After,
            std::num::NonZeroUsize::new(1_000_000).unwrap(),
            &|| {
                calls.set(calls.get() + 1);
                calls.get() > 6
            },
        );
        assert!(matches!(result, Err(Error::Cancelled)));
        assert_eq!(source.document().text(), "abc");
    }

    fn range(anchor: usize, head: usize) -> Selection {
        Selection::new(CharOffset(anchor), CharOffset(head))
    }

    fn select(editor: &mut Editor, ranges: Vec<Selection>, primary: usize) {
        editor
            .set_selections(SelectionSet::new(ranges, primary).unwrap())
            .unwrap();
    }

    fn yanked(text: &str) -> Editor {
        let mut editor = Editor::new(Document::from(text));
        let len = editor.document.text().len_chars();
        select(&mut editor, vec![range(0, len)], 0);
        editor.execute("yank", 1).unwrap();
        editor
    }

    fn destination(source: &Editor, text: &str) -> Editor {
        Editor::with_yank_register(Document::from(text), source.yank_register())
    }

    #[test]
    fn yank_preserves_text_selections_and_redo_then_exits_select_mode() {
        let mut editor = Editor::new(Document::from("abc"));
        editor.execute("insert_mode", 1).unwrap();
        editor.insert_text("X").unwrap();
        editor.execute("normal_mode", 1).unwrap();
        editor.execute("undo", 1).unwrap();
        select(&mut editor, vec![range(2, 0)], 0);
        editor.execute("select_mode", 1).unwrap();
        let revision = editor.document.revision();
        editor.execute("yank", 1).unwrap();
        assert_eq!(editor.document.text(), "abc");
        assert_eq!(editor.document.revision(), revision);
        assert_eq!(editor.selections.primary(), range(2, 0));
        assert_eq!(editor.mode(), Mode::Normal);
        assert_eq!(editor.document.undo_depth(), 0);
        assert_eq!(editor.document.redo_depth(), 1);
        editor.execute("redo", 1).unwrap();
        let mut other = destination(&editor, "");
        other.execute("paste_before", 1).unwrap();
        assert_eq!(other.document.text(), "ab");
    }

    #[test]
    fn character_paste_and_replace_preserve_direction_and_select_new_text() {
        let source = yanked("界e\u{301}");
        for (command, expected, selection) in [
            ("paste_after", "abc界e\u{301}d", range(6, 3)),
            ("paste_before", "a界e\u{301}bcd", range(4, 1)),
            ("replace_with_yanked", "a界e\u{301}d", range(4, 1)),
        ] {
            let mut editor = destination(&source, "abcd");
            select(&mut editor, vec![range(3, 1)], 0);
            editor.execute("select_mode", 1).unwrap();
            editor.execute(command, 1).unwrap();
            assert_eq!(editor.document.text(), expected);
            assert_eq!(editor.selections.primary(), selection);
            assert_eq!(editor.mode(), Mode::Normal);
            assert_eq!(editor.document.undo_depth(), 1);
            editor.execute("undo", 1).unwrap();
            assert_eq!(editor.document.text(), "abcd");
            assert_eq!(editor.selections.primary(), range(3, 1));
            editor.execute("redo", 1).unwrap();
            assert_eq!(editor.document.text(), expected);
            assert_eq!(editor.selections.primary(), selection);
        }
    }

    #[test]
    fn counts_repeat_text_in_one_step_without_changing_the_register() {
        let source = yanked("🦀");
        for command in ["paste_after", "paste_before", "replace_with_yanked"] {
            let mut editor = destination(&source, "ab");
            editor.execute(command, 3).unwrap();
            let expected = match command {
                "paste_after" => "a🦀🦀🦀b",
                "paste_before" => "🦀🦀🦀ab",
                _ => "🦀🦀🦀b",
            };
            assert_eq!(editor.document.text(), expected);
            assert_eq!(editor.document.undo_depth(), 1);
            editor.execute("undo", 1).unwrap();
            assert_eq!(editor.document.text(), "ab");
            assert_eq!(
                source.yank_register.read().as_ref(),
                [Arc::<str>::from("🦀")]
            );
        }
    }

    #[test]
    fn cut_and_change_capture_original_text_and_undo_does_not_rewind_the_register() {
        let mut editor = Editor::new(Document::from("one two"));
        select(&mut editor, vec![range(0, 3)], 0);
        editor.execute("delete_selection", 1).unwrap();
        assert_eq!(editor.document.text(), " two");
        editor.execute("undo", 1).unwrap();
        let mut other = destination(&editor, "");
        other.execute("paste_after", 1).unwrap();
        assert_eq!(other.document.text(), "one");

        select(&mut editor, vec![range(7, 4)], 0);
        editor.execute("change_selection", 1).unwrap();
        editor.insert_text("three").unwrap();
        editor.execute("normal_mode", 1).unwrap();
        assert_eq!(editor.document.text(), "one three");
        assert_eq!(editor.document.undo_depth(), 1);
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document.text(), "one two");
        assert_eq!(editor.selections.primary(), range(7, 4));
        let mut other = destination(&editor, "");
        other.execute("paste_before", 1).unwrap();
        assert_eq!(other.document.text(), "two");
    }

    #[test]
    fn insert_deletions_and_internal_cleanup_preserve_yanked_text() {
        let source = yanked("saved");
        let mut editor = destination(&source, "abc");
        editor.execute("append_mode", 1).unwrap();
        editor.execute("delete_backward", 1).unwrap();
        editor.execute("delete_forward", 1).unwrap();
        editor.execute("normal_mode", 1).unwrap();
        editor.execute("delete_selection_without_yank", 1).unwrap();
        editor.execute("paste_before", 1).unwrap();
        assert_eq!(editor.document.text(), "saved");
    }

    #[test]
    fn fragments_pair_in_document_order_and_repeat_the_last_for_extra_destinations() {
        let mut source = Editor::new(Document::from("A BB CCC"));
        select(&mut source, vec![range(0, 1), range(4, 2), range(5, 8)], 1);
        source.execute("yank", 1).unwrap();
        let mut editor = destination(&source, "1 2 3 4");
        select(
            &mut editor,
            vec![range(0, 1), range(3, 2), range(4, 5), range(6, 7)],
            1,
        );
        let before = editor.selections.clone();
        editor.execute("replace_with_yanked", 1).unwrap();
        assert_eq!(editor.document.text(), "A BB CCC CCC");
        assert_eq!(
            editor.selections.ranges(),
            &[range(0, 1), range(4, 2), range(5, 8), range(9, 12)]
        );
        assert_eq!(editor.selections.primary_index(), 1);
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document.text(), "1 2 3 4");
        assert_eq!(editor.selections, before);
        let mut single = destination(&source, "x");
        single.execute("replace_with_yanked", 1).unwrap();
        assert_eq!(single.document.text(), "A");
    }

    #[test]
    fn one_fragment_broadcasts_to_adjacent_selections_without_swallowing_neighbors() {
        let source = yanked("YZ");
        for (command, expected, ranges) in [
            ("paste_before", "YZaYZb", vec![range(0, 2), range(3, 5)]),
            ("paste_after", "aYZbYZ", vec![range(1, 3), range(4, 6)]),
            (
                "replace_with_yanked",
                "YZYZ",
                vec![range(0, 2), range(2, 4)],
            ),
        ] {
            let mut editor = destination(&source, "ab");
            select(&mut editor, vec![range(0, 1), range(1, 2)], 1);
            editor.execute(command, 1).unwrap();
            assert_eq!(editor.document.text(), expected);
            assert_eq!(editor.selections.ranges(), ranges);
            assert_eq!(editor.selections.primary_index(), 1);
        }
    }

    #[test]
    fn linewise_paste_uses_outer_selected_lines_and_replace_uses_exact_ranges() {
        let source = yanked("one\n");
        for (command, expected) in [
            ("paste_after", "aa\nbb\none\ncc"),
            ("paste_before", "one\naa\nbb\ncc"),
            ("replace_with_yanked", "one\ncc"),
        ] {
            let mut editor = destination(&source, "aa\nbb\ncc");
            select(&mut editor, vec![range(6, 0)], 0);
            editor.execute(command, 1).unwrap();
            assert_eq!(editor.document.text(), expected);
        }
        let mut editor = destination(&source, "abc");
        select(&mut editor, vec![range(1, 2)], 0);
        editor.execute("replace_with_yanked", 1).unwrap();
        assert_eq!(editor.document.text(), "aone\nc");
    }

    #[test]
    fn linewise_paste_handles_empty_buffers_eof_and_unterminated_final_lines() {
        let source = yanked("row\n");
        for (text, cursor, command, expected, selection) in [
            ("", 0, "paste_after", "row\n", range(0, 4)),
            ("", 0, "paste_before", "row\n", range(0, 4)),
            ("end", 1, "paste_after", "end\nrow\n", range(4, 8)),
            ("end", 3, "paste_after", "end\nrow\n", range(4, 8)),
            ("end", 3, "paste_before", "row\nend", range(0, 4)),
            ("end\n", 4, "paste_after", "end\nrow\n", range(4, 8)),
        ] {
            let mut editor = destination(&source, text);
            select(&mut editor, vec![range(cursor, cursor)], 0);
            editor.execute(command, 1).unwrap();
            assert_eq!(editor.document.text(), expected);
            assert_eq!(editor.selections.primary(), selection);
        }
    }

    #[test]
    fn pasted_line_endings_follow_the_destination_but_the_register_retains_original_bytes() {
        for from in ["\n", "\r\n", "\r"] {
            let source = yanked(&format!("界{from}"));
            for to in ["\n", "\r\n", "\r"] {
                let mut editor = destination(&source, &format!("a{to}b{to}"));
                editor.execute("paste_after", 2).unwrap();
                assert_eq!(
                    editor.document.text().to_string(),
                    format!("a{to}界{to}界{to}b{to}")
                );
                assert_eq!(
                    editor.selections.primary(),
                    range(1 + to.len(), 3 + 3 * to.len())
                );
            }
            assert_eq!(source.yank_register.read()[0].as_ref(), format!("界{from}"));
        }
        let source = yanked("a\r\nb\rc\nd");
        let mut editor = destination(&source, "xy");
        editor.execute("paste_after", 1).unwrap();
        assert_eq!(editor.document.text(), "xa\nb\nc\ndy");
    }

    #[test]
    fn linewise_collisions_keep_each_fragment_direction_and_primary() {
        let mut source = Editor::new(Document::from("A\nB\n"));
        select(&mut source, vec![range(0, 2), range(2, 4)], 0);
        source.execute("yank", 1).unwrap();
        for (text, command, expected, ranges) in [
            (
                "xy\nz\n",
                "paste_after",
                "xy\nA\nB\nz\n",
                vec![range(3, 5), range(7, 5)],
            ),
            (
                "xy\nz\n",
                "paste_before",
                "A\nB\nxy\nz\n",
                vec![range(0, 2), range(4, 2)],
            ),
            (
                "xy",
                "paste_after",
                "xy\nA\nB\n",
                vec![range(3, 5), range(7, 5)],
            ),
        ] {
            let mut editor = destination(&source, text);
            select(&mut editor, vec![range(0, 1), range(2, 1)], 1);
            editor.execute(command, 1).unwrap();
            assert_eq!(editor.document.text(), expected);
            assert_eq!(editor.selections.ranges(), ranges);
            assert_eq!(editor.selections.primary_index(), 1);
            editor.execute("undo", 1).unwrap();
            assert_eq!(editor.document.text(), text);
        }
    }

    #[test]
    fn empty_and_unrepresentable_pastes_fail_without_mutating_editor_state() {
        let mut empty = Editor::new(Document::from("abc"));
        assert_eq!(
            empty.execute("paste_after", 1),
            Err(Error::EmptyYankRegister)
        );
        assert_eq!(empty.document.undo_depth(), 0);
        let source = yanked("xy");
        let mut editor = destination(&source, "abc");
        editor.execute("select_mode", 1).unwrap();
        let revision = editor.document.revision();
        let selections = editor.selections.clone();
        assert_eq!(editor.execute("paste_after", usize::MAX), Err(overflow()));
        assert_eq!(editor.document.text(), "abc");
        assert_eq!(editor.document.revision(), revision);
        assert_eq!(editor.selections, selections);
        assert_eq!(editor.mode(), Mode::Select);
        assert_eq!(editor.document.undo_depth(), 0);
        assert_eq!(source.yank_register.read()[0].as_ref(), "xy");
    }

    #[test]
    fn empty_fragments_are_distinct_from_an_unused_register() {
        let source = yanked("");
        let mut editor = destination(&source, "ab");
        editor.execute("paste_after", usize::MAX).unwrap();
        assert_eq!(editor.document.text(), "ab");
        assert_eq!(editor.document.undo_depth(), 0);
        select(&mut editor, vec![range(0, 1), range(1, 2)], 1);
        editor.execute("replace_with_yanked", 1).unwrap();
        assert_eq!(editor.document.text(), "");
        assert_eq!(editor.selections.primary(), range(0, 0));
    }

    proptest! {
        #[test]
        fn adjacent_pastes_preserve_graphemes_and_round_trip_undo(
            saved in "[ab界🦀\r\n\u{301}]{1,12}",
            count in 1usize..4,
            command in prop::sample::select(vec!["paste_after", "paste_before", "replace_with_yanked"]),
            primary in 0usize..4,
            backward in any::<bool>(),
        ) {
            let source = yanked(&saved);
            let mut editor = destination(&source, "abcd");
            let ranges = (0..4).map(|i| if backward { range(i + 1, i) } else { range(i, i + 1) }).collect();
            select(&mut editor, ranges, primary);
            let before = editor.selections.clone();
            editor.execute(command, count).unwrap();
            let result = editor.document.text().to_string();
            let after = editor.selections.clone();
            for selection in after.ranges() {
                prop_assert_eq!(grapheme::floor(editor.document.text(), selection.start()).unwrap(), selection.start());
                prop_assert_eq!(grapheme::ceil(editor.document.text(), selection.end()).unwrap(), selection.end());
            }
            editor.execute("undo", 1).unwrap();
            prop_assert_eq!(editor.document.text().to_string(), "abcd");
            prop_assert_eq!(&editor.selections, &before);
            editor.execute("redo", 1).unwrap();
            prop_assert_eq!(editor.document.text().to_string(), result);
            prop_assert_eq!(editor.selections, after);
        }
    }
}
