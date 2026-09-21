# Key groups and pickers

Normal and select modes have named prefix groups: `g` (Goto), Space, `[` (Previous),
`]` (Next), and Ctrl-w / Space-w ([Window](windows.md)). Pressing a prefix displays its available continuations, using the
same Rustdoc as command help. Completing a command or pressing an unbound key
leaves the group. Escape cancels a pending prefix/count while preserving the
editing mode; Escape with no pending input enters normal mode. Groups are
one-shot by default; `Z` opens sticky view mode. Shortcut hints use the same
box-drawing borders and bold titles as the picker and completion menu, with
bold keys and concise summaries in columns when space permits. Full command
details stay available through `:help`. Small terminals indicate hidden entries;
the bottom status line and global command line stay visible below.

The keymap still supports arbitrary nested sequences. After binding commands,
`Keymap::name_group(mode, prefix, title)` names a group for discovery. Unnamed
prefixes also show their continuations. Group metadata never changes command
dispatch or repeat counts. Keybindings remain Helix-inspired, without a
compatibility guarantee.

## Using the picker

Space-f or `:file_picker` opens a fuzzy file picker. Space-' (`:last_picker`)
reopens the most recently closed file, buffer, jump, symbol, or search picker with its query,
caret, selected result, and scroll position. Accepting a result and cancelling
both retain the picker. Only one previous picker is retained, with its bounded
result list and preview; the full file index is released on the worker.

Reopening refreshes file/buffer results and previews. Cached rows remain visible
until the selected file is rediscovered or the scan finishes, so early partial
results cannot move the selection. Removed targets fall back to a current result.
Symbol pickers reuse their retained catalogs; editing a workspace-symbol query
issues a fresh request. A new session identity rejects results from before close.
Space-k invokes the existing hover command. These actions are documented functions
and can be rebound.

| Input | Behavior |
|---|---|
| Text / bracketed paste | Edit the query; paste cannot execute commands |
| Up / Ctrl-p / Shift-Tab | Previous result |
| Down / Ctrl-n / Tab | Next result |
| Page Up / Ctrl-u | Previous page |
| Page Down / Ctrl-d | Next page |
| Left / Right / Home / End | Move the query caret |
| Backspace / Delete | Edit whole graphemes |
| Enter | Open the selected file |
| Escape / Ctrl-c | Close the picker and retain the original document/view |

Matching uses each whitespace-separated query word as a subsequence; all words
must match, in any order. Uppercase in the query enables case-sensitive matching.
Otherwise matching uses simple Unicode lowercase comparison. Contiguous matches,
word boundaries, camel-case boundaries, and the basename receive higher scores.
Ties use path order. Matched characters are bold and highlighted at grapheme boundaries.
The selected row uses a bold `>` marker and the same foreground match highlights as
other rows, with the terminal's default background. Preview line markers use the
same bold foreground emphasis.
Accent folding, regex queries, and fuzzy-query operators are not supported.

Results appear as scanning proceeds. The picker keeps the best 512 entries and
shows the total match count; narrow the query to find entries beyond that list.
Query edits keep the most recent rows, match highlights, and preview visible
until new results arrive. An empty partial scan keeps those rows; a completed
empty result replaces them with the no-matches message. A small spinner in each
panel's title marks outstanding list or preview work. It advances every 100 ms
through the existing event loop and stops when work finishes or the picker closes.
Before the first result, the panel stays empty with its spinner.

Enter opens a current result even while discovery continues. Retained rows from
an older query cannot be accepted: Enter waits for the current ranking. Subsequent
editing keys stay queued until the file opens; Escape/Ctrl-c can cancel when next
in key order, and resize/focus events still work.

The picker floats in two independently bordered boxes: the query, results, and
footer on the left, and a preview on the right. The current document remains
visible around the boxes and through the gap between them. The preview has its
own title showing the displayed preview's path and uses its full interior height.
While another preview loads, the previous text and title remain together; the
new title, text, and syntax colors replace them at once. Narrow
terminals show only the file list; margins shrink on small terminals, and
terminals too small for results show a resize message.

