# Terminal interface

Build and run the `vex` executable with an optional UTF-8 file path:

```sh
cargo run --release -p vex_term --locked -- path/to/file
# Or build once:
cargo build --release -p vex_term --locked
target/release/vex
```

A missing file starts an empty buffer; it is created on save. Without a path,
Vex starts a scratch buffer. Standard input and output must be terminals.
`--help` and `--version` also work outside a terminal. Use `--` before a file
whose name starts with a dash.

## Controls

The editing commands use Vex's Helix-inspired default keymap. They remain named,
documented functions in `vex_editor`; the terminal layer translates input events
and calls that existing dispatcher.

| Key | Action |
|---|---|
| `h j k l`, arrows | Move the cursor; extend selections in select mode |
| `w b e` | Word movements |
| Digits before a command | Repeat count |
| `gg`, `ge` | Start/end of document |
| Home, End | Start/end of logical line |
| Page Up, Page Down | Move by roughly one viewport of logical lines |
| Ctrl-u, Ctrl-d | Move cursors and scroll half a page up/down in normal/select mode; counts multiply the distance |
| `i`, `a` | Insert before/after the selection |
| `o`, `O` | Open lines below/above selections and enter insert mode |
| Enter in insert mode | Split the line and copy indentation before the caret |
| Backspace, Ctrl-h in insert mode | Delete the preceding grapheme |
| Ctrl-x in insert mode | Request completion immediately; automatic completion also opens after typing |
| `v` | Enter select mode |
| `d`, `c` | Delete/change the selection |
| `x` | Select lines |
| `u`, `U` | Undo/redo |
| `K`, `gd`, Ctrl-o | Hover, go to definition, return from a definition jump |
| `]d`, `[d` | Next/previous diagnostic, with counts and wrapping |
| Space-f, Space-k | Open file picker / show hover |
| Space-s, Space-S | Open document / workspace symbol picker |
| Ctrl-w, Space-w | Enter [window mode](windows.md) to split, focus, swap, and close panes |
| Escape | Cancel a prefix/picker/prompt; otherwise enter normal mode |
| `:` in normal/select mode | Open the command prompt |
| Ctrl-s, Ctrl-q | Save / close the current pane with an unsaved-change check |
| Ctrl-c | Cancel the prompt or return to normal mode |

See the [editing command reference](commands.md) for exact movement semantics.
Prefix groups display their available commands. The [file picker](pickers.md)
supports background discovery and fuzzy matching, Unicode query editing, a preview
on wide terminals, and Ctrl-o return jumps. It protects unsaved changes when opening
another file.
Enter in insert mode and `o`/`O` follow the loaded file's first line ending (LF,
CRLF, or CR), retained even after deleting all line breaks. They copy leading tabs
and spaces literally, without language-specific indentation rules. Enter copies
only indentation before the caret, so splitting leading whitespace preserves the
remaining indentation without duplicating it. Counts such as `3o` create three
lines with a caret on each. Tab inserts a literal tab, displayed at the editor's
configured tab stops.
Bracketed paste in insert mode is a separate undo step, preserving the pasted
bytes. Pasting in normal/select mode shows a message to enter insert mode. Pasting
into the prompt removes control characters and never submits a command.

Consecutive typing (including Enter and Tab) shares one undo group. Movement,
mode/selection changes, and save attempts end it. `c` and its following replacement
text share a group, as do `o`/`O` and their following typing; `d`, Backspace,
Delete, and each paste get their own steps.
Typing after those actions starts a new group. For example, `ihello<Esc>u` removes
the whole word, and `2u` undoes two groups. Undo/redo restores every selection,
adjusted to the current mode; it does not switch modes.

Saving with Ctrl-s while still inserting closes the group. Typing more and undoing
once returns to the saved state and clears the modified indicator. A failed save
also separates typing but does not change the savepoint.

## Command prompt

