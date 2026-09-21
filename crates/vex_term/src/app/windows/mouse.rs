//! Pointer routing uses the same pane rectangles as drawing. Reading an
//! inactive view never changes focus, undo grouping, or its selections.

use super::{App, layout};
use crate::documentation::Area;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

pub(in crate::app) struct State {
    enabled: bool,
    focused: bool,
    drag: Option<layout::Resize>,
    pub(in crate::app) completion: Option<Area>,
    pub(in crate::app) hints: Option<Area>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            enabled: true,
            focused: true,
            drag: None,
            completion: None,
            hints: None,
        }
    }
}

impl App {
    pub(crate) fn mouse_enabled(&self) -> bool {
        self.mouse.enabled
    }

    pub(in crate::app) fn configure_mouse(&mut self, argument: &str) {
        if !argument.is_empty() {
            self.mouse.enabled = argument == "on";
            self.cancel_mouse_drag();
        }
        self.message = format!("mouse {}", if self.mouse.enabled { "on" } else { "off" });
    }

    pub(in crate::app) fn cancel_mouse_drag(&mut self) {
        self.mouse.drag = None;
    }

    pub(in crate::app) fn mouse_focus(&mut self, focused: bool) {
        self.mouse.focused = focused;
        self.cancel_mouse_drag();
    }

