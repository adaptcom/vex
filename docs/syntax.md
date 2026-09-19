# Syntax highlighting

The initial language is Rust, detected from `.rs` file names. Grammars are bundled
at build time, so running the editor needs no grammar downloads or external
Tree-sitter installation.

| Command | Behavior |
|---|---|
| `:language` | Show the current language and whether detection is automatic |
| `:language rust` | Enable Rust highlighting, including in scratch buffers |
| `:language text` | Use plain text |
| `:language auto` | Detect from the current file name and future Save As paths |

Unknown file types stay plain text. Manual overrides survive saves. Changing
syntax language never changes text, selections, revisions, or undo history.
The terminal honors a nonempty `NO_COLOR` environment variable, which suppresses
all colors, including syntax and selection colors.

## Parsing and drawing

`vex_syntax` owns the parser, syntax tree, a shared document snapshot, and a bounded
highlight cache. It uses the Rust grammar's
[bundled highlight query](https://docs.rs/tree-sitter-rust/latest/tree_sitter_rust/constant.HIGHLIGHTS_QUERY.html).
Queries and grammar configuration initialize once per process. Colors remain in
the terminal layer; syntax returns semantic byte ranges such as keyword, string,
comment, function, and type.

The parser and query predicates read borrowed rope chunks. Document edits do not
require a flattened copy of the file. Every edit and undo/redo updates the tree's
byte and point coordinates using `Document::change_since`. The editor synchronizes
after each revision, even within a batch of keys or counted undo. Parsing waits
until drawing asks for highlights, so several edits before a draw share one parse.
Skipped revisions or a different document discard the old tree.

Tree-sitter points count LF rows and byte columns. LF and CRLF documents use
Ropey's index for incremental coordinates. Documents containing bare CR or other
Unicode line separators use a fresh parse after edits, avoiding a mismatch between
Ropey's logical lines and Tree-sitter's rows. Normal Unicode text remains incremental.
Tracking LF counts scans changed regions; the conservative extent of widely spaced
multi-cursor edits or grouped undo can cover unchanged text between edits.

Drawing queries only the horizontally visible part of each displayed line.
Captures enclosing the viewport, such as multiline strings and comments, are
clipped to it. Smaller nested captures override enclosing ones, so escapes retain
their own style inside strings; later query patterns break equal-size ties.
Returned spans are sorted and disjoint. Up to 128 requested ranges are cached,
with at most 4,096 captures per query. Edits invalidate highlight spans. Cursor
movement over cached ranges does not reparse or rerun queries.

The grid paints syntax before selection and cursor overrides. UTF-8 byte offsets
are converted at line starts and advanced by whole grapheme byte lengths while
drawing, preserving wide cells, tabs, and horizontal clipping. The existing cell
diff detects color-only changes and emits nothing for identical frames.

## Initial limits

Parsing runs synchronously with a cooperative 25 ms budget per attempt and a
2 MiB document ceiling. Each highlight query has a cooperative 2 ms budget and a
match/capture limit. These are work limits, not hard latency guarantees: callbacks
cannot interrupt every parser or predicate operation.

A size or budget limit produces plain text; partial or stale highlights are never
shown. A cancelled parse retries after an edit or an explicit `:language rust`
reset. A cancelled query stays plain for that cached range until eviction, an edit,
or a language reset. Editing, saving, and undo remain available.

There are no injected languages, semantic tokens, user grammar loading, theme
configuration, or background parsing yet. The bundled query supplies syntactic
colors without symbol resolution. In incomplete code, Tree-sitter's error recovery
can produce different trees depending on editing history; valid code is checked
against a fresh parse, and node coordinates are checked for both cases.

Source tests cover incremental edits and grouped history, batched/multi-cursor
input, Unicode and line endings, chunk boundaries, nested and clipped captures,
cache bounds, plain-text fallback, file detection, and terminal style precedence.
The terminal smoke test checks that Rust colors reach a real PTY.
