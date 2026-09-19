use std::{ops::Range, sync::Arc};

use crate::{CharOffset, DocumentId, Error, Revision, Rope, Selection, SelectionSet};

/// Which side of inserted/replacement text a position inside an edit follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Affinity {
    Before,
    After,
}

/// Replace a half-open scalar range with UTF-8 text.
#[derive(Clone, Debug)]
pub struct Edit {
    range: Range<CharOffset>,
    text: Arc<str>,
    inserted_chars: usize,
}

impl Edit {
    pub fn new(range: Range<CharOffset>, text: impl Into<Arc<str>>) -> Self {
        let text = text.into();
        let inserted_chars = text.chars().count();
        Self {
            range,
            text,
            inserted_chars,
        }
    }

    pub fn insert(position: CharOffset, text: impl Into<Arc<str>>) -> Self {
        Self::new(position..position, text)
    }

    pub fn delete(range: Range<CharOffset>) -> Self {
        Self::new(range, "")
    }

    pub fn range(&self) -> Range<CharOffset> {
        self.range.clone()
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

#[derive(Clone, Debug)]
struct Change {
    edit: Edit,
    new_start: usize,
    new_end: usize,
}

/// Validated edits expressed in a single document revision's coordinates.
///
/// Input order does not matter. Overlaps and shared starts are rejected;
/// adjacent ranges are allowed. Empty insertions are discarded. Position
/// mapping is O(log E) for E edits and never scans the document text.
#[derive(Clone, Debug)]
pub struct Transaction {
    pub(crate) document_id: DocumentId,
    pub(crate) revision: Revision,
    old_len: usize,
    new_len: usize,
    changes: Vec<Change>,
    pub(crate) selections: Option<SelectionSet>,
}

impl Transaction {
    pub(crate) fn new(
        document_id: DocumentId,
        revision: Revision,
        old_len: usize,
        edits: impl IntoIterator<Item = Edit>,
    ) -> Result<Self, Error> {
        let mut edits: Vec<_> = edits.into_iter().collect();
        for edit in &edits {
            if edit.range.start > edit.range.end {
                return Err(Error::ReversedRange {
                    start: edit.range.start,
                    end: edit.range.end,
                });
            }
            if edit.range.end.0 > old_len {
                return Err(Error::PositionOutOfBounds {
                    position: edit.range.end,
                    len: old_len,
                });
            }
        }
        edits.retain(|edit| !edit.range.is_empty() || !edit.text.is_empty());
        edits.sort_unstable_by_key(|edit| edit.range.start);
        let mut changes = Vec::with_capacity(edits.len());
        let mut consumed = 0;
        let mut produced = 0_usize;
        let mut previous_start = None;
        for edit in edits {
            let start = edit.range.start.0;
            if start < consumed || previous_start == Some(start) {
                return Err(Error::OverlappingEdits);
            }
            let new_start = produced
                .checked_add(start - consumed)
                .ok_or(Error::LengthOverflow)?;
            let new_end = new_start
                .checked_add(edit.inserted_chars)
                .ok_or(Error::LengthOverflow)?;
            consumed = edit.range.end.0;
            produced = new_end;
            previous_start = Some(start);
            changes.push(Change {
                edit,
                new_start,
                new_end,
            });
        }
        let new_len = produced
            .checked_add(old_len - consumed)
            .ok_or(Error::LengthOverflow)?;
        Ok(Self {
            document_id,
            revision,
            old_len,
            new_len,
            changes,
            selections: None,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn edits(&self) -> impl ExactSizeIterator<Item = &Edit> {
        self.changes.iter().map(|change| &change.edit)
    }

    /// Override automatic mapping with selections in the resulting document.
    pub fn with_selections(mut self, selections: SelectionSet) -> Result<Self, Error> {
        selections.validate(self.new_len)?;
        self.selections = Some(selections);
        Ok(self)
    }

    /// Positions inside replaced text, including its start, use `affinity`.
    /// An edit's end maps to the replacement's end, unless another edit starts
    /// there, in which case that edit's affinity applies.
    pub fn map_position(
        &self,
        position: CharOffset,
        affinity: Affinity,
    ) -> Result<CharOffset, Error> {
        if position.0 > self.old_len {
            return Err(Error::PositionOutOfBounds {
                position,
                len: self.old_len,
            });
        }
        Ok(self.map_position_unchecked(position, affinity))
    }

    fn map_position_unchecked(&self, position: CharOffset, affinity: Affinity) -> CharOffset {
        let index = self
            .changes
            .partition_point(|change| change.edit.range.start <= position);
        let Some(change) = index.checked_sub(1).map(|index| &self.changes[index]) else {
            return position;
        };
        if position < change.edit.range.end || position == change.edit.range.start {
            return CharOffset(match affinity {
                Affinity::Before => change.new_start,
                Affinity::After => change.new_end,
            });
        }
        CharOffset(change.new_end + (position.0 - change.edit.range.end.0))
    }

    /// Map both endpoints with the same affinity, then normalize collisions.
    pub fn map_selections(
        &self,
        selections: &SelectionSet,
        affinity: Affinity,
    ) -> Result<SelectionSet, Error> {
        selections.validate(self.old_len)?;
        SelectionSet::new(
            selections
                .ranges()
                .iter()
                .map(|range| {
                    Selection::new(
                        self.map_position_unchecked(range.anchor, affinity),
                        self.map_position_unchecked(range.head, affinity),
                    )
                })
                .collect(),
            selections.primary_index(),
        )
    }

    pub(crate) fn apply_to(&self, text: &mut Rope) {
        // Reversing the batch preserves all of its original coordinates.
        for change in self.changes.iter().rev() {
            let range = &change.edit.range;
            if !range.is_empty() {
                text.remove(range.start.0..range.end.0);
            }
            if !change.edit.text.is_empty() {
                text.insert(range.start.0, &change.edit.text);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{Affinity, CharOffset, Document, Edit, Selection, SelectionSet};

    fn range(anchor: usize, head: usize) -> Selection {
        Selection::new(CharOffset(anchor), CharOffset(head))
    }

    #[test]
    fn mapping_handles_insertions_replacements_deletions_and_shared_boundaries() {
        let document = Document::from("abcdefghij");
        let transaction = document
            .transaction([
                Edit::insert(CharOffset(1), "XY"),
                Edit::new(CharOffset(3)..CharOffset(5), "Z"),
                Edit::new(CharOffset(5)..CharOffset(7), "pq"),
                Edit::delete(CharOffset(8)..CharOffset(10)),
                Edit::insert(CharOffset(10), "!"),
            ])
            .unwrap();
        let expected_before = [0, 1, 4, 5, 5, 6, 6, 8, 9, 9, 9];
        let expected_after = [0, 3, 4, 6, 6, 8, 8, 8, 9, 9, 10];
        for position in 0..=10 {
            assert_eq!(
                transaction
                    .map_position(CharOffset(position), Affinity::Before)
                    .unwrap(),
                CharOffset(expected_before[position])
            );
            assert_eq!(
                transaction
                    .map_position(CharOffset(position), Affinity::After)
                    .unwrap(),
                CharOffset(expected_after[position])
            );
        }
        assert!(
            transaction
                .map_position(CharOffset(11), Affinity::After)
                .is_err()
        );
    }

    #[test]
    fn mapping_merges_colliding_ranges_and_retains_primary() {
        let document = Document::from("abcdefgh");
        let before = SelectionSet::new(vec![range(1, 2), range(4, 5), range(7, 8)], 1).unwrap();
        let transaction = document
            .transaction([Edit::delete(CharOffset(0)..CharOffset(6))])
            .unwrap();
        let mapped = transaction
            .map_selections(&before, Affinity::After)
            .unwrap();
        assert_eq!(mapped.ranges(), &[range(0, 0), range(1, 2)]);
        assert_eq!(mapped.primary_index(), 0);
    }

    #[test]
    fn invalid_batches_are_rejected_before_any_mutation() {
        let document = Document::from("abc");
        for edits in [
            vec![
                Edit::insert(CharOffset(0), "valid"),
                Edit::delete(CharOffset(2)..CharOffset(4)),
            ],
            vec![Edit::delete(CharOffset(2)..CharOffset(1))],
            vec![
                Edit::new(CharOffset(0)..CharOffset(2), "x"),
                Edit::delete(CharOffset(1)..CharOffset(3)),
            ],
            vec![
                Edit::insert(CharOffset(1), "x"),
                Edit::insert(CharOffset(1), "y"),
            ],
            vec![
                Edit::insert(CharOffset(1), "x"),
                Edit::delete(CharOffset(1)..CharOffset(2)),
            ],
            vec![
                Edit::delete(CharOffset(0)..CharOffset(3)),
                Edit::insert(CharOffset(1), "x"),
            ],
            vec![Edit::insert(CharOffset(usize::MAX), "")],
        ] {
            assert!(document.transaction(edits).is_err());
            assert_eq!(document.text(), "abc");
            assert_eq!(document.revision().get(), 0);
            assert_eq!(document.undo_depth(), 0);
        }
    }
}

#[cfg(test)]
mod property_tests {
    use crate::{Affinity, CharOffset, Document, Edit, Selection, SelectionSet};
    use proptest::prelude::*;

    fn text(max: usize) -> impl Strategy<Value = String> {
        prop::collection::vec(any::<char>(), 0..max).prop_map(|chars| chars.into_iter().collect())
    }

    fn range(anchor: usize, head: usize) -> Selection {
        Selection::new(CharOffset(anchor), CharOffset(head))
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]
        #[test]
        fn batches_match_an_independent_flat_text_model(
            original in text(200),
            raw in prop::collection::vec((any::<usize>(), any::<usize>(), text(12)), 0..25),
            anchor in any::<usize>(),
            head in any::<usize>(),
        ) {
            let chars: Vec<char> = original.chars().collect();
            let mut edits: Vec<_> = raw.into_iter().map(|(a, b, text)| {
                let a = a % (chars.len() + 1);
                let b = b % (chars.len() + 1);
                (a.min(b), a.max(b), text)
            }).collect();
            edits.sort_by_key(|edit| edit.0);
            let mut disjoint: Vec<(usize, usize, String)> = Vec::new();
            for edit in edits {
                if disjoint.last().is_none_or(|last| last.1 <= edit.0 && last.0 != edit.0) {
                    disjoint.push(edit);
                }
            }

            // Build expected text from untouched spans and replacements, from left
            // to right. This does not use the rope or the transaction mapping code.
            let mut expected = String::new();
            let mut copied = 0;
            for (start, end, inserted) in &disjoint {
                expected.extend(&chars[copied..*start]);
                expected.push_str(inserted);
                copied = *end;
            }
            expected.extend(&chars[copied..]);

            let mut document = Document::from(original.as_str());
            let snapshot = document.snapshot();
            let before = SelectionSet::single(range(anchor % (chars.len() + 1), head % (chars.len() + 1)));
            let mut selections = before.clone();
            // Reverse input to also test transaction ordering.
            let transaction = document.transaction(disjoint.iter().rev().map(|(s, e, t)| {
                Edit::new(CharOffset(*s)..CharOffset(*e), t.as_str())
            })).unwrap();
            for affinity in [Affinity::Before, Affinity::After] {
                let mapped = (0..=chars.len()).map(|p| transaction.map_position(CharOffset(p), affinity).unwrap()).collect::<Vec<_>>();
                prop_assert!(mapped.windows(2).all(|p| p[0] <= p[1]));
                prop_assert!(mapped.iter().all(|p| p.0 <= expected.chars().count()));
            }
            let changed = !transaction.is_empty();
            prop_assert_eq!(document.apply(transaction, &mut selections).unwrap(), changed);
            prop_assert_eq!(document.text().to_string(), expected.as_str());
            prop_assert_eq!(snapshot.text().to_string(), original.as_str());
            prop_assert!(selections.ranges().iter().all(|s| s.end().0 <= document.text().len_chars()));
            let after = selections.clone();
            prop_assert_eq!(document.undo(&mut selections).unwrap(), changed);
            prop_assert_eq!(document.text().to_string(), original.as_str());
            prop_assert_eq!(&selections, &before);
            prop_assert_eq!(document.redo(&mut selections).unwrap(), changed);
            prop_assert_eq!(document.text().to_string(), expected);
            prop_assert_eq!(selections, after);
        }

    }
}
