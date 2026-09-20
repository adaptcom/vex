//! Reusable picker presentation and selection state. Providers own discovery and
//! matching; the view only receives bounded results and paints visible rows.

pub(crate) mod buffers;
pub(crate) mod files;
mod fuzzy;
mod ignore;
mod preview;
pub(crate) mod symbols;

pub(crate) use preview::Preview;

use crate::{
    input::Prompt,
    screen::{Cursor, CursorShape, Frame, Style},
};
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use vex_core::display;
use vex_editor::Key;

const MAX_QUERY_BYTES: usize = 1024;

/// Shared geometry for drawing, paging, and deciding whether to load a preview.
/// Bounds are exclusive; margins leave the current document visible behind us.
pub(crate) struct Layout {
    left: u16,
    top: u16,
    right: u16,
    bottom: u16,
}

impl Layout {
    pub fn new(width: u16, height: u16) -> Self {
        let x = if width >= 40 { width / 12 } else { 0 };
        let y = if height >= 10 { height / 8 } else { 0 };
        Self {
            left: x,
            top: y,
            right: width - x,
            bottom: height - y,
        }
    }

    pub fn rows(&self) -> u16 {
        if self.right - self.left < 5 {
            return 0;
        }
        (self.bottom - self.top).saturating_sub(6)
    }

    pub fn preview_left(&self) -> Option<u16> {
        (self.rows() > 0 && self.right - self.left >= 82)
            .then_some(self.left + (self.right - self.left) / 2 + 1)
    }

    fn list_right(&self) -> u16 {
        // Two untouched columns separate the independently bordered panels.
        self.preview_left().map_or(self.right, |left| left - 2)
    }
}

pub(crate) struct Entry<T> {
    pub label: String,
    pub value: T,
}
pub(crate) struct Item<T> {
    pub entry: Arc<Entry<T>>,
    pub matched: Vec<usize>,
}

pub(crate) enum Action {
    None,
    Query,
    Selection,
    Accept,
    Cancel,
}

pub(crate) struct Picker<T> {
    pub query: Prompt,
    pub items: Vec<Item<T>>,
    selected: usize,
    top: usize,
    touched: bool,
    pub pending: bool,
    pub matched: usize,
    pub total: usize,
    pub title: String,
    pub noun: &'static str,
    pub notice: String,
    pub preview: Preview,
}

impl<T: Eq> Picker<T> {
    pub fn new(title: String) -> Self {
        Self {
            query: Prompt::default(),
            items: Vec::new(),
            selected: 0,
            top: 0,
            touched: false,
            pending: true,
            matched: 0,
            total: 0,
            title,
            noun: "files",
            notice: String::new(),
            preview: Preview::default(),
        }
    }

    pub fn selected(&self) -> Option<&Arc<Entry<T>>> {
        self.items.get(self.selected).map(|item| &item.entry)
    }

    pub fn replace(&mut self, items: Vec<Item<T>>) {
        let selected = self
            .selected()
            .filter(|_| self.touched)
            .and_then(|old| items.iter().position(|item| item.entry.value == old.value));
        self.items = items;
        self.selected = selected.unwrap_or(0);
    }

    pub fn handle(&mut self, key: Key, page: usize) -> Action {
        match key {
            Key::Escape | Key::Ctrl('c') => return Action::Cancel,
            Key::Enter => return Action::Accept,
            Key::Down | Key::Ctrl('n') | Key::Tab => self.navigate(1),
            Key::Up | Key::Ctrl('p') | Key::BackTab => self.navigate(-1),
            Key::PageDown | Key::Ctrl('d') => self.navigate(page.max(1) as isize),
            Key::PageUp | Key::Ctrl('u') => self.navigate(-(page.max(1) as isize)),
            _ => {
                let before = self.query.text().to_owned();
                if !matches!(key, Key::Char(ch) if self.query.text().len() + ch.len_utf8() > MAX_QUERY_BYTES)
                {
                    self.query.handle(key);
                }
                if self.query.text() != before {
                    self.changed();
                    return Action::Query;
                }
                return Action::None;
            }
        }
        Action::Selection
    }

    pub fn paste(&mut self, text: &str) {
        let mut room = MAX_QUERY_BYTES.saturating_sub(self.query.text().len());
        let text: String = text
            .chars()
            .filter(|c| !c.is_control())
            .take_while(|c| {
                if c.len_utf8() > room {
                    false
                } else {
                    room -= c.len_utf8();
                    true
                }
            })
            .collect();
        self.query.insert(&text);
        self.changed();
    }

    fn changed(&mut self) {
        self.items.clear();
        self.selected = 0;
        self.top = 0;
        self.touched = false;
        self.pending = true;
        self.matched = 0;
        self.preview = Preview::default();
        self.notice.clear();
    }

