# Syntax highlighting

Languages are detected from file extensions, known filenames, and shebangs.
Grammars are bundled at build time, so running the editor needs no grammar
downloads or external Tree-sitter installation.

| Language | Common files / interpreters | Manual name or alias |
|---|---|---|
| Rust | `.rs` | `rust`, `rs` |
| Markdown | `.md`, `.markdown`, `.mdown`, `.mkd` | `markdown`, `md` |
| Bash / POSIX shell | `.sh`, `.bash`, `.bashrc`, `.bash_profile`, `.profile`, `PKGBUILD`; `bash`, `sh`, `dash` shebangs | `bash`, `sh`, `shell` |
| JavaScript / JSX | `.js`, `.mjs`, `.cjs`, `.jsx`; `node` shebangs | `javascript`, `js`, `jsx` |
| TypeScript / TSX | `.ts`, `.mts`, `.cts`, `.tsx` | `typescript`, `ts`, `tsx` |

Known paths take precedence over shebangs. Detection reads at most 256 characters
of a shebang and supports `/usr/bin/env bash` and `/usr/bin/env -S bash -eu`.
It runs on open, Save As, and `:language auto`; editing a shebang does not
continually change the active language. Bash syntax is not a dedicated Zsh or
Fish grammar.

| Command | Behavior |
|---|---|
| `:language` | Show the current language and whether detection is automatic |
| `:language NAME` | Select any registry name or alias, including in scratch buffers |
| `:language text` | Use plain text |
| `:language auto` | Detect from the current filename/shebang and future Save As paths |

Unknown file types stay plain text. Manual overrides survive saves. Changing
syntax language never changes text, selections, revisions, or undo history.
The same language selection controls [language servers](lsp.md) for named files;
`:language text` stops language services as well as syntax highlighting.
The terminal honors a nonempty `NO_COLOR` environment variable, which suppresses
all colors, including syntax and selection colors.

## Indentation

On opening a buffer or reloading it from disk, Vex samples up to 1,000 lines and
129 characters at the start of each line to infer tabs or a space indentation
width from 2–8. Blank lines and block-comment continuation stars are ignored.
Recurring changes in indentation take priority over absolute depth, so a large
nested block does not make the indentation unit larger. A style must account for
at least three quarters of the sample; sparse, conflicting, or tied evidence
falls back to the language's defaults.

Rust and plain text default to four spaces; Markdown, Bash/shell,
JavaScript/JSX, and TypeScript/TSX default to two. Detected spaces set both the
indentation and tab display width. Literal tabs identify the style but cannot
reveal a display width, so they retain the language's default tab stops.

Tab inserts one unit of the chosen spaces/tabs at every insert caret. `>`/`<`
and LSP formatting use the same buffer settings. Detection does not rewrite the
file, run on each keystroke, or change settings while typing. All views of a
buffer share its settings.

The editor API exposes `Indentation { style: IndentStyle, tab_width }` and
`Editor::set_indentation`. `IndentStyle::Spaces(NonZeroUsize)` and
`IndentStyle::Tabs` support independent indentation and tab display widths.
Explicit buffer overrides survive reloads and resetting the same language.
Changing language restores the cached detection with that language's fallback.
Project/user configuration remains future work. `o`, `O`, and Enter continue
copying the existing whitespace prefix literally; syntax-based indentation is
not inferred by these settings.

## Comments

`Space-c` and normal/select-mode Ctrl-c toggle comments; `Space-C` requests block
comments. Rust and JavaScript/TypeScript use `//` and `/* */`; shell uses `#`,
including for the block command. Markdown toggles `<!-- -->` per selected line
with `Space-c` or around selections with `Space-C`. Plain text defaults to `#`
for lines and `/* */` for blocks. Rust recognizes existing `///` and `//!`
comments as well as block documentation comments.

