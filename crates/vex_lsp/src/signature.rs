//! Bounded signature labels and Markdown, prepared on the language worker.

use crate::Documentation;
use serde_json::Value;
use std::time::{Duration, Instant};
use vex_editor::background::Cancellation;
use vex_syntax::markup::{Attributes, Line, MAX_BYTES, MAX_LINES, Span};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SignatureOptions {
    pub trigger_characters: Vec<String>,
}

impl SignatureOptions {
    pub(crate) fn from_capabilities(capabilities: &Value) -> Option<Self> {
        let provider = &capabilities["signatureHelpProvider"];
        if !provider.is_object() {
            return None;
        }
        let trigger_characters = provider["triggerCharacters"]
            .as_array()
            .into_iter()
            .flatten()
            .take(64)
            .filter_map(Value::as_str)
            .filter(|text| {
                !text.is_empty() && text.len() <= 64 && !text.chars().any(char::is_control)
            })
            .map(str::to_owned)
            .collect();
        Some(Self { trigger_characters })
    }
}

#[derive(Debug, Default)]
pub struct SignatureHelp {
    pub signatures: Vec<Documentation>,
    pub active_signature: Option<usize>,
    pub limited: bool,
}

fn bounded(text: &str, remaining: &mut usize, limit: usize) -> (usize, bool) {
    let mut end = text.len().min(*remaining).min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    *remaining -= end;
    (end, end < text.len())
}

/// Parameter offsets are UTF-16 code units, independent of positionEncoding.
fn byte_offset(text: &str, units: u64) -> Option<usize> {
    let mut remaining = usize::try_from(units).ok()?;
    for (byte, ch) in text.char_indices() {
        if remaining == 0 {
            return Some(byte);
        }
        remaining = remaining.checked_sub(ch.len_utf16())?;
    }
    (remaining == 0).then_some(text.len())
}

fn parameter_range(label: &str, parameter: &Value) -> Option<std::ops::Range<usize>> {
    if let Some(text) = parameter["label"].as_str() {
        if text.is_empty() || text.len() > label.len() {
            return None;
        }
        let start = label.find(text)?;
        return Some(start..start + text.len());
    }
    let offsets = parameter["label"].as_array()?;
    if offsets.len() != 2 {
        return None;
    }
    let start = byte_offset(label, offsets[0].as_u64()?)?;
    let end = byte_offset(label, offsets[1].as_u64()?)?;
    (start < end).then_some(start..end)
}

fn label_document(label: &str, active: Option<std::ops::Range<usize>>) -> Documentation {
    let mut document = Documentation::default();
    let mut offset = 0;
    for raw in label.split_inclusive('\n') {
        let text = raw.trim_end_matches(['\r', '\n']);
        let mut line = Line {
            literal: true,
            ..Line::default()
        };
        let range = active
            .as_ref()
            .map(|range| {
                range.start.saturating_sub(offset).min(text.len())
                    ..range.end.saturating_sub(offset).min(text.len())
            })
            .unwrap_or(0..0);
        for (text, attributes) in [
            (&text[..range.start], Attributes::CODE),
            (
                &text[range.clone()],
                Attributes::CODE | Attributes::SELECTED,
            ),
            (&text[range.end..], Attributes::CODE),
        ] {
            if !text.is_empty() {
                line.spans.push(Span {
                    text: text.into(),
                    attributes,
                });
            }
        }
        document.lines.push(line);
        offset += raw.len();
    }
    document
}

