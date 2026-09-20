//! Pane views over retained buffers. Hiding or closing a pane never discards
//! document text or history; quitting protects every unsaved buffer.

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
use vex_core::{DocumentId, Revision};
use vex_editor::{BufferAction, Editor, Language, ViewId, WindowAction};

enum Content {
    Document,
    Git(PathBuf),
}

#[derive(Clone, Copy)]
struct SavedView {
    view: ViewId,
    viewport: Viewport,
}

struct Pane {
    // The document stays owned by the pane while an auxiliary view is shown.
    content: Content,
    document: DocumentId,
    view: ViewId,
    viewport: Viewport,
    saved: HashMap<DocumentId, SavedView>,
    last_accessed: Option<DocumentId>,
    last_modified: [Option<DocumentId>; 2],
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
    // IDs increase in opening order. Successor/predecessor lookup avoids
    // sorting the full buffer catalog for every gn/gp.
    buffers: BTreeMap<DocumentId, Buffer>,
    accessed: HashMap<DocumentId, u64>,
    access_clock: u64,
    observed_revision: Revision,
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
                    saved: HashMap::new(),
                    last_accessed: None,
                    last_modified: [None; 2],
                },
            )]),
            buffers: BTreeMap::new(),
            accessed: HashMap::from([(editor.document().id(), 0)]),
            access_clock: 0,
            observed_revision: editor.document().revision(),
            frame: Frame::default(),
            syntax_documents: Vec::new(),
        }
    }
}

impl App {
    pub(super) fn open_commit_draft(
        &mut self,
        root: PathBuf,
        status_key: PathBuf,
    ) -> io::Result<()> {
        let existing = self
            .git_write
            .drafts
            .iter()
            .find(|(_, draft)| draft.root == root)
            .map(|(id, _)| *id);
        if let Some(id) = existing
            && let Some(window) = self
                .windows
                .panes
                .iter()
                .find(|(_, pane)| pane.document == id && matches!(pane.content, Content::Document))
                .map(|(id, _)| *id)
        {
            self.focus_window(window);
        } else {
            if existing.is_none() && self.git_write.drafts.len() >= 16 {
                return Err(io::Error::other(
                    "too many retained commit drafts (16 repository limit)",
                ));
            }
            // A split retains the original document and status pane. Horizontal
            // fallback also permits composing on narrow terminals.
            if self.split_window(Axis::Vertical).is_err() {
                self.split_window(Axis::Horizontal)?;
            }
            let prepared = if let Some(id) = existing {
                Prepared::Existing(id)
            } else {
                let document = vex_core::Document::default();
                let files = FileState::scratch(&document);
                let mut editor = Editor::with_yank_register(document, self.editor.yank_register());
                editor.set_background_search(true);
                editor.execute("insert_mode", 1).map_err(io::Error::other)?;
                self.git_write.drafts.insert(
                    editor.document().id(),
                    super::git_write::Draft {
                        root,
                        status_key,
                        committed: None,
                    },
                );
                Prepared::New(Box::new(Buffer {
                    editor,
                    files,
                    automatic_language: false,
                }))
            };
            self.replace_window_buffer(prepared)?;
        }
        let id = self.editor.document().id();
        if self.git_write.drafts[&id].committed == Some(self.editor.document().revision()) {
            // Insert mode normalizes selections to insertion cursors. Leave it
            // before selecting the completed message for removal.
            self.editor
                .execute("normal_mode", 1)
                .map_err(io::Error::other)?;
            self.editor
                .set_selections(vex_core::SelectionSet::single(vex_core::Selection::new(
                    vex_core::CharOffset(0),
                    vex_core::CharOffset(self.editor.document().text().len_chars()),
                )))
                .map_err(io::Error::other)?;
            self.editor
                .execute("delete_selection_without_yank", 1)
                .map_err(io::Error::other)?;
            self.editor.finish_undo_group();
            self.files = FileState::scratch(self.editor.document());
            self.editor
                .execute("insert_mode", 1)
                .map_err(io::Error::other)?;
            self.git_write.drafts.get_mut(&id).unwrap().committed = None;
        }
        Ok(())
    }

