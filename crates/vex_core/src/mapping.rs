//! Text-free position maps retained with undo groups for other views.

use crate::{Affinity, CharOffset, Error, Selection, SelectionSet, Transaction};
use std::{ops::Range, sync::Arc};

/// Share undo metadata in O(1), without allocating a container for the common
/// single-map group. Subsequent adjacent typing can still compact in place.
#[derive(Clone, Debug)]
pub(crate) enum PositionMaps {
    Single(Arc<PositionMap>),
    Multiple(Arc<Vec<Arc<PositionMap>>>),
}

impl PositionMaps {
    pub fn as_slice(&self) -> &[Arc<PositionMap>] {
        match self {
            Self::Single(map) => std::slice::from_ref(map),
            Self::Multiple(maps) => maps,
        }
    }

    pub fn push(&mut self, map: Arc<PositionMap>) {
        match self {
            Self::Single(previous) => {
                if previous.can_merge_typing(&map) {
                    Arc::make_mut(previous).merge_typing(&map);
                    return;
                }
                *self = Self::Multiple(Arc::new(vec![previous.clone(), map]));
            }
            Self::Multiple(maps) => {
                let maps = Arc::make_mut(maps);
                if let Some(previous) = maps
                    .last_mut()
                    .filter(|previous| previous.can_merge_typing(&map))
                {
                    Arc::make_mut(previous).merge_typing(&map);
                } else {
                    maps.push(map);
                }
            }
        }
    }

    pub fn try_merge_typing(&mut self, next: &Self) -> bool {
        let (Self::Single(previous), Self::Single(next)) = (self, next) else {
            return false;
        };
        if !previous.can_merge_typing(next) {
            return false;
        }
        Arc::make_mut(previous).merge_typing(next)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PositionMap {
    changes: Vec<(Range<CharOffset>, Range<CharOffset>)>,
}

impl PositionMap {
    /// Helix's sticky selection affinities: starts/carets follow insertions,
    /// ends precede them, and equal-size replacements preserve interior starts.
    pub fn sticky_selection(&self, selection: Selection, reverse: bool) -> Selection {
        let position = |position: CharOffset, after: bool| {
            let ranges = |index: usize| {
                let (old, new) = &self.changes[index];
                if reverse { (new, old) } else { (old, new) }
            };
            // Before-affinity positions on a replacement's start stay in the
            // preceding gap, including boundaries shared by adjacent edits.
            let index = self.changes.partition_point(|change| {
                let old = if reverse { &change.1 } else { &change.0 };
                if after {
                    old.start <= position
                } else {
                    old.start < position
                }
            });
            let Some(index) = index.checked_sub(1) else {
                return position;
            };
            let (old, new) = ranges(index);
            if position < old.end || position == old.start {
                if position == old.start && !old.is_empty() {
                    new.start
                } else if after && old.end.0 - old.start.0 == new.end.0 - new.start.0 {
                    CharOffset(new.start.0 + position.0 - old.start.0)
                } else if after {
                    new.end
                } else {
                    new.start
                }
            } else {
                CharOffset(new.end.0 + position.0 - old.end.0)
            }
        };
        Selection::new(
            position(selection.anchor, selection.anchor <= selection.head),
            position(selection.head, selection.head <= selection.anchor),
        )
    }

    pub fn changes(
        &self,
    ) -> impl ExactSizeIterator<Item = (&Range<CharOffset>, &Range<CharOffset>)> {
        self.changes.iter().map(|(old, new)| (old, new))
    }

    pub fn new(transaction: &Transaction) -> Self {
        Self {
            changes: transaction
                .edits()
                .map(|edit| {
                    let old = edit.range();
                    let start = transaction
                        .map_position(old.start, Affinity::Before)
                        .unwrap();
                    let end = transaction
                        .map_position(old.start, Affinity::After)
                        .unwrap();
                    (old, start..end)
                })
                .collect(),
        }
    }

    fn position(&self, position: CharOffset, reverse: bool) -> CharOffset {
        let ranges = |change: &(Range<CharOffset>, Range<CharOffset>)| {
            if reverse {
                (change.1.clone(), change.0.clone())
            } else {
                change.clone()
            }
        };
        let index = self
            .changes
            .partition_point(|change| ranges(change).0.start <= position);
        let Some(change) = index.checked_sub(1).map(|index| &self.changes[index]) else {
            return position;
        };
        let (old, new) = ranges(change);
        if position < old.end || position == old.start {
            new.end
        } else {
            CharOffset(new.end.0 + (position.0 - old.end.0))
        }
    }

    pub fn selections(
        &self,
        selections: &SelectionSet,
        reverse: bool,
    ) -> Result<SelectionSet, Error> {
        SelectionSet::new(
            selections
                .ranges()
                .iter()
                .map(|selection| {
                    Selection::new(
                        self.position(selection.anchor, reverse),
                        self.position(selection.head, reverse),
                    )
                })
                .collect(),
            selections.primary_index(),
        )
    }

    // Ordinary single-caret typing retains one map for the whole undo group.
    // Multi-edit groups keep their exact sequence, without retaining inserted text.
    pub fn merge_typing(&mut self, next: &Self) -> bool {
        if !self.can_merge_typing(next) {
            return false;
        }
        self.changes[0].1.end = next.changes[0].1.end;
        true
    }

    fn can_merge_typing(&self, next: &Self) -> bool {
        self.changes.len() == 1
            && next.changes.len() == 1
            && self.changes[0].0.is_empty()
            && next.changes[0].0.is_empty()
            && next.changes[0].0.start == self.changes[0].1.end
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Document, Edit};

    #[test]
    fn disjoint_edits_and_reverse_maps_preserve_positions_between_changes() {
        let document = Document::from("abcdefghij");
        let transaction = document
            .transaction([
                Edit::insert(CharOffset(1), "XX"),
                Edit::new(CharOffset(7)..CharOffset(9), "Y"),
            ])
            .unwrap();
        let map = PositionMap::new(&transaction);
        for position in 0..=10 {
            assert_eq!(
                map.position(CharOffset(position), false),
                transaction
                    .map_position(CharOffset(position), Affinity::After)
                    .unwrap()
            );
        }
        assert_eq!(map.position(CharOffset(6), true), CharOffset(4));
        assert_eq!(map.position(CharOffset(11), true), CharOffset(10));
    }
}
