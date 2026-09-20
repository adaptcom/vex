//! Snapshot-validated selections prepared outside the input thread.

use crate::{Editor, Error, Mode, background::Cancellation};
use vex_core::{DocumentId, Revision, Rope, Selection, SelectionSet, Snapshot, grapheme, motion};

/// Normalized selections tied to an immutable document revision. The private
/// fields prevent callers from bypassing bounds and grapheme validation.
#[derive(Debug)]
pub struct PreparedSelections {
    document: DocumentId,
    revision: Revision,
    insert: bool,
    selections: SelectionSet,
}

impl PreparedSelections {
    /// Validate and normalize on a worker, checking cancellation between ranges.
    pub fn new(
        snapshot: &Snapshot,
        selections: SelectionSet,
        mode: Mode,
        cancellation: &Cancellation,
    ) -> Result<Self, Error> {
        Ok(Self {
            document: snapshot.id(),
            revision: snapshot.revision(),
            insert: mode == Mode::Insert,
            selections: normalize(snapshot.text(), selections, mode, || {
                cancellation.is_cancelled()
            })?,
        })
    }
}

impl Editor {
    /// Install worker-prepared selections without repeating text scans. Returns
    /// false if the document, revision, or insertion mode changed meanwhile.
    pub fn apply_prepared_selections(&mut self, prepared: PreparedSelections) -> bool {
        if prepared.document != self.document.id()
            || prepared.revision != self.document.revision()
            || prepared.insert != (self.mode == Mode::Insert)
        {
            return false;
        }
        self.cancel_repeat();
        self.finish_undo_group();
        self.selections = prepared.selections;
        self.preferred_columns = None;
        true
    }
}

pub(crate) fn normalize(
    text: &Rope,
    selections: SelectionSet,
    mode: Mode,
    cancelled: impl Fn() -> bool,
) -> Result<SelectionSet, Error> {
    selections.validate(text.len_chars())?;
    let ranges = selections
        .ranges()
        .iter()
        .map(|&selection| {
            if cancelled() {
                return Err(Error::Cancelled);
            }
            Ok(if mode == Mode::Insert {
                Selection::cursor(grapheme::ceil(text, selection.head)?)
            } else if selection.is_empty() {
                motion::block(text, selection.head)?
            } else {
                let start = grapheme::floor(text, selection.start())?;
                let end = grapheme::ceil(text, selection.end())?;
                if selection.is_backward() {
                    Selection::new(end, start)
                } else {
                    Selection::new(start, end)
                }
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    if cancelled() {
        return Err(Error::Cancelled);
    }
    Ok(SelectionSet::new(ranges, selections.primary_index())?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vex_core::{CharOffset, Document};

    #[test]
    #[ignore = "manual release-mode selection delivery benchmark"]
    fn benchmark_prepared_selection_delivery() {
        use std::{
            hint::black_box,
            time::{Duration, Instant},
        };
        for mib in [1usize, 100] {
            let mut editor = Editor::new(Document::from("name ".repeat((mib << 20) / 5).as_str()));
            for count in [1usize, 1000, 65_536] {
                let selections = SelectionSet::new(
                    (0..count)
                        .map(|index| {
                            Selection::new(CharOffset(index * 5), CharOffset(index * 5 + 4))
                        })
                        .collect(),
                    0,
                )
                .unwrap();
                let snapshot = editor.document().snapshot();
                let mut time = Duration::ZERO;
                for _ in 0..50 {
                    let prepared = PreparedSelections::new(
                        &snapshot,
                        selections.clone(),
                        Mode::Normal,
                        &Cancellation::default(),
                    )
                    .unwrap();
                    let now = Instant::now();
                    assert!(black_box(&mut editor).apply_prepared_selections(black_box(prepared)));
                    time += now.elapsed();
                }
                println!(
                    "{mib} MiB, {count} ranges, UI apply mean {:?} (preparation excluded)",
                    time / 50
                );
            }
        }
    }

    #[test]
    fn prepared_ranges_keep_graphemes_and_reject_stale_documents_and_modes() {
        let mut editor = Editor::new(Document::from("e\u{301}🦀cat"));
        let prepare = |editor: &Editor| {
            PreparedSelections::new(
                &editor.document().snapshot(),
                SelectionSet::single(Selection::new(CharOffset(3), CharOffset(1))),
                Mode::Normal,
                &Cancellation::default(),
            )
            .unwrap()
        };
        let valid = prepare(&editor);
        editor.execute("select_mode", 1).unwrap();
        assert!(editor.apply_prepared_selections(valid));
        assert_eq!(
            editor.selections().primary(),
            Selection::new(CharOffset(3), CharOffset(0))
        );
        let wrong_mode = prepare(&editor);
        editor.execute("insert_mode", 1).unwrap();
        assert!(!editor.apply_prepared_selections(wrong_mode));
        let stale = prepare(&editor);
        editor.insert_text("x").unwrap();
        editor.execute("normal_mode", 1).unwrap();
        assert!(!editor.apply_prepared_selections(stale));
        let other = prepare(&editor);
        assert!(!Editor::new(Document::from("same size")).apply_prepared_selections(other));
        let cancelled = Cancellation::default();
        cancelled.cancel();
        assert!(matches!(
            PreparedSelections::new(
                &editor.document().snapshot(),
                SelectionSet::default(),
                Mode::Normal,
                &cancelled
            ),
            Err(Error::Cancelled)
        ));
    }
}
