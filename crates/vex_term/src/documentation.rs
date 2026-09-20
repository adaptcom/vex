//! Cached wrapping and visible-row painting for prepared documentation.

use crate::{
    picker::{label, paint_box},
    screen::{Frame, Style},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use vex_core::display;
use vex_syntax::markup::{Attributes, Document, Line, Span};

#[derive(Clone, Copy)]
pub(crate) struct Area {
    pub left: u16,
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
}

impl Area {
    fn intersects(self, other: Self) -> bool {
        self.left < other.right
            && other.left < self.right
            && self.top < other.bottom
            && other.top < self.bottom
    }

    pub fn union(self, other: Self) -> Self {
        Self {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }
}

pub(crate) struct Popup {
    document: Document,
    preferred: u16,
    width: u16,
    rows: Vec<Line>,
    top: usize,
    visible: usize,
}

impl Popup {
    pub fn new(document: Document) -> Self {
        let preferred = document
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.text.width())
                    .sum::<usize>()
            })
            .max()
            .unwrap_or(0)
            .saturating_add(4)
            .clamp(28, 88) as u16;
        Self {
            document,
            preferred,
            width: 0,
            rows: Vec::new(),
            top: 0,
            visible: 1,
        }
    }

    pub fn scroll(&mut self, down: bool) {
        let count = (self.visible / 2).max(1);
        self.top = if down {
            self.top
                .saturating_add(count)
                .min(self.rows.len().saturating_sub(self.visible))
        } else {
            self.top.saturating_sub(count)
        };
    }

    fn layout(&mut self, width: u16) {
        if width == self.width {
            return;
        }
        self.width = width;
        self.rows.clear();
        for line in &self.document.lines {
            let mut row = Line {
                literal: line.literal,
                ..Line::default()
            };
            let mut column = 0usize;
            let mut wrapped = false;
            for span in &line.spans {
                for word in span.text.split_inclusive(char::is_whitespace) {
                    let word_width = word.trim_end_matches(char::is_whitespace).width();
                    if column > 0
                        && column + word_width > usize::from(width)
                        && word_width <= usize::from(width)
                    {
                        self.rows.push(std::mem::take(&mut row));
                        column = 0;
                        wrapped = true;
                    }
                    for grapheme in word.graphemes(true) {
                        // Do not retain/emit tens of thousands of combining
                        // bytes in one terminal cell on every redraw.
                        let grapheme = if grapheme.len() > 1024 {
                            "�"
                        } else {
                            grapheme
                        };
                        let tabs = std::num::NonZeroUsize::new(4).unwrap();
                        let mut size = display::width(grapheme, column, tabs);
                        if column + size > usize::from(width) && column > 0 {
                            self.rows.push(std::mem::take(&mut row));
                            column = 0;
                            wrapped = true;
                            size = display::width(grapheme, column, tabs);
                        }
                        if !line.literal
                            && wrapped
                            && column == 0
                            && grapheme.chars().all(char::is_whitespace)
                        {
                            continue;
                        }
                        if grapheme == "\t" {
                            for _ in 0..size.min(usize::from(width)) {
                                append(&mut row, " ", span.attributes);
                            }
                        } else if size > usize::from(width) {
                            append(&mut row, "�", span.attributes);
                            size = 1;
                        } else {
                            append(&mut row, grapheme, span.attributes);
                        }
                        column += size;
                    }
                }
            }
            self.rows.push(row);
        }
        self.top = self.top.min(self.rows.len().saturating_sub(1));
    }

    pub fn paint(&mut self, frame: &mut Frame, body: u16) {
        self.paint_at(frame, body, " Documentation ", false, None);
    }

    /// Signature help prefers space above the caret and leaves it visible.
    pub fn paint_signature(
        &mut self,
        frame: &mut Frame,
        body: u16,
        title: &str,
        avoid: Option<Area>,
    ) -> bool {
        self.paint_at(frame, body, title, true, avoid)
    }

    fn paint_at(
        &mut self,
        frame: &mut Frame,
        body: u16,
        title: &str,
        signature: bool,
        avoid: Option<Area>,
    ) -> bool {
        let Some(cursor) = frame.cursor else {
            return false;
        };
        let width = self.preferred.min(frame.width());
        if width < 8 {
            return false;
        }
        self.layout(width - 4);
        let below = body.saturating_sub(cursor.y.saturating_add(1));
        let above = cursor.y;
        let desired = self.rows.len().clamp(1, 22) as u16 + 2;
        let under = if signature {
            // Keep long documentation above a completion menu when at least
            // a few rows fit there; scrolling handles the remaining content.
            above < desired && above < 6 && below >= above
        } else {
            below >= desired || below >= above
        };
        let height = desired.min(if under { below } else { above });
        if height < 3 {
            return false;
        }
        let x = cursor.x.saturating_sub(1).min(frame.width() - width);
        let y = if under {
            cursor.y + 1
        } else {
            cursor.y - height
        };
        let area = Area {
            left: x,
            top: y,
            right: x + width,
            bottom: y + height,
        };
        if avoid.is_some_and(|other| area.intersects(other)) {
            return false;
        }
        self.visible = usize::from(height - 2);
        self.top = self.top.min(self.rows.len().saturating_sub(self.visible));
        paint_box(
            frame,
            x,
            y,
            x + width,
            y + height,
            if self.document.truncated && !signature {
                " Documentation · limited "
            } else {
                title
            },
        );
        for (index, line) in self
            .rows
            .iter()
            .skip(self.top)
            .take(self.visible)
            .enumerate()
        {
            let mut col = x + 2;
            for span in &line.spans {
                for grapheme in span.text.graphemes(true) {
                    let size = display::visible(grapheme).width() as u16;
                    if col + size > x + width - 2 {
                        break;
                    }
                    frame.put(
                        col,
                        y + 1 + index as u16,
                        grapheme,
                        Style::Markup(span.attributes),
                    );
                    col += size;
                }
            }
        }
        if self.rows.len() > self.visible {
            let footer = format!(" {}/{} ", self.top + 1, self.rows.len());
            label(
                frame,
                x + width.saturating_sub(footer.len() as u16 + 2),
                y + height - 1,
                width - 4,
                &footer,
                Style::Gutter,
            );
        }
        if !signature {
            frame.cursor = None;
        }
        true
    }
}

