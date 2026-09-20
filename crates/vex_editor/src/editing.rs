//! Atomic everyday edits over selections and logical lines.

use std::{collections::BTreeSet, ops::Range, sync::Arc};
use vex_core::{
    Affinity, CharOffset, Edit, Rope, Selection, SelectionSet, Transaction, grapheme, motion,
};

use crate::{CommandContext, Editor, Error, IndentStyle, Mode};

fn repeated(unit: &str, count: usize) -> Result<String, Error> {
    let len = unit
        .len()
        .checked_mul(count)
        .ok_or(vex_core::Error::LengthOverflow)?;
    let mut value = String::new();
    value
        .try_reserve_exact(len)
        .map_err(|_| vex_core::Error::LengthOverflow)?;
    for _ in 0..count {
        value.push_str(unit);
    }
    Ok(value)
}

fn line_range(text: &Rope, selection: Selection) -> Range<usize> {
    let last = selection.end().0 - usize::from(!selection.is_empty());
    text.char_to_line(selection.start().0)..text.char_to_line(last) + 1
}

// Selections are sorted, but their line ranges can overlap. Merge intervals
// before walking lines so multi-cursor edits never touch a line twice.
pub(super) fn line_ranges(editor: &Editor, join: bool) -> Vec<Range<usize>> {
    let text = editor.document.text();
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for &selection in editor.selections.ranges() {
        let mut range = line_range(text, selection);
        if join {
            if range.end == range.start + 1 {
                range.end = (range.end + 1).min(text.len_lines());
            }
            range.end -= 1; // Each line identifies its following line break.
        }
        if range.is_empty() {
            continue;
        }
        if let Some(previous) = ranges.last_mut()
            && range.start <= previous.end
        {
            previous.end = previous.end.max(range.end);
        } else {
            ranges.push(range);
        }
    }
    ranges
}

fn finish(editor: &mut Editor, transaction: Transaction, mode: Mode) -> Result<(), Error> {
    editor.apply(transaction, false)?;
    editor.mode = mode;
    editor.selections = editor.normalized(editor.selections.clone(), mode)?;
    editor.preferred_columns = None;
    Ok(())
}

/// Map original content without absorbing insertions exactly at its end.
fn map_content(
    transaction: &Transaction,
    selections: &SelectionSet,
    caret: Affinity,
) -> Result<SelectionSet, Error> {
    let ranges = selections
        .ranges()
        .iter()
        .map(|selection| {
            if selection.is_empty() {
                return Ok(Selection::cursor(
                    transaction.map_position(selection.head, caret)?,
                ));
            }
            let start = transaction.map_position(selection.start(), Affinity::After)?;
            let end = transaction.map_position(selection.end(), Affinity::Before)?;
            Ok(if selection.is_backward() {
                Selection::new(end, start)
            } else {
                Selection::new(start, end)
            })
        })
        .collect::<Result<Vec<_>, vex_core::Error>>()?;
    Ok(SelectionSet::new(ranges, selections.primary_index())?)
}

pub(crate) fn insert_at_line_edge(editor: &mut Editor, end: bool) -> Result<(), Error> {
    let text = editor.document.text();
    let ranges = editor
        .selections
        .ranges()
        .iter()
        .map(|&selection| {
            let cursor = if editor.mode == Mode::Insert {
                selection.head
            } else {
                motion::cursor(text, selection)?
            };
            let position = if end {
                motion::line_end(text, cursor)?
            } else {
                motion::first_nonwhitespace(text, cursor)?
                    .unwrap_or(motion::line_start(text, cursor)?)
            };
            Ok(Selection::cursor(position))
        })
        .collect::<Result<Vec<_>, vex_core::Error>>()?;
    let selections = SelectionSet::new(ranges, editor.selections.primary_index())?;
    editor.finish_undo_group();
    editor.selections = selections;
    editor.mode = Mode::Insert;
    editor.preferred_columns = None;
    Ok(())
}

