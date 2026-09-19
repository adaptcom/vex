//! Bounded, plain-text completion items. Coordinates are validated on the service
//! thread against the exact requested snapshot, before edits reach the editor.

use crate::protocol::{Position, Positions, Range};
use serde_json::Value;
use std::collections::BTreeMap;
use vex_core::{CharOffset, Edit, Rope};

const MAX_ITEMS: usize = 512;
const MAX_ITEM_BYTES: usize = 64 << 10;
const MAX_RETAINED_BYTES: usize = 4 << 20;
const MAX_PREFIX_CHARS: usize = 256;

#[derive(Clone, Debug)]
pub struct CompletionItem {
    pub label: String,
    pub detail: String,
    pub documentation: String,
    pub edit: Edit,
    pub additional_edits: Vec<Edit>,
    pub resolved: bool,
    pub(crate) raw: Value,
}

impl CompletionItem {
    /// An immediately usable plain-text candidate without a server resolve step.
    pub fn plain(label: impl Into<String>, edit: Edit) -> Self {
        Self {
            label: label.into(),
            detail: String::new(),
            documentation: String::new(),
            edit,
            additional_edits: Vec::new(),
            resolved: true,
            raw: Value::Null,
        }
    }
}

#[derive(Debug)]
pub struct Completions {
    pub items: Vec<CompletionItem>,
    pub incomplete: bool,
    pub limited: bool,
}

fn bounded(text: &str, chars: usize) -> String {
    text.chars().take(chars).collect()
}

struct Coordinates<'a> {
    text: &'a Rope,
    positions: Positions,
    // Hundreds of items often share one replacement range on a long line.
    // Validate each distinct UTF-16 coordinate once rather than rescanning it.
    offsets: BTreeMap<Position, Option<CharOffset>>,
}

impl<'a> Coordinates<'a> {
    fn new(text: &'a Rope) -> Self {
        Self {
            text,
            positions: Positions::new(text),
            offsets: BTreeMap::new(),
        }
    }

    fn offset(&mut self, position: Position) -> Option<CharOffset> {
        *self.offsets.entry(position).or_insert_with(|| {
            let offset = self.positions.offset(self.text, position)?;
            // Navigation clamps invalid columns; edits must never silently do so.
            (self.positions.position(self.text, offset)? == position).then_some(offset)
        })
    }

    fn range(&mut self, value: &Value) -> Option<std::ops::Range<CharOffset>> {
        let range: Range = serde_json::from_value(value.clone()).ok()?;
        let start = self.offset(range.start)?;
        let end = self.offset(range.end)?;
        (start <= end).then_some(start..end)
    }
}

fn word_start(text: &Rope, cursor: CharOffset) -> Option<CharOffset> {
    let mut chars = text.chars_at(cursor.0);
    let mut start = cursor.0;
    while chars
        .prev()
        .is_some_and(|ch| ch == '_' || ch.is_alphanumeric())
    {
        start -= 1;
        if cursor.0 - start > MAX_PREFIX_CHARS {
            return None;
        }
    }
    Some(CharOffset(start))
}

