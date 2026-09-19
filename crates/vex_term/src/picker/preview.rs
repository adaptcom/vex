//! Immutable preview text and semantic colors, prepared together by a worker.

use crate::screen::{Frame, Style};
use std::{num::NonZeroUsize, sync::Arc};
use unicode_segmentation::UnicodeSegmentation;
use vex_core::display;
use vex_editor::HighlightSpan;

#[derive(Default)]
pub(crate) struct Preview {
    pub text: String,
    pub highlights: Arc<[HighlightSpan]>,
}

impl Preview {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }

    /// Draw only visible rows/columns, preserving byte offsets across line
    /// endings, tabs, and Unicode graphemes. No parsing happens while drawing.
    pub fn paint(&self, frame: &mut Frame, x: u16, y: u16, width: u16, rows: usize) {
        let width = usize::from(width.min(frame.width().saturating_sub(x)));
        let rows = rows.min(usize::from(frame.height().saturating_sub(y)));
        let mut byte = 0;
        let mut highlight_index = 0;
        for (row, line) in self.text.split_inclusive('\n').take(rows).enumerate() {
            let mut column = 0;
            for (offset, grapheme) in line.trim_end_matches(['\r', '\n']).grapheme_indices(true) {
                if column >= width {
                    break;
                }
                while highlight_index < self.highlights.len()
                    && self.highlights[highlight_index].range.end.0 <= byte + offset
                {
                    highlight_index += 1;
                }
                let style = self
                    .highlights
                    .get(highlight_index)
                    .filter(|span| span.range.start.0 <= byte + offset)
                    .map_or(Style::Text, |span| Style::Syntax(span.highlight));
                let size = display::width(grapheme, column, NonZeroUsize::new(4).unwrap());
                let col = x + column as u16;
                let row = y + row as u16;
                if grapheme == "\t" {
                    for delta in 0..size.min(width - column) {
                        frame.put(col + delta as u16, row, " ", style);
                    }
                } else if column + size <= width {
                    frame.put(col, row, grapheme, style);
                } else {
                    break;
                }
                column += size;
            }
            byte += line.len();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vex_core::ByteOffset;
    use vex_editor::Highlight;

    #[test]
    fn colors_follow_bytes_across_unicode_crlf_tabs_and_clipped_lines() {
        let text = "// 界e\u{301}\r\n\tlet s = \"界e\u{301}\";\ntrailer";
        let keyword = text.find("let").unwrap();
        let string = text.find('"').unwrap();
        let preview = Preview {
            text: text.into(),
            highlights: vec![
                HighlightSpan {
                    range: ByteOffset(0)..ByteOffset(keyword - 3),
                    highlight: Highlight::Comment,
                },
                HighlightSpan {
                    range: ByteOffset(keyword)..ByteOffset(keyword + 3),
                    highlight: Highlight::Keyword,
                },
                HighlightSpan {
                    range: ByteOffset(string)..ByteOffset(text.rfind('"').unwrap() + 1),
                    highlight: Highlight::String,
                },
            ]
            .into(),
        };
        let mut frame = Frame::default();
        frame.reset(24, 4).unwrap();
        for row in 0..4 {
            frame.label(0, row, "########################", Style::Gutter);
        }
        preview.paint(&mut frame, 2, 1, 14, 2);
        assert!(frame.row_text(1).starts_with("##// 界e\u{301}"));
        assert!(frame.row_text(2).starts_with("##    let s = \"#")); // Wide glyph clipped as a whole.
        assert_eq!(
            frame.style_at(5, 1),
            Some(Style::Syntax(Highlight::Comment))
        );
        assert_eq!(
            frame.style_at(6, 2),
            Some(Style::Syntax(Highlight::Keyword))
        );
        assert_eq!(
            frame.style_at(14, 2),
            Some(Style::Syntax(Highlight::String))
        );
        assert_eq!(frame.style_at(16, 2), Some(Style::Gutter));
        assert_eq!(frame.row_text(3), "########################");
    }
}
