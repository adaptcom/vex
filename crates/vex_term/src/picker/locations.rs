//! Shared location catalogs are labelled once on the worker, then ranked with a
//! bounded heap. Only the retained rows cross back to the drawing thread.

use super::{Entry, catalog};
use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
};
use vex_editor::background::Cancellation;
use vex_lsp::{Destination, Locations};

type Entries = Arc<[catalog::CatalogEntry<Arc<Destination>>]>;

pub(crate) struct Catalog {
    locations: Locations,
    cwd: PathBuf,
    entries: OnceLock<Entries>,
}

impl Catalog {
    pub fn new(locations: Locations, cwd: PathBuf) -> Self {
        Self {
            locations,
            cwd,
            entries: OnceLock::new(),
        }
    }
}

pub(crate) struct Job {
    pub session: u64,
    pub revision: u64,
    pub catalog: Arc<Catalog>,
    pub query: String,
    pub cancellation: Cancellation,
}

pub(crate) type Result = catalog::Result<Arc<Destination>>;

impl Job {
    pub fn run(self) -> Option<Result> {
        if self.catalog.entries.get().is_none() {
            let mut entries = Vec::with_capacity(self.catalog.locations.items.len());
            for destination in &self.catalog.locations.items {
                if self.cancellation.is_cancelled() {
                    return None;
                }
                let path = destination
                    .path
                    .strip_prefix(&self.catalog.cwd)
                    .unwrap_or(&destination.path);
                entries.push(catalog::CatalogEntry {
                    entry: Arc::new(Entry {
                        label: format!(
                            "{}:{}:{}",
                            path.display(),
                            u64::from(destination.range.start.line) + 1,
                            u64::from(destination.range.start.character) + 1
                        ),
                        value: Arc::new(destination.clone()),
                    }),
                    accessed: 0,
                });
            }
            let _ = self.catalog.entries.set(entries.into());
        }
        let mut result = catalog::Job {
            session: self.session,
            revision: self.revision,
            catalog: self.catalog.entries.get()?.clone(),
            query: self.query,
            cancellation: self.cancellation,
        }
        .run()?;
        let mut notices = Vec::new();
        if self.catalog.locations.limited {
            notices.push("Location limit reached".to_owned());
        }
        if self.catalog.locations.skipped != 0 {
            notices.push(format!(
                "{} unsupported or invalid locations skipped",
                self.catalog.locations.skipped
            ));
        }
        result.notice = notices.join(" · ");
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vex_lsp::{Position, Range};

    #[test]
    fn location_ranking_is_bounded_cancelled_and_reuses_labels() {
        let catalog = Arc::new(Catalog::new(
            Locations {
                items: (0..1000)
                    .map(|line| Destination {
                        path: "/project/main.rs".into(),
                        range: Range {
                            start: Position { line, character: 0 },
                            end: Position { line, character: 3 },
                        },
                    })
                    .collect(),
                limited: true,
                skipped: 2,
            },
            "/project".into(),
        ));
        let job = |query: &str| Job {
            session: 1,
            revision: 2,
            catalog: catalog.clone(),
            query: query.into(),
            cancellation: Cancellation::default(),
        };
        let result = job("").run().unwrap();
        assert_eq!(result.items.len(), super::super::files::MAX_RESULTS);
        assert_eq!(result.total, 1000);
        assert_eq!(result.matched, 1000);
        assert!(result.notice.contains("limit"));
        assert!(result.notice.contains("2 unsupported"));
        let entries = catalog.entries.get().unwrap().clone();
        let result = job("main.rs:1000:").run().unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].entry.value.range.start.line, 999);
        assert!(Arc::ptr_eq(&entries, catalog.entries.get().unwrap()));
        let cancelled = job("");
        cancelled.cancellation.cancel();
        assert!(cancelled.run().is_none());
    }
}
