//! Symbol ranking runs on the same worker as file discovery.

use super::{
    Entry, Item,
    files::MAX_RESULTS,
    fuzzy::{Matcher, Query},
};
use std::{cmp::Reverse, sync::Arc};
use vex_editor::background::Cancellation;
use vex_lsp::{Location, Symbols};

pub(crate) struct SymbolJob {
    pub session: u64,
    pub revision: u64,
    pub symbols: Arc<Symbols>,
    pub query: String,
    pub workspace: bool,
    pub cancellation: Cancellation,
}

pub(crate) struct SymbolResult {
    pub session: u64,
    pub revision: u64,
    pub items: Vec<Item<Location>>,
    pub matched: usize,
    pub total: usize,
    pub limited: bool,
}

impl SymbolJob {
    pub fn run(self) -> Option<SymbolResult> {
        let query = Query::new(&self.query);
        let mut matcher = Matcher::default();
        let mut ranked = Vec::new();
        for (index, symbol) in self.symbols.items.iter().enumerate() {
            if self.cancellation.is_cancelled() {
                return None;
            }
            let name = if symbol.container.is_empty() {
                symbol.name.clone()
            } else {
                format!("{}::{}", symbol.container, symbol.name)
            };
            // Workspace results already match the server's query language.
            // Keep that ordering, including matches beyond plain subsequences.
            if let Some(score) = if self.workspace {
                Some(0)
            } else {
                matcher.score(&name, &query)
            } {
                ranked.push((Reverse(score), index, name));
            }
        }
        ranked.sort_by_key(|(score, index, _)| (*score, *index));
        let matched = ranked.len();
        let mut items = Vec::new();
        for (_, index, name) in ranked.into_iter().take(MAX_RESULTS) {
            if self.cancellation.is_cancelled() {
                return None;
            }
            let symbol = &self.symbols.items[index];
            let matched = matcher.indices(&name, &query);
            let label = if self.workspace {
                format!(
                    "{name}  [{}]  {}:{}",
                    symbol.kind_name(),
                    crate::paths::display(&symbol.location.path),
                    u64::from(symbol.location.position.line) + 1
                )
            } else {
                format!(
                    "{name}  [{}]  :{}",
                    symbol.kind_name(),
                    u64::from(symbol.location.position.line) + 1
                )
            };
            items.push(Item {
                entry: Arc::new(Entry {
                    label,
                    value: symbol.location.clone(),
                }),
                matched,
            });
        }
        Some(SymbolResult {
            session: self.session,
            revision: self.revision,
            items,
            matched,
            total: self.symbols.items.len(),
            limited: self.symbols.limited,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vex_lsp::{Position, Symbol};

    fn job(workspace: bool, query: &str) -> SymbolJob {
        SymbolJob {
            session: 1,
            revision: 2,
            query: query.into(),
            workspace,
            cancellation: Cancellation::default(),
            symbols: Arc::new(Symbols {
                items: (0..600)
                    .map(|index| Symbol {
                        name: format!("method{index}"),
                        container: "Type".into(),
                        kind: 6,
                        location: Location {
                            path: "/file.rs".into(),
                            position: Position {
                                line: index,
                                character: 0,
                            },
                        },
                    })
                    .collect(),
                limited: false,
            }),
        }
    }

    #[test]
    fn document_symbols_rank_names_and_containers_and_workspace_preserves_server_matches() {
        let all = job(false, "").run().unwrap();
        assert_eq!(all.matched, 600);
        assert_eq!(all.items.len(), MAX_RESULTS);
        let filtered = job(false, "Type m599").run().unwrap();
        assert_eq!(filtered.items.len(), 1);
        assert_eq!(filtered.items[0].entry.value.position.line, 599);
        assert!(!filtered.items[0].matched.is_empty());
        let workspace = job(true, "server-specific#query").run().unwrap();
        assert_eq!(workspace.items.len(), MAX_RESULTS);
        assert_eq!(workspace.items[0].entry.value.position.line, 0);
        let cancelled = job(false, "m");
        cancelled.cancellation.cancel();
        assert!(cancelled.run().is_none());
    }
}
