//! Selection-local hard wrapping, independent of terminal geometry and LSP.

use std::num::NonZeroUsize;
use unicode_segmentation::UnicodeSegmentation;
use vex_core::{Affinity, Edit, RopeSlice, Selection, SelectionSet, display};

use crate::{Editor, Error};

pub(crate) fn reflow(editor: &mut Editor, width: NonZeroUsize) -> Result<(), Error> {
    let comments = editor.language().map(|language| language.comments());
    let tokens = comments.map_or(&[][..], |comments| comments.line);
    let mut edits = Vec::new();
    for selection in editor.selections.ranges() {
        let original = editor
            .document
            .text()
            .slice(selection.start().0..selection.end().0);
        let wrapped = wrap(
            original,
            width.get(),
            editor.tab_width(),
            editor.newline(),
            tokens,
        );
        if original != wrapped.as_str() {
            edits.push(Edit::new(selection.range(), wrapped));
        }
    }
    // Reflowing already wrapped text should not dirty a buffer or add history.
    if edits.is_empty() {
        return Ok(());
    }
    let transaction = editor.document.transaction(edits)?;
    let selections = editor
        .selections
        .ranges()
        .iter()
        .map(|selection| {
            // Before at both ends keeps adjacent replacements separate. In
            // particular, an endpoint must not absorb the next selection's text.
            let start = transaction.map_position(selection.start(), Affinity::Before)?;
            let end = transaction.map_position(selection.end(), Affinity::Before)?;
            Ok(if selection.is_backward() {
                Selection::new(end, start)
            } else {
                Selection::new(start, end)
            })
        })
        .collect::<Result<Vec<_>, vex_core::Error>>()?;
    let selections = SelectionSet::new(selections, editor.selections.primary_index())?;
    editor.apply(transaction.with_selections(selections)?, false)?;
    editor.selections = editor.normalized(editor.selections.clone(), editor.mode)?;
    editor.preferred_columns = None;
    Ok(())
}

fn columns(text: &str, tab_width: NonZeroUsize) -> usize {
    text.graphemes(true).fold(0usize, |column, grapheme| {
        column.saturating_add(display::width(grapheme, column, tab_width))
    })
}

// Only ASCII spaces/tabs are break opportunities. Nonbreaking spaces and
// whitespace with an attached combining mark must stay part of their word.
fn spacing(grapheme: &str) -> bool {
    matches!(grapheme, " " | "\t")
}

fn indent_len(text: &str) -> usize {
    text.graphemes(true)
        .take_while(|g| spacing(g))
        .map(str::len)
        .sum()
}

fn prefix_len(text: &str, tokens: &[&str]) -> usize {
    let indent = indent_len(text);
    let rest = &text[indent..];
    let marker = tokens
        .iter()
        .filter(|token| rest.starts_with(**token))
        .max_by_key(|token| token.len());
    if let Some(marker) = marker {
        let end = indent + marker.len();
        // Never separate a comment marker from an attached combining mark.
        if text[end..].is_empty() || text.grapheme_indices(true).any(|(at, _)| at == end) {
            return end + indent_len(&text[end..]);
        }
    }
    indent
}

fn split_ending(line: &str) -> (&str, &str) {
    let body = line.trim_end_matches([
        '\r', '\n', '\u{b}', '\u{c}', '\u{85}', '\u{2028}', '\u{2029}',
    ]);
    (body, &line[body.len()..])
}

struct Paragraph {
    prefix: String,
    column: usize,
    has_word: bool,
    trailing: String,
    ending: String,
}

