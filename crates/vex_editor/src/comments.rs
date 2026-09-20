//! Comment edits inspect selected line prefixes and selection edges only.
//! Text stays in the rope; all edits are prepared against one revision.

use crate::{Editor, Error};
use std::{collections::BTreeMap, sync::Arc};
use vex_core::{Affinity, CharOffset, Edit, Rope, Selection, SelectionSet, grapheme, motion};

type Pair = (&'static str, &'static str);
const DEFAULT_BLOCK: &[Pair] = &[("/*", "*/")];

fn starts(text: &Rope, at: usize, end: usize, token: &str) -> bool {
    let length = token.chars().count();
    at + length <= end
        && text.slice(at..at + length) == token
        && grapheme::is_boundary(text, CharOffset(at + length)).unwrap_or(false)
}

fn full_lines(editor: &Editor) -> Vec<Selection> {
    let text = editor.document.text();
    crate::editing::line_ranges(editor, false)
        .into_iter()
        .flatten()
        .map(|line| {
            Selection::new(
                CharOffset(text.line_to_char(line)),
                CharOffset(text.line_to_char(line + 1)),
            )
        })
        .collect()
}

pub(crate) fn toggle(editor: &mut Editor, block: bool) -> Result<(), Error> {
    let config = editor
        .language()
        .map(|language| language.comments())
        .unwrap_or_default();
    if block {
        if config.block.is_empty() && !config.line.is_empty() {
            return line_comments(editor, config.line);
        }
        let pairs = if config.block.is_empty() {
            DEFAULT_BLOCK
        } else {
            config.block
        };
        let ranges = editor.selections.ranges().to_vec();
        return block_comments(editor, &ranges, pairs, true);
    }
    if !config.block.is_empty() {
        let lines = full_lines(editor);
        if all_commented(&blocks(editor.document.text(), &lines, config.block)) {
            return block_comments(editor, &lines, config.block, false);
        }
        let ranges = editor.selections.ranges().to_vec();
        if all_commented(&blocks(editor.document.text(), &ranges, config.block)) {
            return block_comments(editor, &ranges, config.block, false);
        }
        if config.line.is_empty() {
            return block_comments(editor, &lines, config.block, false);
        }
    }
    line_comments(editor, config.line)
}

fn apply(editor: &mut Editor, edits: Vec<Edit>) -> Result<(), Error> {
    let transaction = editor.document.transaction(edits)?;
    editor.apply(transaction, false)?;
    editor.selections = editor.normalized(editor.selections.clone(), editor.mode)?;
    editor.preferred_columns = None;
    Ok(())
}

fn line_comments(editor: &mut Editor, tokens: &[&str]) -> Result<(), Error> {
    let text = editor.document.text();
    let cursor = motion::cursor(text, editor.selections.primary())?;
    let first = motion::first_nonwhitespace(text, cursor)?;
    let token = tokens
        .iter()
        .copied()
        .filter(|token| first.is_some_and(|at| starts(text, at.0, text.len_chars(), token)))
        .max_by_key(|token| token.len())
        .or_else(|| tokens.first().copied())
        .unwrap_or("#");
    let length = token.chars().count();
    let mut lines = Vec::new();
    let mut indent = usize::MAX;
    let mut commented = true;
    let mut margin = true;
    for line in crate::editing::line_ranges(editor, false)
        .into_iter()
        .flatten()
    {
        let start = text.line_to_char(line);
        let Some(at) = motion::first_nonwhitespace(text, CharOffset(start))? else {
            continue;
        };
        indent = indent.min(at.0 - start);
        commented &= starts(text, at.0, text.len_chars(), token);
        margin &= text.get_char(at.0 + length) == Some(' ')
            && grapheme::is_boundary(text, CharOffset(at.0 + length + 1)).unwrap_or(false);
        lines.push((start, at.0));
    }
    let prefix: Arc<str> = format!("{token} ").into();
    let edits = lines
        .into_iter()
        .map(|(start, at)| {
            if commented {
                Edit::delete(CharOffset(at)..CharOffset(at + length + usize::from(margin)))
            } else {
                Edit::insert(CharOffset(start + indent), prefix.clone())
            }
        })
        .collect();
    apply(editor, edits)
}

