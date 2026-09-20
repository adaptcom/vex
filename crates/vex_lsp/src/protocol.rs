//! Bounded LSP framing, file URIs, and UTF-16 coordinates (LSP's default encoding).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::{self, BufRead, Read, Write},
    path::{Path, PathBuf},
};
use vex_core::{CharOffset, Rope};

pub(crate) const MAX_MESSAGE: usize = 32 << 20;
const MAX_HEADER: usize = 8 << 10;

pub(crate) fn read_message(reader: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut length = None;
    let mut total = 0;
    loop {
        let mut line = String::new();
        let read = reader
            .take((MAX_HEADER - total + 1) as u64)
            .read_line(&mut line)?;
        if read == 0 && total == 0 {
            return Ok(None);
        }
        total += read;
        if read == 0 || total > MAX_HEADER {
            return Err(io::Error::other("invalid LSP header"));
        }
        if line == "\r\n" {
            break;
        }
        let (name, value) = line
            .trim_end()
            .split_once(':')
            .ok_or_else(|| io::Error::other("invalid LSP header"))?;
        if name.eq_ignore_ascii_case("Content-Length") {
            if length.is_some() {
                return Err(io::Error::other("duplicate Content-Length"));
            }
            length = Some(value.trim().parse::<usize>().map_err(io::Error::other)?);
        }
    }
    let length = length
        .filter(|len| *len <= MAX_MESSAGE)
        .ok_or_else(|| io::Error::other("missing or oversized Content-Length"))?;
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(io::Error::other)
}

pub(crate) fn write_message(writer: &mut impl Write, value: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() > MAX_MESSAGE {
        return Err(io::Error::other("LSP message too large"));
    }
    write!(writer, "Content-Length: {}\r\n\r\n", bytes.len())?;
    writer.write_all(&bytes)?;
    writer.flush()
}

pub fn file_uri(path: &Path) -> io::Result<String> {
    url::Url::from_file_path(path)
        .map(String::from)
        .map_err(|_| io::Error::other("LSP requires an absolute file path"))
}

