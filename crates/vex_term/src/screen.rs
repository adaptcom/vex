//! Reusable cell grids and buffered terminal updates. Wide glyphs have explicit
//! continuation cells so replacing them clears their entire previous footprint.

use crossterm::{
    cursor::{Hide, MoveTo, SetCursorStyle, Show},
    queue,
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal::{BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate},
};
use std::io::{self, Write};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use vex_core::display;
use vex_editor::Highlight;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Style {
    #[default]
    Text,
    Syntax(Highlight),
    Gutter,
    Status,
    StatusLine,
    StatusBorder,
    InactiveStatus,
    Message,
    PopupTitle,
    Error,
    Selection,
    PrimaryCursor(Option<Highlight>),
    SecondaryCursor,
    InactiveCursor,
    PickerMatch,
    PickerSelectedMatch,
}

impl Style {
    fn bold(self) -> bool {
        matches!(
            self,
            Self::StatusLine | Self::InactiveStatus | Self::PopupTitle
        )
    }

    fn reversed(self) -> bool {
        matches!(self, Self::PrimaryCursor(_))
    }

    fn colors(self) -> (Color, Color) {
        use Color::*;
        match self {
            Self::Text => (Reset, Reset),
            Self::Syntax(highlight) => (
                match highlight {
                    Highlight::Keyword | Highlight::Heading => Magenta,
                    Highlight::Type | Highlight::Property => Cyan,
                    Highlight::Function => Blue,
                    Highlight::Constant
                    | Highlight::Attribute
                    | Highlight::Escape
                    | Highlight::Strong => Yellow,
                    Highlight::String => Green,
                    Highlight::Comment => DarkGrey,
                    Highlight::Operator => Red,
                    Highlight::Punctuation | Highlight::Variable => Reset,
                    Highlight::Label | Highlight::Link => DarkCyan,
                    Highlight::Emphasis => Cyan,
                },
                Reset,
            ),
            Self::Gutter => (DarkGrey, Reset),
            Self::Status => (Black, Grey),
            Self::StatusLine => (Reset, Reset),
            Self::StatusBorder | Self::InactiveStatus => (DarkGrey, Reset),
            Self::Message | Self::PopupTitle => (DarkCyan, Reset),
            Self::Error => (Red, Reset),
            Self::Selection => (Black, Grey),
            Self::PrimaryCursor(highlight) => highlight.map_or(Self::Text, Self::Syntax).colors(),
            Self::SecondaryCursor => (Black, DarkCyan),
            Self::InactiveCursor => (DarkGrey, Reset),
            Self::PickerMatch => (Yellow, Reset),
            Self::PickerSelectedMatch => (DarkYellow, Grey),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Cell {
    text: String,
    width: u16,
    style: Style,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            text: " ".into(),
            width: 1,
            style: Style::Text,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Bar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub shape: CursorShape,
}

#[derive(Debug, Default)]
pub struct Frame {
    width: u16,
    height: u16,
    cells: Vec<Cell>,
    pub cursor: Option<Cursor>,
}

impl Frame {
    /// Copy a pane's complete glyphs and translate its optional hardware cursor.
    pub(crate) fn blit(&mut self, x: u16, y: u16, source: &Frame) {
        for row in 0..source.height.min(self.height.saturating_sub(y)) {
            for column in 0..source.width.min(self.width.saturating_sub(x)) {
                let cell = &source.cells
                    [usize::from(row) * usize::from(source.width) + usize::from(column)];
                if cell.width > 0 {
                    self.put(x + column, y + row, &cell.text, cell.style);
                }
            }
        }
        if let Some(cursor) = source.cursor {
            let (cx, cy) = (
                u32::from(x) + u32::from(cursor.x),
                u32::from(y) + u32::from(cursor.y),
            );
            if cx < u32::from(self.width) && cy < u32::from(self.height) {
                self.cursor = Some(Cursor {
                    x: cx as u16,
                    y: cy as u16,
                    shape: cursor.shape,
                });
            }
        }
    }

    pub(crate) fn inactive(&mut self) {
        self.cursor = None;
        for cell in &mut self.cells {
            cell.style = match cell.style {
                Style::Status | Style::StatusLine => Style::InactiveStatus,
                Style::PrimaryCursor(_) | Style::SecondaryCursor => Style::InactiveCursor,
                other => other,
            };
        }
    }

    pub fn width(&self) -> u16 {
        self.width
    }
    pub fn height(&self) -> u16 {
        self.height
    }

    pub fn reset(&mut self, width: u16, height: u16) -> io::Result<()> {
        let size = usize::from(width) * usize::from(height);
        if size > 1_000_000 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "terminal dimensions exceed one million cells",
            ));
        }
        self.width = width;
        self.height = height;
        self.cursor = None;
        self.cells.resize_with(size, Cell::default);
        for cell in &mut self.cells {
            cell.text.clear();
            cell.text.push(' ');
            cell.width = 1;
            cell.style = Style::Text;
        }
        Ok(())
    }

