//! Bounded binary split layout, independent of buffers and drawing.

use std::io;

pub(super) type WindowId = u64;
pub(super) const MAX_WINDOWS: usize = 16;
const MIN_WIDTH: u16 = 12;
const MIN_HEIGHT: u16 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Axis {
    Vertical,
    Horizontal,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Direction {
    Left,
    Down,
    Up,
    Right,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}
impl Rect {
    pub fn size(self) -> (u16, u16) {
        (self.width, self.height)
    }

    pub fn contains(self, x: u16, y: u16) -> bool {
        x >= self.x
            && y >= self.y
            && x < self.x.saturating_add(self.width)
            && y < self.y.saturating_add(self.height)
    }
}

/// An exact fraction preserves dragged proportions. Equal sizing balances runs
/// of panes on the same axis, including after terminal resizing. Clamping does
/// not overwrite either preference.
#[derive(Clone, Copy, Debug)]
enum Ratio {
    Fraction { first: u16, total: u16 },
    Equal,
}

impl Default for Ratio {
    fn default() -> Self {
        Self::Fraction { first: 1, total: 2 }
    }
}

#[derive(Clone, Debug)]
enum Node {
    Leaf(WindowId),
    Split(Axis, Ratio, Box<Node>, Box<Node>),
}

impl Node {
    /// A perpendicular split shares this axis's space and counts as one pane.
    fn parallel_panes(&self, axis: Axis) -> u16 {
        match self {
            Self::Split(own_axis, _, a, b) if *own_axis == axis => {
                a.parallel_panes(axis) + b.parallel_panes(axis)
            }
            _ => 1,
        }
    }

    fn equalize(&mut self) {
        if let Self::Split(_, ratio, a, b) = self {
            *ratio = Ratio::Equal;
            a.equalize();
            b.equalize();
        }
    }

    fn minimum(&self) -> (u16, u16) {
        match self {
            Self::Leaf(_) => (MIN_WIDTH, MIN_HEIGHT),
            Self::Split(axis, _, a, b) => {
                let (aw, ah) = a.minimum();
                let (bw, bh) = b.minimum();
                match axis {
                    Axis::Vertical => (aw + 1 + bw, ah.max(bh)),
                    Axis::Horizontal => (aw.max(bw), ah + bh),
                }
            }
        }
    }

    fn parts(&self, rect: Rect) -> Option<(Rect, Rect, Rect)> {
        let Self::Split(axis, ratio, first, second) = self else {
            return None;
        };
        let (aw, ah) = first.minimum();
        let (bw, bh) = second.minimum();
        let (available, min_a, min_b) = match axis {
            Axis::Vertical => (rect.width.saturating_sub(1), aw, bw),
            Axis::Horizontal => (rect.height, ah, bh),
        };
        let preferred = match *ratio {
            Ratio::Fraction { first, total } => {
                (u32::from(available) * u32::from(first) / u32::from(total)) as u16
            }
            Ratio::Equal => {
                let a = u32::from(first.parallel_panes(*axis));
                let b = u32::from(second.parallel_panes(*axis));
                let gap = u32::from(*axis == Axis::Vertical);
                // Share pane space plus one separator per pane, then remove
                // the trailing separator from this subtree's extent.
                (((u32::from(available) + 2 * gap) * a / (a + b)).saturating_sub(gap) as u16)
                    .min(available)
            }
        };
        let extent = if available >= min_a + min_b {
            preferred.clamp(min_a, available - min_b)
        } else {
            preferred
        };
        let (mut a, mut b, mut divider) = (rect, rect, rect);
        match axis {
            Axis::Vertical => {
                a.width = extent;
                divider.x += extent;
                divider.width = u16::from(rect.width > 0);
                b.x = divider.x + divider.width;
                b.width = available - extent;
            }
            Axis::Horizontal => {
                a.height = extent;
                b.y += extent;
                b.height = available - extent;
                // The upper pane's status line is also the resize handle.
                divider.y += extent.saturating_sub(1);
                divider.height = u16::from(extent > 0);
            }
        }
        Some((a, b, divider))
    }

    fn divide(&self, rect: Rect, leaves: &mut Vec<(WindowId, Rect)>, dividers: &mut Vec<Rect>) {
        match self {
            Self::Leaf(id) => leaves.push((*id, rect)),
            Self::Split(axis, _, first, second) => {
                let (a, b, divider) = self.parts(rect).unwrap();
                if *axis == Axis::Vertical {
                    dividers.push(divider);
                }
                first.divide(a, leaves, dividers);
                second.divide(b, leaves, dividers);
            }
        }
    }

    fn split(&mut self, target: WindowId, id: WindowId, axis: Axis) -> bool {
        match self {
            Self::Leaf(old) if *old == target => {
                *self = Self::Split(
                    axis,
                    Ratio::default(),
                    Box::new(Self::Leaf(target)),
                    Box::new(Self::Leaf(id)),
                );
                true
            }
            Self::Leaf(_) => false,
            Self::Split(_, _, a, b) => a.split(target, id, axis) || b.split(target, id, axis),
        }
    }

    fn remove(self, target: WindowId) -> Option<Self> {
        match self {
            Self::Leaf(id) => (id != target).then_some(Self::Leaf(id)),
            Self::Split(axis, ratio, a, b) => match (a.remove(target), b.remove(target)) {
                (Some(a), Some(b)) => Some(Self::Split(axis, ratio, Box::new(a), Box::new(b))),
                (a, b) => a.or(b),
            },
        }
    }

    fn swap(&mut self, a: WindowId, b: WindowId) {
        match self {
            Self::Leaf(id) => {
                if *id == a {
                    *id = b;
                } else if *id == b {
                    *id = a;
                }
            }
            Self::Split(_, _, first, second) => {
                first.swap(a, b);
                second.swap(a, b);
            }
        }
    }

    fn handle_at(
        &self,
        rect: Rect,
        x: u16,
        y: u16,
        path: &mut Vec<bool>,
    ) -> Option<(Rect, Axis, u16, u16, u16)> {
        let Self::Split(axis, _, a, b) = self else {
            return None;
        };
        let (first, second, divider) = self.parts(rect)?;
        if divider.contains(x, y) {
            let (aw, ah) = a.minimum();
            let (bw, bh) = b.minimum();
            return Some((
                rect,
                *axis,
                if *axis == Axis::Vertical { aw } else { ah },
                if *axis == Axis::Vertical { bw } else { bh },
                if *axis == Axis::Vertical {
                    first.width
                } else {
                    first.height
                },
            ));
        }
        if first.contains(x, y) {
            path.push(false);
            a.handle_at(first, x, y, path)
        } else if second.contains(x, y) {
            path.push(true);
            b.handle_at(second, x, y, path)
        } else {
            None
        }
    }
}

#[derive(Debug)]
pub(super) struct Resize {
    path: Vec<bool>,
    rect: Rect,
    axis: Axis,
    min_a: u16,
    min_b: u16,
    generation: u64,
    current: u16,
}

#[derive(Clone, Debug)]
pub(super) struct Layout {
    root: Node,
    pub active: WindowId,
    next: WindowId,
    generation: u64,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            root: Node::Leaf(0),
            active: 0,
            next: 1,
            generation: 0,
        }
    }
}