fn item(
    raw: Value,
    coordinates: &mut Coordinates<'_>,
    cursor: CharOffset,
    resolved: bool,
) -> Option<CompletionItem> {
    if raw
        .get("insertTextFormat")
        .is_some_and(|format| format.as_u64() != Some(1))
    {
        return None; // Snippets were not advertised; never insert placeholder syntax.
    }
    let label = raw["label"].as_str()?;
    let (replacement, inserted) = if let Some(edit) = raw.get("textEdit") {
        let value = edit.get("range").or_else(|| edit.get("insert"))?;
        if value["start"]["line"] != value["end"]["line"] {
            return None;
        }
        let replacement = if let Some(single) = edit.get("range") {
            coordinates.range(single)?
        } else {
            let insert = coordinates.range(&edit["insert"])?;
            let replace = coordinates.range(&edit["replace"])?;
            if insert.start != replace.start
                || insert.end > replace.end
                || edit["replace"]["start"]["line"] != edit["replace"]["end"]["line"]
            {
                return None;
            }
            insert // Insert mode preserves the suffix after the cursor.
        };
        if replacement.start > cursor || replacement.end < cursor {
            return None;
        }
        (replacement, edit["newText"].as_str()?)
    } else {
        (
            word_start(coordinates.text, cursor)?..cursor,
            raw["insertText"].as_str().unwrap_or(label),
        )
    };
    let mut additional_edits = Vec::new();
    if let Some(edits) = raw.get("additionalTextEdits") {
        let edits = edits.as_array()?;
        if edits.len() > 64 {
            return None;
        }
        for edit in edits {
            additional_edits.push(Edit::new(
                coordinates.range(&edit["range"])?,
                edit["newText"].as_str()?,
            ));
        }
    }
    let documentation = raw["documentation"]
        .as_str()
        .or_else(|| raw["documentation"]["value"].as_str())
        .unwrap_or_default();
    Some(CompletionItem {
        label: bounded(label, 256),
        detail: bounded(raw["detail"].as_str().unwrap_or_default(), 1024),
        documentation: bounded(documentation, 4096),
        edit: Edit::new(replacement, inserted),
        additional_edits,
        resolved,
        raw,
    })
}

pub(crate) fn parse(
    value: Value,
    text: &Rope,
    cursor: CharOffset,
    resolve: bool,
) -> Result<Completions, String> {
    if cursor.0 > text.len_chars() {
        return Err("invalid completion position".into());
    }
    let incomplete = value["isIncomplete"] == true;
    let values = if value.is_null() {
        &[][..]
    } else {
        value
            .as_array()
            .or_else(|| value["items"].as_array())
            .ok_or("invalid completion list")?
    };
    let start = word_start(text, cursor).ok_or("completion prefix exceeds 256 characters")?;
    let prefix = text.slice(start.0..cursor.0).to_string().to_lowercase();
    let mut candidates: Vec<_> = values
        .iter()
        .take(16_384)
        .filter(|value| {
            let filter = value["filterText"]
                .as_str()
                .or_else(|| value["label"].as_str())
                .unwrap_or_default();
            let mut wanted = prefix.chars();
            let mut next = wanted.next();
            for ch in filter.chars().take(1024).flat_map(char::to_lowercase) {
                if next == Some(ch) {
                    next = wanted.next();
                }
                if next.is_none() {
                    break;
                }
            }
            next.is_none()
        })
        .collect();
    fn sort_key(value: &Value) -> &str {
        value["sortText"]
            .as_str()
            .or_else(|| value["label"].as_str())
            .unwrap_or_default()
    }
    candidates.sort_by(|a, b| sort_key(a).cmp(sort_key(b)));
    let mut coordinates = Coordinates::new(text);
    let mut items = Vec::new();
    let mut bytes = 0;
    let mut limited = values.len() > 16_384;
    for value in candidates {
        let size = value.to_string().len();
        if size > MAX_ITEM_BYTES {
            limited = true;
            continue;
        }
        if items.len() == MAX_ITEMS || bytes + size > MAX_RETAINED_BYTES {
            limited = true;
            break;
        }
        if let Some(item) = item(value.clone(), &mut coordinates, cursor, !resolve) {
            bytes += size;
            items.push(item);
        }
    }
    Ok(Completions {
        items,
        incomplete,
        limited,
    })
}

