//! Read-only delimiter lookups over a cheap, revision-matched tree clone.

use tree_sitter::{Node, Tree};
use vex_core::{CharOffset, Selection, Snapshot, pairs::Delimiters};

use crate::Language;

const SIBLING_LIMIT: usize = 16;

/// Immutable parse result shared with selection workers. Cloning shares both
/// rope storage and Tree-sitter subtrees. An unavailable tree records a failed
/// parse too, so repeated navigation does not keep retrying the same revision.
#[derive(Clone, Debug)]
pub struct ParsedSyntax {
    pub(crate) tree: Option<Tree>,
    pub(crate) snapshot: Snapshot,
    pub(crate) language: Language,
}

impl ParsedSyntax {
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    pub fn language(&self) -> Language {
        self.language
    }

    pub fn available(&self) -> bool {
        self.tree.is_some()
    }

    /// Match the delimiter under the cursor, or the enclosing scope's closing
    /// delimiter when fuzzy. Sibling searches are bounded and ancestor visits
    /// check cancellation. Brackets in opaque leaves use a local text fallback.
    pub fn matching(
        &self,
        position: CharOffset,
        fuzzy: bool,
        cancelled: &impl Fn() -> bool,
    ) -> Option<CharOffset> {
        let text = self.snapshot.text();
        let character = text.get_char(position.0)?;
        let pair = vex_core::pairs::pair(character);
        if !fuzzy && !vex_core::pairs::is_pair(pair.0, pair.1) {
            return None;
        }
        let root = self.tree.as_ref()?.root_node();
        let byte = text.char_to_byte(position.0);
        let mut node = root.descendant_for_byte_range(byte, byte + character.len_utf8())?;
        loop {
            if cancelled() {
                return None;
            }
            for pair in self
                .candidates(node, fuzzy, cancelled)
                .into_iter()
                .flatten()
            {
                if position == pair.close {
                    return Some(pair.open);
                }
                if position == pair.open
                    || (fuzzy && pair.open <= position && position < pair.close)
                {
                    return Some(pair.close);
                }
            }
            if !fuzzy && node.is_named() {
                break;
            }
            let Some(parent) = node.parent() else { break };
            node = parent;
        }
        // A comment is often a single opaque leaf. Keep its local brackets
        // usable without allowing them to escape into surrounding source code.
        let leaf = root.named_descendant_for_byte_range(byte, byte + character.len_utf8())?;
        if leaf.child_count() != 0 || cancelled() {
            return None;
        }
        let start = text.byte_to_char(leaf.start_byte());
        let contents = text.byte_slice(leaf.byte_range());
        vex_core::pairs::matching(contents, CharOffset(position.0 - start), cancelled)
            .map(|at| CharOffset(start + at.0))
    }

    /// Find counted pairs enclosing the entire range. Start after the selected
    /// text so a repeated around-object grows to the next outer scope. One
    /// ancestor traversal handles all counts; no repeated parsing or rescans.
    pub fn closest(
        &self,
        selection: Selection,
        mut count: usize,
        cancelled: &impl Fn() -> bool,
    ) -> Option<Delimiters> {
        let text = self.snapshot.text();
        let position = selection.end();
        let character = text.get_char(position.0)?;
        if count == 0 {
            return None;
        }
        let byte = text.char_to_byte(position.0);
        let mut node = self
            .tree
            .as_ref()?
            .root_node()
            .descendant_for_byte_range(byte, byte + character.len_utf8())?;
        let mut previous = None;
        loop {
            if cancelled() {
                return None;
            }
            for pair in self.candidates(node, true, cancelled).into_iter().flatten() {
                if pair.open <= selection.start()
                    && position <= pair.close
                    && previous != Some(pair)
                {
                    previous = Some(pair);
                    count -= 1;
                    if count == 0 {
                        return Some(pair);
                    }
                }
            }
            node = node.parent()?;
        }
    }

    // A small fixed array avoids per-node allocations. The enclosing node's
    // endpoints take priority, then the delimiter itself, then nearby siblings.
    fn candidates(
        &self,
        node: Node<'_>,
        fuzzy: bool,
        cancelled: &impl Fn() -> bool,
    ) -> [Option<Delimiters>; 3] {
        let edges = if node.is_named() && node.child_count() >= 2 {
            node.child(0)
                .zip(node.child(node.child_count() - 1))
                .and_then(|(open, close)| self.paired_nodes(open, close))
        } else {
            None
        };
        if edges.is_some() {
            return [edges, None, None];
        }
        let direct = self.sibling_pair(node, cancelled);
        if direct.is_some() {
            return [None, direct, None];
        }
        let nearby = if fuzzy {
            let mut sibling = node.next_sibling();
            let mut found = None;
            for _ in 0..SIBLING_LIMIT {
                if cancelled() {
                    break;
                }
                let Some(current) = sibling else { break };
                if let Some((at, ch)) = self.character(current)
                    && vex_core::pairs::pair(ch).1 == ch
                    && let Some(pair) = self.sibling_pair(current, cancelled)
                    && pair.close == at
                {
                    found = Some(pair);
                    break;
                }
                sibling = current.next_sibling();
            }
            found
        } else {
            None
        };
        [edges, direct, nearby]
    }

    fn character(&self, node: Node<'_>) -> Option<(CharOffset, char)> {
        if node.is_missing()
            || node.byte_range().is_empty()
            || node.end_byte() - node.start_byte() > 4
        {
            return None;
        }
        let text = self.snapshot.text();
        let at = text.byte_to_char(node.start_byte());
        let ch = text.get_char(at)?;
        (ch.len_utf8() == node.end_byte() - node.start_byte()).then_some((CharOffset(at), ch))
    }

