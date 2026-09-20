//! Jump labels and fuzzy ranking run on the picker worker, over shared snapshots.
//! Large selections contribute only a bounded snippet, never a flattened buffer.

use super::{Entry, catalog};
use std::sync::{Arc, OnceLock};
use vex_core::{Bookmark, DocumentId, PositionResolver, Revision, SelectionSet, Snapshot};
use vex_editor::background::Cancellation;

const SNIPPET_CHARS: usize = 256;
const SNIPPET_RANGES: usize = 32;

pub(crate) struct Document {
    pub snapshot: Snapshot,
    pub resolver: PositionResolver,
    pub label: String,
}

pub(crate) struct Capture {
    pub identity: u64,
    pub document: Arc<Document>,
    pub selections: Arc<SelectionSet>,
    pub bookmark: Bookmark,
    pub current: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Location {
    pub identity: u64,
    pub document: DocumentId,
    pub revision: Revision,
    pub selections: Arc<SelectionSet>,
    pub line: usize,
}

// Checkpoint identity stays stable across catalog refreshes and query edits.
// Comparing picker rows must not walk a potentially huge selection set.
impl PartialEq for Location {
    fn eq(&self, other: &Self) -> bool {
        self.identity == other.identity
    }
}
impl Eq for Location {}

#[derive(Default)]
pub(crate) struct Catalog {
    pub captures: Vec<Capture>,
    labels: OnceLock<Arc<[catalog::CatalogEntry<Location>]>>,
}

impl Catalog {
    pub fn new(captures: Vec<Capture>) -> Self {
        Self {
            captures,
            labels: OnceLock::new(),
        }
    }

    fn labels(
        &self,
        cancel: &Cancellation,
    ) -> Option<std::result::Result<Arc<[catalog::CatalogEntry<Location>]>, vex_core::Error>> {
        if cancel.is_cancelled() {
            return None;
        }
        if let Some(labels) = self.labels.get() {
            return Some(Ok(labels.clone()));
        }
        let mut labels = Vec::with_capacity(self.captures.len());
        for capture in &self.captures {
            if cancel.is_cancelled() {
                return None;
            }
            let snapshot = &capture.document.snapshot;
            let text = snapshot.text();
            let selections = if capture.bookmark.revision() == snapshot.revision() {
                capture.selections.clone()
            } else {
                match capture.document.resolver.resolve(
                    &capture.bookmark,
                    &capture.selections,
                    || cancel.is_cancelled(),
                ) {
                    Ok(Some(mapped)) => Arc::new(mapped),
                    Ok(None) => return None,
                    Err(error) => return Some(Err(error)),
                }
            };
            let primary = selections.primary();
            let cursor = primary
                .head
                .0
                .saturating_sub(usize::from(primary.anchor < primary.head));
            let line = text.char_to_line(cursor.min(text.len_chars()));
            let mut label = format!(
                "{}:{}{}  ",
                capture.document.label,
                line + 1,
                if capture.current { " *" } else { "" }
            );
            let mut remaining = SNIPPET_CHARS;
            for (index, range) in selections.ranges().iter().enumerate() {
                if remaining == 0 || index == SNIPPET_RANGES {
                    label.push('…');
                    break;
                }
                if cancel.is_cancelled() {
                    return None;
                }
                if index != 0 {
                    label.push(' ');
                    remaining -= 1;
                }
                let start = range.start().0.min(text.len_chars());
                let end = range.end().0.min(text.len_chars());
                let take = (end - start).min(remaining);
                for ch in text.slice(start..start + take).chars() {
                    label.push(if ch.is_control() { ' ' } else { ch });
                }
                remaining -= take;
                if take < end - start {
                    label.push('…');
                    break;
                }
            }
            labels.push(catalog::CatalogEntry {
                entry: Arc::new(Entry {
                    label,
                    value: Location {
                        identity: capture.identity,
                        document: snapshot.id(),
                        revision: snapshot.revision(),
                        selections,
                        line,
                    },
                }),
                accessed: 0, // Keep panes and reverse chronological checkpoints in capture order.
            });
        }
        let labels = Arc::from(labels);
        let _ = self.labels.set(Arc::clone(&labels));
        Some(Ok(labels))
    }
}

pub(crate) struct Job {
    pub session: u64,
    pub revision: u64,
    pub catalog: Arc<Catalog>,
    pub query: String,
    pub cancellation: Cancellation,
}

pub(crate) type Result = catalog::Result<Location>;

impl Job {
    pub fn run(self) -> Option<Result> {
        let catalog = match self.catalog.labels(&self.cancellation)? {
            Ok(catalog) => catalog,
            Err(error) => {
                return Some(Result {
                    session: self.session,
                    revision: self.revision,
                    items: Vec::new(),
                    matched: 0,
                    total: self.catalog.captures.len(),
                    notice: error.to_string(),
                });
            }
        };
        catalog::Job {
            session: self.session,
            revision: self.revision,
            catalog,
            query: self.query,
            cancellation: self.cancellation,
        }
        .run()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vex_core::{CharOffset, Selection};

    #[test]
    fn labels_bound_large_and_many_selections_and_reuse_shared_metadata() {
        let text = "🦀\t界\n".repeat(300_000);
        let document = vex_core::Document::from(text.as_str());
        let length = document.text().len_chars();
        let shared = Arc::new(Document {
            snapshot: document.snapshot(),
            resolver: document.position_resolver(),
            label: "source.rs".into(),
        });
        let large = Arc::new(SelectionSet::single(Selection::new(
            CharOffset(length),
            CharOffset(0),
        )));
        let many = Arc::new(
            SelectionSet::new(
                (0..10_000)
                    .map(|n| Selection::cursor(CharOffset(n * 4)))
                    .collect(),
                9999,
            )
            .unwrap(),
        );
        let catalog = Arc::new(Catalog::new(vec![
            Capture {
                identity: 1,
                bookmark: document.bookmark(),
                document: shared.clone(),
                selections: large.clone(),
                current: true,
            },
            Capture {
                identity: 2,
                bookmark: document.bookmark(),
                document: shared,
                selections: many.clone(),
                current: false,
            },
        ]));
        let job = |query: &str| Job {
            session: 1,
            revision: 1,
            catalog: catalog.clone(),
            query: query.into(),
            cancellation: Cancellation::default(),
        };
        let cancelled = job("");
        cancelled.cancellation.cancel();
        assert!(cancelled.run().is_none());
        assert!(catalog.labels.get().is_none());
        let all = job("").run().unwrap();
        assert_eq!(all.items.len(), 2);
        let first = &all.items[0].entry;
        assert!(first.label.starts_with("source.rs:1 *  🦀 界 "));
        assert!(first.label.ends_with('…'));
        assert!(first.label.len() < 4 * SNIPPET_CHARS + 100);
        assert!(Arc::ptr_eq(&first.value.selections, &large));
        assert!(Arc::ptr_eq(&all.items[1].entry.value.selections, &many));
        assert!(all.items[1].entry.label.len() < 100);
        assert_eq!(all.items[1].entry.value.line, 9999);
        let filtered = job("🦀").run().unwrap();
        assert_eq!(filtered.items.len(), 1);
        assert!(Arc::ptr_eq(first, &filtered.items[0].entry));
        let repeated = job("").run().unwrap();
        assert!(Arc::ptr_eq(first, &repeated.items[0].entry));
    }
}
