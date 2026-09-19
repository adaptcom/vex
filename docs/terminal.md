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
Bracketed paste in insert mode is one edit, preserving the pasted bytes. Pasting
in normal/select mode shows a message to enter insert mode. Pasting into the
prompt removes control characters and never submits a command.

## Command prompt

| Command | Action |
|---|---|
| `:write [PATH]`, `:w [PATH]` | Save to the current or supplied path |
| `:write! [PATH]`, `:w! [PATH]` | Allow replacing an existing destination or external edits |
| `:quit`, `:q` | Quit if the buffer has no unsaved changes |
| `:quit!`, `:q!` | Discard unsaved changes and quit |
| `:write-quit [PATH]`, `:wq [PATH]`, `:x [PATH]` | Save, then quit only if saving succeeds |
| `:help [COMMAND]`, `:h [COMMAND]` | Show help or a command's documentation |
| `:move_word_forward`, etc. | Invoke an editing command by its registered name |

The remaining text after a file command is a literal path, including internal
spaces; quoting, shell expansion, and escapes are not interpreted. Leading and
trailing whitespace is trimmed. Arrow keys, Home/End, Backspace, and Delete edit
the prompt at grapheme boundaries. Escape cancels it.

File commands are also ordinary documented functions. A declaration macro uses
each function's Rustdoc for the runtime registry, so `:help write` shares its
documentation with `app::write_file`.

## Rendering and input

`vex_term` uses Crossterm for events, terminal modes, and escape-sequence
encoding. It has no UI framework or Ratatui dependency:

- `input` converts terminal keys to editor keys and owns prompt editing.
- `app` combines editor state, key dispatch, file state, prompt, and viewport.
- `render` paints visible logical lines, selections, line numbers, status, and
  the prompt into a cell grid. It borrows rope slices where possible. Horizontal
  and vertical scrolling follow the primary cursor; there is no soft wrapping.
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
at the right edge, but finding columns and horizontally scrolled text still
scans the hidden line prefix. A deep position in a huge single line can therefore
be slow; there is no long-line layout cache yet. Clipboard integration, mouse
input, search, syntax highlighting, and LSP are not implemented.

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
```

The Unix smoke script launches the real executable with a controlling
pseudo-terminal. It exercises paste, CRLF, undo/redo, dirty quit, save, resize,
focus, and terminal cleanup. It also runs the panic-cleanup source test under a
PTY; this test returns early in the normal noninteractive Cargo test run.
See [performance measurements](performance.md) for benchmark scope and results.
