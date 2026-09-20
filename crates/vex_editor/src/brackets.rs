//! Typed bracket pairs and bounded, cached matching for cursor decoration.

use std::{cell::Cell, collections::VecDeque};
use vex_core::{Affinity, CharOffset, Edit, Revision, Rope, Selection, SelectionSet, grapheme};

use crate::{Editor, Error, Language, Mode, ViewId};

const PAIRS: &[(char, char, &str)] = &[('(', ')', "()"), ('[', ']', "[]"), ('{', '}', "{}")];
const MATCH_WORK: usize = 4096;
const CACHED_VIEWS: usize = 16;

fn escaped(text: &Rope, at: CharOffset) -> bool {
    let mut chars = text.chars_at(at.0);
    for count in 0..64 {
        if chars.prev() != Some('\\') {
            return count % 2 != 0;
        }
    }
    // A pathological escape run must not stall typing. Prefer literal insertion.
    true
}

pub(super) fn insert(editor: &mut Editor, character: char) -> Result<(), Error> {
    let mut encoded = [0; 4];
    let literal: &str = character.encode_utf8(&mut encoded);
    if !PAIRS
        .iter()
        .any(|&(open, close, _)| character == open || character == close)
    {
        return editor.insert_text(literal);
    }
    let text = editor.document.text();
    let mut edits = Vec::with_capacity(editor.selections.ranges().len());
    let mut destinations = Vec::with_capacity(editor.selections.ranges().len());
    for selection in editor.selections.ranges() {
        let at = selection.head;
        let next = text.get_char(at.0);
        let escaped = escaped(text, at);
        if !escaped && matches!(character, ')' | ']' | '}') && next == Some(character) {
            destinations.push((grapheme::next(text, at, 1)?, 0));
            continue;
        }
        let pair = PAIRS.iter().find(|&&(open, _, _)| open == character);
        let boundary = next
            .is_none_or(|ch| ch.is_whitespace() || matches!(ch, ')' | ']' | '}' | ',' | ';' | ':'));
        let inserted = if !escaped && boundary {
            pair.map_or(literal, |&(_, _, pair)| pair)
        } else {
            literal
        };
        edits.push(Edit::insert(at, inserted));
        destinations.push((at, 1));
    }
    let transaction = editor.document.transaction(edits)?;
    let selections = SelectionSet::new(
        destinations
            .into_iter()
            .map(|(at, offset)| {
                Ok(Selection::cursor(CharOffset(
                    transaction.map_position(at, Affinity::Before)?.0 + offset,
                )))
            })
            .collect::<Result<Vec<_>, vex_core::Error>>()?,
        editor.selections.primary_index(),
    )?;
    if transaction.is_empty() {
        // Stepping over a closer belongs to typing, so keep its undo group open.
        editor.selections = selections;
    } else {
        editor.apply(transaction.with_selections(selections)?, true)?;
    }
    editor.selections = editor.normalized(editor.selections.clone(), Mode::Insert)?;
    editor.preferred_columns = None;
    Ok(())
}

