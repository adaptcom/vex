//! Paint only visible logical lines into our grid. No document-sized String is
//! created; the editor's layout cache skips the horizontally hidden line prefix.

use crate::screen::{Cursor, CursorShape, Frame, Style};
use std::num::NonZeroUsize;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use vex_core::{ByteOffset, CharOffset, display, grapheme, motion};
use vex_editor::{Editor, Mode};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Viewport {
    pub top_line: usize,
    pub left_column: usize,
    alignment: Option<Alignment>,
    browsing: Option<Alignment>,
}

/// A small stamp lets explicit alignment survive background redraws. Ordinary
/// cursor movement, edits, mode changes and resizing restore the scroll margin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Alignment {
    document: vex_core::DocumentId,
    revision: vex_core::Revision,
    selection: vex_core::Selection,
    mode: Mode,
    height: usize,
}

impl Alignment {
    fn new(editor: &Editor, height: usize) -> Self {
        Self {
            document: editor.document().id(),
            revision: editor.document().revision(),
            selection: editor.selections().primary(),
            mode: editor.mode(),
            height,
        }
    }
}

impl Viewport {
    pub(crate) fn hold(&mut self, editor: &Editor, height: usize) {
        self.browsing = None;
        self.alignment = Some(Alignment::new(editor, height));
    }

    /// Wheel browsing preserves the exact selections, including offscreen ones.
    pub(crate) fn scroll(
        &mut self,
        editor: &Editor,
        down: bool,
        lines: usize,
        size: (u16, u16),
    ) -> Result<bool, vex_core::Error> {
        let height = usize::from(size.1.saturating_sub(1));
        if height == 0 {
            return Ok(false);
        }
        self.ensure_visible(editor, usize::from(size.0), height)?;
        let before = self.top_line;
        let last = editor.document().text().len_lines().saturating_sub(height);
        self.top_line = if down {
            before.saturating_add(lines).min(last)
        } else {
            before.saturating_sub(lines).min(last)
        };
        self.browsing = Some(Alignment::new(editor, 0));
        self.alignment = None;
        Ok(before != self.top_line)
    }

    pub(crate) fn resume_following(&mut self) {
        self.browsing = None;
    }

    /// Apply cursor following without painting, including between batched keys.
    pub(crate) fn ensure_visible(
        &mut self,
        editor: &Editor,
        width: usize,
        height: usize,
    ) -> Result<(), vex_core::Error> {
        let primary = primary_cursor(editor)?;
        let column = editor.display_column(primary)?;
        let body_width = width - gutter(width, editor.document().text().len_lines()).width;
        self.follow(editor, primary, column, (body_width, height))
    }

    fn follow(
        &mut self,
        editor: &Editor,
        primary: CharOffset,
        column: usize,
        (width, height): (usize, usize),
    ) -> Result<(), vex_core::Error> {
        if height == 0 || width == 0 {
            return Ok(());
        }
        if self.browsing == Some(Alignment::new(editor, 0)) {
            self.top_line = self
                .top_line
                .min(editor.document().text().len_lines().saturating_sub(height));
            return Ok(());
        }
        self.browsing = None;
        let text = editor.document().text();
        let row = text.char_to_line(primary.0);
        let cursor_span = if editor.mode() != Mode::Insert && primary.0 < text.len_chars() {
            let end = grapheme::next(text, primary, 1)?;
            let slice = text.slice(primary.0..end.0);
            let owned;
            let cluster = match slice.as_str() {
                Some(cluster) => cluster,
                None => {
                    owned = slice.to_string();
                    &owned
                }
            };
            display::width(cluster, column, editor.tab_width()).min(width)
        } else {
            1
        };
        let margin = if self.alignment == Some(Alignment::new(editor, height)) {
            0
        } else {
            self.alignment = None;
            scroll_margin(height)
        };
        if row < self.top_line.saturating_add(margin) {
            self.top_line = row.saturating_sub(margin);
        } else if row >= self.top_line.saturating_add(height - margin) {
            self.top_line = row.saturating_add(margin + 1).saturating_sub(height);
        }
        if column < self.left_column {
            self.left_column = column;
        } else if column.saturating_add(cursor_span) > self.left_column.saturating_add(width) {
            self.left_column = column.saturating_add(cursor_span).saturating_sub(width);
        }
        Ok(())
    }
}

