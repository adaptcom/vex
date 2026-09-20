# Key groups and file picker

Normal and select modes have named prefix groups: `g` (Goto), Space, `[` (Previous),
and `]` (Next). Pressing a prefix displays its available continuations, using the
same Rustdoc as command help. Completing a command or pressing an unbound key
leaves the group. Escape cancels a pending prefix/count while preserving the
editing mode; Escape with no pending input enters normal mode. Groups are
one-shot; sticky groups are not implemented yet.

The keymap still supports arbitrary nested sequences. After binding commands,
`Keymap::name_group(mode, prefix, title)` names a group for discovery. Unnamed
prefixes also show their continuations. Group metadata never changes command
dispatch or repeat counts. Keybindings remain Helix-inspired, without a
compatibility guarantee.

## Using the picker

Space-f or `:file_picker` opens a fuzzy file picker. Space-k invokes the existing
hover command. Both actions are documented functions and can be rebound.

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
document or start a language server. Opening another file requires saving current
changes first; an error leaves the picker open. Successful opens add the origin to the existing
Ctrl-o jump list. As with definition jumps, switching files currently reloads
from disk and creates fresh undo history.

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
and match positions. Additional providers can reuse this view for buffers,
commands, symbols, or diagnostics.

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
