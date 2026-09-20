//! Bounded rename-prompt preparation on the language worker.

use crate::{Range, protocol::Positions};
use serde_json::Value;
use vex_core::{CharOffset, Selection, Snapshot};
use vex_editor::background::Cancellation;

pub(crate) const MAX_NAME_BYTES: usize = 4096;

pub(crate) fn name(text: &str) -> Result<String, String> {
    if text.len() > MAX_NAME_BYTES || text.chars().any(char::is_control) {
        return Err("rename name must be one line and at most 4096 bytes".into());
    }
    Ok(text.into())
}

pub(crate) fn placeholder(
    value: &Value,
    snapshot: &Snapshot,
    selection: Selection,
    cursor: CharOffset,
    positions: &Positions,
    token: &Cancellation,
) -> Result<String, String> {
    if token.is_cancelled() {
        return Err("cancelled".into());
    }
    if value.is_null() {
        return Err("symbol cannot be renamed at this position".into());
    }
    if let Some(default) = value.get("defaultBehavior") {
        if !default.is_boolean() {
            return Err("invalid prepareRename default behavior".into());
        }
        return fallback(snapshot, selection, cursor, token);
    }
    let range: Range = serde_json::from_value(value.get("range").unwrap_or(value).clone())
        .map_err(|_| "invalid prepareRename range")?;
    let start = positions
        .edit_offset(snapshot.text(), range.start)
        .ok_or("invalid prepareRename start")?;
    let end = positions
        .edit_offset(snapshot.text(), range.end)
        .ok_or("invalid prepareRename end")?;
    if start > end {
        return Err("reversed prepareRename range".into());
    }
    if let Some(placeholder) = value.get("placeholder") {
        return name(
            placeholder
                .as_str()
                .ok_or("invalid prepareRename placeholder")?,
        );
    }
    slice_name(snapshot, start.0, end.0)
}

fn slice_name(snapshot: &Snapshot, start: usize, end: usize) -> Result<String, String> {
    let text = snapshot.text();
    if end > text.len_chars()
        || start > end
        || text.char_to_byte(end) - text.char_to_byte(start) > MAX_NAME_BYTES
    {
        return Err("rename selection exceeds 4096 bytes".into());
    }
    name(&text.slice(start..end).to_string())
}

pub(crate) fn fallback(
    snapshot: &Snapshot,
    selection: Selection,
    cursor: CharOffset,
    token: &Cancellation,
) -> Result<String, String> {
    if token.is_cancelled() {
        return Err("cancelled".into());
    }
    if selection.end().0 - selection.start().0 > 1 {
        return slice_name(snapshot, selection.start().0, selection.end().0);
    }
    // Use the same word categories and grapheme boundaries as miw, like Helix.
    // Bound scanning independently of the eventual UTF-8 byte limit.
    let visited = std::cell::Cell::new(0usize);
    let word = vex_core::textobject::word(snapshot.text(), cursor, false, false, &|| {
        visited.set(visited.get() + 1);
        token.is_cancelled() || visited.get() > MAX_NAME_BYTES
    })
    .map_err(|error| error.to_string())?;
    if token.is_cancelled() {
        return Err("cancelled".into());
    }
    if visited.get() > MAX_NAME_BYTES {
        return Err("rename symbol exceeds 4096 graphemes".into());
    }
    slice_name(snapshot, word.start().0, word.end().0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use vex_core::Document;

    #[test]
    fn preparation_decodes_ranges_placeholders_and_defaults_with_strict_utf16_and_bounds() {
        let snapshot = Document::from("😀 let café = café;").snapshot();
        let selection = Selection::new(CharOffset(8), CharOffset(9));
        let token = Cancellation::default();
        let positions = Positions::new(snapshot.text());
        let range = json!({"start":{"line":0,"character":7},"end":{"line":0,"character":11}});
        let decode =
            |v: Value| placeholder(&v, &snapshot, selection, CharOffset(8), &positions, &token);
        assert_eq!(decode(range.clone()).unwrap(), "café");
        assert_eq!(
            decode(json!({"range":range,"placeholder":"new_café"})).unwrap(),
            "new_café"
        );
        assert_eq!(decode(json!({"defaultBehavior":true})).unwrap(), "café");
        assert!(decode(Value::Null).is_err());
        assert_eq!(decode(json!({"defaultBehavior":false})).unwrap(), "café");
        assert!(decode(json!({"defaultBehavior":"invalid"})).is_err());
        assert!(
            decode(json!({"start":{"line":0,"character":1},"end":{"line":0,"character":11}}))
                .is_err()
        );
        assert!(decode(json!({"range":range,"placeholder":"bad\nname"})).is_err());
        assert!(name(&"a".repeat(MAX_NAME_BYTES + 1)).is_err());
        for (text, cursor, expected) in [
            ("foo bar", 3, ""),
            ("foo::bar", 3, "::"),
            ("cafe\u{301}", 0, "cafe\u{301}"),
        ] {
            let snapshot = Document::from(text).snapshot();
            assert_eq!(
                fallback(
                    &snapshot,
                    Selection::new(CharOffset(cursor), CharOffset(cursor + 1)),
                    CharOffset(cursor),
                    &token
                )
                .unwrap(),
                expected
            );
        }
        let oversized = Document::from("a".repeat(100_000).as_str()).snapshot();
        assert!(
            fallback(
                &oversized,
                Selection::cursor(CharOffset(0)),
                CharOffset(0),
                &token
            )
            .is_err()
        );
        token.cancel();
        assert!(decode(range).is_err());
    }
}
