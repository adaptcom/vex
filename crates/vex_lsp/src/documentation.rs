//! Decode modern MarkupContent and legacy MarkedString hover replies on the service.

use crate::Documentation;
use serde_json::Value;
use std::time::{Duration, Instant};
use vex_editor::background::Cancellation;
use vex_syntax::markup::{Attributes, Line, MAX_BYTES, MAX_LINES};

pub(crate) fn hover(value: &Value, cancellation: &Cancellation) -> Result<Documentation, String> {
    let contents = &value["contents"];
    let values = contents
        .as_array()
        .map_or(std::slice::from_ref(contents), Vec::as_slice);
    let mut output = Documentation::default();
    let mut remaining = MAX_BYTES;
    let deadline = Instant::now() + Duration::from_millis(25);
    for (index, value) in values.iter().take(32).enumerate() {
        if cancellation.is_cancelled() {
            return Err("hover cancelled".into());
        }
        let Some(text) = value.as_str().or_else(|| value["value"].as_str()) else {
            continue;
        };
        let mut end = text.len().min(remaining);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        remaining -= end;
        output.truncated |= end < text.len();
        let text = &text[..end];
        let code = value["language"].is_string();
        let markdown = !code && (value.is_string() || value["kind"] == "markdown");
        let mut part = if markdown && Instant::now() < deadline {
            Documentation::markdown(text, || {
                cancellation.is_cancelled() || Instant::now() >= deadline
            })
        } else {
            Documentation::plain(text)
        };
        if cancellation.is_cancelled() {
            return Err("hover cancelled".into());
        }
        // A shared deadline can expire during parsing one part. Keep that part
        // readable as plaintext rather than losing it to cancellation fallback.
        if part.is_empty() && !text.trim().is_empty() {
            part = Documentation::plain(text);
        }
        if code {
            for line in &mut part.lines {
                for span in &mut line.spans {
                    span.attributes |= Attributes::CODE;
                }
            }
        }
        if !output.lines.is_empty() && !part.lines.is_empty() {
            output.lines.push(Line::default());
        }
        output.lines.extend(part.lines);
        output.truncated |= part.truncated;
        if output.lines.len() > MAX_LINES {
            output.lines.truncate(MAX_LINES);
            output.truncated = true;
            break;
        }
        if remaining == 0 {
            output.truncated |= index + 1 < values.len();
            break;
        }
    }
    output.truncated |= values.len() > 32;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn modern_and_legacy_hover_preserve_markup_intent_and_limits() {
        let token = Cancellation::default();
        let markdown = hover(
            &json!({"contents":{"kind":"markdown","value":"# Title\n\n**Bold** and `code`"}}),
            &token,
        )
        .unwrap();
        assert!(markdown.contains("Bold and code"));
        assert!(!markdown.contains("**"));
        let plain = hover(
            &json!({"contents":{"kind":"plaintext","value":"**literal**"}}),
            &token,
        )
        .unwrap();
        assert!(plain.contains("**literal**"));
        let legacy = hover(
            &json!({"contents":[{"language":"rust","value":"fn f<T>() {}"},"**Docs**"]}),
            &token,
        )
        .unwrap();
        assert!(legacy.contains("fn f<T>() {}"));
        assert!(
            legacy.lines[0].spans[0]
                .attributes
                .contains(Attributes::CODE)
        );
        assert!(legacy.contains("Docs"));
        assert!(
            legacy
                .lines
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.attributes.contains(Attributes::STRONG))
        );
        let many = hover(&json!({"contents":vec!["🦀".repeat(5000); 100]}), &token).unwrap();
        assert!(many.truncated);
        assert!(
            many.lines
                .iter()
                .flat_map(|line| &line.spans)
                .map(|span| span.text.len())
                .sum::<usize>()
                <= MAX_BYTES
        );
        token.cancel();
        assert!(hover(&json!({"contents":"docs"}), &token).is_err());
    }
}
