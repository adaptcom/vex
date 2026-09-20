//! Atomic surround edits. Delimiters insert at selection boundaries; selected
//! document contents remain in the rope and never get copied into edit strings.

use crate::{CommandContext, Editor, Error, Mode, ViewId};
use std::{collections::BTreeSet, sync::Arc};
use vex_core::{
    CharOffset, DocumentId, Edit, Revision, Selection, SelectionSet, Snapshot, Transaction, motion,
    pairs,
};

/// The first replacement step previews delimiters but never changes the document
/// or undo history. Keep the original view state until the next input arrives.
#[derive(Debug)]
pub(crate) struct Replacement {
    document: DocumentId,
    revision: Revision,
    view: ViewId,
    mode: Mode,
    origins: SelectionSet,
    columns: Option<Vec<usize>>,
    preview: SelectionSet,
    pairs: Arc<[pairs::Delimiters]>,
}

impl Replacement {
    fn valid(&self, editor: &Editor) -> bool {
        self.document == editor.document.id()
            && self.revision == editor.document.revision()
            && self.view == editor.active_view()
            && self.mode == editor.mode
            && self.preview == editor.selections
    }
}

#[derive(Debug)]
pub(crate) struct Resolved {
    pairs: Arc<[pairs::Delimiters]>,
    preview: SelectionSet,
}

/// Resolve every selection before preparing any edit. Duplicate delimiters are
/// errors, while nested pairs with distinct endpoints are allowed. The ordered
/// set keeps collision checks O(S log S) for S selections.
pub(crate) fn resolve(
    snapshot: &Snapshot,
    syntax: Option<&vex_syntax::ParsedSyntax>,
    origins: &SelectionSet,
    character: Option<char>,
    count: usize,
    cancelled: &impl Fn() -> bool,
) -> Result<Arc<[pairs::Delimiters]>, Error> {
    let text = snapshot.text();
    let mut seen = BTreeSet::new();
    let mut pairs = Vec::with_capacity(origins.ranges().len());
    for &origin in origins.ranges() {
        if cancelled() {
            return Err(Error::SurroundNotFound);
        }
        let Some(pair) =
            crate::textobject::delimiters(text, syntax, origin, character, count, cancelled)?
        else {
            let cursor = motion::cursor(text, origin)?;
            return Err(
                if character.is_some_and(|ch| {
                    pairs::pair(ch).0 == pairs::pair(ch).1 && text.get_char(cursor.0) == Some(ch)
                }) {
                    Error::SurroundAmbiguous
                } else {
                    Error::SurroundNotFound
                },
            );
        };
        if !seen.insert(pair.open) || !seen.insert(pair.close) {
            return Err(Error::SurroundOverlap);
        }
        pairs.push(pair);
    }
    Ok(pairs.into())
}

pub(crate) fn prepare(
    snapshot: &Snapshot,
    origins: &SelectionSet,
    pairs: Arc<[pairs::Delimiters]>,
    cancelled: &impl Fn() -> bool,
) -> Result<Resolved, Error> {
    let mut preview = Vec::with_capacity(pairs.len().saturating_mul(2));
    for pair in pairs.iter() {
        if cancelled() {
            return Err(Error::SurroundNotFound);
        }
        preview.push(motion::block(snapshot.text(), pair.open)?);
        preview.push(motion::block(snapshot.text(), pair.close)?);
    }
    Ok(Resolved {
        pairs,
        preview: SelectionSet::new(preview, origins.primary_index() * 2)?,
    })
}

