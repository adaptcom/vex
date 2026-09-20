//! Incremental syntax parsing and viewport highlighting, independent of a terminal.
//!
//! Parsing reads borrowed rope chunks. Each revision edits the previous tree;
//! parsing is deferred until highlights are requested, coalescing input batches.
//! Highlight queries use byte ranges and return non-overlapping semantic spans.

mod language;
pub use language::{IndentStyle, Indentation, Language, LanguageServer};

use std::{
    cell::Cell,
    cmp::Reverse,
    collections::{BTreeSet, VecDeque},
    fmt,
    ops::{ControlFlow, Range},
    sync::Arc,
    time::{Duration, Instant},
};
use tree_sitter::{
    InputEdit, Node, ParseOptions, Parser, Point, Query, QueryCursor, QueryCursorOptions,
    StreamingIterator, Tree,
};
use vex_core::{ByteOffset, ChangeExtent, Document, Rope, RopeSlice, Snapshot};

/// Initial parsing ceiling. Larger documents remain editable as text.
pub const MAX_HIGHLIGHT_BYTES: usize = 2 << 20;
const PARSE_BUDGET: Duration = Duration::from_millis(25);
const QUERY_BUDGET: Duration = Duration::from_millis(2);
const MAX_CAPTURES: usize = 4_096;
const MAX_CACHED_RANGES: usize = 128;

/// Semantic colors, independent of any terminal palette or theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Highlight {
    Keyword,
    Type,
    Function,
    Constant,
    String,
    Comment,
    Operator,
    Punctuation,
    Attribute,
    Variable,
    Property,
    Label,
    Escape,
    Heading,
    Emphasis,
    Strong,
    Link,
}

impl Highlight {
    fn from_capture(name: &str) -> Option<Self> {
        match name {
            "string.escape" => return Some(Self::Escape),
            "text.title" => return Some(Self::Heading),
            "text.emphasis" => return Some(Self::Emphasis),
            "text.strong" => return Some(Self::Strong),
            "text.uri" | "text.reference" => return Some(Self::Link),
            "text.literal" => return Some(Self::String),
            _ => {}
        }
        Some(match name.split('.').next()? {
            "keyword" => Self::Keyword,
            "type" | "constructor" | "tag" => Self::Type,
            "function" => Self::Function,
            "constant" | "number" | "boolean" => Self::Constant,
            "string" => Self::String,
            "comment" => Self::Comment,
            "operator" => Self::Operator,
            "punctuation" => Self::Punctuation,
            "attribute" => Self::Attribute,
            "variable" => Self::Variable,
            "property" => Self::Property,
            "label" => Self::Label,
            "escape" => Self::Escape,
            _ => return None,
        })
    }
}

/// A half-open UTF-8 byte range and its semantic highlight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HighlightSpan {
    pub range: Range<ByteOffset>,
    pub highlight: Highlight,
}

struct Configuration {
    language: tree_sitter::Language,
    query: Query,
    highlights: Vec<Option<Highlight>>,
    inline: Option<Box<Configuration>>,
    inline_capture: Option<u32>,
}

impl Configuration {
    fn new(language: tree_sitter::Language, sources: &[&str], inline: Option<Box<Self>>) -> Self {
        let query = Query::new(&language, &sources.join("\n"))
            .expect("bundled highlight queries must match their grammar");
        let highlights = query
            .capture_names()
            .iter()
            .map(|name| Highlight::from_capture(name))
            .collect();
        let inline_capture = query.capture_index_for_name("vex.inline");
        Self {
            language,
            query,
            highlights,
            inline,
            inline_capture,
        }
    }
}

struct CachedRange {
    range: Range<ByteOffset>,
    spans: Arc<[HighlightSpan]>,
}

/// Syntax derived from one document revision. No terminal or filesystem I/O.
///
/// Synchronize after every edit/undo/redo to preserve incremental parsing. Skipped
/// revisions and document changes safely trigger a full parse. Parsing and query
/// budgets fall back to plain text, never stale or partially collected highlights.
/// A timed-out parse is retried after the next edit or when syntax is re-created.
pub struct Syntax {
    language: Language,
    configuration: &'static Configuration,
    parser: Parser,
    inline_parser: Option<Parser>,
    tree: Option<Tree>,
    snapshot: Snapshot,
    lf_count: usize,
    dirty: bool,
    cursor: QueryCursor,
    cache: VecDeque<CachedRange>,
    parse_budget: Duration,
    query_budget: Duration,
    #[cfg(test)]
    parses: usize,
    #[cfg(test)]
    incremental_parses: usize,
}

impl fmt::Debug for Syntax {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Syntax")
            .field("language", &self.language)
            .field("revision", &self.snapshot.revision())
            .field("dirty", &self.dirty)
            .finish_non_exhaustive()
    }
}

