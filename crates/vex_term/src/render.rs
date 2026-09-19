//! Paint only visible logical lines into our grid. No document-sized String is
//! created; the editor's layout cache skips the horizontally hidden line prefix.

use crate::screen::{Cursor, CursorShape, Frame, Style};
use std::num::NonZeroUsize;
use unicode_segmentation::UnicodeSegmentation;
use vex_core::{ByteOffset, CharOffset, display, grapheme, motion};
use vex_editor::{Editor, Mode};

#[derive(Debug, Default)]
pub struct Viewport {
    pub top_line: usize,
    pub left_column: usize,
}

pub struct Chrome<'a> {
    pub filename: &'a str,
    pub dirty: bool,
    pub pending: &'a str,
    pub message: &'a str,
    pub error: bool,
    pub prompt: Option<(&'a str, usize)>,
}

/// Paint the document, selections, status, and command/message line.
pub fn paint(
    frame: &mut Frame,
    editor: &Editor,
    viewport: &mut Viewport,
    chrome: Chrome<'_>,
) -> Result<(), vex_core::Error> {
    let width = usize::from(frame.width());
    let height = usize::from(frame.height());
    if width == 0 || height == 0 {
        return Ok(());
    }
    let text = editor.document().text();
    let primary = if editor.mode() == Mode::Insert {
        editor.selections().primary().head
    } else {
        motion::cursor(text, editor.selections().primary())?
    };
    let row = text.char_to_line(primary.0);
    let column = editor.display_column(primary)?;
    let body_height = height.saturating_sub(2);
    let gutter = if width >= 8 {
        (text.len_lines().ilog10() as usize + 2).min(width / 3)
    } else {
        0
    };
    let body_width = width - gutter;
    if body_height > 0 {
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
            display::width(cluster, column, editor.tab_width()).min(body_width)
        } else {
            1
        };
        let margin = 3.min((body_height - 1) / 2);
        if row < viewport.top_line.saturating_add(margin) {
            viewport.top_line = row.saturating_sub(margin);
        } else if row >= viewport.top_line.saturating_add(body_height - margin) {
            viewport.top_line = row.saturating_add(margin + 1).saturating_sub(body_height);
        }
        if column < viewport.left_column {
            viewport.left_column = column;
        } else if column.saturating_add(cursor_span)
            > viewport.left_column.saturating_add(body_width)
        {
            viewport.left_column = column
                .saturating_add(cursor_span)
                .saturating_sub(body_width);
        }
    }
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
            return Style::PrimaryCursor;
        }
        if cursors.binary_search(&position).is_ok() {
            return Style::SecondaryCursor;
        }
        let ranges = editor.selections().ranges();
        let index = ranges.partition_point(|s| s.start() <= position);
        if index > 0 && position < ranges[index - 1].end() {
            Style::Selection
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
            let number = format!("{:>padding$} ", line + 1, padding = gutter - 1);
            frame.label(
                0,
                screen_row as u16,
                &number[number.len().saturating_sub(gutter)..],
                if line == row {
                    Style::Message
                } else {
                    Style::Gutter
                },
            );
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
    if height >= 2 {
        let status_row = (height - 2) as u16;
        frame.fill_row(status_row, Style::Status);
        let mode = match editor.mode() {
            Mode::Normal => "NOR",
            Mode::Select => "SEL",
            Mode::Insert => "INS",
        };
        let left = format!(
            " {mode}{} {} {}",
            if chrome.dirty { " [+]" } else { "" },
            chrome.pending,
            chrome.filename
        );
        frame.label(0, status_row, &left, Style::Status);
        let right = format!(
            " {}:{}  {} sel ",
            row + 1,
            column + 1,
            editor.selections().ranges().len()
        );
        if right.len() < width {
            frame.label(
                (width - right.len()) as u16,
                status_row,
                &right,
                Style::Status,
            );
        }
    }
    let bottom = (height - 1) as u16;
    if let Some((prompt, caret)) = chrome.prompt {
        frame.put(0, bottom, ":", Style::Text);
        let tabs = NonZeroUsize::new(4).unwrap();
        let prompt_column = prompt[..caret].graphemes(true).fold(0usize, |col, g| {
            col.saturating_add(display::width(g, col, tabs))
        });
        let available = width.saturating_sub(1);
        let left = prompt_column.saturating_add(1).saturating_sub(available);
        let mut column = 0;
        for cluster in prompt.graphemes(true) {
            let span = display::width(cluster, column, tabs);
            glyph(
                frame,
                1,
                usize::from(bottom),
                cluster,
                column,
                span,
                left,
                available,
                Style::Text,
            );
            column = column.saturating_add(span);
            if column >= left.saturating_add(available) {
                break;
            }
        }
        frame.cursor = Some(Cursor {
            x: (1 + prompt_column.saturating_sub(left)).min(width - 1) as u16,
            y: bottom,
            shape: CursorShape::Bar,
        });
    } else {
        frame.label(
            0,
            bottom,
            chrome.message,
            if chrome.error {
                Style::Error
            } else {
                Style::Message
            },
        );
    }
    Ok(())
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

    fn render(editor: &Editor, width: u16, height: u16, viewport: &mut Viewport) -> Frame {
        let mut frame = Frame::default();
        frame.reset(width, height).unwrap();
        paint(
            &mut frame,
            editor,
            viewport,
            Chrome {
                filename: "test",
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
    fn unicode_tabs_selections_and_cursor_have_matching_cell_positions() {
        let mut editor = Editor::new(Document::from("a\t界e\u{301}\r\nnext"));
        editor
            .set_selections(SelectionSet::single(Selection::new(
                CharOffset(2),
                CharOffset(5),
            )))
            .unwrap();
        let frame = render(&editor, 24, 6, &mut Viewport::default());
        assert!(frame.row_text(0).starts_with("1 a   界e\u{301}"));
        assert_eq!(frame.style_at(6, 0), Some(Style::Selection));
        assert_eq!(frame.cursor.unwrap().x, 8);
        assert_eq!(frame.style_at(8, 0), Some(Style::PrimaryCursor));
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
            frame.style_at(2, 0),
            Some(Style::Syntax(Highlight::Keyword))
        );
        assert_eq!(
            frame.style_at(5, 0),
            Some(Style::Syntax(Highlight::Function))
        );
        assert_eq!(
            frame.style_at(6, 1),
            Some(Style::Syntax(Highlight::Keyword))
        );
        let quote = 14;
        for x in quote..quote + 5 {
            assert_eq!(frame.style_at(x, 1), Some(Style::Syntax(Highlight::String)));
        }
        assert_eq!(
            frame.style_at(22, 1),
            Some(Style::Syntax(Highlight::Comment))
        );
        editor
            .set_selections(SelectionSet::single(Selection::new(
                CharOffset(0),
                CharOffset(2),
            )))
            .unwrap();
        let frame = render(&editor, 50, 8, &mut Viewport::default());
        assert_eq!(frame.style_at(2, 0), Some(Style::Selection));
        assert_eq!(frame.style_at(3, 0), Some(Style::PrimaryCursor));
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
            for x in 2..19 {
                assert_eq!(
                    frame.style_at(x, y),
                    Some(Style::Syntax(Highlight::Comment))
                );
            }
        }
        assert_eq!(frame.style_at(19, 0), Some(Style::PrimaryCursor));
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