pub(crate) fn scroll_margin(height: usize) -> usize {
    3.min(height.saturating_sub(1) / 2)
}

pub(crate) fn primary_cursor(editor: &Editor) -> Result<CharOffset, vex_core::Error> {
    if editor.mode() == Mode::Insert {
        Ok(editor.selections().primary().head)
    } else {
        motion::cursor(editor.document().text(), editor.selections().primary())
    }
}

pub(crate) struct Gutter {
    pub width: usize,
    pub diagnostic: Option<u16>,
    pub diff: Option<u16>,
    number_start: u16,
    digits: usize,
}

pub(crate) fn gutter(width: usize, lines: usize) -> Gutter {
    let digits = lines.max(1).ilog10() as usize + 1;
    if width >= 12 {
        let digits = digits.min(width / 3);
        Gutter {
            width: digits + 4,
            diagnostic: Some(0),
            number_start: 1,
            digits,
            diff: Some((digits + 2) as u16),
        }
    } else if width >= 8 {
        let digits = digits.min(width / 3 - 1);
        Gutter {
            width: digits + 1,
            diagnostic: None,
            number_start: 0,
            digits,
            diff: None,
        }
    } else {
        Gutter {
            width: 0,
            diagnostic: None,
            number_start: 0,
            digits: 0,
            diff: None,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Chrome<'a> {
    pub filename: &'a str,
    /// Descriptive buffer titles retain their beginning when space is tight;
    /// file paths retain the filename at their end.
    pub title: bool,
    pub dirty: bool,
    pub pending: &'a str,
    pub message: &'a str,
    pub error: bool,
    pub prompt: Option<(&'a str, &'a str, usize)>,
}

/// Paint the document, selections, status, and command/message line.
pub fn paint(
    frame: &mut Frame,
    editor: &Editor,
    viewport: &mut Viewport,
    chrome: Chrome<'_>,
) -> Result<(), vex_core::Error> {
    editor.begin_syntax_frame();
    paint_view(frame, editor, viewport, chrome, 1, None)?;
    paint_command_line(frame, chrome.message, chrome.error, chrome.prompt);
    Ok(())
}

/// Paint one view after the caller has begun the document's syntax frame.
/// Multiple views accumulate their visible ranges in the same request.
pub(crate) fn paint_view(
    frame: &mut Frame,
    editor: &Editor,
    viewport: &mut Viewport,
    chrome: Chrome<'_>,
    reserved_bottom: u16,
    git: Option<&vex_git::Diff>,
) -> Result<(), vex_core::Error> {
    let width = usize::from(frame.width());
    let height = usize::from(frame.height());
    if width == 0 || height == 0 {
        return Ok(());
    }
    let text = editor.document().text();
    let primary = primary_cursor(editor)?;
    let row = text.char_to_line(primary.0);
    let column = editor.display_column(primary)?;
    let body_height = height.saturating_sub(1 + usize::from(reserved_bottom));
    let columns = gutter(width, text.len_lines());
    let gutter = columns.width;
    let body_width = width - gutter;
    viewport.follow(editor, primary, column, (body_width, body_height))?;
    let matching_bracket = editor.matching_bracket(primary);
    let cursors = editor
        .selections()
        .ranges()
        .iter()
        .map(|&s| {
            if editor.mode() == Mode::Insert {
                Ok(s.head)
            } else {
                motion::cursor(text, s)
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let style = |position: CharOffset, syntax: Style| {
        if position == primary {
            let highlight = match syntax {
                Style::Syntax(highlight) => Some(highlight),
                _ => None,
            };
            return if editor.mode() == Mode::Insert {
                Style::InsertCursor(highlight)
            } else {
                Style::PrimaryCursor(highlight)
            };
        }
        if cursors.binary_search(&position).is_ok() {
            return if matching_bracket == Some(position) {
                Style::MatchingBracketCursor
            } else {
                Style::SecondaryCursor
            };
        }
        let ranges = editor.selections().ranges();
        let index = ranges.partition_point(|s| s.start() <= position);
        if index > 0 && position < ranges[index - 1].end() {
            if matching_bracket == Some(position) {
                Style::SelectedMatchingBracket
            } else {
                Style::Selection
            }
        } else if matching_bracket == Some(position) {
            Style::MatchingBracket(match syntax {
                Style::Syntax(highlight) => Some(highlight),
                _ => None,
            })
        } else {
            syntax
        }
    };
    let has_syntax = editor.language().is_some();
    for screen_row in 0..body_height {
        let line = viewport.top_line.saturating_add(screen_row);
        if line >= text.len_lines() {
            frame.put(gutter as u16, screen_row as u16, "~", Style::Gutter);
            continue;
        }
        if gutter > 0 {
            let number = format!("{:>padding$}", line + 1, padding = columns.digits);
            frame.label(
                columns.number_start,
                screen_row as u16,
                &number[number.len().saturating_sub(columns.digits)..],
                if line == row {
                    Style::ActiveLineNumber
                } else {
                    Style::LineNumber
                },
            );
            if let Some(column) = columns.diff
                && let Some(marker) = git.and_then(|diff| diff.marker(line))
            {
                use vex_git::Marker;
                let (glyph, style) = match marker {
                    Marker::Added => ("▍", Style::GitAdded),
                    Marker::Modified => ("▍", Style::GitModified),
                    Marker::Deleted => ("▔", Style::GitDeleted),
                };
                frame.put(column, screen_row as u16, glyph, style);
            }
        }
        let start = CharOffset(text.line_to_char(line));
        let (mut position, mut column) = if viewport.left_column == 0 {
            (start, 0)
        } else {
            editor.position_at_column(start, viewport.left_column)?
        };
        let end = motion::line_end(text, position)?;
        let mut byte = if has_syntax {
            text.char_to_byte(position.0)
        } else {
            0
        };
        let highlights = if has_syntax && position < end {
            let right_column = viewport.left_column.saturating_add(body_width);
            let (right, actual_column) = editor.position_at_column(start, right_column)?;
            // Include a partially clipped grapheme so its placeholder cells keep
            // the same style, without querying the rest of a long logical line.
            let right = if actual_column < right_column && right < end {
                grapheme::next(text, right, 1)?.min(end)
            } else {
                right.min(end)
            };
            Some(editor.syntax_highlights(ByteOffset(byte)..ByteOffset(text.char_to_byte(right.0))))
        } else {
            None
        };
        let highlights = highlights.as_deref().unwrap_or_default();
        let mut highlight_index = 0;
        while position < end && column < viewport.left_column.saturating_add(body_width) {
            let next = grapheme::next(text, position, 1)?;
            let slice = text.slice(position.0..next.0);
            let owned;
            let cluster = match slice.as_str() {
                Some(cluster) => cluster,
                None => {
                    owned = slice.to_string();
                    &owned
                }
            };
            let span = display::width(cluster, column, editor.tab_width());
            while highlight_index < highlights.len()
                && highlights[highlight_index].range.end.0 <= byte
            {
                highlight_index += 1;
            }
            let syntax = highlights
                .get(highlight_index)
                .filter(|highlight| highlight.range.start.0 <= byte)
                .map_or(Style::Text, |highlight| Style::Syntax(highlight.highlight));
            glyph(
                frame,
                gutter,
                screen_row,
                cluster,
                column,
                span,
                viewport.left_column,
                body_width,
                style(position, syntax),
            );
            byte += cluster.len();
            column = column.saturating_add(span);
            position = next;
        }
        if position == end {
            glyph(
                frame,
                gutter,
                screen_row,
                " ",
                column,
                1,
                viewport.left_column,
                body_width,
                style(end, Style::Text),
            );
        }
    }
    if body_height > 0
        && row >= viewport.top_line
        && row - viewport.top_line < body_height
        && column >= viewport.left_column
        && column - viewport.left_column < body_width
    {
        frame.cursor = Some(Cursor {
            x: (gutter + column - viewport.left_column) as u16,
            y: (row - viewport.top_line) as u16,
            shape: if editor.mode() == Mode::Insert {
                CursorShape::Bar
            } else {
                CursorShape::Block
            },
        });
    }
    if height > usize::from(reserved_bottom) {
        paint_status(
            frame,
            body_height as u16,
            editor.mode(),
            chrome,
            (row + 1, column + 1),
            editor.selections().ranges().len(),
        );
    }
    Ok(())
}

/// Status occupies the pane's existing bottom row; its rule also separates
/// horizontal splits. Labels leave the terminal's background untouched.
fn paint_status(
    frame: &mut Frame,
    row: u16,
    mode: Mode,
    chrome: Chrome<'_>,
    position: (usize, usize),
    selections: usize,
) {
    let width = usize::from(frame.width());
    for x in 0..frame.width() {
        frame.put(x, row, "─", Style::StatusBorder);
    }
    let mode = match mode {
        Mode::Normal => " NOR ",
        Mode::Select => " SEL ",
        Mode::Insert => " INS ",
    };
    if width < mode.len() + 2 {
        return;
    }
    frame.label(1, row, mode, Style::StatusLine);
    let mut right = format!(" {}:{} ", position.0, position.1);
    if selections > 1 {
        right = format!(" {}:{}  {selections} sel ", position.0, position.1);
    }
    let mut end = width - 1;
    if right.len() + mode.len() + 3 <= width {
        end -= right.len();
        frame.label(end as u16, row, &right, Style::StatusLine);
        end -= 1;
    }
    let start = mode.len() + 2;
    let pending = chrome.pending.trim();
    let pending_width = pending
        .graphemes(true)
        .map(|g| display::visible(g).width())
        .sum::<usize>()
        + 2;
    // Pending work stays visible in narrow panes even when the filename must
    // give way. A modified file still keeps room for its indicator.
    let minimum_filename = if chrome.dirty { 8 } else { 1 };
    if !pending.is_empty() && end.saturating_sub(start) >= pending_width + minimum_filename {
        end -= pending_width;
        frame.label(end as u16, row, &format!(" {pending} "), Style::StatusLine);
        end -= 1;
    }
    let available = end.saturating_sub(start);
    let suffix = if chrome.dirty { " [+] " } else { " " };
    if available <= suffix.len() + 1 {
        if chrome.dirty {
            // Even the narrowest useful pane must reveal unsaved changes.
            frame.put(mode.len() as u16, row, "+", Style::StatusLine);
        }
        return;
    }
    let error = chrome.prompt.is_some() && chrome.error;
    let name = if error {
        chrome.message
    } else {
        chrome.filename
    };
    let name = status_text(name, available - suffix.len() - 1, !error && !chrome.title);
    frame.label(
        start as u16,
        row,
        &format!(" {name}{suffix}"),
        Style::StatusLine,
    );
}

/// Clip whole visible graphemes, retaining the filename end of a long path.
fn status_text(text: &str, width: usize, keep_end: bool) -> String {
    let visible_width = text
        .graphemes(true)
        .map(|g| display::visible(g).width())
        .sum::<usize>();
    if visible_width <= width {
        return text.graphemes(true).map(display::visible).collect();
    }
    if width == 0 {
        return String::new();
    }
    let mut remaining = width - 1;
    let mut parts = Vec::new();
    let mut take = |grapheme: &str| {
        let visible = display::visible(grapheme);
        let size = visible.width();
        if size > remaining {
            return false;
        }
        remaining -= size;
        parts.push(visible.to_owned());
        true
    };
    if keep_end {
        for g in text.graphemes(true).rev() {
            if !take(g) {
                break;
            }
        }
        parts.reverse();
        format!("…{}", parts.concat())
    } else {
        for g in text.graphemes(true) {
            if !take(g) {
                break;
            }
        }
        format!("{}…", parts.concat())
    }
}

/// Paint the single application-wide command/message line after composing panes.
pub(crate) fn paint_command_line(
    frame: &mut Frame,
    message: &str,
    error: bool,
    prompt: Option<(&str, &str, usize)>,
) {
    let width = usize::from(frame.width());
    let height = usize::from(frame.height());
    if width == 0 || height == 0 {
        return;
    }
    frame.fill_row((height - 1) as u16, Style::Text);
    let bottom = (height - 1) as u16;
    if let Some((prefix, prompt, caret)) = prompt {
        let prefix_width = prefix.len().min(width);
        frame.label(
            0,
            bottom,
            prefix,
            if error { Style::Error } else { Style::Text },
        );
        let tabs = NonZeroUsize::new(4).unwrap();
        let available = width.saturating_sub(prefix_width);
        // Prompt input strips controls (including tabs). Walk back only far
        // enough to fill this row instead of measuring the entire prefix.
        let mut start = caret;
        let mut prompt_column = 0usize;
        for (index, cluster) in prompt[..caret].grapheme_indices(true).rev() {
            let span = display::width(cluster, 0, tabs);
            if prompt_column.saturating_add(span) >= available {
                break;
            }
            start = index;
            prompt_column += span;
        }
        let mut column = 0;
        for cluster in prompt[start..].graphemes(true) {
            let span = display::width(cluster, column, tabs);
            glyph(
                frame,
                prefix_width,
                usize::from(bottom),
                cluster,
                column,
                span,
                0,
                available,
                Style::Text,
            );
            column = column.saturating_add(span);
            if column >= available {
                break;
            }
        }
        frame.cursor = Some(Cursor {
            x: (prefix_width + prompt_column).min(width - 1) as u16,
            y: bottom,
            shape: CursorShape::Bar,
        });
    } else {
        crate::ui::Label::new(message).paint(
            frame,
            0,
            bottom,
            frame.width(),
            if error { Style::Error } else { Style::Message },
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn glyph(
    frame: &mut Frame,
    origin: usize,
    row: usize,
    cluster: &str,
    column: usize,
    span: usize,
    left: usize,
    available: usize,
    style: Style,
) {
    let right = left.saturating_add(available);
    let end = column.saturating_add(span);
    if column >= right || end <= left {
        return;
    }
    if cluster == "\t" || column < left || end > right {
        for visible in column.max(left)..end.min(right) {
            frame.put((origin + visible - left) as u16, row as u16, " ", style);
        }
    } else {
        frame.put((origin + column - left) as u16, row as u16, cluster, style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vex_core::{Document, Selection, SelectionSet};

    #[test]
    fn unsaved_indicator_survives_narrow_panes_and_clears_on_save() {
        for width in 7..40 {
            let mut frame = Frame::default();
            frame.reset(width, 1).unwrap();
            for dirty in [true, false] {
                paint_status(
                    &mut frame,
                    0,
                    Mode::Normal,
                    Chrome {
                        filename: "a/very/long/path/filename.rs",
                        title: false,
                        dirty,
                        pending: "",
                        message: "",
                        error: false,
                        prompt: None,
                    },
                    (1234, 5678),
                    100,
                );
                assert_eq!(frame.row_text(0).contains('+'), dirty, "width {width}");
            }
        }
    }

    fn render(editor: &Editor, width: u16, height: u16, viewport: &mut Viewport) -> Frame {
        let mut frame = Frame::default();
        frame.reset(width, height).unwrap();
        paint(
            &mut frame,
            editor,
            viewport,
            Chrome {
                filename: "test",
                title: false,
                dirty: false,
                pending: "",
                message: "",
                error: false,
                prompt: None,
            },
        )
        .unwrap();
        frame
    }

    #[test]
    fn matching_pairs_follow_the_cursor_and_clear_in_inactive_views() {
        let mut editor = Editor::new(Document::from("(字) x"));
        for mode in ["normal_mode", "select_mode", "insert_mode"] {
            editor.execute(mode, 1).unwrap();
            editor
                .set_selections(SelectionSet::single(Selection::cursor(CharOffset(0))))
                .unwrap();
            let mut frame = render(&editor, 30, 5, &mut Viewport::default());
            assert_eq!(
                frame.style_at(5, 0),
                Some(if mode == "insert_mode" {
                    Style::InsertCursor(None)
                } else {
                    Style::PrimaryCursor(None)
                })
            );
            assert_eq!(frame.style_at(8, 0), Some(Style::MatchingBracket(None)));
            assert_eq!(
                frame.cursor.unwrap().shape,
                if mode == "insert_mode" {
                    CursorShape::Bar
                } else {
                    CursorShape::Block
                }
            );
            frame.inactive();
            assert_eq!(frame.style_at(5, 0), Some(Style::InactiveCursor));
            assert_eq!(frame.style_at(8, 0), Some(Style::Text));
            assert!(frame.cursor.is_none());
        }
        editor.execute("normal_mode", 1).unwrap();
        editor
            .set_selections(SelectionSet::single(Selection::cursor(CharOffset(2))))
            .unwrap();
        let frame = render(&editor, 30, 5, &mut Viewport::default());
        assert_eq!(frame.style_at(5, 0), Some(Style::MatchingBracket(None)));
        assert_eq!(frame.style_at(8, 0), Some(Style::PrimaryCursor(None)));
        editor
            .set_selections(SelectionSet::single(Selection::cursor(CharOffset(4))))
            .unwrap();
        let frame = render(&editor, 30, 5, &mut Viewport::default());
        assert_eq!(frame.style_at(5, 0), Some(Style::Text));
        assert_eq!(frame.style_at(8, 0), Some(Style::Text));
    }

    #[test]
    fn matching_partners_preserve_selection_and_cursor_backgrounds_and_inactive_syntax() {
        let mut editor = Editor::new(Document::from("(x)"));
        editor
            .set_selections(SelectionSet::single(Selection::new(
                CharOffset(0),
                CharOffset(3),
            )))
            .unwrap();
        let frame = render(&editor, 20, 4, &mut Viewport::default());
        assert_eq!(frame.style_at(5, 0), Some(Style::SelectedMatchingBracket));
        assert_eq!(frame.style_at(7, 0), Some(Style::PrimaryCursor(None)));
        editor
            .set_selections(
                SelectionSet::new(
                    vec![
                        Selection::cursor(CharOffset(0)),
                        Selection::cursor(CharOffset(2)),
                    ],
                    1,
                )
                .unwrap(),
            )
            .unwrap();
        let mut frame = render(&editor, 20, 4, &mut Viewport::default());
        assert_eq!(frame.style_at(5, 0), Some(Style::MatchingBracketCursor));
        frame.inactive();
        assert_eq!(frame.style_at(5, 0), Some(Style::InactiveCursor));
        for mode in ["normal_mode", "insert_mode"] {
            let mut editor = Editor::new(Document::from("// (x)"));
            editor.set_language(Some(vex_editor::Language::Rust));
            editor.execute(mode, 1).unwrap();
            editor
                .set_selections(SelectionSet::single(Selection::cursor(CharOffset(3))))
                .unwrap();
            let mut frame = render(&editor, 20, 4, &mut Viewport::default());
            let syntax = Some(vex_editor::Highlight::Comment);
            assert_eq!(
                frame.style_at(8, 0),
                Some(if mode == "insert_mode" {
                    Style::InsertCursor(syntax)
                } else {
                    Style::PrimaryCursor(syntax)
                })
            );
            assert_eq!(frame.style_at(10, 0), Some(Style::MatchingBracket(syntax)));
            frame.inactive();
            assert_eq!(
                frame.style_at(10, 0),
                Some(Style::Syntax(vex_editor::Highlight::Comment))
            );
        }
    }

    #[test]
    fn unicode_tabs_selections_and_cursor_have_matching_cell_positions() {
        let mut editor = Editor::new(Document::from("a\t界e\u{301}\r\nnext"));
        editor
            .set_selections(SelectionSet::single(Selection::new(
                CharOffset(2),
                CharOffset(5),
            )))
            .unwrap();
        let frame = render(&editor, 24, 6, &mut Viewport::default());
        assert!(frame.row_text(0).starts_with(" 1   a   界e\u{301}"));
        assert_eq!(frame.style_at(9, 0), Some(Style::Selection));
        assert_eq!(frame.cursor.unwrap().x, 11);
        assert_eq!(frame.style_at(11, 0), Some(Style::PrimaryCursor(None)));
    }

    #[test]
    fn viewport_follows_cursor_and_never_emits_half_a_wide_glyph() {
        let mut editor = Editor::new(Document::from("abc界defghijkl\n1\n2\n3\n4\n5"));
        editor.execute("goto_line_end", 1).unwrap();
        let mut viewport = Viewport::default();
        let frame = render(&editor, 7, 5, &mut viewport);
        assert!(viewport.left_column > 0);
        assert!(frame.cursor.unwrap().x < 7);
        editor.execute("goto_file_end", 1).unwrap();
        let frame = render(&editor, 7, 5, &mut viewport);
        assert!(viewport.top_line > 0);
        assert!(frame.cursor.unwrap().y < 3);
        for (width, height) in [(0, 0), (1, 1), (2, 2)] {
            render(&editor, width, height, &mut viewport);
        }
    }

    #[test]
    fn terminal_controls_in_text_are_displayed_as_data() {
        let editor = Editor::new(Document::from("a\x1b[2J\0\u{301}z"));
        let frame = render(&editor, 40, 5, &mut Viewport::default());
        assert!(!frame.row_text(0).contains('\x1b'));
        assert!(!frame.row_text(0).contains('\0'));
        assert!(frame.row_text(0).contains("a�[2J��z"));
    }

    #[test]
    fn scrolling_keeps_the_whole_primary_wide_grapheme_visible() {
        let mut editor = Editor::new(Document::from("abcdef界"));
        editor.execute("goto_line_end", 1).unwrap();
        let mut viewport = Viewport::default();
        let frame = render(&editor, 7, 4, &mut viewport);
        assert_eq!(viewport.left_column, 1);
        assert_eq!(frame.row_text(0), "bcdef界");
        assert_eq!(frame.cursor.unwrap().x, 5);
    }

    #[test]
    fn syntax_colors_respect_unicode_cells_and_selection_overlays() {
        use vex_editor::{Highlight, Language};
        let mut editor = Editor::new(Document::from(
            "fn main() {\n\tlet s = \"界e\u{301}\"; // note\n}\n",
        ));
        editor.set_language(Some(Language::Rust));
        editor.execute("goto_file_end", 1).unwrap();
        let frame = render(&editor, 50, 8, &mut Viewport::default());
        assert_eq!(
            frame.style_at(5, 0),
            Some(Style::Syntax(Highlight::Keyword))
        );
        assert_eq!(
            frame.style_at(8, 0),
            Some(Style::Syntax(Highlight::Function))
        );
        assert_eq!(
            frame.style_at(9, 1),
            Some(Style::Syntax(Highlight::Keyword))
        );
        let quote = 17;
        for x in quote..quote + 5 {
            assert_eq!(frame.style_at(x, 1), Some(Style::Syntax(Highlight::String)));
        }
        assert_eq!(
            frame.style_at(25, 1),
            Some(Style::Syntax(Highlight::Comment))
        );
        editor
            .set_selections(SelectionSet::single(Selection::new(
                CharOffset(0),
                CharOffset(2),
            )))
            .unwrap();
        let frame = render(&editor, 50, 8, &mut Viewport::default());
        assert_eq!(frame.style_at(5, 0), Some(Style::Selection));
        assert_eq!(
            frame.style_at(6, 0),
            Some(Style::PrimaryCursor(Some(Highlight::Keyword)))
        );

        editor.execute("insert_mode", 1).unwrap();
        let mut frame = render(&editor, 50, 8, &mut Viewport::default());
        assert_eq!(
            frame.style_at(5, 0),
            Some(Style::InsertCursor(Some(Highlight::Keyword)))
        );
        assert_eq!(frame.cursor.unwrap().shape, CursorShape::Bar);
        frame.inactive();
        assert_eq!(frame.style_at(5, 0), Some(Style::InactiveCursor));
        assert!(frame.cursor.is_none());

        editor.execute("normal_mode", 1).unwrap();
        let frame = render(&editor, 50, 8, &mut Viewport::default());
        assert_eq!(
            frame.style_at(5, 0),
            Some(Style::PrimaryCursor(Some(Highlight::Keyword)))
        );
        assert_eq!(frame.cursor.unwrap().shape, CursorShape::Block);
    }

    #[test]
    fn typing_keeps_visible_syntax_colors_before_the_worker_finishes() {
        use vex_editor::{Highlight, Language, SyntaxWorker};
        let mut editor = Editor::new(Document::from("fn main() {}\n// 界abc\nfn other() {}\n"));
        editor.set_language(Some(Language::Rust));
        editor.set_background_syntax(true);
        editor
            .set_selections(SelectionSet::single(Selection::cursor(CharOffset(18))))
            .unwrap();
        editor.execute("insert_mode", 1).unwrap();
        let mut viewport = Viewport::default();
        render(&editor, 40, 8, &mut viewport);
        let mut worker = SyntaxWorker::default();
        let job = editor.take_syntax_job().unwrap();
        assert!(editor.apply_syntax_result(worker.run(job).unwrap()));
        for text in ["x", "界", "\n", "y"] {
            editor.insert_text(text).unwrap();
            let frame = render(&editor, 40, 8, &mut viewport);
            assert_eq!(
                frame.style_at(5, 0),
                Some(Style::Syntax(Highlight::Keyword))
            );
            assert_eq!(
                frame.style_at(8, 0),
                Some(Style::Syntax(Highlight::Function))
            );
            assert_eq!(
                frame.style_at(5, 1),
                Some(Style::Syntax(Highlight::Comment))
            );
            assert!(editor.take_syntax_job().is_some());
            // Deliberately withhold all worker results while typing.
        }
        editor.execute("undo", 1).unwrap();
        let frame = render(&editor, 40, 8, &mut viewport);
        assert_eq!(
            frame.style_at(5, 0),
            Some(Style::Syntax(Highlight::Keyword))
        );
        assert_eq!(
            frame.style_at(5, 2),
            Some(Style::Syntax(Highlight::Keyword))
        );
        let job = editor.take_syntax_job().unwrap();
        assert!(editor.apply_syntax_result(worker.run(job).unwrap()));
        render(&editor, 40, 8, &mut viewport);
        assert!(editor.take_syntax_job().is_none());
    }

    #[test]
    fn horizontally_clipped_multiline_comments_keep_their_syntax_style() {
        use vex_editor::{Highlight, Language};
        let source = format!(
            "/*{}\n{}*/\nfn main() {{}}",
            "x".repeat(100),
            "y".repeat(100)
        );
        let mut editor = Editor::new(Document::from(source.as_str()));
        editor.set_language(Some(Language::Rust));
        editor
            .set_selections(SelectionSet::single(Selection::cursor(CharOffset(95))))
            .unwrap();
        let mut viewport = Viewport::default();
        let frame = render(&editor, 20, 5, &mut viewport);
        assert!(viewport.left_column > 0);
        for y in 0..2 {
            for x in 5..19 {
                assert_eq!(
                    frame.style_at(x, y),
                    Some(Style::Syntax(Highlight::Comment))
                );
            }
        }
        assert_eq!(
            frame.style_at(19, 0),
            Some(Style::PrimaryCursor(Some(Highlight::Comment)))
        );
    }

    #[test]
    fn a_wide_syntax_grapheme_clipped_at_the_right_edge_colors_its_placeholder() {
        use vex_editor::{Highlight, Language};
        let mut editor = Editor::new(Document::from("//abc界"));
        editor.set_language(Some(Language::Rust));
        let frame = render(&editor, 6, 4, &mut Viewport::default());
        assert_eq!(frame.row_text(0), "//abc ");
        assert_eq!(
            frame.style_at(5, 0),
            Some(Style::Syntax(Highlight::Comment))
        );
    }
}
