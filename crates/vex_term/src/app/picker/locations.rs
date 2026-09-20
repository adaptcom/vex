//! Location lists use the shared floating picker and preview worker.

use super::*;
use vex_core::{DocumentId, Revision};
use vex_lsp::{Locations, Navigation};

pub(super) struct Source {
    pub catalog: Arc<crate::picker::locations::Catalog>,
    pub document: DocumentId,
    pub revision: Revision,
}

impl App {
    pub(in crate::app) fn receive_locations(&mut self, kind: Navigation, mut locations: Locations) {
        if locations.items.is_empty() {
            self.message = if locations.skipped > 0 {
                format!(
                    "no usable {} ({} unsupported or invalid locations)",
                    kind.title().to_lowercase(),
                    locations.skipped
                )
            } else {
                format!("no {} found", kind.title().to_lowercase())
            };
            return;
        }
        if locations.items.len() == 1 && !locations.limited && locations.skipped == 0 {
            self.begin_location_navigation(locations.items.pop().unwrap());
            return;
        }
        self.close_picker();
        self.dismiss_language_help();
        self.keys.cancel(&mut self.editor);
        self.prompt = None;
        self.clear_message();
        let mut view = Picker::new(kind.title().into());
        view.noun = "locations";
        self.picker.next_session += 1;
        self.picker.active = Some(Active {
            view,
            session: self.picker.next_session,
            revision: 0,
            source: super::Source::Locations(Source {
                catalog: Arc::new(crate::picker::locations::Catalog::new(
                    locations,
                    std::env::current_dir().unwrap_or_default(),
                )),
                document: self.editor.document().id(),
                revision: self.editor.document().revision(),
            }),
            cancellation: Cancellation::default(),
            preview_cancel: Cancellation::default(),
            preview_request: 0,
            preview_target: None,
            accept_pending: false,
        });
        self.submit_picker_query();
    }

    pub(in crate::app) fn invalidate_location_picker(&mut self) {
        let stale = self.picker.active.as_ref().is_some_and(|active| {
            let super::Source::Locations(source) = &active.source else {
                return false;
            };
            source.document != self.editor.document().id()
                || source.revision != self.editor.document().revision()
        });
        if stale {
            self.close_picker();
        }
    }

    pub(crate) fn take_location_job(&mut self) -> Option<crate::picker::locations::Job> {
        self.picker.location_job.take()
    }

    pub(crate) fn handle_location_result(
        &mut self,
        result: crate::picker::locations::Result,
    ) -> bool {
        self.invalidate_location_picker();
        let Some(active) = &mut self.picker.active else {
            return false;
        };
        if !matches!(active.source, super::Source::Locations(_))
            || result.session != active.session
            || result.revision != active.revision
            || active.cancellation.is_cancelled()
        {
            return false;
        }
        active.view.replace(
            result
                .items
                .into_iter()
                .map(|item| Item {
                    entry: Arc::new(Entry {
                        label: item.entry.label.clone(),
                        value: Target::Location(item.entry.value.clone()),
                    }),
                    matched: item.matched,
                })
                .collect(),
        );
        active.view.matched = result.matched;
        active.view.total = result.total;
        active.view.pending = false;
        active.view.notice = result.notice;
        let accept = active.accept_pending;
        active.accept_pending = false;
        if accept {
            self.accept_picker();
        } else {
            self.request_picker_preview();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use vex_core::{CharOffset, Selection};
    use vex_lsp::{Answer, Destination, Event as LspEvent, Position, Range};

    fn press(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.handle(Event::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )));
        }
    }

    #[test]
    fn multiple_destinations_filter_preview_accept_and_reopen_without_blocking_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "fn alpha() {}\nfn beta() {}\n").unwrap();
        let mut app = App::open(Some(&path), (140, 24)).unwrap();
        app.enable_lsp();
        app.take_lsp_update();
        press(&mut app, "gr");
        let update = app.take_lsp_update().unwrap();
        assert!(app.input_waiting());
        let request = update.request.unwrap();
        assert!(matches!(
            request.kind,
            vex_lsp::RequestKind::Navigation(Navigation::References)
        ));
        let document = update.document.unwrap();
        let locations = Locations {
            items: (0..2)
                .map(|line| Destination {
                    path: document.path.clone(),
                    range: Range {
                        start: Position { line, character: 3 },
                        end: Position {
                            line,
                            character: if line == 0 { 8 } else { 7 },
                        },
                    },
                })
                .collect(),
            ..Default::default()
        };
        assert!(app.handle_lsp_event(LspEvent::Answer {
            epoch: document.epoch,
            revision: document.snapshot.revision(),
            id: request.id,
            result: Ok(Answer::Locations(Navigation::References, locations))
        }));
        assert!(!app.input_waiting());
        let old = app.take_location_job().unwrap().run().unwrap();
        press(&mut app, ":2:");
        assert!(!app.handle_location_result(old));
        let result = app.take_location_job().unwrap().run().unwrap();
        assert!(app.handle_location_result(result));
        let preview = app.take_preview_job().unwrap().run().unwrap();
        assert!(preview.preview.text.contains("beta"));
        assert!(!preview.preview.highlights.is_empty());
        assert!(app.handle_preview_result(preview));
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert!(app.picker.active.is_none());
        assert!(app.input_waiting());
        let result = app.take_location_navigation().unwrap().run().unwrap();
        assert!(app.handle_location_navigation(result));
        assert!(!app.input_waiting());
        assert_eq!(
            app.editor.selections().primary(),
            Selection::new(CharOffset(21), CharOffset(17))
        );
        app.execute("jump_backward").unwrap();
        assert_eq!(app.editor.selections().primary().start(), CharOffset(0));
        press(&mut app, " '");
        assert_eq!(app.picker.active.as_ref().unwrap().view.query.text(), ":2:");
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert!(app.input_waiting());
        let result = app.take_location_job().unwrap().run().unwrap();
        app.handle_location_result(result);
        assert!(app.input_waiting());
        let result = app.take_location_navigation().unwrap().run().unwrap();
        app.handle_location_navigation(result);
        assert_eq!(app.editor.selections().primary().head, CharOffset(17));
    }
}