impl Layout {
    /// Balance every split without changing its topology or focused pane.
    pub fn equalize(&mut self) {
        self.root.equalize();
        self.generation += 1;
    }

    pub fn begin_resize(&self, size: (u16, u16), x: u16, y: u16) -> Option<Resize> {
        let minimum = self.root.minimum();
        if size.0 < minimum.0 || size.1 < minimum.1 {
            return None;
        }
        let mut path = Vec::new();
        let (rect, axis, min_a, min_b, current) = self.root.handle_at(
            Rect {
                width: size.0,
                height: size.1,
                ..Rect::default()
            },
            x,
            y,
            &mut path,
        )?;
        Some(Resize {
            path,
            rect,
            axis,
            min_a,
            min_b,
            generation: self.generation,
            current,
        })
    }

    pub fn resize(&mut self, handle: &mut Resize, x: u16, y: u16) -> bool {
        if handle.generation != self.generation {
            return false;
        }
        let (total, first) = match handle.axis {
            Axis::Vertical => (handle.rect.width - 1, x.saturating_sub(handle.rect.x)),
            Axis::Horizontal => (
                handle.rect.height,
                y.saturating_add(1).saturating_sub(handle.rect.y),
            ),
        };
        let first = first.clamp(handle.min_a, total - handle.min_b);
        if first == handle.current {
            return false;
        }
        let mut node = &mut self.root;
        for &second in &handle.path {
            let Node::Split(_, _, a, b) = node else {
                return false;
            };
            node = if second { b } else { a };
        }
        let Node::Split(_, ratio, _, _) = node else {
            return false;
        };
        *ratio = Ratio::Fraction { first, total };
        handle.current = first;
        true
    }