    /// `count` combines only adjacent wheel events at identical coordinates.
    pub(crate) fn handle_mouse(&mut self, event: MouseEvent, count: usize) -> bool {
        use MouseEventKind::*;
        if !self.mouse.enabled || !self.mouse.focused {
            return false;
        }
        let (x, y) = (event.column, event.row);
        match event.kind {
            Drag(MouseButton::Left) | Up(MouseButton::Left) if self.mouse.drag.is_some() => {
                let mut drag = self.mouse.drag.take().unwrap();
                let changed = self.windows.layout.resize(&mut drag, x, y);
                if matches!(event.kind, Drag(_)) {
                    self.mouse.drag = Some(drag);
                }
                return changed;
            }
            Up(_) => {
                self.cancel_mouse_drag();
                return false;
            }
            Moved | Drag(_) => return false,
            _ => self.cancel_mouse_drag(),
        }
        // Modal layers own pointer input; it must never reach panes underneath.
        if self.prompt.is_some() || self.picker_query_stamp().is_some() || self.actions.visible() {
            return false;
        }
        if !self.keys.pending_keys().is_empty()
            && self.mouse.hints.is_some_and(|area| area.contains(x, y))
        {
            return false;
        }
        let scroll = match event.kind {
            ScrollDown => Some((true, count.saturating_mul(3))),
            ScrollUp => Some((false, count.saturating_mul(3))),
            _ => None,
        };
        let (panes, _) = self.windows.layout.visible(self.window_area());
        if let Some((_, active)) = panes
            .iter()
            .find(|(id, _)| *id == self.windows.layout.active)
            && active.contains(x, y)
        {
            let (local_x, local_y) = (x - active.x, y - active.y);
            if let Some(redraw) = self.signature_mouse(local_x, local_y, scroll) {
                return redraw;
            }
            if self.completion_visible()
                && self
                    .mouse
                    .completion
                    .is_some_and(|area| area.contains(local_x, local_y))
            {
                return false;
            }
            if let Some(redraw) = self.hover_mouse(local_x, local_y, scroll) {
                return redraw;
            }
        }
        if event.kind == Down(MouseButton::Left) {
            self.mouse.drag = self.windows.layout.begin_resize(self.window_area(), x, y);
            return false;
        }
        let Some((down, lines)) = scroll else {
            return false;
        };
        let Some((id, rect)) = panes.into_iter().find(|(_, rect)| rect.contains(x, y)) else {
            return false;
        };
        if y >= rect.y + rect.height.saturating_sub(1) {
            return false;
        }
        let result = if id == self.windows.layout.active {
            self.viewport.scroll(&self.editor, down, lines, rect.size())
        } else {
            let pane = self.windows.panes.get_mut(&id).unwrap();
            let editor = if pane.document == self.editor.document().id() {
                &mut self.editor
            } else {
                &mut self.windows.buffers.get_mut(&pane.document).unwrap().editor
            };
            editor
                .with_view(pane.view, |editor| {
                    pane.viewport.scroll(editor, down, lines, rect.size())
                })
                .expect("pane view exists")
        };
        match result {
            Ok(changed) => changed,
            Err(error) => {
                self.fail(error);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::tests::{draw, key, press},
        screen::Frame,
    };
    use crossterm::event::{Event, KeyCode, KeyModifiers};
    use vex_core::Document;
    use vex_editor::Mode;

    fn pointer(kind: MouseEventKind, x: u16, y: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn app() -> App {
        App::from_document(Document::from("abcdefgh\n".repeat(200).as_str()), (81, 22))
    }

    #[test]
    fn wheel_preserves_modes_selections_and_undo_and_keyboard_returns_to_cursor() {
        for mode in [Mode::Normal, Mode::Select, Mode::Insert] {
            let mut app = app();
            app.editor
                .execute(
                    match mode {
                        Mode::Normal => "normal_mode",
                        Mode::Select => "select_mode",
                        Mode::Insert => "insert_mode",
                    },
                    1,
                )
                .unwrap();
            let selections = app.editor.selections().clone();
            let revision = app.editor.document().revision();
            assert!(app.handle(pointer(MouseEventKind::ScrollDown, 10, 5)));
            assert_eq!(app.viewport.top_line, 3);
            for _ in 0..3 {
                assert!(draw(&mut app).cursor.is_none());
                assert_eq!(app.viewport.top_line, 3);
                assert_eq!(app.editor.selections(), &selections);
                assert_eq!(app.editor.mode(), mode);
                assert_eq!(app.editor.document().revision(), revision);
            }
            key(&mut app, KeyCode::Up); // Even a movement clamped at line zero follows again.
            assert!(draw(&mut app).cursor.is_some());
            assert_eq!(app.viewport.top_line, 0);
            app.execute("mouse off").unwrap();
            assert!(!app.handle(pointer(MouseEventKind::ScrollDown, 10, 5)));
            assert_eq!(app.viewport.top_line, 0);
            assert!(app.execute("mouse maybe").is_err());
            assert!(!app.mouse_enabled());
            app.execute("mouse on").unwrap();
            assert!(app.mouse_enabled());
        }
        let mut app = app();
        press(&mut app, "ihello");
        app.handle(pointer(MouseEventKind::ScrollDown, 10, 5));
        draw(&mut app);
        press(&mut app, "world");
        key(&mut app, KeyCode::Esc);
        press(&mut app, "u");
        assert_eq!(
            app.editor.document().text().to_string(),
            "abcdefgh\n".repeat(200)
        );
    }

    #[test]
    fn wheel_over_an_inactive_pane_preserves_focus_and_both_view_selections() {
        let mut app = app();
        press(&mut app, "30j");
        draw(&mut app);
        app.execute("vsplit").unwrap();
        press(&mut app, "10j");
        draw(&mut app);
        let active = app.windows.layout.active;
        let view = app.editor.active_view();
        let selections = app.editor.selections().clone();
        let viewport = app.viewport;
        let old = app.windows.panes[&0].viewport.top_line;
        app.handle(pointer(MouseEventKind::ScrollDown, 3, 5));
        assert_eq!(app.windows.panes[&0].viewport.top_line, old + 3);
        draw(&mut app);
        assert_eq!(app.windows.layout.active, active);
        assert_eq!(app.editor.active_view(), view);
        assert_eq!(app.editor.selections(), &selections);
        assert_eq!(app.viewport, viewport);
        assert_eq!(app.windows.panes[&0].viewport.top_line, old + 3);
        // A different document uses the same path without switching buffers.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("other.txt");
        std::fs::write(&path, "other\n".repeat(200)).unwrap();
        app.open_window_from_picker(&path).unwrap();
        let id = app.editor.document().id();
        app.handle(pointer(MouseEventKind::ScrollDown, 3, 5));
        draw(&mut app);
        assert_eq!(app.editor.document().id(), id);
        assert_eq!(app.windows.layout.active, active);
        assert_eq!(app.windows.panes[&0].viewport.top_line, old + 6);
        assert_eq!(app.editor.selections().primary().start().0, 0);
    }

    #[test]
    fn dragging_changes_geometry_without_focus_and_cancels_on_release_focus_resize_and_keys() {
        let mut app = app();
        app.execute("vsplit").unwrap();
        let focus = app.windows.layout.active;
        let selections = app.editor.selections().clone();
        for cancel in [
            pointer(MouseEventKind::Up(MouseButton::Left), 55, 5),
            Event::FocusLost,
            Event::Resize(81, 22),
            Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )),
        ] {
            let divider = app.windows.layout.visible(app.window_area()).1[0].x;
            app.handle(pointer(MouseEventKind::Down(MouseButton::Left), divider, 5));
            app.handle(pointer(MouseEventKind::Drag(MouseButton::Left), 55, 5));
            assert_eq!(
                app.windows.layout.visible(app.window_area()).0[0].1.width,
                55
            );
            app.handle(cancel);
            assert!(app.mouse.drag.is_none());
            app.handle(pointer(MouseEventKind::Drag(MouseButton::Left), 15, 5));
            assert_eq!(
                app.windows.layout.visible(app.window_area()).0[0].1.width,
                55
            );
            assert_eq!(app.windows.layout.active, focus);
            assert_eq!(app.editor.selections(), &selections);
            app.handle(Event::FocusGained);
        }
        app.execute("hsplit").unwrap();
        let panes = app.windows.layout.visible(app.window_area()).0;
        let upper = panes[1].1;
        app.handle(pointer(
            MouseEventKind::Down(MouseButton::Left),
            upper.x + 3,
            upper.height - 1,
        ));
        app.handle(pointer(
            MouseEventKind::Drag(MouseButton::Left),
            upper.x + 3,
            13,
        ));
        app.handle(pointer(
            MouseEventKind::Up(MouseButton::Left),
            upper.x + 3,
            13,
        ));
        assert_eq!(
            app.windows.layout.visible(app.window_area()).0[1].1.height,
            14
        );
        assert!(!app.is_dirty());
        draw(&mut app);
    }

    #[test]
    fn overlays_and_command_line_block_pointer_input_to_underlying_panes() {
        let mut app = app();
        app.execute("vsplit").unwrap();
        press(&mut app, "Z");
        draw(&mut app);
        let area = app.mouse.hints.unwrap();
        let before = app.viewport;
        app.handle(pointer(
            MouseEventKind::ScrollDown,
            area.left + 1,
            area.top + 1,
        ));
        assert_eq!(app.viewport, before);
        app.handle(pointer(
            MouseEventKind::Down(MouseButton::Left),
            40,
            area.top + 1,
        ));
        assert!(app.mouse.drag.is_none());
        key(&mut app, KeyCode::Esc);
        press(&mut app, ":");
        app.handle(pointer(MouseEventKind::ScrollDown, 45, 2));
        app.handle(pointer(MouseEventKind::Down(MouseButton::Left), 40, 2));
        assert!(app.mouse.drag.is_none());
        assert_eq!(app.viewport, before);
        key(&mut app, KeyCode::Esc);
        app.handle(pointer(MouseEventKind::ScrollDown, 45, 21));
        assert_eq!(app.viewport, before);
    }

    #[test]
    fn wheel_clamps_large_counts_and_tiny_or_empty_views() {
        for source in ["", "one", "one\ntwo\n", "line\n".repeat(200).as_str()] {
            let mut app = App::from_document(Document::from(source), (80, 12));
            for (width, height) in [(80, 12), (1, 1), (1, 3), (0, 0)] {
                app.handle(Event::Resize(width, height));
                for kind in [MouseEventKind::ScrollDown, MouseEventKind::ScrollUp] {
                    app.handle_mouse(
                        MouseEvent {
                            kind,
                            column: 0,
                            row: 0,
                            modifiers: KeyModifiers::NONE,
                        },
                        usize::MAX,
                    );
                    let mut frame = Frame::default();
                    frame.reset(width, height).unwrap();
                    app.paint(&mut frame).unwrap();
                    assert!(app.viewport.top_line < app.editor.document().text().len_lines());
                }
            }
            assert_eq!(app.editor.document().text().to_string(), source);
        }
    }

    #[test]
    #[ignore = "manual release-mode mouse scroll, drag, and paint benchmark"]
    fn benchmark_mouse_scroll_and_split_drag() {
        use std::{hint::black_box, time::Instant};
        let mut app =
            App::from_document(Document::from("line\n".repeat(200_000).as_str()), (101, 40));
        app.execute("vsplit").unwrap();
        let focus = app.windows.layout.active;
        let before = app.editor.selections().clone();
        let mut frame = Frame::default();
        frame.reset(101, 40).unwrap();
        app.paint(&mut frame).unwrap();
        let mut scroll = Vec::with_capacity(1000);
        let mut drag = Vec::with_capacity(1000);
        let mut paint = Vec::with_capacity(1000);
        for i in 0..1000 {
            let start = Instant::now();
            app.handle(pointer(
                if i % 2 == 0 {
                    MouseEventKind::ScrollDown
                } else {
                    MouseEventKind::ScrollUp
                },
                3,
                5,
            ));
            scroll.push(start.elapsed());
            let divider = app.windows.layout.visible(app.window_area()).1[0].x;
            app.handle(pointer(MouseEventKind::Down(MouseButton::Left), divider, 5));
            let start = Instant::now();
            let changed = app.handle(pointer(
                MouseEventKind::Drag(MouseButton::Left),
                if i % 2 == 0 { 60 } else { 50 },
                5,
            ));
            drag.push(start.elapsed());
            assert!(changed);
            assert_eq!(
                app.windows.layout.visible(app.window_area()).0[0].1.width,
                if i % 2 == 0 { 60 } else { 50 }
            );
            let start = Instant::now();
            frame.reset(101, 40).unwrap();
            app.paint(&mut frame).unwrap();
            black_box(&frame);
            paint.push(start.elapsed());
        }
        assert_eq!(app.windows.layout.active, focus);
        assert_eq!(app.editor.selections(), &before);
        scroll.sort_unstable();
        drag.sort_unstable();
        paint.sort_unstable();
        eprintln!(
            "1 MB / 200,001 lines / two panes / 1,000 samples: wheel median {:?}, p95 {:?}; drag median {:?}, p95 {:?}; repaint median {:?}, p95 {:?}",
            scroll[500], scroll[950], drag[500], drag[950], paint[500], paint[950]
        );
    }
}