pub(crate) fn resolve(
    original: &CompletionItem,
    value: Value,
    text: &Rope,
    cursor: CharOffset,
) -> Result<CompletionItem, String> {
    if !value.is_object() || value.to_string().len() > MAX_ITEM_BYTES {
        return Err("invalid or oversized resolved completion".into());
    }
    let mut raw = original.raw.clone();
    // Only these advertised fields may be resolved lazily. Preserve the original
    // insertion/filtering data so a documentation reply cannot change the edit.
    for field in ["detail", "documentation", "additionalTextEdits"] {
        if let Some(value) = value.get(field) {
            raw[field] = value.clone();
        }
    }
    if raw.to_string().len() > MAX_ITEM_BYTES {
        return Err("resolved completion exceeds size limit".into());
    }
    item(raw, &mut Coordinates::new(text), cursor, true)
        .ok_or_else(|| "invalid resolved completion edits".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn completions_use_utf16_edits_filter_sort_and_resolve_imports() {
        let text = Rope::from_str("// 🦀\r\nfn main() { ans }");
        let cursor = CharOffset(text.len_chars() - 2);
        let values = json!({"isIncomplete":true,"items":[
            {"label":"answer_two", "sortText":"2"},
            {"label":"unrelated"},
            {"label":"answer", "sortText":"1", "textEdit":{"range":{"start":{"line":1,"character":12},"end":{"line":1,"character":15}},"newText":"answer()"},"data":{"key":5}},
            {"label":"ans_snippet", "insertTextFormat":2, "insertText":"ans($1)"}
        ]});
        let list = parse(values, &text, cursor, true).unwrap();
        assert!(list.incomplete);
        assert_eq!(
            list.items
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>(),
            ["answer", "answer_two"]
        );
        let first = &list.items[0];
        assert_eq!(
            text.slice(first.edit.range().start.0..first.edit.range().end.0),
            "ans"
        );
        assert!(!first.resolved);
        let resolved = resolve(first, json!({"label":"answer", "documentation":{"kind":"plaintext","value":"Docs"},"additionalTextEdits":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},"newText":"use demo::answer;\n"}]}), &text, cursor).unwrap();
        assert_eq!(resolved.edit.text(), "answer()");
        assert_eq!(resolved.additional_edits.len(), 1);
        assert_eq!(resolved.documentation, "Docs");
        assert_eq!(resolved.raw["data"]["key"], 5);
    }

    #[test]
    fn invalid_columns_surrogate_interiors_ranges_and_large_items_are_rejected() {
        let text = Rope::from_str("🦀ans");
        let entry = |start, end| json!({"label":"answer", "textEdit":{"range":{"start":{"line":0,"character":start},"end":{"line":0,"character":end}},"newText":"answer"}});
        for value in [entry(1, 5), entry(2, 6), entry(5, 2), entry(2, 4)] {
            assert!(
                parse(json!([value]), &text, CharOffset(4), false)
                    .unwrap()
                    .items
                    .is_empty()
            );
        }
        let list = parse(
            json!([{"label":"answer", "documentation":"x".repeat(MAX_ITEM_BYTES)}]),
            &text,
            CharOffset(4),
            false,
        )
        .unwrap();
        assert!(list.limited && list.items.is_empty());
        assert!(
            parse(Value::Null, &text, CharOffset(4), false)
                .unwrap()
                .items
                .is_empty()
        );
        let long = Rope::from_str(&"a".repeat(MAX_PREFIX_CHARS + 1));
        assert!(parse(Value::Null, &long, CharOffset(long.len_chars()), false).is_err());
    }

    #[test]
    fn insert_replace_uses_insert_range_and_resolution_cannot_replace_the_original_edit() {
        let text = Rope::from_str("ans_suffix");
        let range = |end| serde_json::json!({"start":{"line":0,"character":0},"end":{"line":0,"character":end}});
        let list = parse(serde_json::json!([{"label":"answer","textEdit":{"insert":range(3),"replace":range(10),"newText":"answer"}}]), &text, CharOffset(3), true).unwrap();
        let item = &list.items[0];
        assert_eq!(item.edit.range(), CharOffset(0)..CharOffset(3));
        let resolved = resolve(item, serde_json::json!({"textEdit":{"range":range(10),"newText":"changed"},"documentation":"docs"}), &text, CharOffset(3)).unwrap();
        assert_eq!(resolved.edit.text(), "answer");
        assert_eq!(resolved.edit.range(), item.edit.range());
        assert_eq!(resolved.documentation, "docs");
    }
}
