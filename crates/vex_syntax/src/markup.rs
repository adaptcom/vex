//! Bounded Markdown presentation using the bundled block and inline grammars.
//! Prepare on a worker; frontends receive styled text, never parser state.

use std::{
    ops::ControlFlow,
    time::{Duration, Instant},
};
use tree_sitter::{Node, ParseOptions, Parser, Tree};

pub const MAX_BYTES: usize = 64 << 10;
pub const MAX_LINES: usize = 4096;
const MAX_NODES: usize = 8192;
const MAX_DEPTH: usize = 64;

/// Compact flags so a terminal can embed them without growing every screen cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attributes(u8);

impl Attributes {
    pub const STRONG: Self = Self(1);
    pub const EMPHASIS: Self = Self(2);
    pub const STRIKE: Self = Self(4);
    pub const CODE: Self = Self(8);
    pub const LINK: Self = Self(16);
    pub const MUTED: Self = Self(32);

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}
impl std::ops::BitOr for Attributes {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}
impl std::ops::BitOrAssign for Attributes {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub attributes: Attributes,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    pub spans: Vec<Span>,
    pub literal: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Document {
    pub lines: Vec<Line>,
    pub truncated: bool,
}

impl Document {
    pub fn plain(text: &str) -> Self {
        let source = bounded(text);
        let mut lines: Vec<_> = source
            .lines()
            .take(MAX_LINES + 1)
            .map(|line| Line {
                spans: vec![Span {
                    text: line.into(),
                    attributes: Attributes::default(),
                }],
                literal: true,
            })
            .collect();
        let truncated = source.len() < text.len() || lines.len() > MAX_LINES;
        lines.truncate(MAX_LINES);
        Self { lines, truncated }
    }

