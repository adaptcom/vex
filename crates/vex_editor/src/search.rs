//! Search state belongs to the editor; a frontend only owns prompt editing and
//! viewport restoration. Search never changes document text or adds undo steps.

use std::sync::Arc;
use vex_core::{
    ByteOffset, CharOffset, Revision, Selection, SelectionSet, grapheme, search::Literal,
};

use crate::{CommandContext, Editor, Error, Mode, SearchDirection};

/// Result of the current preview; accepting a missing query keeps it open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchStatus {
    Empty,
    Match,
    NoMatch,
}

#[derive(Debug, Default)]
pub(crate) struct Search {
    accepted: Option<(Arc<Literal>, SearchDirection)>,
    pub preview: Option<Preview>,
}

#[derive(Debug)]
pub(crate) struct Preview {
    origin: SelectionSet,
    columns: Option<Vec<usize>>,
    revision: Revision,
    mode: Mode,
    count: usize,
    pattern: Option<Arc<Literal>>,
    pub direction: SearchDirection,
    pub status: SearchStatus,
}

pub(crate) fn begin(ctx: &mut CommandContext<'_>, direction: SearchDirection) -> Result<(), Error> {
    let editor = &mut *ctx.editor;
    if editor.search.preview.is_some() {
        return Err(Error::SearchActive);
    }
    if editor.mode == Mode::Insert {
        return Err(Error::WrongMode {
            expected: Mode::Normal,
            actual: editor.mode,
        });
    }
    editor.finish_undo_group();
    editor.search.preview = Some(Preview {
        origin: editor.selections.clone(),
        columns: editor.preferred_columns.clone(),
        revision: editor.document.revision(),
        mode: editor.mode,
        count: ctx.count.get(),
        pattern: None,
        direction,
        status: SearchStatus::Empty,
    });
    Ok(())
}

// Direct API callers can edit or change mode while a prompt is open. Never
// install old coordinates in a different revision or mode.
fn check_preview(editor: &mut Editor) -> Result<(), Error> {
    let preview = editor
        .search
        .preview
        .as_ref()
        .ok_or(Error::NoSearchPreview)?;
    if preview.revision != editor.document.revision() || preview.mode != editor.mode {
        editor.search.preview = None;
        return Err(Error::SearchChanged);
    }
    Ok(())
}

pub(crate) fn update(editor: &mut Editor, query: &str) -> Result<(), Error> {
    check_preview(editor)?;
    let preview = editor.search.preview.as_ref().unwrap();
    let pattern = (!query.is_empty()).then(|| Arc::new(Literal::new(query)));
    let found = pattern
        .as_ref()
        .map(|pattern| {
            locate(
                editor,
                &preview.origin,
                pattern,
                preview.direction,
                preview.count,
                true,
            )
        })
        .transpose()?;
    let preview = editor.search.preview.as_mut().unwrap();
    preview.status = match &found {
        None => SearchStatus::Empty,
        Some(None) => SearchStatus::NoMatch,
        Some(Some(_)) => SearchStatus::Match,
    };
    editor.selections = found.flatten().unwrap_or_else(|| preview.origin.clone());
    editor.preferred_columns = if preview.status == SearchStatus::Match {
        None
    } else {
        preview.columns.clone()
    };
    preview.pattern = pattern;
    Ok(())
}

pub(crate) fn accept(editor: &mut Editor) -> Result<(), Error> {
    check_preview(editor)?;
    let preview = editor.search.preview.as_ref().unwrap();
    match preview.status {
        SearchStatus::Empty => cancel(editor),
        SearchStatus::NoMatch => Err(Error::NoMatch),
        SearchStatus::Match => {
            let preview = editor.search.preview.take().unwrap();
            editor.search.accepted = Some((preview.pattern.unwrap(), preview.direction));
            Ok(())
        }
    }
}

pub(crate) fn cancel(editor: &mut Editor) -> Result<(), Error> {
    check_preview(editor)?;
    let preview = editor.search.preview.take().unwrap();
    editor.selections = preview.origin;
    editor.preferred_columns = preview.columns;
    Ok(())
}

pub(crate) fn repeat(ctx: &mut CommandContext<'_>, reverse: bool) -> Result<(), Error> {
    let editor = &mut *ctx.editor;
    if editor.search.preview.is_some() {
        return Err(Error::SearchActive);
    }
    if editor.mode == Mode::Insert {
        return Err(Error::WrongMode {
            expected: Mode::Normal,
            actual: editor.mode,
        });
    }
    let (pattern, direction) = editor.search.accepted.as_ref().ok_or(Error::NoSearch)?;
    let direction = if reverse {
        direction.reversed()
    } else {
        *direction
    };
    let selections = locate(
        editor,
        &editor.selections,
        pattern,
        direction,
        ctx.count.get(),
        false,
    )?;
    editor.finish_undo_group();
    editor.selections = selections.ok_or(Error::NoMatch)?;
    editor.preferred_columns = None;
    Ok(())
}