`/` searches forward and `?` backward in normal or select mode. Typing previews
literal matches; Enter accepts and Escape or Ctrl-c restores the original
selections and viewport. `n` repeats in the accepted direction, `N` reverses it,
and both wrap and accept counts. See [search semantics and limits](search.md).

| Command | Action |
|---|---|
| `:write [PATH]`, `:w [PATH]` | Save to the current or supplied path |
| `:write! [PATH]`, `:w! [PATH]` | Allow replacing an existing destination or external edits |
| `:quit`, `:q` | Close the current pane; protect the last view of unsaved text |
| `:quit!`, `:q!` | Close the current pane, allowing unsaved text to be discarded |
| `:vsplit [PATH]`, `:hsplit [PATH]` | Split and optionally open a different file |
| `:only[!]` | Keep only the current pane |
| `:quit-all[!]`, `:qa[!]` | Quit all panes, with ! to discard unsaved text |
| `:write-quit [PATH]`, `:wq [PATH]`, `:x [PATH]` | Save, then close the current pane only if saving succeeds |
| `:help [COMMAND]`, `:h [COMMAND]` | Show help or a command's documentation |
| `:language [NAME/text/auto]`, `:lang [...]` | Show or set the language |
| `:lsp-restart` | Restart the configured language server for the current file |
| `:file_picker` | Open the fuzzy project file picker |
| `:move_word_forward`, etc. | Invoke an editing command by its registered name |

The remaining text after a file command is a literal path, including internal
spaces; quoting, shell expansion, and escapes are not interpreted. Leading and
trailing whitespace is trimmed. Arrow keys, Home/End, Backspace, and Delete edit
the prompt at grapheme boundaries. Escape cancels it.

File commands are also ordinary documented functions. A declaration macro uses
each function's Rustdoc for the runtime registry, so `:help write` shares its
documentation with `app::write_file`.

Rust, Markdown, Bash/shell, TypeScript/TSX, and JavaScript/JSX enable Tree-sitter
highlighting automatically. Detection uses filenames and shebangs and follows
successful Save As operations. `:language NAME` also works in scratch buffers; an explicit language
choice persists across saves until `:language auto`. Selections and cursor styles
take precedence over syntax colors. See [syntax architecture and limits](syntax.md).

Named files start their configured server when it is installed on `PATH` (or
selected by its executable override). Diagnostics appear in the gutter and status line; `K`
opens a hover panel. Definition jumps can open another file after saving pending
changes, and Ctrl-o returns to the origin. See [language services](lsp.md) for
setup, active-document service limits, and failure handling.

Tracked files show [Git changes against HEAD](git.md), including unsaved edits.
The gutter reserves separate cells for diagnostics and Git markers around the
line numbers. Added/modified lines use green/yellow `▍` bars; deletions use red
`▔` overlines at the following line boundary. Very narrow panes hide these cells.

## Rendering and input

`vex_term` uses Crossterm for events, terminal modes, and escape-sequence
encoding. It has no UI framework or Ratatui dependency:

- `input` converts terminal keys to editor keys and owns prompt editing.
- `events` combines terminal input and typed background completions in a wakeable
  inbox, with one input thread and independent search, syntax, picker, preview,
  Git, and LSP services.
- `app` combines editor state, key dispatch, file state, prompt, and viewport.
- `render` paints visible logical lines, selections, line numbers, status, and
  the prompt into a cell grid. It borrows rope slices where possible. Horizontal
  and vertical scrolling follow the primary cursor. The editor's shared layout
  cache locates the visible start of horizontally scrolled lines. There is no
  soft wrapping.
- `screen` reuses two grids, compares cells, and buffers changed runs into one
  write and flush. Identical frames emit no bytes. Wide glyphs reserve explicit
  continuation cells so replacing or clipping them does not leave stale text.
- `terminal` owns raw mode, alternate-screen lifetime, and bounded event batches.
  An event burst is limited to 128 events or 4 ms before another draw. The loop
  sleeps on the inbox while idle; terminal events and worker completions wake it.
  Focus gain invalidates the grid; resizing rebuilds it.

