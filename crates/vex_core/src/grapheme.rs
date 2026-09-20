//! Extended grapheme boundaries over rope chunks, without flattening the text.

use std::borrow::Cow;
use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete};

use crate::{CharOffset, Error, Rope};

pub(crate) struct Cursor<'a> {
    text: &'a Rope,
    cursor: GraphemeCursor,
    chunk: &'a str,
    start: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(text: &'a Rope, position: CharOffset) -> Result<Self, Error> {
        let byte = text
            .try_char_to_byte(position.0)
            .map_err(|_| Error::PositionOutOfBounds {
                position,
                len: text.len_chars(),
            })?;
        let (chunk, start, _, _) = text.chunk_at_byte(byte);
        Ok(Self {
            text,
            cursor: GraphemeCursor::new(byte, text.len_bytes(), true),
            chunk,
            start,
        })
    }

    #[inline]
    fn query<T>(
        &mut self,
        operation: impl Fn(&mut GraphemeCursor, &str, usize) -> Result<T, GraphemeIncomplete>,
    ) -> T {
        match operation(&mut self.cursor, self.chunk, self.start) {
            Ok(value) => value,
            Err(request) => self.resume(operation, request),
        }
    }

    #[cold]
    fn resume<T>(
        &mut self,
        operation: impl Fn(&mut GraphemeCursor, &str, usize) -> Result<T, GraphemeIncomplete>,
        mut request: GraphemeIncomplete,
    ) -> T {
        // A tiny overlapping chunk keeps the current boundary inside a chunk
        // when resuming a forward scan. unicode-segmentation 1.13.3 otherwise
        // recounts regional indicators at chunk start even when its running
        // count is already known (upstream fix: unicode-segmentation#175).
        // Preserve that running state: restarting the cursor would repeatedly
        // scan the prefix of a long flag run. No allocation or flattening needed.
        let mut bridge = [0u8; 8];
        let mut overlap = None;
        loop {
            match request {
                GraphemeIncomplete::PreContext(end) => {
                    let (chunk, start, _, _) = self.text.chunk_at_byte(end - 1);
                    self.cursor.provide_context(&chunk[..end - start], start);
                }
                GraphemeIncomplete::PrevChunk | GraphemeIncomplete::NextChunk => {
                    let byte = match request {
                        GraphemeIncomplete::PrevChunk => self.start - 1,
                        _ => self.cursor.cur_cursor(),
                    };
                    let (chunk, start, _, _) = self.text.chunk_at_byte(byte);
                    self.chunk = chunk;
                    self.start = start;
                    overlap = None;
                    if request == GraphemeIncomplete::NextChunk && byte == start {
                        let (previous, _, _, _) = self.text.chunk_at_byte(byte - 1);
                        let left = previous.chars().next_back().unwrap();
                        let right = chunk.chars().next().unwrap();
                        left.encode_utf8(&mut bridge);
                        right.encode_utf8(&mut bridge[left.len_utf8()..]);
                        overlap =
                            Some((left.len_utf8() + right.len_utf8(), byte - left.len_utf8()));
                    }
                }
                GraphemeIncomplete::InvalidOffset => {
                    unreachable!("cursor and chunks describe the same rope")
                }
            }
            let (chunk, start) = match overlap {
                Some((len, start)) => (std::str::from_utf8(&bridge[..len]).unwrap(), start),
                None => (self.chunk, self.start),
            };
            match operation(&mut self.cursor, chunk, start) {
                Ok(value) => return value,
                Err(next) => request = next,
            }
        }
    }

    pub fn is_boundary(&mut self) -> bool {
        self.query(GraphemeCursor::is_boundary)
    }

    pub fn next(&mut self) -> Option<CharOffset> {
        self.query(GraphemeCursor::next_boundary)
            .map(|byte| CharOffset(self.text.byte_to_char(byte)))
    }

    pub fn previous(&mut self) -> Option<CharOffset> {
        self.query(GraphemeCursor::prev_boundary)
            .map(|byte| CharOffset(self.text.byte_to_char(byte)))
    }

    /// Borrow the next cluster from the current rope chunk when possible. The
    /// caller already knows its scalar start, so it can count this short slice
    /// instead of performing another root-to-leaf coordinate lookup per glyph.
    pub fn next_grapheme(&mut self) -> Option<Cow<'a, str>> {
        let byte = self.cursor.cur_cursor();
        let chunk = self.chunk;
        let start = self.start;
        let end = self.query(GraphemeCursor::next_boundary)?;
        Some(if end <= start + chunk.len() {
            Cow::Borrowed(&chunk[byte - start..end - start])
        } else {
            Cow::Owned(self.text.byte_slice(byte..end).to_string())
        })
    }

    /// Printable ASCII bytes available in the currently borrowed chunk.
    pub fn ascii_prefix(&self) -> usize {
        self.chunk.as_bytes()[self.cursor.cur_cursor() - self.start..]
            .iter()
            .take_while(|&&b| (b' '..=b'~').contains(&b))
            .count()
    }

    /// Skip complete ASCII graphemes, retaining the run's final character in case
    /// it joins Unicode in the next cluster/chunk. Only used by layout scanning.
    pub fn advance_ascii(&mut self, count: usize) {
        debug_assert!(count > 0 && count < self.ascii_prefix());
        self.cursor = GraphemeCursor::new(
            self.cursor.cur_cursor() + count,
            self.text.len_bytes(),
            true,
        );
    }
}

