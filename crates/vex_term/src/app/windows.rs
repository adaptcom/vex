//! Window ownership and buffer sharing. Only visible buffers are retained;
//! closing their last view must pass the same save protection as quitting.

mod layout;

use super::App;
use crate::{
    files::FileState,
    render::{self, Chrome, Viewport},
    screen::{Frame, Style},
};
use layout::{Axis, Direction, Layout, WindowId};
use std::{
    collections::{BTreeMap, HashMap},
    io,
    path::{Path, PathBuf},
};
use vex_core::DocumentId;
use vex_editor::{Editor, Language, ViewId, WindowAction};

enum Content {
    Document,
    Git(PathBuf),
}

struct Pane {
    // The document stays owned by the pane while an auxiliary view is shown.
    content: Content,
    document: DocumentId,
    view: ViewId,
    viewport: Viewport,
}

struct Buffer {
    editor: Editor,
    files: FileState,
    automatic_language: bool,
}

enum Prepared {
    Existing(DocumentId),
    New(Box<Buffer>),
}

pub(super) struct State {
    layout: Layout,
    panes: BTreeMap<WindowId, Pane>,
    buffers: HashMap<DocumentId, Buffer>,
    frame: Frame,
    syntax_documents: Vec<DocumentId>,
}

impl State {
    pub fn new(editor: &Editor) -> Self {
        Self {
            layout: Layout::default(),
            panes: BTreeMap::from([(
                0,
                Pane {
                    content: Content::Document,
                    document: editor.document().id(),
                    view: editor.active_view(),
                    viewport: Viewport::default(),
                },
            )]),
            buffers: HashMap::new(),
            frame: Frame::default(),
            syntax_documents: Vec::new(),
        }
    }
}

