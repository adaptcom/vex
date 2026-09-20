//! Cancellable word and paragraph textobjects over ropes.

use crate::{CharOffset, Error, Rope, Selection, grapheme};

fn class(ch: char, long: bool) -> u8 {
    if ch.is_whitespace() {
        0
    } else if long || ch.is_alphanumeric() || ch == '_' {
        1
    } else {
        2
    }
}

fn horizontal_space(ch: char) -> bool {
    ch.is_whitespace()
        && !matches!(
            ch,
            '\n' | '\r' | '\u{b}' | '\u{c}' | '\u{85}' | '\u{2028}' | '\u{2029}'
        )
}

/// Select the cursor’s word or WORD, optionally including adjacent horizontal whitespace.
/// Empty results represent whitespace/EOF. A canceled scan returns the input cursor.
pub fn word(
    text: &Rope,
    position: CharOffset,
    around: bool,
    long: bool,
    cancelled: &impl Fn() -> bool,
) -> Result<Selection, Error> {
    let position = grapheme::floor(text, position)?;
    let Some(ch) = text.get_char(position.0) else {
        return Ok(Selection::cursor(position));
    };
    let category = class(ch, long);
    // Helix leaves a point on whitespace instead of selecting the next word.
    if category == 0 {
        return Ok(Selection::cursor(position));
    }
    let mut start = position;
    let mut left = grapheme::Cursor::new(text, position)?;
    while let Some(previous) = left.previous() {
        if cancelled() {
            return Ok(Selection::cursor(position));
        }
        if class(text.char(previous.0), long) != category {
            break;
        }
        start = previous;
    }
    let mut right = grapheme::Cursor::new(text, position)?;
    let mut end = right.next().unwrap_or(position);
    while let Some(ch) = text.get_char(end.0) {
        if cancelled() {
            return Ok(Selection::cursor(position));
        }
        if class(ch, long) != category {
            break;
        }
        end = right.next().unwrap_or(end);
    }
    if around {
        let word_end = end;
        for ch in text.chars_at(end.0) {
            if cancelled() {
                return Ok(Selection::cursor(position));
            }
            if !horizontal_space(ch) {
                break;
            }
            end.0 += 1;
        }
        if end == word_end {
            for ch in text.chars_at(start.0).reversed() {
                if cancelled() {
                    return Ok(Selection::cursor(position));
                }
                if !horizontal_space(ch) {
                    break;
                }
                start.0 -= 1;
            }
        }
    }
    // A space can itself begin a grapheme with combining marks.
    Ok(Selection::new(
        grapheme::floor(text, start)?,
        grapheme::ceil(text, end)?,
    ))
}

/// A paragraph separator is an empty line, not an indented/whitespace-only line.
fn empty_line(text: &Rope, line: usize) -> bool {
    let line = text.line(line);
    line.len_chars() <= 2
        && line.chars().all(|ch| {
            matches!(
                ch,
                '\n' | '\r' | '\u{b}' | '\u{c}' | '\u{85}' | '\u{2028}' | '\u{2029}'
            )
        })
}

/// Select counted paragraphs at a cursor. Empty lines separate paragraphs; inner
/// objects exclude the final blank run and around objects include it.
pub fn paragraph(
    text: &Rope,
    position: CharOffset,
    around: bool,
    count: usize,
    cancelled: &impl Fn() -> bool,
) -> Result<Selection, Error> {
    let position = grapheme::floor(text, position)?;
    let cursor_line = text.char_to_line(position.0);
    let empty = empty_line(text, cursor_line);
    let follows_empty = empty_line(text, cursor_line.saturating_sub(1));
    let before_text =
        empty && cursor_line + 1 < text.len_lines() && !empty_line(text, cursor_line + 1);
    let last_on_line =
        grapheme::previous(text, CharOffset(text.line_to_char(cursor_line + 1)), 1)? == position;
    // A cursor on the final separator before text targets the following paragraph.
    let following = before_text && last_on_line;
    let mut start = cursor_line + usize::from((follows_empty && !empty) || before_text);
    if !following {
        for separator in [true, false] {
            while start > 0 && empty_line(text, start - 1) == separator {
                if cancelled() {
                    return Ok(Selection::cursor(position));
                }
                start -= 1;
            }
        }
    }
    let mut end = cursor_line + usize::from(following);
    let mut paragraphs = 0;
    let mut remaining = count;
    while remaining > 0 && end < text.len_lines() {
        let began = end;
        while end < text.len_lines() && !empty_line(text, end) {
            if cancelled() {
                return Ok(Selection::cursor(position));
            }
            end += 1;
        }
        paragraphs += usize::from(end != began);
        while end < text.len_lines() && empty_line(text, end) {
            if cancelled() {
                return Ok(Selection::cursor(position));
            }
            end += 1;
        }
        remaining -= 1;
    }
    // Trailing separator runs should still select text when no next paragraph exists.
    if paragraphs < count && end == text.len_lines() {
        for separator in [true, false] {
            while start > 0 && empty_line(text, start - 1) == separator {
                if cancelled() {
                    return Ok(Selection::cursor(position));
                }
                start -= 1;
            }
        }
    }
    if !around {
        while end > start && empty_line(text, end - 1) {
            if cancelled() {
                return Ok(Selection::cursor(position));
            }
            end -= 1;
        }
    }
    Ok(Selection::new(
        CharOffset(text.line_to_char(start)),
        CharOffset(text.line_to_char(end)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::Cell;

    #[test]
    fn large_scans_check_cancellation_without_allocating_the_selected_text() {
        let text = Rope::from_str(&"a".repeat(1 << 20));
        let checks = Cell::new(0);
        let cancelled = || {
            checks.set(checks.get() + 1);
            checks.get() >= 100
        };
        word(&text, CharOffset(0), false, false, &cancelled).unwrap();
        assert_eq!(checks.get(), 100);
        let text = Rope::from_str(&"line\n".repeat(100_000));
        checks.set(0);
        paragraph(&text, CharOffset(0), true, usize::MAX, &cancelled).unwrap();
        assert_eq!(checks.get(), 100);
        assert!(word(&text, CharOffset(usize::MAX), true, false, &|| false).is_err());
        assert!(paragraph(&text, CharOffset(usize::MAX), true, 1, &|| false).is_err());
    }

    proptest! {
        #[test]
        fn objects_keep_valid_grapheme_boundaries(
            parts in prop::collection::vec(prop::sample::select(vec!["a", "_", "e\u{301}", " ", "\t", "\n", "\r\n", "界", "👩\u{200d}💻", "!", "\u{2028}"]), 0..100),
            raw in any::<usize>(), count in 1usize..20,
        ) {
            let text = Rope::from_str(&parts.concat());
            let position = CharOffset(raw % (text.len_chars() + 1));
            for around in [false, true] {
                for selection in [word(&text, position, around, false, &|| false).unwrap(),
                    word(&text, position, around, true, &|| false).unwrap(),
                    paragraph(&text, position, around, count, &|| false).unwrap()] {
                    prop_assert!(selection.anchor <= selection.head);
                    prop_assert!(selection.head.0 <= text.len_chars());
                    prop_assert!(grapheme::is_boundary(&text, selection.anchor).unwrap());
                    prop_assert!(grapheme::is_boundary(&text, selection.head).unwrap());
                }
            }
        }
    }
}