fn locate(
    editor: &Editor,
    origins: &SelectionSet,
    pattern: &Literal,
    direction: SearchDirection,
    count: usize,
    inclusive: bool,
) -> Result<Option<SelectionSet>, Error> {
    let text = editor.document.text();
    let mut selections = Vec::with_capacity(origins.ranges().len());
    for origin in origins.ranges() {
        let position = origin.start();
        // Moving past the entire first grapheme prevents a match inside a
        // combining sequence from repeatedly selecting that same grapheme.
        let boundary = if inclusive == (direction == SearchDirection::Forward) {
            position
        } else {
            grapheme::next(text, position, 1)?
        };
        let boundary = ByteOffset(text.char_to_byte(boundary.0));
        let before = ByteOffset(0)..boundary;
        let after = boundary..ByteOffset(text.len_bytes());
        let (first, second) = if direction == SearchDirection::Forward {
            (after, before)
        } else {
            (before, after)
        };
        let scan = || {
            pattern
                .matches(text, first.clone(), direction)
                .chain(pattern.matches(text, second.clone(), direction))
        };
        // At most two document traversals even for usize::MAX counts. The
        // common case stops at the requested nearby match without a full scan.
        let mut seen = 0;
        let mut last_start = None;
        let mut wanted = count;
        let mut selected = None;
        for pass in 0..2 {
            for found in scan() {
                let start = grapheme::floor(text, CharOffset(text.byte_to_char(found.start.0)))?;
                if last_start == Some(start) {
                    continue;
                }
                last_start = Some(start);
                seen += 1;
                if seen == wanted {
                    let end = grapheme::ceil(text, CharOffset(text.byte_to_char(found.end.0)))?;
                    selected = Some(if direction == SearchDirection::Forward {
                        Selection::new(start, end)
                    } else {
                        Selection::new(end, start)
                    });
                    break;
                }
            }
            if selected.is_some() || seen == 0 {
                break;
            }
            if pass == 0 {
                wanted = (count - 1) % seen + 1;
                seen = 0;
                last_start = None;
            }
        }
        let Some(selected) = selected else {
            return Ok(None);
        };
        selections.push(selected);
    }
    Ok(Some(SelectionSet::new(
        selections,
        origins.primary_index(),
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Key, KeyHandler, Keymap, commands};
    use proptest::prelude::*;
    use vex_core::Document;

    fn range(start: usize, end: usize) -> Selection {
        Selection::new(CharOffset(start), CharOffset(end))
    }

    fn accept(editor: &mut Editor, query: &str, backward: bool) {
        editor
            .execute(
                if backward {
                    "search_backward"
                } else {
                    "search_forward"
                },
                1,
            )
            .unwrap();
        editor.update_search(query).unwrap();
        editor.execute("search_accept", 1).unwrap();
    }

    #[test]
    fn previews_always_start_at_the_origin_and_cancel_restores_columns() {
        let mut editor = Editor::new(Document::from("x cat cater"));
        editor.preferred_columns = Some(vec![12]);
        let origin = editor.selections.clone();
        commands::search_forward(&mut CommandContext::new(&mut editor)).unwrap();
        for (query, expected) in [
            ("c", range(2, 3)),
            ("ca", range(2, 4)),
            ("cater", range(6, 11)),
            ("cat", range(2, 5)),
        ] {
            editor.update_search(query).unwrap();
            assert_eq!(editor.selections.primary(), expected);
        }
        editor.update_search("missing").unwrap();
        assert_eq!(editor.search_status(), Some(SearchStatus::NoMatch));
        assert_eq!(editor.selections, origin);
        assert_eq!(editor.execute("search_accept", 1), Err(Error::NoMatch));
        assert!(editor.search_direction().is_some());
        editor.update_search("cat").unwrap();
        commands::search_cancel(&mut CommandContext::new(&mut editor)).unwrap();
        assert_eq!(editor.selections, origin);
        assert_eq!(editor.preferred_columns, Some(vec![12]));
        assert_eq!(editor.execute("search_next", 1), Err(Error::NoSearch));
    }

    #[test]
    fn overlapping_matches_wrap_and_huge_counts_finish_in_two_scans() {
        let mut editor = Editor::new(Document::from("ababa"));
        accept(&mut editor, "aba", false);
        assert_eq!(editor.selections.primary(), range(0, 3));
        editor.execute("search_next", 1).unwrap();
        assert_eq!(editor.selections.primary(), range(2, 5));
        editor.execute("search_next", 1).unwrap();
        assert_eq!(editor.selections.primary(), range(0, 3));
        editor.execute("search_next", usize::MAX).unwrap();
        assert_eq!(editor.selections.primary(), range(2, 5));
        editor.execute("search_previous", 2).unwrap();
        assert_eq!(editor.selections.primary(), range(5, 2));
        assert_eq!(editor.document.revision().get(), 0);
        assert_eq!(editor.document.undo_depth(), 0);
    }

    #[test]
    fn n_follows_the_accepted_direction_and_n_uppercase_reverses_it() {
        let mut editor = Editor::new(Document::from("cat x cat x cat"));
        editor.execute("goto_file_end", 1).unwrap();
        accept(&mut editor, "cat", true);
        assert_eq!(editor.selections.primary(), range(15, 12));
        editor.execute("search_next", 1).unwrap();
        assert_eq!(editor.selections.primary(), range(9, 6));
        editor.execute("search_previous", 1).unwrap();
        assert_eq!(editor.selections.primary(), range(12, 15));
        editor.execute("search_next", 3).unwrap();
        assert_eq!(editor.selections.primary(), range(15, 12));
        editor.execute("search_backward", 2).unwrap();
        editor.update_search("cat").unwrap();
        assert_eq!(editor.selections.primary(), range(9, 6));
    }

    #[test]
    fn substring_matches_expand_to_graphemes_and_repeats_make_progress() {
        let mut editor = Editor::new(Document::from("e\u{301}\u{301} e\u{301} 🦀"));
        accept(&mut editor, "\u{301}", false);
        assert_eq!(editor.selections.primary(), range(0, 3));
        editor.execute("search_next", 1).unwrap();
        assert_eq!(editor.selections.primary(), range(4, 6));
        editor.execute("search_next", 3).unwrap();
        assert_eq!(editor.selections.primary(), range(0, 3));
        editor.execute("search_previous", 1).unwrap();
        assert_eq!(editor.selections.primary(), range(6, 4));
        accept(&mut editor, "🦀", false);
        assert_eq!(editor.selections.primary(), range(7, 8));
        editor.execute("search_next", usize::MAX).unwrap();
        assert_eq!(editor.selections.primary(), range(7, 8));
    }

    #[test]
    fn multiple_selections_keep_the_primary_and_merge_collisions() {
        let mut editor = Editor::new(Document::from("x aa x aa x aa"));
        let original = SelectionSet::new(vec![range(1, 0), range(7, 8)], 1).unwrap();
        editor.set_selections(original.clone()).unwrap();
        editor.execute("select_mode", 1).unwrap();
        accept(&mut editor, "aa", false);
        assert_eq!(editor.selections.ranges(), &[range(2, 4), range(7, 9)]);
        assert_eq!(editor.selections.primary_index(), 1);
        assert_eq!(editor.mode, Mode::Select);
        editor.execute("search_next", 2).unwrap();
        assert_eq!(editor.selections.ranges(), &[range(2, 4), range(12, 14)]);
        assert_eq!(editor.selections.primary_index(), 0);
        editor
            .set_selections(SelectionSet::new(vec![range(0, 1), range(1, 2)], 1).unwrap())
            .unwrap();
        editor.execute("search_forward", 1).unwrap();
        editor.update_search("aa").unwrap();
        assert_eq!(editor.selections.ranges(), &[range(2, 4)]);
        editor.execute("search_cancel", 1).unwrap();
        assert_eq!(editor.selections.ranges(), &[range(0, 1), range(1, 2)]);
        assert_eq!(editor.selections.primary_index(), 1);
    }

    #[test]
    fn empty_and_cancelled_searches_preserve_the_accepted_pattern() {
        let mut editor = Editor::new(Document::from("a b a"));
        accept(&mut editor, "a", false);
        editor.execute("search_backward", 1).unwrap();
        editor.update_search("b").unwrap();
        editor.update_search("").unwrap();
        assert_eq!(editor.search_status(), Some(SearchStatus::Empty));
        editor.execute("search_accept", 1).unwrap();
        editor.execute("search_next", 1).unwrap();
        assert_eq!(editor.selections.primary(), range(4, 5));
        editor.execute("search_backward", 1).unwrap();
        editor.update_search("b").unwrap();
        editor.execute("search_cancel", 1).unwrap();
        editor.execute("search_next", 1).unwrap();
        assert_eq!(editor.selections.primary(), range(0, 1));
        let mut empty = Editor::new(Document::default());
        empty.execute("search_forward", 1).unwrap();
        empty.update_search("x").unwrap();
        assert_eq!(empty.search_status(), Some(SearchStatus::NoMatch));
        empty.execute("search_cancel", 1).unwrap();
    }

    #[test]
    fn accepted_searches_follow_edits_undo_redo_without_stale_match_caches() {
        let mut editor = Editor::new(Document::from("cat cat"));
        accept(&mut editor, "cat", false);
        editor.execute("delete_selection", 1).unwrap();
        editor.execute("search_next", 1).unwrap();
        assert_eq!(editor.selections.primary(), range(1, 4));
        editor.execute("delete_selection", 1).unwrap();
        let selections = editor.selections.clone();
        assert_eq!(editor.execute("search_next", 1), Err(Error::NoMatch));
        assert_eq!(editor.selections, selections);
        editor.execute("undo", 2).unwrap();
        let snapshot = editor.document.snapshot();
        editor.execute("search_next", 1).unwrap();
        assert_eq!(editor.selections.primary(), range(4, 7));
        assert_eq!(editor.document.revision(), snapshot.revision());
        assert!(editor.document.text().is_instance(snapshot.text()));
        editor.execute("redo", 2).unwrap();
        assert_eq!(editor.document.text(), " ");
    }

    #[test]
    fn stale_previews_never_restore_coordinates_after_an_external_edit() {
        for action in ["search_update", "search_accept", "search_cancel"] {
            let mut editor = Editor::new(Document::from("abc abc"));
            editor.execute("search_forward", 1).unwrap();
            editor.update_search("abc").unwrap();
            editor.execute("delete_selection", 1).unwrap();
            let before = editor.selections.clone();
            let mut context = CommandContext::new(&mut editor);
            context.text = Some("abc");
            assert_eq!(
                (commands::find(action).unwrap().run)(&mut context),
                Err(Error::SearchChanged)
            );
            assert_eq!(editor.selections, before);
            assert_eq!(editor.search_direction(), None);
        }
    }

    #[test]
    fn search_bindings_use_documented_functions_and_preserve_counts() {
        let mut editor = Editor::new(Document::from("a b a b a"));
        let mut keys = KeyHandler::default();
        for key in "2/".chars() {
            keys.handle(&mut editor, Key::Char(key)).unwrap();
        }
        editor.update_search("a").unwrap();
        assert_eq!(editor.selections.primary(), range(4, 5));
        editor.execute("search_accept", 1).unwrap();
        for key in "2n".chars() {
            keys.handle(&mut editor, Key::Char(key)).unwrap();
        }
        assert_eq!(editor.selections.primary(), range(0, 1));
        let mut map = Keymap::empty();
        map.bind(Mode::Normal, vec![Key::Char('s')], "search_backward")
            .unwrap();
        KeyHandler::new(map)
            .handle(&mut editor, Key::Char('s'))
            .unwrap();
        assert_eq!(editor.search_direction(), Some(SearchDirection::Backward));
        assert!(
            commands::find("search_backward")
                .unwrap()
                .description()
                .contains("literal")
        );
        editor.execute("search_cancel", 1).unwrap();
        editor.execute("insert_mode", 1).unwrap();
        for key in "/?nN".chars() {
            keys.handle(&mut editor, Key::Char(key)).unwrap();
        }
        assert!(editor.document.text().to_string().starts_with("/?nN"));
    }

    proptest! {
        #[test]
        fn counted_wrapping_navigation_agrees_with_flat_overlapping_matches(
            source in "[abc ]{0,100}", query in "[abc ]{1,5}",
            position in any::<usize>(), count in 1usize..=usize::MAX, backward in any::<bool>(),
        ) {
            let mut editor = Editor::new(Document::from(source.as_str()));
            let position = position % (source.len() + 1);
            editor.set_selections(SelectionSet::single(Selection::cursor(CharOffset(position)))).unwrap();
            let original = editor.selections.clone();
            let matches: Vec<_> = source.as_bytes().windows(query.len()).enumerate()
                .filter_map(|(i, text)| (text == query.as_bytes()).then_some(i)).collect();
            let expected = |origin: usize, inclusive: bool| {
                let mut positions = matches.clone();
                positions.sort_by_key(|&i| if backward { (i > origin || (!inclusive && i == origin), usize::MAX - i) } else { (i < origin || (!inclusive && i == origin), i) });
                if positions.is_empty() { None } else { Some(positions[(count - 1) % positions.len()]) }
            };
            editor.execute(if backward { "search_backward" } else { "search_forward" }, count).unwrap();
            editor.update_search(&query).unwrap();
            if let Some(position) = expected(position, true) {
                prop_assert_eq!(editor.selections.primary().start(), CharOffset(position));
                editor.execute("search_accept", 1).unwrap();
                editor.execute("search_next", count).unwrap();
                prop_assert_eq!(editor.selections.primary().start(), CharOffset(expected(position, false).unwrap()));
            } else {
                prop_assert_eq!(editor.search_status(), Some(SearchStatus::NoMatch));
                prop_assert_eq!(editor.selections(), &original);
            }
        }
    }
}
