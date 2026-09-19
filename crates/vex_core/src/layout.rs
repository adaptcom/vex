//! Lazy, bounded display-column indexes shared by movement and rendering.
//! Checkpoints refer to grapheme boundaries, not rope chunk boundaries.

use crate::{CharOffset, Document, DocumentId, Error, Revision, Rope, display, grapheme, motion};
use std::{
    collections::{BTreeMap, VecDeque},
    num::NonZeroUsize,
};

const MAX_LINES: usize = 128;
const MAX_CHECKPOINTS: usize = 4096;
const INITIAL_STRIDE: usize = 256;
const RECENT_POINTS: usize = 8;

#[derive(Clone, Copy, Debug)]
struct Point {
    position: CharOffset,
    column: usize,
}

#[derive(Debug)]
struct Line {
    // Scalar offsets are relative to the line start, so an unchanged line's
    // index can move after an edit without rewriting thousands of checkpoints.
    checkpoints: Vec<Point>,
    recent: VecDeque<Point>,
    stride: usize,
    used: u64,
    #[cfg(test)]
    scanned: usize,
}

impl Line {
    fn new(used: u64) -> Self {
        Self {
            checkpoints: vec![Point {
                position: CharOffset(0),
                column: 0,
            }],
            recent: VecDeque::with_capacity(RECENT_POINTS),
            stride: INITIAL_STRIDE,
            used,
            #[cfg(test)]
            scanned: 0,
        }
    }

    fn checkpoint(&mut self, point: Point) {
        if point
            .position
            .0
            .saturating_sub(self.checkpoints.last().unwrap().position.0)
            < self.stride
        {
            return;
        }
        if self.checkpoints.len() == MAX_CHECKPOINTS {
            let mut index = 0;
            self.checkpoints.retain(|_| {
                let keep = index % 2 == 0;
                index += 1;
                keep
            });
            self.stride = self.stride.saturating_mul(2);
        }
        if point
            .position
            .0
            .saturating_sub(self.checkpoints.last().unwrap().position.0)
            >= self.stride
        {
            self.checkpoints.push(point);
        }
    }

    fn remember(&mut self, point: Point) {
        if let Some(index) = self
            .recent
            .iter()
            .position(|p| p.position == point.position)
        {
            self.recent.remove(index);
        }
        if self.recent.len() == RECENT_POINTS {
            self.recent.pop_front();
        }
        self.recent.push_back(point);
    }

    fn before(&self, target: Target) -> Point {
        let index = self
            .checkpoints
            .partition_point(|&p| target.can_start_at(p));
        let mut point = self.checkpoints[index.saturating_sub(1)];
        for &recent in &self.recent {
            if target.can_start_at(recent) && recent.position > point.position {
                point = recent;
            }
        }
        point
    }

    fn scan(
        &mut self,
        text: &Rope,
        start: CharOffset,
        end: CharOffset,
        target: Target,
        tabs: NonZeroUsize,
    ) -> Result<Point, Error> {
        let end = CharOffset(end.0 - start.0);
        let mut point = self.before(target);
        let mut cursor = None;
        while point.position < end && !target.reached(point) {
            // Printable ASCII is one cell per scalar, except that the last byte
            // of a run may join the following Unicode text. Leave it to the
            // grapheme cursor, including at rope chunk boundaries.
            if cursor.is_none() {
                cursor = Some(grapheme::Cursor::new(
                    text,
                    CharOffset(start.0 + point.position.0),
                )?);
            }
            let cursor = cursor.as_mut().unwrap();
            let ascii = cursor.ascii_prefix().saturating_sub(1);
            let remaining = target.remaining(point).min(end.0 - point.position.0);
            let to_checkpoint = self
                .stride
                .saturating_sub(
                    point
                        .position
                        .0
                        .saturating_sub(self.checkpoints.last().unwrap().position.0),
                )
                .max(1);
            let advance = ascii.min(remaining).min(to_checkpoint);
            let next = if advance > 0 {
                cursor.advance_ascii(advance);
                Point {
                    position: CharOffset(point.position.0 + advance),
                    column: point.column.saturating_add(advance),
                }
            } else {
                let cluster = cursor.next_grapheme().expect("before logical line end");
                let next = CharOffset(point.position.0 + cluster.chars().count());
                let span = display::width(&cluster, point.column, tabs);
                Point {
                    position: next,
                    column: point.column.saturating_add(span),
                }
            };
            if target.overshot(next) {
                break;
            }
            #[cfg(test)]
            {
                self.scanned += next.position.0 - point.position.0;
            }
            point = next;
            self.checkpoint(point);
        }
        self.remember(point);
        Ok(point)
    }
}

#[derive(Clone, Copy)]
enum Target {
    Position(CharOffset),
    Column(usize),
}

