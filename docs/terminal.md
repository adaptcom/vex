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
| `i`, `a` | Insert before/after the selection |
| `v` | Enter select mode |
| `d`, `c` | Delete/change the selection |
| `x` | Select lines |
| `u`, `U` | Undo/redo |
| Escape | Cancel pending input and enter normal mode |
| `:` in normal/select mode | Open the command prompt |
| Ctrl-s, Ctrl-q | Save / quit with an unsaved-change check |
| Ctrl-c | Cancel the prompt or return to normal mode |

See the [editing command reference](commands.md) for exact movement semantics.
Enter in insert mode follows the loaded file's first line ending (LF, CRLF, or
CR). Tab inserts a literal tab, displayed at the editor's configured tab stops.
Bracketed paste in insert mode is a separate undo step, preserving the pasted
bytes. Pasting in normal/select mode shows a message to enter insert mode. Pasting
into the prompt removes control characters and never submits a command.

Consecutive typing (including Enter and Tab) shares one undo group. Movement,
mode/selection changes, and save attempts end it. `c` and its following replacement
text share a group; `d`, Backspace, Delete, and each paste get their own steps.
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
| `:quit`, `:q` | Quit if the buffer has no unsaved changes |
| `:quit!`, `:q!` | Discard unsaved changes and quit |
| `:write-quit [PATH]`, `:wq [PATH]`, `:x [PATH]` | Save, then quit only if saving succeeds |
| `:help [COMMAND]`, `:h [COMMAND]` | Show help or a command's documentation |
| `:language [rust/text/auto]`, `:lang [...]` | Show or set the syntax language |
| `:move_word_forward`, etc. | Invoke an editing command by its registered name |

The remaining text after a file command is a literal path, including internal
spaces; quoting, shell expansion, and escapes are not interpreted. Leading and
trailing whitespace is trimmed. Arrow keys, Home/End, Backspace, and Delete edit
the prompt at grapheme boundaries. Escape cancels it.

File commands are also ordinary documented functions. A declaration macro uses
each function's Rustdoc for the runtime registry, so `:help write` shares its
documentation with `app::write_file`.

Rust files (`.rs`) enable Tree-sitter syntax highlighting automatically; other
files start as plain text. Detection follows successful Save As operations.
`:language rust` enables highlighting in scratch buffers; an explicit language
choice persists across saves until `:language auto`. Selections and cursor styles
take precedence over syntax colors. See [syntax architecture and limits](syntax.md).

## Rendering and input

`vex_term` uses Crossterm for events, terminal modes, and escape-sequence
encoding. It has no UI framework or Ratatui dependency:

- `input` converts terminal keys to editor keys and owns prompt editing.
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
  sleeps while idle. Focus gain invalidates the grid; resizing rebuilds it.

The primary cursor changes shape between block and bar. Other cursors and
selected ranges use cell styles. The status line shows mode, unsaved changes
(`[+]`), pending keys/count, path, primary position, and selection count. The
bottom line shows messages, errors, or the prompt.

Movement and rendering share display-width conventions in `vex_core::display`.
Control characters and standalone zero-width clusters appear as replacement
cells; file contents and paths cannot inject terminal escape sequences. A wide
grapheme clipped at a viewport edge is represented by spaces. Actual emoji and
ambiguous-width display still depends on the terminal's Unicode support.

Cleanup restores raw mode, cursor visibility/shape, line wrapping, paste/focus
reporting, and the main screen on ordinary exit, I/O failure, and panic. The panic
hook performs cleanup before printing the diagnostic. Unix SIGINT, SIGTERM, and
SIGHUP handlers notify the event loop to unwind through the same cleanup.

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

This version has one buffer and view. File I/O is synchronous. Rendering stops
at the right edge. Cached display columns avoid repeatedly scanning hidden line
prefixes; cold queries and reindexing after an early edit can still be expensive.
Syntax currently supports Rust, with size and work budgets for synchronous parsing.
Clipboard integration, mouse input, search, and LSP are not implemented.

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
cargo bench -p vex_term --bench rendering --locked -- --noplot
cargo run --release -p vex_term --example long_lines --locked -- 10 100
```

The Unix smoke script launches the real executable with a controlling
pseudo-terminal. It exercises Rust syntax colors, paste, CRLF, grouped undo/redo,
insert-mode savepoints, dirty quit, save, resize,
focus, seeking/editing at the end of a 1 MiB line, and terminal cleanup. It also
runs the panic-cleanup source test under a
PTY; this test returns early in the normal noninteractive Cargo test run.
See [performance measurements](performance.md) for benchmark scope and results.
