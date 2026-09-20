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
}

#[derive(Clone, Debug)]
enum Node {
    Leaf(WindowId),
    Split(Axis, Box<Node>, Box<Node>),
}

impl Node {
    fn divide(&self, rect: Rect, leaves: &mut Vec<(WindowId, Rect)>, dividers: &mut Vec<Rect>) {
        match self {
            Self::Leaf(id) => leaves.push((*id, rect)),
            Self::Split(axis, first, second) => {
                let (mut a, mut b, mut divider) = (rect, rect, rect);
                match axis {
                    Axis::Vertical => {
                        let available = rect.width.saturating_sub(1);
                        a.width = available / 2;
                        divider.x += a.width;
                        divider.width = u16::from(rect.width > 0);
                        b.x = divider.x + divider.width;
                        b.width = available - a.width;
                        dividers.push(divider);
                    }
                    Axis::Horizontal => {
                        // The upper pane's status line is the separator.
                        a.height = rect.height / 2;
                        b.y += a.height;
                        b.height = rect.height - a.height;
                    }
                }
                first.divide(a, leaves, dividers);
                second.divide(b, leaves, dividers);
            }
        }
    }

    fn split(&mut self, target: WindowId, id: WindowId, axis: Axis) -> bool {
        match self {
            Self::Leaf(old) if *old == target => {
                *self = Self::Split(axis, Box::new(Self::Leaf(target)), Box::new(Self::Leaf(id)));
                true
            }
            Self::Leaf(_) => false,
            Self::Split(_, a, b) => a.split(target, id, axis) || b.split(target, id, axis),
        }
    }

    fn remove(self, target: WindowId) -> Option<Self> {
        match self {
            Self::Leaf(id) => (id != target).then_some(Self::Leaf(id)),
            Self::Split(axis, a, b) => match (a.remove(target), b.remove(target)) {
                (Some(a), Some(b)) => Some(Self::Split(axis, Box::new(a), Box::new(b))),
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
            Self::Split(_, first, second) => {
                first.swap(a, b);
                second.swap(a, b);
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct Layout {
    root: Node,
    pub active: WindowId,
    next: WindowId,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            root: Node::Leaf(0),
            active: 0,
            next: 1,
        }
    }
}

impl Layout {
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
        // The caller focuses the new window after installing its view.
        *self = proposed;
        Ok(id)
    }

    pub fn remove(&mut self, id: WindowId) {
        self.root = self
            .root
            .clone()
            .remove(id)
            .expect("retain at least one window");
    }

    pub fn only(&mut self) {
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
        self.root.swap(self.active, other);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
