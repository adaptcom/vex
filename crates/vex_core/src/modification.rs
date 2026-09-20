//! Resolve the last modification from text-free undo metadata. Composition is
//! cancellable and can run on a worker; capturing this descriptor is O(1).

use crate::{CharOffset, Selection, mapping::PositionMaps};
use std::ops::Range;

#[derive(Clone, Debug)]
pub struct Modification {
    pub(crate) original_len: usize,
    pub(crate) primary: Selection,
    pub(crate) maps: PositionMaps,
}

#[derive(Clone, Copy)]
struct Piece {
    original: Option<usize>,
    len: usize,
}

fn append(pieces: &mut Vec<Piece>, piece: Piece) {
    if piece.len == 0 {
        return;
    }
    if let Some(last) = pieces.last_mut()
        && match (last.original, piece.original) {
            (None, None) => true,
            (Some(start), Some(next)) => start + last.len == next,
            _ => false,
        }
    {
        last.len += piece.len;
    } else {
        pieces.push(piece);
    }
}

struct Pieces<'a> {
    pieces: &'a [Piece],
    index: usize,
    consumed: usize,
    position: usize,
}

impl Pieces<'_> {
    fn advance(
        &mut self,
        to: usize,
        mut output: Option<&mut Vec<Piece>>,
        cancelled: &impl Fn() -> bool,
    ) -> bool {
        while self.position < to {
            if cancelled() {
                return false;
            }
            let Some(piece) = self.pieces.get(self.index) else {
                break;
            };
            let count = (to - self.position).min(piece.len - self.consumed);
            if let Some(output) = output.as_deref_mut() {
                append(
                    output,
                    Piece {
                        original: piece.original.map(|start| start + self.consumed),
                        len: count,
                    },
                );
            }
            self.position += count;
            self.consumed += count;
            if self.consumed == piece.len {
                self.index += 1;
                self.consumed = 0;
            }
        }
        true
    }
}

impl Modification {
    /// End of the composed change overlapping the original primary selection,
    /// or the first change when none overlap. Returns None on cancellation or
    /// when the group's insertions were completely removed again.
    pub fn position(&self, cancelled: impl Fn() -> bool) -> Option<CharOffset> {
        if cancelled() {
            return None;
        }
        let overlaps = |range: &Range<CharOffset>| {
            range.start == self.primary.start()
                || (range.end > self.primary.start() && self.primary.end() > range.start)
        };
        let maps = self.maps.as_slice();
        if maps.len() == 1 && maps[0].changes().len() == 1 {
            return maps[0].changes().next().map(|(_, new)| new.end);
        }
        // Track original spans and inserted lengths, never source bytes. A
        // normal typing group has one merged map and takes the fast path above.
        let mut pieces = vec![Piece {
            original: Some(0),
            len: self.original_len,
        }];
        let mut output = Vec::new();
        for map in maps {
            if cancelled() {
                return None;
            }
            output.clear();
            let mut input = Pieces {
                pieces: &pieces,
                index: 0,
                consumed: 0,
                position: 0,
            };
            for (old, new) in map.changes() {
                if !input.advance(old.start.0, Some(&mut output), &cancelled)
                    || !input.advance(old.end.0, None, &cancelled)
                    || cancelled()
                {
                    return None;
                }
                append(
                    &mut output,
                    Piece {
                        original: None,
                        len: new.end.0 - new.start.0,
                    },
                );
            }
            if !input.advance(usize::MAX, Some(&mut output), &cancelled) {
                return None;
            }
            std::mem::swap(&mut pieces, &mut output);
        }
        let mut first = None;
        let mut old = 0;
        let mut new = 0;
        let mut inserted = false;
        for piece in pieces.iter().chain(std::iter::once(&Piece {
            original: Some(self.original_len),
            len: 0,
        })) {
            if cancelled() {
                return None;
            }
            if let Some(start) = piece.original {
                if start > old || inserted {
                    let end = CharOffset(new);
                    first.get_or_insert(end);
                    if overlaps(&(CharOffset(old)..CharOffset(start))) {
                        return Some(end);
                    }
                }
                inserted = false;
                old = start + piece.len;
            } else {
                inserted = true;
            }
            new += piece.len;
        }
        first
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Document, Edit, SelectionSet};
    use proptest::prelude::*;

    fn apply(document: &mut Document, selections: &mut SelectionSet, edits: Vec<Edit>) {
        let transaction = document.transaction(edits).unwrap();
        document.apply_grouped(transaction, selections).unwrap();
    }
    fn replace(from: usize, to: usize, text: &str) -> Edit {
        Edit::new(CharOffset(from)..CharOffset(to), text)
    }
    fn position(document: &Document) -> Option<CharOffset> {
        document
            .last_modification()
            .and_then(|change| change.position(|| false))
    }