    fn navigate(&mut self, delta: isize) {
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.items.len().saturating_sub(1));
        self.touched = true;
    }

    pub fn paint(&mut self, frame: &mut Frame) {
        frame.cursor = None;
        let layout = Layout::new(frame.width(), frame.height());
        let Layout {
            left, top, bottom, ..
        } = layout;
        let right = layout.list_right();
        paint_box(
            frame,
            left,
            top,
            right,
            bottom,
            &format!(" {} ", self.title),
        );
        if let Some(preview_left) = layout.preview_left() {
            let title = self.selected().map_or_else(
                || " Preview ".to_owned(),
                |entry| format!(" Preview · {} ", entry.label),
            );
            paint_box(frame, preview_left, top, layout.right, bottom, &title);
            self.preview.paint(
                frame,
                preview_left + 2,
                top + 1,
                layout.right - preview_left - 3,
                usize::from(bottom - top - 2),
            );
        }
        if right - left < 2 || bottom - top < 2 {
            return;
        }
        let rows = usize::from(layout.rows());
        if rows == 0 {
            if bottom - top > 2 {
                label(
                    frame,
                    left + 1,
                    top + 1,
                    right - left - 2,
                    "Esc close · resize for picker",
                    Style::Gutter,
                );
            }
            return;
        }
        // From here x/right and y/bottom describe the inside of the border.
        let x = left + 1;
        let right = right - 1;
        let y = top + 1;
        let bottom = bottom - 1;
        rule(frame, left, right + 1, y + 1, "├", "┤");
        rule(frame, left, right + 1, bottom - 2, "├", "┤");
        for col in x..right {
            frame.put(col, bottom - 1, " ", Style::Status);
        }
        frame.put(x, y, ">", Style::Message);
        // Keep the prompt caret visible, scrolling at grapheme boundaries.
        let room = usize::from(right - x - 2);
        let before = &self.query.text()[..self.query.cursor()];
        let mut start = self.query.cursor();
        let mut columns = 0;
        for (byte, grapheme) in before.grapheme_indices(true).rev() {
            let size = display::visible(grapheme).width();
            if columns + size >= room {
                break;
            }
            start = byte;
            columns += size;
        }
        label(
            frame,
            x + 2,
            y,
            right - x - 2,
            &self.query.text()[start..],
            Style::Text,
        );
        frame.cursor = Some(Cursor {
            x: x + 2 + columns as u16,
            y,
            shape: CursorShape::Bar,
        });
        self.top = self.top.min(self.selected);
        if self.selected >= self.top + rows {
            self.top = self.selected + 1 - rows;
        }
        if self.items.is_empty() {
            let message = if self.pending {
                "Loading / matching…".into()
            } else {
                format!("No matching {}", self.noun)
            };
            label(frame, x + 1, y + 2, right - x - 1, &message, Style::Gutter);
        }
        for (offset, item) in self.items.iter().enumerate().skip(self.top).take(rows) {
            let row = y + 2 + (offset - self.top) as u16;
            let selected = offset == self.selected;
            let base = if selected {
                Style::Selection
            } else {
                Style::Text
            };
            for col in x..right {
                frame.put(col, row, " ", base);
            }
            frame.put(x, row, if selected { ">" } else { " " }, base);
            let mut col = x + 2;
            for (byte, grapheme) in item.entry.label.grapheme_indices(true) {
                let size = display::visible(grapheme).width() as u16;
                if col.saturating_add(size) > right.saturating_sub(1) {
                    break;
                }
                let found = item
                    .matched
                    .iter()
                    .any(|&i| byte <= i && i < byte + grapheme.len());
                frame.put(
                    col,
                    row,
                    grapheme,
                    if found {
                        if selected {
                            Style::PickerSelectedMatch
                        } else {
                            Style::PickerMatch
                        }
                    } else {
                        base
                    },
                );
                col += size;
            }
        }
        let status = if !self.notice.is_empty() {
            self.notice.clone()
        } else {
            format!(
                " {}/{} matches · {} {}{} · ↑↓ move · Enter open · Esc close",
                self.items.len(),
                self.matched,
                self.total,
                self.noun,
                if self.pending { " · loading" } else { "" }
            )
        };
        label(frame, x, bottom - 1, right - x, &status, Style::Status);
    }
}

pub(crate) fn paint_box(
    frame: &mut Frame,
    left: u16,
    top: u16,
    right: u16,
    bottom: u16,
    title: &str,
) {
    for row in top..bottom {
        for col in left..right {
            frame.put(col, row, " ", Style::Text);
        }
    }
    if right - left < 2 || bottom - top < 2 {
        return;
    }
    for row in top + 1..bottom - 1 {
        frame.put(left, row, "│", Style::Gutter);
        frame.put(right - 1, row, "│", Style::Gutter);
    }
    rule(frame, left, right, top, "┌", "┐");
    rule(frame, left, right, bottom - 1, "└", "┘");
    label(
        frame,
        left + 2,
        top,
        (right - left).saturating_sub(4),
        title,
        Style::PopupTitle,
    );
}

