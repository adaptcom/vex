//! Selection scans over shared text. Large scans run on the search worker.

use std::num::NonZeroUsize;
use vex_core::{
    CharOffset, Document, Selection, SelectionSet, grapheme, layout::LayoutCache, motion,
};

use crate::Error;

pub(crate) fn copy_lines(
    document: &Document,
    origins: &SelectionSet,
    count: usize,
    down: bool,
    tabs: NonZeroUsize,
    cancelled: &impl Fn() -> bool,
) -> Result<SelectionSet, Error> {
    let text = document.text();
    let mut layout = LayoutCache::default();
    let mut ranges = origins.ranges().to_vec();
    let mut primary = origins.primary_index();
    for (index, &selection) in origins.ranges().iter().enumerate() {
        if cancelled() {
            return Ok(origins.clone());
        }
        let anchor = if selection.is_backward() {
            grapheme::previous(text, selection.anchor, 1)?
        } else {
            selection.anchor
        };
        let head = motion::cursor(text, selection)?;
        let anchor_column = layout.column(document, anchor, tabs)?;
        let head_column = layout.column(document, head, tabs)?;
        let mut anchor_row = text.char_to_line(anchor.0);
        let mut head_row = text.char_to_line(head.0);
        let height = anchor_row.abs_diff(head_row) + 1;
        let mut copies = 0;
        while copies < count {
            if cancelled() {
                return Ok(origins.clone());
            }
            if down {
                anchor_row = anchor_row.saturating_add(height);
                head_row = head_row.saturating_add(height);
                if anchor_row >= text.len_lines() || head_row >= text.len_lines() {
                    break;
                }
            } else {
                if anchor_row == 0 && head_row == 0 {
                    break;
                }
                anchor_row = anchor_row.saturating_sub(height);
                head_row = head_row.saturating_sub(height);
            }
            let (anchor, actual_anchor) = layout.at_column(
                document,
                CharOffset(text.line_to_char(anchor_row)),
                anchor_column,
                tabs,
            )?;
            if actual_anchor != anchor_column {
                continue;
            }
            let (head, actual_head) = layout.at_column(
                document,
                CharOffset(text.line_to_char(head_row)),
                head_column,
                tabs,
            )?;
            if actual_head != head_column {
                continue;
            }
            // Include the whole endpoint grapheme in either direction.
            let copy = motion::put_cursor(text, Selection::cursor(anchor), head, true)?;
            if index == origins.primary_index() {
                primary = ranges.len();
            }
            ranges.push(copy);
            copies += 1;
        }
    }
    Ok(SelectionSet::new(ranges, primary)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Dispatch, Editor, Key, KeyHandler, Mode, SearchCompletion};

    fn range(anchor: usize, head: usize) -> Selection {
        Selection::new(CharOffset(anchor), CharOffset(head))
    }

    #[test]
    fn copies_skip_short_lines_preserve_direction_and_follow_primary() {
        for backward in [false, true] {
            let mut editor = Editor::new(Document::from("abcd\na\nABCD\nxy\n1234"));
            let original = if backward { range(4, 1) } else { range(1, 4) };
            editor
                .set_selections(SelectionSet::single(original))
                .unwrap();
            editor.execute("select_mode", 1).unwrap();
            editor.execute("copy_selection_on_next_line", 2).unwrap();
            let expected = if backward {
                vec![original, range(11, 8), range(19, 16)]
            } else {
                vec![original, range(8, 11), range(16, 19)]
            };
            assert_eq!(editor.selections().ranges(), expected);
            assert_eq!(editor.selections().primary_index(), 2);
            assert_eq!(editor.mode(), Mode::Select);
            assert_eq!(editor.document().revision().get(), 0);
            assert_eq!(editor.document().undo_depth(), 0);
            editor
                .execute("copy_selection_on_next_line", usize::MAX)
                .unwrap();
            assert_eq!(editor.selections().ranges(), expected);
        }
    }

    #[test]
    fn copies_use_display_columns_and_graphemes_with_configured_tabs() {
        let mut editor = Editor::new(Document::from(
            "  e\u{301}x\n\t界x\nabcde\n  👩\u{200d}💻z\n",
        ));
        editor.set_tab_width(NonZeroUsize::new(2).unwrap());
        editor
            .set_selections(SelectionSet::single(range(2, 4)))
            .unwrap();
        editor.execute("copy_selection_on_next_line", 3).unwrap();
        assert_eq!(
            editor.selections().ranges(),
            &[range(2, 4), range(7, 8), range(12, 13), range(18, 21)]
        );
        assert_eq!(editor.selections().primary_index(), 3);
        // A target inside a tab or wide glyph must be skipped, not clamped.
        editor
            .set_selections(SelectionSet::single(range(4, 5)))
            .unwrap();
        editor.execute("copy_selection_on_next_line", 1).unwrap();
        assert_eq!(editor.selections().ranges(), &[range(4, 5), range(13, 14)]);
    }

    #[test]
    fn multiline_copies_advance_by_height_and_include_crlf_once() {
        let mut editor = Editor::new(Document::from("ab\r\ncd\r\nef\r\ngh\r\nij"));
        editor
            .set_selections(SelectionSet::single(range(0, 8)))
            .unwrap();
        editor.execute("copy_selection_on_next_line", 1).unwrap();
        assert_eq!(editor.selections().ranges(), &[range(0, 8), range(8, 16)]);
        assert_eq!(editor.selections().primary_index(), 1);
        editor.execute("keep_primary_selection", 1).unwrap();
        editor
            .execute("copy_selection_on_prev_line", usize::MAX)
            .unwrap();
        assert_eq!(editor.selections().ranges(), &[range(0, 8), range(8, 16)]);
        assert_eq!(editor.selections().primary_index(), 0);
    }

    #[test]
    fn overlapping_copies_merge_and_c_key_retains_mode() {
        for mode in [Mode::Normal, Mode::Select] {
            let mut editor = Editor::new(Document::from("ab\ncd\nef"));
            editor
                .set_selections(SelectionSet::new(vec![range(0, 1), range(3, 4)], 1).unwrap())
                .unwrap();
            if mode == Mode::Select {
                editor.execute("select_mode", 1).unwrap();
            }
            assert_eq!(
                KeyHandler::default()
                    .handle(&mut editor, Key::Char('C'))
                    .unwrap(),
                Dispatch::Executed("copy_selection_on_next_line")
            );
            assert_eq!(
                editor.selections().ranges(),
                &[range(0, 1), range(3, 4), range(6, 7)]
            );
            assert_eq!(editor.selections().primary_index(), 2);
            assert_eq!(editor.mode(), mode);
        }
        let mut editor = Editor::new(Document::default());
        editor.execute("insert_mode", 1).unwrap();
        KeyHandler::default()
            .handle(&mut editor, Key::Char('C'))
            .unwrap();
        assert_eq!(editor.document().text(), "C");
    }

    #[test]
    fn background_copies_preserve_order_and_reject_cancelled_or_stale_work() {
        let mut editor = Editor::new(Document::from("abc\ndef\nghi"));
        editor.set_background_search(true);
        let original = editor.selections().clone();
        editor.execute("copy_selection_on_next_line", 2).unwrap();
        assert_eq!(editor.selections(), &original);
        assert!(editor.search_waiting());
        assert_eq!(editor.search_progress(), Some("selecting..."));
        let result = editor.take_search_job().unwrap().run().unwrap();
        assert_eq!(
            editor.apply_search_result(result).unwrap(),
            SearchCompletion::Navigation
        );
        assert_eq!(
            editor.selections().ranges(),
            &[range(0, 1), range(4, 5), range(8, 9)]
        );
        assert!(!editor.search_waiting());
        editor.execute("copy_selection_on_next_line", 1).unwrap();
        let job = editor.take_search_job().unwrap();
        editor.execute("normal_mode", 1).unwrap();
        assert!(job.run().is_none());
        editor.execute("copy_selection_on_next_line", 1).unwrap();
        let result = editor.take_search_job().unwrap().run().unwrap();
        editor.execute("move_right", 1).unwrap();
        let moved = editor.selections().clone();
        assert_eq!(
            editor.apply_search_result(result).unwrap(),
            SearchCompletion::Ignored
        );
        assert_eq!(editor.selections(), &moved);
        editor.execute("copy_selection_on_next_line", 1).unwrap();
        let result = editor.take_search_job().unwrap().run().unwrap();
        let tabs = editor.tab_width();
        editor.set_tab_width(NonZeroUsize::new(tabs.get() + 1).unwrap());
        editor.set_tab_width(tabs);
        assert_eq!(
            editor.apply_search_result(result).unwrap(),
            SearchCompletion::Ignored
        );
        assert_eq!(editor.selections(), &moved);
    }
}