The primary cursor changes shape between block and bar. Other cursors and
selected ranges use cell styles. The status line shows mode, unsaved changes
(`[+]`), pending keys/count, path, primary position, and a count for multiple
selections. Labels sit within a thin grey border on the normal terminal
background; all labels are bold, with inactive labels dimmed. Long paths are
shortened from the left to keep the cursor position visible. Status borders join vertical pane dividers
with box-drawing junctions. The
bottom line shows messages, errors, or the prompt. This line is global across all
splits; each pane has only its own status line.

Movement and rendering share display-width conventions in `vex_core::display`.
Control characters and standalone zero-width clusters appear as replacement
cells; file contents and paths cannot inject terminal escape sequences. A wide
grapheme clipped at a viewport edge is represented by spaces. Actual emoji and
ambiguous-width display still depends on the terminal's Unicode support.

Cleanup restores raw mode, cursor visibility/shape, line wrapping, paste/focus
reporting, and the main screen on ordinary exit, I/O failure, and panic. The panic
hook performs cleanup before printing the diagnostic. Unix SIGINT, SIGTERM, and
SIGHUP handlers notify the event loop to unwind through the same cleanup.

## Event queue and background work

The main thread owns `App`, editing commands, and drawing. One producer thread
exclusively owns Crossterm's `poll` / `read` calls. Independent persistent workers
execute search and syntax jobs over immutable rope snapshots. All producers
notify the same inbox, so completions are handled even when no new key arrives.
The LSP service uses a small futures executor and dedicated pipe threads, without
Tokio. No service holds a shared mutable editor lock.

The inbox holds up to 256 terminal events and applies backpressure to the input
producer. Syntax requests and completions batch all visible buffers together. The worker
retains a parser per open buffer; replacing a batch reissues any outstanding
ranges for its other buffers. Search and syntax each have a separate latest-completion slot, so they
cannot overwrite each other and full input cannot block a needed result. Ready
services alternate to prevent starvation, with ordinary input taking a turn
between completions. Search and syntax use the same worker
mailbox implementation: at most one running and one replaceable pending job,
with cooperative cancellation of older work. Input keys stay in FIFO order.
File discovery/matching and previews use that mailbox implementation with two
additional latest-result slots. The file index stays on its worker, and each
completion is a bounded snapshot rather than a batch of new paths.

Git uses another latest-result slot and batches all open buffers together, with
cached HEAD baselines and hunks shared across views. Edit debounce and periodic
repository refresh use event-loop deadlines. See [Git architecture and limits](git.md).

`BackgroundEvent` carries search, syntax, picker, preview, and Git results. LSP has a separate typed
event variant and a FIFO of up to 128 events with producer backpressure, preserving
status, diagnostic, and response ordering. Service-specific validation and state
updates happen on the main thread. Each service chooses its queue policy;
adding a service does not imply it may discard messages.

An early Enter or `n` / `N` may depend on an unfinished search. The inbox then
holds later keys until the destination is ready, preserving sequences such as
`/cat<Enter>nd`. Resize/focus events can pass the held keys. Escape and Ctrl-c
cancel when next in key order, and ready completions take priority over further
ordinary input. This avoids applying edits to an unresolved search position.
Enter during picker matching uses the same input ordering until the selected
file opens. Closing a picker cancels its work and releases its index.

Closing the runtime wakes blocked producers, cancels search, syntax, picker, preview, and Git jobs,
shuts down the language server, and joins owned threads before restoring terminal state. Input errors and worker
failures wake the main loop and unwind through cleanup. Input polling has a
50 ms shutdown check; the main inbox wait checks signal flags at most every
100 ms while idle. Cancellation is cooperative; service limits and remaining
synchronous work are documented in [search](search.md), [syntax](syntax.md), and
[language services](lsp.md), and [Git](git.md).

## Files and current limits

