//! Bounded fuzzy ranking shared by in-memory picker providers.

use super::{
    Entry, Item,
    files::MAX_RESULTS,
    fuzzy::{Matcher, Query},
};
use std::{cmp::Reverse, collections::BinaryHeap, sync::Arc};
use vex_editor::background::Cancellation;

pub(crate) struct CatalogEntry<T> {
    pub entry: Arc<Entry<T>>,
    pub accessed: u64,
}

pub(crate) struct Job<T> {
    pub session: u64,
    pub revision: u64,
    pub catalog: Arc<[CatalogEntry<T>]>,
    pub query: String,
    pub cancellation: Cancellation,
}

pub(crate) struct Result<T> {
    pub session: u64,
    pub revision: u64,
    pub items: Vec<Item<T>>,
    pub matched: usize,
    pub total: usize,
}

impl<T> Job<T> {
    pub fn run(self) -> Option<Result<T>> {
        if self.cancellation.is_cancelled() {
            return None;
        }
        let query = Query::new(&self.query);
        let mut matcher = Matcher::default();
        let mut ranked = BinaryHeap::new();
        let mut matched = 0;
        for (index, candidate) in self.catalog.iter().enumerate() {
            if self.cancellation.is_cancelled() {
                return None;
            }
            if let Some(score) = matcher.score(&candidate.entry.label, &query) {
                matched += 1;
                let rank = (score, candidate.accessed, Reverse(index));
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
        Some(Result {
            session: self.session,
            revision: self.revision,
            items,
            matched,
            total: self.catalog.len(),
        })
    }
}