pub fn file_path(uri: &str) -> io::Result<PathBuf> {
    url::Url::parse(uri)
        .map_err(io::Error::other)?
        .to_file_path()
        .map_err(|_| io::Error::other("location is not a local file"))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// Index LSP line starts once per snapshot; Ropey also recognizes Unicode line
/// separators which LSP treats as ordinary characters.
pub(crate) struct Positions {
    starts: Vec<usize>,
}

impl Positions {
    pub fn new(text: &Rope) -> Self {
        Self::cancellable(text, || false).expect("not cancelled")
    }

    pub fn cancellable(text: &Rope, cancelled: impl Fn() -> bool) -> Option<Self> {
        let mut starts = vec![0];
        let mut cr = false;
        for (index, ch) in text.chars().enumerate() {
            if index % 4096 == 0 && cancelled() {
                return None;
            }
            if ch == '\n' && cr {
                *starts.last_mut().unwrap() = index + 1;
            } else if matches!(ch, '\r' | '\n') {
                starts.push(index + 1);
            }
            cr = ch == '\r';
        }
        Some(Self { starts })
    }

    fn line_end(&self, text: &Rope, line: usize) -> usize {
        let mut end = self
            .starts
            .get(line + 1)
            .copied()
            .unwrap_or(text.len_chars());
        let start = self.starts[line];
        if end > start && text.char(end - 1) == '\n' {
            end -= 1;
        }
        if end > start && text.char(end - 1) == '\r' {
            end -= 1;
        }
        end
    }

    pub fn position(&self, text: &Rope, offset: CharOffset) -> Option<Position> {
        if offset.0 > text.len_chars() {
            return None;
        }
        let line = self.starts.partition_point(|start| *start <= offset.0) - 1;
        let end = offset.0.min(self.line_end(text, line));
        Some(Position {
            line: line.try_into().ok()?,
            character: text
                .slice(self.starts[line]..end)
                .len_utf16_cu()
                .try_into()
                .ok()?,
        })
    }

    pub fn offset(&self, text: &Rope, wanted: Position) -> Option<CharOffset> {
        let start = *self.starts.get(wanted.line as usize)?;
        let line = text.slice(start..self.line_end(text, wanted.line as usize));
        Some(CharOffset(
            start + line.utf16_cu_to_char((wanted.character as usize).min(line.len_utf16_cu())),
        ))
    }

    /// Edits may clamp columns beyond EOL (LSP's rule), but cannot split a
    /// surrogate pair. Navigation's backward clamping is unsafe for replacement.
    pub fn edit_offset(&self, text: &Rope, wanted: Position) -> Option<CharOffset> {
        let offset = self.offset(text, wanted)?;
        let actual = self.position(text, offset)?;
        (actual.character == wanted.character
            || offset.0 == self.line_end(text, wanted.line as usize))
        .then_some(offset)
    }
}

/// Convert a scalar position to LSP UTF-16 coordinates.
pub fn position(text: &Rope, offset: CharOffset) -> Option<Position> {
    Positions::new(text).position(text, offset)
}

/// Clamp overlong columns to line end and surrogate interiors backward.
/// Invalid line numbers are rejected.
pub fn offset(text: &Rope, position: Position) -> Option<CharOffset> {
    Positions::new(text).offset(text, position)
}

#[derive(Clone, Debug, Deserialize)]
pub struct Diagnostic {
    pub range: Range,
    #[serde(default)]
    pub severity: Option<u32>,
    pub message: String,
}

pub(crate) fn hover_text(value: &Value) -> String {
    fn append(value: &Value, output: &mut String) {
        if output.len() >= 64 << 10 {
            return;
        }
        if let Some(text) = value
            .as_str()
            .or_else(|| value.get("value").and_then(Value::as_str))
        {
            // Bound the displayed payload without cutting UTF-8.
            output.extend(text.chars().take(16_384));
            output.push('\n');
        } else if let Some(values) = value.as_array() {
            for value in values {
                append(value, output);
            }
        }
    }
    let mut output = String::new();
    append(&value["contents"], &mut output);
    output.trim().to_owned()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub path: PathBuf,
    pub position: Position,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    #[test]
    fn framing_counts_utf8_bytes_and_handles_consecutive_messages_and_bad_lengths() {
        let value = json!({"message": "界🦀"});
        let mut bytes = Vec::new();
        write_message(&mut bytes, &value).unwrap();
        write_message(&mut bytes, &Value::Null).unwrap();
        let mut reader = Cursor::new(bytes);
        assert_eq!(read_message(&mut reader).unwrap(), Some(value));
        assert_eq!(read_message(&mut reader).unwrap(), Some(Value::Null));
        assert!(read_message(&mut reader).unwrap().is_none());
        for text in [
            "Content-Length: 999999999\r\n\r\n",
            "Content-Length: 4\r\nContent-Length: 4\r\n\r\nnull",
            "X: a\r\n\r\n",
            "Content-Length: 20\r\n\r\n{}",
        ] {
            assert!(read_message(&mut Cursor::new(text)).is_err());
        }
    }

    #[test]
    fn utf16_positions_handle_surrogates_crlf_and_unicode_separators() {
        let text = Rope::from_str("a🦀e\u{301}\r\n界\u{2028}x\ry\nz");
        assert_eq!(
            position(&text, CharOffset(2)),
            Some(Position {
                line: 0,
                character: 3
            })
        );
        assert_eq!(
            offset(
                &text,
                Position {
                    line: 0,
                    character: 2
                }
            ),
            Some(CharOffset(1))
        );
        assert_eq!(
            offset(
                &text,
                Position {
                    line: 1,
                    character: 2
                }
            ),
            Some(CharOffset(8))
        );
        assert_eq!(
            offset(
                &text,
                Position {
                    line: 0,
                    character: 100
                }
            ),
            Some(CharOffset(4))
        );
        assert_eq!(
            offset(
                &text,
                Position {
                    line: 9,
                    character: 0
                }
            ),
            None
        );
        for index in 0..=text.len_chars() {
            if index > 0
                && index < text.len_chars()
                && text.char(index - 1) == '\r'
                && text.char(index) == '\n'
            {
                continue;
            }
            assert_eq!(
                offset(&text, position(&text, CharOffset(index)).unwrap()),
                Some(CharOffset(index))
            );
        }
    }

    #[test]
    fn indexed_columns_match_flat_utf16_across_chunks_and_line_ending_variants() {
        let source = format!(
            "{}\r\n{}\r{}\n",
            "ab🦀e\u{301}\u{2028}".repeat(900),
            "界".repeat(1200),
            "x".repeat(2000)
        );
        let text = Rope::from_str(&source);
        let positions = Positions::new(&text);
        for line in 0..positions.starts.len() {
            let start = positions.starts[line];
            let content: Vec<_> = text
                .slice(start..)
                .chars()
                .take_while(|ch| !matches!(ch, '\r' | '\n'))
                .collect();
            for column in
                (0..=(content.iter().map(|ch| ch.len_utf16()).sum::<usize>() + 8)).step_by(13)
            {
                let mut units = 0;
                let scalars = content
                    .iter()
                    .take_while(|ch| {
                        units += ch.len_utf16();
                        units <= column
                    })
                    .count();
                assert_eq!(
                    positions.offset(
                        &text,
                        Position {
                            line: line as u32,
                            character: column as u32
                        }
                    ),
                    Some(CharOffset(start + scalars))
                );
                let actual = positions
                    .position(&text, CharOffset(start + scalars))
                    .unwrap();
                assert_eq!(
                    actual.character as usize,
                    content[..scalars]
                        .iter()
                        .map(|ch| ch.len_utf16())
                        .sum::<usize>()
                );
            }
        }
    }

    #[test]
    #[ignore = "manual release-mode coordinate benchmark"]
    fn benchmark_indexed_lsp_columns() {
        use std::{hint::black_box, time::Instant};
        for mib in [1usize, 8] {
            let text = Rope::from_str(&"a🦀".repeat((mib << 20) / 5));
            let positions = Positions::new(&text);
            let queries: Vec<_> = (0..128)
                .map(|i| ((text.len_utf16_cu() - 1) * (i + 128) / 256) as u32)
                .collect();
            let now = Instant::now();
            let indexed: Vec<_> = queries
                .iter()
                .map(|&character| {
                    positions
                        .offset(black_box(&text), Position { line: 0, character })
                        .unwrap()
                })
                .collect();
            let fast = now.elapsed();
            let now = Instant::now();
            let flat: Vec<_> = queries
                .iter()
                .map(|&character| {
                    let mut units = 0;
                    CharOffset(
                        black_box(&text)
                            .chars()
                            .take_while(|ch| {
                                units += ch.len_utf16() as u32;
                                units <= character
                            })
                            .count(),
                    )
                })
                .collect();
            let slow = now.elapsed();
            assert_eq!(indexed, flat);
            println!(
                "{mib} MiB, 128 columns, cached line index: indexed={fast:?}, sequential={slow:?}"
            );
        }
    }

    #[test]
    fn file_uris_round_trip_spaces_unicode_and_reserved_characters() {
        let path = std::env::temp_dir().join("界 #?%.rs");
        assert_eq!(file_path(&file_uri(&path).unwrap()).unwrap(), path);
        assert!(file_path("https://example.com/code.rs").is_err());
    }
}
