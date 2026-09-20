//! Bounded buffer ranking on the existing picker worker. Catalogs contain only
//! labels and identities; document snapshots are requested for one preview.

use super::{
    Entry, Item,
    files::MAX_RESULTS,
    fuzzy::{Matcher, Query},
};
use std::{cmp::Reverse, collections::BinaryHeap, sync::Arc};
use vex_core::DocumentId;
use vex_editor::background::Cancellation;

pub(crate) struct CatalogEntry {
    pub entry: Arc<Entry<DocumentId>>,
    pub accessed: u64,
}

pub(crate) struct BufferJob {
    pub session: u64,
    pub revision: u64,
    pub catalog: Arc<[CatalogEntry]>,
    pub query: String,
    pub cancellation: Cancellation,
}

pub(crate) struct BufferResult {
    pub session: u64,
    pub revision: u64,
    pub items: Vec<Item<DocumentId>>,
    pub matched: usize,
    pub total: usize,
}

impl BufferJob {
    pub fn run(self) -> Option<BufferResult> {
        let query = Query::new(&self.query);
        let mut matcher = Matcher::default();
        let mut ranked = BinaryHeap::new();
        let mut matched = 0;
        for (index, buffer) in self.catalog.iter().enumerate() {
            if self.cancellation.is_cancelled() {
                return None;
            }
            if let Some(score) = matcher.score(&buffer.entry.label, &query) {
                matched += 1;
                let rank = (score, buffer.accessed, Reverse(index));
                if ranked.len() < MAX_RESULTS {
                    ranked.push(Reverse(rank));
                } else if rank > ranked.peek().unwrap().0 {
                    *ranked.peek_mut().unwrap() = Reverse(rank);
                }
            }
        }
        let mut items = Vec::with_capacity(ranked.len());
        for Reverse((_, _, Reverse(index))) in ranked.into_sorted_vec() {
            if self.cancellation.is_cancelled() {
                return None;
            }
            let entry = self.catalog[index].entry.clone();
            let matched = matcher.indices(&entry.label, &query);
            items.push(Item { entry, matched });
        }
        Some(BufferResult {
            session: self.session,
            revision: self.revision,
            items,
            matched,
            total: self.catalog.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vex_core::Document;

    #[test]
    fn ranking_is_bounded_recent_first_and_cancellable() {
        let catalog: Arc<[CatalogEntry]> = (0..700)
            .map(|n| CatalogEntry {
                entry: Arc::new(Entry {
                    label: format!("src/buffer{n}.rs"),
                    value: Document::default().id(),
                }),
                accessed: n,
            })
            .collect();
        let job = |query: &str| BufferJob {
            session: 1,
            revision: 2,
            catalog: catalog.clone(),
            query: query.into(),
            cancellation: Cancellation::default(),
        };
        let all = job("").run().unwrap();
        assert_eq!(all.matched, 700);
        assert_eq!(all.items.len(), MAX_RESULTS);
        assert_eq!(all.items[0].entry.value, catalog[699].entry.value);
        let filtered = job("bf698").run().unwrap();
        assert_eq!(filtered.items.len(), 1);
        assert_eq!(filtered.items[0].entry.value, catalog[698].entry.value);
        assert!(!filtered.items[0].matched.is_empty());
        let cancelled = job("");
        cancelled.cancellation.cancel();
        assert!(cancelled.run().is_none());
    }
}
