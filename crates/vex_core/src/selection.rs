use std::ops::Range;

use crate::{CharOffset, Error};

/// A directional, half-open selection. Equal endpoints represent a caret.
///
/// A normal-mode block cursor will be a one-grapheme selection, while an
/// insert-mode caret can have zero width. Backward selections keep their head
/// before their anchor without reversing the underlying edit range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: CharOffset,
    pub head: CharOffset,
}

impl Selection {
    pub const fn new(anchor: CharOffset, head: CharOffset) -> Self {
        Self { anchor, head }
    }

    pub const fn cursor(position: CharOffset) -> Self {
        Self::new(position, position)
    }

    pub fn start(self) -> CharOffset {
        self.anchor.min(self.head)
    }

    pub fn end(self) -> CharOffset {
        self.anchor.max(self.head)
    }

    pub fn range(self) -> Range<CharOffset> {
        self.start()..self.end()
    }

    pub fn is_empty(self) -> bool {
        self.anchor == self.head
    }

    pub fn is_backward(self) -> bool {
        self.head < self.anchor
    }
}

/// A nonempty, sorted set with overlapping selections and duplicate carets merged.
///
/// Adjacent nonempty ranges stay separate. A caret at a range's start or inside
/// it merges with that range; a caret at its end stays separate. A merge keeps
/// the primary selection's direction when present, otherwise the first sorted
/// selection's direction. The primary follows its selection through sorting.
/// Bounds are checked against a document when using the set with that document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectionSet {
    ranges: Vec<Selection>,
    primary: usize,
}

impl SelectionSet {
    pub fn new(ranges: Vec<Selection>, primary: usize) -> Result<Self, Error> {
        if ranges.is_empty() {
            return Err(Error::EmptySelectionSet);
        }
        if primary >= ranges.len() {
            return Err(Error::InvalidPrimary {
                index: primary,
                count: ranges.len(),
            });
        }
        let mut tagged: Vec<_> = ranges.into_iter().enumerate().collect();
        tagged.sort_unstable_by_key(|(index, range)| (range.start(), range.end(), *index));
        let mut merged: Vec<Selection> = Vec::with_capacity(tagged.len());
        let mut primary_index = 0;
        for (index, range) in tagged {
            if let Some(last_index) = merged.len().checked_sub(1) {
                let last = merged[last_index];
                if range.start() < last.end() || range.start() == last.start() {
                    let backward = if index == primary {
                        range.is_backward()
                    } else {
                        last.is_backward()
                    };
                    let start = last.start();
                    let end = last.end().max(range.end());
                    merged[last_index] = if backward {
                        Selection::new(end, start)
                    } else {
                        Selection::new(start, end)
                    };
                    if index == primary {
                        primary_index = last_index;
                    }
                    continue;
                }
            }
            if index == primary {
                primary_index = merged.len();
            }
            merged.push(range);
        }
        Ok(Self {
            ranges: merged,
            primary: primary_index,
        })
    }

    pub fn single(selection: Selection) -> Self {
        Self {
            ranges: vec![selection],
            primary: 0,
        }
    }

    pub fn ranges(&self) -> &[Selection] {
        &self.ranges
    }

    pub fn primary(&self) -> Selection {
        self.ranges[self.primary]
    }

    pub fn primary_index(&self) -> usize {
        self.primary
    }

    /// Check all endpoints against a document's scalar length, allowing EOF.
    pub fn validate(&self, len: usize) -> Result<(), Error> {
        // Sorted disjoint ranges guarantee that the final endpoint is maximal.
        let position = self.ranges.last().expect("nonempty selection set").end();
        if position.0 > len {
            return Err(Error::PositionOutOfBounds { position, len });
        }
        Ok(())
    }
}

impl Default for SelectionSet {
    fn default() -> Self {
        Self::single(Selection::cursor(CharOffset(0)))
    }
}

#[cfg(test)]
mod tests {
    use crate::{CharOffset, Error, Selection, SelectionSet};

    fn range(anchor: usize, head: usize) -> Selection {
        Selection::new(CharOffset(anchor), CharOffset(head))
    }