    fn paired_nodes(&self, open: Node<'_>, close: Node<'_>) -> Option<Delimiters> {
        let (open_at, open) = self.character(open)?;
        let (close_at, close) = self.character(close)?;
        (open_at < close_at && vex_core::pairs::is_pair(open, close)).then_some(Delimiters {
            open: open_at,
            close: close_at,
        })
    }

    fn sibling_pair(&self, node: Node<'_>, cancelled: &impl Fn() -> bool) -> Option<Delimiters> {
        let (_, ch) = self.character(node)?;
        let (open, close) = vex_core::pairs::pair(ch);
        if !vex_core::pairs::is_pair(open, close) {
            return None;
        }
        // Try both directions for symmetric quotes and closure bars.
        for backward in [true, false] {
            if (backward && ch != close) || (!backward && ch != open) {
                continue;
            }
            let mut sibling = if backward {
                node.prev_sibling()
            } else {
                node.next_sibling()
            };
            for _ in 0..SIBLING_LIMIT {
                if cancelled() {
                    return None;
                }
                let Some(current) = sibling else { break };
                if let Some((_, candidate)) = self.character(current) {
                    if candidate == (if backward { open } else { close }) {
                        return if backward {
                            self.paired_nodes(current, node)
                        } else {
                            self.paired_nodes(node, current)
                        };
                    }
                    if candidate == ch {
                        break;
                    }
                }
                sibling = if backward {
                    current.prev_sibling()
                } else {
                    current.next_sibling()
                };
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Syntax;
    use vex_core::Document;

    fn parsed(source: &str) -> ParsedSyntax {
        Syntax::new(Language::Rust, &Document::from(source)).parsed(&|| false)
    }

    #[test]
    fn syntax_matches_scopes_strings_closure_bars_and_opaque_comment_brackets() {
        let source = "fn main() { let f = |x| { f(\"a)\\\"b\"); }; /* [c] */ }";
        let syntax = parsed(source);
        for (left, right) in [
            (source.find('(').unwrap(), source.find(')').unwrap()),
            (source.find('{').unwrap(), source.rfind('}').unwrap()),
            (source.find('|').unwrap(), source.rfind('|').unwrap()),
            (source.find('"').unwrap(), source.rfind('"').unwrap()),
        ] {
            assert_eq!(
                syntax.matching(CharOffset(left), true, &|| false),
                Some(CharOffset(right)),
                "at {left}"
            );
            assert_eq!(
                syntax.matching(CharOffset(right), true, &|| false),
                Some(CharOffset(left)),
                "at {right}"
            );
        }
        let inside = source.find("a)").unwrap();
        assert_eq!(
            syntax.matching(CharOffset(inside), true, &|| false),
            Some(CharOffset(source.rfind('"').unwrap()))
        );
        assert_eq!(syntax.matching(CharOffset(inside), false, &|| false), None);
        assert_eq!(
            syntax.matching(CharOffset(source.len()), true, &|| false),
            None
        );
        let comment = Syntax::new(Language::Bash, &Document::from("# [c]")).parsed(&|| false);
        assert_eq!(
            comment.matching(CharOffset(2), true, &|| false),
            Some(CharOffset(4))
        );
        assert_eq!(
            comment.matching(CharOffset(4), true, &|| false),
            Some(CharOffset(2))
        );
    }

    #[test]
    fn nearest_pairs_follow_selection_extent_counts_and_unicode_offsets() {
        let source = "fn f() { foo([\"界\"]); }";
        let syntax = parsed(source);
        let inside = source[..source.find('界').unwrap()].chars().count();
        let selection = Selection::new(CharOffset(inside), CharOffset(inside + 1));
        for (count, open, close) in [(1, '"', '"'), (2, '[', ']'), (3, '(', ')'), (4, '{', '}')] {
            let left = if open == '(' {
                source.rfind(open)
            } else {
                source.find(open)
            }
            .unwrap();
            let right = source.rfind(close).unwrap();
            let pair = Delimiters {
                open: CharOffset(source[..left].chars().count()),
                close: CharOffset(source[..right].chars().count()),
            };
            assert_eq!(
                syntax.closest(selection, count, &|| false),
                Some(pair),
                "count {count}"
            );
        }
        assert_eq!(syntax.closest(selection, usize::MAX, &|| false), None);
        assert_eq!(syntax.closest(selection, 1, &|| true), None);
    }

    #[test]
    fn shared_trees_survive_incremental_edits_and_unavailable_parses_stay_unavailable() {
        let mut document = Document::from("fn f() { [1] }");
        let mut syntax = Syntax::new(Language::Rust, &document);
        let old = syntax.parsed(&|| false);
        let cloned = old.clone();
        assert_eq!(
            cloned.matching(CharOffset(7), true, &|| false),
            Some(CharOffset(13))
        );
        assert_eq!(syntax.parses, 1);
        let transaction = document
            .transaction([vex_core::Edit::insert(CharOffset(0), "// comment\n")])
            .unwrap();
        document
            .apply(transaction, &mut vex_core::SelectionSet::default())
            .unwrap();
        syntax.synchronize(&document);
        let new = syntax.parsed(&|| false);
        assert_eq!(
            old.matching(CharOffset(7), true, &|| false),
            Some(CharOffset(13))
        );
        assert_eq!(
            new.matching(CharOffset(18), true, &|| false),
            Some(CharOffset(24))
        );
        assert_ne!(old.snapshot.revision(), new.snapshot.revision());
        syntax.dirty = true;
        syntax.parse_budget = std::time::Duration::ZERO;
        let unavailable = syntax.parsed(&|| false);
        assert!(!unavailable.available());
        assert!(!syntax.parsed(&|| false).available());
    }
}
