//! All-or-nothing decoding and preparation of LSP text workspace edits.
//! Resource operations are deliberately not advertised by this client.

use crate::{
    Range,
    protocol::{Positions, file_path},
};
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use vex_core::{Edit, Snapshot, Transaction};
use vex_editor::background::Cancellation;

const MAX_DOCUMENTS: usize = 4096;
const MAX_EDITS: usize = 65_536;
const MAX_BYTES: usize = 16 << 20;

#[derive(Clone, Debug)]
pub struct TextEdit {
    pub range: Range,
    pub new_text: Arc<str>,
}

#[derive(Clone, Debug)]
pub struct DocumentEdit {
    pub path: PathBuf,
    pub version: Option<i32>,
    pub edits: Vec<TextEdit>,
}

#[derive(Clone, Debug, Default)]
pub struct WorkspaceEdit {
    pub documents: Vec<DocumentEdit>,
}

/// The exact editor snapshot represented by a server's document version.
#[derive(Clone, Debug)]
pub struct SynchronizedDocument {
    pub path: PathBuf,
    pub version: i32,
    pub document: vex_core::DocumentId,
    pub revision: vex_core::Revision,
}

/// Decode either LSP form without silently dropping unsupported changes.
/// Versioned `documentChanges` takes precedence over `changes` as specified.
pub fn parse(value: &Value, cancellation: &Cancellation) -> Result<WorkspaceEdit, String> {
    if value.is_null() {
        return Ok(WorkspaceEdit::default());
    }
    if !value.is_object() {
        return Err("invalid workspace edit".into());
    }
    let mut documents: BTreeMap<PathBuf, DocumentEdit> = BTreeMap::new();
    let (mut count, mut bytes) = (0usize, 0usize);
    let annotations = &value["changeAnnotations"];
    let mut append = |uri: &str, version: Option<i32>, edits: &Value| -> Result<(), String> {
        if cancellation.is_cancelled() {
            return Err("workspace edit cancelled".into());
        }
        let path = file_path(uri).map_err(|e| e.to_string())?;
        let edits = edits.as_array().ok_or("invalid text edit list")?;
        bytes = bytes
            .checked_add(path.as_os_str().len())
            .ok_or("workspace edit size overflow")?;
        if bytes > MAX_BYTES {
            return Err("workspace edit exceeds 16 MiB of text and paths".into());
        }
        let document = documents
            .entry(path.clone())
            .or_insert_with(|| DocumentEdit {
                path,
                version,
                edits: Vec::new(),
            });
        if document.version != version {
            return Err("conflicting workspace document versions".into());
        }
        for edit in edits {
            if cancellation.is_cancelled() {
                return Err("workspace edit cancelled".into());
            }
            count += 1;
            if count > MAX_EDITS {
                return Err("workspace edit exceeds 65536 text edits".into());
            }
            if let Some(annotation) = edit.get("annotationId") {
                let id = annotation.as_str().ok_or("invalid edit annotation")?;
                let annotation = annotations.get(id).ok_or("missing edit annotation")?;
                if annotation["needsConfirmation"] == true {
                    return Err(
                        "workspace edit requires unsupported change-annotation confirmation".into(),
                    );
                }
            }
            let range: Range = serde_json::from_value(edit["range"].clone())
                .map_err(|_| "invalid workspace edit range")?;
            if range.start > range.end {
                return Err("reversed workspace edit range".into());
            }
            let new_text = edit["newText"]
                .as_str()
                .ok_or("missing workspace replacement text")?;
            bytes = bytes
                .checked_add(new_text.len())
                .ok_or("workspace edit size overflow")?;
            if bytes > MAX_BYTES {
                return Err("workspace edit exceeds 16 MiB of text and paths".into());
            }
            document.edits.push(TextEdit {
                range,
                new_text: new_text.into(),
            });
        }
        if documents.len() > MAX_DOCUMENTS {
            return Err("workspace edit exceeds 4096 documents".into());
        }
        Ok(())
    };
    if let Some(changes) = value.get("documentChanges") {
        for change in changes.as_array().ok_or("invalid documentChanges")? {
            if change.get("kind").is_some() {
                return Err("workspace resource operations are not supported".into());
            }
            let document = &change["textDocument"];
            let uri = document["uri"]
                .as_str()
                .ok_or("missing workspace document URI")?;
            let version = match document.get("version") {
                Some(Value::Null) => None,
                Some(version) => Some(
                    version
                        .as_i64()
                        .and_then(|v| v.try_into().ok())
                        .ok_or("invalid workspace document version")?,
                ),
                None => return Err("missing workspace document version".into()),
            };
            append(uri, version, &change["edits"])?;
        }
    } else if let Some(changes) = value.get("changes") {
        for (uri, edits) in changes.as_object().ok_or("invalid workspace changes")? {
            append(uri, None, edits)?;
        }
    }
    Ok(WorkspaceEdit {
        documents: documents
            .into_values()
            .filter(|document| !document.edits.is_empty())
            .collect(),
    })
}