Line comments share the minimum indentation of the selected nonblank lines;
overlapping line selections are edited once. Block toggling skips whitespace
selections and retains existing comments when other selections need commenting.
Both preserve mode, direction, the primary selection, and one-step undo.
The implementation inspects rope prefixes and range edges without copying the
buffer. `cargo bench -p vex_editor --bench commands -- comment_selected_lines_and_undo`
measures selected-line work in 1 MiB and 100 MiB buffers.

## Parsing and drawing

`vex_syntax` owns the parser, syntax tree, a shared document snapshot, and a bounded
highlight cache. In the terminal, a persistent syntax worker owns this state,
including grammar initialization and each grammar's bundled highlight queries.
TypeScript combines the JavaScript base query with its TypeScript additions;
TSX also includes JSX captures.
Queries and grammar configuration initialize once per process. Colors remain in
the terminal layer; syntax returns semantic byte ranges such as keyword, string,
comment, function, and type. Markdown adds heading, emphasis, strong emphasis,
and link captures. Its block tree is incremental; visible inline regions are
parsed separately under the query budget and cached with their resulting spans.
Inline syntax does not cross paragraphs or enter fenced code blocks. Code fences
receive literal styling; highlighting their embedded language is future work.

Drawing reads cached spans and records visible byte ranges. After drawing, the
event loop submits a batch of snapshot requests containing the missing ranges
for all visible buffers. Views of the same buffer contribute to one request and
share its cached spans. The worker retains a parser per visible buffer; a batch
completion prevents one buffer from overwriting another buffer's results. Text
appears immediately; completed colors trigger a redraw even while input is idle.
Search and syntax have independent workers and completion slots in the shared
event queue. Neither worker accesses mutable editor state. Completed highlights
also publish a cheap immutable tree clone for match-mode commands. The editor
shares that tree with selection jobs only at the same document revision and
language. Edits invalidate it, and hiding a buffer releases it. A cold structural
request parses on the selection worker under the same limits and caches its
result; subsequent bracket navigation reuses it while the buffer stays visible
and its revision and language remain unchanged.

The parser and query predicates read borrowed rope chunks without flattening the
file. Every edit and undo/redo records a conservative changed extent using
`Document::change_since`, even within a batch of keys or counted undo. The editor
retains at most 256 small change records, without intermediate rope snapshots.
The worker composes these extents across coalesced revisions and updates the old
tree's byte and point coordinates before parsing. If it falls further behind,
misses a revision, or changes documents, it safely reparses from scratch.

Tree-sitter points count LF rows and byte columns. LF and CRLF documents use
Ropey's index for incremental coordinates. Documents containing bare CR or other
Unicode line separators use a fresh parse after edits, avoiding a mismatch between
Ropey's logical lines and Tree-sitter's rows. Normal Unicode text remains incremental.
Tracking LF counts scans changed regions on the worker; the extent of widely spaced
multi-cursor edits or grouped undo can cover unchanged text between edits.

Drawing requests only the horizontally visible part of each displayed line.
Captures enclosing the viewport, such as multiline strings and comments, are
clipped to it. Smaller nested captures override enclosing ones, so escapes retain
their own style inside strings; later query patterns break equal-size ties.
Returned spans are sorted and disjoint. Up to 128 distinct ranges per buffer per frame are
requested and cached, with at most 4,096 captures per query. Extra ranges in very
tall viewports stay plain text. Cursor movement over cached ranges does not
reparse or rerun queries.

After an edit, cached colors follow surviving text until fresh highlights arrive,
so typing does not flash the viewport back to plain text. Only cached viewport
spans are remapped, using byte shifts outside the changed extent and exact
position maps inside it, including disjoint edits and grouped undo/redo. Typing
inside a colored token temporarily inherits its color; completely replaced
tokens lose their old color. Newline edits and horizontal clipping reuse
overlapping cached spans. These provisional colors never count as a completed
request and never make an old syntax tree eligible for structural commands.

