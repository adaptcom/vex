//! Lazy positions over a text-free forward journal. Capturing a bookmark or a
//! resolver is O(1); edits never visit bookmark selections. Links are retained
//! only by live bookmarks/resolvers, independently of undo history eviction.

use crate::{DocumentId, Error, Revision, SelectionSet, mapping::PositionMaps};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};

#[derive(Default)]
struct Node {
    next: OnceLock<Link>,
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Node")
            .field("linked", &self.next.get().is_some())
            .finish()
    }
}

#[derive(Debug)]
struct Link {
    batch: Batch,
    next: Arc<Node>,
}

// Expiring a very old bookmark must not recursively drop an unbounded chain.
impl Drop for Node {
    fn drop(&mut self) {
        let mut next = self.next.take().map(|link| link.next);
        while let Some(node) = next {
            let Ok(mut node) = Arc::try_unwrap(node) else {
                break;
            };
            next = node.next.take().map(|link| link.next);
        }
    }
}

#[derive(Clone, Debug)]
struct Batch {
    end: Revision,
    maps: PositionMaps,
    reverse: bool,
}

/// An opaque position in a document's change journal, with no text snapshot.
/// Clone it to retain the same revision. Keep the original selections alongside
/// it, then resolve them with `Document::position_resolver` on a worker.
#[derive(Clone, Debug)]
pub struct Bookmark {
    document: DocumentId,
    revision: Revision,
    length: usize,
    node: Arc<Node>,
}

impl Bookmark {
    pub fn document_id(&self) -> DocumentId {
        self.document
    }
    pub fn revision(&self) -> Revision {
        self.revision
    }
}

/// Immutable upper bound for remapping several bookmarks to one revision. Later
/// edits cannot enter this resolver, even while it is running on another thread.
#[derive(Clone, Debug)]
pub struct PositionResolver {
    end: Bookmark,
    pending: Option<Batch>,
}

impl PositionResolver {
    pub fn bookmark(&self) -> &Bookmark {
        &self.end
    }

    /// Map selections with sticky edge affinities, normalizing collisions after
    /// each map. Returns None on cancellation. Text/grapheme normalization is
    /// left to the consumer with the matching document snapshot.
    pub fn resolve(
        &self,
        from: &Bookmark,
        selections: &SelectionSet,
        cancelled: impl Fn() -> bool,
    ) -> Result<Option<SelectionSet>, Error> {
        if from.document != self.end.document {
            return Err(Error::WrongDocument);
        }
        if from.revision > self.end.revision {
            return Err(Error::StaleRevision {
                expected: from.revision,
                actual: self.end.revision,
            });
        }
        selections.validate(from.length)?;
        if cancelled() {
            return Ok(None);
        }
        let mut selections = selections.clone();
        if from.revision == self.end.revision {
            return Ok(Some(selections));
        }
        let mut apply = |batch: &Batch| -> Result<bool, Error> {
            if batch.end <= from.revision {
                return Ok(true);
            }
            let maps = batch.maps.as_slice();
            for index in 0..maps.len() {
                if cancelled() {
                    return Ok(false);
                }
                let map = &maps[if batch.reverse {
                    maps.len() - index - 1
                } else {
                    index
                }];
                let mut ranges = Vec::with_capacity(selections.ranges().len());
                for &range in selections.ranges() {
                    if cancelled() {
                        return Ok(false);
                    }
                    ranges.push(map.sticky_selection(range, batch.reverse));
                }
                selections = SelectionSet::new(ranges, selections.primary_index())?;
            }
            Ok(true)
        };
        let mut node = &from.node;
        while !Arc::ptr_eq(node, &self.end.node) {
            if cancelled() {
                return Ok(None);
            }
            let link = node
                .next
                .get()
                .expect("a live bookmark retains its forward journal");
            if !apply(&link.batch)? {
                return Ok(None);
            }
            node = &link.next;
        }
        if let Some(batch) = &self.pending
            && !apply(batch)?
        {
            return Ok(None);
        }
        selections.validate(self.end.length)?;
        Ok((!cancelled()).then_some(selections))
    }
}

#[derive(Debug, Default)]
pub(crate) struct Journal {
    tail: Arc<Node>,
    pending: Option<Batch>,
    // A new bookmark pins the current boundary. The next edit publishes the
    // pending batch before recording changes beyond that bookmark's revision.
    seal: AtomicBool,
}

impl Journal {
    pub fn bookmark(&self, document: DocumentId, revision: Revision, length: usize) -> Bookmark {
        self.seal.store(true, Ordering::Relaxed);
        Bookmark {
            document,
            revision,
            length,
            node: self.tail.clone(),
        }
    }

