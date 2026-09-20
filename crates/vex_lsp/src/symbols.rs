//! Normalize hierarchical and flat LSP symbols into bounded navigation entries.

use crate::{
    Location,
    protocol::{Range, file_path},
};
use serde_json::Value;
use std::path::Path;

const MAX_SYMBOLS: usize = 16_384;
const MAX_BYTES: usize = 4 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub container: String,
    pub kind: u32,
    pub location: Location,
}

impl Symbol {
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            1 => "file",
            2 => "module",
            3 => "namespace",
            4 => "package",
            5 => "class",
            6 => "method",
            7 => "property",
            8 => "field",
            9 => "constructor",
            10 => "enum",
            11 => "interface",
            12 => "function",
            13 => "variable",
            14 => "constant",
            15 => "string",
            16 => "number",
            17 => "boolean",
            18 => "array",
            19 => "object",
            20 => "key",
            21 => "null",
            22 => "enum member",
            23 => "struct",
            24 => "event",
            25 => "operator",
            26 => "type parameter",
            _ => "symbol",
        }
    }
}

#[derive(Debug, Default)]
pub struct Symbols {
    pub items: Vec<Symbol>,
    pub limited: bool,
}

fn label(text: &str) -> String {
    text.chars().take(256).collect()
}

fn range(value: &Value) -> Option<Range> {
    let range: Range = serde_json::from_value(value.clone()).ok()?;
    (range.start <= range.end).then_some(range)
}

/// Workspace ranges are required: we deliberately do not advertise lazy resolve.
/// Malformed entries and non-file URIs cannot prevent valid symbols from loading.
pub(crate) fn parse(value: &Value, document: Option<&Path>) -> Result<Symbols, String> {
    if value.is_null() {
        return Ok(Symbols::default());
    }
    let values = value.as_array().ok_or("invalid symbol response")?;
    let mut result = Symbols::default();
    let mut stack = vec![(values.iter(), String::new())];
    let (mut visited, mut bytes) = (0, 0);
    while let Some((items, parent)) = stack.last_mut() {
        let Some(value) = items.next() else {
            stack.pop();
            continue;
        };
        if visited == MAX_SYMBOLS {
            result.limited = true;
            break;
        }
        visited += 1;
        let Some(name) = value["name"]
            .as_str()
            .filter(|name| !name.trim().is_empty())
        else {
            continue;
        };
        let name = label(name);
        let container = label(value["containerName"].as_str().unwrap_or(parent));
        let location = if value.get("location").is_some() {
            let location = &value["location"];
            location["uri"]
                .as_str()
                .and_then(|uri| file_path(uri).ok())
                .zip(range(&location["range"]))
                .map(|(path, range)| Location {
                    path,
                    position: range.start,
                })
        } else {
            document.and_then(|path| {
                let enclosing = range(&value["range"])?;
                let selection = range(&value["selectionRange"])?;
                (enclosing.start <= selection.start && selection.end <= enclosing.end).then(|| {
                    Location {
                        path: path.into(),
                        position: selection.start,
                    }
                })
            })
        };
        if let Some(location) = location {
            bytes += name.len() + container.len() + location.path.as_os_str().len();
            if bytes > MAX_BYTES {
                result.limited = true;
                break;
            }
            result.items.push(Symbol {
                name: name.clone(),
                container: container.clone(),
                kind: value["kind"]
                    .as_u64()
                    .and_then(|n| n.try_into().ok())
                    .unwrap_or(0),
                location,
            });
        }
        if document.is_some()
            && let Some(children) = value["children"].as_array()
        {
            let parent = if container.is_empty() {
                name
            } else {
                label(&format!("{container}::{name}"))
            };
            stack.push((children.iter(), parent));
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Position, file_uri};
    use serde_json::json;

    #[test]
    fn hierarchical_symbols_use_selection_ranges_and_preserve_containers() {
        let range = json!({"start":{"line":2,"character":0},"end":{"line":9,"character":1}});
        let selection = json!({"start":{"line":2,"character":7},"end":{"line":2,"character":10}});
        let symbols = parse(
            &json!([{"name":"外","kind":23,"range":range,"selectionRange":selection,
            "children":[{"name":"run","kind":6,"range":range,"selectionRange":selection}]}]),
            Some(Path::new("/test.rs")),
        )
        .unwrap();
        assert_eq!(symbols.items.len(), 2);
        assert_eq!(symbols.items[1].container, "外");
        assert_eq!(symbols.items[1].kind_name(), "method");
        assert_eq!(
            symbols.items[1].location.position,
            Position {
                line: 2,
                character: 7
            }
        );
    }

    #[test]
    fn flat_symbols_skip_invalid_locations_and_bound_retained_results() {
        let valid = json!({"name":"run","kind":999,"containerName":"Module","location":{
            "uri":file_uri(Path::new("/space 界.rs")).unwrap(),
            "range":{"start":{"line":0,"character":3},"end":{"line":0,"character":6}}
        }});
        let mut bad = valid.clone();
        bad["location"]["uri"] = json!("https://example.com/source");
        let mut reversed = valid.clone();
        reversed["location"]["range"]["end"]["character"] = json!(1);
        let mut unresolved = valid.clone();
        unresolved["location"]
            .as_object_mut()
            .unwrap()
            .remove("range");
        let result = parse(&json!([bad, reversed, unresolved, valid]), None).unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].kind_name(), "symbol");
        assert_eq!(result.items[0].location.path, Path::new("/space 界.rs"));
        let result = parse(&Value::Array(vec![valid; MAX_SYMBOLS + 1]), None).unwrap();
        assert_eq!(result.items.len(), MAX_SYMBOLS);
        assert!(result.limited);
        assert!(parse(&Value::Null, None).unwrap().items.is_empty());
        assert!(parse(&json!({}), None).is_err());
    }
}
