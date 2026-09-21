//! Bounded code actions. Opaque payloads live and are destroyed on the service.

use crate::{Document, Range, ServerCommand, protocol, workspace_edit};
use serde_json::{Value, json};
use std::{cmp::Reverse, io, sync::Arc};
use vex_core::{DocumentId, Revision, Selection};
use vex_editor::background::Cancellation;

const MAX_ACTIONS: usize = 256;
const MAX_BYTES: usize = 8 << 20;

#[derive(Clone, Debug)]
pub struct CodeAction {
    pub title: Arc<str>,
    ticket: Arc<()>,
    index: usize,
    priority: u8,
}

#[derive(Debug)]
pub struct CodeActions {
    pub items: Vec<CodeAction>,
    pub limited: bool,
}

#[derive(Debug)]
pub struct ActionEdit {
    pub edit: Option<workspace_edit::WorkspaceEdit>,
    pub command: Option<ServerCommand>,
    pub versions: Vec<workspace_edit::SynchronizedDocument>,
}

/// Frontend handles carry only a title and an unforgeable ticket into this cache.
/// In particular, dismissing a menu cannot drop megabytes of opaque server JSON
/// on the input thread. Replacing the cache invalidates all previous handles.
#[derive(Default)]
pub(crate) struct Catalog {
    ticket: Arc<()>,
    origin: Option<(u64, DocumentId, Revision)>,
    values: Vec<Value>,
}

impl Catalog {
    fn raw(&self, action: &CodeAction, document: &Document) -> Result<&Value, String> {
        if !Arc::ptr_eq(&self.ticket, &action.ticket)
            || self.origin
                != Some((
                    document.epoch,
                    document.snapshot.id(),
                    document.snapshot.revision(),
                ))
        {
            return Err("code action belongs to an obsolete document or server session".into());
        }
        self.values
            .get(action.index)
            .ok_or_else(|| "invalid code action handle".into())
    }

    pub(crate) fn validate(&self, action: &CodeAction, document: &Document) -> Result<(), String> {
        self.raw(action, document).map(|_| ())
    }

