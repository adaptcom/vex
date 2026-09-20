//! Delimiter identities shared by textobjects, surrounds, and syntax matching.

use crate::{CharOffset, RopeSlice, Selection};

/// Ordered scalar positions of the two delimiters, excluding their contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Delimiters {
    pub open: CharOffset,
    pub close: CharOffset,
}

/// Asymmetric pairs accepted by match mode, in either input direction.
pub const BRACKETS: &[(char, char)] = &[
    ('(', ')'),
    ('[', ']'),
    ('{', '}'),
    ('<', '>'),
    ('‘', '’'),
    ('“', '”'),
    ('«', '»'),
    ('「', '」'),
    ('（', '）'),
];

/// Resolve either side of a bracket to its opening/closing pair. Any other
/// character surrounds symmetrically, including quotes and arbitrary Unicode.
pub fn pair(character: char) -> (char, char) {
    BRACKETS
        .iter()
        .copied()
        .find(|&(open, close)| character == open || character == close)
        .unwrap_or((character, character))
}

/// Known syntax pairs include symmetric string/closure delimiters. Arbitrary
/// literal surround characters remain supported by [`pair`] and [`enclosing`].
pub fn is_pair(open: char, close: char) -> bool {
    BRACKETS.contains(&(open, close)) || (open == close && matches!(open, '\'' | '"' | '`' | '|'))
}

/// An outward scan visits rope chunks once, keeping only a nesting counter.
/// Reusing it for successive enclosing pairs avoids rescanning nested contents.
struct Scan<'a> {
    chars: ropey::iter::Chars<'a>,
    position: usize,
    backward: bool,
    target: char,
    nested: char,
    depth: usize,
}

impl<'a> Scan<'a> {
    fn new(text: RopeSlice<'a>, at: usize, backward: bool, target: char, nested: char) -> Self {
        Self {
            chars: text.chars_at(at),
            position: at,
            backward,
            target,
            nested,
            depth: 0,
        }
    }

    fn nth(&mut self, mut count: usize, cancelled: &impl Fn() -> bool) -> Option<CharOffset> {
        if count == 0 {
            return None;
        }
        loop {
            if cancelled() {
                return None;
            }
            let (position, ch) = if self.backward {
                let ch = self.chars.prev()?;
                self.position -= 1;
                (self.position, ch)
            } else {
                let ch = self.chars.next()?;
                let position = self.position;
                self.position += 1;
                (position, ch)
            };
            if ch == self.target {
                if self.depth != 0 {
                    self.depth -= 1;
                } else {
                    count -= 1;
                    if count == 0 {
                        return Some(CharOffset(position));
                    }
                }
            } else if ch == self.nested {
                self.depth += 1;
            }
        }
    }
}

/// Match the asymmetric bracket under the cursor. Plain text balances only the
/// requested bracket kind; quote/comment contents have no special interpretation.
/// Invalid positions, missing pairs, and cancellation return `None`.
pub fn matching(
    text: RopeSlice<'_>,
    position: CharOffset,
    cancelled: &impl Fn() -> bool,
) -> Option<CharOffset> {
    let character = text.get_char(position.0)?;
    let (open, close) = pair(character);
    if open == close {
        return None;
    }
    let backward = character == close;
    Scan::new(
        text,
        position.0 + usize::from(!backward),
        backward,
        if backward { open } else { close },
        character,
    )
    .nth(1, cancelled)
}

/// Find a specified surround at the cursor. Either bracket selects its pair;
/// counts seek successively outer delimiters on each side. A delimiter directly
/// under the cursor stays fixed, as in Helix. Symmetric delimiters directly under
/// the cursor are ambiguous without syntax and return `None`.
pub fn enclosing(
    text: RopeSlice<'_>,
    position: CharOffset,
    character: char,
    count: usize,
    cancelled: &impl Fn() -> bool,
) -> Option<Delimiters> {
    if position.0 > text.len_chars() || count == 0 || cancelled() {
        return None;
    }
    let (open, close) = pair(character);
    let under_cursor = text.get_char(position.0);
    if open == close && under_cursor == Some(open) {
        return None;
    }
    let left = if under_cursor == Some(open) {
        position
    } else {
        Scan::new(text, position.0, true, open, close).nth(count, cancelled)?
    };
    let right = if under_cursor == Some(close) {
        position
    } else {
        Scan::new(
            text,
            (position.0 + usize::from(under_cursor.is_some())).min(text.len_chars()),
            false,
            close,
            open,
        )
        .nth(count, cancelled)?
    };
    (left < right).then_some(Delimiters {
        open: left,
        close: right,
    })
}

