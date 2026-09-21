//! Small presentation helpers shared by menus. Work is bounded by visible text.

use crate::screen::{Frame, Style};
use unicode_segmentation::{GraphemeCursor, UnicodeSegmentation};
use unicode_width::UnicodeWidthStr;
use vex_core::display;

pub(crate) struct Label<'a> {
    text: &'a str,
    middle: bool,
    matches: &'a [usize],
}

impl<'a> Label<'a> {
    pub fn new(text: &'a str) -> Self {
        Self {
            text,
            middle: false,
            matches: &[],
        }
    }

    /// Retain both ends, so paths and similarly named files stay distinguishable.
    pub fn middle(mut self) -> Self {
        self.middle = true;
        self
    }

    /// Match offsets must be sorted UTF-8 character boundaries in the original label.
    pub fn matched(mut self, matches: &'a [usize]) -> Self {
        self.matches = matches;
        self
    }

    pub fn paint(&self, frame: &mut Frame, x: u16, y: u16, width: u16, style: Style) {
        let width = usize::from(width.min(frame.width().saturating_sub(x)));
        if width == 0 {
            return;
        }
        let (end, _) = prefix(self.text, width);
        let truncated = end < self.text.len();
        let (end, suffix) = if !truncated {
            (end, self.text.len())
        } else if self.middle {
            let (end, used) = prefix(self.text, (width - 1) / 2);
            let mut room = width - 1 - used;
            let mut start = self.text.len();
            for (byte, grapheme) in self.text.grapheme_indices(true).rev() {
                let size = display::visible(grapheme).width();
                if size > room {
                    break;
                }
                room -= size;
                start = byte;
            }
            (end, start)
        } else {
            (prefix(self.text, width - 1).0, self.text.len())
        };
        let mut parts = [
            (Some(0), &self.text[..end]),
            (None, if truncated { "…" } else { "" }),
            (Some(suffix), &self.text[suffix..]),
        ];
        // If eliding the middle would hide every match, show the matching
        // context instead. This matters especially for workspace-search snippets.
        if truncated
            && self.middle
            && width >= 3
            && let Some(&focus) = self.matches.first()
            && self.matches.iter().all(|&m| end <= m && m < suffix)
        {
            let mut cursor = GraphemeCursor::new(focus, self.text.len(), true);
            let focus = if cursor.is_boundary(self.text, 0).expect("complete label") {
                focus
            } else {
                cursor
                    .prev_boundary(self.text, 0)
                    .expect("complete label")
                    .unwrap_or(0)
            };
            let mut start = focus;
            let mut room = (width - 2) / 2;
            for (byte, grapheme) in self.text[..focus].grapheme_indices(true).rev() {
                let size = display::visible(grapheme).width();
                if size > room {
                    break;
                }
                room -= size;
                start = byte;
            }
            let end = start + prefix(&self.text[start..], width - 2).0;
            parts = [
                (None, if start > 0 { "…" } else { "" }),
                (Some(start), &self.text[start..end]),
                (None, if end < self.text.len() { "…" } else { "" }),
            ];
        }
        let mut col = x;
        for (offset, text) in parts {
            for (byte, grapheme) in text.grapheme_indices(true) {
                let found = offset.is_some_and(|offset| {
                    let at = offset + byte;
                    let index = self.matches.partition_point(|&m| m < at);
                    self.matches
                        .get(index)
                        .is_some_and(|&m| m < at + grapheme.len())
                });
                frame.put(
                    col,
                    y,
                    grapheme,
                    if found { Style::PickerMatch } else { style },
                );
                col += display::visible(grapheme).width() as u16;
            }
        }
    }
}

fn prefix(text: &str, width: usize) -> (usize, usize) {
    let mut columns = 0;
    for (byte, grapheme) in text.grapheme_indices(true) {
        let size = display::visible(grapheme).width();
        if size > width - columns {
            return (byte, columns);
        }
        columns += size;
    }
    (text.len(), columns)
}

/// Reuse a box's bottom border for position feedback without taking a text row.
pub(crate) fn menu_position(
    frame: &mut Frame,
    left: u16,
    right: u16,
    bottom: u16,
    selected: Option<usize>,
    total: usize,
    visible: usize,
) {
    if total <= visible || bottom == 0 {
        return;
    }
    let text = selected.map_or_else(
        || format!(" {total} items "),
        |i| format!(" {}/{total} ", i + 1),
    );
    if text.len() + 4 <= usize::from(right.saturating_sub(left)) {
        crate::picker::label(
            frame,
            right - 2 - text.len() as u16,
            bottom - 1,
            text.len() as u16,
            &text,
            Style::Gutter,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipped_unicode_labels_keep_original_match_offsets_and_never_touch_neighbors() {
        let text = "界e\u{301}/very-long-directory/file🦀.rs";
        let matched = [text.find('界').unwrap(), text.find('🦀').unwrap()];
        for width in 0..=45 {
            let mut frame = Frame::default();
            frame.reset(50, 1).unwrap();
            frame.fill_row(0, Style::Error);
            Label::new(text)
                .middle()
                .matched(&matched)
                .paint(&mut frame, 2, 0, width, Style::Text);
            assert_eq!(frame.style_at(1, 0), Some(Style::Error));
            assert_eq!(frame.style_at(2 + width, 0), Some(Style::Error));
            let row = frame.row_text(0);
            if width >= 24 {
                assert!(row.contains("file🦀.rs"), "{width}: {row}");
                let crab = row[..row.find('🦀').unwrap()].width() as u16;
                assert_eq!(frame.style_at(crab, 0), Some(Style::PickerMatch));
            }
            if (1..text.width()).contains(&usize::from(width)) {
                assert!(row.contains('…'));
            }
        }
        let text = "a/long/path/to/e\u{301}vidence/followed/by/a/long/snippet";
        let matched = [text.find('\u{301}').unwrap()];
        let mut frame = Frame::default();
        frame.reset(20, 1).unwrap();
        Label::new(text)
            .middle()
            .matched(&matched)
            .paint(&mut frame, 0, 0, 20, Style::Text);
        assert!(frame.row_text(0).contains("e\u{301}vidence"));
        assert!((0..20).any(|col| frame.style_at(col, 0) == Some(Style::PickerMatch)));
    }
}
