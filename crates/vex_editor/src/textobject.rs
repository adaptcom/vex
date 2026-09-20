//! Textobject selection over shared rope snapshots. Scans check cancellation
//! between graphemes or lines and never copy the selected text.

use crate::Error;
use vex_core::{
    Rope, Selection, SelectionSet, grapheme, motion, pairs,
    textobject::{paragraph, word},
};
use vex_syntax::ParsedSyntax;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Object {
    Word,
    LongWord,
    Paragraph,
    Pair(Option<char>),
}

pub(crate) fn select(
    text: &Rope,
    origins: &SelectionSet,
    object: Object,
    around: bool,
    count: usize,
    syntax: Option<&ParsedSyntax>,
    cancelled: &impl Fn() -> bool,
) -> Result<SelectionSet, Error> {
    let mut ranges = Vec::with_capacity(origins.ranges().len());
    for &origin in origins.ranges() {
        if cancelled() {
            return Ok(origins.clone());
        }
        let position = motion::cursor(text, origin)?;
        let selected = match object {
            Object::Word | Object::LongWord => word(
                text,
                position,
                around,
                matches!(object, Object::LongWord),
                cancelled,
            )?,
            Object::Paragraph => paragraph(text, position, around, count, cancelled)?,
            Object::Pair(character) => {
                if let Some(pair) = delimiters(text, syntax, origin, character, count, cancelled)? {
                    let start = if around {
                        grapheme::floor(text, pair.open)?
                    } else {
                        grapheme::next(text, pair.open, 1)?
                    };
                    let end = if around {
                        grapheme::next(text, pair.close, 1)?
                    } else {
                        grapheme::floor(text, pair.close)?
                    };
                    if origin.is_backward() {
                        Selection::new(end, start.min(end))
                    } else {
                        Selection::new(start.min(end), end)
                    }
                } else {
                    origin
                }
            }
        };
        ranges.push(selected);
    }
    Ok(SelectionSet::new(ranges, origins.primary_index())?)
}

pub(crate) fn delimiters(
    text: &Rope,
    syntax: Option<&ParsedSyntax>,
    origin: Selection,
    character: Option<char>,
    count: usize,
    cancelled: &impl Fn() -> bool,
) -> Result<Option<pairs::Delimiters>, Error> {
    let syntax = syntax.filter(|syntax| syntax.available());
    Ok(if let Some(character) = character {
        let position = motion::cursor(text, origin)?;
        let (open, close) = pairs::pair(character);
        if open == close && text.get_char(position.0) == Some(character) {
            syntax
                .and_then(|syntax| syntax.matching(position, true, cancelled))
                .filter(|other| text.get_char(other.0) == Some(character) && *other != position)
                .map(|other| pairs::Delimiters {
                    open: position.min(other),
                    close: position.max(other),
                })
        } else {
            pairs::enclosing(text.slice(..), position, character, count, cancelled)
        }
    } else if let Some(syntax) = syntax {
        syntax.closest(origin, count, cancelled)
    } else {
        pairs::closest(text.slice(..), origin, count, cancelled)
    })
}