fn rule(frame: &mut Frame, left: u16, right: u16, row: u16, start: &str, end: &str) {
    frame.put(left, row, start, Style::Gutter);
    for col in left + 1..right - 1 {
        frame.put(col, row, "─", Style::Gutter);
    }
    frame.put(right - 1, row, end, Style::Gutter);
}

/// Labels clipped to a panel rather than the whole terminal, with the same
/// grapheme/control handling as ordinary editor drawing.
pub(crate) fn label(frame: &mut Frame, mut x: u16, y: u16, width: u16, text: &str, style: Style) {
    let right = x.saturating_add(width).min(frame.width());
    for grapheme in text.graphemes(true) {
        let size = display::visible(grapheme).width() as u16;
        if x.saturating_add(size) > right {
            break;
        }
        frame.put(x, y, grapheme, style);
        x += size;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floating_boxes_preserve_background_and_clip_content_and_cursor_inside_borders() {
        let mut picker = Picker::new("界project".repeat(60));
        picker.paste(&"界e\u{301}".repeat(60));
        picker.replace(vec![Item {
            entry: Arc::new(Entry {
                label: "界filename".repeat(60),
                value: 0,
            }),
            matched: vec![0],
        }]);
        picker.preview = Preview::plain("界preview".repeat(60));
        for width in [0, 1, 2, 3, 4, 5, 39, 40, 80, 97, 98, 99, 120] {
            for height in [0, 1, 2, 3, 6, 7, 8, 9, 10, 18, 24] {
                let mut frame = Frame::default();
                frame.reset(width, height).unwrap();
                let background = "x".repeat(usize::from(width));
                for row in 0..height {
                    frame.label(0, row, &background, Style::Error);
                }
                picker.paint(&mut frame);
                let layout = Layout::new(width, height);
                for row in 0..height {
                    let text = frame.row_text(row);
                    if row < layout.top || row >= layout.bottom {
                        assert_eq!(text, background);
                    } else {
                        assert!(text.starts_with(&"x".repeat(usize::from(layout.left))));
                        assert!(text.ends_with(&"x".repeat(usize::from(width - layout.right))));
                    }
                }
                if layout.right - layout.left >= 2 && layout.bottom - layout.top >= 2 {
                    let boxes = 1 + usize::from(layout.preview_left().is_some());
                    assert_eq!(frame.row_text(layout.top).matches('┌').count(), boxes);
                    assert_eq!(frame.row_text(layout.top).matches('┐').count(), boxes);
                    assert_eq!(
                        frame.row_text(layout.bottom - 1).matches('└').count(),
                        boxes
                    );
                    assert_eq!(
                        frame.row_text(layout.bottom - 1).matches('┘').count(),
                        boxes
                    );
                    for row in layout.top + 1..layout.bottom - 1 {
                        assert_eq!(frame.style_at(layout.left, row), Some(Style::Gutter));
                        assert_eq!(frame.style_at(layout.right - 1, row), Some(Style::Gutter));
                    }
                }
                if let Some(preview_left) = layout.preview_left() {
                    for row in layout.top..layout.bottom {
                        for col in layout.list_right()..preview_left {
                            assert_eq!(frame.style_at(col, row), Some(Style::Error));
                        }
                        assert_eq!(
                            frame.style_at(layout.list_right() - 1, row),
                            Some(Style::Gutter)
                        );
                        assert_eq!(frame.style_at(preview_left, row), Some(Style::Gutter));
                    }
                }
                assert!(frame.cursor.is_none_or(|cursor| cursor.x > layout.left
                    && cursor.x < layout.list_right() - 1
                    && cursor.y > layout.top
                    && cursor.y < layout.bottom - 1));
            }
        }
    }

    #[test]
    fn picker_edits_query_navigates_preserves_selected_identity_and_clips_small_frames() {
        let mut picker = Picker::new("project".into());
        let items = || {
            (0..40)
                .map(|i| Item {
                    entry: Arc::new(Entry {
                        label: format!("界/file{i}.rs"),
                        value: i,
                    }),
                    matched: vec![0],
                })
                .collect()
        };
        picker.replace(items());
        picker.handle(Key::PageDown, 10);
        assert_eq!(picker.selected().unwrap().value, 10);
        let mut reordered: Vec<_> = items();
        reordered.reverse();
        picker.replace(reordered);
        assert_eq!(picker.selected().unwrap().value, 10);
        for (width, height) in [(0, 0), (1, 1), (2, 3), (40, 8), (120, 24)] {
            let mut frame = Frame::default();
            frame.reset(width, height).unwrap();
            picker.paint(&mut frame);
            assert!(frame.cursor.is_none_or(|c| c.x < width && c.y < height));
        }
        picker.paste("界e\u{301}\n:q!\x1b");
        assert_eq!(picker.query.text(), "界e\u{301}:q!");
        assert!(picker.items.is_empty());
        assert!(matches!(picker.handle(Key::Enter, 10), Action::Accept));
        picker.paste(&"界".repeat(2000));
        assert!(picker.query.text().len() <= MAX_QUERY_BYTES);
    }
}
