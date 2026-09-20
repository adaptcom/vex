//! Bounded Myers line diff. Baseline lines are interned once; equality is exact,
//! including the final newline, with CRLF normalized to LF for Git checkouts.

use std::{
    collections::HashMap,
    ops::Range,
    time::{Duration, Instant},
};
use vex_core::Rope;
use vex_editor::background::Cancellation;

pub const MAX_LINES: usize = 200_000;
const MAX_DISTANCE: usize = 1024;
const MAX_WORK: usize = 4_000_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub before: Range<usize>,
    pub after: Range<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Marker {
    Added,
    Modified,
    Deleted,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Diff {
    pub hunks: Vec<Hunk>,
    /// Expensive diffs leave the gutter empty instead of publishing an estimate.
    pub limited: bool,
    markers: Vec<(Range<usize>, Marker)>,
}

impl Diff {
    fn new(hunks: Vec<Hunk>) -> Self {
        let markers = hunks
            .iter()
            .map(|hunk| {
                if hunk.after.is_empty() {
                    // The overline sits on the next line, including the empty
                    // trailing line when a removal reaches the end of a file.
                    (hunk.after.start..hunk.after.start + 1, Marker::Deleted)
                } else {
                    (
                        hunk.after.clone(),
                        if hunk.before.is_empty() {
                            Marker::Added
                        } else {
                            Marker::Modified
                        },
                    )
                }
            })
            .collect();
        Self {
            hunks,
            markers,
            limited: false,
        }
    }

    pub fn marker(&self, line: usize) -> Option<Marker> {
        let end = self
            .markers
            .partition_point(|(range, _)| range.start <= line);
        end.checked_sub(1).and_then(|index| {
            let (range, marker) = &self.markers[index];
            range.contains(&line).then_some(*marker)
        })
    }
}

pub(crate) struct BaseLines {
    lines: HashMap<String, u32>,
    tokens: Vec<u32>,
}

fn normalized_lines(text: &Rope) -> impl Iterator<Item = String> + '_ {
    text.lines()
        .filter(|line| line.len_bytes() > 0)
        .map(|line| {
            let mut line = line.to_string();
            if line.ends_with("\r\n") {
                line.remove(line.len() - 2);
            }
            line
        })
}

impl BaseLines {
    pub fn new(text: &Rope, cancellation: &Cancellation) -> Option<Self> {
        if text.len_lines() > MAX_LINES {
            return None;
        }
        let mut lines = HashMap::new();
        let mut tokens = Vec::new();
        for line in normalized_lines(text) {
            if cancellation.is_cancelled() {
                return None;
            }
            let next = lines.len() as u32;
            tokens.push(*lines.entry(line).or_insert(next));
        }
        Some(Self { lines, tokens })
    }

    pub fn diff(&self, text: &Rope, cancellation: &Cancellation) -> Option<Diff> {
        if text.len_lines() > MAX_LINES {
            return Some(Diff {
                limited: true,
                ..Diff::default()
            });
        }
        let mut after = Vec::new();
        for line in normalized_lines(text) {
            if cancellation.is_cancelled() {
                return None;
            }
            // Unknown lines can share a sentinel: only cross-side equality matters.
            after.push(self.lines.get(&line).copied().unwrap_or(u32::MAX));
        }
        let mut budget = Budget {
            cancellation,
            work: 0,
            until: Instant::now() + Duration::from_millis(100),
        };
        let hunks = myers(&self.tokens, &after, &mut budget);
        if cancellation.is_cancelled() {
            return None;
        }
        Some(match hunks {
            Some(hunks) => Diff::new(hunks),
            None => Diff {
                limited: true,
                ..Diff::default()
            },
        })
    }
}

struct Budget<'a> {
    cancellation: &'a Cancellation,
    work: usize,
    until: Instant,
}
impl Budget<'_> {
    fn step(&mut self) -> Option<()> {
        self.work += 1;
        if self.work > MAX_WORK
            || (self.work.is_multiple_of(1024)
                && (self.cancellation.is_cancelled() || Instant::now() >= self.until))
        {
            None
        } else {
            Some(())
        }
    }
}

#[derive(Clone, Copy)]
enum Edit {
    Equal,
    Insert,
    Delete,
}

