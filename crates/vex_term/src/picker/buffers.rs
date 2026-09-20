//! Buffer catalogs use the shared bounded picker ranker.

use super::catalog;
use vex_core::DocumentId;

pub(crate) type CatalogEntry = catalog::CatalogEntry<DocumentId>;
pub(crate) type BufferJob = catalog::Job<DocumentId>;
pub(crate) type BufferResult = catalog::Result<DocumentId>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker::{Entry, files::MAX_RESULTS};
    use std::sync::Arc;
    use vex_core::Document;
    use vex_editor::background::Cancellation;

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
