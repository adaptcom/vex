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
    pub(crate) fn for_document(
        &self,
        document: &crate::Document,
        positions: &crate::protocol::Positions,
    ) -> Option<Vec<crate::Diagnostic>> {
        let file = self
            .state
            .lock()
            .unwrap()
            .files
            .get(&document.path)
            .cloned();
        file.filter(|file| {
            file.version == Some((document.snapshot.id(), document.snapshot.revision()))
        })
        .map(|file| {
            file.entries
                .iter()
                .filter_map(|entry| {
                    let start = positions.offset(document.snapshot.text(), entry.range.start)?;
                    let end = positions.offset(document.snapshot.text(), entry.range.end)?;
                    Some(crate::Diagnostic {
                        start,
                        end,
                        line: document.snapshot.text().char_to_line(start.0),
                        severity: entry.severity,
                        message: entry.message.to_string(),
                    })
                })
                .collect::<Vec<_>>()
        })
    }
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
    // Bounded opaque code-action context can survive a focus change. The public
    // catalog only retains typed fields; each session owns its private cache.
    pub raw: Option<Value>,
    raw_bytes: usize,
}

impl Publication {
    pub fn decode(mut value: Value) -> Option<Self> {
        let params = &mut value["params"];
        let uri = params["uri"].as_str()?;
        if uri.len() > 16_384 {
            return None;
        }
        let path = file_path(uri).ok()?;
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
        let raw_bytes = encoded_size(&values).unwrap_or(0);
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
            raw: (raw_bytes > 0).then_some(values),
            raw_bytes,
        })
    }

    fn bytes(&self) -> usize {
        size(&self.file) + self.raw_bytes
    }
}

const MAX_CONTEXT_BYTES: usize = 8 << 20;

fn encoded_size(value: &Value) -> Option<usize> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            if self.0 > MAX_CONTEXT_BYTES {
                return Err(std::io::Error::other("diagnostic context limit"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value).ok()?;
    Some(counter.0)
}

type ContextVersion = (i32, DocumentId, Revision);
struct ContextEntry {
    version: ContextVersion,
    value: Value,
    bytes: usize,
    used: u64,
}

/// Retain bounded opaque diagnostics for code actions after focus changes.
/// The UI never clones or drops these server-owned payloads.
#[derive(Default)]
pub(crate) struct ContextCache {
    files: BTreeMap<PathBuf, ContextEntry>,
    bytes: usize,
    sequence: u64,
}

impl ContextCache {
    pub fn insert(&mut self, path: PathBuf, version: ContextVersion, value: Value) {
        if let Some(old) = self.files.remove(&path) {
            self.bytes -= old.bytes;
        }
        let Some(bytes) = encoded_size(&value) else {
            return;
        };
        while self.bytes + bytes > MAX_CONTEXT_BYTES || self.files.len() >= 512 {
            let path = self
                .files
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .unwrap()
                .0
                .clone();
            self.bytes -= self.files.remove(&path).unwrap().bytes;
        }
        self.sequence += 1;
        self.bytes += bytes;
        self.files.insert(
            path,
            ContextEntry {
                version,
                value,
                bytes,
                used: self.sequence,
            },
        );
    }

    pub fn get(&mut self, document: &crate::Document, version: i32) -> Option<Value> {
        let entry = self.files.get_mut(&document.path)?;
        if entry.version
            != (
                version,
                document.snapshot.id(),
                document.snapshot.revision(),
            )
        {
            return None;
        }
        self.sequence += 1;
        entry.used = self.sequence;
        Some(entry.value.clone())
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
        }]}})).unwrap()
    }

    #[test]
    fn coalescing_preserves_newest_versions_clears_and_opaque_context() {
        let mut pending = Pending::default();
        pending.push(publication("file:///active.rs", 2, "new"));
        pending.push(publication("file:///active.rs", 1, "old"));
        pending.push(publication("file:///hidden.rs", 5, "hidden"));
        let active = pending.pop().unwrap();
        assert_eq!(&*active.file.entries[0].message, "new");
        assert_eq!(active.raw.unwrap()[0]["data"]["opaque"], true);
        let hidden = pending.pop().unwrap();
        assert_eq!(hidden.raw.as_ref().unwrap()[0]["data"]["opaque"], true);
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

    #[test]
    fn cached_code_action_context_is_bounded_and_requires_the_current_document_and_wire_version() {
        let text = vex_core::Document::from("hello");
        let mut document = crate::Document {
            epoch: 1,
            restart: 0,
            language: vex_editor::Language::Rust,
            path: PathBuf::from("file-0.rs"),
            snapshot: text.snapshot(),
            saved: 0,
            saved_snapshot: None,
        };
        let mut cache = ContextCache::default();
        let version = (7, text.id(), text.revision());
        for index in 0..513 {
            cache.insert(
                PathBuf::from(format!("file-{index}.rs")),
                version,
                json!([{"data":index}]),
            );
        }
        assert_eq!(cache.files.len(), 512);
        assert!(cache.get(&document, 7).is_none());
        document.path = PathBuf::from("file-512.rs");
        assert_eq!(cache.get(&document, 7).unwrap()[0]["data"], 512);
        assert!(cache.get(&document, 8).is_none());
        document.snapshot = vex_core::Document::from("hello").snapshot();
        assert!(cache.get(&document, 7).is_none());
        cache.insert(
            document.path.clone(),
            version,
            json!("x".repeat(MAX_CONTEXT_BYTES + 1)),
        );
        assert!(!cache.files.contains_key(&document.path));
        assert!(cache.bytes <= MAX_CONTEXT_BYTES);
    }
}