    #[test]
    fn backward_ranges_preserve_direction() {
        let selection = range(8, 2);
        assert_eq!(selection.range(), CharOffset(2)..CharOffset(8));
        assert!(selection.is_backward());
        assert!(!selection.is_empty());
    }

    #[test]
    fn normalization_sorts_and_tracks_primary() {
        let selections =
            SelectionSet::new(vec![range(10, 8), range(1, 3), range(5, 5)], 0).unwrap();
        assert_eq!(
            selections.ranges(),
            &[range(1, 3), range(5, 5), range(10, 8)]
        );
        assert_eq!(selections.primary_index(), 2);
        assert_eq!(selections.primary(), range(10, 8));
    }

    #[test]
    fn overlap_union_keeps_primary_direction() {
        let selections =
            SelectionSet::new(vec![range(2, 6), range(8, 4), range(7, 10)], 1).unwrap();
        assert_eq!(selections.ranges(), &[range(10, 2)]);
        assert_eq!(selections.primary_index(), 0);
    }

    #[test]
    fn merge_without_primary_keeps_first_direction() {
        let selections =
            SelectionSet::new(vec![range(5, 1), range(3, 7), range(9, 10)], 2).unwrap();
        assert_eq!(selections.ranges(), &[range(7, 1), range(9, 10)]);
        assert_eq!(selections.primary_index(), 1);
    }

    #[test]
    fn duplicate_carets_and_contained_carets_merge() {
        let selections =
            SelectionSet::new(vec![range(0, 3), range(1, 1), range(5, 5), range(5, 5)], 3).unwrap();
        assert_eq!(selections.ranges(), &[range(0, 3), range(5, 5)]);
        assert_eq!(selections.primary_index(), 1);
    }

    #[test]
    fn adjacent_ranges_and_a_caret_at_the_end_stay_separate() {
        let selections = SelectionSet::new(vec![range(0, 2), range(2, 4), range(4, 4)], 1).unwrap();
        assert_eq!(
            selections.ranges(),
            &[range(0, 2), range(2, 4), range(4, 4)]
        );
    }

    #[test]
    fn caret_at_start_merges_with_the_following_range() {
        let selections = SelectionSet::new(vec![range(0, 2), range(2, 2), range(4, 2)], 2).unwrap();
        assert_eq!(selections.ranges(), &[range(0, 2), range(4, 2)]);
        assert_eq!(selections.primary_index(), 1);
    }

    #[test]
    fn empty_sets_and_invalid_primary_are_rejected() {
        assert_eq!(SelectionSet::new(vec![], 0), Err(Error::EmptySelectionSet));
        assert_eq!(
            SelectionSet::new(vec![range(0, 0)], 1),
            Err(Error::InvalidPrimary { index: 1, count: 1 })
        );
    }
}

#[cfg(test)]
mod property_tests {
    use crate::{CharOffset, Selection, SelectionSet};
    use proptest::prelude::*;

    fn range(anchor: usize, head: usize) -> Selection {
        Selection::new(CharOffset(anchor), CharOffset(head))
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]
        #[test]
        fn normalization_preserves_coverage_and_primary(
            input in prop::collection::vec((0usize..100, 0usize..100), 1..30),
            primary in any::<usize>(),
        ) {
            let primary = primary % input.len();
            let original_primary = range(input[primary].0, input[primary].1);
            let selections = SelectionSet::new(input.iter().map(|&(a, h)| range(a, h)).collect(), primary).unwrap();
            prop_assert!(selections.primary().start() <= original_primary.start());
            prop_assert!(selections.primary().end() >= original_primary.end());
            prop_assert_eq!(selections.primary().is_backward(), original_primary.is_backward());
            for position in 0..100 {
                let original = input.iter().any(|&(a, h)| (a.min(h)..a.max(h)).contains(&position));
                let normalized = selections.ranges().iter().any(|s| s.range().contains(&CharOffset(position)));
                prop_assert_eq!(original, normalized);
            }
            for pair in selections.ranges().windows(2) {
                prop_assert!(pair[0].end() <= pair[1].start());
                prop_assert!(pair[0].start() < pair[1].start());
            }
            let again = SelectionSet::new(selections.ranges().to_vec(), selections.primary_index()).unwrap();
            prop_assert_eq!(again, selections);
        }

    }
}