impl Target {
    fn can_start_at(self, point: Point) -> bool {
        match self {
            Self::Position(position) => point.position <= position,
            // Strict comparison also handles columns saturating at usize::MAX:
            // column lookup must find the *first* boundary reaching that value.
            Self::Column(usize::MAX) => point.column < usize::MAX,
            Self::Column(column) => point.column <= column,
        }
    }
    fn reached(self, point: Point) -> bool {
        match self {
            Self::Position(p) => point.position >= p,
            Self::Column(c) => point.column >= c,
        }
    }
    fn overshot(self, point: Point) -> bool {
        match self {
            Self::Position(p) => point.position > p,
            Self::Column(c) => point.column > c,
        }
    }
    fn remaining(self, point: Point) -> usize {
        match self {
            Self::Position(p) => p.0.saturating_sub(point.position.0),
            Self::Column(c) => c.saturating_sub(point.column),
        }
    }
}

/// A view's display-column cache. Indexes up to 128 recently used logical lines,
/// with at most 4,096 sparse checkpoints and eight recent positions per line.
/// Very long lines progressively increase checkpoint spacing to bound memory.
/// The first query scans the needed prefix; subsequent queries reuse the index.
/// No document text or historical snapshots are retained by this cache.
#[derive(Debug, Default)]
pub struct LayoutCache {
    version: Option<(DocumentId, Revision)>,
    tabs: Option<NonZeroUsize>,
    lines: BTreeMap<CharOffset, Line>,
    clock: u64,
}

impl LayoutCache {
    /// Update after each edit, undo, or redo to retain the unchanged prefix and
    /// shift unaffected later lines without rebuilding their indexes.
    /// Queries also synchronize automatically; skipped revisions or a different
    /// document discard the index conservatively. Failed/no-op edits need no work.
    pub fn synchronize(&mut self, document: &Document) {
        let version = (document.id(), document.revision());
        if self.version == Some(version) {
            return;
        }
        if self.version.is_some_and(|(id, revision)| {
            id == version.0 && revision.get().checked_add(1) == Some(version.1.get())
        }) {
            let change = document.change;
            for (start, mut line) in std::mem::take(&mut self.lines) {
                if start > change.old_end {
                    let shifted = CharOffset(start.0 - change.old_end.0 + change.new_end.0);
                    self.lines.insert(shifted, line);
                } else if start < change.start {
                    // The boundary at an edit may join its new neighbor; keep
                    // only boundaries strictly before the first changed scalar.
                    let changed = CharOffset(change.start.0 - start.0);
                    line.checkpoints
                        .truncate(line.checkpoints.partition_point(|p| p.position < changed));
                    line.recent.retain(|p| p.position < changed);
                    self.lines.insert(start, line);
                }
            }
        } else {
            self.lines.clear();
        }
        self.version = Some(version);
    }

    fn line(&mut self, document: &Document, start: CharOffset, tabs: NonZeroUsize) -> &mut Line {
        self.synchronize(document);
        if self.tabs != Some(tabs) {
            self.lines.clear();
            self.tabs = Some(tabs);
        }
        self.clock = self.clock.saturating_add(1);
        if !self.lines.contains_key(&start) && self.lines.len() == MAX_LINES {
            let oldest = *self
                .lines
                .iter()
                .min_by_key(|(_, line)| line.used)
                .unwrap()
                .0;
            self.lines.remove(&oldest);
        }
        let line = self
            .lines
            .entry(start)
            .or_insert_with(|| Line::new(self.clock));
        line.used = self.clock;
        line
    }

    /// Display column at a scalar position, rounded down to a grapheme boundary.
    pub fn column(
        &mut self,
        document: &Document,
        position: CharOffset,
        tabs: NonZeroUsize,
    ) -> Result<usize, Error> {
        let text = document.text();
        let position = grapheme::floor(text, position)?;
        let start = motion::line_start(text, position)?;
        Ok(self
            .line(document, start, tabs)
            .scan(
                text,
                start,
                position,
                Target::Position(CharOffset(position.0 - start.0)),
                tabs,
            )?
            .column)
    }

    /// Locate a column in the logical line containing `position`. Returns the
    /// grapheme's scalar boundary and actual column. Inside tabs/wide glyphs,
    /// return their start; beyond the line, return its end before the newline.
    pub fn at_column(
        &mut self,
        document: &Document,
        position: CharOffset,
        column: usize,
        tabs: NonZeroUsize,
    ) -> Result<(CharOffset, usize), Error> {
        let text = document.text();
        let start = motion::line_start(text, position)?;
        let end = motion::line_end(text, start)?;
        let point = self.line(document, start, tabs).scan(
            text,
            start,
            end,
            Target::Column(column),
            tabs,
        )?;
        Ok((CharOffset(start.0 + point.position.0), point.column))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Edit, SelectionSet};
    use proptest::prelude::*;
    use unicode_segmentation::UnicodeSegmentation;