pub(crate) fn transaction(
    snapshot: &Snapshot,
    origins: &SelectionSet,
    pairs: &[pairs::Delimiters],
    replacement: Option<char>,
    cancelled: &impl Fn() -> bool,
) -> Result<Transaction, Error> {
    let (open, close): (Arc<str>, Arc<str>) = replacement.map_or_else(
        || (Arc::from(""), Arc::from("")),
        |ch| {
            let (open, close) = pairs::pair(ch);
            (Arc::from(open.to_string()), Arc::from(close.to_string()))
        },
    );
    let mut edits = Vec::with_capacity(pairs.len().saturating_mul(2));
    for pair in pairs {
        if cancelled() {
            return Err(Error::SurroundNotFound);
        }
        edits.push(Edit::new(
            pair.open..CharOffset(pair.open.0 + 1),
            Arc::clone(&open),
        ));
        edits.push(Edit::new(
            pair.close..CharOffset(pair.close.0 + 1),
            Arc::clone(&close),
        ));
    }
    let transaction = snapshot.transaction(edits)?;
    // Equal-length scalar replacements keep coordinates unchanged, including
    // selection endpoints at a replaced bracket (Helix's sticky mapping).
    Ok(if replacement.is_some() {
        transaction.with_selections(origins.clone())?
    } else {
        transaction
    })
}

pub(crate) fn preview(editor: &mut Editor, resolved: Resolved) {
    editor.replacement = Some(Replacement {
        document: editor.document.id(),
        revision: editor.document.revision(),
        view: editor.active_view(),
        mode: editor.mode,
        origins: std::mem::replace(&mut editor.selections, resolved.preview.clone()),
        columns: editor.preferred_columns.take(),
        preview: resolved.preview,
        pairs: resolved.pairs,
    });
}

pub(crate) fn cancel(editor: &mut Editor) {
    if let Some(replacement) = editor.replacement.take()
        && replacement.valid(editor)
    {
        editor.selections = replacement.origins;
        editor.preferred_columns = replacement.columns;
    }
}

pub(crate) fn finish(ctx: &mut CommandContext<'_>) -> Result<(), Error> {
    let character = ctx.character.ok_or(Error::MissingCharacter)?;
    let editor = &mut *ctx.editor;
    let replacement = editor
        .replacement
        .take()
        .ok_or(Error::NoSurroundReplacement)?;
    if !replacement.valid(editor) {
        return Err(Error::SurroundChanged);
    }
    editor.selections = replacement.origins;
    editor.preferred_columns = replacement.columns;
    crate::search::replace_surround(editor, replacement.pairs, character)
}

pub(crate) fn apply(editor: &mut Editor, transaction: Transaction) -> Result<(), Error> {
    editor.apply(transaction, false)?;
    editor.mode = Mode::Normal;
    editor.selections = editor.normalized(editor.selections.clone(), Mode::Normal)?;
    editor.preferred_columns = None;
    Ok(())
}

fn insert(edits: &mut Vec<(CharOffset, String)>, position: CharOffset, text: &str) {
    if let Some((previous, content)) = edits.last_mut()
        && *previous == position
    {
        content.push_str(text);
    } else {
        edits.push((position, text.into()));
    }
}