fn wrap(
    text: RopeSlice<'_>,
    width: usize,
    tab_width: NonZeroUsize,
    newline: &str,
    tokens: &[&str],
) -> String {
    let mut output = String::with_capacity(text.len_bytes());
    let mut paragraph: Option<Paragraph> = None;
    for line in text.lines() {
        let line = line.to_string();
        if line.is_empty() {
            continue;
        }
        let (body, ending) = split_ending(&line);
        let (prefix, content) = body.split_at(prefix_len(body, tokens));
        if (content.is_empty() || paragraph.as_ref().is_some_and(|p| p.prefix != prefix))
            && let Some(previous) = paragraph.take()
        {
            output.push_str(&previous.trailing);
            output.push_str(&previous.ending);
        }
        if content.is_empty() {
            output.push_str(&line);
            continue;
        }
        let paragraph = paragraph.get_or_insert_with(|| {
            output.push_str(prefix);
            Paragraph {
                prefix: prefix.into(),
                column: columns(prefix, tab_width),
                has_word: false,
                trailing: String::new(),
                ending: String::new(),
            }
        });
        let mut graphemes = content.grapheme_indices(true).peekable();
        while let Some((start, _)) = graphemes.peek().copied() {
            let mut end = start;
            while let Some(&(at, grapheme)) = graphemes.peek() {
                if spacing(grapheme) {
                    break;
                }
                end = at + grapheme.len();
                graphemes.next();
            }
            let word = &content[start..end];
            let word_width = columns(word, tab_width);
            if paragraph.has_word {
                if paragraph
                    .column
                    .saturating_add(1)
                    .saturating_add(word_width)
                    > width
                {
                    output.push_str(newline);
                    output.push_str(prefix);
                    paragraph.column = columns(prefix, tab_width);
                } else {
                    output.push(' ');
                    paragraph.column = paragraph.column.saturating_add(1);
                }
            }
            output.push_str(word);
            paragraph.column = paragraph.column.saturating_add(word_width);
            paragraph.has_word = true;
            // Retain the last line's trailing spacing, including when a
            // partial selection ends just before an unselected word.
            let trailing_start = end;
            while let Some(&(at, grapheme)) = graphemes.peek() {
                if !spacing(grapheme) {
                    break;
                }
                end = at + grapheme.len();
                graphemes.next();
            }
            paragraph.trailing.clear();
            paragraph.trailing.push_str(&content[trailing_start..end]);
        }
        paragraph.ending.clear();
        paragraph.ending.push_str(ending);
    }
    if let Some(paragraph) = paragraph {
        output.push_str(&paragraph.trailing);
        output.push_str(&paragraph.ending);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Language, Mode};
    use vex_core::{CharOffset, Document, grapheme};

    fn range(anchor: usize, head: usize) -> Selection {
        Selection::new(CharOffset(anchor), CharOffset(head))
    }

    fn editor(source: &str) -> Editor {
        let mut editor = Editor::new(Document::from(source));
        editor.execute("select_all", 1).unwrap();
        editor
    }

    #[test]
    fn reflow_joins_and_wraps_each_paragraph_preserving_blank_lines_and_endings() {
        for newline in ["\n", "\r\n", "\r"] {
            let source = format!(
                "one   two{newline}three four five{newline} \t{newline}six seven{newline}eight{newline}"
            );
            let expected = format!(
                "one two{newline}three four{newline}five{newline} \t{newline}six seven{newline}eight{newline}"
            );
            let mut editor = editor(&source);
            editor.execute("reflow", 10).unwrap();
            assert_eq!(editor.document.text(), expected.as_str());
            assert_eq!(
                editor.selections.primary(),
                range(0, expected.chars().count())
            );
            let revision = editor.document.revision();
            editor.execute("reflow", 10).unwrap();
            assert_eq!(editor.document.revision(), revision);
            assert_eq!(editor.document.undo_depth(), 1);
        }
    }

    #[test]
    fn default_width_is_eighty_and_explicit_one_is_not_a_repeat_count() {
        let source = "word ".repeat(19) + "word";
        let mut editor = editor(&source);
        editor.execute("reflow", 0).unwrap();
        assert_eq!(
            editor.document.text(),
            format!("{}\n{}", source[..79].to_owned(), &source[80..]).as_str()
        );
        editor.execute("reflow", 1).unwrap();
        assert_eq!(editor.document.text(), vec!["word"; 20].join("\n").as_str());
        editor.execute("reflow", usize::MAX).unwrap();
        assert_eq!(editor.document.text(), source.as_str());
    }

    #[test]
    fn partial_adjacent_backward_selections_keep_direction_primary_and_atomic_history() {
        let source = "[one two threefour five six]";
        let before = SelectionSet::new(vec![range(1, 14), range(27, 14)], 1).unwrap();
        let mut editor = editor(source);
        editor.set_selections(before.clone()).unwrap();
        editor.execute("select_mode", 1).unwrap();
        editor.execute("reflow", 8).unwrap();
        let expected = "[one two\nthreefour\nfive six]";
        assert_eq!(editor.document.text(), expected);
        assert_eq!(editor.mode(), Mode::Select);
        assert_eq!(editor.selections.primary_index(), 1);
        assert_eq!(editor.selections.ranges(), &[range(1, 14), range(27, 14)]);
        let after = editor.selections.clone();
        assert_eq!(editor.document.undo_depth(), 1);
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document.text(), source);
        assert_eq!(editor.selections, before);
        editor.execute("redo", 1).unwrap();
        assert_eq!(editor.document.text(), expected);
        assert_eq!(editor.selections, after);
    }

    #[test]
    fn selection_lengths_change_independently_and_leave_unselected_text_alone() {
        let source = "[one   two three] untouched [four   five six]";
        let mut editor = editor(source);
        let second = source.find("four").unwrap();
        editor
            .set_selections(
                SelectionSet::new(vec![range(1, 16), range(source.len() - 1, second)], 1).unwrap(),
            )
            .unwrap();
        editor.execute("reflow", 8).unwrap();
        let expected = "[one two\nthree] untouched [four\nfive six]";
        assert_eq!(editor.document.text(), expected);
        assert_eq!(
            editor.selections.ranges(),
            &[
                range(1, 14),
                range(expected.len() - 1, expected.find("four").unwrap())
            ]
        );
        assert_eq!(editor.selections.primary_index(), 1);
        assert_eq!(editor.mode(), Mode::Normal);
    }

    #[test]
    fn indentation_and_language_comment_prefixes_continue_at_their_display_width() {
        for (source, expected, width, language) in [
            (
                "  one two three\n  four five",
                "  one two\n  three\n  four five",
                11,
                None,
            ),
            ("\tone two three", "\tone two\n\tthree", 11, None),
            ("one two\n  three four", "one two\n  three\n  four", 8, None),
            (
                "  /// one two\n  /// three four\n  ///\n  //! five six seven\n",
                "  /// one\n  /// two\n  /// three\n  /// four\n  ///\n  //! five\n  //! six\n  //! seven\n",
                11,
                Some(Language::Rust),
            ),
            (
                "# one two three\n# four",
                "# one two\n# three\n# four",
                9,
                Some(Language::Python),
            ),
            (
                "//one two three",
                "//one two\n//three",
                9,
                Some(Language::Go),
            ),
        ] {
            let mut editor = editor(source);
            editor.set_language(language);
            editor.set_tab_width(NonZeroUsize::new(4).unwrap());
            editor.execute("reflow", width).unwrap();
            assert_eq!(editor.document.text(), expected, "{source:?}");
        }
    }

    #[test]
    fn unicode_words_use_cell_width_and_keep_combining_emoji_and_nonbreaking_spaces() {
        for (source, expected, width) in [
            ("界界 界界 界", "界界\n界界 界", 7),
            (
                "e\u{301} e\u{301} e\u{301}",
                "e\u{301} e\u{301}\ne\u{301}",
                3,
            ),
            (
                "👩\u{200d}💻 👩\u{200d}💻 👩\u{200d}💻",
                "👩\u{200d}💻 👩\u{200d}💻\n👩\u{200d}💻",
                5,
            ),
            ("a\u{a0}b c", "a\u{a0}b\nc", 3),
            ("e \u{301}x z", "e \u{301}x\nz", 2),
            (
                "https://example.com/long/path next",
                "https://example.com/long/path\nnext",
                8,
            ),
        ] {
            let mut editor = editor(source);
            editor.execute("reflow", width).unwrap();
            assert_eq!(editor.document.text(), expected, "{source:?}");
            for selection in editor.selections.ranges() {
                assert!(grapheme::is_boundary(editor.document.text(), selection.anchor).unwrap());
                assert!(grapheme::is_boundary(editor.document.text(), selection.head).unwrap());
            }
        }
    }

    #[test]
    fn empty_unchanged_and_insert_caret_inputs_do_not_edit_or_dirty_the_buffer() {
        for source in [
            "",
            " \t\r\n\r\n",
            "one two\n",
            "  one two\n",
            "e\u{301}",
            "界",
        ] {
            let mut editor = editor(source);
            let before = editor.selections.clone();
            let revision = editor.document.revision();
            editor.execute("reflow", 0).unwrap();
            assert_eq!(editor.document.text(), source);
            assert_eq!(editor.selections, before);
            assert_eq!(editor.document.revision(), revision);
            assert_eq!(editor.document.undo_depth(), 0);
        }
        let mut editor = editor("one two three");
        editor.execute("insert_mode", 1).unwrap();
        editor.execute("reflow", 1).unwrap();
        assert_eq!(editor.document.text(), "one two three");
        assert_eq!(editor.mode(), Mode::Insert);
        assert_eq!(editor.document.undo_depth(), 0);
    }

    #[test]
    fn selected_trailing_spacing_does_not_join_an_unselected_word() {
        let mut editor = editor("one two three next");
        editor
            .set_selections(SelectionSet::single(range(0, 14)))
            .unwrap();
        editor.execute("reflow", 7).unwrap();
        assert_eq!(editor.document.text(), "one two\nthree next");
        assert_eq!(editor.selections.primary(), range(0, 14));
    }
}