    /// Safe fallback for unsupported syntax or an exhausted parse budget.
    pub fn markdown(text: &str, cancelled: impl Fn() -> bool) -> Self {
        let source = bounded(text);
        let deadline = Instant::now() + Duration::from_millis(25);
        let parse = |source: &str, language: tree_sitter::Language| -> Option<Tree> {
            let mut parser = Parser::new();
            parser.set_language(&language).ok()?;
            let mut progress = |_: &tree_sitter::ParseState| {
                if cancelled() || Instant::now() >= deadline {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            };
            parser.parse_with_options(
                &mut |offset, _| &source.as_bytes()[offset..],
                None,
                Some(ParseOptions::new().progress_callback(&mut progress)),
            )
        };
        if cancelled() {
            return Self::default();
        }
        let Some(tree) = parse(source, tree_sitter_md::LANGUAGE.into()) else {
            return Self::plain(text);
        };
        let mut render = Render {
            source,
            document: Self::default(),
            nodes: 0,
            parse: &parse,
            cancelled: &cancelled,
            deadline,
        };
        if render
            .block(tree.root_node(), Attributes::default(), 0, 0)
            .is_none()
        {
            return if cancelled() {
                Self::default()
            } else {
                Self::plain(text)
            };
        }
        while render
            .document
            .lines
            .last()
            .is_some_and(|line| line.spans.is_empty())
        {
            render.document.lines.pop();
        }
        render.document.truncated =
            source.len() < text.len() || render.document.lines.len() > MAX_LINES;
        render.document.lines.truncate(MAX_LINES);
        render.document
    }

    pub fn is_empty(&self) -> bool {
        self.lines
            .iter()
            .all(|line| line.spans.iter().all(|span| span.text.trim().is_empty()))
    }

    pub fn contains(&self, needle: &str) -> bool {
        self.lines.iter().any(|line| {
            line.spans
                .iter()
                .map(|span| span.text.as_str())
                .collect::<String>()
                .contains(needle)
        })
    }

    pub fn plain_text(&self) -> String {
        self.lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl From<String> for Document {
    fn from(text: String) -> Self {
        Self::plain(&text)
    }
}
impl From<&str> for Document {
    fn from(text: &str) -> Self {
        Self::plain(text)
    }
}

fn bounded(text: &str) -> &str {
    let mut end = text.len().min(MAX_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

struct Render<'a, P, C> {
    source: &'a str,
    document: Document,
    nodes: usize,
    parse: &'a P,
    cancelled: &'a C,
    deadline: Instant,
}

impl<P, C> Render<'_, P, C>
where
    P: Fn(&str, tree_sitter::Language) -> Option<Tree>,
    C: Fn() -> bool,
{
    fn check(&mut self, depth: usize) -> Option<()> {
        self.nodes += 1;
        (self.nodes <= MAX_NODES
            && depth <= MAX_DEPTH
            && !(self.cancelled)()
            && Instant::now() < self.deadline)
            .then_some(())
    }
    fn push(&mut self, text: &str, attributes: Attributes) {
        if text.is_empty() {
            return;
        }
        if self.document.lines.is_empty() {
            self.document.lines.push(Line::default());
        }
        let line = self.document.lines.last_mut().unwrap();
        if let Some(last) = line
            .spans
            .last_mut()
            .filter(|last| last.attributes == attributes)
        {
            last.text.push_str(text);
        } else {
            line.spans.push(Span {
                text: text.into(),
                attributes,
            });
        }
    }
    fn newline(&mut self) {
        self.document.lines.push(Line::default());
    }
    fn separate(&mut self) {
        if self
            .document
            .lines
            .last()
            .is_some_and(|line| !line.spans.is_empty())
        {
            self.newline();
        }
    }
    fn blank(&mut self) {
        self.separate();
        if self.document.lines.len() > 1
            && !self.document.lines[self.document.lines.len() - 2]
                .spans
                .is_empty()
        {
            self.newline();
        }
    }
    fn literal(&mut self, text: &str, attributes: Attributes) {
        for line in text.lines() {
            self.push(line, attributes);
            if let Some(current) = self.document.lines.last_mut() {
                current.literal = true;
            }
            self.newline();
        }
    }
    fn inline_source(&mut self, source: &str, style: Attributes, depth: usize) -> Option<()> {
        let tree = (self.parse)(source, tree_sitter_md::INLINE_LANGUAGE.into())?;
        self.inline(tree.root_node(), source, style, depth)
    }
    fn block(
        &mut self,
        node: Node<'_>,
        style: Attributes,
        indent: usize,
        depth: usize,
    ) -> Option<()> {
        self.check(depth)?;
        let source = self.source;
        match node.kind() {
            "atx_heading" | "setext_heading" => {
                self.blank();
                if let Some(content) = node.child_by_field_name("heading_content") {
                    self.inline_source(
                        &source[content.byte_range()],
                        style | Attributes::STRONG,
                        depth + 1,
                    )?;
                }
                self.blank();
            }
            "inline" | "pipe_table_cell" => {
                // Continuation markers belong to the block grammar and must
                // not become literal > or indentation inside inline content.
                let mut text = String::new();
                let mut start = node.start_byte();
                for child in node.named_children(&mut node.walk()) {
                    if child.kind() == "block_continuation" {
                        text.push_str(&source[start..child.start_byte()]);
                        start = child.end_byte();
                    }
                }
                text.push_str(&source[start..node.end_byte()]);
                self.inline_source(&text, style, depth + 1)?;
            }
            "paragraph" => {
                for child in node.named_children(&mut node.walk()) {
                    self.block(child, style, indent, depth + 1)?;
                }
                self.separate();
                if indent == 0 {
                    self.blank();
                }
            }
            "fenced_code_block" => {
                self.blank();
                for child in node.named_children(&mut node.walk()) {
                    if child.kind() == "code_fence_content" {
                        self.literal(&source[child.byte_range()], style | Attributes::CODE);
                    }
                }
                self.blank();
            }
            "indented_code_block" => {
                self.blank();
                for line in source[node.byte_range()].lines() {
                    self.literal(
                        line.strip_prefix("    ")
                            .or_else(|| line.strip_prefix('\t'))
                            .unwrap_or(line),
                        style | Attributes::CODE,
                    );
                }
                self.blank();
            }
            "list_item" => {
                self.separate();
                self.push(&"  ".repeat(indent), style);
                let mut marker = "• ".to_owned();
                for child in node.named_children(&mut node.walk()) {
                    if matches!(child.kind(), "list_marker_dot" | "list_marker_parenthesis") {
                        marker = format!("{} ", source[child.byte_range()].trim());
                    }
                }
                self.push(&marker, style);
                for child in node.named_children(&mut node.walk()) {
                    self.block(child, style, indent + 1, depth + 1)?;
                }
                self.separate();
            }
            "block_quote" => {
                self.separate();
                let start = self.document.lines.len().saturating_sub(1);
                for child in node.named_children(&mut node.walk()) {
                    self.block(child, style, indent, depth + 1)?;
                }
                for line in &mut self.document.lines[start..] {
                    if !line.spans.is_empty() {
                        line.spans.insert(
                            0,
                            Span {
                                text: "│ ".into(),
                                attributes: style | Attributes::MUTED,
                            },
                        );
                    }
                }
            }
            "pipe_table_header" | "pipe_table_row" => {
                self.separate();
                let mut first = true;
                for cell in node
                    .named_children(&mut node.walk())
                    .filter(|child| child.kind() == "pipe_table_cell")
                {
                    if !first {
                        self.push(" │ ", style);
                    }
                    first = false;
                    self.block(
                        cell,
                        if node.kind() == "pipe_table_header" {
                            style | Attributes::STRONG
                        } else {
                            style
                        },
                        indent,
                        depth + 1,
                    )?;
                }
                self.separate();
            }
            "task_list_marker_checked" => self.push("☑ ", style),
            "task_list_marker_unchecked" => self.push("☐ ", style),
            "thematic_break" => {
                self.separate();
                self.push("────────", style | Attributes::MUTED);
                self.blank();
            }
            "html_block" => {
                self.literal(&source[node.byte_range()], style);
                self.blank();
            }
            "link_reference_definition"
            | "pipe_table_delimiter_row"
            | "block_continuation"
            | "block_quote_marker"
            | "list_marker_dot"
            | "list_marker_parenthesis"
            | "list_marker_minus"
            | "list_marker_plus"
            | "list_marker_star" => {}
            _ => {
                for child in node.named_children(&mut node.walk()) {
                    self.block(child, style, indent, depth + 1)?;
                }
            }
        }
        Some(())
    }
    fn inline(
        &mut self,
        node: Node<'_>,
        source: &str,
        mut style: Attributes,
        depth: usize,
    ) -> Option<()> {
        self.check(depth)?;
        let text = &source[node.byte_range()];
        match node.kind() {
            "emphasis_delimiter" | "code_span_delimiter" => return Some(()),
            "strong_emphasis" => style |= Attributes::STRONG,
            "emphasis" => style |= Attributes::EMPHASIS,
            "strikethrough" => style |= Attributes::STRIKE,
            "code_span" => style |= Attributes::CODE,
            "hard_line_break" => {
                self.newline();
                return Some(());
            }
            "backslash_escape" => {
                self.push(&text[1..], style);
                return Some(());
            }
            "inline_link"
            | "full_reference_link"
            | "collapsed_reference_link"
            | "shortcut_link"
            | "image" => {
                for child in node
                    .named_children(&mut node.walk())
                    .filter(|child| matches!(child.kind(), "link_text" | "image_description"))
                {
                    self.inline(child, source, style | Attributes::LINK, depth + 1)?;
                }
                return Some(());
            }
            "uri_autolink" | "email_autolink" => {
                self.push(
                    text.trim_start_matches('<').trim_end_matches('>'),
                    style | Attributes::LINK,
                );
                return Some(());
            }
            "entity_reference" | "numeric_character_reference" => {
                let decoded = entity(text);
                self.push(decoded.as_deref().unwrap_or(text), style);
                return Some(());
            }
            _ => {}
        }
        let mut start = node.start_byte();
        for child in node.named_children(&mut node.walk()) {
            self.push(&source[start..child.start_byte()].replace('\n', " "), style);
            self.inline(child, source, style, depth + 1)?;
            start = child.end_byte();
        }
        self.push(&source[start..node.end_byte()].replace('\n', " "), style);
        Some(())
    }
}

fn entity(text: &str) -> Option<String> {
    Some(match text {
        "&amp;" => "&".into(),
        "&lt;" => "<".into(),
        "&gt;" => ">".into(),
        "&quot;" => "\"".into(),
        "&apos;" => "'".into(),
        "&nbsp;" => "\u{a0}".into(),
        _ => {
            let number = text.strip_prefix("&#")?.strip_suffix(';')?;
            let (number, radix) = number
                .strip_prefix('x')
                .or_else(|| number.strip_prefix('X'))
                .map_or((number, 10), |s| (s, 16));
            char::from_u32(u32::from_str_radix(number, radix).ok()?)?.to_string()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn text(document: &Document) -> String {
        document
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn markdown_preserves_content_and_renders_structure_inline_styles_and_code() {
        let document = Document::markdown(
            "# Heading\n\nA **bold _and italic_** [link](https://example.com) with `x*y` and &amp; &#x1f980;.\n\n- first\n- second\n\n> quote\n\n```rust\nfn main() {}\n```\n",
            || false,
        );
        let rendered = text(&document);
        assert!(rendered.contains("Heading"), "{rendered:?}");
        assert!(
            rendered.contains("A bold and italic link with x*y and & 🦀."),
            "{rendered:?}"
        );
        assert!(rendered.contains("• first\n• second"), "{rendered:?}");
        assert!(rendered.contains("│ quote"), "{rendered:?}");
        assert!(rendered.contains("fn main() {}"));
        assert!(!rendered.contains("```"));
        assert!(!rendered.contains("https://"));
        let spans: Vec<_> = document.lines.iter().flat_map(|line| &line.spans).collect();
        assert!(spans.iter().any(|span| span.text.contains("and italic")
            && span.attributes.contains(Attributes::STRONG)
            && span.attributes.contains(Attributes::EMPHASIS)));
        assert!(
            spans
                .iter()
                .any(|span| span.text == "x*y" && span.attributes.contains(Attributes::CODE))
        );
        assert!(
            spans
                .iter()
                .any(|span| span.text == "link" && span.attributes.contains(Attributes::LINK))
        );
    }
    #[test]
    fn plaintext_stays_literal_and_markdown_has_size_depth_and_cancellation_limits() {
        assert_eq!(
            text(&Document::plain("**literal**\n# text")),
            "**literal**\n# text"
        );
        let document = Document::plain(&"🦀".repeat(MAX_BYTES));
        assert!(document.truncated);
        assert_eq!(text(&document).len(), MAX_BYTES);
        let lines = Document::plain(&"\n".repeat(MAX_BYTES));
        assert!(lines.truncated);
        assert_eq!(lines.lines.len(), MAX_LINES);
        assert!(Document::markdown("# Heading", || true).is_empty());
        let nested = format!("{}text", "> ".repeat(100));
        assert_eq!(text(&Document::markdown(&nested, || false)), nested);
    }
}