    pub(super) fn leave_commit_draft(&mut self, status_key: &Path) -> io::Result<()> {
        if self.windows.panes.len() > 1 {
            self.close_window(true)?;
        }
        if let Some(window) = self
            .windows
            .panes
            .iter()
            .find(|(_, pane)| matches!(&pane.content, Content::Git(key) if key == status_key))
            .map(|(id, _)| *id)
        {
            self.focus_window(window);
        } else {
            self.set_git_view(Some(status_key.into()));
        }
        Ok(())
    }

    pub(super) fn commit_draft_revision(
        &self,
        id: DocumentId,
        text: &str,
    ) -> Option<vex_core::Revision> {
        let editor = if self.editor.document().id() == id {
            &self.editor
        } else {
            &self.windows.buffers.get(&id)?.editor
        };
        (editor.document().text() == text).then_some(editor.document().revision())
    }

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

    fn visible_documents(&self) -> Vec<DocumentId> {
        let mut documents: Vec<_> = self
            .windows
            .layout
            .visible(self.window_area())
            .0
            .into_iter()
            .filter_map(|(id, rect)| {
                let pane = &self.windows.panes[&id];
                (rect.width > 0 && rect.height > 0 && matches!(pane.content, Content::Document))
                    .then_some(pane.document)
            })
            .collect();
        documents.sort_unstable();
        documents.dedup();
        documents
    }

    pub(super) fn git_documents(&self) -> Vec<vex_git::Document> {
        self.visible_documents()
            .into_iter()
            .filter_map(|id| {
                let (editor, files) = if id == self.editor.document().id() {
                    (&self.editor, &self.files)
                } else {
                    let buffer = &self.windows.buffers[&id];
                    (&buffer.editor, &buffer.files)
                };
                files.target().map(|path| vex_git::Document {
                    path: path.into(),
                    snapshot: editor.document().snapshot(),
                })
            })
            .collect()
    }

    pub(super) fn visible_file_probes(&self) -> Vec<crate::files::watch::Probe> {
        let mut documents = std::collections::HashSet::new();
        self.windows
            .layout
            .visible(self.window_area())
            .0
            .into_iter()
            .filter_map(|(id, rect)| {
                let pane = &self.windows.panes[&id];
                if rect.width == 0
                    || rect.height == 0
                    || !matches!(pane.content, Content::Document)
                    || !documents.insert(pane.document)
                {
                    return None;
                }
                if pane.document == self.editor.document().id() {
                    self.files.probe(self.editor.document())
                } else {
                    let buffer = &self.windows.buffers[&pane.document];
                    buffer.files.probe(buffer.editor.document())
                }
            })
            .collect()
    }

    pub(super) fn with_file_buffer_mut<T>(
        &mut self,
        id: DocumentId,
        f: impl FnOnce(&mut Editor, &mut FileState, bool) -> T,
    ) -> Option<T> {
        if id == self.editor.document().id() {
            Some(f(
                &mut self.editor,
                &mut self.files,
                self.automatic_language,
            ))
        } else {
            let buffer = self.windows.buffers.get_mut(&id)?;
            Some(f(
                &mut buffer.editor,
                &mut buffer.files,
                buffer.automatic_language,
            ))
        }
    }

