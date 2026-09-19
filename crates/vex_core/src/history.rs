use std::collections::VecDeque;

use crate::{Rope, SelectionSet};

#[derive(Clone, Debug)]
pub(crate) struct State {
    pub text: Rope,
    pub selections: SelectionSet,
}

#[derive(Debug)]
struct Entry {
    before: State,
    after: State,
}

/// Rope clones share unchanged storage, including across undo branches.
#[derive(Debug)]
pub(crate) struct History {
    done: VecDeque<Entry>,
    undone: Vec<Entry>,
    limit: usize,
}

impl Default for History {
    fn default() -> Self {
        Self {
            done: VecDeque::new(),
            undone: Vec::new(),
            limit: 1_000,
        }
    }
}

impl History {
    pub fn record(&mut self, before: State, after: State) {
        self.undone.clear();
        if self.limit != 0 {
            self.done.push_back(Entry { before, after });
            while self.done.len() > self.limit {
                self.done.pop_front();
            }
        }
    }

    pub fn undo(&mut self) -> Option<State> {
        let entry = self.done.pop_back()?;
        let state = entry.before.clone();
        self.undone.push(entry);
        Some(state)
    }

    pub fn redo(&mut self) -> Option<State> {
        let entry = self.undone.pop()?;
        let state = entry.after.clone();
        self.done.push_back(entry);
        Some(state)
    }

    pub fn undo_depth(&self) -> usize {
        self.done.len()
    }

    pub fn redo_depth(&self) -> usize {
        self.undone.len()
    }

    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit;
        self.undone.clear();
        while self.done.len() > limit {
            self.done.pop_front();
        }
    }
}
