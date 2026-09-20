//! Navigation for derived, read-only views. Rows have stable identities, so a
//! refresh can move content without moving the user's attention to another item.

use crate::screen::Style;

pub(crate) struct Row<K> {
    pub id: K,
    pub text: String,
    pub style: Style,
}

pub(crate) struct Sections<K> {
    pub rows: Vec<Row<K>>,
    pub selected: usize,
    pub top: usize,
}

impl<K> Default for Sections<K> {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            selected: 0,
            top: 0,
        }
    }
}

impl<K: Clone + PartialEq> Sections<K> {
    /// Keep the selected identity and top row through refresh. If the selection
    /// disappeared, prefer a surviving ancestor before the nearest row.
    pub fn replace(&mut self, rows: Vec<Row<K>>, parent: impl Fn(&K) -> Option<K>) {
        let mut target = self.rows.get(self.selected).map(|row| row.id.clone());
        let offset = self.selected.saturating_sub(self.top);
        let old_top = self.rows.get(self.top).map(|row| &row.id);
        let new_top = old_top.and_then(|id| rows.iter().position(|row| &row.id == id));
        while let Some(id) = target {
            if let Some(index) = rows.iter().position(|row| row.id == id) {
                self.selected = index;
                break;
            }
            target = parent(&id);
        }
        self.selected = self.selected.min(rows.len().saturating_sub(1));
        self.top = new_top
            .unwrap_or_else(|| self.selected.saturating_sub(offset))
            .min(self.selected);
        self.rows = rows;
    }

    /// Move by visible rows without touching the underlying document.
    pub fn move_by(&mut self, down: bool, count: usize) {
        self.selected = if down {
            self.selected
                .saturating_add(count)
                .min(self.rows.len().saturating_sub(1))
        } else {
            self.selected.saturating_sub(count)
        };
    }

    pub fn ensure_visible(&mut self, height: usize) {
        if self.selected < self.top {
            self.top = self.selected;
        }
        if self.selected >= self.top.saturating_add(height.max(1)) {
            self.top = self.selected.saturating_sub(height.saturating_sub(1));
        }
        self.top = self.top.min(self.rows.len().saturating_sub(height));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rows(ids: &[u8]) -> Vec<Row<u8>> {
        ids.iter()
            .map(|&id| Row {
                id,
                text: id.to_string(),
                style: Style::Text,
            })
            .collect()
    }
    #[test]
    fn refresh_preserves_selection_and_scroll_and_falls_back_to_parent() {
        let mut list = Sections::default();
        list.replace(rows(&[1, 2, 3, 4, 5]), |_| None);
        list.selected = 3;
        list.top = 2;
        list.replace(rows(&[0, 1, 2, 3, 4, 5]), |_| None);
        assert_eq!((list.selected, list.top), (4, 3));
        list.replace(rows(&[0, 1, 2, 3, 5]), |id| (*id == 4).then_some(3));
        assert_eq!(list.rows[list.selected].id, 3);
        list.ensure_visible(1);
        assert_eq!(list.top, list.selected);
        list.replace(Vec::new(), |_| None);
        list.move_by(true, usize::MAX);
        list.ensure_visible(0);
        assert_eq!((list.selected, list.top), (0, 0));
    }
}
