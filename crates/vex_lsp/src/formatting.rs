//! Formatting parameters and bounded edits prepared on the language service.

use crate::{Document, Range, protocol, workspace_edit};
use serde_json::{Value, json};
use vex_core::Selection;
use vex_editor::{IndentStyle, Indentation, background::Cancellation};

pub(crate) fn params(
    uri: &str,
    document: &Document,
    selection: Option<Selection>,
    indentation: Indentation,
) -> Result<Value, String> {
    let tab_size = u32::try_from(indentation.tab_width.get())
        .ok()
        .filter(|width| *width <= i32::MAX as u32)
        .ok_or("formatting tab width exceeds the LSP integer limit")?;
    let mut params = json!({"textDocument":{"uri":uri},"options":{
        "tabSize":tab_size,"insertSpaces":matches!(indentation.style, IndentStyle::Spaces(_))
    }});
    if let Some(selection) = selection {
        let text = document.snapshot.text();
        let range = Range {
            start: protocol::position(text, selection.start())
                .ok_or("invalid formatting selection")?,
            end: protocol::position(text, selection.end()).ok_or("invalid formatting selection")?,
        };
        params["range"] = serde_json::to_value(range).unwrap();
    }
    Ok(params)
}

pub(crate) fn parse(
    value: Value,
    uri: &str,
    version: i32,
    cancellation: &Cancellation,
) -> Result<workspace_edit::WorkspaceEdit, String> {
    if cancellation.is_cancelled() {
        return Err("formatting cancelled".into());
    }
    if value.is_null() {
        return Ok(workspace_edit::WorkspaceEdit::default());
    }
    // Move the reply into a one-document envelope without cloning its strings.
    // Shared decoding enforces the same range, edit-count and text budgets as
    // rename/actions. The worker subsequently checks offsets and overlaps.
    let mut change = json!({"textDocument":{"uri":uri,"version":version}});
    change["edits"] = value;
    let mut envelope = json!({"documentChanges":[]});
    envelope["documentChanges"]
        .as_array_mut()
        .unwrap()
        .push(change);
    workspace_edit::parse(&envelope, cancellation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroUsize;
    use vex_core::{CharOffset, Document as TextDocument};
    use vex_editor::Language;

    fn document() -> Document {
        Document {
            epoch: 1,
            path: "/tmp/format.rs".into(),
            language: Language::Rust,
            snapshot: TextDocument::from("a🦀b\r\nz").snapshot(),
            saved: 0,
            saved_snapshot: None,
        }
    }
    #[test]
    fn range_uses_exact_utf16_selection_and_buffer_options_in_both_directions() {
        let document = document();
        let indentation = Indentation {
            style: IndentStyle::Spaces(NonZeroUsize::new(2).unwrap()),
            tab_width: NonZeroUsize::new(8).unwrap(),
        };
        for (anchor, head) in [(1, 3), (3, 1)] {
            let value = params(
                "uri",
                &document,
                Some(Selection::new(CharOffset(anchor), CharOffset(head))),
                indentation,
            )
            .unwrap();
            assert_eq!(
                value,
                json!({"textDocument":{"uri":"uri"},"options":{"tabSize":8,"insertSpaces":true},
                "range":{"start":{"line":0,"character":1},"end":{"line":0,"character":4}}})
            );
        }
        let tabs = params(
            "uri",
            &document,
            None,
            Indentation {
                style: IndentStyle::Tabs,
                ..indentation
            },
        )
        .unwrap();
        assert!(tabs.get("range").is_none());
        assert_eq!(tabs["options"], json!({"tabSize":8,"insertSpaces":false}));
        assert!(
            params(
                "uri",
                &document,
                Some(Selection::new(CharOffset(0), CharOffset(100))),
                indentation
            )
            .is_err()
        );
        assert!(
            params(
                "uri",
                &document,
                None,
                Indentation {
                    tab_width: NonZeroUsize::new(usize::MAX).unwrap(),
                    ..indentation
                }
            )
            .is_err()
        );
    }

    #[test]
    fn replies_use_the_captured_version_and_shared_atomic_edit_validation() {
        let document = document();
        let token = Cancellation::default();
        let edit = json!({"range":{"start":{"line":0,"character":1},"end":{"line":0,"character":3}},"newText":"crab"});
        let parsed = parse(json!([edit.clone()]), "file:///tmp/format.rs", 8, &token).unwrap();
        assert_eq!(parsed.documents[0].version, Some(8));
        let transaction =
            workspace_edit::transaction(&parsed.documents[0], &document.snapshot, Some(8), &token)
                .unwrap();
        assert_eq!(transaction.edits().len(), 1);
        assert!(
            workspace_edit::transaction(&parsed.documents[0], &document.snapshot, Some(9), &token)
                .is_err()
        );
        let overlapping = parse(
            json!([edit.clone(), edit.clone()]),
            "file:///tmp/format.rs",
            8,
            &token,
        )
        .unwrap();
        assert!(
            workspace_edit::transaction(
                &overlapping.documents[0],
                &document.snapshot,
                Some(8),
                &token
            )
            .is_err()
        );
        let mut invalid = edit.clone();
        invalid["range"]["start"]["character"] = json!(2); // inside the surrogate pair
        let parsed = parse(
            json!([edit.clone(), invalid]),
            "file:///tmp/format.rs",
            8,
            &token,
        )
        .unwrap();
        assert!(
            workspace_edit::transaction(&parsed.documents[0], &document.snapshot, Some(8), &token)
                .is_err()
        );
        for invalid in [
            json!({}),
            json!([{"newText":"missing range"}]),
            json!(vec![edit; 65_537]),
        ] {
            assert!(parse(invalid, "file:///tmp/format.rs", 8, &token).is_err());
        }
        assert!(
            parse(Value::Null, "uri", 8, &token)
                .unwrap()
                .documents
                .is_empty()
        );
        assert!(
            parse(json!([]), "file:///tmp/format.rs", 8, &token)
                .unwrap()
                .documents
                .is_empty()
        );
        token.cancel();
        assert!(parse(Value::Null, "uri", 8, &token).is_err());
    }
}