struct Block {
    range: Selection,
    start: usize,
    end: usize,
    pair: Option<Pair>,
}

fn blocks(text: &Rope, ranges: &[Selection], pairs: &[Pair]) -> Vec<Block> {
    ranges
        .iter()
        .filter_map(|&range| {
            let slice = text.slice(range.start().0..range.end().0);
            let leading = slice.chars().position(|ch| !ch.is_whitespace())?;
            let trailing = slice
                .chars_at(slice.len_chars())
                .reversed()
                .take_while(|ch| ch.is_whitespace())
                .count();
            let start = grapheme::floor(text, CharOffset(range.start().0 + leading))
                .ok()?
                .0;
            let end = grapheme::ceil(text, CharOffset(range.end().0 - trailing))
                .ok()?
                .0;
            let pair = pairs
                .iter()
                .copied()
                .filter(|&(open, close)| {
                    let close_len = close.chars().count();
                    end - start >= open.chars().count() + close_len
                        && starts(text, start, end, open)
                        && starts(text, end - close_len, end, close)
                        && grapheme::is_boundary(text, CharOffset(end - close_len)).unwrap_or(false)
                })
                .max_by_key(|(open, close)| (open.len(), close.len()));
            Some(Block {
                range,
                start,
                end,
                pair,
            })
        })
        .collect()
}

fn all_commented(blocks: &[Block]) -> bool {
    !blocks.is_empty() && blocks.iter().all(|block| block.pair.is_some())
}