pub(crate) fn parse(value: &Value, cancellation: &Cancellation) -> Result<SignatureHelp, String> {
    let Some(signatures) = value["signatures"].as_array() else {
        return Ok(SignatureHelp::default());
    };
    let mut output = SignatureHelp {
        limited: signatures.len() > 32,
        ..SignatureHelp::default()
    };
    let mut remaining = MAX_BYTES;
    let mut lines = MAX_LINES;
    let deadline = Instant::now() + Duration::from_millis(25);
    for (index, signature) in signatures.iter().take(32).enumerate() {
        if cancellation.is_cancelled() {
            return Err("signature help cancelled".into());
        }
        let Some(label) = signature["label"]
            .as_str()
            .filter(|label| !label.is_empty())
        else {
            continue;
        };
        let (end, limited) = bounded(label, &mut remaining, 4096);
        if end == 0 || lines == 0 {
            output.limited = true;
            break;
        }
        let parameters = signature["parameters"].as_array();
        let active = signature["activeParameter"]
            .as_u64()
            .or_else(|| value["activeParameter"].as_u64())
            .and_then(|index| usize::try_from(index).ok())
            .filter(|index| parameters.is_some_and(|items| *index < items.len()))
            .unwrap_or(0);
        let parameter = parameters
            .and_then(|items| items.get(active))
            .unwrap_or(&Value::Null);
        let mut document = label_document(&label[..end], parameter_range(&label[..end], parameter));
        document.truncated = limited;
        for value in [&parameter["documentation"], &signature["documentation"]] {
            let Some(text) = value.as_str().or_else(|| value["value"].as_str()) else {
                continue;
            };
            let (end, limited) = bounded(text, &mut remaining, MAX_BYTES);
            document.truncated |= limited;
            let text = &text[..end];
            let mut part = if (value.is_string() || value["kind"] == "markdown")
                && Instant::now() < deadline
            {
                Documentation::markdown(text, || {
                    cancellation.is_cancelled() || Instant::now() >= deadline
                })
            } else {
                Documentation::plain(text)
            };
            if cancellation.is_cancelled() {
                return Err("signature help cancelled".into());
            }
            if part.is_empty() && !text.trim().is_empty() {
                part = Documentation::plain(text);
            }
            if !part.lines.is_empty() {
                document.lines.push(Line::default());
                document.lines.extend(part.lines);
            }
            document.truncated |= part.truncated;
        }
        document.truncated |= document.lines.len() > lines;
        document.lines.truncate(lines);
        lines -= document.lines.len();
        output.limited |= document.truncated;
        if value["activeSignature"].as_u64() == Some(index as u64) {
            output.active_signature = Some(output.signatures.len());
        }
        output.signatures.push(document);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn selected(document: &Documentation) -> String {
        document
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter(|span| span.attributes.contains(Attributes::SELECTED))
            .map(|span| span.text.as_str())
            .collect()
    }

    #[test]
    fn utf16_offsets_parameter_override_and_markup_are_preserved() {
        let result = parse(
            &json!({"activeSignature":1,"activeParameter":1,"signatures":[
                {"label":"f(🦀: T, value: U)","parameters":[{"label":[2,7]}, {"label":"value: U"}]},
                {"label":"f(🦀: T, value: U)","activeParameter":0,"parameters":[
                    {"label":[2,7],"documentation":{"kind":"plaintext","value":"**literal**"}},
                    {"label":"value: U"}],"documentation":{"kind":"markdown","value":"**Details**"}}
            ]}),
            &Cancellation::default(),
        )
        .unwrap();
        assert_eq!(result.active_signature, Some(1));
        assert_eq!(selected(&result.signatures[0]), "value: U");
        assert_eq!(selected(&result.signatures[1]), "🦀: T");
        assert!(result.signatures[1].contains("**literal**"));
        assert!(result.signatures[1].contains("Details"));
        assert!(!result.signatures[1].contains("**Details**"));
        assert_eq!(byte_offset("🦀", 1), None);
        assert_eq!(parameter_range("a🦀", &json!({"label":[1,2]})), None);
        assert_eq!(parameter_range("abc", &json!({"label":[2,1]})), None);
    }

    #[test]
    fn empty_invalid_limited_and_cancelled_replies_are_bounded() {
        let token = Cancellation::default();
        assert!(parse(&Value::Null, &token).unwrap().signatures.is_empty());
        let signatures =
            vec![json!({"label":"x".repeat(5000),"documentation":"🦀".repeat(MAX_BYTES)}); 40];
        let result = parse(&json!({"signatures":signatures}), &token).unwrap();
        assert!(result.limited);
        assert!(
            result
                .signatures
                .iter()
                .flat_map(|doc| &doc.lines)
                .flat_map(|line| &line.spans)
                .map(|span| span.text.len())
                .sum::<usize>()
                <= MAX_BYTES
        );
        assert!(
            result
                .signatures
                .iter()
                .map(|doc| doc.lines.len())
                .sum::<usize>()
                <= MAX_LINES
        );
        let result = parse(&json!({"activeParameter":999,"signatures":[{}, {"label":"f(a, b)","parameters":[{"label":"a"},{"label":"b"}]}]}), &token).unwrap();
        assert_eq!(selected(&result.signatures[0]), "a");
        token.cancel();
        assert!(parse(&json!({"signatures":[{"label":"f()"}]}), &token).is_err());
    }
}
