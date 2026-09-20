# Key groups and pickers

Normal and select modes have named prefix groups: `g` (Goto), Space, `[` (Previous),
`]` (Next), and Ctrl-w / Space-w ([Window](windows.md)). Pressing a prefix displays its available continuations, using the
same Rustdoc as command help. Completing a command or pressing an unbound key
leaves the group. Escape cancels a pending prefix/count while preserving the
editing mode; Escape with no pending input enters normal mode. Groups are
one-shot; sticky groups are not implemented yet. Shortcut hints use the same
box-drawing borders and bold titles as the picker and completion menu, with
plain-text entries inside and the global command line left visible below.

The keymap still supports arbitrary nested sequences. After binding commands,
`Keymap::name_group(mode, prefix, title)` names a group for discovery. Unnamed
prefixes also show their continuations. Group metadata never changes command
dispatch or repeat counts. Keybindings remain Helix-inspired, without a
compatibility guarantee.

## Using the picker

Space-f or `:file_picker` opens a fuzzy file picker. Space-' (`:last_picker`)
reopens the most recently closed file, buffer, or symbol picker with its query,
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
Ties use path order. Matched characters are highlighted at grapheme boundaries.
Accent folding, regex queries, and fuzzy-query operators are not supported.

Results appear as scanning proceeds. The picker keeps the best 512 entries and
shows the total match count; narrow the query to find entries beyond that list.
Enter opens the highlighted result even while discovery continues. If the current
query has no results yet, Enter waits for its ranking to finish. Subsequent
editing keys stay queued until the file opens; Escape/Ctrl-c can cancel when next
in key order, and resize/focus events still work.

The picker floats in two independently bordered boxes: the query, results, and
footer on the left, and a preview on the right. The current document remains
visible around the boxes and through the gap between them. The preview has its
own title showing the selected path and uses its full interior height. Narrow
terminals show only the file list; margins shrink on small terminals, and
terminals too small for results show a resize message.

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
choose a loaded buffer. Entries include hidden files, scratch buffers, and commit
drafts. `*` marks the current buffer and `+` marks unsaved edits. An empty query
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

Both use the same two floating boxes, bold titles, grey selection, and navigation
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

The root is the nearest enclosing Git repository of the current file, otherwise
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

`.git/info/exclude` at the chosen root supplies lower-priority rules; `.ignore`
in each directory supplies rules after that directory's `.gitignore`. Parent
directory rules above the chosen root, Git's index, global Git configuration,
and linked-worktree exclusion files are not read. Patterns are case sensitive.
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
```

The PTY check covers hints, Unicode queries, floating borders, highlighted
previews, resizing, cancellation, early Enter followed by edit/save, jump-back,
unsaved buffers, and terminal cleanup.
