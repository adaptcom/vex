//! Extended grapheme boundaries over rope chunks, without flattening the text.

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

    fn query<T>(
        &mut self,
        operation: impl Fn(&mut GraphemeCursor, &str, usize) -> Result<T, GraphemeIncomplete>,
    ) -> T {
        loop {
            match operation(&mut self.cursor, self.chunk, self.start) {
                Ok(value) => return value,
                Err(GraphemeIncomplete::PreContext(end)) => {
                    let (chunk, start, _, _) = self.text.chunk_at_byte(end - 1);
                    self.cursor.provide_context(&chunk[..end - start], start);
                }
                Err(request @ (GraphemeIncomplete::PrevChunk | GraphemeIncomplete::NextChunk)) => {
                    let byte = match request {
                        GraphemeIncomplete::PrevChunk => self.start - 1,
                        _ => self.start + self.chunk.len(),
                    };
                    let (chunk, start, _, _) = self.text.chunk_at_byte(byte);
                    self.chunk = chunk;
                    self.start = start;
                }
                Err(GraphemeIncomplete::InvalidOffset) => {
                    unreachable!("cursor and chunks describe the same rope")
                }
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