Loading and saving stream UTF-8. Saving writes a temporary sibling, flushes and
syncs it, then replaces the target atomically. Existing permissions are retained;
symlinks are followed and retained. By default, a save refuses to overwrite a
different existing file, a removed file, or contents changed since load/save.
The disk comparison runs on save, never during drawing. A failed write leaves
the buffer dirty and keeps the editor open.

The dirty check compares shared rope identity in O(1); undoing to a savepoint
clears it. An independent edit that recreates identical bytes still counts as
modified. New files use the temporary file's owner-only permissions. Atomic
replacement creates a new inode, so other hard links, ownership, extended
attributes, and ACLs are not preserved. The content check is not a lock against
concurrent writers, and the parent directory is not synced for crash durability.

Split panes can show shared or different buffers; see [window mode](windows.md).
Buffers are retained while at least one pane displays them. File I/O is synchronous. Rendering stops
at the right edge. Cached display columns avoid repeatedly scanning hidden line
prefixes; cold queries and reindexing after an early edit can still be expensive.
Bundled syntax languages share size and work budgets on a background worker.
Language services include diagnostics, hover, definition navigation, and completion.
Clipboard integration, mouse input, and workspace edits are not implemented.

## Layout cache

Each editor owns a `vex_core::layout::LayoutCache`. Vertical motion and rendering
query it through `Editor::display_column` and `Editor::position_at_column`.
The existing pure motion functions remain available for callers without a cache.
Queries update derived layout data without editing text or selections.

The cache indexes logical lines on demand. Sparse checkpoints store a grapheme's
scalar offset and display column; eight recent positions per line accelerate
nearby movement. Queries seek to the closest known checkpoint and scan the gap.
ASCII runs advance in batches; Unicode clusters usually borrow directly from
rope chunks. Chunk boundaries do not become artificial grapheme boundaries.

At most 128 lines are retained, evicting the least recently used line. Each line
has at most 4,096 checkpoints, initially spaced by 256 scalars. Spacing grows as
needed for very long lines, keeping checkpoint storage around 8 MiB at the limit
on a 64-bit build, plus a small amount of bookkeeping. No old document snapshots
are kept by the cache.

Transactions and history retain the extent of changed text in old and new scalar
coordinates. Every editing command synchronizes the cache, including individual
text events within a terminal input batch. It keeps boundaries strictly before
the first edit and discards the affected suffix of that line. Entire unchanged
lines after the last edit move to their new positions without rescanning, using
line-relative offsets. Undo and redo reverse/reapply those extents. Changing tab
width or document identity clears the indexes; a cache that missed revisions also
clears conservatively. Line joins, CRLF changes, and newly combined graphemes are
covered by the same invalidation rules.

Initial indexing is still proportional to the needed prefix. Editing near the
start of a huge line invalidates its later checkpoints, and the next deep query
must rebuild them. Extremely large clusters and Unicode rules requiring long
lookbehind can also be expensive. The cache does not make every operation depend
only on viewport size; benchmarks report cold and warmed behavior separately.

## Validation

Unit/property tests are colocated with their Rust source. They cover input,
Unicode prompt editing, viewport geometry, wide-cell replacement, incremental
output, save failures, and application command dispatch.

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --release -p vex_term --locked
python3 tools/terminal_smoke.py
python3 tools/git_smoke.py # Requires Git.
python3 tools/lsp_smoke.py # Requires rust-analyzer and a Rust toolchain.
python3 tools/languages_smoke.py # Requires typescript-language-server and TypeScript.
cargo bench -p vex_term --bench rendering --locked -- --noplot
cargo run --release -p vex_term --example long_lines --locked -- 10 100
```

The Unix smoke script launches the real executable with a controlling
pseudo-terminal. It exercises Rust syntax colors, paste, CRLF, grouped undo/redo,
insert-mode savepoints, shared/different-file splits, window key sequences, dirty quit, save, resize,
focus, seeking/editing at the end of a 1 MiB line, and terminal cleanup. It also
runs the panic-cleanup source test under a
PTY; this test returns early in the normal noninteractive Cargo test run.
See [performance measurements](performance.md) for benchmark scope and results.
