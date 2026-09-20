//! Bounded, shared diagnostics. JSON decoding and catalog reads belong to workers;
//! the frontend only clones a handle and reads its atomic generation.

use crate::{Range, file_path};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use vex_core::{DocumentId, Revision};

const MAX_FILES: usize = 4096;
const MAX_ITEMS: usize = 65_536;
const MAX_BYTES: usize = 16 << 20;
const MAX_FILE_ITEMS: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub range: Range,
    /// 1 error, 2 warning, 3 information, 4 hint; 0 unspecified.
    pub severity: u32,
    pub message: Arc<str>,
    pub source: Arc<str>,
    pub code: Arc<str>,
}

#[derive(Debug)]
pub struct File {
    pub path: PathBuf,
    pub entries: Vec<Entry>,
    pub version: Option<(DocumentId, Revision)>,
    pub limited: bool,
}

#[derive(Debug, Default)]
struct State {
    files: BTreeMap<PathBuf, Arc<File>>,
    bytes: usize,
    items: usize,
    limited: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Catalog {
    state: Arc<Mutex<State>>,
    generation: Arc<AtomicU64>,
}

pub struct Snapshot {
    pub generation: u64,
    pub files: Vec<Arc<File>>,
    pub limited: bool,
}

impl Catalog {
    pub fn same_catalog(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Call on a worker: the short critical section copies shared file handles,
    /// never message strings, and does not include sorting or fuzzy matching.
    pub fn snapshot(&self) -> Snapshot {
        let state = self.state.lock().unwrap();
        Snapshot {
            generation: self.generation(),
            files: state.files.values().cloned().collect(),
            limited: state.limited,
        }
    }

    pub(crate) fn replace(&self, mut file: File) {
        let mut state = self.state.lock().unwrap();
        if let Some(old) = state.files.remove(&file.path) {
            state.items -= old.entries.len();
            state.bytes -= size(&old);
        }
        if state.files.len() == MAX_FILES {
            state.limited = true;
        } else if !file.entries.is_empty() || file.limited {
            // Keep an explicit empty/limited record when a replacement cannot
            // fit, rather than retaining its obsolete diagnostics.
            let mut bytes = size(&file);
            while state.items + file.entries.len() > MAX_ITEMS || state.bytes + bytes > MAX_BYTES {
                let Some(entry) = file.entries.pop() else {
                    break;
                };
                bytes -= entry_size(&entry);
                file.limited = true;
            }
            if state.bytes + bytes <= MAX_BYTES {
                file.entries.shrink_to_fit();
                state.bytes += bytes;
                state.items += file.entries.len();
                state.files.insert(file.path.clone(), Arc::new(file));
            } else {
                state.limited = true;
            }
        }
        self.generation.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn reset_limited(&self) {
        let mut state = self.state.lock().unwrap();
        *state = State {
            limited: true,
            ..State::default()
        };
        self.generation.fetch_add(1, Ordering::Release);
    }
}

fn size(file: &File) -> usize {
    file.path.as_os_str().len() * 2
        + std::mem::size_of::<File>()
        + file.entries.iter().map(entry_size).sum::<usize>()
}

fn entry_size(entry: &Entry) -> usize {
    std::mem::size_of::<Entry>() + entry.message.len() + entry.source.len() + entry.code.len()
}

pub(crate) struct Publication {
    pub file: File,
    pub version: Option<i64>,
    // Only the active document needs opaque code-action context. Other files
    // retain typed, bounded fields, never arbitrary diagnostic data.
    pub raw: Option<Value>,
}

impl Publication {
    pub fn decode(mut value: Value, active_uri: &str) -> Option<Self> {
        let params = &mut value["params"];
        let uri = params["uri"].as_str()?;
        if uri.len() > 16_384 {
            return None;
        }
        let path = file_path(uri).ok()?;
        let active = uri == active_uri;
        let version = match &params["version"] {
            Value::Null => None,
            value => Some(value.as_i64()?),
        };
        let mut values = params["diagnostics"].take();
        let values_array = values.as_array_mut()?;
        let mut limited = values_array.len() > MAX_FILE_ITEMS;
        values_array.truncate(MAX_FILE_ITEMS);
        let mut entries = Vec::with_capacity(values_array.len());
        for value in values_array.iter() {
            let (Ok(range), Some(message)) = (
                serde_json::from_value::<Range>(value["range"].clone()),
                value["message"].as_str(),
            ) else {
                limited = true;
                continue;
            };
            if range.end < range.start {
                limited = true;
                continue;
            }
            let mut text = |text: &str, limit: usize| -> Arc<str> {
                let mut end = text.len().min(limit);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                limited |= end < text.len();
                text[..end].into()
            };
            entries.push(Entry {
                range,
                severity: value["severity"]
                    .as_u64()
                    .filter(|n| (1..=4).contains(n))
                    .unwrap_or(0) as u32,
                message: text(message, 4096),
                source: text(value["source"].as_str().unwrap_or(""), 256),
                code: match &value["code"] {
                    Value::String(code) => text(code, 256),
                    Value::Number(code) => code.to_string().into(),
                    _ => "".into(),
                },
            });
        }
        Some(Self {
            file: File {
                path,
                entries: {
                    entries.shrink_to_fit();
                    entries
                },
                limited,
                version: None,
            },
            version,
            raw: active.then_some(values),
        })
    }

    fn bytes(&self) -> usize {
        // Charging the full framing limit bounds opaque JSON without a second
        // serialization pass. At most one active URI is present per session.
        size(&self.file)
            + if self.raw.is_some() {
                crate::protocol::MAX_MESSAGE
            } else {
                0
            }
    }
}

/// Coalesce per file before the service processes a burst. Overflow explicitly
/// invalidates the prior catalog, preventing dropped clears from leaving stale
/// entries behind. It never fails the server or blocks UI input.
#[derive(Default)]
pub(crate) struct Pending {
    entries: BTreeMap<PathBuf, Publication>,
    bytes: usize,
    items: usize,
    pub reset: bool,
}

// Dropped by the transport callback after releasing the inbox lock. Opaque
// diagnostic JSON can otherwise make a coalesced replacement block UI updates.
pub(crate) struct Retired {
    _one: Option<Publication>,
    _batch: BTreeMap<PathBuf, Publication>,
}

impl Pending {
    pub fn push(&mut self, publication: Publication) -> Retired {
        if self.entries.get(&publication.file.path).is_some_and(
            |old| matches!((old.version, publication.version), (Some(old), Some(new)) if old > new),
        ) {
            return Retired {
                _one: Some(publication),
                _batch: BTreeMap::new(),
            };
        }
        let old = self.entries.remove(&publication.file.path);
        if let Some(old) = &old {
            self.bytes -= old.bytes();
            self.items -= old.file.entries.len();
        }
        let mut retired = Retired {
            _one: old,
            _batch: BTreeMap::new(),
        };
        if self.entries.len() == MAX_FILES
            || self.bytes + publication.bytes() > MAX_BYTES + crate::protocol::MAX_MESSAGE
            || self.items + publication.file.entries.len() > MAX_ITEMS
        {
            retired._batch = std::mem::take(&mut self.entries);
            self.bytes = 0;
            self.items = 0;
            self.reset = true;
        }
        self.bytes += publication.bytes();
        self.items += publication.file.entries.len();
        self.entries
            .insert(publication.file.path.clone(), publication);
        retired
    }

    pub fn pop(&mut self) -> Option<Publication> {
        let (_, publication) = self.entries.pop_first()?;
        self.bytes -= publication.bytes();
        self.items -= publication.file.entries.len();
        Some(publication)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn publication(uri: &str, version: i64, message: &str) -> Publication {
        Publication::decode(json!({"params": {"uri":uri,"version":version,"diagnostics":[{
            "range":{"start":{"line":2,"character":1},"end":{"line":2,"character":4}},
            "message":message,"severity":2,"source":"rustc","code":"E0001","data":{"opaque":true}
        }]}}), "file:///active.rs").unwrap()
    }

    #[test]
    fn coalescing_preserves_newest_versions_clears_and_opaque_active_context() {
        let mut pending = Pending::default();
        pending.push(publication("file:///active.rs", 2, "new"));
        pending.push(publication("file:///active.rs", 1, "old"));
        pending.push(publication("file:///hidden.rs", 5, "hidden"));
        let active = pending.pop().unwrap();
        assert_eq!(&*active.file.entries[0].message, "new");
        assert_eq!(active.raw.unwrap()[0]["data"]["opaque"], true);
        let hidden = pending.pop().unwrap();
        assert!(hidden.raw.is_none());
        let catalog = Catalog::default();
        catalog.replace(hidden.file);
        let before = catalog.snapshot();
        let mut empty = publication("file:///hidden.rs", 6, "");
        empty.file.entries.clear();
        catalog.replace(empty.file);
        assert!(catalog.snapshot().files.is_empty());
        assert!(catalog.generation() > before.generation);
        assert_eq!(&*before.files[0].entries[0].message, "hidden");
    }

    #[test]
    fn catalog_and_pending_limits_are_explicit_and_replacements_do_not_keep_old_entries() {
        let catalog = Catalog::default();
        let mut pending = Pending::default();
        for n in 0..=MAX_FILES {
            let p = publication(&format!("file:///f{n}.rs"), 1, "error");
            pending.push(publication(&format!("file:///f{n}.rs"), 1, "error"));
            catalog.replace(p.file);
        }
        assert!(pending.reset);
        assert_eq!(pending.entries.len(), 1);
        let snapshot = catalog.snapshot();
        assert!(snapshot.limited);
        assert_eq!(snapshot.files.len(), MAX_FILES);
        catalog.reset_limited();
        assert!(catalog.snapshot().files.is_empty());
        let mut p = publication("file:///huge.rs", 1, &"🦀".repeat(3000));
        assert!(p.file.limited);
        assert_eq!(p.file.entries[0].message.len(), 4096);
        p.file.entries = vec![p.file.entries[0].clone(); MAX_ITEMS + 1];
        catalog.replace(p.file);
        let limited = catalog.snapshot();
        assert!(limited.files[0].limited);
        assert!(size(&limited.files[0]) <= MAX_BYTES);
    }
}