pub(super) fn empty_pair_end(
    text: &Rope,
    head: CharOffset,
) -> Result<Option<CharOffset>, vex_core::Error> {
    let Some(before) = head.0.checked_sub(1) else {
        return Ok(None);
    };
    let open = text.get_char(before);
    let close = text.get_char(head.0);
    if PAIRS
        .iter()
        .any(|&(left, right, _)| open == Some(left) && close == Some(right))
        && !escaped(text, CharOffset(before))
    {
        Ok(Some(grapheme::next(text, head, 1)?))
    } else {
        Ok(None)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    view: ViewId,
    revision: Revision,
    position: CharOffset,
    language: Option<Language>,
    parsed: bool,
}

#[derive(Debug, Default)]
pub(super) struct Cache(VecDeque<(Stamp, Option<CharOffset>)>);

impl Editor {
    /// Find the partner of (), [], or {} under a displayed cursor. Reuses ready
    /// syntax without parsing; otherwise scans a bounded amount of plain text.
    /// Matches and misses are cached per view, revision, position, and syntax
    /// availability. Missing or distant plaintext partners have no decoration.
    pub fn matching_bracket(&self, position: CharOffset) -> Option<CharOffset> {
        let character = self.document.text().get_char(position.0)?;
        if !PAIRS
            .iter()
            .any(|&(open, close, _)| character == open || character == close)
        {
            return None;
        }
        let parsed = self.parsed_syntax().filter(|parsed| parsed.available());
        let stamp = Stamp {
            view: self.active_view(),
            revision: self.document.revision(),
            position,
            language: self.language(),
            parsed: parsed.is_some(),
        };
        if let Some((_, result)) = self
            .bracket_matches
            .borrow()
            .0
            .iter()
            .find(|(key, _)| *key == stamp)
        {
            return *result;
        }
        let work = Cell::new(0);
        let cancelled = || {
            work.set(work.get() + 1);
            work.get() > MATCH_WORK
        };
        let result = match parsed {
            Some(parsed) => parsed.matching(position, false, &cancelled),
            None => vex_core::pairs::matching(self.document.text().slice(..), position, &cancelled),
        };
        let mut cache = self.bracket_matches.borrow_mut();
        cache.0.retain(|(key, _)| key.view != stamp.view);
        if cache.0.len() == CACHED_VIEWS {
            cache.0.pop_front();
        }
        cache.0.push_back((stamp, result));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Key, KeyHandler, SyntaxWorker};
    use vex_core::{ByteOffset, Document};

    fn inserting(text: &str, positions: &[usize], primary: usize) -> Editor {
        let mut editor = Editor::new(Document::from(text));
        editor.execute("insert_mode", 1).unwrap();
        editor
            .set_selections(
                SelectionSet::new(
                    positions
                        .iter()
                        .map(|&at| Selection::cursor(CharOffset(at)))
                        .collect(),
                    primary,
                )
                .unwrap(),
            )
            .unwrap();
        editor
    }

    fn type_text(editor: &mut Editor, text: &str) {
        let mut keys = KeyHandler::default();
        for ch in text.chars() {
            keys.handle(editor, Key::Char(ch)).unwrap();
        }
    }

    #[test]
    fn nested_pairs_skip_closers_and_stay_in_one_undo_group() {
        let mut editor = inserting("", &[0], 0);
        type_text(&mut editor, "foo([{");
        assert_eq!(editor.document.text(), "foo([{}])");
        assert_eq!(editor.selections.primary().head, CharOffset(6));
        let revision = editor.document.revision();
        type_text(&mut editor, "}])");
        assert_eq!(editor.document.revision(), revision);
        assert_eq!(editor.selections.primary().head, CharOffset(9));
        type_text(&mut editor, ";");
        editor.execute("normal_mode", 1).unwrap();
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document.text(), "");
        editor.execute("redo", 1).unwrap();
        assert_eq!(editor.document.text(), "foo([{}]);");
    }

    #[test]
    fn pair_insertion_respects_neighbors_escapes_and_literal_input() {
        for (text, at, typed, expected) in [
            ("word", 0, '(', "(word"),
            (" ", 0, '[', "[] "),
            (";", 0, '{', "{};"),
            ("\\", 1, '(', "\\("),
            ("\\\\", 2, '(', "\\\\()"),
            ("\\)", 1, ')', "\\))"),
            ("", 0, '\'', "'"),
            ("", 0, '"', "\""),
            ("", 0, '<', "<"),
        ] {
            let mut editor = inserting(text, &[at], 0);
            editor.insert_character(typed).unwrap();
            assert_eq!(editor.document.text(), expected);
        }
        let mut editor = inserting("", &[0], 0);
        editor.insert_text("(").unwrap();
        editor.insert_paste("[{}").unwrap();
        assert_eq!(editor.document.text(), "([{}");
        editor.execute("normal_mode", 1).unwrap();
        assert!(matches!(
            editor.insert_character('('),
            Err(Error::WrongMode { .. })
        ));
    }

    #[test]
    fn multiple_carets_mix_pair_insertion_and_skipping_atomically() {
        let mut editor = inserting("字\nword", &[1, 2, 6], 1);
        editor.insert_character('(').unwrap();
        assert_eq!(editor.document.text(), "字()\n(word()");
        assert_eq!(editor.selections.primary_index(), 1);
        assert_eq!(
            editor
                .selections
                .ranges()
                .iter()
                .map(|s| s.head.0)
                .collect::<Vec<_>>(),
            [2, 5, 10]
        );
        editor.insert_character(')').unwrap();
        assert_eq!(editor.document.text(), "字()\n()word()");
        assert_eq!(
            editor
                .selections
                .ranges()
                .iter()
                .map(|s| s.head.0)
                .collect::<Vec<_>>(),
            [3, 6, 12]
        );
        editor.execute("normal_mode", 1).unwrap();
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document.text(), "字\nword");
    }

    #[test]
    fn empty_pair_backspace_handles_counts_overlaps_and_graphemes() {
        let mut editor = inserting("() [] {}", &[1, 4, 7], 2);
        editor.execute("delete_backward", 1).unwrap();
        assert_eq!(editor.document.text(), "  ");
        assert_eq!(editor.selections.primary_index(), 2);
        editor.execute("normal_mode", 1).unwrap();
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document.text(), "() [] {}");
        let mut editor = inserting("()", &[1, 2], 1);
        editor.execute("delete_backward", 1).unwrap();
        assert_eq!(editor.document.text(), "");
        assert_eq!(editor.selections.ranges().len(), 1);
        let mut editor = inserting("a()", &[2], 0);
        editor.execute("delete_backward", 2).unwrap();
        assert_eq!(editor.document.text(), ")");
        let mut editor = inserting("\\()", &[2], 0);
        editor.execute("delete_backward", 1).unwrap();
        assert_eq!(editor.document.text(), "\\)");
        let mut editor = inserting("字()\u{301}", &[2], 0);
        editor.insert_character(')').unwrap();
        assert_eq!(editor.selections.primary().head, CharOffset(4));
        let mut editor = inserting("字()\u{301}", &[2], 0);
        editor.execute("delete_backward", 1).unwrap();
        assert_eq!(editor.document.text(), "字");
    }

    #[test]
    fn repeat_replays_pair_logic_at_the_new_location() {
        let mut editor = inserting("\n", &[0], 0);
        type_text(&mut editor, "(x)");
        editor.execute("normal_mode", 1).unwrap();
        editor
            .set_selections(SelectionSet::single(Selection::cursor(CharOffset(4))))
            .unwrap();
        editor.execute("repeat_insert", 1).unwrap();
        assert_eq!(editor.document.text(), "(x)\n(x)");
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document.text(), "(x)\n");
    }

    #[test]
    fn matching_tracks_position_edits_undo_and_views() {
        let mut editor = inserting("([字])", &[2], 0);
        for (at, expected) in [
            (0, Some(4)),
            (4, Some(0)),
            (1, Some(3)),
            (2, None),
            (5, None),
        ] {
            assert_eq!(
                editor.matching_bracket(CharOffset(at)),
                expected.map(CharOffset)
            );
        }
        let first = editor.active_view();
        let second = editor.duplicate_view();
        editor
            .with_view(second, |editor| {
                assert_eq!(editor.matching_bracket(CharOffset(1)), Some(CharOffset(3)));
            })
            .unwrap();
        assert_eq!(editor.active_view(), first);
        assert_eq!(editor.bracket_matches.borrow().0.len(), 2);
        editor.insert_text("abc").unwrap();
        assert_eq!(editor.matching_bracket(CharOffset(0)), Some(CharOffset(7)));
        editor.execute("normal_mode", 1).unwrap();
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.matching_bracket(CharOffset(0)), Some(CharOffset(4)));
    }

    #[test]
    fn cached_misses_are_bounded_and_upgraded_when_background_syntax_arrives() {
        let text = format!("fn main() {{/*{}*/}}", "x".repeat(MATCH_WORK * 2));
        let mut editor = Editor::new(Document::from(text.as_str()));
        editor.set_language(Some(Language::Rust));
        editor.set_background_syntax(true);
        assert_eq!(editor.matching_bracket(CharOffset(10)), None);
        assert_eq!(editor.matching_bracket(CharOffset(10)), None);
        assert_eq!(editor.bracket_matches.borrow().0.len(), 1);
        assert!(editor.parsed_syntax().is_none(), "drawing must not parse");
        editor.begin_syntax_frame();
        editor.syntax_highlights(ByteOffset(0)..ByteOffset(12));
        let result = SyntaxWorker::default()
            .run(editor.take_syntax_job().unwrap())
            .unwrap();
        assert!(editor.apply_syntax_result(result));
        assert_eq!(
            editor.matching_bracket(CharOffset(10)),
            Some(CharOffset(text.len() - 1))
        );
        editor.set_language(None);
        assert_eq!(editor.matching_bracket(CharOffset(10)), None);
    }

    #[test]
    fn ready_syntax_keeps_string_brackets_out_of_code_pairs() {
        let text = "fn main() { let s = \"}\"; }";
        let mut editor = Editor::new(Document::from(text));
        editor.set_language(Some(Language::Rust));
        editor.syntax_highlights(ByteOffset(0)..ByteOffset(text.len()));
        assert_eq!(
            editor.matching_bracket(CharOffset(10)),
            Some(CharOffset(text.len() - 1))
        );
    }

    #[test]
    #[ignore = "manual release-mode bracket typing and matching benchmark"]
    fn benchmark_bracket_typing_and_matching() {
        use std::{hint::black_box, time::Instant};
        let source = "x".repeat(1 << 20);
        for operation in [
            "cached match",
            "bounded cold miss",
            "type pair and Backspace",
        ] {
            let text = match operation {
                "cached match" => format!("(){source}"),
                "bounded cold miss" => format!("({source})"),
                _ => format!("\n{source}"),
            };
            let mut editor = inserting(&text, &[0], 0);
            editor.matching_bracket(CharOffset(0));
            let mut times = Vec::with_capacity(1_000);
            for _ in 0..1_000 {
                if operation == "bounded cold miss" {
                    editor.bracket_matches.borrow_mut().0.clear();
                }
                let start = Instant::now();
                if operation == "type pair and Backspace" {
                    editor.insert_character(black_box('(')).unwrap();
                    editor.execute("delete_backward", 1).unwrap();
                } else {
                    black_box(editor.matching_bracket(black_box(CharOffset(0))));
                }
                times.push(start.elapsed());
            }
            times.sort_unstable();
            eprintln!("{operation}: median {:?}, p95 {:?}", times[500], times[950]);
        }
    }
}