/// Convert every range against the same captured snapshot. Known LSP versions
/// are wire versions, not editor revision numbers. Unknown versions are rejected.
pub fn transaction(
    document: &DocumentEdit,
    snapshot: &Snapshot,
    wire_version: Option<i32>,
    cancellation: &Cancellation,
) -> Result<Transaction, String> {
    if document.version.is_some() && document.version != wire_version {
        return Err("workspace document version changed or is unknown".into());
    }
    if document.edits.len() > MAX_EDITS
        || document
            .edits
            .iter()
            .map(|edit| edit.new_text.len())
            .sum::<usize>()
            > MAX_BYTES
    {
        return Err("workspace text edit limit exceeded".into());
    }
    let positions = Positions::cancellable(snapshot.text(), || cancellation.is_cancelled())
        .ok_or("workspace edit cancelled")?;
    let mut edits = Vec::with_capacity(document.edits.len());
    for (index, edit) in document.edits.iter().enumerate() {
        if cancellation.is_cancelled() {
            return Err("workspace edit cancelled".into());
        }
        if edit.range.start > edit.range.end {
            return Err("reversed workspace edit range".into());
        }
        let start = positions
            .edit_offset(snapshot.text(), edit.range.start)
            .ok_or("invalid workspace edit start")?;
        let end = positions
            .edit_offset(snapshot.text(), edit.range.end)
            .ok_or("invalid workspace edit end")?;
        edits.push((start, end, index, edit.new_text.clone()));
    }
    edits.sort_unstable_by_key(|(start, _, index, _)| (*start, *index));
    let mut combined: Vec<Edit> = Vec::new();
    let mut consumed = vex_core::CharOffset(0);
    let mut edits = edits.into_iter().peekable();
    while let Some((start, mut end, _, text)) = edits.next() {
        if cancellation.is_cancelled() {
            return Err("workspace edit cancelled".into());
        }
        if start < consumed {
            return Err("workspace edits overlap".into());
        }
        let text = if edits.peek().is_some_and(|next| next.0 == start) {
            let mut text = text.to_string();
            while edits.peek().is_some_and(|next| next.0 == start) {
                if cancellation.is_cancelled() {
                    return Err("workspace edit cancelled".into());
                }
                if end != start {
                    return Err("workspace edits overlap at a shared start".into());
                }
                let (_, next_end, _, next_text) = edits.next().unwrap();
                end = next_end;
                text.push_str(&next_text);
            }
            Arc::from(text)
        } else {
            text
        };
        consumed = end;
        // Avoid manufacturing history/revisions for no-op formatter output.
        if snapshot.text().slice(start.0..end.0) != text.as_ref() {
            combined.push(Edit::new(start..end, text));
        }
    }
    snapshot.transaction(combined).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use vex_core::{Document, SelectionSet};

    fn edit(start: u32, end: u32, text: &str) -> Value {
        json!({"range":{"start":{"line":0,"character":start},"end":{"line":0,"character":end}}, "newText":text})
    }

    #[test]
    fn workspace_forms_versions_annotations_and_resource_operations_are_validated_together() {
        let token = Cancellation::default();
        let value = json!({"changes":{"file:///tmp/ignored.rs":[edit(0, 1, "a")]}, "documentChanges":[
            {"textDocument":{"uri":"file:///tmp/one.rs","version":3},"edits":[edit(0, 1, "a")]},
            {"textDocument":{"uri":"file:///tmp/one.rs","version":3},"edits":[edit(3, 4, "b")]},
            {"textDocument":{"uri":"file:///tmp/two.rs","version":null},"edits":[edit(0, 0, "new")]}
        ]});
        let parsed = parse(&value, &token).unwrap();
        assert_eq!(parsed.documents.len(), 2);
        assert_eq!(parsed.documents[0].version, Some(3));
        assert_eq!(parsed.documents[0].edits.len(), 2);
        for invalid in [
            json!({"documentChanges":[value["documentChanges"][0], {"kind":"rename","oldUri":"file:///tmp/a","newUri":"file:///tmp/b"}]}),
            json!({"changes":{"https://example.com/a":[edit(0, 1, "bad")]}}),
            json!({"changes":{"file:///tmp/a":[edit(2, 1, "bad")]}}),
            json!({"documentChanges":[{"textDocument":{"uri":"file:///tmp/a"},"edits":[]}]}),
            json!({"documentChanges":[value["documentChanges"][0], {"textDocument":{"uri":"file:///tmp/one.rs","version":4},"edits":[]}]}),
            json!({"changes":{"file:///tmp/a":[{"range":edit(0, 1, "")["range"],"newText":"x","annotationId":"a"}]},"changeAnnotations":{"a":{"label":"Confirm","needsConfirmation":true}}}),
        ] {
            assert!(parse(&invalid, &token).is_err(), "{invalid}");
        }
        token.cancel();
        assert!(parse(&value, &token).is_err());
    }

    #[test]
    fn utf16_edits_combine_ordered_inserts_and_reject_overlaps_even_when_one_is_a_noop() {
        let mut document = Document::from("a🦀foo\r\n");
        let token = Cancellation::default();
        let decode = |edits| {
            parse(&json!({"changes":{"file:///tmp/a":edits}}), &token)
                .unwrap()
                .documents
                .remove(0)
        };
        let batch = decode(vec![
            edit(3, 3, "one"),
            edit(3, 3, "two"),
            edit(3, 6, "bar"),
        ]);
        let transaction = transaction(&batch, &document.snapshot(), None, &token).unwrap();
        assert_eq!(transaction.edits().len(), 1);
        document
            .apply(transaction, &mut SelectionSet::default())
            .unwrap();
        assert_eq!(document.text(), "a🦀onetwobar\r\n");
        for edits in [
            vec![edit(2, 3, "half surrogate")],
            vec![edit(0, 4, "a🦀o"), edit(3, 5, "overlap")],
            vec![
                edit(3, 5, "replace"),
                edit(3, 3, "insert after replacement"),
            ],
        ] {
            assert!(
                super::transaction(&decode(edits), &document.snapshot(), None, &token).is_err()
            );
        }
        let mut batch = decode(vec![edit(3, 3, "")]);
        batch.version = Some(9);
        assert!(super::transaction(&batch, &document.snapshot(), None, &token).is_err());
        assert!(super::transaction(&batch, &document.snapshot(), Some(8), &token).is_err());
        assert!(
            super::transaction(&batch, &document.snapshot(), Some(9), &token)
                .unwrap()
                .is_empty()
        );
        let overlong = decode(vec![edit(3, u32::MAX, "end")]);
        let transaction =
            super::transaction(&overlong, &document.snapshot(), None, &token).unwrap();
        document
            .apply(transaction, &mut SelectionSet::default())
            .unwrap();
        assert_eq!(document.text(), "a🦀end\r\n");
    }
}
