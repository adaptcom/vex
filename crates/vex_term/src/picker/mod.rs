//! Reusable picker presentation and selection state. Providers own discovery and
//! matching; the view only receives bounded results and paints visible rows.

pub(crate) mod files;
mod fuzzy;
mod ignore;

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
    pub notice: String,
    pub preview: String,
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
            notice: String::new(),
            preview: String::new(),
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
        self.preview.clear();
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
        let width = frame.width();
        let height = frame.height();
        if width == 0 || height == 0 {
            return;
        }
        let x = if width >= 40 { 2 } else { 0 };
        let y = if height >= 10 { 1 } else { 0 };
        let right = width - x;
        let bottom = height - y;
        for row in y..bottom {
            for col in x..right {
                frame.put(col, row, " ", Style::Text);
            }
        }
        for col in x..right {
            frame.put(col, y, " ", Style::Status);
        }
        label(
            frame,
            x,
            y,
            right - x,
            &format!(" Files · {}", self.title),
            Style::Status,
        );
        for col in x..right {
            frame.put(col, bottom - 1, " ", Style::Status);
        }
        if bottom - y < 5 {
            label(
                frame,
                x,
                bottom - 1,
                right - x,
                "Esc close · resize for picker",
                Style::Status,
            );
            frame.cursor = None;
            return;
        }
        frame.put(x, y + 1, ">", Style::Message);
        // Keep the prompt caret visible, scrolling at grapheme boundaries.
        let room = usize::from((right - x).saturating_sub(3));
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
            x.saturating_add(2),
            y + 1,
            (right - x).saturating_sub(2),
            &self.query.text()[start..],
            Style::Text,
        );
        frame.cursor = (right - x >= 3).then_some(Cursor {
            x: (x + 2 + columns as u16).min(right - 1),
            y: y + 1,
            shape: CursorShape::Bar,
        });
        let preview = right - x >= 90;
        let divider = if preview { x + (right - x) / 2 } else { right };
        let rows = usize::from(bottom - y - 3);
        self.top = self.top.min(self.selected);
        if self.selected >= self.top + rows {
            self.top = self.selected + 1 - rows;
        }
        if self.items.is_empty() {
            label(
                frame,
                x + 1,
                y + 2,
                divider.saturating_sub(x + 1),
                if self.pending {
                    "Scanning / matching…"
                } else {
                    "No matching files"
                },
                Style::Gutter,
            );
        }
        for (offset, item) in self.items.iter().enumerate().skip(self.top).take(rows) {
            let row = y + 2 + (offset - self.top) as u16;
            let selected = offset == self.selected;
            let base = if selected {
                Style::Selection
            } else {
                Style::Text
            };
            for col in x..divider {
                frame.put(col, row, " ", base);
            }
            frame.put(x, row, if selected { ">" } else { " " }, base);
            let mut col = x + 2;
            for (byte, grapheme) in item.entry.label.grapheme_indices(true) {
                let size = display::visible(grapheme).width() as u16;
                if col.saturating_add(size) > divider.saturating_sub(1) {
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
        if preview {
            for row in y + 2..bottom - 1 {
                frame.put(divider, row, "│", Style::Gutter);
            }
            for (offset, line) in self.preview.lines().take(rows).enumerate() {
                label(
                    frame,
                    divider + 2,
                    y + 2 + offset as u16,
                    right.saturating_sub(divider + 2),
                    line,
                    Style::Text,
                );
            }
        }
        let status = if !self.notice.is_empty() {
            self.notice.clone()
        } else {
            format!(
                " {}/{} matches · {} files{} · ↑↓ move · Enter open · Esc close",
                self.items.len(),
                self.matched,
                self.total,
                if self.pending { " · scanning" } else { "" }
            )
        };
        label(frame, x, bottom - 1, right - x, &status, Style::Status);
    }
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
