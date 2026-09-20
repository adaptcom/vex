//! Destination loading and range conversion share the existing picker worker.

use super::{App, jumps::Jump};
use crate::{files::FileState, picker::search::OpenDocument};
use std::{io, path::PathBuf, sync::Arc};
use vex_core::{Document, DocumentId, Revision};
use vex_editor::{Mode, PreparedSelections, background::Cancellation};
use vex_lsp::Destination;

#[derive(Default)]
pub(super) struct State {
    pending: Option<Pending>,
    job: Option<Job>,
}

struct Pending {
    origin: Jump,
    window: u64,
    mode: Mode,
    cancellation: Cancellation,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

pub(crate) struct Job {
    destination: Destination,
    documents: Arc<[OpenDocument]>,
    pub cancellation: Cancellation,
}

pub(super) struct Loaded {
    pub path: PathBuf,
    pub document: DocumentId,
    pub revision: Revision,
    pub file: Option<(Document, FileState)>,
    pub selections: PreparedSelections,
}

pub(crate) struct Result {
    loaded: io::Result<Box<Loaded>>,
    cancellation: Cancellation,
}

impl Job {
    pub fn run(self) -> Option<Result> {
        let loaded = (|| {
            if self.cancellation.is_cancelled() {
                return Err(io::Error::other("navigation cancelled"));
            }
            let path = crate::files::resolve(&self.destination.path)?;
            let (snapshot, file) = if let Some(open) =
                self.documents.iter().find(|open| open.path == path)
            {
                (open.snapshot.clone(), None)
            } else {
                if !std::fs::metadata(&path)?.is_file() {
                    return Err(io::Error::other("location is not a regular file"));
                }
                let (document, files) =
                    FileState::load_with_cancel(Some(&path), || self.cancellation.is_cancelled())?;
                (document.snapshot(), Some((document, files)))
            };
            let selections = vex_lsp::destination_selection(
                &snapshot,
                self.destination.range,
                &self.cancellation,
            )
            .map_err(io::Error::other)?;
            Ok(Box::new(Loaded {
                path,
                document: snapshot.id(),
                revision: snapshot.revision(),
                file,
                selections,
            }))
        })();
        (!self.cancellation.is_cancelled()).then_some(Result {
            loaded,
            cancellation: self.cancellation,
        })
    }
}

impl App {
    pub(super) fn begin_location_navigation(&mut self, destination: Destination) {
        self.cancel_location_navigation();
        let cancellation = Cancellation::default();
        self.navigation.pending = Some(Pending {
            origin: self.current_jump(),
            window: self.focused_window_id(),
            mode: self.editor.mode(),
            cancellation: cancellation.clone(),
        });
        self.navigation.job = Some(Job {
            destination,
            documents: self.workspace_documents(),
            cancellation,
        });
        self.message = "opening location…".into();
    }

    pub(super) fn cancel_location_navigation(&mut self) {
        self.navigation.pending = None;
        self.navigation.job = None;
    }

    pub(super) fn location_navigation_waiting(&self) -> bool {
        self.navigation.pending.is_some()
    }

    pub(crate) fn take_location_navigation(&mut self) -> Option<Job> {
        self.navigation.job.take()
    }

    pub(crate) fn handle_location_navigation(&mut self, result: Result) -> bool {
        let Some(pending) = &self.navigation.pending else {
            return false;
        };
        if !pending.cancellation.same_request(&result.cancellation) {
            return false;
        }
        let pending = self.navigation.pending.take().unwrap();
        if pending.cancellation.is_cancelled()
            || pending.window != self.focused_window_id()
            || pending.origin.document != self.editor.document().id()
            || pending.origin.bookmark.revision() != self.editor.document().revision()
            || pending.origin.selections.as_ref() != self.editor.selections()
            || pending.mode != self.editor.mode()
        {
            return true;
        }
        match result
            .loaded
            .and_then(|loaded| self.open_loaded_location(*loaded))
        {
            Ok(()) => {
                self.push_jump(pending.origin.clone());
                self.keys.cancel(&mut self.editor);
                self.clear_message();
            }
            Err(error) => self.fail(error),
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use vex_core::CharOffset;
    use vex_lsp::{Position, Range};

    #[test]
    fn destination_jobs_reject_cancelled_stale_and_invalid_results_without_switching() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("other.rs");
        std::fs::write(&path, "🦀foo bar\n").unwrap();
        let target = Destination {
            path: path.clone(),
            range: Range {
                start: Position {
                    line: 0,
                    character: 2,
                },
                end: Position {
                    line: 0,
                    character: 5,
                },
            },
        };
        let mut app = App::from_document(Document::from("origin"), (80, 24));
        let origin = app.editor.document().id();
        app.begin_location_navigation(target.clone());
        let job = app.take_location_navigation().unwrap();
        app.handle(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(job.run().is_none());
        assert!(!app.input_waiting());
        app.begin_location_navigation(target.clone());
        let result = app.take_location_navigation().unwrap().run().unwrap();
        app.editor.execute("move_right", 1).unwrap();
        assert!(app.handle_location_navigation(result));
        assert_eq!(app.editor.document().id(), origin);
        assert!(!app.input_waiting());
        app.begin_location_navigation(Destination {
            range: Range {
                start: Position {
                    line: 99,
                    character: 0,
                },
                end: Position {
                    line: 99,
                    character: 1,
                },
            },
            ..target.clone()
        });
        let result = app.take_location_navigation().unwrap().run().unwrap();
        app.handle_location_navigation(result);
        assert_eq!(app.editor.document().id(), origin);
        assert!(app.message.contains("invalid destination"));
        app.begin_location_navigation(target.clone());
        let result = app.take_location_navigation().unwrap().run().unwrap();
        app.handle_location_navigation(result);
        assert_eq!(app.editor.selections().primary().head, CharOffset(1));
        assert_eq!(app.editor.selections().primary().anchor, CharOffset(4));
        // Hidden, unsaved buffers are the source of truth, even after disk removal.
        app.editor.execute("insert_mode", 1).unwrap();
        app.editor.insert_text("X").unwrap();
        app.editor.execute("normal_mode", 1).unwrap();
        app.execute("jump_backward").unwrap();
        std::fs::remove_file(&path).unwrap();
        app.begin_location_navigation(target.clone());
        let result = app.take_location_navigation().unwrap().run().unwrap();
        app.handle_location_navigation(result);
        assert_eq!(app.editor.document().text(), "🦀Xfoo bar\n");
        // A changed destination is rejected before altering focus or history.
        app.execute("jump_backward").unwrap();
        app.begin_location_navigation(target);
        let result = app.take_location_navigation().unwrap().run().unwrap();
        let changed = result.loaded.as_ref().unwrap().document;
        app.with_file_buffer_mut(changed, |editor, _, _| {
            editor.execute("insert_mode", 1).unwrap();
            editor.insert_text("Y").unwrap();
        });
        app.handle_location_navigation(result);
        assert_eq!(app.editor.document().id(), origin);
        assert!(app.message.contains("destination changed"));
    }
}