    fn check(cache: &mut LayoutCache, document: &Document, position: usize, tabs: NonZeroUsize) {
        let text = document.text();
        let line = text.char_to_line(position);
        let start = CharOffset(text.line_to_char(line));
        let end = motion::line_end(text, start).unwrap();
        let flat = text.slice(start.0..end.0).to_string();
        let mut points = vec![Point {
            position: start,
            column: 0,
        }];
        for cluster in flat.graphemes(true) {
            let last = *points.last().unwrap();
            points.push(Point {
                position: CharOffset(last.position.0 + cluster.chars().count()),
                column: last
                    .column
                    .saturating_add(display::width(cluster, last.column, tabs)),
            });
        }
        let expected = points
            .iter()
            .rfind(|p| p.position.0 <= position)
            .unwrap()
            .column;
        assert_eq!(
            cache.column(document, CharOffset(position), tabs).unwrap(),
            expected
        );
        for goal in [
            0,
            1,
            expected.saturating_sub(1),
            expected,
            expected.saturating_add(1),
            usize::MAX,
        ] {
            let index = points
                .iter()
                .position(|p| p.column >= goal)
                .unwrap_or(points.len() - 1);
            let expected = points[if points[index].column > goal {
                index - 1
            } else {
                index
            }];
            assert_eq!(
                cache
                    .at_column(document, CharOffset(position), goal, tabs)
                    .unwrap(),
                (expected.position, expected.column)
            );
        }
    }

    #[test]
    fn cached_layout_matches_flat_segmentation_across_chunks_and_line_endings() {
        for input in [
            "a\t界e\u{301}👩\u{200d}💻\0\r\n\u{301}\t🇺🇸🇨🇦\rnext\u{2028}last\n".repeat(90),
            format!("{}{}end", "a".repeat(1023), "\u{301}".repeat(1500)),
            "🇺🇸🇨🇦🇯🇵".repeat(150),
            String::new(),
        ] {
            let document = Document::from(input.as_str());
            let mut cache = LayoutCache::default();
            let len = document.text().len_chars();
            for width in [1, 4, 7, usize::MAX] {
                let tabs = NonZeroUsize::new(width).unwrap();
                for index in (0..=60).rev() {
                    check(&mut cache, &document, index * len / 60, tabs);
                }
            }
        }
    }

    #[test]
    fn indexes_are_bounded_and_end_edits_retain_the_indexed_prefix() {
        let len = 2 << 20;
        let mut document = Document::from("x".repeat(len).as_str());
        let tabs = NonZeroUsize::new(4).unwrap();
        let mut cache = LayoutCache::default();
        assert_eq!(cache.column(&document, CharOffset(len), tabs).unwrap(), len);
        let line = &cache.lines[&CharOffset(0)];
        assert!(line.stride > INITIAL_STRIDE);
        assert!(line.checkpoints.len() <= MAX_CHECKPOINTS);
        let scanned = line.scanned;
        let stride = line.stride;
        assert_eq!(
            cache
                .at_column(&document, CharOffset(0), len - 80, tabs)
                .unwrap(),
            (CharOffset(len - 80), len - 80)
        );
        let mut selections = SelectionSet::default();
        for (position, text) in [(len - 2, "e"), (len - 1, "\u{301}")] {
            let transaction = document
                .transaction([Edit::insert(CharOffset(position), text)])
                .unwrap();
            document
                .apply_grouped(transaction, &mut selections)
                .unwrap();
            cache.synchronize(&document);
        }
        assert_eq!(document.undo_depth(), 1);
        assert_eq!(
            cache.column(&document, CharOffset(len + 2), tabs).unwrap(),
            len + 1
        );
        document.undo(&mut selections).unwrap();
        assert_eq!(cache.column(&document, CharOffset(len), tabs).unwrap(), len);
        document.redo(&mut selections).unwrap();
        assert_eq!(
            cache.column(&document, CharOffset(len + 2), tabs).unwrap(),
            len + 1
        );
        assert!(cache.lines[&CharOffset(0)].scanned - scanned < stride * 4 + 100);

        let document = Document::from("x\n".repeat(MAX_LINES * 2).as_str());
        for position in (0..document.text().len_chars()).step_by(2) {
            cache.column(&document, CharOffset(position), tabs).unwrap();
        }
        assert_eq!(cache.lines.len(), MAX_LINES);
    }