    fn raw(&self, size: (u16, u16)) -> (Vec<(WindowId, Rect)>, Vec<Rect>) {
        let mut leaves = Vec::new();
        let mut dividers = Vec::new();
        self.root.divide(
            Rect {
                width: size.0,
                height: size.1,
                ..Rect::default()
            },
            &mut leaves,
            &mut dividers,
        );
        (leaves, dividers)
    }

    pub fn ids(&self) -> Vec<WindowId> {
        self.raw((0, 0)).0.into_iter().map(|(id, _)| id).collect()
    }

    /// A tiny resize temporarily displays the active pane alone. The complete
    /// layout is retained and restored as soon as the terminal fits it again.
    pub fn visible(&self, size: (u16, u16)) -> (Vec<(WindowId, Rect)>, Vec<Rect>) {
        let (leaves, dividers) = self.raw(size);
        if leaves
            .iter()
            .any(|(_, r)| r.width < MIN_WIDTH || r.height < MIN_HEIGHT)
        {
            (
                vec![(
                    self.active,
                    Rect {
                        width: size.0,
                        height: size.1,
                        ..Rect::default()
                    },
                )],
                vec![],
            )
        } else {
            (leaves, dividers)
        }
    }

    pub fn split(&mut self, axis: Axis, size: (u16, u16)) -> io::Result<WindowId> {
        if self.ids().len() >= MAX_WINDOWS {
            return Err(io::Error::other("at most 16 windows may be open"));
        }
        let mut proposed = self.clone();
        let id = self.next;
        proposed.root.split(self.active, id, axis);
        if proposed
            .raw(size)
            .0
            .iter()
            .any(|(_, r)| r.width < MIN_WIDTH || r.height < MIN_HEIGHT)
        {
            return Err(io::Error::other("terminal is too small for another split"));
        }
        proposed.next = id.checked_add(1).expect("window identity exhausted");
        proposed.generation += 1;
        // The caller focuses the new window after installing its view.
        *self = proposed;
        Ok(id)
    }

    pub fn remove(&mut self, id: WindowId) {
        self.generation += 1;
        self.root = self
            .root
            .clone()
            .remove(id)
            .expect("retain at least one window");
    }

    pub fn only(&mut self) {
        self.generation += 1;
        self.root = Node::Leaf(self.active);
    }

    pub fn neighbor(&self, direction: Direction, size: (u16, u16)) -> Option<WindowId> {
        let (leaves, _) = self.visible(size);
        let current = leaves.iter().find(|(id, _)| *id == self.active)?.1;
        let (cx, cy, cw, ch) = (
            i32::from(current.x),
            i32::from(current.y),
            i32::from(current.width),
            i32::from(current.height),
        );
        leaves
            .into_iter()
            .filter_map(|(id, r)| {
                if id == self.active {
                    return None;
                }
                let (x, y, w, h) = (
                    i32::from(r.x),
                    i32::from(r.y),
                    i32::from(r.width),
                    i32::from(r.height),
                );
                let (distance, cross, overlaps) = match direction {
                    Direction::Left => (
                        cx - x - w,
                        (2 * y + h - 2 * cy - ch).abs(),
                        y < cy + ch && y + h > cy,
                    ),
                    Direction::Right => (
                        x - cx - cw,
                        (2 * y + h - 2 * cy - ch).abs(),
                        y < cy + ch && y + h > cy,
                    ),
                    Direction::Up => (
                        cy - y - h,
                        (2 * x + w - 2 * cx - cw).abs(),
                        x < cx + cw && x + w > cx,
                    ),
                    Direction::Down => (
                        y - cy - ch,
                        (2 * x + w - 2 * cx - cw).abs(),
                        x < cx + cw && x + w > cx,
                    ),
                };
                (overlaps && distance >= 0).then_some(((distance, cross, id), id))
            })
            .min_by_key(|(rank, _)| *rank)
            .map(|(_, id)| id)
    }