fn append(line: &mut Line, text: &str, attributes: Attributes) {
    if let Some(last) = line
        .spans
        .last_mut()
        .filter(|span| span.attributes == attributes)
    {
        last.text.push_str(text);
    } else {
        line.spans.push(Span {
            text: text.into(),
            attributes,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::{Cursor, CursorShape};

    #[test]
    fn excessive_combining_clusters_are_bounded_before_cached_redraw() {
        let mut popup = Popup::new(Document::plain(&format!("x{}", "\u{301}".repeat(20_000))));
        popup.layout(80);
        assert_eq!(popup.rows.len(), 1);
        assert_eq!(popup.rows[0].spans[0].text, "�");
    }

    #[test]
    fn popup_wraps_unicode_scrolls_and_preserves_surrounding_document_on_resize() {
        let doc = Document::markdown(
            "# Heading\n\n**Bold** with 界 and e\u{301} wraps over several lines.\n\n- First\n- Second\n\n```rust\nfn main() {}\n```",
            || false,
        );
        let mut popup = Popup::new(doc);
        let mut frame = Frame::default();
        frame.reset(40, 16).unwrap();
        frame.label(0, 0, "original file", Style::Text);
        frame.cursor = Some(Cursor {
            x: 3,
            y: 1,
            shape: CursorShape::Block,
        });
        popup.paint(&mut frame, 14);
        assert!(frame.row_text(0).contains("original file"));
        assert!((0..14).any(|row| frame.row_text(row).contains("Documentation")));
        assert!((0..14).any(|row| frame.row_text(row).contains("Heading")));
        assert!(!(0..14).any(|row| frame.row_text(row).contains("**")));
        for width in [8, 12, 20, 40] {
            frame.reset(width, 8).unwrap();
            frame.cursor = Some(Cursor {
                x: width - 1,
                y: 0,
                shape: CursorShape::Block,
            });
            popup.paint(&mut frame, 6);
            popup.scroll(true);
            assert!(popup.top > 0);
            frame.cursor = Some(Cursor {
                x: width - 1,
                y: 5,
                shape: CursorShape::Block,
            });
            popup.paint(&mut frame, 6);
            popup.scroll(false);
        }
    }

    #[test]
    #[ignore = "manual release-mode documentation layout and redraw benchmark"]
    fn benchmark_documentation_layout_and_redraw() {
        use std::{hint::black_box, time::Instant};
        let source = "Documentation with 界, e\u{301}, words and more words.\n\n".repeat(1500);
        let document = Document::markdown(&source, || false);
        let mut popup = Popup::new(document);
        let mut frame = Frame::default();
        frame.reset(100, 30).unwrap();
        let mut layout = Vec::new();
        let mut redraw = Vec::new();
        for _ in 0..200 {
            popup.width = 0;
            let start = Instant::now();
            popup.layout(popup.preferred.min(frame.width()) - 4);
            layout.push(start.elapsed());
            frame.cursor = Some(Cursor {
                x: 0,
                y: 0,
                shape: CursorShape::Block,
            });
            let start = Instant::now();
            popup.paint(black_box(&mut frame), 28);
            redraw.push(start.elapsed());
        }
        layout.sort_unstable();
        redraw.sort_unstable();
        eprintln!(
            "64KiB documentation: layout median {:?}, p95 {:?}; cached redraw median {:?}, p95 {:?}",
            layout[100], layout[189], redraw[100], redraw[189]
        );
    }
}