    #[test]
    fn primary_change_wins_over_additional_edits_and_adjacent_changes_compose() {
        let mut document = Document::from("0123456789");
        let mut selections = SelectionSet::single(Selection::new(CharOffset(7), CharOffset(8)));
        apply(
            &mut document,
            &mut selections,
            vec![replace(1, 1, "A"), replace(7, 9, "BC")],
        );
        assert_eq!(position(&document), Some(CharOffset(10)));
        let mut document = Document::from("abc");
        let mut selections = SelectionSet::single(Selection::new(CharOffset(0), CharOffset(1)));
        apply(
            &mut document,
            &mut selections,
            vec![replace(0, 1, "XX"), replace(1, 2, "Y")],
        );
        assert_eq!(position(&document), Some(CharOffset(3)));
    }

    #[test]
    fn groups_choose_composed_changes_and_captured_metadata_survives_later_edits_and_history() {
        let mut document = Document::from("0123456789");
        let mut selections = SelectionSet::single(Selection::new(CharOffset(9), CharOffset(10)));
        apply(&mut document, &mut selections, vec![replace(5, 6, "XYZ")]);
        let captured = document.last_modification().unwrap();
        assert_eq!(captured.position(|| false), Some(CharOffset(8)));
        apply(&mut document, &mut selections, vec![replace(0, 0, "AA")]);
        assert_eq!(position(&document), Some(CharOffset(2))); // Earlier fallback change.
        apply(&mut document, &mut selections, vec![replace(13, 14, "!")]);
        assert_eq!(position(&document), Some(CharOffset(14))); // Original primary wins.
        assert_eq!(captured.position(|| false), Some(CharOffset(8)));
        document.undo(&mut selections).unwrap();
        assert_eq!(position(&document), None);
        document.redo(&mut selections).unwrap();
        assert_eq!(position(&document), Some(CharOffset(14)));
        document.set_history_limit(0);
        assert_eq!(position(&document), None);
    }

    #[test]
    fn empty_files_net_empty_groups_and_cancellation_do_not_invent_a_destination() {
        let mut document = Document::default();
        let mut selections = SelectionSet::single(Selection::cursor(CharOffset(0)));
        apply(&mut document, &mut selections, vec![replace(0, 0, "a")]);
        apply(&mut document, &mut selections, vec![replace(1, 1, "b")]);
        let captured = document.last_modification().unwrap();
        assert_eq!(captured.maps.as_slice().len(), 1); // Ordinary typing stays compact.
        assert_eq!(captured.position(|| false), Some(CharOffset(2)));
        apply(&mut document, &mut selections, vec![replace(0, 2, "")]);
        let change = document.last_modification().unwrap();
        assert_eq!(change.position(|| false), None);
        let polls = std::cell::Cell::new(0);
        assert_eq!(
            change.position(|| {
                polls.set(polls.get() + 1);
                polls.get() >= 3
            }),
            None
        );
        assert_eq!(polls.get(), 3);
        assert_eq!(change.position(|| false), None);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(96))]
        #[test]
        fn composed_metadata_agrees_with_flat_provenance(
            primary in 0usize..24,
            edits in prop::collection::vec((0usize..80, 0usize..8, 0usize..5), 0..48),
        ) {
            let original: String = (0..24).map(|n| char::from_u32(0x4e00 + n).unwrap()).collect();
            let mut document = Document::from(original.as_str());
            let mut selections = SelectionSet::single(Selection::new(CharOffset(primary), CharOffset(primary + 1)));
            for (start, length, inserted) in edits {
                let start = start % (document.text().len_chars() + 1);
                let end = (start + length).min(document.text().len_chars());
                apply(&mut document, &mut selections, vec![replace(start, end, &"x".repeat(inserted))]);
            }
            // Original characters are unique; inserted text is ASCII. Infer
            // gaps independently from the final flat character sequence.
            let retained: Vec<_> = std::iter::once((-1isize, -1isize))
                .chain(document.text().chars().enumerate().filter_map(|(at, ch)| {
                    (ch != 'x').then_some((ch as isize - 0x4e00, at as isize))
                }))
                .chain(std::iter::once((24, document.text().len_chars() as isize)))
                .collect();
            let changes: Vec<_> = retained.windows(2).filter_map(|pair| {
                let [(before, old_at), (after, new_at)] = pair else { unreachable!() };
                (after - before > 1 || new_at - old_at > 1).then_some(((before + 1) as usize..*after as usize, CharOffset(*new_at as usize)))
            }).collect();
            let expected = changes.iter().find(|(range, _)| range.start == primary || (range.end > primary && primary + 1 > range.start))
                .or_else(|| changes.first()).map(|(_, end)| *end);
            prop_assert_eq!(position(&document), expected);
        }
    }
}