    /// Put a whole printable grapheme; clipped wide glyphs are not emitted.
    /// Controls and standalone zero-width clusters become replacement cells.
    pub fn put(&mut self, x: u16, y: u16, grapheme: &str, style: Style) {
        let visible = display::visible(grapheme);
        let width = visible.width();
        if y >= self.height || usize::from(x) + width > usize::from(self.width) {
            return;
        }
        let index = usize::from(y) * usize::from(self.width) + usize::from(x);
        // This also makes overwriting only part of an existing wide glyph safe.
        let mut start = index;
        while self.cells[start].width == 0 {
            start -= 1;
        }
        let end = index + width;
        let mut clear_end = end;
        for i in start..end {
            clear_end = clear_end.max(i + usize::from(self.cells[i].width));
        }
        for cell in &mut self.cells[start..clear_end] {
            cell.text.clear();
            cell.text.push(' ');
            cell.width = 1;
            cell.style = style;
        }
        let cell = &mut self.cells[index];
        cell.text.clear();
        cell.text.push_str(visible);
        cell.width = width as u16;
        cell.style = style;
        for cell in &mut self.cells[index + 1..end] {
            cell.text.clear();
            cell.width = 0;
            cell.style = style;
        }
    }

    /// Write a clipped UI label. Its contents never become terminal commands.
    pub fn label(&mut self, mut x: u16, y: u16, text: &str, style: Style) {
        for grapheme in text.graphemes(true) {
            let width = display::visible(grapheme).width();
            if usize::from(x) + width > usize::from(self.width) {
                break;
            }
            self.put(x, y, grapheme, style);
            x += width as u16;
        }
    }

    pub fn fill_row(&mut self, y: u16, style: Style) {
        for x in 0..self.width {
            self.put(x, y, " ", style);
        }
    }

    /// A readable row for snapshots and diagnostics; continuation cells are omitted.
    pub fn row_text(&self, row: u16) -> String {
        if row >= self.height {
            return String::new();
        }
        let start = usize::from(row) * usize::from(self.width);
        self.cells[start..start + usize::from(self.width)]
            .iter()
            .map(|cell| cell.text.as_str())
            .collect()
    }

    pub fn style_at(&self, x: u16, y: u16) -> Option<Style> {
        if x >= self.width || y >= self.height {
            return None;
        }
        Some(self.cells[usize::from(y) * usize::from(self.width) + usize::from(x)].style)
    }
}

#[derive(Debug, Default)]
pub struct Renderer {
    front: Frame,
    back: Frame,
    initialized: bool,
    output: Vec<u8>,
}

impl Renderer {
    pub fn frame(&mut self, width: u16, height: u16) -> io::Result<&mut Frame> {
        self.back.reset(width, height)?;
        Ok(&mut self.back)
    }

    pub fn invalidate(&mut self) {
        self.initialized = false;
    }