    pub fn resolver(
        &self,
        document: DocumentId,
        revision: Revision,
        length: usize,
    ) -> PositionResolver {
        PositionResolver {
            end: self.bookmark(document, revision, length),
            pending: self.pending.clone(),
        }
    }

    fn flush(&mut self) {
        if let Some(batch) = self.pending.take() {
            let next = Arc::new(Node::default());
            self.tail
                .next
                .set(Link {
                    batch,
                    next: next.clone(),
                })
                .expect("publish once");
            self.tail = next;
        }
    }

    pub fn record(&mut self, end: Revision, maps: &PositionMaps, reverse: bool) {
        if Arc::strong_count(&self.tail) == 1 {
            self.pending = None; // No live bookmark needs this change or earlier metadata.
            self.seal.store(false, Ordering::Relaxed);
            return;
        }
        let seal = self.seal.swap(false, Ordering::Relaxed);
        if seal {
            self.flush();
        }
        if !reverse
            && let Some(previous) = &mut self.pending
            && !previous.reverse
            && previous.maps.try_merge_typing(maps)
        {
            previous.end = end;
            return;
        }
        self.flush();
        self.pending = Some(Batch {
            end,
            maps: maps.clone(),
            reverse,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CharOffset, Document, Edit, Selection};
    use proptest::prelude::*;

    fn range(a: usize, h: usize) -> Selection {
        Selection::new(CharOffset(a), CharOffset(h))
    }
    fn apply(document: &mut Document, start: usize, end: usize, text: &str, grouped: bool) {
        let mut selections = SelectionSet::single(range(start, end));
        let transaction = document
            .transaction([Edit::new(CharOffset(start)..CharOffset(end), text)])
            .unwrap();
        if grouped {
            document
                .apply_grouped(transaction, &mut selections)
                .unwrap();
        } else {
            document.apply(transaction, &mut selections).unwrap();
        }
    }
    fn resolve(document: &Document, mark: &Bookmark, selections: &SelectionSet) -> SelectionSet {
        document
            .position_resolver()
            .resolve(mark, selections, || false)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn bookmarks_capture_exact_boundaries_inside_typing_groups_and_resolvers_are_immutable() {
        let mut document = Document::from("abc");
        let first = document.bookmark();
        let original = SelectionSet::single(range(0, 1));
        apply(&mut document, 0, 0, "x", true);
        let middle = document.bookmark();
        let at_middle = SelectionSet::single(range(1, 2));
        let frozen = document.position_resolver();
        apply(&mut document, 1, 1, "y", true);
        assert_eq!(
            frozen
                .resolve(&first, &original, || false)
                .unwrap()
                .unwrap(),
            at_middle
        );
        assert_eq!(
            resolve(&document, &first, &original),
            SelectionSet::single(range(2, 3))
        );
        assert_eq!(
            resolve(&document, &middle, &at_middle),
            SelectionSet::single(range(2, 3))
        );
        let mut active = SelectionSet::default();
        document.undo(&mut active).unwrap();
        assert_eq!(resolve(&document, &first, &original), original);
        document.redo(&mut active).unwrap();
        assert_eq!(
            resolve(&document, &middle, &at_middle),
            SelectionSet::single(range(2, 3))
        );
        assert!(matches!(
            frozen.resolve(&document.bookmark(), &original, || false),
            Err(Error::StaleRevision { .. })
        ));
        assert!(matches!(
            Document::from("abc")
                .position_resolver()
                .resolve(&first, &original, || false),
            Err(Error::WrongDocument)
        ));
    }

    #[test]
    fn insertion_edges_replacements_direction_primary_and_history_eviction_are_preserved() {
        let mut document = Document::from("abcdefghij");
        document.set_history_limit(0);
        let mark = document.bookmark();
        let selections =
            SelectionSet::new(vec![range(1, 3), range(8, 6), range(10, 10)], 1).unwrap();
        apply(&mut document, 3, 3, "🦀", false);
        assert_eq!(
            resolve(&document, &mark, &selections),
            SelectionSet::new(vec![range(1, 3), range(9, 7), range(11, 11)], 1).unwrap()
        );
        apply(&mut document, 1, 1, "e\u{301}", false);
        assert_eq!(
            resolve(&document, &mark, &selections),
            SelectionSet::new(vec![range(3, 5), range(11, 9), range(13, 13)], 1).unwrap()
        );
        let now = document.bookmark();
        let caret = SelectionSet::single(range(4, 4));
        apply(&mut document, 2, 6, "WXYZ", false);
        assert_eq!(resolve(&document, &now, &caret), caret);
        assert_eq!(document.undo_depth(), 0);
        let before = document.bookmark();
        apply(&mut document, 2, 6, "Q", false);
        assert_eq!(
            resolve(&document, &before, &SelectionSet::single(range(2, 2))),
            SelectionSet::single(range(2, 2))
        );
        assert_eq!(
            resolve(&document, &before, &caret),
            SelectionSet::single(range(3, 3))
        );
    }

    #[test]
    fn typing_metadata_compacts_but_many_distinct_changes_cancel_and_drop_iteratively() {
        let mut document = Document::from("abc");
        document.set_history_limit(0);
        let mark = document.bookmark();
        for n in 0..10_000 {
            apply(&mut document, n, n, "x", true);
        }
        let end = document.position_resolver();
        assert!(Arc::ptr_eq(&mark.node, &end.end.node));
        assert_eq!(end.pending.as_ref().unwrap().maps.as_slice().len(), 1);
        let at = SelectionSet::single(range(0, 1));
        assert_eq!(
            end.resolve(&mark, &at, || false).unwrap().unwrap(),
            SelectionSet::single(range(10_000, 10_001))
        );
        drop(end);
        for _ in 0..10_000 {
            apply(&mut document, 0, 1, "y", false);
        }
        let end = document.position_resolver();
        let polls = std::cell::Cell::new(0);
        assert!(
            end.resolve(&mark, &at, || {
                polls.set(polls.get() + 1);
                polls.get() >= 10
            })
            .unwrap()
            .is_none()
        );
        assert_eq!(polls.get(), 10);
        let weak = Arc::downgrade(&mark.node);
        drop(mark); // Dropping the oldest root releases 10,000 nodes without recursion.
        assert!(weak.upgrade().is_none());
        drop(end);
        apply(&mut document, 0, 1, "z", false);
        assert!(document.position_resolver().pending.is_none());
    }

    // Independent scalar model of sticky endpoints for one replacement.
    fn model(selections: &SelectionSet, start: usize, end: usize, len: usize) -> SelectionSet {
        let position = |p: usize, after: bool| {
            if p < start {
                return p;
            }
            if p == start && start != end {
                return start;
            }
            if p <= start || p < end {
                if after {
                    if end - start == len { p } else { start + len }
                } else {
                    start
                }
            } else {
                start + len + p - end
            }
        };
        SelectionSet::new(
            selections
                .ranges()
                .iter()
                .map(|s| {
                    range(
                        position(s.anchor.0, s.anchor <= s.head),
                        position(s.head.0, s.head <= s.anchor),
                    )
                })
                .collect(),
            selections.primary_index(),
        )
        .unwrap()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]
        #[test]
        fn lazy_resolution_agrees_with_eager_mapping_through_edits_undo_redo_and_eviction(
            actions in prop::collection::vec((0u8..7, 0usize..80, 0usize..8, 0usize..5), 1..80),
        ) {
            let mut document = Document::from("abcdefghijklmnop");
            let original = SelectionSet::new(vec![range(1, 4), range(10, 7), range(16, 16)], 1).unwrap();
            let mut marks = vec![(document.bookmark(), original.clone(), original)];
            let mut past = Vec::new();
            let mut future = Vec::new();
            for (kind, a, length, inserted) in actions {
                let edit = match kind {
                    0..=3 => {
                        let start = a % (document.text().len_chars() + 1);
                        let end = (start + length).min(document.text().len_chars());
                        apply(&mut document, start, end, &"x".repeat(inserted), false);
                        if start == end && inserted == 0 { None } else {
                            past.push((start, end - start, inserted)); future.clear();
                            Some((start, end, inserted))
                        }
                    }
                    4 => {
                        document.undo(&mut SelectionSet::default()).unwrap();
                        past.pop().map(|(start, old, new)| { future.push((start, old, new)); (start, start + new, old) })
                    }
                    5 => {
                        document.redo(&mut SelectionSet::default()).unwrap();
                        future.pop().map(|(start, old, new)| { past.push((start, old, new)); (start, start + old, new) })
                    }
                    _ => {
                        let start = a % (document.text().len_chars() + 1);
                        let end = (start + length).min(document.text().len_chars());
                        let set = SelectionSet::single(range(start, end));
                        if marks.len() == 8 { marks.remove(0); }
                        marks.push((document.bookmark(), set.clone(), set));
                        None
                    }
                };
                if let Some((start, end, len)) = edit {
                    for (_, _, expected) in &mut marks { *expected = model(expected, start, end, len); }
                }
                let resolver = document.position_resolver();
                for (bookmark, original, expected) in &marks {
                    prop_assert_eq!(resolver.resolve(bookmark, original, || false).unwrap().unwrap(), expected.clone());
                }
            }
        }
    }
}