pub(crate) fn replace(ctx: &mut CommandContext<'_>) -> Result<(), Error> {
    let character = ctx.character.ok_or(Error::MissingCharacter)?;
    let editor = &mut *ctx.editor;
    let mut bytes = [0; 4];
    let unit = if character == '\n' {
        editor.newline()
    } else {
        character.encode_utf8(&mut bytes)
    };
    let edits = editor
        .selections
        .ranges()
        .iter()
        .filter(|s| !s.is_empty())
        .map(|selection| {
            let count = grapheme::count(editor.document.text(), selection.range())?;
            Ok(Edit::new(selection.range(), repeated(unit, count)?))
        })
        .collect::<Result<Vec<_>, Error>>()?;
    let transaction = editor.document.transaction(edits)?;
    let ranges = editor
        .selections
        .ranges()
        .iter()
        .map(|selection| {
            let start = transaction.map_position(selection.start(), Affinity::Before)?;
            // The replacement for this range ends before any adjacent replacement.
            let end = transaction.map_position(selection.end(), Affinity::Before)?;
            Ok(if selection.is_backward() {
                Selection::new(end, start)
            } else {
                Selection::new(start, end)
            })
        })
        .collect::<Result<Vec<_>, vex_core::Error>>()?;
    let transaction = transaction.with_selections(SelectionSet::new(
        ranges,
        editor.selections.primary_index(),
    )?)?;
    finish(editor, transaction, Mode::Normal)
}

pub(crate) fn indent(ctx: &mut CommandContext<'_>, remove: bool) -> Result<(), Error> {
    let editor = &mut *ctx.editor;
    let settings = editor.indentation();
    let step = match settings.style {
        IndentStyle::Spaces(width) => width.get(),
        IndentStyle::Tabs => settings.tab_width.get(),
    };
    let width = step
        .checked_mul(ctx.count.get())
        .ok_or(vex_core::Error::LengthOverflow)?;
    let text = editor.document.text();
    let mut edits = Vec::new();
    for line in line_ranges(editor, false).into_iter().flatten() {
        let start = CharOffset(text.line_to_char(line));
        let mut columns = 0usize;
        let mut chars = 0;
        for ch in text.line(line).chars() {
            columns = match ch {
                ' ' => columns.saturating_add(1),
                '\t' => columns
                    .saturating_add(settings.tab_width.get() - columns % settings.tab_width.get()),
                _ => break,
            };
            chars += 1;
            if remove && columns >= width {
                break;
            }
        }
        if remove {
            if chars > 0 {
                let end = grapheme::floor(text, CharOffset(start.0 + chars))?;
                if end > start {
                    edits.push(Edit::delete(start..end));
                }
            }
        } else if motion::first_nonwhitespace(text, start)?.is_some() {
            let inserted = match settings.style {
                IndentStyle::Spaces(_) => repeated(" ", width - columns % step)?,
                IndentStyle::Tabs => repeated("\t", ctx.count.get())?,
            };
            edits.push(Edit::insert(start, inserted));
        }
    }
    let transaction = editor.document.transaction(edits)?;
    let selections = map_content(&transaction, &editor.selections, Affinity::After)?;
    finish(
        editor,
        transaction.with_selections(selections)?,
        Mode::Normal,
    )
}

pub(crate) fn join(editor: &mut Editor) -> Result<(), Error> {
    let text = editor.document.text();
    let mut edits = Vec::new();
    for line in line_ranges(editor, true).into_iter().flatten() {
        let start = motion::line_end(text, CharOffset(text.line_to_char(line)))?;
        let next = text.line_to_char(line + 1);
        let indent = text
            .line(line + 1)
            .chars()
            .take_while(|ch| matches!(ch, ' ' | '\t'))
            .count();
        let end = grapheme::floor(text, CharOffset(next + indent))?;
        // Preserve existing spacing, and don't add trailing whitespace when
        // joining an empty last line (the final newline of a file).
        let space =
            end.0 < text.len_chars() && (start.0 == 0 || !text.char(start.0 - 1).is_whitespace());
        edits.push(Edit::new(start..end, if space { " " } else { "" }));
    }
    let transaction = editor.document.transaction(edits)?;
    finish(editor, transaction, editor.mode)
}