impl App {
    pub(super) fn active_git_view(&self) -> Option<&PathBuf> {
        match &self.windows.panes[&self.windows.layout.active].content {
            Content::Git(key) => Some(key),
            Content::Document => None,
        }
    }
    pub(super) fn set_git_view(&mut self, key: Option<PathBuf>) {
        self.windows
            .panes
            .get_mut(&self.windows.layout.active)
            .unwrap()
            .content = key.map_or(Content::Document, Content::Git);
    }
    pub(super) fn git_view_keys(&self) -> Vec<PathBuf> {
        let mut keys: Vec<_> = self
            .windows
            .panes
            .values()
            .filter_map(|pane| match &pane.content {
                Content::Git(key) => Some(key.clone()),
                Content::Document => None,
            })
            .collect();
        keys.sort();
        keys.dedup();
        keys
    }
    pub(super) fn remap_git_view(&mut self, from: &Path, to: &Path) {
        for pane in self.windows.panes.values_mut() {
            if matches!(&pane.content,Content::Git(key) if key==from) {
                pane.content = Content::Git(to.into());
            }
        }
    }
    pub(super) fn unsaved_paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<_> = std::iter::once((&self.editor, &self.files))
            .chain(
                self.windows
                    .buffers
                    .values()
                    .map(|buffer| (&buffer.editor, &buffer.files)),
            )
            .filter(|(editor, files)| files.is_dirty(editor.document()))
            .filter_map(|(_, files)| files.target().map(Path::to_path_buf))
            .collect();
        paths.sort();
        paths
    }

    pub(super) fn git_documents(&self) -> Vec<vex_git::Document> {
        std::iter::once((&self.editor, &self.files))
            .chain(
                self.windows
                    .buffers
                    .values()
                    .map(|buffer| (&buffer.editor, &buffer.files)),
            )
            .filter_map(|(editor, files)| {
                files.target().map(|path| vex_git::Document {
                    path: path.into(),
                    snapshot: editor.document().snapshot(),
                })
            })
            .collect()
    }

    pub(crate) fn take_syntax_batch(&mut self) -> Option<crate::events::SyntaxBatch> {
        let mut editors: Vec<_> = std::iter::once(&mut self.editor)
            .chain(
                self.windows
                    .buffers
                    .values_mut()
                    .map(|buffer| &mut buffer.editor),
            )
            .collect();
        let mut changed = false;
        for editor in &mut editors {
            changed |= editor.take_syntax_job().is_some();
        }
        let documents: Vec<_> = editors
            .iter()
            .map(|editor| editor.document().id())
            .collect();
        changed |= documents.len() != self.windows.syntax_documents.len()
            || documents
                .iter()
                .any(|id| !self.windows.syntax_documents.contains(id));
        if !changed {
            return None;
        }
        // Replacing a batch also replaces any work still pending for its other
        // buffers. Reissue those requests so none are stranded by cancellation.
        self.windows.syntax_documents.clone_from(&documents);
        let jobs = editors
            .iter_mut()
            .filter_map(|editor| {
                editor.cancel_syntax_request();
                editor.take_syntax_job()
            })
            .collect();
        Some(crate::events::SyntaxBatch {
            documents,
            jobs,
            cancellation: Default::default(),
        })
    }

    pub(crate) fn handle_syntax_results(&mut self, results: Vec<vex_editor::SyntaxResult>) -> bool {
        let mut changed = false;
        for result in results {
            if result.document_id() == self.editor.document().id() {
                changed |= self.editor.apply_syntax_result(result);
            } else if let Some(buffer) = self.windows.buffers.get_mut(&result.document_id()) {
                changed |= buffer.editor.apply_syntax_result(result);
            }
        }
        changed
    }

    fn window_area(&self) -> (u16, u16) {
        (self.size.0, self.size.1.saturating_sub(1))
    }

    pub(super) fn active_size(&self) -> (u16, u16) {
        self.windows
            .layout
            .visible(self.window_area())
            .0
            .into_iter()
            .find(|(id, _)| *id == self.windows.layout.active)
            .unwrap()
            .1
            .size()
    }

    fn remember_viewport(&mut self) {
        self.windows
            .panes
            .get_mut(&self.windows.layout.active)
            .unwrap()
            .viewport = self.viewport;
    }

    fn switch_buffer(&mut self, buffer: Buffer) -> Buffer {
        Buffer {
            editor: std::mem::replace(&mut self.editor, buffer.editor),
            files: std::mem::replace(&mut self.files, buffer.files),
            automatic_language: std::mem::replace(
                &mut self.automatic_language,
                buffer.automatic_language,
            ),
        }
    }

    fn focus_window(&mut self, id: WindowId) {
        self.remember_viewport();
        self.dismiss_language_help();
        self.editor.finish_undo_group();
        let target = &self.windows.panes[&id];
        let (document, view, viewport) = (target.document, target.view, target.viewport);
        if document != self.editor.document().id() {
            let buffer = self
                .windows
                .buffers
                .remove(&document)
                .expect("window's buffer exists");
            let old = self.switch_buffer(buffer);
            self.windows.buffers.insert(old.editor.document().id(), old);
        }
        assert!(self.editor.focus_view(view));
        self.viewport = viewport;
        self.windows.layout.active = id;
        self.keys.cancel();
        self.prompt = None;
    }

    fn views_of(&self, document: DocumentId) -> usize {
        self.windows
            .panes
            .values()
            .filter(|pane| pane.document == document)
            .count()
    }

    /// Resolve identity before loading, so existing buffers retain unsaved text,
    /// savepoints, and undo history even when opened through a symlink alias.
    fn prepare_file(&self, path: &Path) -> io::Result<Prepared> {
        let target = crate::files::resolve(path)?;
        if self.files.target() == Some(target.as_path()) {
            return Ok(Prepared::Existing(self.editor.document().id()));
        }
        if let Some((&id, _)) = self
            .windows
            .buffers
            .iter()
            .find(|(_, buffer)| buffer.files.target() == Some(target.as_path()))
        {
            return Ok(Prepared::Existing(id));
        }
        let (document, files) = FileState::load(Some(path))?;
        let mut editor = Editor::new(document);
        editor.set_language(Language::detect(files.path(), editor.document().text()));
        editor.set_background_search(true);
        editor.set_background_syntax(true);
        Ok(Prepared::New(Box::new(Buffer {
            editor,
            files,
            automatic_language: true,
        })))
    }

    fn replace_window_buffer(&mut self, prepared: Prepared) -> io::Result<()> {
        if matches!(&prepared, Prepared::Existing(id) if *id == self.editor.document().id()) {
            return Ok(());
        }
        let keep_old = self.views_of(self.editor.document().id()) > 1;
        if !keep_old && self.is_dirty() {
            return Err(io::Error::other(
                "save this buffer before opening another file",
            ));
        }
        self.dismiss_language_help();
        self.editor.finish_undo_group();
        let buffer = match prepared {
            Prepared::New(buffer) => *buffer,
            Prepared::Existing(id) => {
                let mut buffer = self.windows.buffers.remove(&id).expect("existing buffer");
                let view = buffer.editor.duplicate_view();
                buffer.editor.focus_view(view);
                buffer
            }
        };
        let old_view = self.editor.active_view();
        let mut old = self.switch_buffer(buffer);
        if keep_old {
            assert!(old.editor.remove_view(old_view));
            self.windows.buffers.insert(old.editor.document().id(), old);
        }
        self.viewport = Viewport::default();
        self.windows.panes.insert(
            self.windows.layout.active,
            Pane {
                content: Content::Document,
                document: self.editor.document().id(),
                view: self.editor.active_view(),
                viewport: self.viewport,
            },
        );
        self.keys.cancel();
        self.prompt = None;
        Ok(())
    }

    pub(super) fn open_window_file(&mut self, path: &Path) -> io::Result<()> {
        let prepared = self.prepare_file(path)?;
        self.replace_window_buffer(prepared)
    }

    pub(super) fn snapshot_for_path(&self, path: &Path) -> Option<vex_core::Snapshot> {
        if self.files.target() == Some(path) {
            return Some(self.editor.document().snapshot());
        }
        self.windows
            .buffers
            .values()
            .find(|buffer| buffer.files.target() == Some(path))
            .map(|buffer| buffer.editor.document().snapshot())
    }

    pub(super) fn open_window_definition(
        &mut self,
        location: &vex_lsp::Location,
    ) -> io::Result<vex_core::CharOffset> {
        let prepared = self.prepare_file(&location.path)?;
        let editor = match &prepared {
            Prepared::Existing(id) if *id == self.editor.document().id() => &self.editor,
            Prepared::Existing(id) => &self.windows.buffers[id].editor,
            Prepared::New(buffer) => &buffer.editor,
        };
        let offset = vex_lsp::offset(editor.document().text(), location.position)
            .ok_or_else(|| io::Error::other("invalid definition position"))?;
        self.replace_window_buffer(prepared)?;
        Ok(offset)
    }

    pub(super) fn open_window_from_picker(&mut self, path: &Path) -> io::Result<()> {
        let prepared = self.prepare_file(path)?;
        if !matches!(&prepared, Prepared::Existing(id) if *id == self.editor.document().id()) {
            if self.views_of(self.editor.document().id()) == 1 && self.is_dirty() {
                return Err(io::Error::other(
                    "save this buffer before opening another file",
                ));
            }
            self.record_jump();
        }
        self.replace_window_buffer(prepared)
    }

    fn split_window(&mut self, axis: Axis) -> io::Result<()> {
        let id = self.windows.layout.split(axis, self.window_area())?;
        let view = self.editor.duplicate_view();
        self.windows.panes.insert(
            id,
            Pane {
                content: Content::Document,
                document: self.editor.document().id(),
                view,
                viewport: self.viewport,
            },
        );
        self.focus_window(id);
        Ok(())
    }

    pub(super) fn split_with_path(&mut self, vertical: bool, path: &str) -> io::Result<()> {
        // File errors must not leave an accidental duplicate window behind.
        let prepared = if path.is_empty() {
            None
        } else {
            Some(self.prepare_file(Path::new(path))?)
        };
        self.split_window(if vertical {
            Axis::Vertical
        } else {
            Axis::Horizontal
        })?;
        if let Some(prepared) = prepared {
            self.replace_window_buffer(prepared)?;
        }
        Ok(())
    }

    pub(super) fn close_window(&mut self, force: bool) -> io::Result<()> {
        let id = self.windows.layout.active;
        let pane = &self.windows.panes[&id];
        let (document, view) = (pane.document, pane.view);
        if self.views_of(document) == 1 && self.is_dirty() && !force {
            return Err(io::Error::other(
                "unsaved changes; use :w to save or :q! to discard",
            ));
        }
        let ids = self.windows.layout.ids();
        if ids.len() == 1 {
            self.quit = true;
            return Ok(());
        }
        let index = ids.iter().position(|other| *other == id).unwrap();
        let next = ids[(index + 1) % ids.len()];
        self.focus_window(next);
        self.windows.layout.remove(id);
        self.windows.panes.remove(&id);
        if document == self.editor.document().id() {
            self.editor.remove_view(view);
        } else if self.views_of(document) == 0 {
            self.windows.buffers.remove(&document);
        } else {
            self.windows
                .buffers
                .get_mut(&document)
                .unwrap()
                .editor
                .remove_view(view);
        }
        Ok(())
    }

    pub(super) fn only_window(&mut self, force: bool) -> io::Result<()> {
        if !force
            && self
                .windows
                .buffers
                .values()
                .any(|buffer| buffer.files.is_dirty(buffer.editor.document()))
        {
            return Err(io::Error::other(
                "another buffer has unsaved changes; save it or use :only! to discard",
            ));
        }
        self.windows.layout.only();
        self.windows
            .panes
            .retain(|id, _| *id == self.windows.layout.active);
        self.windows.buffers.clear();
        self.editor.retain_active_view();
        Ok(())
    }

    pub(super) fn quit_all(&mut self, force: bool) -> io::Result<()> {
        if !force
            && (self.is_dirty()
                || self
                    .windows
                    .buffers
                    .values()
                    .any(|buffer| buffer.files.is_dirty(buffer.editor.document())))
        {
            return Err(io::Error::other(
                "unsaved changes; save the buffers or use :qa! to discard",
            ));
        }
        self.quit = true;
        Ok(())
    }

    pub(super) fn window_action(&mut self, action: WindowAction, count: usize) -> io::Result<()> {
        use WindowAction::*;
        match action {
            SplitVertical => self.split_window(Axis::Vertical),
            SplitHorizontal => self.split_window(Axis::Horizontal),
            Close => self.close_window(false),
            Only => self.only_window(false),
            OpenHorizontal => self.open_selected_files(Axis::Horizontal),
            OpenVertical => self.open_selected_files(Axis::Vertical),
            Rotate => {
                let ids = self.windows.layout.ids();
                let current = ids
                    .iter()
                    .position(|id| *id == self.windows.layout.active)
                    .unwrap();
                self.focus_window(ids[(current + count % ids.len()) % ids.len()]);
                Ok(())
            }
            _ => {
                let direction = match action {
                    FocusLeft | SwapLeft => Direction::Left,
                    FocusDown | SwapDown => Direction::Down,
                    FocusUp | SwapUp => Direction::Up,
                    FocusRight | SwapRight => Direction::Right,
                    _ => unreachable!(),
                };
                for _ in 0..count.min(layout::MAX_WINDOWS) {
                    let Some(other) = self.windows.layout.neighbor(direction, self.window_area())
                    else {
                        break;
                    };
                    if matches!(action, SwapLeft | SwapDown | SwapUp | SwapRight) {
                        self.windows.layout.swap(other);
                    } else {
                        self.focus_window(other);
                    }
                }
                Ok(())
            }
        }
    }

    fn selected_paths(&self) -> io::Result<Vec<PathBuf>> {
        let text = self.editor.document().text();
        let base = self
            .files
            .target()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .map_or_else(std::env::current_dir, Ok)?;
        let mut paths = Vec::new();
        for selection in self.editor.selections().ranges() {
            if paths.len() >= layout::MAX_WINDOWS {
                return Err(io::Error::other("too many selected filenames"));
            }
            let mut range = selection.start().0..selection.end().0;
            if range.len() <= 1 {
                let delimiter = |ch: char| ch.is_whitespace() || "\"'`<>()[]{}".contains(ch);
                while range.start > 0
                    && !delimiter(text.char(range.start - 1))
                    && range.len() <= 4096
                {
                    range.start -= 1;
                }
                while range.end < text.len_chars()
                    && !delimiter(text.char(range.end))
                    && range.len() <= 4096
                {
                    range.end += 1;
                }
            }
            if range.len() > 4096 {
                return Err(io::Error::other("selected filename is too long"));
            }
            let selected = text.slice(range).to_string();
            let name = selected.trim().trim_matches(['\"', '\'', '`']);
            if name.is_empty() {
                return Err(io::Error::other("no filename in selection"));
            }
            let path = base.join(name);
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        Ok(paths)
    }

    fn open_selected_files(&mut self, axis: Axis) -> io::Result<()> {
        let paths = self.selected_paths()?;
        // Validate all splits and file loads before changing the current layout.
        let mut proposed = self.windows.layout.clone();
        let mut prepared = Vec::new();
        for path in paths {
            proposed.active = proposed.split(axis, self.window_area())?;
            if !std::fs::metadata(&path)?.is_file() {
                return Err(io::Error::other("selected path is not a regular file"));
            }
            prepared.push(self.prepare_file(&path)?);
        }
        for buffer in prepared {
            self.split_window(axis)?;
            // Selections may refer to the same file through different aliases.
            let buffer = match buffer {
                Prepared::New(buffer) => {
                    let target = buffer.files.target();
                    if self.files.target() == target {
                        Prepared::Existing(self.editor.document().id())
                    } else if let Some((&id, _)) = self
                        .windows
                        .buffers
                        .iter()
                        .find(|(_, open)| open.files.target() == target)
                    {
                        Prepared::Existing(id)
                    } else {
                        Prepared::New(buffer)
                    }
                }
                existing => existing,
            };
            self.replace_window_buffer(buffer)?;
        }
        Ok(())
    }

    /// Paint views in local coordinates, then copy their cells into the terminal
    /// frame. Popups and completion remain clipped to the focused pane.
    pub(super) fn paint_windows(&mut self, frame: &mut Frame) -> io::Result<()> {
        self.remember_viewport();
        self.editor.begin_syntax_frame();
        for buffer in self.windows.buffers.values() {
            buffer.editor.begin_syntax_frame();
        }
        let (leaves, dividers) = self.windows.layout.visible(self.window_area());
        if leaves.len() == 1 {
            self.paint_current_window(frame, 1)?;
            self.remember_viewport();
            return Ok(());
        }
        let mut local = std::mem::take(&mut self.windows.frame);
        for &(id, rect) in &leaves {
            local.reset(rect.width, rect.height)?;
            if id == self.windows.layout.active {
                self.paint_current_window(&mut local, 0)?;
                self.remember_viewport();
            } else if let Content::Git(key) = &self.windows.panes[&id].content {
                self.status
                    .views
                    .get_mut(key)
                    .unwrap()
                    .paint(&mut local, 0, false);
                local.inactive();
            } else {
                let pane = self.windows.panes.get_mut(&id).unwrap();
                let (editor, files) = if pane.document == self.editor.document().id() {
                    (&mut self.editor, &self.files)
                } else {
                    let buffer = self.windows.buffers.get_mut(&pane.document).unwrap();
                    (&mut buffer.editor, &buffer.files)
                };
                let filename = files
                    .path()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "[scratch]".into());
                let dirty = files.is_dirty(editor.document());
                let git = self.git.diff(
                    editor.document().id(),
                    editor.document().revision(),
                    files.target(),
                );
                editor
                    .with_view(pane.view, |editor| {
                        render::paint_view(
                            &mut local,
                            editor,
                            &mut pane.viewport,
                            Chrome {
                                filename: &filename,
                                dirty,
                                pending: "",
                                message: "",
                                error: false,
                                prompt: None,
                            },
                            0,
                            git,
                        )
                    })
                    .expect("existing view")
                    .map_err(io::Error::other)?;
                local.inactive();
            }
            frame.blit(rect.x, rect.y, &local);
        }
        self.windows.frame = local;
        paint_dividers(frame, &leaves, &dividers);
        Ok(())
    }

    pub(super) fn check_save_target(&self, argument: &str) -> io::Result<()> {
        if !argument.is_empty() {
            let target = crate::files::resolve(Path::new(argument))?;
            if self
                .windows
                .buffers
                .values()
                .any(|buffer| buffer.files.target() == Some(target.as_path()))
            {
                return Err(io::Error::other(
                    "destination is already open in another buffer",
                ));
            }
        }
        Ok(())
    }
}

