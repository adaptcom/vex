//! Reusable picker presentation and selection state. Providers own discovery and
//! matching; the view only receives bounded results and paints visible rows.

pub(crate) mod browser;
pub(crate) mod buffers;
mod catalog;
pub(crate) mod diagnostics;
pub(crate) mod files;
mod fuzzy;
mod ignore;
pub(crate) mod jumps;
pub(crate) mod locations;
mod preview;
pub(crate) mod search;
pub(crate) mod symbols;

pub(crate) use preview::Preview;

use crate::{
    input::Prompt,
    screen::{Cursor, CursorShape, Frame, Style},
    ui::Label,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use vex_core::display;
use vex_editor::Key;

pub(crate) const MAX_QUERY_BYTES: usize = 1024;
const SPINNER_INTERVAL: Duration = Duration::from_millis(100);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

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
        // Keep the shared command line and bottom status line visible even on
        // short terminals; collapse outer margins before covering editor chrome.
        let body = height.saturating_sub(2);
        let y = if height >= 10 { height / 8 } else { 0 };
        Self {
            left: x,
            top: y.min(body),
            right: width - x,
            bottom: (height - y).min(body),
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
    Register(char),
    None,
    Query,
    Selection,
    Accept,
    Cancel,
    Parent,
}

/// Small navigation checkpoint; directory history need not retain result lists.
pub(crate) struct Bookmark<T> {
    query: Prompt,
    selected: Option<Arc<Entry<T>>>,
    top: usize,
}

pub(crate) struct Picker<T> {
    pub query: Prompt,
    pub items: Vec<Item<T>>,
    selected: usize,
    top: usize,
    touched: bool,
    restore: Option<Arc<Entry<T>>>,
    pub pending: bool,
    current: bool,
    pub matched: usize,
    pub total: usize,
    pub title: String,
    pub noun: &'static str,
    pub browse: bool,
    pub notice: String,
    pub preview: Preview,
    preview_title: String,
    pub preview_pending: bool,
    spinner: usize,
    spinner_due: Option<Instant>,
}

impl<T: Eq> Picker<T> {
    pub fn new(title: String) -> Self {
        Self {
            query: Prompt::default(),
            items: Vec::new(),
            selected: 0,
            top: 0,
            touched: false,
            restore: None,
            pending: true,
            current: false,
            matched: 0,
            total: 0,
            title,
            noun: "files",
            browse: false,
            notice: String::new(),
            preview: Preview::default(),
            preview_title: String::new(),
            preview_pending: false,
            spinner: 0,
            spinner_due: None,
        }
    }

    pub fn selected(&self) -> Option<&Arc<Entry<T>>> {
        self.items.get(self.selected).map(|item| &item.entry)
    }

    pub fn bookmark(&mut self) -> Bookmark<T> {
        Bookmark {
            query: std::mem::take(&mut self.query),
            selected: self.restore.clone().or_else(|| self.selected().cloned()),
            top: self.top,
        }
    }

    pub fn restore_bookmark(&mut self, bookmark: Bookmark<T>) {
        self.query = bookmark.query;
        self.restore = bookmark.selected;
        self.top = bookmark.top;
        self.touched = true;
    }

    pub fn select_on_refresh(&mut self, value: T) {
        self.restore = Some(Arc::new(Entry {
            label: String::new(),
            value,
        }));
        self.touched = true;
    }

    /// Retained rows remain drawable, but only current rows may be accepted.
    pub fn current(&self) -> bool {
        self.current
    }

    pub fn begin_update(&mut self) {
        self.current = false;
        self.pending = true;
        self.notice.clear();
    }

    pub fn set_preview(&mut self, preview: Preview) {
        self.preview_title = self
            .selected()
            .map_or_else(String::new, |entry| entry.label.clone());
        self.preview = preview;
        self.preview_pending = false;
    }

    pub fn clear_preview(&mut self) {
        self.preview = Preview::default();
        self.preview_title.clear();
        self.preview_pending = false;
    }

    pub fn animation_deadline(&self) -> Option<Instant> {
        (self.pending || self.preview_pending)
            .then_some(self.spinner_due)
            .flatten()
    }

    /// Animate only while work is outstanding, using the event loop's clock.
    pub fn poll_animation(&mut self, now: Instant) -> bool {
        if !self.pending && !self.preview_pending {
            self.spinner = 0;
            return self.spinner_due.take().is_some();
        }
        match self.spinner_due {
            Some(due) if now < due => false,
            due => {
                self.spinner = if due.is_some() {
                    (self.spinner + 1) % SPINNER.len()
                } else {
                    0
                };
                self.spinner_due = Some(now + SPINNER_INTERVAL);
                true
            }
        }
    }

    /// Keep the retained selection when refreshed results arrive after reopening.
    pub fn resume(&mut self) {
        self.restore = self.selected().cloned();
        self.touched = true;
        if self.query.register_pending() {
            self.query.register_key(Key::Escape);
        }
    }

    pub fn replace(&mut self, items: Vec<Item<T>>) {
        self.replace_incremental(items, true);
    }

    /// A reopened picker keeps its cached rows until the selected identity is
    /// rediscovered or the scan finishes. Early partial scans cannot lose it.
    pub fn replace_incremental(&mut self, items: Vec<Item<T>>, complete: bool) -> bool {
        if let Some(wanted) = &self.restore {
            if let Some(index) = items
                .iter()
                .position(|item| item.entry.value == wanted.value)
            {
                self.selected = index;
                self.items = items;
                self.restore = None;
                self.current = true;
                return true;
            }
            if !complete {
                return false;
            }
            self.restore = None;
        }
        // An empty partial scan isn't an empty final result. Keep the last
        // visible batch until this query produces rows or finishes.
        if !complete && items.is_empty() && !self.items.is_empty() {
            return false;
        }
        let selected = self
            .selected()
            .filter(|_| self.touched)
            .and_then(|old| items.iter().position(|item| item.entry.value == old.value));
        self.items = items;
        self.selected = selected.unwrap_or(0);
        self.current = true;
        true
    }

    pub fn handle(&mut self, key: Key, page: usize) -> Action {
        if let Some(name) = self.query.register_key(key) {
            return name.map_or(Action::None, Action::Register);
        }
        match key {
            Key::Escape | Key::Ctrl('c') => return Action::Cancel,
            Key::Enter => return Action::Accept,
            Key::Backspace if self.browse && self.query.text().is_empty() => return Action::Parent,
            Key::Down | Key::Ctrl('n') | Key::Tab => self.navigate(1),
            Key::Up | Key::Ctrl('p') | Key::BackTab => self.navigate(-1),
            Key::PageDown | Key::Ctrl('d') => self.navigate(page.max(1) as isize),
            Key::PageUp | Key::Ctrl('u') => self.navigate(-(page.max(1) as isize)),
            _ => {
                if !matches!(key, Key::Char(ch) if self.query.text().len() + ch.len_utf8() > MAX_QUERY_BYTES)
                    && self.query.handle(key)
                {
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
        self.restore = None;
        self.touched = false;
        self.begin_update();
    }

    fn navigate(&mut self, delta: isize) {
        self.restore = None;
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
        paint_box(frame, left, top, right, bottom, "");
        if bottom - top >= 2 {
            Label::new(&format!(" {} ", self.title)).middle().paint(
                frame,
                left + 2,
                top,
                (right - left).saturating_sub(4 + if self.pending { 3 } else { 0 }),
                Style::PopupTitle,
            );
        }
        if self.pending {
            self.paint_spinner(frame, left, top, right, bottom);
        }
        if let Some(preview_left) = layout.preview_left() {
            let title = if self.preview_title.is_empty() {
                " Preview ".to_owned()
            } else {
                format!(" Preview · {} ", self.preview_title)
            };
            paint_box(frame, preview_left, top, layout.right, bottom, "");
            Label::new(&title).middle().paint(
                frame,
                preview_left + 2,
                top,
                (layout.right - preview_left)
                    .saturating_sub(4 + if self.preview_pending { 3 } else { 0 }),
                Style::PopupTitle,
            );
            if self.preview_pending {
                self.paint_spinner(frame, preview_left, top, layout.right, bottom);
            }
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
                    "Resize to show picker",
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
        frame.put(x, y, ">", Style::PickerMarker);
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
        // A restored directory has no rows until its worker responds. Loading
        // frames must not discard the checkpoint's scroll position.
        if !(self.pending && self.items.is_empty() && self.restore.is_some()) {
            self.top = self.top.min(self.selected);
            if self.selected >= self.top + rows {
                self.top = self.selected + 1 - rows;
            }
            self.top = self.top.min(self.items.len().saturating_sub(rows));
        }
        if self.items.is_empty() && !self.pending && self.notice.is_empty() {
            let message = format!("No matching {}", self.noun);
            label(frame, x + 1, y + 2, right - x - 1, &message, Style::Gutter);
        }
        for (offset, item) in self.items.iter().enumerate().skip(self.top).take(rows) {
            let row = y + 2 + (offset - self.top) as u16;
            let selected = offset == self.selected;
            let base = Style::Text;
            frame.put(
                x,
                row,
                if selected { ">" } else { " " },
                Style::PickerMarker,
            );
            Label::new(&item.entry.label)
                .middle()
                .matched(&item.matched)
                .paint(frame, x + 2, row, right - x - 3, base);
        }
        if !self.notice.is_empty() {
            Label::new(&self.notice).paint(frame, x + 1, bottom - 1, right - x - 2, Style::Text);
        } else {
            let position = if self.items.is_empty() {
                0
            } else {
                self.selected + 1
            };
            let count = format!("{position}/{}", self.matched);
            let available = right - x - 2;
            let counts = if self.matched > self.items.len() {
                format!("{count} matches · {} shown", self.items.len())
            } else {
                format!("{count} matches · {} {}", self.total, self.noun)
            };
            let controls = if self.browse {
                "Enter open · BS parent"
            } else {
                "↑↓ move · Enter open"
            };
            let short_controls = if self.browse {
                controls
            } else {
                "↑↓ · Enter open"
            };
            let choices = [
                format!("{counts} · {controls}"),
                format!("{count} · {short_controls}"),
                format!("{count} · Enter open"),
                format!("{count} · Enter"),
                count,
            ];
            let status = choices
                .iter()
                .find(|s| s.width() <= usize::from(available))
                .unwrap_or(choices.last().unwrap());
            Label::new(status).paint(frame, x + 1, bottom - 1, available, Style::Text);
        }
    }

    fn paint_spinner(&self, frame: &mut Frame, left: u16, top: u16, right: u16, bottom: u16) {
        if right - left >= 7 && bottom - top >= 2 {
            frame.put(right - 4, top, " ", Style::PopupTitle);
            frame.put(right - 3, top, SPINNER[self.spinner], Style::PopupTitle);
            frame.put(right - 2, top, " ", Style::PopupTitle);
        }
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
    Label::new(title).middle().paint(
        frame,
        left + 2,
        top,
        (right - left).saturating_sub(4),
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
    fn directory_checkpoint_survives_loading_frames_and_restores_the_selected_row() {
        let items = || {
            (0..40)
                .map(|value| Item {
                    entry: Arc::new(Entry {
                        label: format!("entry{value:02}/"),
                        value,
                    }),
                    matched: Vec::new(),
                })
                .collect()
        };
        let mut picker = Picker::new("Browse".into());
        picker.browse = true;
        picker.paste("entry");
        picker.handle(Key::Left, 1);
        picker.replace(items());
        picker.pending = false;
        picker.navigate(30);
        let mut frame = Frame::default();
        frame.reset(80, 20).unwrap();
        picker.paint(&mut frame);
        picker.navigate(-4); // Selection sits above the bottom of the page.
        picker.paint(&mut frame);
        let before = (picker.top, picker.selected);
        assert!(before.0 > 0);
        let mut returned = Picker::new("Browse".into());
        returned.restore_bookmark(picker.bookmark());
        returned.paint(&mut frame);
        returned.paint(&mut frame);
        returned.replace(items());
        returned.pending = false;
        returned.paint(&mut frame);
        assert_eq!((returned.top, returned.selected), before);
        assert_eq!(returned.query.text(), "entry");
        assert_eq!(returned.query.cursor(), 4);
        assert_eq!(returned.selected().unwrap().value, 26);
    }

    #[test]
    fn growing_picker_fills_the_page_without_moving_selection_and_narrow_footer_keeps_controls() {
        let mut picker = Picker::new("Files".into());
        picker.replace(
            (0..20)
                .map(|value| Item {
                    entry: Arc::new(Entry {
                        label: format!("file-{value:02}.txt"),
                        value,
                    }),
                    matched: Vec::new(),
                })
                .collect(),
        );
        picker.pending = false;
        picker.matched = 20;
        picker.total = 20;
        picker.navigate(19);
        let mut frame = Frame::default();
        frame.reset(40, 12).unwrap();
        picker.paint(&mut frame);
        let narrow = Layout::new(40, 12);
        let footer = frame.row_text(narrow.bottom - 2);
        assert!(
            footer.contains("20/20") && footer.contains("Enter") && !footer.contains("Esc"),
            "{footer}"
        );
        frame.reset(80, 24).unwrap();
        picker.paint(&mut frame);
        let layout = Layout::new(80, 24);
        assert_eq!(picker.selected().unwrap().value, 19);
        assert_eq!(picker.top, 20 - usize::from(layout.rows()));
        assert!(frame.row_text(layout.top + 3).contains("file-08.txt"));
        assert!(frame.row_text(layout.bottom - 4).contains("file-19.txt"));
    }

    #[test]
    fn pending_queries_keep_rows_and_preview_with_only_a_marker_and_match_colors() {
        let mut picker = Picker::new("Files".into());
        picker.replace(vec![Item {
            entry: Arc::new(Entry {
                label: "alpha".into(),
                value: 1,
            }),
            matched: vec![0],
        }]);
        picker.pending = false;
        picker.set_preview(Preview::plain("old preview"));
        let selected = picker.selected().unwrap().clone();
        picker.paste("beta");
        assert!(!picker.current());
        assert!(Arc::ptr_eq(&selected, picker.selected().unwrap()));
        assert_eq!(picker.preview.text, "old preview");
        assert!(!picker.replace_incremental(vec![], false));
        assert!(Arc::ptr_eq(&selected, picker.selected().unwrap()));

        let mut frame = Frame::default();
        frame.reset(120, 24).unwrap();
        picker.paint(&mut frame);
        let layout = Layout::new(120, 24);
        let row = layout.top + 3;
        assert!(frame.row_text(row).contains("> alpha"));
        assert_eq!(
            frame.style_at(layout.left + 3, row),
            Some(Style::PickerMatch)
        );
        assert_eq!(frame.style_at(layout.left + 4, row), Some(Style::Text));
        assert!(
            (layout.left + 1..layout.list_right() - 1)
                .all(|x| frame.style_at(x, row) != Some(Style::Selection))
        );
        let text: String = (0..24).map(|row| frame.row_text(row)).collect();
        assert!(!text.contains("Loading") && !text.contains("loading"));
        assert!(text.contains("old preview") && text.contains("Preview · alpha"));

        assert!(picker.replace_incremental(vec![], true));
        picker.pending = false;
        assert!(picker.current() && picker.selected().is_none());
        frame.reset(120, 24).unwrap();
        picker.paint(&mut frame);
        assert!(frame.row_text(row).contains("No matching files"));
    }

    #[test]
    fn spinner_animates_idle_work_at_bounded_intervals_and_stops_when_both_jobs_finish() {
        let mut picker = Picker::<()>::new("Files".into());
        let now = Instant::now();
        assert!(picker.poll_animation(now));
        assert_eq!(picker.animation_deadline(), Some(now + SPINNER_INTERVAL));
        assert!(!picker.poll_animation(now + SPINNER_INTERVAL / 2));
        let mut frame = Frame::default();
        frame.reset(120, 24).unwrap();
        picker.paint(&mut frame);
        let layout = Layout::new(120, 24);
        assert!(frame.row_text(layout.top).contains(SPINNER[0]));
        assert!(picker.poll_animation(now + SPINNER_INTERVAL));
        frame.reset(120, 24).unwrap();
        picker.paint(&mut frame);
        assert!(frame.row_text(layout.top).contains(SPINNER[1]));
        picker.pending = false;
        picker.preview_pending = true;
        assert!(picker.animation_deadline().is_some());
        assert!(picker.poll_animation(now + SPINNER_INTERVAL * 2));
        frame.reset(120, 24).unwrap();
        picker.paint(&mut frame);
        assert_eq!(frame.row_text(layout.top).matches(SPINNER[2]).count(), 1);
        picker.preview_pending = false;
        assert!(picker.animation_deadline().is_none());
        assert!(picker.poll_animation(now + SPINNER_INTERVAL * 3));
        assert!(!picker.poll_animation(now + Duration::from_secs(10)));
        frame.reset(120, 24).unwrap();
        picker.paint(&mut frame);
        assert!(
            SPINNER
                .iter()
                .all(|glyph| !frame.row_text(layout.top).contains(glyph))
        );
    }

    #[test]
    fn reopening_keeps_scroll_and_selection_until_incremental_scan_finds_it() {
        let items = |values: &[usize]| {
            values
                .iter()
                .map(|&value| Item {
                    entry: Arc::new(Entry {
                        label: value.to_string(),
                        value,
                    }),
                    matched: Vec::new(),
                })
                .collect()
        };
        let mut picker = Picker::new("Files".into());
        picker.replace(items(&[10, 20, 30]));
        picker.navigate(2);
        picker.top = 1;
        picker.resume();
        picker.replace_incremental(items(&[1, 2]), false);
        assert_eq!(picker.selected().unwrap().value, 30);
        assert_eq!(picker.top, 1);
        picker.replace_incremental(items(&[1, 2, 10, 20, 30]), false);
        assert_eq!(picker.selected().unwrap().value, 30);
        picker.resume();
        picker.replace_incremental(items(&[1, 2]), true);
        assert_eq!(picker.selected().unwrap().value, 1);
        assert!(picker.restore.is_none());
    }

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
        assert_eq!(picker.items.len(), 40);
        assert_eq!(picker.selected().unwrap().value, 10);
        assert!(!picker.current());
        assert!(matches!(picker.handle(Key::Enter, 10), Action::Accept));
        picker.paste(&"界".repeat(2000));
        assert!(picker.query.text().len() <= MAX_QUERY_BYTES);
    }
}