    pub fn swap(&mut self, other: WindowId) {
        self.generation += 1;
        self.root.swap(self.active, other);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equalization_balances_parallel_runs_regardless_of_split_order_and_terminal_size() {
        for axis in [Axis::Vertical, Axis::Horizontal] {
            for split_first in [false, true] {
                let mut layout = Layout::default();
                layout.equalize(); // A single pane is unchanged.
                for _ in 0..3 {
                    let id = layout.split(axis, (160, 48)).unwrap();
                    layout.active = if split_first { 0 } else { id };
                }
                let active = layout.active;
                let ids = layout.ids();
                layout.equalize();
                for size in [(51, 12), (122, 30), (123, 31), (124, 32), (160, 48)] {
                    let panes = layout.visible(size).0;
                    assert_eq!(panes.len(), 4);
                    let extents: Vec<_> = panes
                        .iter()
                        .map(|(_, rect)| match axis {
                            Axis::Vertical => rect.width,
                            Axis::Horizontal => rect.height,
                        })
                        .collect();
                    assert!(extents.iter().max().unwrap() - extents.iter().min().unwrap() <= 1);
                    let total: u16 = extents.iter().sum();
                    assert_eq!(
                        total,
                        if axis == Axis::Vertical {
                            size.0 - 3
                        } else {
                            size.1
                        }
                    );
                    layout.equalize();
                    assert_eq!(layout.visible(size).0, panes);
                }
                assert_eq!(layout.visible((2, 2)).0.len(), 1);
                layout.equalize();
                assert_eq!(layout.visible((160, 48)).0.len(), 4);
                assert_eq!(layout.active, active);
                assert_eq!(layout.ids(), ids);
            }
        }
    }

    #[test]
    fn equalization_resets_dragged_nested_splits_and_invalidates_old_handles() {
        let size = (122, 30);
        let mut layout = Layout::default();
        layout.active = layout.split(Axis::Vertical, size).unwrap();
        layout.active = layout.split(Axis::Vertical, size).unwrap();
        layout.active = layout.split(Axis::Horizontal, size).unwrap();
        layout.active = layout.split(Axis::Horizontal, size).unwrap();
        let mut drag = layout.begin_resize(size, 60, 1).unwrap();
        assert!(layout.resize(&mut drag, 20, 1));
        let upper = layout.visible(size).0[2].1;
        let mut nested = layout
            .begin_resize(size, upper.x + 1, upper.height - 1)
            .unwrap();
        assert!(layout.resize(&mut nested, upper.x + 1, 21));
        layout.equalize();
        assert!(!layout.resize(&mut drag, 55, 1));
        assert!(!layout.resize(&mut nested, 100, 20));
        let panes = layout.visible(size).0;
        assert!(panes.iter().all(|(_, rect)| rect.width == 40));
        assert_eq!(panes[0].1.height, 30);
        assert_eq!(panes[1].1.height, 30);
        assert!(panes[2..].iter().all(|(_, rect)| rect.height == 10));
        let tiny = layout.visible((38, 9)).0;
        assert_eq!(tiny.len(), 5);
        assert!(tiny.iter().all(|(_, rect)| rect.width == MIN_WIDTH));
        assert!(tiny[2..].iter().all(|(_, rect)| rect.height == MIN_HEIGHT));
        assert_eq!(layout.visible(size).0, panes);
    }

    #[test]
    fn drag_ratios_preserve_exact_cells_and_restore_after_small_resizes() {
        let mut layout = Layout::default();
        layout.split(Axis::Vertical, (82, 24)).unwrap();
        let mut drag = layout.begin_resize((82, 24), 40, 6).unwrap();
        assert!(!layout.resize(&mut drag, 40, 6));
        assert_eq!(layout.visible((101, 24)).0[0].1.width, 50);
        assert!(layout.resize(&mut drag, 60, 6));
        assert_eq!(layout.visible((82, 24)).0[0].1.width, 60);
        assert_eq!(layout.visible((82, 24)).0[1].1.width, 21);
        assert_eq!(layout.visible((163, 24)).0[0].1.width, 120);
        assert_eq!(layout.visible((25, 24)).0[0].1.width, 12);
        assert_eq!(layout.visible((20, 24)).0.len(), 1);
        assert!(layout.begin_resize((20, 24), 12, 5).is_none());
        assert_eq!(layout.visible((82, 24)).0[0].1.width, 60);
        assert!(layout.resize(&mut drag, u16::MAX, 6));
        assert_eq!(layout.visible((82, 24)).0[1].1.width, MIN_WIDTH);
        assert!(layout.resize(&mut drag, 0, 6));
        assert_eq!(layout.visible((82, 24)).0[0].1.width, MIN_WIDTH);
        assert_eq!(layout.active, 0);
    }

    #[test]
    fn nested_resize_handles_use_status_lines_and_subtree_minimums() {
        let mut layout = Layout::default();
        let right = layout.split(Axis::Vertical, (101, 30)).unwrap();
        layout.active = right;
        layout.split(Axis::Horizontal, (101, 30)).unwrap();
        let upper_right = layout.visible((101, 30)).0[1].1;
        let mut horizontal = layout
            .begin_resize((101, 30), upper_right.x + 2, 14)
            .unwrap();
        assert!(layout.resize(&mut horizontal, upper_right.x + 2, 20));
        assert_eq!(layout.visible((101, 30)).0[1].1.height, 21);
        assert!(layout.resize(&mut horizontal, 0, u16::MAX));
        assert_eq!(layout.visible((101, 30)).0[2].1.height, MIN_HEIGHT);
        // Create two columns in the upper-right subtree, then shrink its parent.
        layout.split(Axis::Vertical, (101, 30)).unwrap();
        let mut outer = layout.begin_resize((101, 30), 50, 1).unwrap();
        layout.resize(&mut outer, 100, 1);
        let panes = layout.visible((101, 30)).0;
        assert_eq!(panes.len(), 4);
        assert_eq!(panes[1].1.width, MIN_WIDTH);
        assert_eq!(panes[2].1.width, MIN_WIDTH);
        assert_eq!(panes[3].1.width, MIN_WIDTH * 2 + 1);
        assert!(
            !layout.resize(&mut horizontal, 0, 10),
            "topology changes invalidate old handles"
        );
        layout.only();
        assert!(!layout.resize(&mut outer, 10, 1));
    }

    #[test]
    fn nested_splits_focus_swap_close_and_resize_preserve_layout() {
        let mut layout = Layout::default();
        let right = layout.split(Axis::Vertical, (81, 21)).unwrap();
        layout.active = right;
        let bottom = layout.split(Axis::Horizontal, (81, 21)).unwrap();
        assert_eq!(
            layout.visible((81, 21)).0,
            vec![
                (
                    0,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 40,
                        height: 21
                    }
                ),
                (
                    right,
                    Rect {
                        x: 41,
                        y: 0,
                        width: 40,
                        height: 10
                    }
                ),
                (
                    bottom,
                    Rect {
                        x: 41,
                        y: 10,
                        width: 40,
                        height: 11
                    }
                ),
            ]
        );
        assert_eq!(layout.neighbor(Direction::Down, (81, 21)), Some(bottom));
        assert_eq!(layout.neighbor(Direction::Left, (81, 21)), Some(0));
        layout.swap(bottom);
        assert_eq!(layout.neighbor(Direction::Up, (81, 21)), Some(bottom));
        assert_eq!(layout.visible((3, 2)).0.len(), 1);
        assert_eq!(layout.visible((81, 21)).0.len(), 3);
        assert!(layout.split(Axis::Vertical, (3, 2)).is_err());
        layout.remove(bottom);
        assert_eq!(layout.visible((81, 21)).0[1].1.height, 21);
        layout.only();
        assert_eq!(layout.ids(), vec![right]);
    }
}