    pub(crate) fn take_syntax_batch(&mut self) -> Option<crate::events::SyntaxBatch> {
        let documents = self.visible_documents();
        let mut changed = documents != self.windows.syntax_documents;
        if changed {
            for id in self.windows.syntax_documents.clone() {
                if !documents.contains(&id) {
                    self.with_file_buffer_mut(id, |editor, _, _| {
                        editor.begin_syntax_frame();
                        editor.cancel_syntax_request();
                        editor.release_syntax_tree();
                    });
                }
            }
        }
        for &id in &documents {
            changed |= self
                .with_file_buffer_mut(id, |editor, _, _| editor.take_syntax_job().is_some())
                .unwrap();
        }
        if !changed {
            return None;
        }
        // Replacing a batch replaces pending work for all visible buffers.
        self.windows.syntax_documents.clone_from(&documents);
        let jobs = documents
            .iter()
            .filter_map(|&id| {
                self.with_file_buffer_mut(id, |editor, _, _| {
                    editor.cancel_syntax_request();
                    editor.take_syntax_job()
                })
                .flatten()
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
        self.observe_buffer_revision();
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
        self.record_buffer_access();
        self.keys.cancel();
        self.git_write.prefix = None;
        self.git_write.status_prefix = None;
        self.prompt = None;
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
        let mut editor = Editor::with_yank_register(document, self.editor.yank_register());
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
            self.set_git_view(None);
            return Ok(());
        }
        self.dismiss_language_help();
        self.observe_buffer_revision();
        self.editor.finish_undo_group();
        let window = self.windows.layout.active;
        let old_id = self.editor.document().id();
        let old_view = SavedView {
            view: self.editor.active_view(),
            viewport: self.viewport,
        };
        self.windows
            .panes
            .get_mut(&window)
            .unwrap()
            .saved
            .insert(old_id, old_view);
        let (buffer, viewport) = match prepared {
            Prepared::New(buffer) => (*buffer, Viewport::default()),
            Prepared::Existing(id) => {
                let saved = self
                    .windows
                    .panes
                    .get_mut(&window)
                    .unwrap()
                    .saved
                    .remove(&id);
                let owned = self
                    .windows
                    .panes
                    .values()
                    .any(|pane| pane.document == id || pane.saved.contains_key(&id));
                let mut buffer = self.windows.buffers.remove(&id).expect("existing buffer");
                let viewport = if let Some(saved) = saved {
                    assert!(buffer.editor.focus_view(saved.view));
                    saved.viewport
                } else {
                    if owned {
                        let view = buffer.editor.duplicate_view();
                        buffer.editor.focus_view(view);
                    }
                    Viewport::default()
                };
                (buffer, viewport)
            }
        };
        let old = self.switch_buffer(buffer);
        self.windows.buffers.insert(old_id, old);
        self.viewport = viewport;
        let pane = self.windows.panes.get_mut(&window).unwrap();
        pane.content = Content::Document;
        pane.document = self.editor.document().id();
        pane.view = self.editor.active_view();
        pane.viewport = viewport;
        pane.last_accessed = Some(old_id);
        self.record_buffer_access();
        self.keys.cancel();
        self.prompt = None;
        Ok(())
    }

    pub(super) fn open_window_file(&mut self, path: &Path) -> io::Result<()> {
        let prepared = self.prepare_file(path)?;
        self.replace_window_buffer(prepared)
    }

    pub(super) fn open_buffer(&mut self, id: DocumentId) -> io::Result<()> {
        if id != self.editor.document().id() && !self.windows.buffers.contains_key(&id) {
            return Err(io::Error::other("buffer is no longer open"));
        }
        self.replace_window_buffer(Prepared::Existing(id))
    }

    fn record_buffer_access(&mut self) {
        self.windows.access_clock += 1;
        self.windows
            .accessed
            .insert(self.editor.document().id(), self.windows.access_clock);
        self.windows.observed_revision = self.editor.document().revision();
    }

    /// Constant-time bookkeeping; never inspects document contents or hidden buffers.
    pub(super) fn observe_buffer_revision(&mut self) {
        let revision = self.editor.document().revision();
        if revision == self.windows.observed_revision {
            return;
        }
        self.windows.observed_revision = revision;
        let id = self.editor.document().id();
        let history = &mut self
            .windows
            .panes
            .get_mut(&self.windows.layout.active)
            .unwrap()
            .last_modified;
        if history[0] != Some(id) {
            *history = [Some(id), history[0]];
        }
    }

    pub(super) fn buffer_action(&mut self, action: BufferAction, count: usize) -> io::Result<()> {
        use std::ops::Bound::{Excluded, Unbounded};
        self.observe_buffer_revision();
        let current = self.editor.document().id();
        let pane = &self.windows.panes[&self.windows.layout.active];
        let id = match action {
            BufferAction::OpenSelected => return self.open_selected_paths(None),
            BufferAction::LastAccessed => pane
                .last_accessed
                .ok_or_else(|| io::Error::other("no last accessed buffer"))?,
            BufferAction::LastModified => pane
                .last_modified
                .into_iter()
                .flatten()
                .find(|&id| id != current)
                .ok_or_else(|| io::Error::other("no other modified buffer in this pane"))?,
            BufferAction::Next | BufferAction::Previous => {
                let count = count % (self.windows.buffers.len() + 1);
                if count == 0 {
                    return Ok(());
                }
                let before = self.windows.buffers.range(..current).map(|(&id, _)| id);
                let after = self
                    .windows
                    .buffers
                    .range((Excluded(current), Unbounded))
                    .map(|(&id, _)| id);
                if action == BufferAction::Next {
                    after.chain(before).nth(count - 1).unwrap()
                } else {
                    before.rev().chain(after.rev()).nth(count - 1).unwrap()
                }
            }
        };
        self.open_buffer(id)
    }

    /// Explicitly release a document in every pane. Pane closing only hides it.
    pub(super) fn close_buffer(&mut self, force: bool) -> io::Result<()> {
        let closing = self.editor.document().id();
        if self.is_dirty() && !force {
            return Err(io::Error::other(
                "unsaved changes; save the buffer or use :bc! to discard",
            ));
        }
        if self.git_write.drafts.contains_key(&closing) {
            self.check_git_writes_finished()?;
        }
        let active = self.windows.layout.active;
        let replacement = self.windows.panes[&active]
            .last_accessed
            .filter(|id| self.windows.buffers.contains_key(id))
            .or_else(|| self.windows.buffers.keys().next().copied());
        let prepared = if let Some(id) = replacement {
            Prepared::Existing(id)
        } else {
            let document = vex_core::Document::default();
            let files = FileState::scratch(&document);
            let mut editor = Editor::with_yank_register(document, self.editor.yank_register());
            editor.set_background_search(true);
            editor.set_background_syntax(true);
            Prepared::New(Box::new(Buffer {
                editor,
                files,
                automatic_language: true,
            }))
        };
        self.replace_window_buffer(prepared)?;
        let replacement = self.editor.document().id();
        let other_panes: Vec<_> = self
            .windows
            .panes
            .iter()
            .filter(|(_, pane)| pane.document == closing)
            .map(|(&id, _)| id)
            .collect();
        for id in other_panes {
            self.focus_window(id);
            let content = std::mem::replace(
                &mut self.windows.panes.get_mut(&id).unwrap().content,
                Content::Document,
            );
            self.open_buffer(replacement)?;
            self.windows.panes.get_mut(&id).unwrap().content = content;
        }
        self.focus_window(active);
        for pane in self.windows.panes.values_mut() {
            pane.saved.remove(&closing);
            if pane.last_accessed == Some(closing) {
                pane.last_accessed = None;
            }
            for modified in &mut pane.last_modified {
                if *modified == Some(closing) {
                    *modified = None;
                }
            }
        }
        self.windows.buffers.remove(&closing);
        self.windows.accessed.remove(&closing);
        self.git_write.drafts.remove(&closing);
        Ok(())
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

    pub(super) fn buffer_catalog(&self) -> std::sync::Arc<[crate::picker::buffers::CatalogEntry]> {
        use crate::picker::{Entry, buffers::CatalogEntry};
        use std::sync::Arc;
        let current = self.editor.document().id();
        std::iter::once((&self.editor, &self.files))
            .chain(
                self.windows
                    .buffers
                    .values()
                    .map(|buffer| (&buffer.editor, &buffer.files)),
            )
            .map(|(editor, files)| {
                let id = editor.document().id();
                let mut label = self.commit_title(id).unwrap_or_else(|| {
                    files
                        .path()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "[scratch]".into())
                });
                let dirty = files.is_dirty(editor.document());
                if dirty || id == current {
                    label.push_str("  [");
                    if id == current {
                        label.push('*');
                    }
                    if dirty {
                        label.push('+');
                    }
                    label.push(']');
                }
                CatalogEntry {
                    entry: Arc::new(Entry { label, value: id }),
                    accessed: self.windows.accessed[&id],
                }
            })
            .collect()
    }

    pub(super) fn buffer_preview(
        &self,
        id: DocumentId,
        session: u64,
        request: u64,
        cancellation: vex_editor::background::Cancellation,
    ) -> Option<crate::picker::files::PreviewJob> {
        let (editor, files) = if id == self.editor.document().id() {
            (&self.editor, &self.files)
        } else {
            let buffer = self.windows.buffers.get(&id)?;
            (&buffer.editor, &buffer.files)
        };
        let line = editor
            .document()
            .text()
            .char_to_line(editor.selections().primary().head.0);
        Some(crate::picker::files::PreviewJob {
            session,
            request,
            cancellation,
            path: files.target().map(Path::to_path_buf),
            snapshot: Some(editor.document().snapshot()),
            language: editor.language(),
            position: Some(vex_lsp::Position {
                line: line.try_into().ok()?,
                character: 0,
            }),
        })
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
            self.record_jump();
        }
        self.replace_window_buffer(prepared)
    }