pub(crate) fn add_newlines(ctx: &mut CommandContext<'_>, below: bool) -> Result<(), Error> {
    let editor = &mut *ctx.editor;
    let text = editor.document.text();
    let mut points = BTreeSet::new();
    for &selection in editor.selections.ranges() {
        let lines = line_range(text, selection);
        // Keep an existing terminator on the original selected line. At an
        // unterminated EOF the first inserted break starts the new empty line.
        let at = if below {
            CharOffset(text.line_to_char(lines.end))
        } else {
            CharOffset(text.line_to_char(lines.start))
        };
        points.insert(at);
    }
    let inserted: Arc<str> = repeated(editor.newline(), ctx.count.get())?.into();
    let transaction = editor.document.transaction(
        points
            .into_iter()
            .map(|at| Edit::insert(at, inserted.clone())),
    )?;
    let selections = map_content(
        &transaction,
        &editor.selections,
        if below {
            Affinity::Before
        } else {
            Affinity::After
        },
    )?;
    finish(
        editor,
        transaction.with_selections(selections)?,
        editor.mode,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Indentation, Language};
    use proptest::prelude::*;
    use std::num::NonZeroUsize;
    use vex_core::Document;

    fn range(anchor: usize, head: usize) -> Selection {
        Selection::new(CharOffset(anchor), CharOffset(head))
    }

    fn select(editor: &mut Editor, ranges: &[(usize, usize)], primary: usize) {
        editor
            .set_selections(
                SelectionSet::new(ranges.iter().map(|&(a, h)| range(a, h)).collect(), primary)
                    .unwrap(),
            )
            .unwrap();
    }

    fn assert_history(editor: &mut Editor, before: &str, after: &str, selections: &SelectionSet) {
        assert_eq!(editor.document.text(), after);
        assert_eq!(editor.document.undo_depth(), 1);
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document.text(), before);
        assert_eq!(&editor.selections, selections);
        editor.execute("redo", 1).unwrap();
        assert_eq!(editor.document.text(), after);
    }

    #[test]
    fn insert_edges_use_cursor_lines_merge_carets_and_group_typing() {
        let source = "  one\r\n\t two\r\n \t\r\nlast";
        for (command, carets, after) in [
            (
                "insert_at_line_start",
                [2, 9],
                "  XYone\r\n\t XYtwo\r\n \t\r\nlast",
            ),
            (
                "insert_at_line_end",
                [5, 12],
                "  oneXY\r\n\t twoXY\r\n \t\r\nlast",
            ),
        ] {
            let mut editor = Editor::new(Document::from(source));
            select(&mut editor, &[(3, 4), (4, 5), (10, 11)], 1);
            editor.execute(command, 99).unwrap();
            assert_eq!(editor.mode(), Mode::Insert);
            assert_eq!(
                editor.selections.ranges(),
                &[range(carets[0], carets[0]), range(carets[1], carets[1])]
            );
            assert_eq!(editor.selections.primary_index(), 0);
            assert_eq!(editor.document.undo_depth(), 0);
            editor.insert_text("X").unwrap();
            editor.insert_text("Y").unwrap();
            editor.execute("normal_mode", 1).unwrap();
            assert_eq!(editor.document.text(), after);
            assert_eq!(editor.document.undo_depth(), 1);
            editor.execute("undo", 1).unwrap();
            assert_eq!(editor.document.text(), source);
        }
        let mut editor = Editor::new(Document::from(source));
        editor.execute("select_mode", 1).unwrap();
        select(&mut editor, &[(12, 3)], 0);
        editor.execute("insert_at_line_start", 1).unwrap();
        assert_eq!(editor.selections.primary(), range(2, 2));
    }

    #[test]
    fn insert_edges_handle_blank_lines_empty_buffers_and_unicode() {
        for (source, start, end) in [
            ("", 0, 0),
            (" \t\r\n", 0, 2),
            ("\u{2003}e\u{301}界\r\n", 1, 4),
        ] {
            for (command, expected) in
                [("insert_at_line_start", start), ("insert_at_line_end", end)]
            {
                let mut editor = Editor::new(Document::from(source));
                editor.execute(command, 1).unwrap();
                assert_eq!(editor.selections.primary(), range(expected, expected));
                assert_eq!(editor.document.text(), source);
            }
        }
    }

    #[test]
    fn replacement_counts_graphemes_keeps_direction_and_does_not_yank() {
        let mut donor = Editor::new(Document::from("saved"));
        donor.execute("select_all", 1).unwrap();
        donor.execute("yank", 1).unwrap();
        let source = "e\u{301}👩\u{200d}💻\r\nZ";
        let mut editor = Editor::with_yank_register(Document::from(source), donor.yank_register());
        select(&mut editor, &[(0, 2), (7, 2)], 1);
        let before = editor.selections.clone();
        editor.execute("select_mode", 1).unwrap();
        let mut ctx = CommandContext::new(&mut editor);
        ctx.character = Some('界');
        ctx.count = NonZeroUsize::new(99).unwrap();
        crate::commands::replace(&mut ctx).unwrap();
        assert_eq!(editor.mode(), Mode::Normal);
        assert_eq!(editor.selections.ranges(), &[range(0, 1), range(3, 1)]);
        assert_eq!(editor.selections.primary_index(), 1);
        assert_history(&mut editor, source, "界界界Z", &before);
        donor.execute("replace_with_yanked", 1).unwrap();
        assert_eq!(donor.document.text(), "saved");
    }

    #[test]
    fn replacement_enter_tab_eof_and_cancellation_inputs_are_atomic() {
        for newline in ["\n", "\r\n", "\r"] {
            let source = format!("e\u{301}界{newline}");
            for (character, unit) in [('\n', newline), ('\t', "\t"), ('\u{301}', "\u{301}")] {
                let mut editor = Editor::new(Document::from(source.as_str()));
                select(&mut editor, &[(0, 3)], 0);
                let before = editor.selections.clone();
                let mut ctx = CommandContext::new(&mut editor);
                ctx.character = Some(character);
                crate::commands::replace(&mut ctx).unwrap();
                assert_history(
                    &mut editor,
                    &source,
                    &format!("{unit}{unit}{newline}"),
                    &before,
                );
                for selection in editor.selections.ranges() {
                    assert!(
                        grapheme::is_boundary(editor.document.text(), selection.start()).unwrap()
                    );
                    assert!(
                        grapheme::is_boundary(editor.document.text(), selection.end()).unwrap()
                    );
                }
            }
        }
        let mut editor = Editor::new(Document::default());
        assert!(matches!(
            editor.execute("replace", 1),
            Err(Error::MissingCharacter)
        ));
        let mut ctx = CommandContext::new(&mut editor);
        ctx.character = Some('x');
        crate::commands::replace(&mut ctx).unwrap();
        assert_eq!(editor.document.text(), "");
        assert_eq!(editor.document.undo_depth(), 0);
    }

    #[test]
    fn indentation_uses_language_defaults_and_explicit_tab_settings() {
        for language in Language::ALL {
            let mut editor = Editor::new(Document::from("x"));
            editor.set_language(Some(*language));
            editor.execute("indent", 1).unwrap();
            let spaces = if *language == Language::Rust { 4 } else { 2 };
            assert_eq!(
                editor.document.text(),
                format!("{}x", " ".repeat(spaces)).as_str()
            );
            assert_eq!(editor.tab_width().get(), spaces);
            editor.execute("unindent", 1).unwrap();
            assert_eq!(editor.document.text(), "x");
        }
        let mut editor = Editor::new(Document::from("x\n"));
        editor.set_language(Some(Language::Rust));
        let tabs = Indentation {
            style: IndentStyle::Tabs,
            tab_width: NonZeroUsize::new(8).unwrap(),
        };
        editor.set_indentation(tabs);
        editor.set_language(Some(Language::Rust)); // A syntax reset retains overrides.
        assert_eq!(editor.indentation(), tabs);
        editor.execute("indent", 2).unwrap();
        assert_eq!(editor.document.text(), "\t\tx\n");
        editor.execute("unindent", 1).unwrap();
        assert_eq!(editor.document.text(), "\tx\n");
        editor.set_language(Some(Language::TypeScript));
        assert_eq!(editor.indentation(), Language::TypeScript.indentation());
        editor.set_language(None);
        assert_eq!(editor.indentation(), Indentation::default());
        assert_eq!(editor.document.text(), "\tx\n");
    }

    #[test]
    fn indentation_merges_touched_lines_excludes_end_boundary_and_skips_blanks() {
        let source = " a b\r\n  c\r\n\r\n \t\r\nd";
        let mut editor = Editor::new(Document::from(source));
        select(&mut editor, &[(1, 2), (4, 3), (6, 11)], 1);
        let before = editor.selections.clone();
        editor.execute("select_mode", 1).unwrap();
        editor.execute("indent", 2).unwrap();
        assert_eq!(editor.mode(), Mode::Normal);
        assert!(editor.selections.ranges()[1].is_backward());
        assert_eq!(editor.selections.primary_index(), 1);
        assert_history(
            &mut editor,
            source,
            "        a b\r\n        c\r\n\r\n \t\r\nd",
            &before,
        );
        editor.execute("select_all", 1).unwrap();
        editor.execute("indent", 1).unwrap();
        assert_eq!(
            editor.document.text(),
            "            a b\r\n            c\r\n\r\n \t\r\n    d"
        );
    }

    #[test]
    fn unindent_handles_mixed_whitespace_counts_and_grapheme_boundaries() {
        for (source, count, after) in [
            (" \tx\n", 1, "x\n"),
            ("  \t  x\r\n", 1, "  x\r\n"),
            ("  \t  x\r\n", 2, "x\r\n"),
            ("  x", 8, "x"),
            (" \u{301}x", 1, " \u{301}x"),
            ("", 1, ""),
        ] {
            let mut editor = Editor::new(Document::from(source));
            editor.execute("unindent", count).unwrap();
            assert_eq!(editor.document.text(), after);
            assert!(
                grapheme::is_boundary(editor.document.text(), editor.selections.primary().start())
                    .unwrap()
            );
        }
        for command in [
            "indent",
            "unindent",
            "add_newline_above",
            "add_newline_below",
        ] {
            let mut editor = Editor::new(Document::from("x\r\n"));
            let before = editor.selections.clone();
            assert!(editor.execute(command, usize::MAX).is_err());
            assert_eq!(editor.document.text(), "x\r\n");
            assert_eq!(editor.selections, before);
            assert_eq!(editor.document.undo_depth(), 0);
        }
    }

    #[test]
    fn joins_merge_overlapping_line_requests_and_preserve_history() {
        let source = "a b\r\n  c\r\n\td\r\ne";
        let mut editor = Editor::new(Document::from(source));
        select(&mut editor, &[(0, 1), (3, 2), (5, 12)], 1);
        let before = editor.selections.clone();
        editor.execute("select_mode", 1).unwrap();
        editor.execute("join_selections", 99).unwrap();
        assert_eq!(editor.mode(), Mode::Select);
        assert!(editor.selections.ranges()[1].is_backward());
        assert_eq!(editor.selections.primary_index(), 1);
        assert_history(&mut editor, source, "a b c d\r\ne", &before);
    }

    #[test]
    fn joining_handles_blank_lines_existing_spacing_and_eof() {
        for (source, after) in [
            ("a\n b", "a b"),
            ("a \n b", "a b"),
            ("a\t\n b", "a\tb"),
            ("a\rb", "a b"),
            ("a\r\nb", "a b"),
            ("\na", " a"),
            ("a\n", "a"),
            ("a\n   ", "a"),
            ("a\n\n b", "a b"),
            ("// a\n  // b", "// a // b"),
            ("a\n \u{301}b", "a  \u{301}b"),
            ("last", "last"),
            ("", ""),
        ] {
            let mut editor = Editor::new(Document::from(source));
            editor.execute("select_all", 1).unwrap();
            editor.execute("join_selections", 1).unwrap();
            assert_eq!(editor.document.text(), after, "source {source:?}");
        }
        let mut editor = Editor::new(Document::from("one\ntwo\nthree"));
        editor.execute("select_line", 1).unwrap();
        editor.execute("join_selections", 1).unwrap();
        assert_eq!(editor.document.text(), "one two\nthree");
    }

    #[test]
    fn blank_lines_preserve_original_selections_and_mode_with_one_undo_step() {
        for newline in ["\n", "\r\n", "\r"] {
            let source = format!("a b{newline}c{newline}d");
            for below in [false, true] {
                let mut editor = Editor::new(Document::from(source.as_str()));
                select(&mut editor, &[(0, 1), (3, 2)], 1);
                let before = editor.selections.clone();
                editor.execute("select_mode", 1).unwrap();
                editor
                    .execute(
                        if below {
                            "add_newline_below"
                        } else {
                            "add_newline_above"
                        },
                        2,
                    )
                    .unwrap();
                assert_eq!(editor.mode(), Mode::Select);
                assert_eq!(editor.selections.primary_index(), 1);
                for (selection, expected) in editor.selections.ranges().iter().zip(["a", "b"]) {
                    assert_eq!(
                        editor
                            .document
                            .text()
                            .slice(selection.start().0..selection.end().0),
                        expected
                    );
                }
                let after = if below {
                    format!("a b{newline}{newline}{newline}c{newline}d")
                } else {
                    format!("{newline}{newline}{source}")
                };
                assert_history(&mut editor, &source, &after, &before);
            }
        }
        for source in ["", "last", "last\n"] {
            let mut editor = Editor::new(Document::from(source));
            editor.execute("select_all", 1).unwrap();
            let before = editor.selections.clone();
            editor.execute("add_newline_below", 1).unwrap();
            if source.is_empty() {
                assert_eq!(editor.selections.primary(), range(0, 1));
            } else {
                assert_eq!(editor.selections, before);
            }
            assert_history(&mut editor, source, &format!("{source}\n"), &before);
        }
    }

    #[test]
    fn line_edits_map_other_views_through_changes_and_history() {
        let mut editor = Editor::new(Document::from("one\n  two\nlast"));
        select(&mut editor, &[(6, 7)], 0);
        let other = editor.duplicate_view();
        select(&mut editor, &[(0, 1)], 0);
        editor.execute("add_newline_above", 2).unwrap();
        editor.execute("indent", 1).unwrap();
        editor.execute("join_selections", 1).unwrap();
        let check = |editor: &Editor| {
            let selection = editor.selections.primary();
            editor
                .document
                .text()
                .slice(selection.start().0..selection.end().0)
                .to_string()
        };
        assert_eq!(editor.with_view(other, check).unwrap(), "t");
        editor.execute("undo", 3).unwrap();
        assert_eq!(editor.document.text(), "one\n  two\nlast");
        assert_eq!(editor.with_view(other, check).unwrap(), "t");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]
        #[test]
        fn everyday_edits_keep_unicode_selections_valid_and_undo_atomically(
            parts in prop::collection::vec(prop::sample::select(vec!["a", "界", "e\u{301}", "👩\u{200d}💻", " ", "\t", "\r\n", "\n", "\r", "\u{301}"]), 0..40),
            offsets in prop::collection::vec((0usize..80, 0usize..80), 1..6),
            command in 0usize..6,
            count in 1usize..5,
            character in prop::sample::select(vec!['🦀', '\u{301}', '\n', '\t']),
        ) {
            let source = parts.concat();
            let mut editor = Editor::new(Document::from(source.as_str()));
            let len = editor.document.text().len_chars() + 1;
            let ranges: Vec<_> = offsets.iter().map(|&(a, h)| (a % len, h % len)).collect();
            select(&mut editor, &ranges, ranges.len() - 1);
            let before = editor.selections.clone();
            if command == 0 {
                let mut ctx = CommandContext::new(&mut editor);
                ctx.character = Some(character);
                crate::commands::replace(&mut ctx).unwrap();
            } else {
                editor.execute(["replace", "indent", "unindent", "join_selections", "add_newline_above", "add_newline_below"][command], count).unwrap();
            }
            let text = editor.document.text();
            for selection in editor.selections.ranges() {
                prop_assert!(selection.end().0 <= text.len_chars());
                prop_assert!(grapheme::is_boundary(text, selection.start()).unwrap());
                prop_assert!(grapheme::is_boundary(text, selection.end()).unwrap());
                prop_assert!(!selection.is_empty() || selection.head.0 == text.len_chars());
            }
            if editor.document.undo_depth() > 0 {
                prop_assert_eq!(editor.document.undo_depth(), 1);
                editor.execute("undo", 1).unwrap();
                prop_assert_eq!(editor.document.text().to_string(), source);
                prop_assert_eq!(editor.selections, before);
            }
        }
    }
}
