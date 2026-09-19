//! Pure movement calculations. All positions are scalar offsets on grapheme boundaries.

use std::num::NonZeroUsize;
use unicode_width::UnicodeWidthStr;

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

fn class(text: &Rope, position: CharOffset) -> Option<Class> {
    text.get_char(position.0).map(|ch| {
        if ch.is_whitespace() {
            Class::Space
        } else if ch.is_alphanumeric() || ch == '_' {
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
    word_forward_impl(text, selection, count, false)
}

/// Select through the next word end, excluding following whitespace.
pub fn word_end(text: &Rope, selection: Selection, count: usize) -> Result<Selection, Error> {
    word_forward_impl(text, selection, count, true)
}

fn word_forward_impl(
    text: &Rope,
    selection: Selection,
    count: usize,
    end: bool,
) -> Result<Selection, Error> {
    if count == 0 {
        return Ok(selection);
    }
    let mut start = cursor(text, selection)?;
    let mut head = grapheme::next(text, start, 1)?;
    if head.0 == text.len_chars() {
        return Ok(selection);
    }
    let mut previous = class(text, start);
    let mut scanner = grapheme::Cursor::new(text, head)?;
    for step in 0..count {
        let initial = head;
        loop {
            let current = class(text, head);
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
    if count == 0 {
        return Ok(selection);
    }
    let position = cursor(text, selection)?;
    if position.0 == 0 {
        return Ok(selection);
    }
    let mut start = grapheme::next(text, position, 1)?;
    let mut head = position;
    let mut right = class(text, head);
    for step in 0..count {
        let initial = head;
        loop {
            let left = grapheme::previous(text, head, 1)?;
            let left_class = if head.0 == 0 { None } else { class(text, left) };
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
        Some(text) => text.width(),
        None => slice.to_string().width(),
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
