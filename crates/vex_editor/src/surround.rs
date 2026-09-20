//! Atomic surround edits. Delimiters insert at selection boundaries; selected
//! document contents remain in the rope and never get copied into edit strings.

use crate::{CommandContext, Error, Mode};
use vex_core::{CharOffset, Edit, Selection, SelectionSet, pairs};

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