    #[test]
    fn skipped_revisions_wrong_documents_and_invalid_queries_do_not_reuse_stale_layout() {
        let mut document = Document::from("a\t界\r\nx");
        let tabs = NonZeroUsize::new(4).unwrap();
        let mut cache = LayoutCache::default();
        check(&mut cache, &document, 3, tabs);
        assert!(cache.column(&document, CharOffset(999), tabs).is_err());
        assert!(
            cache
                .at_column(&document, CharOffset(999), 0, tabs)
                .is_err()
        );
        let mut selections = SelectionSet::default();
        for (position, text) in [(0, "z"), (2, "\n")] {
            let transaction = document
                .transaction([Edit::insert(CharOffset(position), text)])
                .unwrap();
            document.apply(transaction, &mut selections).unwrap();
        }
        for position in 0..=document.text().len_chars() {
            check(&mut cache, &document, position, tabs);
        }
        let other = Document::from("界\tzz");
        check(&mut cache, &other, 4, tabs);
        check(&mut cache, &other, 4, NonZeroUsize::new(8).unwrap());
    }

    #[test]
    fn unchanged_later_lines_shift_without_rescanning_but_line_joins_invalidate_them() {
        let len = 20_000;
        let mut document = Document::from(format!("a\n{}", "x".repeat(len)).as_str());
        let tabs = NonZeroUsize::new(4).unwrap();
        let mut cache = LayoutCache::default();
        let mut selections = SelectionSet::default();
        assert_eq!(
            cache.column(&document, CharOffset(len + 2), tabs).unwrap(),
            len
        );
        let scanned = cache.lines[&CharOffset(2)].scanned;
        let transaction = document
            .transaction([Edit::insert(CharOffset(0), "界\r\n")])
            .unwrap();
        document.apply(transaction, &mut selections).unwrap();
        assert_eq!(
            cache.column(&document, CharOffset(len + 5), tabs).unwrap(),
            len
        );
        assert_eq!(cache.lines[&CharOffset(5)].scanned, scanned);
        document.undo(&mut selections).unwrap();
        assert_eq!(
            cache.column(&document, CharOffset(len + 2), tabs).unwrap(),
            len
        );
        assert_eq!(cache.lines[&CharOffset(2)].scanned, scanned);
        document.redo(&mut selections).unwrap();
        assert_eq!(
            cache.column(&document, CharOffset(len + 5), tabs).unwrap(),
            len
        );
        assert_eq!(cache.lines[&CharOffset(5)].scanned, scanned);
        let transaction = document
            .transaction([Edit::delete(CharOffset(4)..CharOffset(5))])
            .unwrap();
        document.apply(transaction, &mut selections).unwrap();
        assert_eq!(
            cache.column(&document, CharOffset(len + 4), tabs).unwrap(),
            len + 1
        );
        assert!(!cache.lines.contains_key(&CharOffset(5)));
        assert!(cache.lines[&CharOffset(3)].scanned >= len);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn edits_undo_redo_and_line_joins_match_flat_layout(
            parts in prop::collection::vec(prop::sample::select(vec!["a", "\t", "界", "e\u{301}", "👩\u{200d}💻", "🇺", "🇸", "\r\n", "\r", "\u{301}"]), 0..60),
            steps in prop::collection::vec((0u8..8, any::<u16>(), any::<u16>(), 0usize..8), 1..40),
        ) {
            let mut document = Document::from(parts.concat().as_str());
            let mut selections = SelectionSet::default();
            let mut cache = LayoutCache::default();
            let tabs = NonZeroUsize::new(4).unwrap();
            for (kind, a, b, inserted) in steps {
                let len = document.text().len_chars();
                for position in [0, len / 2, len] { check(&mut cache, &document, position, tabs); }
                let a = usize::from(a) % (len + 1);
                let b = usize::from(b) % (len + 1);
                match kind {
                    0 => { document.undo(&mut selections).unwrap(); }
                    1 => { document.redo(&mut selections).unwrap(); }
                    _ => {
                        let text = ["", "x", "\t", "\r\n", "\u{301}", "\u{200d}", "🇺", "界"][inserted];
                        let edits = if kind == 7 && a != b {
                            vec![Edit::insert(CharOffset(a.max(b)), text), Edit::insert(CharOffset(a.min(b)), "\n")]
                        } else {
                            vec![Edit::new(CharOffset(a.min(b))..CharOffset(a.max(b)), text)]
                        };
                        let transaction = document.transaction(edits).unwrap();
                        if kind < 4 {
                            document.apply(transaction, &mut selections).unwrap();
                        } else {
                            document.apply_grouped(transaction, &mut selections).unwrap();
                        }
                    }
                }
                cache.synchronize(&document);
                let len = document.text().len_chars();
                for position in [0, len / 3, len / 2, len.saturating_sub(1), len] { check(&mut cache, &document, position, tabs); }
            }
        }
    }
}