The bottom status and command lines remain visible on short terminals too.
Long labels show an ellipsis and retain both ends of paths. If that would hide
every match, the label shows the matching context instead. Match colors keep their
original Unicode boundaries. The footer uses the terminal background and shows the
selected result number, with shorter controls on narrow terminals. Resizing keeps
the selection visible and fills the available result rows.

Previews reuse the editor's language registry, Tree-sitter grammars, highlight
queries, and syntax colors for all bundled languages. Reading, parsing, and querying all
run on the preview worker; the UI receives text and semantic spans together.
Unrecognized file types remain plain text. Preview reads are limited to 64 KiB and 200
lines; binary/non-UTF-8 files show a message. Parsing uses the displayed prefix
and the existing syntax time/capture limits, falling back to plain text if a
limit is reached. Tabs use four-column stops, and Unicode graphemes are clipped
without crossing the pane border.

Resizing to a narrow terminal cancels preview work. Previews never edit the
document or start a language server. The chosen file opens in the focused pane.
Opening another file retains the previous buffer, including unsaved edits and
undo history. Successful opens add the origin to the existing Ctrl-o jump list.
Loaded files reuse their buffers even when no pane displays them; an error leaves
the picker open.

## Buffer picker

Space-b (`:buffer_picker`) uses the same floating boxes and navigation keys to
choose a loaded buffer, including hidden files and scratch buffers.
`*` marks the current buffer and `+` marks unsaved edits. An empty query
orders entries by recent access; fuzzy scores take priority for a nonempty query.

The picker captures labels and identities once on opening. Query changes share
that catalog with the existing matching worker, keeping at most 512 results in
a bounded heap. Matching checks cancellation between candidates and computes
highlight positions only for retained results. The UI never scans document text
to build or filter the list.

Only the selected buffer supplies a shared rope snapshot to the preview worker.
Previews show unsaved text around its cursor, with syntax colors from its language
configuration; scratch buffers need no file on disk. Text is limited to 200 lines
and the existing byte cap. Buffers over 8 MiB get a bounded plain-text preview
without parsing the whole document. External reloads invalidate pending previews.
Accepting switches the current pane and records a Ctrl-o checkpoint; early Enter
waits for the current query before dispatching subsequent editing keys.

## Jump picker