New requests replace queued work and cooperatively cancel obsolete work. Results
must match the active request, document revision, and language session before
being applied. Resetting the same language also starts a new session and clears
cached colors. A current batch replaces provisional colors, including empty
results after a budget limit. Empty results are cached too, preventing a timeout
from causing a redraw loop. Missing edit history or exceeding the document size
limit clears the cache immediately.

Standalone `Editor` integrations remain synchronous by default. To use a worker,
enable `set_background_syntax`, call `begin_syntax_frame` before collecting ranges
with `syntax_highlights`, and take the aggregated `take_syntax_job` after drawing.
Run jobs through a persistent `SyntaxWorker` off the UI thread and deliver results
via `apply_syntax_result` on the owning thread. Its return value requests a redraw.

The grid paints syntax before selection and cursor overrides. UTF-8 byte offsets
are converted at line starts and advanced by whole grapheme byte lengths while
drawing, preserving wide cells, tabs, and horizontal clipping. The existing cell
diff detects color-only changes and emits nothing for identical frames.

The [file picker](pickers.md) reuses `vex_syntax` for all bundled languages. Its separate
preview worker uses the same filename/shebang detection, parses the bounded
displayed prefix, and returns text and highlight spans as one result. The UI
uses the same syntax palette, with no parser work or language-server startup.
Changing the selection or query, hiding the preview on resize, and closing the
picker cancel obsolete work; preview identities reject late text and colors.

## Initial limits

Parsing runs on the syntax worker with a cooperative 25 ms budget per attempt and a
2 MiB document ceiling. Each highlight query has a cooperative 2 ms budget and a
match/capture limit. These are work limits, not hard latency guarantees: callbacks
cannot interrupt every parser or predicate operation. Grammar initialization,
changed-region LF counting, and bounded capture resolution are also indivisible.

A size or budget limit produces plain text; incomplete worker results are never
published. Retained colors may briefly reflect the previous syntax while current
work is pending. A timed-out parse retries after an edit or an explicit `:language NAME`
reset. A timed-out query stays plain for that cached range until eviction, an edit,
or a language reset. Superseded requests can retry the same revision; cancellation
does not poison its cache. Editing, saving, and undo remain available.

General language injections, semantic tokens, user grammar loading, and theme
configuration are not implemented yet. Bundled queries supply syntactic
colors without symbol resolution. In incomplete code, Tree-sitter's error recovery
can produce different trees depending on editing history; valid code is checked
against a fresh parse, and node coordinates are checked for both cases.

Source tests cover incremental edits and grouped history, batched/multi-cursor
input, Unicode and line endings, chunk boundaries, nested and clipped captures,
cache bounds, cancellation/retry, stale completions, independent service delivery,
plain-text fallback, file detection, and terminal style precedence. The terminal
smoke test checks idle completion, editing, and undo colors in a real PTY.

## Adding a language

The shared registry is [`crates/vex_syntax/src/language.rs`](../crates/vex_syntax/src/language.rs).
Each `languages!` entry defines names/aliases, extensions, filenames, interpreters,
LSP language ID, indentation, comment delimiters, grammar, ordered highlight queries, and an optional server.
The macro generates language identities and lookup; query compilation is cached
once per language. Editor opening, Save As, `:language`, file previews, server
startup, and completion all use this registry.

To add another bundled language:

1. Add its Tree-sitter grammar to the workspace and `vex_syntax` dependencies.
2. Add a registry entry with its grammar, queries, and `indentation` defaults.
   Use `server: None` for
   syntax-only support, or provide the server executable, argument list,
   environment override, status label, and root markers.
3. Add representative source fixtures to the colocated syntax tests and check
   filename detection. The existing LSP transport and completion code need no
   language-specific branches.

The registry is compiled into Vex. Loading languages or server settings from a
user configuration file remains future work. These indentation settings control
Tab, explicit shifts, and formatting; they do not infer indentation from syntax.
