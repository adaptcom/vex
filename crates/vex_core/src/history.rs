use std::{collections::VecDeque, sync::Arc};

use crate::mapping::{PositionMap, PositionMaps};
use crate::{Rope, SelectionSet, document::ChangeExtent};

#[derive(Clone, Debug)]
pub(crate) struct State {
    pub text: Rope,
    pub selections: SelectionSet,
}

#[derive(Debug)]
struct Entry {
    before: State,
    after: State,
    change: ChangeExtent,
    maps: PositionMaps,
}

/// Rope clones share unchanged storage, including across undo branches.
#[derive(Debug)]
pub(crate) struct History {
    done: VecDeque<Entry>,
    undone: Vec<Entry>,
    limit: usize,
    open_group: bool,
}

impl Default for History {
    fn default() -> Self {
        Self {
            done: VecDeque::new(),
            undone: Vec::new(),
            limit: 1_000,
            open_group: false,
        }
    }
}

impl History {
    pub fn record(
        &mut self,
        before: State,
        after: State,
        change: ChangeExtent,
        map: Arc<PositionMap>,
        grouped: bool,
    ) {
        self.undone.clear();
        if grouped && self.open_group {
            let entry = self.done.back_mut().expect("open group has an entry");
            // Keep only the group's endpoints. A prefix/suffix survives the
            // whole group only if it survives both the old group and this edit.
            let suffix = (entry.before.text.len_chars() - entry.change.old_end.0)
                .min(before.text.len_chars() - change.old_end.0);
            entry.change = ChangeExtent {
                start: entry.change.start.min(change.start),
                old_end: crate::CharOffset(entry.before.text.len_chars() - suffix),
                new_end: crate::CharOffset(after.text.len_chars() - suffix),
            };
            entry.after = after;
            entry.maps.push(map);
        } else if self.limit != 0 {
            self.done.push_back(Entry {
                before,
                after,
                change,
                maps: PositionMaps::Single(map),
            });
            while self.done.len() > self.limit {
                self.done.pop_front();
            }
        }
        self.open_group = grouped && self.limit != 0;
    }

    pub fn finish_group(&mut self) {
        self.open_group = false;
    }

    pub fn undo(&mut self) -> Option<(State, ChangeExtent, PositionMaps)> {
        let entry = self.done.pop_back()?;
        let state = (
            entry.before.clone(),
            entry.change.reversed(),
            entry.maps.clone(),
        );
        self.undone.push(entry);
        Some(state)
    }

    pub fn redo(&mut self) -> Option<(State, ChangeExtent, PositionMaps)> {
        let entry = self.undone.pop()?;
        let state = (entry.after.clone(), entry.change, entry.maps.clone());
        self.done.push_back(entry);
        Some(state)
    }

    pub fn undo_depth(&self) -> usize {
        self.done.len()
    }

    pub fn modification(&self) -> Option<crate::Modification> {
        let entry = self.done.back()?;
        Some(crate::Modification {
            original_len: entry.before.text.len_chars(),
            primary: entry.before.selections.primary(),
            maps: entry.maps.clone(),
        })
    }

    pub fn redo_depth(&self) -> usize {
        self.undone.len()
    }

    pub fn set_limit(&mut self, limit: usize) {
        self.finish_group();
        self.limit = limit;
        self.undone.clear();
        while self.done.len() > limit {
            self.done.pop_front();
        }
    }
}