/// Whether a scalar offset is an extended grapheme boundary. BOF and EOF are boundaries.
pub fn is_boundary(text: &Rope, position: CharOffset) -> Result<bool, Error> {
    Ok(Cursor::new(text, position)?.is_boundary())
}

/// Move forward by up to `count` grapheme boundaries, stopping at EOF.
/// Starting inside a cluster moves to its end on the first step.
pub fn next(text: &Rope, position: CharOffset, count: usize) -> Result<CharOffset, Error> {
    let mut cursor = Cursor::new(text, position)?;
    let mut result = position;
    for _ in 0..count {
        let Some(next) = cursor.next() else { break };
        result = next;
    }
    Ok(result)
}

/// Count clusters intersecting a scalar range without flattening the rope.
/// Partial clusters at either edge count once; an empty range counts zero.
pub fn count(text: &Rope, range: std::ops::Range<CharOffset>) -> Result<usize, Error> {
    if range.start > range.end {
        return Err(Error::ReversedRange {
            start: range.start,
            end: range.end,
        });
    }
    if range.end.0 > text.len_chars() {
        return Err(Error::PositionOutOfBounds {
            position: range.end,
            len: text.len_chars(),
        });
    }
    let mut cursor = Cursor::new(text, range.start)?;
    let end = text.char_to_byte(range.end.0);
    let mut count = 0;
    while cursor.cursor.cur_cursor() < end {
        cursor
            .query(GraphemeCursor::next_boundary)
            .expect("range ends within the rope");
        count += 1;
    }
    Ok(count)
}

/// Move backward by up to `count` grapheme boundaries, stopping at BOF.
/// Starting inside a cluster moves to its start on the first step.
pub fn previous(text: &Rope, position: CharOffset, count: usize) -> Result<CharOffset, Error> {
    let mut cursor = Cursor::new(text, position)?;
    let mut result = position;
    for _ in 0..count {
        let Some(previous) = cursor.previous() else {
            break;
        };
        result = previous;
    }
    Ok(result)
}

/// Round a scalar offset down to the containing grapheme's start.
pub fn floor(text: &Rope, position: CharOffset) -> Result<CharOffset, Error> {
    let mut cursor = Cursor::new(text, position)?;
    Ok(if cursor.is_boundary() {
        position
    } else {
        cursor.previous().unwrap_or(CharOffset(0))
    })
}

