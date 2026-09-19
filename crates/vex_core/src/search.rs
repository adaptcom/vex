//! Streaming, case-sensitive literal search over rope chunks. Only the pattern
//! is copied. Matches may overlap and always have valid UTF-8 endpoints.

use std::ops::Range;

use crate::{ByteOffset, Rope};

/// Order in which candidate match positions are visited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
}

impl Direction {
    pub fn reversed(self) -> Self {
        match self {
            Self::Forward => Self::Backward,
            Self::Backward => Self::Forward,
        }
    }
}

/// A compiled UTF-8 literal with forward and reverse KMP failure tables.
/// Construction and storage are O(pattern bytes); scanning is O(bytes visited).
#[derive(Debug)]
pub struct Literal {
    text: String,
    backward: Vec<u8>,
    forward_table: Vec<usize>,
    backward_table: Vec<usize>,
}

impl Literal {
    pub fn new(text: &str) -> Self {
        let backward: Vec<_> = text.bytes().rev().collect();
        Self {
            text: text.into(),
            forward_table: failure_table(text.as_bytes()),
            backward_table: failure_table(&backward),
            backward,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Iterate overlapping matches whose starting byte is in `starts`, in the
    /// requested direction. Bounds are clamped to the document; reversed ranges
    /// and empty patterns yield no matches. A match can end beyond `starts.end`.
    /// Seeking is logarithmic; iteration stops as soon as the caller has enough.
    pub fn matches<'a>(
        &'a self,
        text: &'a Rope,
        starts: Range<ByteOffset>,
        direction: Direction,
    ) -> impl Iterator<Item = Range<ByteOffset>> + 'a {
        let start = starts.start.0.min(text.len_bytes());
        let end = starts.end.0.min(text.len_bytes()).max(start);
        let limit = end
            .saturating_add(self.text.len().saturating_sub(1))
            .min(text.len_bytes());
        let (pattern, table, bytes) = match direction {
            Direction::Forward => (
                self.text.as_bytes(),
                &self.forward_table,
                text.bytes_at(start),
            ),
            Direction::Backward => (
                self.backward.as_slice(),
                &self.backward_table,
                text.bytes_at(limit).reversed(),
            ),
        };
        let length = if pattern.is_empty() || start == end {
            0
        } else {
            limit - start
        };
        let mut bytes = bytes.take(length).enumerate();
        let mut matched = 0;
        std::iter::from_fn(move || {
            for (index, byte) in bytes.by_ref() {
                while matched > 0 && pattern[matched] != byte {
                    matched = table[matched - 1];
                }
                if pattern[matched] == byte {
                    matched += 1;
                }
                if matched == pattern.len() {
                    matched = table[matched - 1];
                    let position = match direction {
                        Direction::Forward => start + index + 1 - pattern.len(),
                        Direction::Backward => limit - index - 1,
                    };
                    if position < end {
                        return Some(ByteOffset(position)..ByteOffset(position + pattern.len()));
                    }
                }
            }
            None
        })
    }
}

fn failure_table(pattern: &[u8]) -> Vec<usize> {
    let mut table = vec![0; pattern.len()];
    let mut prefix = 0;
    for index in 1..pattern.len() {
        while prefix > 0 && pattern[index] != pattern[prefix] {
            prefix = table[prefix - 1];
        }
        if pattern[index] == pattern[prefix] {
            prefix += 1;
        }
        table[index] = prefix;
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn starts(text: &Rope, query: &str, direction: Direction) -> Vec<usize> {
        Literal::new(query)
            .matches(text, ByteOffset(0)..ByteOffset(text.len_bytes()), direction)
            .map(|m| m.start.0)
            .collect()
    }

    #[test]
    fn overlaps_unicode_line_endings_empty_patterns_and_bounds() {
        let text = Rope::from_str("ababa界\r\n界");
        assert_eq!(starts(&text, "aba", Direction::Forward), [0, 2]);
        assert_eq!(starts(&text, "aba", Direction::Backward), [2, 0]);
        assert_eq!(starts(&text, "界", Direction::Backward), [10, 5]);
        assert_eq!(starts(&text, "界\r\n", Direction::Forward), [5]);
        assert!(starts(&text, "", Direction::Forward).is_empty());
        assert!(starts(&Rope::new(), "a", Direction::Backward).is_empty());
        for direction in [Direction::Forward, Direction::Backward] {
            let pattern = Literal::new("aba");
            let found: Vec<_> = pattern
                .matches(&text, ByteOffset(1)..ByteOffset(3), direction)
                .collect();
            assert_eq!(found, [ByteOffset(2)..ByteOffset(5)]);
            assert!(
                pattern
                    .matches(&text, ByteOffset(99)..ByteOffset(0), direction)
                    .next()
                    .is_none()
            );
        }
    }

    #[test]
    fn matches_span_real_rope_chunks_in_both_directions() {
        let text = Rope::from_str(&"ab界\r\n".repeat(2_000));
        let boundary = text.chunks().next().unwrap().len();
        let start = text.byte_to_char(boundary) - 3;
        let end = start + 9;
        let query = text.slice(start..end).to_string();
        let expected = ByteOffset(text.char_to_byte(start))..ByteOffset(text.char_to_byte(end));
        assert!(expected.start.0 < boundary && expected.end.0 > boundary);
        for direction in [Direction::Forward, Direction::Backward] {
            assert_eq!(
                Literal::new(&query)
                    .matches(
                        &text,
                        expected.start..ByteOffset(expected.start.0 + 1),
                        direction
                    )
                    .next(),
                Some(expected.clone())
            );
        }
        // The pattern itself can be much larger than a rope chunk.
        assert_eq!(starts(&text, &text.to_string(), Direction::Backward), [0]);
    }

    proptest! {
        #[test]
        fn streaming_matches_agree_with_flat_byte_windows(
            source in prop::collection::vec(prop::sample::select(vec!['a', 'b', '界', '🦀', '\u{301}', '\r', '\n']), 0..180),
            query in prop::collection::vec(prop::sample::select(vec!['a', 'b', '界', '🦀', '\u{301}', '\r', '\n']), 0..8),
            a in 0usize..600, b in 0usize..600,
        ) {
            let source: String = source.into_iter().collect();
            let query: String = query.into_iter().collect();
            let rope = Rope::from_str(&source);
            let pattern = Literal::new(&query);
            let range = a.min(b)..a.max(b);
            let mut expected: Vec<_> = if query.is_empty() { vec![] } else {
                source.as_bytes().windows(query.len()).enumerate()
                    .filter(|(i, bytes)| range.contains(i) && *bytes == query.as_bytes())
                    .map(|(i, _)| ByteOffset(i)..ByteOffset(i + query.len())).collect()
            };
            for direction in [Direction::Forward, Direction::Backward] {
                let actual: Vec<_> = pattern.matches(&rope, ByteOffset(range.start)..ByteOffset(range.end), direction).collect();
                prop_assert_eq!(&actual, &expected);
                expected.reverse();
            }
        }
    }
}
