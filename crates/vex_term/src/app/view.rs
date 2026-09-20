//! Pane-sized view commands reuse cached display columns. Scrolling only changes
//! selections when the primary cursor reaches the edge of the visible area.

use super::App;
use crate::render;
use vex_core::{CharOffset, Selection, SelectionSet, motion};
use vex_editor::{Mode, ViewAction};

impl App {
    pub(super) fn view_action(
        &mut self,
        action: ViewAction,
        count: usize,
    ) -> Result<(), vex_editor::Error> {
        let (width, height) = self.active_size();
        let width = usize::from(width);
        let height = usize::from(height).saturating_sub(1); // Pane status line.
        if height == 0 || width == 0 {
            return Ok(());
        }
        self.viewport.ensure_visible(&self.editor, width, height)?;
        let cursor = render::primary_cursor(&self.editor)?;
        let text = self.editor.document().text();
        let row = text.char_to_line(cursor.0);
        let last = text.len_lines() - 1;
        let margin = render::scroll_margin(height);
        use ViewAction::*;
        match action {
            AlignTop => self.viewport.top_line = row,
            AlignCenter => self.viewport.top_line = row.saturating_sub((height - 1) / 2),
            AlignBottom => self.viewport.top_line = row.saturating_sub(height - 1),
            AlignMiddle => {
                let body_width = width - render::gutter(width, text.len_lines()).width;
                self.viewport.left_column = self
                    .editor
                    .display_column(cursor)?
                    .saturating_sub(body_width / 2);
            }
            ScrollUp | ScrollDown | PageUp | PageDown => {
                let down = matches!(action, ScrollDown | PageDown);
                let distance = if matches!(action, PageUp | PageDown) {
                    height
                } else {
                    count
                };
                self.viewport.top_line = if down {
                    self.viewport.top_line.saturating_add(distance).min(last)
                } else {
                    self.viewport.top_line.saturating_sub(distance)
                };
                let target = if down {
                    self.viewport.top_line.saturating_add(margin)
                } else {
                    self.viewport.top_line.saturating_add(height - margin - 1)
                }
                .min(last);
                let destination = CharOffset(text.line_to_char(target));
                if (down && destination > cursor) || (!down && destination < cursor) {
                    let primary = self.editor.selections().primary_index();
                    let mut ranges = self.editor.selections().ranges().to_vec();
                    ranges[primary] = if self.editor.mode() == Mode::Insert {
                        Selection::cursor(destination)
                    } else {
                        motion::put_cursor(
                            text,
                            ranges[primary],
                            destination,
                            self.editor.mode() == Mode::Select,
                        )?
                    };
                    self.editor
                        .set_selections(SelectionSet::new(ranges, primary)?)?;
                }
            }
            WindowTop | WindowCenter | WindowBottom => {
                let last_visible = last.saturating_sub(self.viewport.top_line).min(height - 1);
                let margin = margin.min(last_visible / 2);
                let offset = match action {
                    WindowTop => margin.saturating_add(count.saturating_sub(1)),
                    WindowBottom => {
                        last_visible.saturating_sub(margin.saturating_add(count.saturating_sub(1)))
                    }
                    _ => last_visible / 2,
                }
                .clamp(margin, last_visible.saturating_sub(margin));
                let line = self.viewport.top_line.saturating_add(offset);
                self.editor.execute("goto_line", line.saturating_add(1))?;
            }
        }
        self.viewport.hold(&self.editor, height);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::Frame;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use vex_core::Document;

    fn key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
        app.handle(Event::Key(KeyEvent::new(code, modifiers)));
        assert!(!app.error, "{}", app.message);
    }