    fn split_window(&mut self, axis: Axis) -> io::Result<()> {
        self.observe_buffer_revision();
        let last_modified = self.windows.panes[&self.windows.layout.active].last_modified;
        let id = self.windows.layout.split(axis, self.window_area())?;
        let view = self.editor.duplicate_view();
        self.windows.panes.insert(
            id,
            Pane {
                content: Content::Document,
                document: self.editor.document().id(),
                view,
                viewport: self.viewport,
                saved: HashMap::new(),
                last_accessed: None,
                last_modified,
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

    fn remove_pane_views(&mut self, pane: Pane) {
        for (document, view) in std::iter::once((pane.document, pane.view))
            .chain(pane.saved.into_iter().map(|(id, saved)| (id, saved.view)))
        {
            self.with_file_buffer_mut(document, |editor, _, _| {
                editor.remove_view(view);
            });
        }
    }

    pub(super) fn close_window(&mut self, force: bool) -> io::Result<()> {
        let id = self.windows.layout.active;
        let ids = self.windows.layout.ids();
        if ids.len() == 1 {
            return self.quit_all(force);
        }
        let index = ids.iter().position(|other| *other == id).unwrap();
        self.focus_window(ids[(index + 1) % ids.len()]);
        self.windows.layout.remove(id);
        let pane = self.windows.panes.remove(&id).unwrap();
        self.remove_pane_views(pane);
        Ok(())
    }

    pub(super) fn only_window(&mut self, _force: bool) -> io::Result<()> {
        self.windows.layout.only();
        let active = self
            .windows
            .panes
            .remove(&self.windows.layout.active)
            .unwrap();
        let removed = std::mem::take(&mut self.windows.panes);
        self.windows
            .panes
            .insert(self.windows.layout.active, active);
        for pane in removed.into_values() {
            self.remove_pane_views(pane);
        }
        Ok(())
    }

    pub(super) fn quit_all(&mut self, force: bool) -> io::Result<()> {
        self.check_git_writes_finished()?;
        if !force
            && ((self.is_dirty()
                && !self
                    .git_write
                    .drafts
                    .contains_key(&self.editor.document().id()))
                || self.windows.buffers.iter().any(|(id, buffer)| {
                    !self.git_write.drafts.contains_key(id)
                        && buffer.files.is_dirty(buffer.editor.document())
                }))
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
        self.open_selected_paths(Some(axis))
    }

    fn open_selected_paths(&mut self, axis: Option<Axis>) -> io::Result<()> {
        let paths = self.selected_paths()?;
        // Validate all splits and file loads before changing the current layout.
        let mut proposed = self.windows.layout.clone();
        let mut prepared = Vec::new();
        for path in paths {
            if let Some(axis) = axis {
                proposed.active = proposed.split(axis, self.window_area())?;
            }
            if !std::fs::metadata(&path)?.is_file() {
                return Err(io::Error::other("selected path is not a regular file"));
            }
            prepared.push(self.prepare_file(&path)?);
        }
        self.record_jump();
        for buffer in prepared {
            if let Some(axis) = axis {
                self.split_window(axis)?;
            }
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
        for id in self.visible_documents() {
            self.with_file_buffer_mut(id, |editor, _, _| editor.begin_syntax_frame());
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
                let filename = self
                    .git_write
                    .drafts
                    .get(&pane.document)
                    .map(|draft| {
                        format!(
                            "Git commit · {}",
                            draft.root.file_name().unwrap_or_default().to_string_lossy()
                        )
                    })
                    .unwrap_or_else(|| {
                        files
                            .path()
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| "[scratch]".into())
                    });
                let dirty = files.is_dirty(editor.document());
                let git = self.git.gutter_diff(editor.document().id(), files.target());
                editor
                    .with_view(pane.view, |editor| {
                        render::paint_view(
                            &mut local,
                            editor,
                            &mut pane.viewport,
                            Chrome {
                                filename: &filename,
                                title: self.git_write.drafts.contains_key(&pane.document),
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
    fn yank_survives_replacing_buffers_and_is_isolated_between_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.txt");
        let target = dir.path().join("target.txt");
        std::fs::write(&source, "cat dog").unwrap();
        std::fs::write(&target, "xy").unwrap();
        let mut app = App::open(Some(&source), (80, 24)).unwrap();
        app.editor
            .set_selections(SelectionSet::single(Selection::new(
                CharOffset(0),
                CharOffset(3),
            )))
            .unwrap();
        press(&mut app, "y");
        assert!(!app.is_dirty());
        app.open_window_file(&target).unwrap();
        press(&mut app, "p");
        assert!(!app.error, "{}", app.message);
        assert_eq!(app.editor.document().text(), "xcaty");
        press(&mut app, "u");
        app.open_window_file(&source).unwrap();
        press(&mut app, "P");
        assert_eq!(app.editor.document().text(), "catcat dog");

        let mut other = App::open(Some(&target), (80, 24)).unwrap();
        press(&mut other, "p");
        assert!(other.error);
        assert_eq!(other.editor.document().text(), "xy");
    }

    #[test]
    fn cuts_and_replacement_share_the_register_across_existing_split_buffers() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, "dog").unwrap();
        let mut app = App::from_document(Document::from("cat"), (80, 24));
        press(&mut app, "y");
        app.execute("vsplit").unwrap();
        app.open_window_file(&target).unwrap();
        press(&mut app, "p");
        assert_eq!(app.editor.document().text(), "dcog");
        press(&mut app, "u");
        press(&mut app, "d");
        assert_eq!(app.editor.document().text(), "og");
        app.execute("jump_view_left").unwrap();
        press(&mut app, "R");
        assert_eq!(app.editor.document().text(), "dat");
        app.execute("jump_view_right").unwrap();
        press(&mut app, "P");
        assert_eq!(app.editor.document().text(), "dog");
        press(&mut app, "u");
        assert_eq!(app.editor.document().text(), "og");
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
        assert!(app.execute("qa").is_err());
        assert_eq!(app.windows.panes.len(), 2);
        // An explicit save cannot overwrite a different open buffer, even with !.
        assert!(app.execute(&format!("w! {}", a.display())).is_err());
        app.execute("w").unwrap();
        app.open_window_file(&a).unwrap();
        assert_eq!(app.editor.document().id(), original);
        assert_eq!(app.editor.document().text(), "Xalpha");
        assert!(app.is_dirty());
        assert_eq!(app.windows.buffers.len(), 1);
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
    fn closing_panes_retains_hidden_unsaved_buffers_and_undo() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("clean.txt");
        std::fs::write(&path, "clean").unwrap();
        let mut app = App::from_document(Document::from("scratch"), (80, 24));
        let scratch = app.editor.document().id();
        press(&mut app, "iX");
        key(&mut app, KeyCode::Esc);
        app.execute(&format!("vsplit {}", path.display())).unwrap();
        app.execute("only").unwrap();
        assert_eq!(app.windows.panes.len(), 1);
        assert!(!app.is_dirty());
        assert!(app.execute("q").is_err());
        app.replace_window_buffer(Prepared::Existing(scratch))
            .unwrap();
        assert_eq!(app.editor.document().text(), "Xscratch");
        press(&mut app, "u");
        assert_eq!(app.editor.document().text(), "scratch");
        app.execute("vsplit").unwrap();
        app.open_window_file(&path).unwrap();
        press(&mut app, "iY");
        key(&mut app, KeyCode::Esc);
        app.execute("q").unwrap();
        assert!(!app.should_quit());
        assert_eq!(app.editor.document().id(), scratch);
        assert!(app.execute("q").is_err());
        app.open_window_file(&path).unwrap();
        assert_eq!(app.editor.document().text(), "Yclean");
        press(&mut app, "u");
        assert!(!app.is_dirty());
        app.execute("q").unwrap();
        assert!(app.should_quit());
    }

    #[test]
    fn switching_restores_each_panes_selections_and_viewport() {
        let directory = tempfile::tempdir().unwrap();
        let a = directory.path().join("a.txt");
        let b = directory.path().join("b.txt");
        std::fs::write(&a, "line\n".repeat(100)).unwrap();
        std::fs::write(&b, "beta").unwrap();
        let mut app = App::open(Some(&a), (100, 24)).unwrap();
        press(&mut app, "30j");
        draw(&mut app);
        let first = app.editor.selections().clone();
        let viewport = app.viewport;
        app.execute("vsplit").unwrap();
        press(&mut app, "20j");
        let second = app.editor.selections().clone();
        app.open_window_file(&b).unwrap();
        app.execute("jump_view_left").unwrap();
        app.open_window_file(&b).unwrap();
        app.open_window_file(&a).unwrap();
        assert_eq!(app.editor.selections(), &first);
        assert_eq!(app.viewport, viewport);
        app.execute("jump_view_right").unwrap();
        app.open_window_file(&a).unwrap();
        assert_eq!(app.editor.selections(), &second);
        app.execute("only").unwrap();
        // Closing other panes drops their saved views, without dropping text.
        app.open_window_file(&b).unwrap();
        app.open_window_file(&a).unwrap();
        assert_eq!(app.editor.selections(), &second);
    }

    #[test]
    fn buffer_navigation_cycles_counts_and_tracks_each_panes_edits() {
        let directory = tempfile::tempdir().unwrap();
        let b = directory.path().join("b.txt");
        let c = directory.path().join("c.txt");
        std::fs::write(&b, "beta").unwrap();
        std::fs::write(&c, "gamma").unwrap();
        let mut app = App::from_document(Document::from("alpha"), (100, 24));
        let a = app.editor.document().id();
        press(&mut app, "iX");
        key(&mut app, KeyCode::Esc);
        app.open_window_file(&b).unwrap();
        let b_id = app.editor.document().id();
        press(&mut app, "iY");
        key(&mut app, KeyCode::Esc);
        app.open_window_file(&c).unwrap();
        let c_id = app.editor.document().id();
        for (keys, expected) in [
            ("ga", b_id),
            ("ga", c_id),
            ("2gp", a),
            ("5gn", c_id),
            ("gm", b_id),
            ("gm", a),
            ("gm", b_id),
        ] {
            press(&mut app, keys);
            assert_eq!(app.editor.document().id(), expected, "{keys}");
            assert!(!app.error, "{}", app.message);
        }
        app.buffer_action(BufferAction::Next, usize::MAX).unwrap();
        assert_eq!(app.editor.document().id(), b_id); // usize::MAX is divisible by 3.
        app.execute("vsplit").unwrap();
        app.open_window_file(&c).unwrap();
        press(&mut app, "iZ");
        key(&mut app, KeyCode::Esc);
        app.execute("jump_view_left").unwrap();
        press(&mut app, "gm");
        assert_eq!(app.editor.document().id(), a); // Other pane's edit did not change gm.
        assert_eq!(app.editor.document().text(), "Xalpha");
        press(&mut app, "u");
        assert_eq!(app.editor.document().text(), "alpha");
        press(&mut app, "vgn"); // Buffer bindings also work in select mode.
        assert_eq!(app.editor.document().id(), b_id);
    }

    #[test]
    fn selected_files_open_in_the_current_pane_and_can_jump_to_hidden_scratch() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.txt");
        std::fs::write(&target, "target").unwrap();
        let mut app = App::from_document(Document::from(target.to_str().unwrap()), (80, 24));
        let scratch = app.editor.document().id();
        press(&mut app, "gf");
        assert_eq!(app.windows.panes.len(), 1);
        assert_eq!(app.editor.document().text(), "target");
        app.execute("jump_back").unwrap();
        app.take_lsp_update();
        assert_eq!(app.editor.document().id(), scratch);
        press(&mut app, "g");
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.editor.document().id(), scratch);
    }

    #[test]
    fn edits_map_a_hidden_saved_view_before_it_is_restored() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("other.txt");
        std::fs::write(&path, "other").unwrap();
        let mut app = App::from_document(Document::from("abc\ndef\n"), (100, 24));
        let scratch = app.editor.document().id();
        app.execute("vsplit").unwrap();
        press(&mut app, "jl");
        assert_eq!(app.editor.selections().primary().start(), CharOffset(5));
        app.open_window_file(&path).unwrap();
        app.execute("jump_view_left").unwrap();
        press(&mut app, "iXX");
        key(&mut app, KeyCode::Esc);
        app.execute("jump_view_right").unwrap();
        press(&mut app, "ga");
        assert_eq!(app.editor.document().id(), scratch);
        assert_eq!(app.editor.selections().primary().start(), CharOffset(7));
        assert_eq!(app.editor.document().text().char(7), 'e');
    }

    #[test]
    fn goto_file_validates_all_paths_before_switching_and_retains_every_opened_file() {
        let directory = tempfile::tempdir().unwrap();
        let origin = directory.path().join("origin.txt");
        std::fs::write(&origin, "one.txt two.txt").unwrap();
        std::fs::write(directory.path().join("one.txt"), "one").unwrap();
        let mut app = App::open(Some(&origin), (100, 24)).unwrap();
        let original = app.editor.document().id();
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
        press(&mut app, "gf");
        assert!(app.error);
        assert_eq!(app.editor.document().id(), original);
        assert!(app.windows.buffers.is_empty());
        std::fs::write(directory.path().join("two.txt"), "two").unwrap();
        press(&mut app, "gf");
        assert!(!app.error);
        assert_eq!(app.editor.document().text(), "two");
        assert_eq!(app.windows.panes.len(), 1);
        assert_eq!(app.windows.buffers.len(), 2);
        press(&mut app, "gp");
        assert_eq!(app.editor.document().text(), "one");
        press(&mut app, "gp");
        assert_eq!(app.editor.document().id(), original);
    }

    #[test]
    fn explicit_buffer_close_protects_edits_replaces_every_pane_and_releases_history() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("other.txt");
        std::fs::write(&path, "other").unwrap();
        let mut app = App::from_document(Document::from("scratch"), (100, 24));
        let scratch = app.editor.document().id();
        app.open_window_file(&path).unwrap();
        let other = app.editor.document().id();
        press(&mut app, "iX");
        key(&mut app, KeyCode::Esc);
        app.execute("vsplit").unwrap();
        assert!(app.execute("bc").is_err());
        assert_eq!(app.windows.panes.len(), 2);
        app.execute("bc!").unwrap();
        assert!(
            app.windows
                .panes
                .values()
                .all(|pane| pane.document == scratch)
        );
        assert!(app.windows.buffers.is_empty());
        assert!(!app.windows.accessed.contains_key(&other));
        assert!(
            app.windows
                .panes
                .values()
                .all(|pane| !pane.saved.contains_key(&other))
        );
        app.execute("bc").unwrap();
        let new = app.editor.document().id();
        assert_ne!(new, scratch);
        assert_eq!(app.editor.document().text(), "");
        assert!(app.windows.panes.values().all(|pane| pane.document == new));
        assert!(app.windows.buffers.is_empty());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "other");
        app.execute("bn").unwrap();
        app.execute("bp").unwrap();
        assert_eq!(app.editor.document().id(), new);
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
    fn highlighting_batches_include_visible_buffers_and_survive_superseded_work() {
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
        app.execute("only").unwrap();
        draw(&mut app);
        let batch = app.take_syntax_batch().unwrap();
        assert_eq!(batch.documents, vec![app.editor.document().id()]);
        assert_eq!(app.windows.buffers.len(), 1);
        assert_eq!(app.git_documents().len(), 1);
        assert_eq!(app.visible_file_probes().len(), 1);
        // Cached colors survive being hidden; no text parse is needed to return.
        app.open_window_file(&b).unwrap();
        draw(&mut app);
        assert!(app.take_syntax_batch().unwrap().jobs.is_empty());
    }
}