pub(crate) fn match_brackets(
    text: &Rope,
    syntax: Option<&ParsedSyntax>,
    origins: &SelectionSet,
    extend: bool,
    cancelled: &impl Fn() -> bool,
) -> Result<SelectionSet, Error> {
    let mut ranges = Vec::with_capacity(origins.ranges().len());
    for &origin in origins.ranges() {
        if cancelled() {
            return Ok(origins.clone());
        }
        let position = motion::cursor(text, origin)?;
        let destination = if let Some(syntax) = syntax.filter(|syntax| syntax.available()) {
            syntax.matching(position, true, cancelled)
        } else {
            pairs::matching(text.slice(..), position, cancelled)
        };
        ranges.push(if let Some(destination) = destination {
            motion::put_cursor(text, origin, destination, extend)?
        } else {
            origin
        });
    }
    Ok(SelectionSet::new(ranges, origins.primary_index())?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Editor, Key, KeyHandler, Mode, SearchCompletion};
    use vex_core::{CharOffset, Document, Selection, grapheme};

    fn selected(text: &str, position: usize, keys: &str) -> (Editor, String) {
        let mut editor = Editor::new(Document::from(text));
        editor
            .set_selections(SelectionSet::single(Selection::cursor(CharOffset(
                position,
            ))))
            .unwrap();
        let mut handler = KeyHandler::default();
        for ch in keys.chars() {
            handler.handle(&mut editor, Key::Char(ch)).unwrap();
        }
        let text = editor
            .document()
            .text()
            .slice(editor.selections().primary().start().0..editor.selections().primary().end().0)
            .to_string();
        (editor, text)
    }

    #[test]
    fn word_objects_use_cursor_categories_horizontal_spaces_and_ignore_counts() {
        for (text, at, keys, expected) in [
            ("one two!", 5, "miw", "two"),
            ("one two!", 5, "9miw", "two"),
            ("one two!", 5, "maw", " two"),
            ("one  two", 1, "maw", "one  "),
            ("one!?.two", 4, "miw", "!?."),
            ("one!?.two", 4, "miW", "one!?.two"),
            ("one  two", 3, "miw", ""),
            ("one\ntwo", 3, "maw", ""),
            ("  word\r\n", 4, "maw", "  word"),
            ("  word\u{a0}tail", 4, "maw", "word\u{a0}"),
            ("", 0, "miw", ""),
            ("日本e\u{301}語!", 1, "miw", "日本e\u{301}語"),
            ("👩\u{200d}💻! abc", 0, "miw", "👩\u{200d}💻!"),
        ] {
            let (editor, actual) = selected(text, at, keys);
            assert_eq!(actual, expected, "{text:?} {at} {keys}");
            assert_eq!(editor.document().revision().get(), 0);
            for endpoint in [
                editor.selections().primary().anchor,
                editor.selections().primary().head,
            ] {
                assert!(grapheme::is_boundary(editor.document().text(), endpoint).unwrap());
            }
        }
    }

    #[test]
    fn delimiter_objects_include_or_exclude_pairs_preserve_direction_and_keep_missing_ranges() {
        for (text, at, keys, expected) in [
            ("(a(b)c)", 3, "mi)", "b"),
            ("(a(b)c)", 3, "2ma(", "(a(b)c)"),
            ("(a(b)c)", 2, "2mi(", "b)c"),
            ("{[x] y}", 2, "mim", "x"),
            ("{[x] y}", 2, "2mam", "{[x] y}"),
            ("{[x] y}", 2, "mammam", "{[x] y}"),
            ("\"word\"", 3, "mi\"", "word"),
            ("\"word\"", 0, "mi\"", "\""),
            ("word", 1, "mi)", "o"),
            ("()", 0, "mi(", ""),
            ("«e\u{301}界»", 2, "ma»", "«e\u{301}界»"),
            ("(\r\nx\r\n)", 3, "mi(", "\r\nx\r\n"),
        ] {
            assert_eq!(selected(text, at, keys).1, expected, "{text:?} {at} {keys}");
        }
        let mut editor = Editor::new(Document::from("(one) [two]"));
        editor
            .set_selections(
                SelectionSet::new(
                    vec![
                        Selection::new(CharOffset(3), CharOffset(2)),
                        Selection::new(CharOffset(8), CharOffset(9)),
                    ],
                    1,
                )
                .unwrap(),
            )
            .unwrap();
        let mut keys = KeyHandler::default();
        for ch in "vmam".chars() {
            keys.handle(&mut editor, Key::Char(ch)).unwrap();
        }
        assert_eq!(editor.mode(), Mode::Select);
        assert_eq!(
            editor.selections().ranges(),
            &[
                Selection::new(CharOffset(5), CharOffset(0)),
                Selection::new(CharOffset(6), CharOffset(11)),
            ]
        );
        assert_eq!(editor.selections().primary_index(), 1);
    }

    #[test]
    fn matching_moves_or_extends_and_plain_text_requires_a_bracket_under_the_cursor() {
        let (editor, selected) = selected("(a[b])", 0, "99mm");
        assert_eq!(selected, ")");
        assert_eq!(
            editor.selections().primary(),
            Selection::new(CharOffset(5), CharOffset(6))
        );
        let (editor, selected) = self::selected("(a[b])", 0, "vmm");
        assert_eq!(selected, "(a[b])");
        assert_eq!(editor.mode(), Mode::Select);
        assert_eq!(self::selected("(a[b])", 0, "mmmm").1, "(");
        assert_eq!(self::selected("(abc)", 2, "mm").1, "b");
        let mut editor = Editor::new(Document::from("fn f() { f(\"a)\\\"b\"); }"));
        editor.set_language(Some(crate::Language::Rust));
        editor
            .set_selections(SelectionSet::single(Selection::cursor(CharOffset(12))))
            .unwrap();
        editor.execute("match_brackets", 1).unwrap();
        assert_eq!(editor.selections().primary().start(), CharOffset(17));
    }

    #[test]
    fn paragraphs_include_line_endings_and_counts_cross_separators() {
        for (text, at, keys, expected) in [
            ("one\ntwo\n\nthree\n\n", 1, "mip", "one\ntwo\n"),
            ("one\ntwo\n\nthree\n\n", 1, "map", "one\ntwo\n\n"),
            ("one\ntwo\n\nthree\n\n", 8, "mip", "three\n"),
            ("one\n\nthree\n\n", 1, "2mip", "one\n\nthree\n"),
            ("one\n\nthree\n\n", 1, "2map", "one\n\nthree\n\n"),
            ("one\r\n\r\nthree", 1, "mip", "one\r\n"),
            ("one\r\n\r\nthree", 5, "mip", "three"),
            ("one\n \nthree", 1, "mip", "one\n \nthree"),
            ("one\n\n\nthree", 4, "mip", "one\n"),
            ("", 0, "mip", ""),
            ("one", 1, "999map", "one"),
            ("first\n\nsecond", 9, "999mip", "first\n\nsecond"),
        ] {
            assert_eq!(selected(text, at, keys).1, expected, "{text:?} {at} {keys}");
        }
    }

    #[test]
    fn selection_transforms_merge_objects_and_keep_the_primary_and_mode() {
        let mut editor = Editor::new(Document::from("first second"));
        editor
            .set_selections(
                SelectionSet::new(
                    vec![
                        Selection::new(CharOffset(1), CharOffset(0)),
                        Selection::new(CharOffset(3), CharOffset(4)),
                        Selection::new(CharOffset(10), CharOffset(9)),
                    ],
                    2,
                )
                .unwrap(),
            )
            .unwrap();
        let mut keys = KeyHandler::default();
        for ch in "vmiw".chars() {
            keys.handle(&mut editor, Key::Char(ch)).unwrap();
        }
        assert_eq!(editor.mode(), Mode::Select);
        assert_eq!(
            editor.selections().ranges(),
            &[
                Selection::new(CharOffset(0), CharOffset(5)),
                Selection::new(CharOffset(6), CharOffset(12)),
            ]
        );
        assert_eq!(editor.selections().primary_index(), 1);
    }

    #[test]
    fn background_textobjects_are_ordered_cancellable_and_reject_changed_views() {
        let mut editor = Editor::new(Document::from("long_word\n"));
        editor.set_background_search(true);
        let mut keys = KeyHandler::default();
        for ch in "miw".chars() {
            keys.handle(&mut editor, Key::Char(ch)).unwrap();
        }
        assert!(editor.search_waiting());
        let job = editor.take_search_job().unwrap();
        editor.execute("normal_mode", 1).unwrap();
        assert!(job.run().is_none());
        for ch in "mip".chars() {
            keys.handle(&mut editor, Key::Char(ch)).unwrap();
        }
        let result = editor.take_search_job().unwrap().run().unwrap();
        editor.execute("move_right", 1).unwrap();
        assert_eq!(
            editor.apply_search_result(result).unwrap(),
            SearchCompletion::Ignored
        );
        assert_eq!(editor.selections().primary().start(), CharOffset(1));
    }

    #[test]
    fn textobject_prefixes_show_help_and_cancel_without_changing_selections() {
        for (prefix, title) in [
            ("m", "Match"),
            ("mi", "Match inside"),
            ("ma", "Match around"),
        ] {
            for cancel in [Key::Escape, Key::Ctrl('c'), Key::Enter, Key::Tab] {
                let mut editor = Editor::new(Document::from("alpha beta"));
                let mut keys = KeyHandler::default();
                let original = editor.selections().clone();
                for ch in format!("2{prefix}").chars() {
                    keys.handle(&mut editor, Key::Char(ch)).unwrap();
                }
                assert_eq!(keys.hints().unwrap().title, title);
                keys.handle(&mut editor, cancel).unwrap();
                assert!(keys.hints().is_none());
                assert_eq!(keys.count(), None);
                assert_eq!(editor.selections(), &original);
            }
        }
    }
}
