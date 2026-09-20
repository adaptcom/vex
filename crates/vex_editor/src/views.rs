//! Independent selections over one document, undo history, and derived caches.

use crate::{Editor, Mode};
use std::collections::BTreeMap;
use vex_core::{Revision, SelectionSet};

/// A view identity scoped to its editor/document.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ViewId(u64);

#[derive(Clone, Debug)]
struct View {
    selections: SelectionSet,
    mode: Mode,
    preferred_columns: Option<Vec<usize>>,
}

#[derive(Debug)]
pub(super) struct Views {
    active: ViewId,
    next: u64,
    inactive: BTreeMap<ViewId, View>,
    revision: Revision,
}

impl Views {
    pub fn new(revision: Revision) -> Self {
        Self {
            active: ViewId(0),
            next: 1,
            inactive: BTreeMap::new(),
            revision,
        }
    }
}

impl Editor {
    /// Apply a revision-checked external reload as one undo step. Preserve view
    /// modes and map cursors through unchanged text; within replaced text, keep
    /// their relative scalar offset, clamped to the replacement. All endpoints
    /// are normalized back to grapheme boundaries before rendering.
    pub fn apply_external_change(
        &mut self,
        transaction: vex_core::Transaction,
    ) -> Result<(), crate::Error> {
        use vex_core::{Affinity, CharOffset, Selection};
        let replacements: Vec<_> = transaction
            .edits()
            .map(|edit| (edit.range(), edit.text().chars().count()))
            .collect();
        let map = |selections: &SelectionSet| -> Result<SelectionSet, vex_core::Error> {
            let position = |position: CharOffset| -> Result<CharOffset, vex_core::Error> {
                for (range, len) in &replacements {
                    if range.contains(&position) {
                        let start = transaction.map_position(range.start, Affinity::Before)?;
                        return Ok(CharOffset(start.0 + (position.0 - range.start.0).min(*len)));
                    }
                }
                transaction.map_position(position, Affinity::After)
            };
            SelectionSet::new(
                selections
                    .ranges()
                    .iter()
                    .map(|selection| {
                        Ok(Selection::new(
                            position(selection.anchor)?,
                            position(selection.head)?,
                        ))
                    })
                    .collect::<Result<Vec<_>, vex_core::Error>>()?,
                selections.primary_index(),
            )
        };
        let selections = map(&self.selections)?;
        let inactive = self
            .views
            .inactive
            .iter()
            .map(|(&id, view)| Ok((id, map(&view.selections)?)))
            .collect::<Result<Vec<_>, vex_core::Error>>()?;
        self.apply(transaction.with_selections(selections)?, false)?;
        self.selections = self.normalized(self.selections.clone(), self.mode)?;
        for (id, selections) in inactive {
            let selections = self.normalized(selections, self.views.inactive[&id].mode)?;
            self.views.inactive.get_mut(&id).unwrap().selections = selections;
        }
        self.preferred_columns = None;
        self.newline = crate::line_ending(self.document.text());
        self.language_action = None;
        self.application_action = None;
        Ok(())
    }

    pub fn active_view(&self) -> ViewId {
        self.views.active
    }

    /// Duplicate cursor state without copying text or undo history.
    pub fn duplicate_view(&mut self) -> ViewId {
        let id = ViewId(self.views.next);
        self.views.next = self
            .views
            .next
            .checked_add(1)
            .expect("view identity exhausted");
        self.views.inactive.insert(
            id,
            View {
                selections: self.selections.clone(),
                mode: self.mode,
                preferred_columns: self.preferred_columns.clone(),
            },
        );
        id
    }

    /// Focus a view and cancel work tied to the previous selection.
    pub fn focus_view(&mut self, id: ViewId) -> bool {
        if id != self.views.active && !self.views.inactive.contains_key(&id) {
            return false;
        }
        self.finish_undo_group();
        self.language_action = None;
        self.swap_view(id);
        true
    }

    /// Read a view for rendering, without changing focus or closing undo groups.
    pub fn with_view<T>(&mut self, id: ViewId, read: impl FnOnce(&Editor) -> T) -> Option<T> {
        if id != self.views.active && !self.views.inactive.contains_key(&id) {
            return None;
        }
        let previous = self.views.active;
        self.swap_view(id);
        let result = read(self);
        self.swap_view(previous);
        Some(result)
    }

    /// Remove a view. Returns false when this is the last view; the owner must
    /// then either retain the editor or discard the document itself.
    pub fn remove_view(&mut self, id: ViewId) -> bool {
        if id == self.views.active {
            let Some(next) = self.views.inactive.keys().next().copied() else {
                return false;
            };
            self.focus_view(next);
        }
        self.views.inactive.remove(&id);
        true
    }

    pub fn retain_active_view(&mut self) {
        self.views.inactive.clear();
    }

    fn swap_view(&mut self, id: ViewId) {
        if id == self.views.active {
            return;
        }
        let mut other = self.views.inactive.remove(&id).expect("existing view");
        std::mem::swap(&mut self.selections, &mut other.selections);
        std::mem::swap(&mut self.mode, &mut other.mode);
        std::mem::swap(&mut self.preferred_columns, &mut other.preferred_columns);
        self.views.inactive.insert(self.views.active, other);
        self.views.active = id;
    }