    fn press(app: &mut App, keys: &str) {
        for ch in keys.chars() {
            key(app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
    }

    fn draw(app: &mut App) -> Frame {
        let mut frame = Frame::default();
        frame.reset(app.size.0, app.size.1).unwrap();
        app.paint(&mut frame).unwrap();
        frame
    }

    fn lines() -> App {
        App::from_document(Document::from("abcdefgh\n".repeat(100).as_str()), (80, 12))
    }

    #[test]
    fn alignment_survives_redraws_and_following_resumes_on_motion_edits_and_resize() {
        let mut app = lines();
        // No intervening paint: queued movements must still align correctly.
        press(&mut app, "20j3l");
        let selections = app.editor.selections().clone();
        for (keys, expected_row) in [("zt", 0), ("zb", 9), ("zz", 4), ("zc", 4)] {
            press(&mut app, keys);
            for _ in 0..3 {
                let frame = draw(&mut app);
                assert_eq!(frame.cursor.unwrap().y, expected_row, "{keys}");
                assert_eq!(app.editor.selections(), &selections);
            }
        }
        press(&mut app, "ztj");
        assert_eq!(draw(&mut app).cursor.unwrap().y, 3);
        press(&mut app, "zt");
        app.handle(Event::Resize(80, 22));
        assert_eq!(draw(&mut app).cursor.unwrap().y, 3);
        press(&mut app, "zbiX");
        assert_eq!(draw(&mut app).cursor.unwrap().y, 16);
        assert!(app.is_dirty());
    }

    #[test]
    fn sticky_scroll_keeps_other_selections_and_clamps_only_the_primary() {
        for select in [false, true] {
            let mut app = lines();
            let secondary = Selection::new(CharOffset(3), CharOffset(4));
            app.editor
                .set_selections(
                    SelectionSet::new(
                        vec![secondary, Selection::new(CharOffset(183), CharOffset(184))],
                        1,
                    )
                    .unwrap(),
                )
                .unwrap();
            if select {
                press(&mut app, "v");
            }
            press(&mut app, "zzZ");
            let before = app.editor.selections().clone();
            press(&mut app, "j");
            assert_eq!(app.editor.selections(), &before);
            assert_eq!(app.viewport.top_line, 17);
            press(&mut app, "3j");
            assert_eq!(app.viewport.top_line, 20);
            assert_eq!(
                render::primary_cursor(&app.editor).unwrap(),
                CharOffset(23 * 9)
            );
            assert_eq!(app.editor.selections().ranges()[0], secondary);
            if select {
                assert_eq!(
                    app.editor.selections().primary().anchor,
                    before.primary().anchor
                );
            }
            draw(&mut app);
            assert_eq!(app.viewport.top_line, 20);
            press(&mut app, "k");
            assert_eq!(app.viewport.top_line, 19);
            assert_eq!(app.keys.hints().unwrap().title, "View (sticky)");
            key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
            assert!(app.keys.hints().is_none());
            let row = draw(&mut app).cursor.unwrap().y;
            press(&mut app, "j");
            assert_eq!(draw(&mut app).cursor.unwrap().y, row + 1);
            assert!(!app.is_dirty());
        }
    }

    #[test]
    fn paging_aliases_use_the_pane_text_height_and_work_inside_both_prefixes() {
        for prefix in ["", "z", "Z"] {
            for (down, up, modifiers) in [
                (KeyCode::PageDown, KeyCode::PageUp, KeyModifiers::NONE),
                (
                    KeyCode::Char('f'),
                    KeyCode::Char('b'),
                    KeyModifiers::CONTROL,
                ),
            ] {
                let mut app = lines();
                press(&mut app, "20jzz");
                let top = app.viewport.top_line;
                press(&mut app, &format!("3{prefix}"));
                key(&mut app, down, modifiers);
                assert_eq!(app.viewport.top_line, top + 10);
                draw(&mut app);
                assert_eq!(
                    app.editor
                        .document()
                        .text()
                        .char_to_line(render::primary_cursor(&app.editor).unwrap().0)
                        - app.viewport.top_line,
                    3
                );
                if prefix == "z" {
                    press(&mut app, "z");
                }
                key(&mut app, up, modifiers);
                assert_eq!(app.viewport.top_line, top);
                draw(&mut app);
                assert_eq!(
                    app.editor
                        .document()
                        .text()
                        .char_to_line(render::primary_cursor(&app.editor).unwrap().0)
                        - app.viewport.top_line,
                    6
                );
            }
        }
        let mut app = lines();
        press(&mut app, "20jZ");
        key(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(
            render::primary_cursor(&app.editor).unwrap(),
            CharOffset(25 * 9)
        );
        key(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(
            render::primary_cursor(&app.editor).unwrap(),
            CharOffset(20 * 9)
        );
        assert!(app.keys.hints().is_some());
    }

    #[test]
    fn horizontal_centering_uses_display_width_and_alignment_is_pane_local() {
        let source = format!("{}\t界e\u{301}{}\n", "x".repeat(100), "x".repeat(100)).repeat(100);
        let mut app = App::from_document(Document::from(source.as_str()), (80, 22));
        press(&mut app, "20j103lzmzt");
        let column = app
            .editor
            .display_column(render::primary_cursor(&app.editor).unwrap())
            .unwrap();
        let gutter = render::gutter(80, app.editor.document().text().len_lines()).width;
        assert_eq!(app.viewport.left_column, column - (80 - gutter) / 2);
        assert_eq!(
            draw(&mut app).cursor.unwrap().x as usize,
            gutter + (80 - gutter) / 2
        );
        let original = app.viewport;
        app.execute("hsplit").unwrap();
        press(&mut app, "zb");
        let active_height = app.active_size().1 as usize - 1;
        assert_eq!(app.viewport.top_line, 20 - (active_height - 1));
        app.execute("jump_view_up").unwrap();
        assert_eq!(app.viewport, original);
        // Resizing invalidates the old alignment stamp, retaining a visible cursor.
        draw(&mut app);
        assert_ne!(app.viewport.top_line, original.top_line);
    }

    #[test]
    fn window_jumps_respect_margins_counts_and_select_anchors() {
        let mut app = lines();
        press(&mut app, "20jzz");
        assert_eq!(app.viewport.top_line, 16);
        for (keys, line) in [("gt", 19), ("gb", 22), ("gc", 20), ("2gt", 20), ("2gb", 21)] {
            press(&mut app, keys);
            assert_eq!(
                render::primary_cursor(&app.editor).unwrap(),
                CharOffset(line * 9)
            );
            draw(&mut app);
            assert_eq!(app.viewport.top_line, 16);
        }
        press(&mut app, "gtv");
        let anchor = app.editor.selections().primary().anchor;
        press(&mut app, "gb");
        assert_eq!(app.editor.selections().primary().anchor, anchor);
        assert_eq!(app.editor.mode(), Mode::Select);
    }

    #[test]
    fn huge_counts_empty_files_and_tiny_views_stay_bounded() {
        for text in ["", "界", "a\r\nb\r\n", "a\nb\nc"] {
            let mut app = App::from_document(Document::from(text), (80, 12));
            for size in [(80, 12), (1, 1), (1, 3), (0, 0), (12, 5)] {
                app.handle(Event::Resize(size.0, size.1));
                for command in [
                    "align_view_top",
                    "align_view_bottom",
                    "align_view_center",
                    "align_view_middle",
                    "scroll_down",
                    "scroll_up",
                    "page_down",
                    "page_up",
                    "goto_window_top",
                    "goto_window_center",
                    "goto_window_bottom",
                ] {
                    let action = vex_editor::commands::find(command).unwrap();
                    let mut ctx = vex_editor::CommandContext::new(&mut app.editor);
                    ctx.count = std::num::NonZeroUsize::new(usize::MAX).unwrap();
                    (action.run)(&mut ctx).unwrap();
                    app.apply_application_action();
                    assert!(!app.error, "{}", app.message);
                    draw(&mut app);
                    assert!(app.viewport.top_line < app.editor.document().text().len_lines());
                }
            }
            assert_eq!(app.editor.document().text().to_string(), text);
        }
    }

    #[test]
    #[ignore = "manual release-mode view dispatch and paint benchmark"]
    fn benchmark_sticky_view_dispatch_and_paint() {
        use std::{hint::black_box, time::Instant};
        let mut app =
            App::from_document(Document::from("line\n".repeat(200_000).as_str()), (100, 40));
        press(&mut app, "100000GzzZ");
        let before = app.editor.selections().clone();
        let mut frame = Frame::default();
        frame.reset(100, 40).unwrap();
        app.paint(&mut frame).unwrap();
        let mut dispatch = Vec::with_capacity(1000);
        let mut paint = Vec::with_capacity(1000);
        for _ in 0..1000 {
            let start = Instant::now();
            press(&mut app, "jk");
            dispatch.push(start.elapsed() / 2);
            let start = Instant::now();
            frame.reset(100, 40).unwrap();
            app.paint(&mut frame).unwrap();
            black_box(&frame);
            paint.push(start.elapsed());
        }
        assert_eq!(app.editor.selections(), &before);
        dispatch.sort_unstable();
        paint.sort_unstable();
        eprintln!(
            "1 MB / 200,001 lines; 1,000 samples; sticky view dispatch median {:?}, p95 {:?}; paint with hints median {:?}, p95 {:?}",
            dispatch[500], dispatch[950], paint[500], paint[950]
        );
    }
}