/// Join pane status rules to vertical dividers, including a divider starting
/// below a wider pane. Embedded labels take precedence over a border junction.
fn paint_dividers(
    frame: &mut Frame,
    panes: &[(WindowId, layout::Rect)],
    dividers: &[layout::Rect],
) {
    let vertical = |x: u16, y: u16| {
        dividers
            .iter()
            .any(|d| d.x == x && d.y <= y && y < d.y + d.height)
    };
    for divider in dividers {
        let x = divider.x;
        for y in divider.y.saturating_sub(1)..divider.y + divider.height {
            if y < divider.y && frame.style_at(x, y) != Some(Style::StatusBorder) {
                continue;
            }
            let up = y.checked_sub(1).is_some_and(|y| vertical(x, y));
            let down = vertical(x, y + 1);
            let left = panes.iter().any(|(_, pane)| {
                pane.y + pane.height - 1 == y && pane.x < x && pane.x + pane.width >= x
            });
            let right = panes.iter().any(|(_, pane)| {
                pane.y + pane.height - 1 == y && pane.x <= x + 1 && pane.x + pane.width > x + 1
            });
            let glyph = match (up, down, left, right) {
                (true, true, true, true) => "┼",
                (true, true, true, false) => "┤",
                (true, true, false, true) => "├",
                (true, false, true, true) => "┴",
                (false, true, true, true) => "┬",
                (true, false, true, false) => "┘",
                (true, false, false, true) => "└",
                (false, true, true, false) => "┐",
                (false, true, false, true) => "┌",
                _ => "│",
            };
            frame.put(x, y, glyph, Style::StatusBorder);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{draw, key, press};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use vex_core::{CharOffset, Document, Selection, SelectionSet};

    fn ctrl(app: &mut App, key: char) {
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char(key),
            KeyModifiers::CONTROL,
        )));
    }

    #[test]
    fn unfocused_cursors_are_dim_and_only_the_focused_pane_owns_the_terminal_cursor() {
        let mut app = App::from_document(Document::from("abc"), (81, 21));
        app.execute("vsplit").unwrap();
        let frame = draw(&mut app);
        assert_eq!(frame.style_at(5, 0), Some(Style::InactiveCursor));
        assert_eq!(frame.style_at(46, 0), Some(Style::PrimaryCursor(None)));
        assert_eq!(frame.cursor.unwrap().x, 46);
        app.execute("jump_view_left").unwrap();
        let frame = draw(&mut app);
        assert_eq!(frame.style_at(5, 0), Some(Style::PrimaryCursor(None)));
        assert_eq!(frame.style_at(46, 0), Some(Style::InactiveCursor));
        assert_eq!(frame.cursor.unwrap().x, 5);
    }

    #[test]
    fn horizontal_panes_meet_at_the_status_line_without_a_divider_row() {
        let mut app = App::from_document(Document::from("alpha\nbeta"), (80, 13));
        app.execute("hsplit").unwrap();
        let (panes, dividers) = app.windows.layout.visible(app.window_area());
        assert!(dividers.is_empty());
        assert_eq!(panes[0].1.height, 6);
        assert_eq!(panes[1].1.y, 6);
        assert_eq!(panes[1].1.height, 6);
        let frame = draw(&mut app);
        assert_eq!(frame.style_at(0, 5), Some(Style::StatusBorder));
        assert_eq!(frame.style_at(2, 5), Some(Style::InactiveStatus));
        assert!(frame.row_text(6).contains("alpha"));
        assert_eq!(frame.style_at(0, 11), Some(Style::StatusBorder));
        assert_eq!(frame.style_at(2, 11), Some(Style::StatusLine));
        assert!(
            (0..13)
                .filter(|row| ![5, 11].contains(row))
                .all(|row| !frame.row_text(row).contains('─'))
        );
        app.execute("vsplit").unwrap();
        let frame = draw(&mut app);
        assert_eq!(frame.row_text(5).chars().nth(39), Some('┬'));
        assert_eq!(frame.row_text(11).chars().nth(39), Some('┴'));
    }

    #[test]
    fn command_search_and_message_line_is_global_beneath_every_split() {
        let mut app = App::from_document(Document::from("alpha beta"), (81, 21));
        app.execute("vsplit").unwrap();
        app.execute("hsplit").unwrap();
        press(&mut app, ":help");
        let frame = draw(&mut app);
        assert_eq!(frame.cursor.unwrap().y, 20);
        assert!(frame.row_text(20).starts_with(":help"));
        assert_eq!(
            (0..21)
                .filter(|&row| frame.row_text(row).contains(":help"))
                .count(),
            1
        );
        let (panes, _) = app.windows.layout.visible(app.window_area());
        for (_, rect) in panes {
            assert!(matches!(
                frame.style_at(rect.x + 2, rect.y + rect.height - 1),
                Some(Style::StatusLine | Style::InactiveStatus)
            ));
            assert!(rect.y + rect.height <= 20);
        }
        assert_eq!(frame.row_text(9).chars().nth(40), Some('├'));
        assert_eq!(frame.row_text(19).chars().nth(40), Some('┴'));
        key(&mut app, KeyCode::Esc);
        press(&mut app, "/beta");
        let frame = draw(&mut app);
        assert_eq!(frame.cursor.unwrap().y, 20);
        assert!(frame.row_text(20).starts_with("/beta"));
        key(&mut app, KeyCode::Esc);
        app.fail("this message spans the terminal rather than a narrow pane");
        let frame = draw(&mut app);
        assert!(
            frame
                .row_text(20)
                .starts_with("this message spans the terminal rather than a narrow pane")
        );
        assert_eq!(frame.style_at(70, 20), Some(Style::Text));
    }

    #[test]
    fn switching_windows_cancels_a_search_tied_to_the_old_view() {
        let mut app = App::from_document(Document::from("origin target"), (80, 24));
        app.editor.set_background_search(true);
        app.execute("vsplit").unwrap();
        press(&mut app, "/target");
        let result = app.editor.take_search_job().unwrap().run().unwrap();
        app.execute("jump_view_left").unwrap();
        assert!(!app.handle_search_result(result));
        assert!(app.prompt.is_none());
        assert_eq!(app.editor.selections().primary().start(), CharOffset(0));
    }

    #[test]
    fn splits_render_shared_text_with_independent_cursors_and_viewports() {
        let source = (0..100)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        let mut app = App::from_document(Document::from(source.as_str()), (81, 21));
        ctrl(&mut app, 'w');
        press(&mut app, "v");
        assert_eq!(app.windows.panes.len(), 2);
        assert_eq!(app.windows.buffers.len(), 0);
        let right = app.windows.layout.active;
        press(&mut app, "30j");
        let frame = draw(&mut app);
        let viewport = app.viewport;
        assert!(frame.cursor.unwrap().x > 40);
        assert_eq!(frame.row_text(0).chars().nth(40), Some('│'));
        assert!(frame.row_text(0).starts_with("   1   line 0"));
        assert!(viewport.top_line > 0);
        press(&mut app, " wh");
        assert_eq!(app.windows.layout.active, 0);
        assert_eq!(app.viewport.top_line, 0);
        press(&mut app, "iXX");
        key(&mut app, KeyCode::Esc);
        let frame = draw(&mut app);
        assert!(frame.row_text(0).contains("XXline 0"));
        press(&mut app, " wl");
        assert_eq!(app.windows.layout.active, right);
        assert_eq!(app.viewport, viewport);
        assert_eq!(
            app.editor
                .document()
                .text()
                .char_to_line(app.editor.selections().primary().start().0),
            30
        );
        // Undo from the other pane still uses the same document history.
        press(&mut app, "u");
        assert_eq!(app.editor.document().text(), source.as_str());
        assert!(!app.is_dirty());
        press(&mut app, " wH");
        assert_eq!(app.windows.layout.ids(), vec![right, 0]);
        assert!(draw(&mut app).cursor.unwrap().x < 40);
    }

    #[test]
    fn control_aliases_do_not_save_or_quit_and_half_pages_use_the_pane_height() {
        let mut app = App::from_document(Document::from("abc\n".repeat(100).as_str()), (80, 23));
        ctrl(&mut app, 'w');
        ctrl(&mut app, 's');
        assert_eq!(app.windows.panes.len(), 2);
        assert_eq!(app.active_size(), (80, 11));
        assert!(!app.error);
        ctrl(&mut app, 'd');
        assert_eq!(app.editor.selections().primary().start(), CharOffset(20));
        ctrl(&mut app, 'w');
        ctrl(&mut app, 'q');
        assert_eq!(app.windows.panes.len(), 1);
        assert!(!app.should_quit());
        assert_eq!(app.editor.selections().primary().start(), CharOffset(0));
        app.handle(Event::Resize(1, 1));
        draw(&mut app);
        let original = app.windows.layout.ids();
        assert!(app.execute("vsplit").is_err());
        assert_eq!(app.windows.layout.ids(), original);
    }

    #[test]
    fn different_buffers_reopen_without_losing_changes_and_share_savepoints() {
        let directory = tempfile::tempdir().unwrap();
        let a = directory.path().join("a.txt");
        let b = directory.path().join("b.txt");
        std::fs::write(&a, "alpha").unwrap();
        std::fs::write(&b, "beta").unwrap();
        let mut app = App::open(Some(&a), (100, 24)).unwrap();
        let original = app.editor.document().id();
        press(&mut app, "iX");
        key(&mut app, KeyCode::Esc);
        app.execute(&format!("vsplit {}", b.display())).unwrap();
        assert_eq!(app.editor.document().text(), "beta");
        press(&mut app, "iY");
        key(&mut app, KeyCode::Esc);
        assert!(app.execute("q").is_err());
        assert!(app.execute("only").is_err());
        assert!(app.execute("qa").is_err());
        assert_eq!(app.windows.panes.len(), 2);
        // An explicit save cannot overwrite a different open buffer, even with !.
        assert!(app.execute(&format!("w! {}", a.display())).is_err());
        app.execute("w").unwrap();
        app.open_window_file(&a).unwrap();
        assert_eq!(app.editor.document().id(), original);
        assert_eq!(app.editor.document().text(), "Xalpha");
        assert!(app.is_dirty());
        assert!(app.windows.buffers.is_empty());
        app.execute("w").unwrap();
        app.execute("jump_view_left").unwrap();
        assert!(!app.is_dirty());
        app.execute("q").unwrap();
        assert!(!app.should_quit());
        app.execute("q").unwrap();
        assert!(app.should_quit());
        assert_eq!(std::fs::read_to_string(a).unwrap(), "Xalpha");
        assert_eq!(std::fs::read_to_string(b).unwrap(), "Ybeta");
    }

    #[test]
    fn dirty_shared_views_can_close_but_the_last_view_is_protected() {
        let mut app = App::from_document(Document::from("abc"), (80, 24));
        press(&mut app, "iX");
        key(&mut app, KeyCode::Esc);
        app.execute("vsplit").unwrap();
        app.execute("hsplit").unwrap();
        app.execute("only").unwrap();
        assert_eq!(app.windows.panes.len(), 1);
        assert_eq!(app.editor.document().text(), "Xabc");
        assert!(app.execute("q").is_err());
        app.execute("vsplit").unwrap();
        app.execute("q").unwrap();
        assert!(!app.should_quit());
        assert!(app.execute("q").is_err());
        app.execute("q!").unwrap();
        assert!(app.should_quit());
    }

    #[test]
    fn selected_filenames_open_relative_to_current_file_and_failure_is_atomic() {
        let directory = tempfile::tempdir().unwrap();
        let origin = directory.path().join("origin.txt");
        std::fs::write(&origin, "one.txt two.txt").unwrap();
        std::fs::write(directory.path().join("one.txt"), "one").unwrap();
        let mut app = App::open(Some(&origin), (120, 30)).unwrap();
        app.editor
            .set_selections(
                SelectionSet::new(
                    vec![
                        Selection::new(CharOffset(0), CharOffset(7)),
                        Selection::new(CharOffset(8), CharOffset(15)),
                    ],
                    0,
                )
                .unwrap(),
            )
            .unwrap();
        assert!(app.open_selected_files(Axis::Vertical).is_err());
        assert_eq!(app.windows.panes.len(), 1);
        std::fs::write(directory.path().join("two.txt"), "two").unwrap();
        ctrl(&mut app, 'w');
        press(&mut app, "F");
        assert_eq!(app.windows.panes.len(), 3);
        assert_eq!(app.editor.document().text(), "two");
        app.execute("jump_view_left").unwrap();
        assert_eq!(app.editor.document().text(), "one");
        app.execute("jump_view_left").unwrap();
        app.editor
            .set_selections(SelectionSet::single(Selection::new(
                CharOffset(3),
                CharOffset(4),
            )))
            .unwrap();
        assert_eq!(
            app.selected_paths().unwrap(),
            vec![directory.path().join("one.txt").canonicalize().unwrap()]
        );
    }

    #[test]
    fn resize_temporarily_collapses_panes_and_restores_their_positions() {
        let mut app = App::from_document(Document::from("abc"), (80, 24));
        app.execute("vsplit").unwrap();
        press(&mut app, "ll");
        let cursor = draw(&mut app).cursor.unwrap();
        app.handle(Event::Resize(10, 3));
        assert_eq!(app.active_size(), (10, 2));
        assert_eq!(app.windows.panes.len(), 2);
        assert!(draw(&mut app).cursor.unwrap().x < 10);
        app.handle(Event::Resize(80, 24));
        assert_eq!(draw(&mut app).cursor.unwrap(), cursor);
    }

    #[test]
    fn highlighting_batches_include_all_buffers_and_survive_superseded_work() {
        let directory = tempfile::tempdir().unwrap();
        let a = directory.path().join("a.rs");
        let b = directory.path().join("b.rs");
        std::fs::write(&a, "fn alpha() {}\n").unwrap();
        std::fs::write(&b, "fn beta() {}\n").unwrap();
        let mut app = App::open(Some(&a), (100, 24)).unwrap();
        app.editor.set_background_syntax(true);
        app.execute(&format!("vsplit {}", b.display())).unwrap();
        draw(&mut app);
        let first = app.take_syntax_batch().unwrap();
        assert_eq!(first.jobs.len(), 2);
        assert!(app.take_syntax_batch().is_none());
        press(&mut app, "i//");
        key(&mut app, KeyCode::Esc);
        draw(&mut app);
        let second = app.take_syntax_batch().unwrap();
        assert_eq!(second.jobs.len(), 2);
        assert!(
            first
                .jobs
                .iter()
                .all(|job| job.cancellation().is_cancelled())
        );
        let results = second
            .jobs
            .into_iter()
            .filter_map(|job| vex_editor::SyntaxWorker::default().run(job))
            .collect();
        assert!(app.handle_syntax_results(results));
        let frame = draw(&mut app);
        assert_eq!(
            frame.style_at(6, 0),
            Some(Style::Syntax(vex_editor::Highlight::Keyword))
        );
        assert!(app.take_syntax_batch().is_none());
        // Focus does not replace the document or its cached colors.
        app.execute("jump_view_left").unwrap();
        draw(&mut app);
        assert!(app.take_syntax_batch().is_none());
    }
}