pub(crate) fn add(ctx: &mut CommandContext<'_>) -> Result<(), Error> {
    let character = ctx.character.ok_or(Error::MissingCharacter)?;
    let editor = &mut *ctx.editor;
    if editor.mode == Mode::Insert {
        return Err(Error::WrongMode {
            expected: Mode::Normal,
            actual: editor.mode,
        });
    }
    editor.finish_undo_group();
    let (open, close) = pairs::pair(character);
    let mut open_bytes = [0; 4];
    let mut close_bytes = [0; 4];
    let (open, close) = if character == '\n' {
        (editor.newline(), editor.newline())
    } else {
        (
            &*open.encode_utf8(&mut open_bytes),
            &*close.encode_utf8(&mut close_bytes),
        )
    };
    let inserted = open.chars().count() + close.chars().count();
    let mut edits = Vec::new();
    let mut selections = Vec::with_capacity(editor.selections.ranges().len());
    let mut added = 0usize;
    for selection in editor.selections.ranges() {
        // Adjacent selections and empty cursors can insert at the same offset.
        // Coalesce those inserts in traversal order: previous close, next open.
        insert(&mut edits, selection.start(), open);
        insert(&mut edits, selection.end(), close);
        let start = selection
            .start()
            .0
            .checked_add(added)
            .ok_or(vex_core::Error::LengthOverflow)?;
        added = added
            .checked_add(inserted)
            .ok_or(vex_core::Error::LengthOverflow)?;
        let end = selection
            .end()
            .0
            .checked_add(added)
            .ok_or(vex_core::Error::LengthOverflow)?;
        selections.push(if selection.is_backward() {
            Selection::new(CharOffset(end), CharOffset(start))
        } else {
            Selection::new(CharOffset(start), CharOffset(end))
        });
    }
    let transaction = editor
        .document
        .transaction(edits.into_iter().map(|(at, text)| Edit::insert(at, text)))?
        .with_selections(SelectionSet::new(
            selections,
            editor.selections.primary_index(),
        )?)?;
    editor.apply(transaction, false)?;
    editor.mode = Mode::Normal;
    editor.selections = editor.normalized(editor.selections.clone(), Mode::Normal)?;
    editor.preferred_columns = None;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Editor, Key, KeyHandler};
    use vex_core::{Document, grapheme};

    fn press(editor: &mut Editor, keys: &str) {
        let mut handler = KeyHandler::default();
        for ch in keys.chars() {
            handler.handle(editor, Key::Char(ch)).unwrap();
        }
    }

    fn input(keys: &mut KeyHandler, editor: &mut Editor, sequence: &str) -> Result<(), Error> {
        for ch in sequence.chars() {
            keys.handle(editor, Key::Char(ch))?;
        }
        Ok(())
    }

    fn range(anchor: usize, head: usize) -> Selection {
        Selection::new(CharOffset(anchor), CharOffset(head))
    }

    #[test]
    fn delete_pairs_handles_counts_quotes_nearest_empty_contents_and_preserves_yanks() {
        for (text, keys, expected) in [
            ("(a(b)c)", "3lmd)", "(abc)"),
            ("(a(b)c)", "3l2md(", "a(b)c"),
            ("{[word]}", "2lmdm", "{word}"),
            ("{[word]}", "2l2mdm", "[word]"),
            ("\"word\"", "lmd\"", "word"),
            ("a界a", "lmda", "界"),
            ("()", "md)", ""),
            ("「e\u{301}界」\r\n", "lmd」", "e\u{301}界\r\n"),
        ] {
            let mut editor = Editor::new(Document::from(text));
            let mut keys_handler = KeyHandler::default();
            input(&mut keys_handler, &mut editor, keys).unwrap();
            assert_eq!(editor.document().text(), expected, "{text:?} {keys}");
            assert_eq!(editor.document().undo_depth(), 1);
            assert_eq!(editor.mode(), Mode::Normal);
            editor.execute("undo", 1).unwrap();
            assert_eq!(editor.document().text(), text);
            editor.execute("redo", 1).unwrap();
            assert_eq!(editor.document().text(), expected);
        }
        let mut editor = Editor::new(Document::from("(x)"));
        press(&mut editor, "lyvmd)p");
        assert_eq!(editor.document().text(), "xx");
    }

    #[test]
    fn replacement_previews_delimiters_then_restores_direction_primary_and_undo() {
        let mut editor = Editor::new(Document::from("(one) [two]"));
        let before = SelectionSet::new(vec![range(4, 1), range(7, 10)], 1).unwrap();
        editor.set_selections(before.clone()).unwrap();
        let mut keys = KeyHandler::default();
        input(&mut keys, &mut editor, "vmrm").unwrap();
        assert_eq!(keys.hints().unwrap().title, "Replace with a pair of");
        assert_eq!(editor.mode(), Mode::Select);
        assert_eq!(
            editor.selections().ranges(),
            &[range(0, 1), range(4, 5), range(6, 7), range(10, 11)]
        );
        assert_eq!(editor.selections().primary_index(), 2);
        assert_eq!(editor.document().undo_depth(), 0);
        assert_eq!(editor.document().text(), "(one) [two]");
        input(&mut keys, &mut editor, "}").unwrap();
        assert_eq!(editor.document().text(), "{one} {two}");
        assert_eq!(editor.selections(), &before);
        assert_eq!(editor.mode(), Mode::Normal);
        assert_eq!(editor.document().undo_depth(), 1);
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document().text(), "(one) [two]");
        assert_eq!(editor.selections(), &before);
        editor.execute("redo", 1).unwrap();
        assert_eq!(editor.document().text(), "{one} {two}");
        assert_eq!(editor.selections(), &before);
    }

    #[test]
    fn replacement_arguments_are_literal_and_cancellation_restores_the_exact_view() {
        for replacement in ['m', '2', ':', ' ', '界', '\u{301}'] {
            let mut editor = Editor::new(Document::from("(a)\r\n"));
            let mut keys = KeyHandler::default();
            input(&mut keys, &mut editor, "lmr)").unwrap();
            keys.handle(&mut editor, Key::Char(replacement)).unwrap();
            assert_eq!(
                editor.document().text().to_string(),
                format!("{replacement}a{replacement}\r\n")
            );
            for selection in editor.selections().ranges() {
                for at in [selection.anchor, selection.head] {
                    assert!(grapheme::is_boundary(editor.document().text(), at).unwrap());
                }
            }
        }
        for cancel in [Key::Escape, Key::Ctrl('c'), Key::Enter, Key::Tab, Key::Left] {
            let mut editor = Editor::new(Document::from("(abc)"));
            let before = SelectionSet::single(range(4, 1));
            editor.set_selections(before.clone()).unwrap();
            editor.execute("select_mode", 1).unwrap();
            editor.preferred_columns = Some(vec![42]);
            let mut keys = KeyHandler::default();
            input(&mut keys, &mut editor, "9mr(").unwrap_err(); // Missing outer pair; no preview.
            assert_eq!(editor.selections(), &before);
            input(&mut keys, &mut editor, "mr(").unwrap();
            keys.handle(&mut editor, cancel).unwrap();
            assert_eq!(editor.selections(), &before);
            assert_eq!(editor.preferred_columns, Some(vec![42]));
            assert_eq!(editor.mode(), Mode::Select);
            assert_eq!(editor.document().undo_depth(), 0);
            assert!(keys.hints().is_none());
        }
    }

    #[test]
    fn missing_ambiguous_and_overlapping_pairs_leave_all_text_and_selections_untouched() {
        for (text, selections, sequence, error) in [
            (
                "(a) b",
                vec![range(1, 2), range(4, 5)],
                "md(",
                Error::SurroundNotFound,
            ),
            (
                "(ab)",
                vec![range(1, 2), range(2, 3)],
                "mr(",
                Error::SurroundOverlap,
            ),
            ("\"a\"", vec![range(0, 1)], "md\"", Error::SurroundAmbiguous),
        ] {
            let mut editor = Editor::new(Document::from(text));
            let before = SelectionSet::new(selections, 0).unwrap();
            editor.set_selections(before.clone()).unwrap();
            let mut keys = KeyHandler::default();
            assert_eq!(input(&mut keys, &mut editor, sequence), Err(error));
            assert_eq!(editor.document().text(), text);
            assert_eq!(editor.selections(), &before);
            assert_eq!(editor.document().undo_depth(), 0);
            assert!(keys.hints().is_none());
        }
    }

    #[test]
    fn distinct_nested_pairs_edit_together_and_syntax_resolves_a_quote_under_cursor() {
        let before = SelectionSet::new(vec![range(1, 2), range(3, 4)], 1).unwrap();
        for (sequence, expected) in [("md(", "abc"), ("mr(]", "[a[b]c]")] {
            let mut editor = Editor::new(Document::from("(a(b)c)"));
            editor.set_selections(before.clone()).unwrap();
            press(&mut editor, sequence);
            assert_eq!(editor.document().text(), expected);
            assert_eq!(editor.document().undo_depth(), 1);
            editor.execute("undo", 1).unwrap();
            assert_eq!(editor.selections(), &before);
        }
        let mut editor = Editor::new(Document::from("fn f() { f(\"a\\\"b\"); }"));
        editor.set_language(Some(crate::Language::Rust));
        editor
            .set_selections(SelectionSet::single(range(11, 12)))
            .unwrap();
        press(&mut editor, "mr\"'");
        assert_eq!(editor.document().text(), "fn f() { f('a\\\"b'); }");
    }

    #[test]
    fn explicit_brackets_do_not_parse_a_cold_language_buffer() {
        let mut editor = Editor::new(Document::from("fn f() { (word) }"));
        editor.set_language(Some(crate::Language::Rust));
        editor
            .set_selections(SelectionSet::single(range(10, 11)))
            .unwrap();
        let mut keys = KeyHandler::default();
        input(&mut keys, &mut editor, "mr(").unwrap();
        assert!(editor.parsed_syntax().is_none());
        input(&mut keys, &mut editor, "]").unwrap();
        assert_eq!(editor.document().text(), "fn f() { [word] }");
    }

    #[test]
    fn background_edits_and_previews_cancel_and_reject_stale_results() {
        let mut editor = Editor::new(Document::from("(word)"));
        editor.set_background_search(true);
        let mut keys = KeyHandler::default();
        input(&mut keys, &mut editor, "lmr(").unwrap();
        assert!(editor.search_waiting());
        let pending = editor.take_search_job().unwrap();
        keys.handle(&mut editor, Key::Escape).unwrap();
        assert!(pending.run().is_none());
        input(&mut keys, &mut editor, "mr(").unwrap();
        let result = editor.take_search_job().unwrap().run().unwrap();
        editor.apply_search_result(result).unwrap();
        assert_eq!(editor.selections().ranges(), &[range(0, 1), range(5, 6)]);
        input(&mut keys, &mut editor, "[").unwrap();
        assert_eq!(editor.selections().primary(), range(1, 2));
        assert_eq!(editor.document().text(), "(word)");
        let pending = editor.take_search_job().unwrap();
        editor.execute("move_right", 1).unwrap();
        assert!(pending.run().is_none());
        input(&mut keys, &mut editor, "md(").unwrap();
        let result = editor.take_search_job().unwrap().run().unwrap();
        editor.execute("move_right", 1).unwrap();
        assert_eq!(
            editor.apply_search_result(result).unwrap(),
            crate::SearchCompletion::Ignored
        );
        assert_eq!(editor.document().text(), "(word)");
        input(&mut keys, &mut editor, "md(").unwrap();
        let result = editor.take_search_job().unwrap().run().unwrap();
        editor.apply_search_result(result).unwrap();
        assert_eq!(editor.document().text(), "word");
        assert_eq!(editor.document().undo_depth(), 1);
    }

    #[test]
    fn external_edits_and_focus_changes_cancel_previews_before_mapping_original_selections() {
        let mut editor = Editor::new(Document::from("(abc)"));
        let before = SelectionSet::single(range(1, 4));
        editor.set_selections(before.clone()).unwrap();
        let other = editor.duplicate_view();
        let mut keys = KeyHandler::default();
        input(&mut keys, &mut editor, "mr(").unwrap();
        assert!(editor.focus_view(other));
        assert_eq!(editor.selections(), &before);
        input(&mut keys, &mut editor, "mr(").unwrap();
        let transaction = editor
            .document()
            .transaction([Edit::insert(CharOffset(0), "x")])
            .unwrap();
        editor.apply_external_change(transaction).unwrap();
        assert_eq!(editor.selections().primary(), range(2, 5));
        assert!(editor.replacement.is_none());
        keys.cancel(&mut editor);
        assert_eq!(editor.selections().primary(), range(2, 5));
    }

    #[test]
    fn surrounds_include_delimiters_preserve_direction_and_form_one_undo_step() {
        let mut editor = Editor::new(Document::from("a界"));
        let before = SelectionSet::new(
            vec![
                Selection::new(CharOffset(0), CharOffset(1)),
                Selection::new(CharOffset(2), CharOffset(1)),
            ],
            1,
        )
        .unwrap();
        editor.set_selections(before.clone()).unwrap();
        press(&mut editor, "v9ms)");
        assert_eq!(editor.document().text(), "(a)(界)");
        assert_eq!(editor.mode(), Mode::Normal);
        let after = SelectionSet::new(
            vec![
                Selection::new(CharOffset(0), CharOffset(3)),
                Selection::new(CharOffset(6), CharOffset(3)),
            ],
            1,
        )
        .unwrap();
        assert_eq!(editor.selections(), &after);
        assert_eq!(editor.document().undo_depth(), 1);
        press(&mut editor, "u");
        assert_eq!(editor.document().text(), "a界");
        assert_eq!(editor.selections(), &before);
        press(&mut editor, "U");
        assert_eq!(editor.document().text(), "(a)(界)");
        assert_eq!(editor.selections(), &after);
    }

    #[test]
    fn surrounds_handle_empty_selections_crlf_unicode_and_leave_yanks_unchanged() {
        let mut editor = Editor::new(Document::from(""));
        press(&mut editor, "ms]");
        assert_eq!(editor.document().text(), "[]");
        assert_eq!(
            editor.selections().primary(),
            Selection::new(CharOffset(0), CharOffset(2))
        );
        let mut editor = Editor::new(Document::from("ab\r\n"));
        press(&mut editor, "yms«");
        assert_eq!(editor.document().text(), "«a»b\r\n");
        press(&mut editor, "R"); // The yank still contains only a.
        assert_eq!(editor.document().text(), "ab\r\n");
        let mut handler = KeyHandler::default();
        for key in [Key::Char('m'), Key::Char('s'), Key::Enter] {
            handler.handle(&mut editor, key).unwrap();
        }
        assert_eq!(editor.document().text(), "\r\na\r\nb\r\n");
        assert_eq!(
            editor.selections().primary(),
            Selection::new(CharOffset(0), CharOffset(5))
        );
        let mut editor = Editor::new(Document::from("a界"));
        press(&mut editor, "lms\u{301}");
        for endpoint in [
            editor.selections().primary().anchor,
            editor.selections().primary().head,
        ] {
            assert!(grapheme::is_boundary(editor.document().text(), endpoint).unwrap());
        }
        press(&mut editor, "uU");
        assert_eq!(editor.document().text(), "a\u{301}界\u{301}");
    }

    #[test]
    fn literal_m_is_not_a_nearest_pair_request_and_cancel_preserves_select_mode() {
        let mut editor = Editor::new(Document::from("a"));
        press(&mut editor, "msm");
        assert_eq!(editor.document().text(), "mam");
        for cancel in [Key::Escape, Key::Ctrl('c'), Key::Tab] {
            let mut editor = Editor::new(Document::from("a"));
            let mut handler = KeyHandler::default();
            for key in [Key::Char('v'), Key::Char('m'), Key::Char('s'), cancel] {
                handler.handle(&mut editor, key).unwrap();
            }
            assert_eq!(editor.document().text(), "a");
            assert_eq!(editor.mode(), Mode::Select);
        }
    }

    #[test]
    fn a_selection_next_to_an_eof_cursor_keeps_separate_surrounds() {
        let mut editor = Editor::new(Document::from("a"));
        editor
            .set_selections(
                SelectionSet::new(
                    vec![
                        Selection::new(CharOffset(0), CharOffset(1)),
                        Selection::cursor(CharOffset(1)),
                    ],
                    1,
                )
                .unwrap(),
            )
            .unwrap();
        press(&mut editor, "ms[");
        assert_eq!(editor.document().text(), "[a][]");
        assert_eq!(
            editor.selections().ranges(),
            &[
                Selection::new(CharOffset(0), CharOffset(3)),
                Selection::new(CharOffset(3), CharOffset(5)),
            ]
        );
        assert_eq!(editor.selections().primary_index(), 1);
    }
}