Space-j (`:jumplist_picker`) lists saved locations from every pane, newest first
within each pane, following [Helix's jump picker](https://github.com/helix-editor/helix/blob/master/helix-term/src/commands.rs).
Rows show the path (or scratch name), primary cursor line, and selected
text. `*` marks the buffer that is current in the checkpoint's source pane.
Filtering matches paths and snippets. Enter restores the entire selection set
in the focused pane, including direction and primary selection, and records
the origin for Ctrl-o. Hidden unsaved buffers and scratch buffers work too.

Opening shares one rope snapshot per referenced buffer and existing selection
allocations. Labels are built once on the picker worker, reading at most 256
scalars across at most 32 ranges per checkpoint; truncated snippets end in `…`.
Queries reuse those labels and the buffer picker's bounded fuzzy ranker, keeping
at most 512 results. This bounds label work even for whole-file selections or
thousands of carets. Checkpoint identity comparisons do not traverse selections.
Previews use current unsaved snapshots and the same syntax/size limits as buffer
previews, centered around the saved primary cursor's line.

Space-' retains the query, selected checkpoint, and scroll position. Reopening
captures current snapshots; closed pickers release their captured document text.
External reloads refresh rows and previews. Enter waits for refreshed results
before releasing queued input, and revision checks reject stale destinations.
The worker remaps saved positions through edits, undo/redo, and reloads before
building snippets and preview lines. Checkpoint identities remain stable after
remapping, preserving the selected row on reopening. Remapping checks cancellation
between changes and selections; its cost grows with the intervening changes and
saved ranges, independently of the size of unchanged document text.

## Workspace text search

Space-/ (`:global_search`) searches file contents below the current working
directory, following [Helix's global-search behavior](https://github.com/helix-editor/helix/blob/master/helix-term/src/commands.rs).
The query is a regular expression with smart case and multiline anchors, using
the [rope regex engine](search.md). An empty query waits for input. Invalid
patterns leave the picker editable. The root stays fixed until the picker closes;
Space-' reopens it with the same root, query, and selected result.

Results show relative paths, line numbers, and short matching excerpts. The
preview uses syntax colors and scrolls to the match. Enter selects the whole
matching lines in the focused pane and records a Ctrl-o checkpoint. If results
are still pending, Enter starts the current query immediately and holds later
editing keys until a match opens or the query finishes. Normal typing waits
150 ms after the last query edit before starting work, including while idle.

The search includes unsaved text from loaded files, including hidden buffers.
Their shared rope snapshots take precedence over disk contents. Scratch buffers
have no path and are excluded. Discovery applies the same ignore and visibility
rules as the file picker. Files created during a search session are picked up
by closing and reopening the picker. Open-buffer revisions are checked before
accepting results; missing files and invalid destinations leave the picker open.

Accepting remembers the query in `/`, or a chosen register such as `"a<space>/`,
and makes it available to `n`/`N`. Clipboard registers use the existing background
clipboard transport. Escape/Ctrl-c cancels without changing the search register.

Discovery and regex scanning share the existing picker worker; preview work
uses its existing separate worker. Query changes cancel prior work and reuse
the file index. Only the first 512 matching line ranges in path/line order are
retained, with a total count. Search stops at 100,000 regex matches, 1 GiB of
source bytes, or a cooperative ten-second deadline; narrow the query when a
limit notice appears. Each file is limited to 128 MiB. Disk reads skip binary
(NUL-containing), invalid UTF-8, unreadable, and oversized files, reporting the
skip count. The picker limits query input to 1 KiB, including pasted text.
Filesystem calls and
regex compilation are not preemptible, but run off the input thread.

## Symbol pickers

Space-s (`:symbol_picker`) opens document symbols, and Space-S
(`:workspace_symbol_picker`) searches workspace symbols. Both work in normal and
select modes, following the [Helix bindings](https://docs.helix-editor.com/keymap.html#space-mode).
They use the current named file's language server, including rust-analyzer for
Rust, and report unavailable or unsupported services inside the picker.

The document picker requests an outline once, flattens nested symbols with their
container names, and filters names and containers locally using the existing
fuzzy matcher. Workspace typing cancels the previous request and waits 150 ms
before sending the current query. The server owns workspace search semantics and
ordering; the empty query requests its initial list. This searches the active
server's workspace, not a combined index of every configured language.

Both use the same two floating boxes, bold titles, selection marker, and navigation
keys as the file picker. Entries show symbol kind and line; workspace entries
also show the file path. The preview scrolls to the selected symbol, marks its
line number, and retains syntax coloring. Open files use their current buffer
snapshots, including unsaved text; other files are read on the preview worker.
Symbol previews read at most 8 MiB and display at most 200 lines / 64 KiB around
the destination. Parsing uses the existing syntax budgets and may fall back to
plain text.

Enter jumps in the focused pane and records the origin for Ctrl-o. Unsaved text
is protected by the same rules as definition jumps. Escape or Ctrl-c dismisses
without moving the cursor; focus loss also cancels symbol requests. Early Enter
waits for the current results before subsequent editing keys are dispatched.
Query generations, document revisions, and request identities reject stale
server replies, rankings, and previews. Unsupported requests, errors, and empty
results release any pending acceptance.

Symbol parsing accepts both hierarchical `DocumentSymbol` and flat
`SymbolInformation` results, following the
[document-symbol protocol](https://github.com/microsoft/language-server-protocol/blob/gh-pages/_specifications/lsp/3.17/language/documentSymbol.md).
Workspace symbols require a resolved local-file location; lazy resolution is
not advertised, as allowed by the
[workspace-symbol protocol](https://github.com/microsoft/language-server-protocol/blob/gh-pages/_specifications/lsp/3.17/workspace/symbol.md).
At most 16,384 entries / 4 MiB are retained and 512 matches displayed. Limited
responses are marked; narrower workspace queries can request different results.
Ranking shares the file-picker worker, and previews share its preview worker.

## Discovery and visibility

For the file picker, the root is the nearest enclosing Git repository of the current file, otherwise
the outermost enclosing Cargo project, otherwise the working directory captured
when opening the picker. Scratch buffers start discovery from the working
directory. No language server or Git executable is required.

Vex owns its directory walker, ignore parser, and fuzzy scorer; the picker uses
existing workspace crates without new external dependencies. Project `.gitignore`
files are applied relative to their own directories, with deeper files and later
rules taking precedence. Supported
patterns include `*`, `?`, character ranges/negated classes, ASCII POSIX classes,
`**`, anchored paths, directory-only rules, escaped characters, comments, trailing
spaces, and negation. Ignored directories are pruned, so a child cannot be
re-included without also including its parent.

`.git/info/exclude` supplies lower-priority rules; `.ignore` in each directory
supplies rules after that directory's `.gitignore`. When starting in a
subdirectory, parent ignore files are inherited up to the nearest repository
root (or 64 ancestors), with patterns relative to their original directories.
Git's index, global Git configuration, and linked-worktree exclusion files are
not read. Patterns are case sensitive.
Consequently tracked files
that match ignore patterns are also hidden. Ignore files must be UTF-8 and at
most 64 KiB each; read errors appear in the picker footer.

Dot-prefixed entries and symlinks are excluded. Symlink directories are never
followed. Discovery stops at 64 directory levels, 200,000 files, or 64 MiB of
stored path/label bytes, with a notice when a limit is hit. The memory total also
includes index entries, parsed rules, and matcher scratch storage. The index is
retained while the picker is open, including across query changes; reopening
rescans to observe filesystem and ignore-rule changes.

## Implementation and checks

`picker::Picker<T>` owns query editing, selection, scrolling, and cell-grid
drawing independently of the entry payload. The file provider supplies paths
and match positions. Buffer and symbol providers share the same view and worker;
additional providers can reuse them for commands or diagnostics.

Directory traversal and matching run on one persistent worker, interleaving
small scan/ranking batches. Preview reads and syntax use a second worker. Both
reuse the existing latest-job mailbox and cancellation tokens, with separate result slots
in the shared terminal event queue. Enumeration stays in the worker's index;
only bounded result snapshots cross to the UI. Closing releases the index on
the worker. Session, query, and preview identities reject obsolete completions.

The fuzzy scorer uses dynamic programming and reusable scratch storage;
highlight traces are computed only for retained results. Ignore matching uses
iterative dynamic programming, a fast path for literal patterns, and reusable
scratch buffers. It checks cancellation between rules and pattern tokens.
Filesystem calls and individual candidate scoring are not preemptible. Opening
the chosen file remains synchronous, like existing definition navigation.

Unit/property tests stay alongside their Rust source. An optional comparison
test checks project ignore behavior against Git; Git is used only for that test.

```sh
cargo test --workspace --locked
cargo test -p vex_term --locked project_ignore_results_agree_with_git -- --ignored
cargo build --release -p vex_term --locked
python3 tools/picker_smoke.py
python3 tools/workspace_search_smoke.py
```

The PTY check covers hints, Unicode queries, floating borders, highlighted
previews, resizing, cancellation, early Enter followed by edit/save, jump-back,
unsaved buffers, and terminal cleanup. The workspace search check also covers
idle debounce expiry, regex errors, matching-line selection, retained searches,
and early acceptance before queued edits.