/// Find the counted enclosing bracket pair around the whole selection. Already
/// selected opening delimiters are stepped over, so repeating `mam` grows out.
/// Quotes need syntax to distinguish opening and closing roles.
///
/// The forward traversal and each bracket kind's reverse traversal never revisit
/// text. Counts therefore add no repeated whole-document scans. Cancellation is
/// checked for every scalar; no document text is copied.
pub fn closest(
    text: RopeSlice<'_>,
    selection: Selection,
    mut count: usize,
    cancelled: &impl Fn() -> bool,
) -> Option<Delimiters> {
    if selection.end().0 > text.len_chars() || count == 0 {
        return None;
    }
    let mut depths = [0usize; BRACKETS.len()];
    let mut left: [Option<Scan<'_>>; BRACKETS.len()] = std::array::from_fn(|_| None);
    for (distance, ch) in text.chars_at(selection.start().0).enumerate() {
        if cancelled() {
            return None;
        }
        let Some(kind) = BRACKETS
            .iter()
            .position(|&(open, close)| ch == open || ch == close)
        else {
            continue;
        };
        let (open, _) = BRACKETS[kind];
        if ch == open {
            depths[kind] += 1;
            continue;
        }
        if depths[kind] != 0 {
            depths[kind] -= 1;
            continue;
        }
        let close = CharOffset(selection.start().0 + distance);
        let scan = left[kind].get_or_insert_with(|| {
            Scan::new(text, selection.start().0, true, open, BRACKETS[kind].1)
        });
        let Some(open) = scan.nth(1, cancelled) else {
            continue;
        };
        if close.0 < selection.end().0.saturating_sub(1) {
            continue;
        }
        count -= 1;
        if count == 0 {
            return Some(Delimiters { open, close });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Rope;
    use std::cell::Cell;

    fn positions(open: usize, close: usize) -> Option<Delimiters> {
        Some(Delimiters {
            open: CharOffset(open),
            close: CharOffset(close),
        })
    }

    #[test]
    fn explicit_pairs_balance_counts_and_keep_a_delimiter_under_the_cursor_fixed() {
        let text = Rope::from_str("(a(b)c)");
        for ch in ['(', ')'] {
            for (at, count, expected) in [
                (3, 1, positions(2, 4)),
                (3, 2, positions(0, 6)),
                (2, 2, positions(2, 6)),
                (4, 2, positions(0, 4)),
                (3, usize::MAX, None),
                (7, 1, None),
                (8, 1, None),
            ] {
                assert_eq!(
                    enclosing(text.slice(..), CharOffset(at), ch, count, &|| false),
                    expected
                );
            }
        }
        let text = Rope::from_str("«界» \"word\"");
        assert_eq!(
            enclosing(text.slice(..), CharOffset(1), '»', 1, &|| false),
            positions(0, 2)
        );
        assert_eq!(
            enclosing(text.slice(..), CharOffset(5), '"', 1, &|| false),
            positions(4, 9)
        );
        assert_eq!(
            enclosing(text.slice(..), CharOffset(4), '"', 1, &|| false),
            None
        );
    }

    #[test]
    fn matching_round_trips_nested_unicode_brackets_and_ignores_plain_text_quotes() {
        let text = Rope::from_str("(a(b)c)「界」 \"x\"");
        for (left, right) in [(0, 6), (2, 4), (7, 9)] {
            assert_eq!(
                matching(text.slice(..), CharOffset(left), &|| false),
                Some(CharOffset(right))
            );
            assert_eq!(
                matching(text.slice(..), CharOffset(right), &|| false),
                Some(CharOffset(left))
            );
        }
        for at in [1, 10, 11, text.len_chars(), usize::MAX] {
            assert_eq!(matching(text.slice(..), CharOffset(at), &|| false), None);
        }
    }

    #[test]
    fn closest_encloses_whole_selections_and_repeated_around_objects_grow_outward() {
        let text = Rope::from_str("{[x] y}");
        for (start, end, count, expected) in [
            (2, 3, 1, positions(1, 3)),
            (2, 3, 2, positions(0, 6)),
            (1, 4, 1, positions(0, 6)),
            (2, 6, 1, positions(0, 6)),
            (0, 7, 1, None),
            (2, 3, usize::MAX, None),
            (1, 8, 1, None),
        ] {
            for range in [
                Selection::new(CharOffset(start), CharOffset(end)),
                Selection::new(CharOffset(end), CharOffset(start)),
            ] {
                assert_eq!(closest(text.slice(..), range, count, &|| false), expected);
            }
        }
        let text = Rope::from_str("(a)(b)");
        assert_eq!(
            closest(
                text.slice(..),
                Selection::new(CharOffset(1), CharOffset(5)),
                1,
                &|| false
            ),
            None
        );
    }

    #[test]
    fn scans_cancel_per_scalar_and_nested_counts_do_not_rescan_text() {
        let source = format!("({})", "x".repeat(1 << 20));
        let text = Rope::from_str(&source);
        let calls = Cell::new(0);
        let cancelled = || {
            calls.set(calls.get() + 1);
            calls.get() >= 100
        };
        assert_eq!(matching(text.slice(..), CharOffset(0), &cancelled), None);
        assert_eq!(calls.get(), 100);
        calls.set(0);
        assert_eq!(
            enclosing(
                text.slice(..),
                CharOffset(text.len_chars() / 2),
                '(',
                1,
                &cancelled
            ),
            None
        );
        assert_eq!(calls.get(), 100);
        calls.set(0);
        assert_eq!(
            closest(
                text.slice(..),
                Selection::cursor(CharOffset(1)),
                1,
                &cancelled
            ),
            None
        );
        assert_eq!(calls.get(), 100);
        let depth = 4096;
        let text = Rope::from_str(&format!("{}x{}", "(".repeat(depth), ")".repeat(depth)));
        calls.set(0);
        assert_eq!(
            closest(
                text.slice(..),
                Selection::new(CharOffset(depth), CharOffset(depth + 1)),
                depth,
                &|| {
                    calls.set(calls.get() + 1);
                    false
                }
            ),
            positions(0, depth * 2)
        );
        assert!(calls.get() <= depth * 2 + 1, "{} visits", calls.get());
    }

    #[test]
    fn either_side_of_a_bracket_and_literal_characters_resolve_consistently() {
        for &(open, close) in BRACKETS {
            assert_eq!(pair(open), (open, close));
            assert_eq!(pair(close), (open, close));
        }
        for ch in ['"', '\'', '`', '|', 'm', '界', ' '] {
            assert_eq!(pair(ch), (ch, ch));
        }
    }
}