impl Syntax {
    pub fn new(language: Language, document: &Document) -> Self {
        Self::from_snapshot(language, document.snapshot())
    }

    /// Construct parser state on the thread that will own it.
    pub fn from_snapshot(language: Language, snapshot: Snapshot) -> Self {
        let configuration = language.configuration();
        let mut parser = Parser::new();
        parser
            .set_language(&configuration.language)
            .expect("bundled grammar must be compatible with Tree-sitter");
        let inline_parser = configuration.inline.as_ref().map(|configuration| {
            let mut parser = Parser::new();
            parser
                .set_language(&configuration.language)
                .expect("bundled inline grammar must be compatible with Tree-sitter");
            parser
        });
        let mut cursor = QueryCursor::new();
        cursor.set_match_limit(MAX_CAPTURES as u32);
        Self {
            language,
            configuration,
            parser,
            inline_parser,
            tree: None,
            lf_count: initial_lf_count(snapshot.text()),
            snapshot,
            dirty: true,
            cursor,
            cache: VecDeque::new(),
            parse_budget: PARSE_BUDGET,
            query_budget: QUERY_BUDGET,
            #[cfg(test)]
            parses: 0,
            #[cfg(test)]
            incremental_parses: 0,
        }
    }

    pub fn language(&self) -> Language {
        self.language
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// Update tree coordinates without parsing. Adjacent edits can be batched
    /// before a draw, including a counted undo that crosses several groups.
    pub fn synchronize(&mut self, document: &Document) {
        self.synchronize_snapshot(document.snapshot(), document.change_since(&self.snapshot));
    }

    /// Advance to a snapshot using a conservative edit covering all intervening
    /// changes. The caller must provide unchanged prefix/suffix coordinates
    /// relative to this syntax snapshot and the new snapshot, or None to reparse.
    pub fn synchronize_snapshot(&mut self, snapshot: Snapshot, change: Option<ChangeExtent>) {
        if self.snapshot.id() == snapshot.id() && self.snapshot.revision() == snapshot.revision() {
            return;
        }
        self.cache.clear();
        let old = self.snapshot.text();
        let new = snapshot.text();
        if old.len_bytes() <= MAX_HIGHLIGHT_BYTES
            && new.len_bytes() <= MAX_HIGHLIGHT_BYTES
            && self.snapshot.id() == snapshot.id()
            && let Some(change) = change
        {
            let old_compatible = old.len_lines() - 1 == self.lf_count;
            self.lf_count = self.lf_count - count_lf(old.slice(change.start.0..change.old_end.0))
                + count_lf(new.slice(change.start.0..change.new_end.0));
            // Ropey recognizes more line endings than Tree-sitter's LF-only
            // Points. Only use its fast line index when both conventions agree.
            // CR-only and Unicode separators still parse correctly from scratch.
            if old_compatible && new.len_lines() - 1 == self.lf_count {
                if let Some(tree) = &mut self.tree {
                    let start_byte = old.char_to_byte(change.start.0);
                    let old_end_byte = old.char_to_byte(change.old_end.0);
                    let new_end_byte = new.char_to_byte(change.new_end.0);
                    tree.edit(&InputEdit {
                        start_byte,
                        old_end_byte,
                        new_end_byte,
                        start_position: point(old, start_byte),
                        old_end_position: point(old, old_end_byte),
                        new_end_position: point(new, new_end_byte),
                    });
                }
            } else {
                self.tree = None;
            }
        } else {
            self.tree = None;
            self.lf_count = initial_lf_count(new);
        }
        self.parser.reset();
        self.snapshot = snapshot;
        self.dirty = true;
    }

    fn parse(&mut self, cancelled: &impl Fn() -> bool) {
        if !self.dirty || cancelled() {
            return;
        }
        self.dirty = false;
        let text = self.snapshot.text();
        if text.len_bytes() > MAX_HIGHLIGHT_BYTES || self.parse_budget.is_zero() {
            self.tree = None;
            return;
        }
        #[cfg(test)]
        {
            self.parses += 1;
            self.incremental_parses += usize::from(self.tree.is_some());
        }
        let start = Instant::now();
        let budget = self.parse_budget;
        let mut progress = |_: &tree_sitter::ParseState| {
            if cancelled() || start.elapsed() >= budget {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let tree = self.parser.parse_with_options(
            &mut |byte, _| chunk_from(text, byte),
            self.tree.as_ref(),
            Some(ParseOptions::new().progress_callback(&mut progress)),
        );
        // Superseded viewport requests may retry this revision. Keep the edited
        // old tree, but reset resumable parser state before using different input.
        if cancelled() {
            self.dirty = true;
            self.parser.reset();
            return;
        }
        self.tree = tree;
        if self.tree.is_none() {
            self.parser.reset();
        }
    }

    /// Query only captures intersecting a visible byte range. Results are sorted,
    /// disjoint, and clipped to that range; repeated requests share cached spans.
    /// Invalid or empty ranges and unavailable syntax return an empty slice.
    pub fn highlights(
        &mut self,
        document: &Document,
        range: Range<ByteOffset>,
    ) -> Arc<[HighlightSpan]> {
        self.synchronize(document);
        self.highlights_current(range, || false)
    }

    /// Query the current snapshot with cooperative cancellation. Cancelled work
    /// is never cached; time-budget failures remain cached until invalidation.
    pub fn highlights_current(
        &mut self,
        range: Range<ByteOffset>,
        is_cancelled: impl Fn() -> bool,
    ) -> Arc<[HighlightSpan]> {
        let text = self.snapshot.text();
        if is_cancelled() || range.start >= range.end || range.end.0 > text.len_bytes() {
            return Arc::from([]);
        }
        if let Some(index) = self.cache.iter().position(|entry| entry.range == range) {
            let entry = self.cache.remove(index).unwrap();
            let spans = Arc::clone(&entry.spans);
            self.cache.push_back(entry);
            return spans;
        }
        self.parse(&is_cancelled);
        if is_cancelled() {
            return Arc::from([]);
        }
        let Some(tree) = &self.tree else {
            return Arc::from([]);
        };
        let query = &self.configuration.query;
        let text = self.snapshot.text();
        self.cursor.set_byte_range(range.start.0..range.end.0);
        let start = Instant::now();
        let budget = self.query_budget;
        let cancelled = Cell::new(budget.is_zero());
        let mut progress = |_: &tree_sitter::QueryCursorState| {
            if is_cancelled() || start.elapsed() >= budget {
                cancelled.set(true);
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let mut raw = Vec::new();
        let mut inline_ranges = Vec::new();
        {
            // Query predicates also borrow chunks, including nodes that straddle
            // rope leaves; only Tree-sitter's predicate scratch buffers may copy.
            let mut captures = self.cursor.captures_with_options(
                query,
                tree.root_node(),
                |node: Node| text.byte_slice(node.byte_range()).chunks(),
                QueryCursorOptions::new().progress_callback(&mut progress),
            );
            while let Some((matched, index)) = captures.next() {
                if is_cancelled()
                    || raw.len() + inline_ranges.len() >= MAX_CAPTURES
                    || start.elapsed() >= budget
                {
                    cancelled.set(true);
                    break;
                }
                let capture = matched.captures()[*index];
                if Some(capture.index) == self.configuration.inline_capture {
                    inline_ranges.push(capture.node.range());
                    continue;
                }
                let Some(highlight) = self.configuration.highlights[capture.index as usize] else {
                    continue;
                };
                let node_range = capture.node.byte_range();
                let clipped = node_range.start.max(range.start.0)..node_range.end.min(range.end.0);
                if !clipped.is_empty() {
                    raw.push(Capture {
                        range: clipped,
                        // Inner captures override outer ones, such as escapes
                        // inside strings. Later query patterns break exact ties.
                        priority: (Reverse(node_range.len()), matched.pattern_index, raw.len()),
                        highlight,
                    });
                }
            }
        }
        // Markdown has separate block and inline grammars. Parse each visible
        // inline region in isolation so emphasis cannot leak across paragraphs
        // or into code fences. The same total query budget bounds this work;
        // the resulting spans share the regular viewport cache.
        let mut exceeded = self.cursor.did_exceed_match_limit();
        if let (Some(parser), Some(configuration)) =
            (&mut self.inline_parser, &self.configuration.inline)
        {
            for included in inline_ranges {
                if cancelled.get() || is_cancelled() || start.elapsed() >= budget {
                    cancelled.set(true);
                    break;
                }
                parser.reset();
                parser
                    .set_included_ranges(&[included])
                    .expect("inline node has a valid range");
                let mut parse_progress = |_: &tree_sitter::ParseState| {
                    if is_cancelled() || start.elapsed() >= budget {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                };
                let Some(tree) = parser.parse_with_options(
                    &mut |byte, _| chunk_from(text, byte),
                    None,
                    Some(ParseOptions::new().progress_callback(&mut parse_progress)),
                ) else {
                    parser.reset();
                    cancelled.set(true);
                    break;
                };
                let mut captures = self.cursor.captures_with_options(
                    &configuration.query,
                    tree.root_node(),
                    |node: Node| text.byte_slice(node.byte_range()).chunks(),
                    QueryCursorOptions::new().progress_callback(&mut progress),
                );
                while let Some((matched, index)) = captures.next() {
                    if is_cancelled() || raw.len() >= MAX_CAPTURES || start.elapsed() >= budget {
                        cancelled.set(true);
                        break;
                    }
                    let capture = matched.captures()[*index];
                    let Some(highlight) = configuration.highlights[capture.index as usize] else {
                        continue;
                    };
                    let node_range = capture.node.byte_range();
                    let clipped =
                        node_range.start.max(range.start.0)..node_range.end.min(range.end.0);
                    if !clipped.is_empty() {
                        raw.push(Capture {
                            range: clipped,
                            priority: (
                                Reverse(node_range.len()),
                                query.pattern_count() + matched.pattern_index,
                                raw.len(),
                            ),
                            highlight,
                        });
                    }
                }
                drop(captures);
                exceeded |= self.cursor.did_exceed_match_limit();
            }
        }
        if is_cancelled() {
            return Arc::from([]);
        }
        let spans: Arc<[HighlightSpan]> = if cancelled.get() || exceeded {
            Arc::from([])
        } else {
            resolve(raw).into()
        };
        if self.cache.len() == MAX_CACHED_RANGES {
            self.cache.pop_front();
        }
        self.cache.push_back(CachedRange {
            range,
            spans: Arc::clone(&spans),
        });
        spans
    }
}

fn chunk_from(text: &Rope, byte: usize) -> &[u8] {
    if byte >= text.len_bytes() {
        return &[];
    }
    let (chunk, start, _, _) = text.chunk_at_byte(byte);
    &chunk.as_bytes()[byte - start..]
}

fn point(text: &Rope, byte: usize) -> Point {
    let row = text.byte_to_line(byte);
    Point::new(row, byte - text.line_to_byte(row))
}

fn initial_lf_count(text: &Rope) -> usize {
    if text.len_bytes() <= MAX_HIGHLIGHT_BYTES {
        count_lf(text.slice(..))
    } else {
        0
    }
}

fn count_lf(text: RopeSlice<'_>) -> usize {
    text.chunks()
        .map(|chunk| chunk.bytes().filter(|&b| b == b'\n').count())
        .sum()
}

type Priority = (Reverse<usize>, usize, usize);
struct Capture {
    range: Range<usize>,
    priority: Priority,
    highlight: Highlight,
}

fn resolve(captures: Vec<Capture>) -> Vec<HighlightSpan> {
    let mut edges = Vec::with_capacity(captures.len() * 2);
    for (index, capture) in captures.iter().enumerate() {
        edges.push((capture.range.start, true, index));
        edges.push((capture.range.end, false, index));
    }
    edges.sort_unstable();
    let mut active = BTreeSet::<Priority>::new();
    let mut result: Vec<HighlightSpan> = Vec::new();
    let mut previous = 0;
    for (byte, entering, index) in edges {
        if previous < byte
            && let Some(&(_, _, chosen)) = active.last()
        {
            let highlight = captures[chosen].highlight;
            if let Some(last) = result.last_mut()
                && last.highlight == highlight
                && last.range.end.0 == previous
            {
                last.range.end = ByteOffset(byte);
            } else {
                result.push(HighlightSpan {
                    range: ByteOffset(previous)..ByteOffset(byte),
                    highlight,
                });
            }
        }
        let priority = captures[index].priority;
        if entering {
            active.insert(priority);
        } else {
            active.remove(&priority);
        }
        previous = byte;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::path::Path;
    use vex_core::{CharOffset, Edit, Selection, SelectionSet};

    // Correctness tests use generous budgets, independent of machine load.
    fn syntax(document: &Document) -> Syntax {
        let mut syntax = Syntax::new(Language::Rust, document);
        syntax.parse_budget = Duration::from_secs(10);
        syntax.query_budget = Duration::from_secs(10);
        syntax
    }

    fn all(syntax: &mut Syntax, document: &Document) -> Arc<[HighlightSpan]> {
        syntax.highlights(
            document,
            ByteOffset(0)..ByteOffset(document.text().len_bytes()),
        )
    }

    fn at(spans: &[HighlightSpan], byte: usize) -> Option<Highlight> {
        spans
            .iter()
            .find(|span| span.range.contains(&ByteOffset(byte)))
            .map(|span| span.highlight)
    }

    #[test]
    fn bundled_languages_highlight_and_stay_correct_after_edits() {
        use Highlight::*;
        type Case<'a> = (Language, &'a str, &'a [(&'a str, Highlight)]);
        let cases: &[Case<'_>] = &[
            (
                Language::Rust,
                "fn main() { let n = 3; }",
                &[("fn", Keyword), ("main", Function)],
            ),
            (
                Language::Bash,
                "# comment\nif true; then\n  echo \"hello\"\nfi\n",
                &[
                    ("# comment", Comment),
                    ("if", Keyword),
                    ("echo", Function),
                    ("hello", String),
                ],
            ),
            (
                Language::JavaScript,
                "const count = 42; function greet() { return \"hello\"; }",
                &[
                    ("const", Keyword),
                    ("42", Constant),
                    ("greet", Function),
                    ("hello", String),
                ],
            ),
            (
                Language::Jsx,
                "const view = <div title=\"hello\">text</div>;",
                &[
                    ("const", Keyword),
                    ("div", Type),
                    ("title", Attribute),
                    ("hello", String),
                ],
            ),
            (
                Language::TypeScript,
                "interface User { name: string }\nconst count: number = 42;",
                &[
                    ("interface", Keyword),
                    ("User", Type),
                    ("string", Type),
                    ("const", Keyword),
                    ("42", Constant),
                ],
            ),
            (
                Language::Tsx,
                "const View = (props: { name: string }) => <div title=\"hello\">{props.name}</div>;",
                &[
                    ("const", Keyword),
                    ("string", Type),
                    ("div", Type),
                    ("title", Attribute),
                    ("hello", String),
                ],
            ),
            (
                Language::Markdown,
                "# Heading\n\n**bold** and *italic* with `code` and [link](file.md).\n\n```sh\necho *literal*\n```\n",
                &[
                    ("Heading", Heading),
                    ("bold", Strong),
                    ("italic", Emphasis),
                    ("code", String),
                    ("link", Link),
                    ("file.md", Link),
                    ("literal", String),
                    ("echo", String),
                ],
            ),
        ];
        for &(language, source, expected) in cases {
            let mut document = Document::from(source);
            let mut syntax = Syntax::new(language, &document);
            syntax.parse_budget = Duration::from_secs(10);
            syntax.query_budget = Duration::from_secs(10);
            let spans = all(&mut syntax, &document);
            assert!(
                !syntax.tree.as_ref().unwrap().root_node().has_error(),
                "{language:?}"
            );
            for &(token, highlight) in expected {
                assert_eq!(
                    at(&spans, source.find(token).unwrap()),
                    Some(highlight),
                    "{language:?}: {token}"
                );
            }
            let mut selections = SelectionSet::single(Selection::cursor(CharOffset(0)));
            let transaction = document.replace_selections(&selections, "\n").unwrap();
            document.apply(transaction, &mut selections).unwrap();
            let updated = all(&mut syntax, &document);
            let mut fresh = Syntax::new(language, &document);
            fresh.parse_budget = Duration::from_secs(10);
            fresh.query_budget = Duration::from_secs(10);
            assert_eq!(updated, all(&mut fresh, &document), "{language:?}");
            assert_eq!(syntax.incremental_parses, 1);
        }
    }

    #[test]
    fn markdown_inline_regions_are_isolated_and_viewport_clipped() {
        let source = "*open\n\nclose*\n\n**bold**\n";
        let document = Document::from(source);
        let mut syntax = Syntax::new(Language::Markdown, &document);
        syntax.parse_budget = Duration::from_secs(10);
        syntax.query_budget = Duration::from_secs(10);
        let spans = all(&mut syntax, &document);
        assert_eq!(at(&spans, source.find("open").unwrap()), None);
        assert_eq!(at(&spans, source.find("close").unwrap()), None);
        let start = source.find("bold").unwrap();
        let clipped = syntax.highlights(&document, ByteOffset(start)..ByteOffset(start + 2));
        assert_eq!(
            &*clipped,
            &[HighlightSpan {
                range: ByteOffset(start)..ByteOffset(start + 2),
                highlight: Highlight::Strong
            }]
        );
    }

    fn assert_matches_fresh_parse(syntax: &mut Syntax, document: &Document) {
        syntax.synchronize(document);
        syntax.parse(&|| false);
        let tree = syntax.tree.as_ref().unwrap();
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let source = document.text().to_string();
        let fresh = parser.parse(&source, None).unwrap();
        // Tree-sitter's recovery can choose different error nodes depending on
        // parse history. Coordinates must remain exact even in incomplete code.
        if fresh.root_node().has_error() {
            let mut pending = vec![tree.root_node()];
            while let Some(node) = pending.pop() {
                let range = node.byte_range();
                assert!(range.start <= range.end && range.end <= source.len());
                for (byte, actual) in [
                    (range.start, node.start_position()),
                    (range.end, node.end_position()),
                ] {
                    let prefix = &source.as_bytes()[..byte];
                    let row = prefix.iter().filter(|&&b| b == b'\n').count();
                    let column = prefix
                        .iter()
                        .rposition(|&b| b == b'\n')
                        .map_or(byte, |last| byte - last - 1);
                    assert_eq!(actual, Point::new(row, column));
                }
                for index in 0..node.child_count() {
                    pending.push(node.child(index).unwrap());
                }
            }
            return;
        }
        let mut pending = vec![(tree.root_node(), fresh.root_node())];
        while let Some((actual, expected)) = pending.pop() {
            assert_eq!(
                actual.kind(),
                expected.kind(),
                "source: {:?}\nactual: {}\nexpected: {}",
                document.text().to_string(),
                tree.root_node().to_sexp(),
                fresh.root_node().to_sexp()
            );
            assert_eq!(actual.byte_range(), expected.byte_range());
            assert_eq!(actual.start_position(), expected.start_position());
            assert_eq!(actual.end_position(), expected.end_position());
            assert_eq!(actual.is_error(), expected.is_error());
            assert_eq!(actual.is_missing(), expected.is_missing());
            assert_eq!(actual.child_count(), expected.child_count());
            for index in 0..actual.child_count() {
                pending.push((actual.child(index).unwrap(), expected.child(index).unwrap()));
            }
        }
    }

    #[test]
    fn cancelled_parses_and_queries_retry_the_same_revision_without_partial_caches() {
        let document = Document::from("fn demo() { let value = 42; }\n".repeat(4_096).as_str());
        let mut syntax = syntax(&document);
        let range = ByteOffset(0)..ByteOffset(29);
        let checks = Cell::new(0);
        let cancelled = || {
            checks.set(checks.get() + 1);
            checks.get() >= 3
        };
        assert!(
            syntax
                .highlights_current(range.clone(), cancelled)
                .is_empty()
        );
        assert!(checks.get() >= 3);
        assert!(syntax.dirty);
        assert!(syntax.cache.is_empty());
        let spans = syntax.highlights_current(range.clone(), || false);
        assert!(!spans.is_empty());
        assert!(!syntax.dirty);
        // Use an already parsed tree and an empty range cache to interrupt
        // querying. No cancelled range may enter the cache.
        syntax.cache.clear();
        checks.set(0);
        assert!(
            syntax
                .highlights_current(range.clone(), || {
                    checks.set(checks.get() + 1);
                    checks.get() >= 4
                })
                .is_empty()
        );
        assert!(checks.get() >= 4);
        assert!(syntax.cache.is_empty());
        assert_eq!(syntax.highlights_current(range, || false), spans);
    }

    #[test]
    fn snapshot_edits_reuse_the_tree_across_coalesced_revisions() {
        let mut document = Document::from("fn main() {}\r\n");
        let mut syntax = syntax(&document);
        all(&mut syntax, &document);
        let mut selections = SelectionSet::single(Selection::cursor(CharOffset(0)));
        for text in ["/", "/", "界", "\r\n"] {
            let edit = document.replace_selections(&selections, text).unwrap();
            document.apply_grouped(edit, &mut selections).unwrap();
        }
        syntax.synchronize_snapshot(
            document.snapshot(),
            Some(ChangeExtent {
                start: CharOffset(0),
                old_end: CharOffset(0),
                new_end: CharOffset(5),
            }),
        );
        assert_matches_fresh_parse(&mut syntax, &document);
        assert_eq!(syntax.parses, 2);
        assert_eq!(syntax.incremental_parses, 1);
    }

    #[test]
    fn rust_highlights_include_predicates_and_nested_escape_overrides() {
        let source = "#[derive(Debug)]\r\nfn greet(名字: &str) -> u32 {\r\n  /* comment */ let message = \"界e\u{301}\\n\";\r\n  Vec::new(); 42\r\n}\r\n";
        let document = Document::from(source);
        let mut syntax = syntax(&document);
        let spans = all(&mut syntax, &document);
        for (needle, expected) in [
            ("fn", Highlight::Keyword),
            ("greet", Highlight::Function),
            ("名字", Highlight::Variable),
            ("str", Highlight::Type),
            ("u32", Highlight::Type),
            ("comment", Highlight::Comment),
            ("let", Highlight::Keyword),
            ("界", Highlight::String),
            ("\\n", Highlight::Escape),
            ("Vec", Highlight::Type),
            ("new", Highlight::Function),
            ("42", Highlight::Constant),
        ] {
            assert_eq!(
                at(&spans, source.find(needle).unwrap()),
                Some(expected),
                "{needle}"
            );
        }
        assert!(
            spans
                .windows(2)
                .all(|pair| pair[0].range.end <= pair[1].range.start)
        );
        assert_matches_fresh_parse(&mut syntax, &document);
        assert_eq!(
            Language::from_path(Path::new("src/lib.rs")),
            Some(Language::Rust)
        );
        assert_eq!(
            Language::from_path(Path::new("FILE.RS")),
            Some(Language::Rust)
        );
        assert_eq!(Language::from_path(Path::new("notes.txt")), None);
    }

    #[test]
    fn captures_cover_clipped_multiline_comments_and_chunked_strings() {
        let source = format!(
            "/*{}\nfn hidden() {{}}\n*/\nfn visible() {{ let s = \"{}界\\nend\"; }}",
            " comment ".repeat(160),
            "x".repeat(2048)
        );
        let document = Document::from(source.as_str());
        assert!(document.text().chunks().count() > 1);
        let mut syntax = syntax(&document);
        let start = source.find("hidden").unwrap();
        let window = ByteOffset(start)..ByteOffset(start + 3);
        let spans = syntax.highlights(&document, window.clone());
        assert_eq!(
            &*spans,
            &[HighlightSpan {
                range: window,
                highlight: Highlight::Comment
            }]
        );
        let start = source.find("界").unwrap();
        let spans = syntax.highlights(
            &document,
            ByteOffset(start)..ByteOffset(start + "界\\nend".len()),
        );
        assert_eq!(at(&spans, start), Some(Highlight::String));
        assert_eq!(at(&spans, start + "界".len()), Some(Highlight::Escape));
        assert_eq!(at(&spans, start + "界\\n".len()), Some(Highlight::String));
        assert_matches_fresh_parse(&mut syntax, &document);
    }

    #[test]
    fn batched_multi_caret_edits_and_grouped_history_reuse_the_tree() {
        let source = "fn first() { let a = 1; }\r\nfn second() { let b = 2; }\r\n";
        let mut document = Document::from(source);
        let mut syntax = syntax(&document);
        let original = all(&mut syntax, &document);
        let mut selections = SelectionSet::new(
            vec![
                Selection::cursor(CharOffset(0)),
                Selection::cursor(CharOffset(source.find("fn second").unwrap())),
            ],
            1,
        )
        .unwrap();
        for inserted in ["/*界", "e\u{301}*/"] {
            let transaction = document.replace_selections(&selections, inserted).unwrap();
            document
                .apply_grouped(transaction, &mut selections)
                .unwrap();
            syntax.synchronize(&document);
        }
        assert_eq!(syntax.parses, 1);
        assert_eq!(document.undo_depth(), 1);
        let edited = all(&mut syntax, &document);
        assert_eq!(syntax.parses, 2);
        assert_eq!(syntax.incremental_parses, 1);
        assert_matches_fresh_parse(&mut syntax, &document);
        document.undo(&mut selections).unwrap();
        assert_eq!(all(&mut syntax, &document), original);
        assert_matches_fresh_parse(&mut syntax, &document);
        document.redo(&mut selections).unwrap();
        assert_eq!(all(&mut syntax, &document), edited);
        assert_matches_fresh_parse(&mut syntax, &document);
        assert_eq!(syntax.incremental_parses, 3);
    }

    #[test]
    fn skipped_revisions_and_other_documents_discard_cached_syntax() {
        let mut document = Document::from("fn first() {}");
        let mut syntax = syntax(&document);
        let first = all(&mut syntax, &document);
        assert!(Arc::ptr_eq(&first, &all(&mut syntax, &document)));
        let mut selections = SelectionSet::default();
        for text in ["/*", "comment*/"] {
            let transaction = document.replace_selections(&selections, text).unwrap();
            document.apply(transaction, &mut selections).unwrap();
        }
        assert_matches_fresh_parse(&mut syntax, &document);
        assert_eq!(syntax.incremental_parses, 0);
        let other = Document::from("\"a string now\"");
        assert_eq!(at(&all(&mut syntax, &other), 0), Some(Highlight::String));
        assert_matches_fresh_parse(&mut syntax, &other);
        assert_eq!(syntax.incremental_parses, 0);
    }

    #[test]
    fn unfinished_comments_recover_after_undo_and_retyping_the_delimiter() {
        let source = "fn café() {\r\n let 名字 = \"界e\u{301}\"; /* comment */\n}\n";
        let mut document = Document::from(source);
        let mut syntax = syntax(&document);
        let before = all(&mut syntax, &document);
        let start = document.text().byte_to_char(source.find("*/").unwrap());
        let mut selections = SelectionSet::default();
        let transaction = document
            .transaction([Edit::delete(CharOffset(start)..CharOffset(start + 2))])
            .unwrap();
        document.apply(transaction, &mut selections).unwrap();
        assert_matches_fresh_parse(&mut syntax, &document);
        document.undo(&mut selections).unwrap();
        assert_eq!(all(&mut syntax, &document), before);
        document.redo(&mut selections).unwrap();
        syntax.synchronize(&document);
        let transaction = document
            .transaction([Edit::insert(CharOffset(start), "*/")])
            .unwrap();
        document.apply(transaction, &mut selections).unwrap();
        assert_eq!(all(&mut syntax, &document), before);
        assert_matches_fresh_parse(&mut syntax, &document);
    }

    #[test]
    fn unusual_line_endings_and_split_crlf_preserve_tree_coordinates() {
        for separator in [
            "\n", "\r\n", "\r", "\u{85}", "\u{2028}", "\u{2029}", "\u{b}", "\u{c}",
        ] {
            let source = format!("fn a() {{}}{separator}fn b() {{ let s = \"界\"; }}");
            let mut document = Document::from(source.as_str());
            let mut syntax = syntax(&document);
            assert_matches_fresh_parse(&mut syntax, &document);
            let mut selections = SelectionSet::default();
            let at = source.find("fn b").unwrap();
            let at = document.text().byte_to_char(at);
            for edit in [
                Edit::insert(CharOffset(at), "\r"),
                Edit::insert(CharOffset(at + 1), "\n"),
                Edit::delete(CharOffset(at)..CharOffset(at + 1)),
            ] {
                let transaction = document.transaction([edit]).unwrap();
                document.apply(transaction, &mut selections).unwrap();
                assert_matches_fresh_parse(&mut syntax, &document);
            }
            for _ in 0..3 {
                document.undo(&mut selections).unwrap();
                assert_matches_fresh_parse(&mut syntax, &document);
            }
        }
    }

    #[test]
    fn viewport_cache_is_bounded_and_empty_or_invalid_ranges_are_safe() {
        let document = Document::from("fn a() {}\n".repeat(MAX_CACHED_RANGES + 1).as_str());
        let mut syntax = syntax(&document);
        for line in 0..=MAX_CACHED_RANGES {
            let start = document.text().line_to_byte(line);
            let spans = syntax.highlights(&document, ByteOffset(start)..ByteOffset(start + 2));
            assert_eq!(at(&spans, start), Some(Highlight::Keyword));
        }
        assert_eq!(syntax.cache.len(), MAX_CACHED_RANGES);
        assert_eq!(syntax.parses, 1);
        for range in [
            ByteOffset(0)..ByteOffset(0),
            ByteOffset(0)..ByteOffset(usize::MAX),
        ] {
            assert!(syntax.highlights(&document, range).is_empty());
        }
    }

    #[test]
    fn budget_and_size_fallbacks_never_reuse_stale_highlights() {
        let mut document = Document::from("fn a() {}");
        let mut syntax = syntax(&document);
        assert!(!all(&mut syntax, &document).is_empty());
        let mut selections = SelectionSet::default();
        let transaction = document.replace_selections(&selections, "//").unwrap();
        document.apply(transaction, &mut selections).unwrap();
        syntax.parse_budget = Duration::ZERO;
        assert!(all(&mut syntax, &document).is_empty());
        syntax.parse_budget = Duration::from_secs(10);
        // No repeated parse attempts at the same revision.
        assert!(all(&mut syntax, &document).is_empty());
        document.undo(&mut selections).unwrap();
        syntax.query_budget = Duration::ZERO;
        assert!(all(&mut syntax, &document).is_empty());
        syntax.query_budget = Duration::from_secs(10);
        assert!(all(&mut syntax, &document).is_empty());
        let transaction = document
            .transaction([Edit::insert(
                CharOffset(document.text().len_chars()),
                " ".repeat(MAX_HIGHLIGHT_BYTES),
            )])
            .unwrap();
        document.apply(transaction, &mut selections).unwrap();
        assert!(all(&mut syntax, &document).is_empty());
        assert!(syntax.tree.is_none());
        document.undo(&mut selections).unwrap();
        assert!(!all(&mut syntax, &document).is_empty());
        assert_matches_fresh_parse(&mut syntax, &document);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn incremental_trees_match_fresh_parses_through_unicode_edits_and_history(
            actions in prop::collection::vec((0u8..6, any::<usize>(), any::<usize>(), 0usize..9), 1..35),
        ) {
            let mut document = Document::from("fn café() {\r\n let 名字 = \"界e\u{301}\"; /* comment */\n}\n");
            let mut syntax = syntax(&document);
            let mut selections = SelectionSet::default();
            assert_matches_fresh_parse(&mut syntax, &document);
            for (kind, a, b, inserted) in actions {
                let len = document.text().len_chars();
                let a = a % (len + 1);
                let b = b % (len + 1);
                match kind {
                    0 => { document.undo(&mut selections).unwrap(); }
                    1 => { document.redo(&mut selections).unwrap(); }
                    _ => {
                        let text = ["", "fn", "界", "\r", "\n", "\u{2028}", "/*", "\"", "e\u{301}"][inserted];
                        let transaction = document.transaction([Edit::new(CharOffset(a.min(b))..CharOffset(a.max(b)), text)]).unwrap();
                        if kind == 2 { document.apply(transaction, &mut selections).unwrap(); }
                        else { document.apply_grouped(transaction, &mut selections).unwrap(); }
                    }
                }
                assert_matches_fresh_parse(&mut syntax, &document);
            }
        }
    }
}