    /// Emit changed cells and cursor state in one buffered write and flush.
    /// An unchanged frame produces no output. Failed writes invalidate the cache.
    pub fn present(&mut self, writer: &mut impl Write) -> io::Result<usize> {
        let full = !self.initialized
            || self.front.width != self.back.width
            || self.front.height != self.back.height;
        self.output.clear();
        let mut last_position = None;
        let mut last_style = None;
        let cells_changed = full || self.front.cells != self.back.cells;
        let cursor_changed = full || self.front.cursor != self.back.cursor;
        if !cells_changed && !cursor_changed {
            return Ok(0);
        }
        queue!(self.output, BeginSynchronizedUpdate, Hide)?;
        if full {
            queue!(self.output, ResetColor, Clear(ClearType::All))?;
        }
        for y in 0..self.back.height {
            for x in 0..self.back.width {
                let index = usize::from(y) * usize::from(self.back.width) + usize::from(x);
                let cell = &self.back.cells[index];
                if cell.width == 0 || (!full && self.front.cells[index] == *cell) {
                    continue;
                }
                if last_position != Some((x, y)) {
                    queue!(self.output, MoveTo(x, y))?;
                }
                if last_style != Some(cell.style) {
                    let (foreground, background) = cell.style.colors();
                    queue!(
                        self.output,
                        SetForegroundColor(foreground),
                        SetBackgroundColor(background)
                    )?;
                    if last_style.is_some_and(Style::bold) != cell.style.bold() {
                        queue!(
                            self.output,
                            SetAttribute(if cell.style.bold() {
                                Attribute::Bold
                            } else {
                                Attribute::NormalIntensity
                            })
                        )?;
                    }
                    if last_style.is_some_and(Style::reversed) != cell.style.reversed() {
                        queue!(
                            self.output,
                            SetAttribute(if cell.style.reversed() {
                                Attribute::Reverse
                            } else {
                                Attribute::NoReverse
                            })
                        )?;
                    }
                    last_style = Some(cell.style);
                }
                queue!(self.output, Print(&cell.text))?;
                last_position = Some((x + cell.width, y));
            }
        }
        queue!(self.output, ResetColor)?;
        if let Some(cursor) = self
            .back
            .cursor
            .filter(|c| c.x < self.back.width && c.y < self.back.height)
        {
            if full || self.front.cursor.map(|c| c.shape) != Some(cursor.shape) {
                queue!(
                    self.output,
                    match cursor.shape {
                        CursorShape::Block => SetCursorStyle::SteadyBlock,
                        CursorShape::Bar => SetCursorStyle::SteadyBar,
                    }
                )?;
            }
            queue!(self.output, MoveTo(cursor.x, cursor.y))?;
            // The cell grid draws the reversed block. A terminal block on top
            // could invert it again or impose the terminal's fixed cursor color.
            // Insert and prompt carets still use the terminal's bar cursor.
            if cursor.shape == CursorShape::Bar {
                queue!(self.output, Show)?;
            }
        }
        queue!(self.output, EndSynchronizedUpdate)?;
        if let Err(error) = writer.write_all(&self.output).and_then(|()| writer.flush()) {
            self.initialized = false;
            return Err(error);
        }
        std::mem::swap(&mut self.front, &mut self.back);
        self.initialized = true;
        Ok(self.output.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_reverses_existing_colors_without_leaking_reverse_to_other_cells() {
        for highlight in [None, Some(Highlight::Keyword), Some(Highlight::String)] {
            let underlying = highlight.map_or(Style::Text, Style::Syntax);
            let cursor = Style::PrimaryCursor(highlight);
            assert_eq!(cursor.colors(), underlying.colors());
            let mut renderer = Renderer::default();
            let frame = renderer.frame(3, 1).unwrap();
            frame.put(0, 0, "a", cursor);
            frame.put(1, 0, "b", underlying);
            frame.cursor = Some(Cursor {
                x: 0,
                y: 0,
                shape: CursorShape::Block,
            });
            let mut bytes = Vec::new();
            renderer.present(&mut bytes).unwrap();
            let output = String::from_utf8(bytes).unwrap();
            assert!(output.contains("\x1b[7ma"));
            assert!(output.contains("\x1b[27mb"));
            assert!(!output.contains("\x1b[?25h"));
            let frame = renderer.frame(3, 1).unwrap();
            frame.put(0, 0, "a", underlying);
            frame.put(1, 0, "b", underlying);
            frame.cursor = Some(Cursor {
                x: 1,
                y: 0,
                shape: CursorShape::Bar,
            });
            let mut bytes = Vec::new();
            renderer.present(&mut bytes).unwrap();
            let output = String::from_utf8(bytes).unwrap();
            assert!(!output.contains("\x1b[7m"));
            assert!(output.contains("\x1b[?25h"));
        }
        assert_eq!(
            Style::InactiveCursor.colors(),
            (Color::DarkGrey, Color::Reset)
        );
        assert!(!Style::InactiveCursor.reversed());
    }

    #[test]
    fn unchanged_frames_emit_nothing_and_wide_to_narrow_clears_the_trailing_cell() {
        let mut renderer = Renderer::default();
        renderer.frame(4, 1).unwrap().put(0, 0, "界", Style::Text);
        renderer.present(&mut Vec::new()).unwrap();
        renderer.frame(4, 1).unwrap().put(0, 0, "界", Style::Text);
        assert_eq!(renderer.present(&mut Vec::new()).unwrap(), 0);
        renderer.frame(4, 1).unwrap().put(0, 0, "a", Style::Text);
        let mut bytes = Vec::new();
        renderer.present(&mut bytes).unwrap();
        let output = String::from_utf8(bytes).unwrap();
        assert!(output.contains("a "));
        assert!(!output.contains("\x1b[2J"));
    }

    #[test]
    fn partial_overwrite_removes_a_wide_glyph_and_controls_are_visible() {
        let mut frame = Frame::default();
        frame.reset(5, 1).unwrap();
        frame.put(0, 0, "界", Style::Text);
        frame.put(1, 0, "a", Style::Selection);
        assert_eq!(frame.row_text(0), " a   ");
        frame.put(4, 0, "界", Style::Text);
        assert_eq!(frame.row_text(0), " a   ");
        frame.label(0, 0, "\x1b\u{301}x", Style::Text);
        assert!(!frame.row_text(0).contains('\x1b'));
        assert!(frame.row_text(0).starts_with("��x"));
    }

    #[test]
    fn resize_and_write_failure_require_a_full_redraw() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("broken"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut renderer = Renderer::default();
        renderer.frame(4, 2).unwrap();
        assert!(renderer.present(&mut Broken).is_err());
        let mut bytes = Vec::new();
        renderer.present(&mut bytes).unwrap();
        assert!(String::from_utf8(bytes).unwrap().contains("\x1b[2J"));
        renderer.frame(1, 1).unwrap();
        let mut bytes = Vec::new();
        renderer.present(&mut bytes).unwrap();
        assert!(String::from_utf8(bytes).unwrap().contains("\x1b[2J"));
        renderer.frame(0, 0).unwrap();
        renderer.present(&mut Vec::new()).unwrap();
    }

    #[test]
    fn style_only_changes_redraw_and_unchanged_syntax_emits_nothing() {
        let mut renderer = Renderer::default();
        renderer.frame(2, 1).unwrap().put(0, 0, "界", Style::Text);
        renderer.present(&mut Vec::new()).unwrap();
        let keyword = Style::Syntax(Highlight::Keyword);
        renderer.frame(2, 1).unwrap().put(0, 0, "界", keyword);
        let mut output = Vec::new();
        assert!(renderer.present(&mut output).unwrap() > 0);
        assert!(
            output
                .windows("界".len())
                .any(|window| window == "界".as_bytes())
        );
        let frame = renderer.frame(2, 1).unwrap();
        frame.put(0, 0, "界", keyword);
        assert_eq!(frame.style_at(1, 0), Some(keyword));
        assert_eq!(renderer.present(&mut Vec::new()).unwrap(), 0);
    }
}