fn block_comments(
    editor: &mut Editor,
    ranges: &[Selection],
    pairs: &[Pair],
    select_added: bool,
) -> Result<(), Error> {
    let text = editor.document.text();
    let blocks = blocks(text, ranges, pairs);
    if all_commented(&blocks) {
        let mut edits = Vec::with_capacity(blocks.len() * 2);
        for block in blocks {
            let (open, close) = block.pair.unwrap();
            let mut start = block.start + open.chars().count();
            let mut end = block.end - close.chars().count();
            if start < end
                && text.char(start) == ' '
                && grapheme::is_boundary(text, CharOffset(start + 1))?
            {
                start += 1;
            }
            if end > start
                && text.char(end - 1) == ' '
                && grapheme::is_boundary(text, CharOffset(end - 1))?
            {
                end -= 1;
            }
            edits.push(Edit::delete(CharOffset(block.start)..CharOffset(start)));
            edits.push(Edit::delete(CharOffset(end)..CharOffset(block.end)));
        }
        return apply(editor, edits);
    }
    let (open, close) = pairs[0];
    // Adjacent selections share insertion boundaries: the previous closing
    // token comes before the next opening token in a single edit at that point.
    let mut insertions: BTreeMap<usize, String> = BTreeMap::new();
    let mut spans = Vec::new();
    let mut blocks = blocks.into_iter().peekable();
    for &range in ranges {
        let before = insertions
            .get(&range.start().0)
            .map_or(0, |s| s.chars().count());
        if blocks.peek().is_some_and(|block| block.range == range) {
            let block = blocks.next().unwrap();
            if block.pair.is_none() {
                insertions
                    .entry(block.start)
                    .or_default()
                    .push_str(&format!("{open} "));
                insertions
                    .entry(block.end)
                    .or_default()
                    .push_str(&format!(" {close}"));
            }
        }
        let after = insertions
            .get(&range.end().0)
            .map_or(0, |s| s.chars().count());
        spans.push((range, before, after));
    }
    let edits = insertions
        .into_iter()
        .map(|(at, value)| Edit::insert(CharOffset(at), value));
    let mut transaction = editor.document.transaction(edits)?;
    if select_added {
        let ranges = spans
            .into_iter()
            .map(|(range, before, after)| {
                let start = CharOffset(
                    transaction.map_position(range.start(), Affinity::Before)?.0 + before,
                );
                let end =
                    CharOffset(transaction.map_position(range.end(), Affinity::Before)?.0 + after);
                Ok(if range.is_backward() {
                    Selection::new(end, start)
                } else {
                    Selection::new(start, end)
                })
            })
            .collect::<Result<Vec<_>, vex_core::Error>>()?;
        transaction = transaction.with_selections(SelectionSet::new(
            ranges,
            editor.selections.primary_index(),
        )?)?;
    }
    editor.apply(transaction, false)?;
    editor.selections = editor.normalized(editor.selections.clone(), editor.mode)?;
    editor.preferred_columns = None;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Key, KeyHandler, Language, Mode};
    use vex_core::Document;

    fn editor(source: &str, language: Option<Language>) -> Editor {
        let mut editor = Editor::new(Document::from(source));
        editor.set_language(language);
        editor.execute("select_all", 1).unwrap();
        editor
    }

    #[test]
    fn line_comments_align_at_common_indent_skip_blanks_and_undo_as_one_edit() {
        for newline in ["\n", "\r\n", "\r"] {
            let source = format!("  one{newline}    two{newline}{newline}  界");
            let mut editor = editor(&source, Some(Language::Rust));
            let before = editor.selections.clone();
            editor.execute("select_mode", 1).unwrap();
            editor.execute("toggle_comments", 1).unwrap();
            assert_eq!(
                editor.document.text(),
                format!("  // one{newline}  //   two{newline}{newline}  // 界").as_str()
            );
            assert_eq!(editor.mode(), Mode::Select);
            assert_eq!(editor.document.undo_depth(), 1);
            editor.execute("toggle_comments", 1).unwrap();
            assert_eq!(editor.document.text(), source.as_str());
            editor.execute("undo", 2).unwrap();
            assert_eq!(editor.document.text(), source.as_str());
            assert_eq!(editor.selections, before);
        }
    }

    #[test]
    fn languages_choose_line_or_block_delimiters_and_plain_text_has_fallbacks() {
        for (language, line, block) in [
            (Some(Language::Rust), "// x", "/* x */"),
            (Some(Language::Bash), "# x", "# x"),
            (Some(Language::TypeScript), "// x", "/* x */"),
            (Some(Language::Markdown), "<!-- x -->", "<!-- x -->"),
            (None, "# x", "/* x */"),
        ] {
            for (command, expected) in [("toggle_comments", line), ("toggle_block_comments", block)]
            {
                let mut editor = editor("x", language);
                editor.execute(command, 1).unwrap();
                assert_eq!(editor.document.text(), expected);
                editor.execute(command, 1).unwrap();
                assert_eq!(editor.document.text(), "x");
            }
        }
    }

    #[test]
    fn overlapping_line_requests_are_edited_once_and_next_line_end_is_excluded() {
        let mut editor = editor("a b\r\nc\r\nd", Some(Language::Rust));
        editor
            .set_selections(
                SelectionSet::new(
                    vec![
                        Selection::new(CharOffset(0), CharOffset(1)),
                        Selection::new(CharOffset(5), CharOffset(2)),
                    ],
                    1,
                )
                .unwrap(),
            )
            .unwrap();
        editor.execute("toggle_comments", 1).unwrap();
        assert_eq!(editor.document.text(), "// a b\r\nc\r\nd");
        assert_eq!(editor.selections.primary_index(), 1);
        assert!(editor.selections.primary().is_backward());
    }

    #[test]
    fn mixed_line_comments_and_documentation_prefixes_toggle_consistently() {
        for (source, expected) in [
            ("// a\nb", "// // a\n// b"),
            ("//a\n// b", "a\n b"),
            (" // a\n   // b", " a\n   b"),
            ("/// a\n/// b", "a\nb"),
            ("//! a\n//! b", "a\nb"),
        ] {
            let mut editor = editor(source, Some(Language::Rust));
            editor.execute("toggle_comments", 1).unwrap();
            assert_eq!(editor.document.text(), expected);
        }
    }

    #[test]
    fn adjacent_block_selections_keep_each_wrapper_and_primary_direction() {
        let mut editor = editor("ab", Some(Language::Rust));
        let original = SelectionSet::new(
            vec![
                Selection::new(CharOffset(0), CharOffset(1)),
                Selection::new(CharOffset(2), CharOffset(1)),
            ],
            1,
        )
        .unwrap();
        editor.set_selections(original.clone()).unwrap();
        editor.execute("toggle_block_comments", 1).unwrap();
        assert_eq!(editor.document.text(), "/* a *//* b */");
        assert_eq!(
            editor.selections.ranges(),
            &[
                Selection::new(CharOffset(0), CharOffset(7)),
                Selection::new(CharOffset(14), CharOffset(7))
            ]
        );
        assert_eq!(editor.selections.primary_index(), 1);
        editor.execute("toggle_block_comments", 1).unwrap();
        assert_eq!(editor.document.text(), "ab");
        assert_eq!(editor.selections, original);
    }

    #[test]
    fn block_comments_preserve_whitespace_skip_already_commented_ranges_and_unwrap_empty() {
        for (source, expected) in [
            (" \t界e\u{301} \r\n", " \t/* 界e\u{301} */ \r\n"),
            ("/* */", ""),
            ("/**/", ""),
            ("/** doc */", "doc"),
            ("", ""),
            (" \t\n", " \t\n"),
        ] {
            let mut editor = editor(source, Some(Language::Rust));
            editor.execute("toggle_block_comments", 1).unwrap();
            assert_eq!(editor.document.text(), expected);
            for selection in editor.selections.ranges() {
                assert!(grapheme::is_boundary(editor.document.text(), selection.start()).unwrap());
                assert!(grapheme::is_boundary(editor.document.text(), selection.end()).unwrap());
            }
        }
        let mut editor = editor("/* a */b", Some(Language::Rust));
        editor
            .set_selections(
                SelectionSet::new(
                    vec![
                        Selection::new(CharOffset(0), CharOffset(7)),
                        Selection::new(CharOffset(7), CharOffset(8)),
                    ],
                    0,
                )
                .unwrap(),
            )
            .unwrap();
        editor.execute("toggle_block_comments", 1).unwrap();
        assert_eq!(editor.document.text(), "/* a *//* b */");
        editor.execute("toggle_block_comments", 1).unwrap();
        assert_eq!(editor.document.text(), "ab");
    }

    #[test]
    fn automatic_comments_use_per_line_blocks_for_markdown_and_detect_selected_blocks() {
        let mut editor = editor("  one\r\n  two\r\n\r\n", Some(Language::Markdown));
        editor.execute("toggle_comments", 1).unwrap();
        assert_eq!(
            editor.document.text(),
            "  <!-- one -->\r\n  <!-- two -->\r\n\r\n"
        );
        editor.execute("toggle_comments", 1).unwrap();
        assert_eq!(editor.document.text(), "  one\r\n  two\r\n\r\n");
        editor.execute("toggle_block_comments", 1).unwrap();
        assert_eq!(editor.document.text(), "  <!-- one\r\n  two -->\r\n\r\n");
        editor.execute("toggle_comments", 1).unwrap();
        assert_eq!(editor.document.text(), "  one\r\n  two\r\n\r\n");
    }

    #[test]
    fn control_c_comments_but_still_cancels_pending_input() {
        let mut editor = editor("one", Some(Language::Rust));
        let mut keys = KeyHandler::default();
        keys.handle(&mut editor, Key::Ctrl('c')).unwrap();
        assert_eq!(editor.document.text(), "// one");
        keys.handle(&mut editor, Key::Char(' ')).unwrap();
        keys.handle(&mut editor, Key::Ctrl('c')).unwrap();
        assert_eq!(editor.document.text(), "// one");
        assert!(keys.pending_keys().is_empty());
        keys.handle(&mut editor, Key::Char(' ')).unwrap();
        keys.handle(&mut editor, Key::Char('c')).unwrap();
        assert_eq!(editor.document.text(), "one");
    }
}