    pub(crate) fn needs_resolution(
        &self,
        action: &CodeAction,
        document: &Document,
        capabilities: &Value,
    ) -> Result<bool, String> {
        let raw = self.raw(action, document)?;
        Ok(!raw["command"].is_string()
            && (raw["edit"].is_null() || raw["command"].is_null())
            && capabilities["codeActionProvider"]["resolveProvider"] == true)
    }
    pub(crate) fn params(&self, action: &CodeAction, document: &Document) -> Result<Value, String> {
        Ok(self.raw(action, document)?.clone())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn ready(
        &self,
        action: &CodeAction,
        document: &Document,
        resolved: Option<Value>,
        capabilities: &Value,
        versions: Vec<workspace_edit::SynchronizedDocument>,
        token: &Cancellation,
    ) -> Result<ActionEdit, String> {
        let original = self.raw(action, document)?;
        let merged;
        let raw = if let Some(resolved) = resolved {
            encoded_size(&resolved, MAX_BYTES, token)?;
            let fields = resolved.as_object().ok_or("invalid resolved code action")?;
            if resolved["title"] != original["title"] {
                return Err("code action resolution changed its title".into());
            }
            let mut value = original.as_object().unwrap().clone();
            for (key, field) in fields {
                if token.is_cancelled() {
                    return Err("code action cancelled".into());
                }
                if let Some(original) = value.get(key).filter(|value| !value.is_null()) {
                    if original != field {
                        return Err(format!("code action resolution changed existing {key}"));
                    }
                } else {
                    value.insert(key.clone(), field.clone());
                }
            }
            merged = Value::Object(value);
            encoded_size(&merged, MAX_BYTES, token)?;
            &merged
        } else {
            original
        };
        if !raw["disabled"].is_null() {
            return Err(format!(
                "code action is disabled: {}",
                raw["disabled"]["reason"].as_str().unwrap_or("unavailable")
            ));
        }
        let bare_command = raw["command"].is_string();
        let command = if bare_command {
            Some(raw)
        } else {
            raw.get("command").filter(|value| !value.is_null())
        };
        let command = command
            .map(|value| -> Result<ServerCommand, String> {
                let name = value["command"]
                    .as_str()
                    .ok_or("invalid code action command")?;
                let arguments = match value.get("arguments") {
                    None | Some(Value::Null) => Vec::new(),
                    Some(Value::Array(values)) => values.clone(),
                    _ => return Err("invalid code action command arguments".into()),
                };
                let command = ServerCommand {
                    name: name.into(),
                    arguments: arguments.into(),
                };
                command.validate(capabilities)?;
                Ok(command)
            })
            .transpose()?;
        let edit = (!bare_command)
            .then(|| raw.get("edit").filter(|value| !value.is_null()))
            .flatten()
            .map(|value| workspace_edit::parse(value, token))
            .transpose()?;
        if token.is_cancelled() {
            return Err("code action cancelled".into());
        }
        if edit.is_none() && command.is_none() {
            return Err("code action has no edit or command".into());
        }
        Ok(ActionEdit {
            edit,
            command,
            versions,
        })
    }

    pub(crate) fn replace(
        &mut self,
        value: Value,
        document: &Document,
        token: &Cancellation,
    ) -> Result<CodeActions, String> {
        let values = match value {
            Value::Null => Vec::new(),
            Value::Array(values) => values,
            _ => return Err("invalid code action list".into()),
        };
        let mut next = Self {
            origin: Some((
                document.epoch,
                document.snapshot.id(),
                document.snapshot.revision(),
            )),
            ..Self::default()
        };
        let mut limited = values.len() > MAX_ACTIONS;
        let mut bytes = 0usize;
        let mut items = Vec::new();
        for value in values.into_iter().take(MAX_ACTIONS) {
            if token.is_cancelled() {
                return Err("code actions cancelled".into());
            }
            if !value["disabled"].is_null() {
                continue;
            }
            let Some(title) = value["title"]
                .as_str()
                .filter(|title| !title.trim().is_empty())
            else {
                limited = true;
                continue;
            };
            bytes += encoded_size(&value, MAX_BYTES - bytes, token)?;
            let title: String = title
                .chars()
                .take(256)
                .map(|ch| if ch.is_control() { ' ' } else { ch })
                .collect();
            items.push(CodeAction {
                title: title.into(),
                priority: priority(&value),
                ticket: next.ticket.clone(),
                index: next.values.len(),
            });
            next.values.push(value);
        }
        if token.is_cancelled() {
            return Err("code actions cancelled".into());
        }
        // Stable ties preserve server order, matching Helix's action menu.
        items.sort_by_key(|action| Reverse(action.priority));
        *self = next;
        Ok(CodeActions { items, limited })
    }
}

fn priority(value: &Value) -> u8 {
    if value["command"].is_string() {
        return 0;
    }
    let mut kind = value["kind"].as_str().unwrap_or("").split('.');
    let category = match kind.next() {
        Some("quickfix") => 7,
        Some("refactor") => match kind.next() {
            Some("extract") => 6,
            Some("inline") => 5,
            Some("rewrite") => 4,
            Some("move") => 3,
            Some("surround") => 2,
            _ => 1,
        },
        Some("source") => 1,
        _ => 0,
    };
    category * 4
        + u8::from(
            value["diagnostics"]
                .as_array()
                .is_some_and(|items| !items.is_empty()),
        ) * 2
        + u8::from(value["isPreferred"] == true)
}

fn encoded_size(value: &Value, maximum: usize, token: &Cancellation) -> Result<usize, String> {
    struct Counter<'a> {
        remaining: usize,
        token: &'a Cancellation,
    }
    impl io::Write for Counter<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.token.is_cancelled() {
                return Err(io::Error::other("code action cancelled"));
            }
            self.remaining = self
                .remaining
                .checked_sub(bytes.len())
                .ok_or_else(|| io::Error::other("code action payload exceeds 8 MiB"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter {
        remaining: maximum,
        token,
    };
    serde_json::to_writer(&mut counter, value).map_err(|error| error.to_string())?;
    Ok(maximum - counter.remaining)
}

/// The unmodified diagnostics actually published for this active wire version.
/// Opaque data/code fields are necessary for many servers' fixes.
#[derive(Default)]
pub(crate) struct Diagnostics {
    pub version: i32,
    pub items: Vec<(Range, Value)>,
}
impl Diagnostics {
    pub fn update(
        &mut self,
        values: Value,
        document: &Document,
        positions: &protocol::Positions,
        version: i32,
    ) -> Vec<crate::Diagnostic> {
        self.version = version;
        self.items.clear();
        let Value::Array(values) = values else {
            return Vec::new();
        };
        let mut diagnostics = Vec::new();
        for value in values.into_iter().take(512) {
            let Ok(range) = serde_json::from_value::<Range>(value["range"].clone()) else {
                continue;
            };
            let (Some(start), Some(end), Some(message)) = (
                positions.offset(document.snapshot.text(), range.start),
                positions.offset(document.snapshot.text(), range.end),
                value["message"].as_str(),
            ) else {
                continue;
            };
            if end < start {
                continue;
            }
            let severity = value["severity"]
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(1);
            diagnostics.push(crate::Diagnostic {
                start,
                end,
                line: document.snapshot.text().char_to_line(start.0),
                severity,
                message: message.chars().take(4096).collect(),
            });
            self.items.push((range, value));
        }
        diagnostics
    }
    pub fn params(
        &self,
        uri: &str,
        document: &Document,
        version: i32,
        selection: Selection,
        token: &Cancellation,
    ) -> Result<Value, String> {
        let text = document.snapshot.text();
        let range = Range {
            start: protocol::position(text, selection.start())
                .ok_or("invalid code action selection")?,
            end: protocol::position(text, selection.end())
                .ok_or("invalid code action selection")?,
        };
        let mut bytes = 0usize;
        let mut diagnostics = Vec::new();
        if self.version == version {
            for (other, value) in &self.items {
                // Include an empty diagnostic/selection at a range's boundary.
                if (other.start < range.end && range.start < other.end)
                    || (other.start == other.end
                        && range.start <= other.start
                        && other.start <= range.end)
                    || (range.start == range.end
                        && other.start <= range.start
                        && range.start <= other.end)
                {
                    bytes += encoded_size(value, MAX_BYTES - bytes, token)?;
                    diagnostics.push(value.clone());
                }
            }
        }
        Ok(
            json!({"textDocument":{"uri":uri},"range":range,"context":{"diagnostics":diagnostics,"triggerKind":1}}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vex_core::{CharOffset, Document as TextDocument};
    use vex_editor::Language;

    fn document() -> Document {
        Document {
            epoch: 1,
            restart: 0,
            language: Language::Rust,
            path: "/tmp/action.rs".into(),
            snapshot: TextDocument::from("a🦀b\n").snapshot(),
            saved: 0,
            saved_snapshot: None,
        }
    }
    fn capabilities() -> Value {
        json!({"codeActionProvider":{"resolveProvider":true},"executeCommandProvider":{"commands":["fix"]}})
    }

    #[test]
    fn stable_priority_disabled_filter_limits_and_service_owned_tickets() {
        let document = document();
        let token = Cancellation::default();
        let mut catalog = Catalog::default();
        let items = catalog
            .replace(
                json!([
                    {"title":"command","command":"fix"},
                    {"title":"extract","kind":"refactor.extract"},
                    {"title":"quick","kind":"quickfix"},
                    {"title":"preferred","kind":"quickfix","isPreferred":true},
                    {"title":"diagnostic","kind":"quickfix","diagnostics":[{}]},
                    {"title":"both","kind":"quickfix","diagnostics":[{}],"isPreferred":true},
                    {"title":"tie","kind":"quickfix"},
                    {"title":"disabled","kind":"quickfix","disabled":{"reason":"x"}},
                    {"title":"sanitized\n\u{001b}","command":"fix"},
                    {"command":"fix"}
                ]),
                &document,
                &token,
            )
            .unwrap();
        assert!(items.limited);
        assert_eq!(
            items
                .items
                .iter()
                .map(|item| item.title.as_ref())
                .collect::<Vec<_>>(),
            [
                "both",
                "diagnostic",
                "preferred",
                "quick",
                "tie",
                "extract",
                "command",
                "sanitized  "
            ]
        );
        let first = items.items[0].clone();
        assert!(catalog.validate(&first, &document).is_ok());
        let mut changed = document.clone();
        changed.epoch += 1;
        assert!(catalog.validate(&first, &changed).is_err());
        changed = document.clone();
        changed.snapshot = TextDocument::from("a🦀b\n").snapshot();
        assert!(catalog.validate(&first, &changed).is_err());
        let many = catalog
            .replace(
                json!(vec![json!({"title":"fix","command":"fix"}); 300]),
                &document,
                &token,
            )
            .unwrap();
        assert!(many.limited);
        assert_eq!(many.items.len(), MAX_ACTIONS);
        assert!(catalog.validate(&first, &document).is_err());
        assert!(
            catalog
                .replace(
                    json!([{"title":"huge","data":"x".repeat(MAX_BYTES)}]),
                    &document,
                    &token
                )
                .is_err()
        );
        // A failed replacement does not invalidate a previously usable menu.
        assert!(catalog.validate(&many.items[0], &document).is_ok());
        token.cancel();
        assert!(catalog.replace(Value::Null, &document, &token).is_err());
    }

    #[test]
    fn resolve_preserves_opaque_data_and_existing_fields_and_checks_command_before_edit() {
        let document = document();
        let token = Cancellation::default();
        let caps = capabilities();
        let mut catalog = Catalog::default();
        let value = json!({"title":"Fix","data":{"opaque":[1,{"a":2}]},"kind":"quickfix"});
        let actions = catalog
            .replace(
                json!([value.clone(),{"title":"Command","command":"fix"}]),
                &document,
                &token,
            )
            .unwrap();
        let action = &actions.items[0];
        assert_eq!(catalog.params(action, &document).unwrap(), value);
        assert!(catalog.needs_resolution(action, &document, &caps).unwrap());
        assert!(
            !catalog
                .needs_resolution(&actions.items[1], &document, &caps)
                .unwrap()
        );
        let mut resolved = value.clone();
        resolved["edit"] = json!({"changes":{"file:///tmp/action.rs":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"newText":"b"}]}});
        resolved["command"] =
            json!({"title":"Execute","command":"fix","arguments":[{"preserved":true}]});
        let ready = catalog
            .ready(
                action,
                &document,
                Some(resolved.clone()),
                &caps,
                vec![],
                &token,
            )
            .unwrap();
        assert_eq!(ready.edit.unwrap().documents.len(), 1);
        assert_eq!(
            ready.command.unwrap().arguments[0],
            json!({"preserved":true})
        );
        for field in ["title", "data", "kind"] {
            let mut invalid = resolved.clone();
            invalid[field] = json!("changed");
            assert!(
                catalog
                    .ready(action, &document, Some(invalid), &caps, vec![], &token)
                    .unwrap_err()
                    .contains("changed")
            );
        }
        resolved["command"]["command"] = json!("unadvertised");
        assert!(
            catalog
                .ready(action, &document, Some(resolved), &caps, vec![], &token)
                .unwrap_err()
                .contains("does not advertise")
        );
        assert!(
            catalog
                .ready(action, &document, None, &caps, vec![], &token)
                .unwrap_err()
                .contains("no edit or command")
        );
        let mut disabled = value;
        disabled["disabled"] = json!({"reason":"no longer available"});
        assert!(
            catalog
                .ready(action, &document, Some(disabled), &caps, vec![], &token)
                .unwrap_err()
                .contains("disabled")
        );
        token.cancel();
        assert!(
            catalog
                .ready(&actions.items[1], &document, None, &caps, vec![], &token)
                .is_err()
        );
    }

    #[test]
    fn diagnostic_context_uses_utf16_range_and_preserves_current_overlapping_data() {
        let document = document();
        let token = Cancellation::default();
        let diagnostic = json!({"range":{"start":{"line":0,"character":1},"end":{"line":0,"character":3}},
            "message":"crab","code":23,"data":{"opaque":true}});
        let other = json!({"range":{"start":{"line":0,"character":3},"end":{"line":0,"character":4}},"message":"b"});
        let mut cache = Diagnostics::default();
        let result = cache.update(
            json!([diagnostic.clone(), other]),
            &document,
            &protocol::Positions::new(document.snapshot.text()),
            7,
        );
        assert_eq!(result[0].start, CharOffset(1));
        assert_eq!(result[0].end, CharOffset(2));
        let selected = Selection::new(CharOffset(2), CharOffset(1));
        let params = cache
            .params("file:///tmp/action.rs", &document, 7, selected, &token)
            .unwrap();
        assert_eq!(params["range"], diagnostic["range"]);
        assert_eq!(params["context"]["diagnostics"], json!([diagnostic]));
        assert_eq!(params["context"]["triggerKind"], 1);
        assert_eq!(
            cache.params("uri", &document, 8, selected, &token).unwrap()["context"]["diagnostics"],
            json!([])
        );
        let point = Selection::new(CharOffset(1), CharOffset(1));
        assert_eq!(
            cache.params("uri", &document, 7, point, &token).unwrap()["context"]["diagnostics"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}
