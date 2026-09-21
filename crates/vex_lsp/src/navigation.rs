//! Bounded, cancellable navigation decoding on the language-service thread.

use crate::{
    Location, Range,
    protocol::{Positions, file_path},
};
use serde_json::Value;
use std::{collections::BTreeSet, path::PathBuf};
use vex_core::{CharOffset, Selection, SelectionSet, Snapshot};
use vex_editor::{Mode, PreparedSelections, background::Cancellation};

const MAX_LOCATIONS: usize = 65_536;
const MAX_PATH_BYTES: usize = 8 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Navigation {
    Definition,
    TypeDefinition,
    Implementation,
    References,
}

impl Navigation {
    pub fn title(self) -> &'static str {
        match self {
            Self::Definition => "Definitions",
            Self::TypeDefinition => "Type definitions",
            Self::Implementation => "Implementations",
            Self::References => "References",
        }
    }

    pub(crate) fn request(self) -> (&'static str, &'static str) {
        match self {
            Self::Definition => ("textDocument/definition", "definitionProvider"),
            Self::TypeDefinition => ("textDocument/typeDefinition", "typeDefinitionProvider"),
            Self::Implementation => ("textDocument/implementation", "implementationProvider"),
            Self::References => ("textDocument/references", "referencesProvider"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Destination {
    pub path: PathBuf,
    pub range: Range,
}

impl Destination {
    pub fn location(&self) -> Location {
        Location {
            path: self.path.clone(),
            position: self.range.start,
        }
    }
}

#[derive(Debug, Default)]
pub struct Locations {
    pub items: Vec<Destination>,
    pub limited: bool,
    pub skipped: usize,
}

fn range(value: &Value) -> Option<Range> {
    let range: Range = serde_json::from_value(value.clone()).ok()?;
    (range.start <= range.end).then_some(range)
}

/// Accept scalar locations, arrays of locations/links, and null. Unsupported
/// URI schemes and malformed entries are counted rather than hiding valid hits.
pub(crate) fn locations(value: &Value, cancellation: &Cancellation) -> Result<Locations, String> {
    let values = if value.is_null() {
        &[]
    } else if let Some(values) = value.as_array() {
        values.as_slice()
    } else if value.is_object() {
        std::slice::from_ref(value)
    } else {
        return Err("invalid location response".into());
    };
    let mut result = Locations::default();
    let mut seen = BTreeSet::new();
    let mut bytes = 0;
    for (index, value) in values.iter().enumerate() {
        if cancellation.is_cancelled() {
            return Err("request cancelled".into());
        }
        if index == MAX_LOCATIONS {
            result.limited = true;
            break;
        }
        let destination = (|| {
            let uri = value
                .get("uri")
                .or_else(|| value.get("targetUri"))?
                .as_str()?;
            // LocationLink separates the declaration's full extent from the
            // symbol to select. Plain Location replies only provide `range`.
            let range = range(
                value
                    .get("targetSelectionRange")
                    .or_else(|| value.get("targetRange"))
                    .or_else(|| value.get("range"))?,
            )?;
            Some(Destination {
                path: file_path(uri).ok()?,
                range,
            })
        })();
        let Some(destination) = destination else {
            result.skipped += 1;
            continue;
        };
        bytes += destination.path.as_os_str().len();
        if bytes > MAX_PATH_BYTES {
            result.limited = true;
            break;
        }
        if seen.insert((
            destination.path.clone(),
            destination.range.start,
            destination.range.end,
        )) {
            result.items.push(destination);
        }
    }
    Ok(result)
}

/// Document highlights, unlike workspace references, refer only to this
/// snapshot. Preserve the primary occurrence when results arrive out of order.
pub(crate) fn highlights(
    value: &Value,
    snapshot: &Snapshot,
    positions: &Positions,
    cursor: CharOffset,
    cancellation: &Cancellation,
) -> Result<Option<PreparedSelections>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let values = value
        .as_array()
        .ok_or("invalid document highlight response")?;
    if values.len() > MAX_LOCATIONS {
        // Never silently edit only a prefix of a symbol's occurrences.
        return Err("too many document highlights (65536 limit)".into());
    }
    let mut selections = Vec::new();
    let mut primary = 0;
    for value in values {
        if cancellation.is_cancelled() {
            return Err("request cancelled".into());
        }
        let Some(range) = range(&value["range"]) else {
            continue;
        };
        let Some(start) = positions.offset(snapshot.text(), range.start) else {
            continue;
        };
        let Some(end) = positions.offset(snapshot.text(), range.end) else {
            continue;
        };
        if start <= cursor && cursor < end {
            primary = selections.len();
        }
        selections.push(Selection::new(start, end));
    }
    if selections.is_empty() {
        return Ok(None);
    }
    let selections = SelectionSet::new(selections, primary).map_err(|e| e.to_string())?;
    PreparedSelections::new(snapshot, selections, Mode::Normal, cancellation)
        .map(Some)
        .map_err(|e| e.to_string())
}

/// Resolve one returned range against a shared snapshot on a worker. The head
/// sits at the start of the symbol, matching Helix's navigation selections.
pub fn destination_selection(
    snapshot: &Snapshot,
    range: Range,
    cancellation: &Cancellation,
) -> Result<PreparedSelections, String> {
    if cancellation.is_cancelled() {
        return Err("request cancelled".into());
    }
    if range.start > range.end {
        return Err("invalid destination range".into());
    }
    let positions = Positions::cancellable(snapshot.text(), || cancellation.is_cancelled())
        .ok_or("request cancelled")?;
    let start = positions
        .offset(snapshot.text(), range.start)
        .ok_or("invalid destination start")?;
    let end = positions
        .offset(snapshot.text(), range.end)
        .ok_or("invalid destination end")?;
    PreparedSelections::new(
        snapshot,
        SelectionSet::single(Selection::new(end, start)),
        Mode::Normal,
        cancellation,
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Position;
    use serde_json::json;
    use vex_core::Document;
    use vex_editor::Editor;

    fn span(start: u32, end: u32) -> Value {
        json!({"start":{"line":0,"character":start},"end":{"line":0,"character":end}})
    }

    #[test]
    fn locations_prefer_symbol_ranges_deduplicate_and_skip_invalid_entries() {
        let a = json!({"uri":"file:///tmp/a.rs", "range":span(2, 4)});
        let link = json!({"targetUri":"file:///tmp/b.rs", "targetRange":span(0, 9), "targetSelectionRange":span(3, 4)});
        let result = locations(&json!([a, link, a, {"uri":"https://example.com/a", "range":span(0, 1)}, {"uri":"file:///tmp/a.rs","range":span(3, 1)}]), &Cancellation::default()).unwrap();
        assert_eq!(result.items.len(), 2);
        assert_eq!(
            result.items[0].range,
            serde_json::from_value(span(2, 4)).unwrap()
        );
        assert_eq!(
            result.items[1].range,
            serde_json::from_value(span(3, 4)).unwrap()
        );
        let fallback = locations(
            &json!({"targetUri":"file:///tmp/b.rs", "targetRange":span(0, 9)}),
            &Cancellation::default(),
        )
        .unwrap();
        assert_eq!(
            fallback.items[0].range,
            serde_json::from_value(span(0, 9)).unwrap()
        );
        assert_eq!(result.skipped, 2);
        assert_eq!(
            locations(&a, &Cancellation::default()).unwrap().items.len(),
            1
        );
        assert!(
            locations(&Value::Null, &Cancellation::default())
                .unwrap()
                .items
                .is_empty()
        );
        assert!(locations(&json!(3), &Cancellation::default()).is_err());
        let cancelled = Cancellation::default();
        cancelled.cancel();
        assert!(locations(&json!([a]), &cancelled).is_err());
        let result = locations(
            &Value::Array(vec![a; MAX_LOCATIONS + 1]),
            &Cancellation::default(),
        )
        .unwrap();
        assert!(result.limited);
        assert_eq!(result.items.len(), 1);
    }

    #[test]
    fn definition_links_select_the_enum_name_instead_of_its_declaration() {
        let mut editor = Editor::new(Document::from(
            "/// 🦀\npub enum Highlight {\n    Keyword,\n    Type,\n}\n",
        ));
        let cancellation = Cancellation::default();
        let result = locations(
            &json!([{
                "targetUri": "file:///tmp/highlight.rs",
                "targetRange": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 4, "character": 1}
                },
                "targetSelectionRange": {
                    "start": {"line": 1, "character": 9},
                    "end": {"line": 1, "character": 18}
                }
            }]),
            &cancellation,
        )
        .unwrap();
        let selection = destination_selection(
            &editor.document().snapshot(),
            result.items[0].range,
            &cancellation,
        )
        .unwrap();
        assert!(editor.apply_prepared_selections(selection));
        let selection = editor.selections().primary();
        assert_eq!(selection, Selection::new(CharOffset(24), CharOffset(15)));
        assert_eq!(
            editor
                .document()
                .text()
                .slice(selection.start().0..selection.end().0)
                .to_string(),
            "Highlight",
        );
    }

    #[test]
    fn highlights_handle_utf16_graphemes_primary_order_duplicates_and_empty_replies() {
        let mut editor = Editor::new(Document::from("🦀foo e\u{301} foo\r\n"));
        let snapshot = editor.document().snapshot();
        let positions = Positions::new(snapshot.text());
        let result = highlights(
            &json!([
                {"range":span(9, 12)}, {"range":span(2, 5)}, {"range":span(2, 5)},
                {"range":span(7, 8)}, {"range":span(8, 7)}
            ]),
            &snapshot,
            &positions,
            CharOffset(2),
            &Cancellation::default(),
        )
        .unwrap()
        .unwrap();
        assert!(editor.apply_prepared_selections(result));
        assert_eq!(editor.selections().ranges().len(), 3);
        assert_eq!(
            editor.selections().primary(),
            Selection::new(CharOffset(1), CharOffset(4))
        );
        assert_eq!(
            editor.selections().ranges()[1],
            Selection::new(CharOffset(5), CharOffset(7))
        );
        assert!(
            highlights(
                &json!([]),
                &snapshot,
                &positions,
                CharOffset(0),
                &Cancellation::default()
            )
            .unwrap()
            .is_none()
        );
        assert!(
            highlights(
                &Value::Array(vec![json!({}); MAX_LOCATIONS + 1]),
                &snapshot,
                &positions,
                CharOffset(0),
                &Cancellation::default()
            )
            .is_err()
        );
        let selection = destination_selection(
            &snapshot,
            Range {
                start: Position {
                    line: 0,
                    character: 2,
                },
                end: Position {
                    line: 0,
                    character: 5,
                },
            },
            &Cancellation::default(),
        )
        .unwrap();
        assert!(editor.apply_prepared_selections(selection));
        assert_eq!(
            editor.selections().primary(),
            Selection::new(CharOffset(4), CharOffset(1))
        );
    }
}
