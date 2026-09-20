//! Diagnostic labels and ranking live on the picker worker. The UI retains only
//! the bounded visible result set and a cheap service catalog handle.

use super::{Entry, catalog, search::OpenDocument};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use vex_core::{DocumentId, Revision};
use vex_editor::background::Cancellation;
use vex_lsp::{Range, diagnostics::Catalog};

#[derive(Clone, Debug)]
pub(crate) struct Hit {
    pub path: Arc<PathBuf>,
    pub range: Range,
    pub version: Option<(DocumentId, Revision)>,
    pub message: Arc<str>,
    severity: u32,
}

impl PartialEq for Hit {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path && self.range == other.range && self.message == other.message
    }
}
impl Eq for Hit {}

pub(crate) struct Job {
    pub session: u64,
    pub revision: u64,
    pub catalog: Catalog,
    pub path: Option<PathBuf>,
    pub cwd: PathBuf,
    pub documents: Arc<[OpenDocument]>,
    pub query: String,
    pub cancellation: Cancellation,
}

pub(crate) struct Result {
    pub ranked: catalog::Result<Hit>,
    pub generation: u64,
}

struct Cache {
    catalog: Catalog,
    generation: u64,
    path: Option<PathBuf>,
    cwd: PathBuf,
    documents: Arc<[OpenDocument]>,
    entries: Arc<[catalog::CatalogEntry<Hit>]>,
    notice: String,
}

#[derive(Default)]
pub(crate) struct Worker(Option<Cache>);

impl Worker {
    pub fn run(&mut self, job: Job) -> Option<Result> {
        if job.cancellation.is_cancelled() {
            return None;
        }
        if !self.0.as_ref().is_some_and(|cache| {
            cache.catalog.same_catalog(&job.catalog)
                && cache.generation == job.catalog.generation()
                && cache.path == job.path
                && cache.cwd == job.cwd
                && Arc::ptr_eq(&cache.documents, &job.documents)
        }) {
            let snapshot = job.catalog.snapshot();
            let open: BTreeMap<_, _> = job
                .documents
                .iter()
                .map(|doc| (&doc.path, &doc.snapshot))
                .collect();
            let mut entries = Vec::new();
            let mut limited = snapshot.limited;
            let mut stale = 0;
            for file in snapshot.files {
                if job.cancellation.is_cancelled() {
                    return None;
                }
                // Servers can publish a symlink spelling of an already-open
                // file (including /var versus /private/var on macOS). Resolve
                // on this worker before checking unsaved buffer revisions.
                let path = crate::files::resolve(&file.path).unwrap_or_else(|_| file.path.clone());
                if job.path.as_ref().is_some_and(|scope| scope != &path) {
                    continue;
                }
                let current = open.get(&path);
                let valid = match (file.version, current) {
                    (Some((id, revision)), Some(current)) => {
                        id == current.id() && revision == current.revision()
                    }
                    (Some(_), None) => false,
                    (None, Some(current)) => current.revision().get() == 0,
                    (None, None) => true,
                };
                if !valid {
                    stale += 1;
                    continue;
                }
                let version = current.map(|current| (current.id(), current.revision()));
                let path = Arc::new(path);
                limited |= file.limited;
                for diagnostic in &file.entries {
                    if job.cancellation.is_cancelled() {
                        return None;
                    }
                    let severity = match diagnostic.severity {
                        1 => "ERROR",
                        2 => "WARN",
                        3 => "INFO",
                        4 => "HINT",
                        _ => "",
                    };
                    let location = if job.path.is_some() {
                        format!(
                            "{}:{}",
                            u64::from(diagnostic.range.start.line) + 1,
                            u64::from(diagnostic.range.start.character) + 1
                        )
                    } else {
                        format!(
                            "{}:{}:{}",
                            path.strip_prefix(&job.cwd).unwrap_or(&path).display(),
                            u64::from(diagnostic.range.start.line) + 1,
                            u64::from(diagnostic.range.start.character) + 1
                        )
                    };
                    let label = format!(
                        "{severity} {} {} {location} {}",
                        diagnostic.source, diagnostic.code, diagnostic.message
                    )
                    .chars()
                    .map(|ch| if ch.is_control() { ' ' } else { ch })
                    .collect();
                    entries.push(catalog::CatalogEntry {
                        entry: Arc::new(Entry {
                            label,
                            value: Hit {
                                path: path.clone(),
                                range: diagnostic.range,
                                version,
                                message: diagnostic.message.clone(),
                                severity: diagnostic.severity,
                            },
                        }),
                        accessed: 0,
                    });
                }
            }
            entries.sort_by_key(|entry| match entry.entry.value.severity {
                0 => 4,
                other => other,
            });
            let mut notices = Vec::new();
            if limited {
                notices.push("Diagnostic limit reached".to_owned());
            }
            if stale != 0 {
                notices.push(format!(
                    "{stale} stale files omitted; awaiting current diagnostics"
                ));
            }
            self.0 = Some(Cache {
                catalog: job.catalog.clone(),
                generation: snapshot.generation,
                path: job.path,
                cwd: job.cwd,
                documents: job.documents,
                entries: entries.into(),
                notice: notices.join(" · "),
            });
        }
        let cache = self.0.as_ref()?;
        let mut ranked = catalog::Job {
            session: job.session,
            revision: job.revision,
            catalog: cache.entries.clone(),
            query: job.query,
            cancellation: job.cancellation,
        }
        .run()?;
        ranked.notice = cache.notice.clone();
        Some(Result {
            ranked,
            generation: cache.generation,
        })
    }
}