/// Round a scalar offset up to the containing grapheme's end.
pub fn ceil(text: &Rope, position: CharOffset) -> Result<CharOffset, Error> {
    let mut cursor = Cursor::new(text, position)?;
    Ok(if cursor.is_boundary() {
        position
    } else {
        cursor.next().unwrap_or(CharOffset(text.len_chars()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use unicode_segmentation::UnicodeSegmentation;

    fn check_against_flat_text(input: &str) {
        let text = Rope::from_str(input);
        let mut boundaries = vec![0];
        for grapheme in input.graphemes(true) {
            boundaries.push(boundaries.last().unwrap() + grapheme.chars().count());
        }
        assert_eq!(
            count(&text, CharOffset(0)..CharOffset(text.len_chars())).unwrap(),
            boundaries.len() - 1
        );
        for position in 0..=text.len_chars() {
            let before = boundaries
                .iter()
                .copied()
                .rfind(|b| *b < position)
                .unwrap_or(0);
            let after = boundaries
                .iter()
                .copied()
                .find(|b| *b > position)
                .unwrap_or(text.len_chars());
            let boundary = boundaries.contains(&position);
            assert_eq!(
                is_boundary(&text, CharOffset(position)).unwrap(),
                boundary,
                "at {position}"
            );
            assert_eq!(previous(&text, CharOffset(position), 1).unwrap().0, before);
            assert_eq!(next(&text, CharOffset(position), 1).unwrap().0, after);
            assert_eq!(
                floor(&text, CharOffset(position)).unwrap().0,
                if boundary { position } else { before }
            );
            assert_eq!(
                ceil(&text, CharOffset(position)).unwrap().0,
                if boundary { position } else { after }
            );
        }
    }

    #[test]
    fn clusters_cross_rope_chunks_in_both_directions() {
        for input in [
            "a\r\ne\u{301}👩\u{200d}💻🇺🇸🇨🇦क्\u{200d}ष".repeat(90),
            format!("a{}z", "\u{301}".repeat(1_500)),
            "🇺🇸🇨🇦🇯🇵".repeat(150),
        ] {
            assert!(Rope::from_str(&input).chunks().count() > 1);
            check_against_flat_text(&input);
        }
    }

    #[test]
    fn regional_indicator_runs_keep_their_parity_across_chunk_boundaries() {
        for padding in 0..16 {
            let input = format!("{}{}z", "a".repeat(padding), "🇺".repeat(400));
            check_against_flat_text(&input);
        }
    }

    #[test]
    fn limits_empty_text_and_invalid_positions() {
        let text = Rope::from_str("e\u{301}🦀");
        assert_eq!(
            next(&text, CharOffset(0), usize::MAX).unwrap(),
            CharOffset(3)
        );
        assert_eq!(
            previous(&text, CharOffset(3), usize::MAX).unwrap(),
            CharOffset(0)
        );
        assert_eq!(next(&text, CharOffset(1), 0).unwrap(), CharOffset(1));
        assert!(next(&text, CharOffset(4), 0).is_err());
        assert_eq!(count(&text, CharOffset(1)..CharOffset(2)).unwrap(), 1);
        assert_eq!(count(&text, CharOffset(1)..CharOffset(1)).unwrap(), 0);
        assert!(count(&text, CharOffset(3)..CharOffset(2)).is_err());
        assert!(count(&text, CharOffset(0)..CharOffset(4)).is_err());
        check_against_flat_text("");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]
        #[test]
        fn chunked_boundaries_match_unicode_segmentation(
            parts in prop::collection::vec(prop_oneof![
                Just("a"), Just("\r\n"), Just("e\u{301}"), Just("👩\u{200d}💻"),
                Just("🇺"), Just("🇸"), Just("\u{301}"), Just("क्ष"), Just("界"),
            ], 0..250),
        ) {
            check_against_flat_text(&parts.concat());
        }
    }
}