    pub(super) fn synchronize_views(&mut self) {
        if self.views.revision == self.document.revision() {
            return;
        }
        // Temporarily take the map so normalization can use shared layout data.
        let mut inactive = std::mem::take(&mut self.views.inactive);
        for view in inactive.values_mut() {
            let selections = self
                .document
                .map_other_selections(&view.selections)
                .expect("view positions map to the current document");
            view.selections = self
                .normalized(selections, view.mode)
                .expect("valid mapped view");
            view.preferred_columns = None;
        }
        self.views.inactive = inactive;
        self.views.revision = self.document.revision();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use vex_core::{CharOffset, Document, Selection};

    #[test]
    fn external_replacements_preserve_view_modes_cursors_and_reject_stale_work() {
        let mut editor = Editor::new(Document::from("abcd e\u{301}nd\n"));
        editor.execute("move_right", 2).unwrap();
        let first = editor.active_view();
        let second = editor.duplicate_view();
        editor.focus_view(second);
        editor.execute("insert_mode", 1).unwrap();
        let transaction = editor
            .document()
            .transaction([vex_core::Edit::new(CharOffset(0)..CharOffset(4), "XY")])
            .unwrap();
        let stale = transaction.clone();
        editor.apply_external_change(transaction).unwrap();
        assert_eq!(editor.mode(), Mode::Insert);
        assert_eq!(
            editor.selections().primary(),
            Selection::cursor(CharOffset(2))
        );
        editor.focus_view(first);
        assert_eq!(editor.mode(), Mode::Normal);
        assert_eq!(editor.selections().primary().start(), CharOffset(2));
        let text = editor.document().text().clone();
        assert!(editor.apply_external_change(stale).is_err());
        assert!(text.is_instance(editor.document().text()));
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document().text(), "abcd e\u{301}nd\n");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(96))]
        #[test]
        fn switching_views_between_edits_and_history_keeps_every_view_valid(
            actions in prop::collection::vec((0usize..4, 0u8..10), 0..120),
        ) {
            let mut editor = Editor::new(Document::from("e\u{301} 👩\u{200d}💻\r\n日本語\nlast"));
            let views = [editor.active_view(), editor.duplicate_view(), editor.duplicate_view(), editor.duplicate_view()];
            for (view, action) in actions {
                editor.focus_view(views[view]);
                match action {
                    0..=2 => {
                        editor.execute("insert_mode", 1).unwrap();
                        editor.insert_text(["\u{301}", "🦀", "\r\n"][action as usize]).unwrap();
                        editor.insert_text("x").unwrap();
                    }
                    3 => { editor.execute("insert_mode", 1).unwrap(); editor.execute("delete_backward", 1).unwrap(); }
                    4 => { editor.execute("undo", 1).unwrap(); }
                    5 => { editor.execute("redo", 1).unwrap(); }
                    6 => { editor.execute("move_right", 2).unwrap(); }
                    7 => { editor.execute("move_left", 2).unwrap(); }
                    8 => { editor.execute("select_mode", 1).unwrap(); }
                    _ => { editor.execute("normal_mode", 1).unwrap(); }
                }
                for &view in &views {
                    editor.with_view(view, |editor| {
                        let text = editor.document().text();
                        for selection in editor.selections().ranges() {
                            assert!(selection.end().0 <= text.len_chars());
                            assert!(vex_core::grapheme::is_boundary(text, selection.anchor).unwrap());
                            assert!(vex_core::grapheme::is_boundary(text, selection.head).unwrap());
                            if editor.mode() == Mode::Insert { assert!(selection.is_empty()); }
                        }
                    }).unwrap();
                }
            }
        }
    }

    #[test]
    fn views_share_text_and_undo_but_keep_independent_cursors() {
        let mut editor = Editor::new(Document::from("one two three"));
        let first = editor.active_view();
        let second = editor.duplicate_view();
        editor.execute("move_right", 8).unwrap();
        editor.focus_view(second);
        assert_eq!(editor.selections().primary().start(), CharOffset(0));
        editor.execute("insert_mode", 1).unwrap();
        editor.insert_text("XX").unwrap();
        editor.insert_text("Y").unwrap();
        editor.execute("normal_mode", 1).unwrap();
        editor.focus_view(first);
        assert_eq!(editor.selections().primary().start(), CharOffset(11));
        editor.focus_view(second);
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document().text(), "one two three");
        editor.focus_view(first);
        assert_eq!(editor.selections().primary().start(), CharOffset(8));
        editor.focus_view(second);
        editor.execute("redo", 1).unwrap();
        editor.focus_view(first);
        assert_eq!(editor.selections().primary().start(), CharOffset(11));
    }

    #[test]
    fn inactive_views_map_disjoint_edits_and_stay_on_grapheme_boundaries() {
        let mut editor = Editor::new(Document::from("abc def ghi"));
        editor
            .set_selections(SelectionSet::single(Selection::new(
                CharOffset(4),
                CharOffset(5),
            )))
            .unwrap();
        let other = editor.duplicate_view();
        editor
            .set_selections(
                SelectionSet::new(
                    vec![
                        Selection::cursor(CharOffset(1)),
                        Selection::cursor(CharOffset(9)),
                    ],
                    0,
                )
                .unwrap(),
            )
            .unwrap();
        editor.execute("insert_mode", 1).unwrap();
        editor.insert_text("\u{301}").unwrap();
        editor.focus_view(other);
        assert_eq!(editor.selections().primary().start(), CharOffset(5));
        assert!(editor.remove_view(other));
        assert!(!editor.remove_view(editor.active_view()));
    }
}
