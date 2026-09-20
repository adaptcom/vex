//! Pure movement calculations. All positions are scalar offsets on grapheme boundaries.

use std::num::NonZeroUsize;

use crate::{CharOffset, Error, Rope, Selection, grapheme};

/// The displayed cursor of a directional, half-open selection.
pub fn cursor(text: &Rope, selection: Selection) -> Result<CharOffset, Error> {
    if selection.anchor < selection.head {
        grapheme::previous(text, selection.head, 1)
    } else {
        grapheme::floor(text, selection.head)
    }
}

/// Select the whole grapheme at a position, or an empty caret at EOF.
pub fn block(text: &Rope, position: CharOffset) -> Result<Selection, Error> {
    let start = grapheme::floor(text, position)?;
    Ok(Selection::new(start, grapheme::next(text, start, 1)?))
}

/// Place a block cursor, optionally extending from the original anchor grapheme.
/// Crossing the anchor keeps that grapheme selected in either direction.
pub fn put_cursor(
    text: &Rope,
    selection: Selection,
    destination: CharOffset,
    extend: bool,
) -> Result<Selection, Error> {
    let destination = grapheme::floor(text, destination)?;
    if !extend {
        return block(text, destination);
    }
    let origin = if selection.is_backward() {
        grapheme::previous(text, selection.anchor, 1)?
    } else {
        grapheme::floor(text, selection.anchor)?
    };
    if destination < origin {
        Ok(Selection::new(
            grapheme::next(text, origin, 1)?,
            destination,
        ))
    } else {
        Ok(Selection::new(
            origin,
            grapheme::next(text, destination, 1)?,
        ))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Space,
    Word,
    Punctuation,
}

fn class(text: &Rope, position: CharOffset, long: bool) -> Option<Class> {
    text.get_char(position.0).map(|ch| {
        if ch.is_whitespace() {
            Class::Space
        } else if long || ch.is_alphanumeric() || ch == '_' {
            Class::Word
        } else {
            Class::Punctuation
        }
    })
}

/// Select through the next word start, treating Unicode letters/numbers and
/// underscore as words, punctuation as a separate run, and whitespace as gaps.
/// Repeats continue from the previous result; no whole-line allocation is needed.
pub fn word_forward(text: &Rope, selection: Selection, count: usize) -> Result<Selection, Error> {
    word_forward_impl(text, selection, count, false, false)
}

/// Select through the next word end, excluding following whitespace.
pub fn word_end(text: &Rope, selection: Selection, count: usize) -> Result<Selection, Error> {
    word_forward_impl(text, selection, count, true, false)
}

/// Select through the next whitespace-separated WORD start; punctuation belongs
/// to the surrounding run. Counts and selections behave like word_forward.
pub fn long_word_forward(
    text: &Rope,
    selection: Selection,
    count: usize,
) -> Result<Selection, Error> {
    word_forward_impl(text, selection, count, false, true)
}

/// Select through the end of a whitespace-separated WORD.
pub fn long_word_end(text: &Rope, selection: Selection, count: usize) -> Result<Selection, Error> {
    word_forward_impl(text, selection, count, true, true)
}

fn word_forward_impl(
    text: &Rope,
    selection: Selection,
    count: usize,
    end: bool,
    long: bool,
) -> Result<Selection, Error> {
    if count == 0 {
        return Ok(selection);
    }
    let mut start = cursor(text, selection)?;
    let mut head = grapheme::next(text, start, 1)?;
    if head.0 == text.len_chars() {
        return Ok(selection);
    }
    let mut previous = class(text, start, long);
    let mut scanner = grapheme::Cursor::new(text, head)?;
    for step in 0..count {
        let initial = head;
        loop {
            let current = class(text, head, long);
            let target = previous != current
                && if end {
                    previous != Some(Class::Space)
                } else {
                    current != Some(Class::Space)
                };
            if target {
                if head != initial {
                    break;
                }
                if step == 0 {
                    start = head;
                }
            }
            if current.is_none() {
                break;
            }
            previous = current;
            head = scanner.next().unwrap_or(CharOffset(text.len_chars()));
        }
        if head.0 == text.len_chars() {
            break;
        }
    }
    Ok(Selection::new(start, head))
}

/// Select backward to a word start, including intervening whitespace.
pub fn word_backward(text: &Rope, selection: Selection, count: usize) -> Result<Selection, Error> {
    word_backward_impl(text, selection, count, false)
}

/// Select backward to a whitespace-separated WORD start.
pub fn long_word_backward(
    text: &Rope,
    selection: Selection,
    count: usize,
) -> Result<Selection, Error> {
    word_backward_impl(text, selection, count, true)
}

fn word_backward_impl(
    text: &Rope,
    selection: Selection,
    count: usize,
    long: bool,
) -> Result<Selection, Error> {
    if count == 0 {
        return Ok(selection);
    }
    let position = cursor(text, selection)?;
    if position.0 == 0 {
        return Ok(selection);
    }
    let mut start = grapheme::next(text, position, 1)?;
    let mut head = position;
    let mut right = class(text, head, long);
    for step in 0..count {
        let initial = head;
        loop {
            let left = grapheme::previous(text, head, 1)?;
            let left_class = if head.0 == 0 {
                None
            } else {
                class(text, left, long)
            };
            let target = right != left_class && right != Some(Class::Space);
            if target {
                if head != initial {
                    break;
                }
                if step == 0 {
                    start = head;
                }
            }
            if head.0 == 0 {
                break;
            }
            head = left;
            right = left_class;
        }
        if head.0 == 0 {
            break;
        }
    }
    Ok(Selection::new(start, head))
}

/// Find the counted character beyond the current grapheme without wrapping or
/// stopping at line boundaries. A newline target matches any logical line ending.
/// Matches inside one grapheme count once, and destinations are whole graphemes.
/// Exclusive finds skip the adjacent grapheme so repeating a till motion advances.
/// Missing the requested occurrence returns None rather than a partial move.
pub fn find_char(
    text: &Rope,
    position: CharOffset,
    character: char,
    count: usize,
    direction: crate::search::Direction,
    inclusive: bool,
) -> Result<Option<CharOffset>, Error> {
    use crate::search::Direction;
    let position = grapheme::floor(text, position)?;
    if count == 0 {
        return Ok(None);
    }
    let start = match direction {
        Direction::Forward => grapheme::next(text, position, if inclusive { 1 } else { 2 })?,
        Direction::Backward if !inclusive => grapheme::previous(text, position, 1)?,
        Direction::Backward => position,
    };
    let mut chars = text.chars_at(start.0);
    let mut offset = start.0;
    let mut remaining = count;
    let mut last = None;
    loop {
        let (index, ch) = match direction {
            Direction::Forward => {
                let Some(ch) = chars.next() else { break };
                let index = offset;
                offset += 1;
                (index, ch)
            }
            Direction::Backward => {
                let Some(ch) = chars.prev() else { break };
                offset -= 1;
                (offset, ch)
            }
        };
        let matches = if character == '\n' {
            matches!(
                ch,
                '\n' | '\r' | '\u{000b}' | '\u{000c}' | '\u{0085}' | '\u{2028}' | '\u{2029}'
            )
        } else {
            ch == character
        };
        if !matches {
            continue;
        }
        let found = grapheme::floor(text, CharOffset(index))?;
        if last == Some(found) {
            continue;
        }
        last = Some(found);
        remaining -= 1;
        if remaining == 0 {
            return Ok(Some(match (inclusive, direction) {
                (true, _) => found,
                (false, Direction::Forward) => grapheme::previous(text, found, 1)?,
                (false, Direction::Backward) => grapheme::next(text, found, 1)?,
            }));
        }
    }
    Ok(None)
}

/// First non-whitespace grapheme of the current logical line; None on blank lines.
pub fn first_nonwhitespace(text: &Rope, position: CharOffset) -> Result<Option<CharOffset>, Error> {
    let start = line_start(text, position)?;
    let end = line_end(text, position)?;
    text.slice(start.0..end.0)
        .chars()
        .position(|ch| !ch.is_whitespace())
        .map(|offset| grapheme::floor(text, CharOffset(start.0 + offset)))
        .transpose()
}

/// Position at a zero-based grapheme column in this line. Tabs and wide glyphs
/// each count once; oversized columns stop at the line-ending boundary.
pub fn at_grapheme_column(
    text: &Rope,
    position: CharOffset,
    column: usize,
) -> Result<CharOffset, Error> {
    let mut position = line_start(text, position)?;
    let end = line_end(text, position)?;
    let mut scanner = grapheme::Cursor::new(text, position)?;
    for _ in 0..column {
        if position == end {
            break;
        }
        position = scanner.next().unwrap_or(end).min(end);
    }
    Ok(position)
}

/// Start of the logical line containing a position.
pub fn line_start(text: &Rope, position: CharOffset) -> Result<CharOffset, Error> {
    let position = grapheme::floor(text, position)?;
    Ok(CharOffset(text.line_to_char(text.char_to_line(position.0))))
}

/// Boundary before the logical line ending, or EOF on the final line.
pub fn line_end(text: &Rope, position: CharOffset) -> Result<CharOffset, Error> {
    let position = grapheme::floor(text, position)?;
    let line = text.char_to_line(position.0);
    if line + 1 == text.len_lines() {
        return Ok(CharOffset(text.len_chars()));
    }
    grapheme::previous(text, CharOffset(text.line_to_char(line + 1)), 1)
}

fn width(
    text: &Rope,
    start: CharOffset,
    end: CharOffset,
    column: usize,
    tab_width: NonZeroUsize,
) -> usize {
    if text.get_char(start.0) == Some('\t') {
        return tab_width.get() - column % tab_width.get();
    }
    let slice = text.slice(start.0..end.0);
    // Most graphemes borrow a single rope chunk. Only split clusters allocate.
    match slice.as_str() {
        Some(text) => crate::display::width(text, column, tab_width),
        None => crate::display::width(&slice.to_string(), column, tab_width),
    }
}

/// Display column within a logical line, accounting for tabs and wide graphemes.
pub fn column(text: &Rope, position: CharOffset, tab_width: NonZeroUsize) -> Result<usize, Error> {
    let position = grapheme::floor(text, position)?;
    let mut start = line_start(text, position)?;
    let mut scanner = grapheme::Cursor::new(text, start)?;
    let mut column = 0usize;
    while start < position {
        let end = scanner.next().unwrap_or(position);
        column = column.saturating_add(width(text, start, end, column, tab_width));
        start = end;
    }
    Ok(column)
}

/// Move by logical lines toward a retained display column. Short lines clamp at
/// their line ending. Positions inside a wide glyph or tab resolve to its start.
/// The caller retains `goal_column` across consecutive vertical movements.
pub fn vertical(
    text: &Rope,
    position: CharOffset,
    count: usize,
    down: bool,
    goal_column: usize,
    tab_width: NonZeroUsize,
) -> Result<CharOffset, Error> {
    let position = grapheme::floor(text, position)?;
    let line = text.char_to_line(position.0);
    let target = if down {
        line.saturating_add(count).min(text.len_lines() - 1)
    } else {
        line.saturating_sub(count)
    };
    if target == line {
        return Ok(position);
    }
    let mut start = CharOffset(text.line_to_char(target));
    let end = line_end(text, start)?;
    let mut scanner = grapheme::Cursor::new(text, start)?;
    let mut column = 0usize;
    while start < end && column < goal_column {
        let next = scanner.next().unwrap_or(end);
        let next_column = column.saturating_add(width(text, start, next, column, tab_width));
        if next_column > goal_column {
            break;
        }
        column = next_column;
        start = next;
    }
    Ok(start)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selected(text: &Rope, selection: Selection) -> String {
        text.slice(selection.start().0..selection.end().0)
            .to_string()
    }

    #[test]
    fn long_words_keep_punctuation_together_and_counts_stop_at_document_edges() {
        let text = Rope::from_str("foo.bar + baz-qux\nnext");
        let initial = block(&text, CharOffset(0)).unwrap();
        assert_eq!(
            selected(&text, word_forward(&text, initial, 1).unwrap()),
            "foo"
        );
        let first = long_word_forward(&text, initial, 1).unwrap();
        assert_eq!(selected(&text, first), "foo.bar ");
        assert_eq!(
            selected(&text, long_word_forward(&text, first, 2).unwrap()),
            "+ baz-qux\n"
        );
        assert_eq!(
            selected(&text, long_word_end(&text, initial, 1).unwrap()),
            "foo.bar"
        );
        assert_eq!(
            selected(&text, long_word_end(&text, initial, 3).unwrap()),
            "foo.bar + baz-qux"
        );
        let end = block(&text, CharOffset(text.len_chars())).unwrap();
        assert_eq!(
            selected(&text, long_word_backward(&text, end, 1).unwrap()),
            "next"
        );
        assert_eq!(
            long_word_backward(&text, end, usize::MAX).unwrap().head,
            CharOffset(0)
        );
        assert_eq!(
            long_word_forward(&text, initial, usize::MAX)
                .unwrap()
                .head
                .0,
            text.len_chars()
        );
        assert_eq!(long_word_end(&text, initial, 0).unwrap(), initial);
    }

    #[test]
    fn character_finds_cross_lines_and_till_skips_adjacent_matches() {
        use crate::search::Direction::{Backward, Forward};
        let text = Rope::from_str("a:b:c\n:a");
        for (position, count, direction, inclusive, expected) in [
            (0, 1, Forward, true, Some(1)),
            (0, 2, Forward, true, Some(3)),
            (0, 3, Forward, true, Some(6)),
            (0, 4, Forward, true, None),
            (0, 1, Forward, false, Some(2)),
            (2, 1, Forward, false, Some(5)),
            (7, 1, Backward, true, Some(6)),
            (7, 2, Backward, true, Some(3)),
            (7, 1, Backward, false, Some(4)),
            (0, usize::MAX, Forward, true, None),
        ] {
            assert_eq!(
                find_char(
                    &text,
                    CharOffset(position),
                    ':',
                    count,
                    direction,
                    inclusive
                )
                .unwrap(),
                expected.map(CharOffset)
            );
        }
        let text = Rope::from_str(&format!("{}🦀", "x".repeat(4096)));
        assert_eq!(
            find_char(&text, CharOffset(0), '🦀', 1, Forward, true).unwrap(),
            Some(CharOffset(4096))
        );
        assert_eq!(
            find_char(&text, CharOffset(4096), 'x', 4096, Backward, true).unwrap(),
            Some(CharOffset(0))
        );
    }

    #[test]
    fn character_finds_keep_combining_clusters_emoji_and_mixed_line_endings_whole() {
        use crate::search::Direction::{Backward, Forward};
        let text = Rope::from_str("e\u{301}👩\u{200d}💻:🦀:");
        assert_eq!(
            find_char(&text, CharOffset(0), ':', 1, Forward, false).unwrap(),
            Some(CharOffset(2))
        );
        assert_eq!(
            find_char(&text, CharOffset(0), '\u{200d}', 1, Forward, true).unwrap(),
            Some(CharOffset(2))
        );
        assert_eq!(
            find_char(&text, CharOffset(5), '\u{301}', 1, Backward, true).unwrap(),
            Some(CharOffset(0))
        );
        let text = Rope::from_str("a\r\nb\nc\rd");
        for (count, expected) in [(1, 1), (2, 4), (3, 6)] {
            assert_eq!(
                find_char(&text, CharOffset(0), '\n', count, Forward, true).unwrap(),
                Some(CharOffset(expected))
            );
        }
        assert_eq!(
            find_char(&text, CharOffset(0), '\n', 1, Forward, false).unwrap(),
            Some(CharOffset(3))
        );
        assert_eq!(
            find_char(&text, CharOffset(7), '\n', 2, Backward, true).unwrap(),
            Some(CharOffset(4))
        );
        assert_eq!(
            find_char(&text, CharOffset(7), '\n', 1, Backward, false).unwrap(),
            Some(CharOffset(5))
        );
    }

    #[test]
    fn grapheme_columns_and_first_nonwhitespace_are_bounded_by_the_current_line() {
        let text = Rope::from_str("\t界e\u{301}x\r\n \t\n");
        for (column, expected) in [(0, 0), (1, 1), (2, 2), (3, 4), (usize::MAX, 5)] {
            assert_eq!(
                at_grapheme_column(&text, CharOffset(2), column).unwrap(),
                CharOffset(expected)
            );
        }
        assert_eq!(
            first_nonwhitespace(&text, CharOffset(4)).unwrap(),
            Some(CharOffset(1))
        );
        assert_eq!(first_nonwhitespace(&text, CharOffset(8)).unwrap(), None);
        assert_eq!(
            first_nonwhitespace(&Rope::new(), CharOffset(0)).unwrap(),
            None
        );
    }

    #[test]
    fn word_selections_repeat_and_handle_punctuation_and_unicode() {
        let text = Rope::from_str("hello world.foo e\u{301}lan!");
        let first = word_forward(&text, block(&text, CharOffset(0)).unwrap(), 1).unwrap();
        assert_eq!(selected(&text, first), "hello ");
        let second = word_forward(&text, first, 1).unwrap();
        assert_eq!(selected(&text, second), "world");
        let third = word_forward(&text, second, 1).unwrap();
        assert_eq!(selected(&text, third), ".");
        assert_eq!(
            selected(&text, word_forward(&text, first, 3).unwrap()),
            "world.foo "
        );
        assert_eq!(
            selected(&text, word_end(&text, first, 1).unwrap()),
            " world"
        );
        assert_eq!(
            selected(
                &text,
                word_backward(&text, block(&text, CharOffset(8)).unwrap(), 1).unwrap()
            ),
            "wor"
        );
        let combining = block(&text, CharOffset(16)).unwrap();
        assert_eq!(
            selected(&text, word_end(&text, combining, 1).unwrap()),
            "e\u{301}lan"
        );
    }

    #[test]
    fn extending_across_anchor_includes_whole_graphemes() {
        let text = Rope::from_str("ae\u{301}🦀z");
        let initial = block(&text, CharOffset(1)).unwrap();
        let backward = put_cursor(&text, initial, CharOffset(0), true).unwrap();
        assert_eq!(backward, Selection::new(CharOffset(3), CharOffset(0)));
        let forward = put_cursor(&text, backward, CharOffset(3), true).unwrap();
        assert_eq!(forward, Selection::new(CharOffset(1), CharOffset(4)));
    }

    #[test]
    fn vertical_movement_counts_cells_and_clamps_at_line_end() {
        let text = Rope::from_str("a\t界z\r\nx\r\n123456z");
        let tabs = NonZeroUsize::new(4).unwrap();
        assert_eq!(column(&text, CharOffset(3), tabs).unwrap(), 6);
        let short = vertical(&text, CharOffset(3), 1, true, 6, tabs).unwrap();
        assert_eq!(short, CharOffset(7));
        assert_eq!(
            vertical(&text, short, 1, true, 6, tabs).unwrap(),
            CharOffset(15)
        );
        assert_eq!(
            vertical(&text, CharOffset(15), 2, false, 5, tabs).unwrap(),
            CharOffset(2)
        );
        assert_eq!(line_start(&text, CharOffset(5)).unwrap(), CharOffset(0));
        assert_eq!(line_end(&text, CharOffset(0)).unwrap(), CharOffset(4));
    }
}