fn myers(before: &[u32], after: &[u32], budget: &mut Budget<'_>) -> Option<Vec<Hunk>> {
    let mut prefix = 0;
    while prefix < before.len().min(after.len()) && before[prefix] == after[prefix] {
        budget.step()?;
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < (before.len() - prefix).min(after.len() - prefix)
        && before[before.len() - 1 - suffix] == after[after.len() - 1 - suffix]
    {
        budget.step()?;
        suffix += 1;
    }
    let a = &before[prefix..before.len() - suffix];
    let b = &after[prefix..after.len() - suffix];
    if a.is_empty() && b.is_empty() {
        return Some(Vec::new());
    }
    if a.is_empty() || b.is_empty() || b.iter().all(|token| *token == u32::MAX) {
        return Some(vec![Hunk {
            before: prefix..prefix + a.len(),
            after: prefix..prefix + b.len(),
        }]);
    }
    let max = (a.len() + b.len()).min(MAX_DISTANCE);
    let offset = max + 1;
    let mut frontier = vec![-1isize; 2 * max + 3];
    frontier[offset + 1] = 0;
    let mut trace = Vec::new();
    let (n, m) = (a.len() as isize, b.len() as isize);
    for distance in 0..=max {
        let d = distance as isize;
        let mut complete = false;
        for k in (-d..=d).step_by(2) {
            budget.step()?;
            let index = (offset as isize + k) as usize;
            let mut x = if k == -d || (k != d && frontier[index - 1] < frontier[index + 1]) {
                frontier[index + 1]
            } else {
                frontier[index - 1] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                budget.step()?;
                x += 1;
                y += 1;
            }
            frontier[index] = x;
            if x >= n && y >= m {
                complete = true;
                break;
            }
        }
        trace.push(frontier[offset - distance..=offset + distance].to_vec());
        if complete {
            let (mut x, mut y) = (n, m);
            let mut edits = Vec::new();
            for distance in (1..trace.len()).rev() {
                let d = distance as isize;
                let k = x - y;
                let previous = |k: isize| trace[distance - 1][(k + d - 1) as usize];
                let previous_k = if k == -d || (k != d && previous(k - 1) < previous(k + 1)) {
                    k + 1
                } else {
                    k - 1
                };
                let previous_x = previous(previous_k);
                let previous_y = previous_x - previous_k;
                while x > previous_x && y > previous_y {
                    edits.push(Edit::Equal);
                    x -= 1;
                    y -= 1;
                }
                if x == previous_x {
                    edits.push(Edit::Insert);
                    y -= 1;
                } else {
                    edits.push(Edit::Delete);
                    x -= 1;
                }
            }
            while x > 0 && y > 0 {
                edits.push(Edit::Equal);
                x -= 1;
                y -= 1;
            }
            let (mut x, mut y) = (prefix, prefix);
            let mut hunks = Vec::new();
            let mut start = None;
            for edit in edits.into_iter().rev().chain(std::iter::once(Edit::Equal)) {
                match edit {
                    Edit::Equal => {
                        if let Some((a, b)) = start.take() {
                            hunks.push(Hunk {
                                before: a..x,
                                after: b..y,
                            });
                        }
                        x += 1;
                        y += 1;
                    }
                    Edit::Delete => {
                        start.get_or_insert((x, y));
                        x += 1;
                    }
                    Edit::Insert => {
                        start.get_or_insert((x, y));
                        y += 1;
                    }
                }
            }
            return Some(hunks);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn diff(before: &str, after: &str) -> Diff {
        let cancellation = Cancellation::default();
        BaseLines::new(&Rope::from_str(before), &cancellation)
            .unwrap()
            .diff(&Rope::from_str(after), &cancellation)
            .unwrap()
    }

    #[test]
    fn markers_cover_added_modified_deleted_boundaries_and_empty_buffers() {
        let changed = diff("a\nb\nc\nd\ne\n", "a\nB\nc\nextra\nd\n");
        assert_eq!(changed.marker(0), None);
        assert_eq!(changed.marker(1), Some(Marker::Modified));
        assert_eq!(changed.marker(3), Some(Marker::Added));
        assert_eq!(changed.marker(4), None);
        assert_eq!(changed.marker(5), Some(Marker::Deleted));
        assert_eq!(diff("a\nb\n", "b\n").marker(0), Some(Marker::Deleted));
        assert_eq!(diff("a\n", "").marker(0), Some(Marker::Deleted));
        assert_eq!(diff("", "x\ny\n").marker(1), Some(Marker::Added));
        assert_eq!(diff("x\n", "x").marker(0), Some(Marker::Modified));
        assert!(diff("界\r\n🦀\r\n", "界\n🦀\n").hunks.is_empty());
    }

    #[test]
    fn cancellation_and_limits_do_not_publish_approximate_hunks() {
        let cancellation = Cancellation::default();
        let base = BaseLines::new(&Rope::from_str("a\n"), &cancellation).unwrap();
        cancellation.cancel();
        assert!(base.diff(&Rope::from_str("b\n"), &cancellation).is_none());
        let a = (0..2000).map(|i| format!("{i}\n")).collect::<String>();
        let b = (0..2000)
            .rev()
            .map(|i| format!("{i}\n"))
            .collect::<String>();
        assert!(diff(&a, &b).limited);
    }

    proptest! {
        #[test]
        fn hunks_reconstruct_the_target_and_match_a_small_lcs_oracle(a in prop::collection::vec(0u8..6,0..30), b in prop::collection::vec(0u8..6,0..30)) {
            let source = |values:&[u8]| values.iter().map(|n| format!("{n}\n")).collect::<String>();
            let result = diff(&source(&a), &source(&b));
            prop_assert!(!result.limited);
            let mut rebuilt = Vec::new();
            let mut cursor = 0;
            let mut distance = 0;
            for hunk in result.hunks {
                rebuilt.extend_from_slice(&a[cursor..hunk.before.start]);
                rebuilt.extend_from_slice(&b[hunk.after.clone()]);
                distance += hunk.before.len() + hunk.after.len();
                cursor = hunk.before.end;
            }
            rebuilt.extend_from_slice(&a[cursor..]);
            prop_assert_eq!(rebuilt, b.clone());
            let mut lcs = vec![vec![0; b.len()+1]; a.len()+1];
            for i in 0..a.len() { for j in 0..b.len() {
                lcs[i+1][j+1] = if a[i] == b[j] { lcs[i][j]+1 } else { lcs[i][j+1].max(lcs[i+1][j]) };
            }}
            prop_assert_eq!(distance, a.len()+b.len()-2*lcs[a.len()][b.len()]);
        }
    }
}
