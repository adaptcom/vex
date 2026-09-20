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
| `W B E` | Whitespace-separated WORD movements; punctuation stays in each WORD |
| `f<char>`, `F<char>` | Select through the next/previous character, crossing lines |
| `t<char>`, `T<char>` | Select until just before/after the next/previous character |
| Digits before a command | Repeat count |
| `gg`, `ge` | Start/end of document; a count before `gg` goes to that line |
| `<count>G` | Go to the one-based line number; uncounted `G` does nothing |
| `gs` | First non-whitespace grapheme on the current line |
| `<count>g\|` | Go to the one-based grapheme column, defaulting to 1 |
| Home, End | Start/end of logical line |
| Page Up, Page Down | Move by roughly one viewport of logical lines |
| Ctrl-u, Ctrl-d | Move cursors and scroll half a page up/down in normal/select mode; counts multiply the distance |
| `i`, `a` | Insert before/after the selection |
| `I`, `A` | Insert at the first non-whitespace character / end of each cursor's line |
| `r<char>` | Replace each selected grapheme with a character; Enter and Tab also work |
| `>`, `<` | Indent/unindent selected lines, using language defaults and repeat counts |
| `J` | Join selected lines, or join the next line for a single-line selection |
| Space-c, Ctrl-c in normal/select mode | Toggle comments using the language's delimiters |
| Space-C | Toggle block comments; use line comments for languages that only support those |
| `[Space`, `]Space` | Add empty lines above/below selections, retaining normal/select mode |
| `o`, `O` | Open lines below/above selections and enter insert mode |
| Enter, Ctrl-j in insert mode | Split the line and copy indentation before the caret |
| Backspace, Ctrl-h in insert mode | Delete the preceding grapheme |
| Delete, Ctrl-d in insert mode | Delete the next grapheme |
| Ctrl-w in insert mode | Delete the preceding word |
| Ctrl-u, Ctrl-k in insert mode | Delete toward the start/end of the line |
| Ctrl-x in insert mode | Request completion immediately; automatic completion also opens after typing |
| `v` | Enter select mode |
| `y` | Yank selections to the internal register shared across files and panes |
| `p`, `P` | Paste after/before selections; newline-terminated yanks paste below/above selected lines |
| `R` | Replace selections with yanked text without overwriting the register |
| `d`, `c` | Cut selections to the internal register, then delete/change them |
| `%` | Select the entire document |
| `;` | Collapse each selection to its displayed cursor |
| `,` | Keep only the primary selection |
| `C` | Copy selections onto following lines at the same display columns; counts add copies |
| `x` | Expand to whole lines, then extend below on repeated presses; accepts counts |
| `X` | Expand to line boundaries, preserving direction |
| `_` | Trim whitespace from selection edges |
| `u`, `U` | Undo/redo |
| `gd`, Ctrl-o | Go to definition / return to the previous jump checkpoint |
| `]d`, `[d` | Next/previous diagnostic, with counts and wrapping |
| Space-f, Space-k | Open file picker / show hover |
| Space-s, Space-S | Open document / workspace symbol picker |
| Space-g | Open the [repository status view](git-status.md) |
| Ctrl-w, Space-w | Enter [window mode](windows.md) to split, focus, swap, and close panes |
| Escape | Cancel a prefix/picker/prompt; otherwise enter normal mode |
| `:` in normal/select mode | Open the command prompt |
| Ctrl-s | Save a selection checkpoint in normal/select mode; split the typing undo group in insert mode |
| Ctrl-c | Cancel prompts, completion, or pending key sequences; toggle comments in normal/select mode |

See the [editing command reference](commands.md) for exact movement semantics.

Character finds wait for one key, retaining the count (for example, `3f:`).
Digits, spaces, and `:` are literal targets while waiting. Enter finds a logical
line ending, including CRLF; Tab finds a tab. Escape or Ctrl-c cancels without
leaving select mode. Other non-character keys cancel the pending find. Each
selection searches independently without wrapping, and stays unchanged if the
requested occurrence is missing. Normal mode selects the traversed span; select
mode keeps its anchor. Till motions skip adjacent matches so repetition advances.
Matches within a combining/emoji cluster select the whole grapheme.

`gg` and `G` clamp oversized line counts to the last content line, excluding the
empty EOF line after a trailing newline. `ge` retains Vex's existing EOF-boundary
behavior. `g|` counts graphemes rather than terminal cells: a tab or wide glyph
counts once. Oversized columns stop at the line-ending boundary. `gs` keeps
whitespace-only lines unchanged. These motions run synchronously over the rope;
bounded/cancellable scans for unusually large inputs are tracked in TODO.md.
Prefix groups display their available commands. The [file picker](pickers.md)
supports background discovery and fuzzy matching, Unicode query editing, a preview
on wide terminals, and Ctrl-o return jumps. It protects unsaved changes when opening
another file.

Selection controls work in normal and select mode, retaining the current mode.
`%` replaces all selections with one range covering the document; `;` keeps every
cursor but collapses its range to one whole grapheme (or an empty cursor at EOF).
`,` keeps the primary range with its direction intact.

`x` first expands each range to the full logical lines it touches, including line
endings, and faces forward. Once aligned, each further press adds a line below.
Counts include the initial alignment, so `3x` from a character selects three
lines, while `3x` on already selected lines adds three more. `X` aligns without
adding lines and preserves direction. Both merge overlapping ranges, retain the
primary, and exclude the next line when a selection ends exactly at its start.

`C` adds a copy of each selection on the following line, preserving both endpoint
columns and direction. It skips lines whose endpoints would fall beyond the line
or inside a tab/wide glyph. Multi-line ranges advance by their height, so copying
a two-line range starts two lines lower. Counts add that many fitting copies per
original selection. Overlaps merge, and the primary follows its last copy.
Normal/select mode is retained, and text, registers, and undo history are unchanged.
`copy_selection_on_prev_line` provides the same operation upward as a named command;
its Alt binding is part of the remaining modifier work.

Selection copying runs on the existing search worker over a shared rope snapshot.
The status shows `selecting...`; subsequent edit keys wait for completion. Resize
and Escape/Ctrl-c cancellation still work. Cancellation is checked between candidate lines;
individual cold display-column lookups and final selection normalization remain
indivisible. Stale results cannot replace changed selections or text.

`_` trims Unicode whitespace without editing the document or splitting graphemes.
Whitespace-only selections are removed; if the primary is removed, the last
surviving range becomes primary. If no ranges survive, one cursor remains at the
original primary's displayed position. Selection controls leave the yank register
and undo/redo entries intact, and end any ongoing typing group.

Enter in insert mode and `o`/`O` follow the loaded file's first line ending (LF,
CRLF, or CR), retained even after deleting all line breaks. They copy leading tabs
and spaces literally, without language-specific indentation rules. Enter copies
only indentation before the caret, so splitting leading whitespace preserves the
remaining indentation without duplicating it. Counts such as `3o` create three
lines with a caret on each. Tab inserts a literal tab, displayed at the editor's
configured tab stops.

`I` and `A` enter insert mode on each selection's cursor line; duplicate carets
on a line merge. `I` uses the first non-whitespace grapheme, or the line start
when blank. `A` stops before the line ending. They do not infer indentation on
empty lines. Typing then forms one undo step, as with `i` and `a`.

`r` waits for a character, replacing each selected grapheme once (a combining
cluster, emoji sequence, or CRLF pair counts as one). Enter uses the buffer's
line ending, Tab inserts a literal tab, and Escape/Ctrl-c cancels without edits.
Replacement retains the ranges and their directions, returns to normal mode,
and leaves the yank register unchanged. An empty EOF selection does nothing.

`>` and `<` edit each touched line once, including when multiple selections
share it. An endpoint at the next line's start excludes that line. Indenting
skips blank lines; spaces advance to the next indentation boundary, with counts
adding further levels. Unindent removes leading spaces/tabs up to the counted
width, consuming whole tabs at the configured tab stops. These commands return
to normal mode, retain the selected text, and create one undo step.
See [language indentation defaults](syntax.md) for widths and API overrides.

`J` removes line endings within the selected lines and the following indentation.
A single-line selection joins the next line. It adds a space where needed,
preserves existing spacing, and avoids adding trailing whitespace at EOF.
Comment markers are preserved literally; stripping repeated comment prefixes
is future work. Shared joins happen once. `J` retains normal/select mode.

`[Space` and `]Space` add counted blank lines outside selections, preserving
the original selected text and mode. Shared insertion points are handled once;
each command is one undo step and uses the buffer's line endings. Below an
unterminated last line, the first inserted break starts the new empty line.
Counts apply to `>`/`<` and blank-line insertion; `I`, `A`, `r`, and `J` ignore
them, following the selection-based behavior of the
[Helix keymap](https://docs.helix-editor.com/keymap.html).

The internal yank register lives for the session, independently of buffer and undo
history. `y`, `p`, `P`, and `R` return to normal mode; pastes select the inserted
text. Multiple fragments pair with destinations in document order, repeating the
last fragment for extra destinations and ignoring unused fragments. Counts such
as `3p` repeat each fragment within one undo step. `R` replaces the exact selected
ranges, and neither replacement nor undo/redo changes the saved register.

If any yanked fragment ends in LF, CRLF, or CR, `p`/`P` paste at line boundaries.
A final line without a line ending is copied as characterwise text. Pasting below
an unterminated destination line adds a separator. Register pastes normalize line
endings to the destination's convention while retaining the original register
bytes. Backspace/Delete in insert mode do not overwrite the register. System
clipboard integration and named registers are planned in [TODO](../TODO.md).

Bracketed paste in insert mode is a separate undo step, preserving the pasted
bytes. Pasting in normal/select mode shows a message to enter insert mode. Pasting
into the prompt removes control characters and never submits a command.

Consecutive typing (including Enter, Tab, and insert-mode deletion) shares one undo group. Movement,
mode/selection changes, and save attempts end it. `c` and its following replacement
text share a group, as do `o`/`O` and their following typing; `d` and each paste
get their own steps. Typing after those actions starts a new group.
For example, `ihello<Esc>u` removes
the whole word, and `2u` undoes two groups. Undo/redo restores every selection,
adjusted to the current mode; it does not switch modes.

Ctrl-s in insert mode ends the current undo group without saving or leaving insert
mode. Typing more and undoing once returns to that checkpoint. In normal/select
mode, Ctrl-s records all selections for Ctrl-o to return to, including in scratch
buffers. Save with `:w`; a failed save separates typing but leaves the savepoint
unchanged.

Ctrl-w deletes back by words. Ctrl-u deletes to the first non-whitespace character,
then to the line start on another press, then joins with the preceding line.
Ctrl-k deletes to the line end, then removes the line ending on another press.
These commands handle CRLF as one unit, merge overlapping deletions, and leave
the yank register unchanged.

## Command prompt

`/` searches forward and `?` backward in normal or select mode. Typing previews
literal matches; Enter accepts and Escape or Ctrl-c restores the original
selections and viewport. `n` repeats in the accepted direction, `N` reverses it,
and both wrap and accept counts. See [search semantics and limits](search.md).

| Command | Action |
|---|---|
| `:write [PATH]`, `:w [PATH]` | Save to the current or supplied path |
| `:write! [PATH]`, `:w! [PATH]` | Allow replacing an existing destination or external edits |
| `:reload[!]` | Reload the current file as one undo step; `!` accepts disk contents over unsaved edits |
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
selected by its executable override). Diagnostics appear in the gutter and status line; Space-k
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
  Git, file polling, and LSP services.
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

The primary cursor is a reversed block in normal/select mode and a terminal bar
in insert mode. The bar preserves the underlying text's syntax colors. Other
cursors and selected ranges use cell styles. The status line shows mode, unsaved
changes (`[+]`), pending keys/count, path, primary position, and a count for multiple
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
The repository status view has its own query worker and latest-result slot,
including lazy diffs and background syntax colors. See [repository status](git-status.md).
Stage, unstage, and commit use a separate ordered write worker and a reliable
FIFO of completion events. Commit drafts are normal editor buffers retained for
the session; status remains visible in the adjacent pane.

File polling has a separate worker and latest-result slot. Every two seconds,
the UI snapshots file-backed documents shown in visible panes, deduplicating
shared buffers. Only one batch runs at a time; metadata checks, reads, comparisons,
and reload edit preparation happen off the UI thread. Results validate the document,
revision, path, and save generation before applying. Hidden splits and documents
covered by a Git pane are excluded; temporary picker/help overlays do not hide
their underlying document from polling.

`BackgroundEvent` carries search, syntax, picker, preview, Git, and file-poll results. LSP has a separate typed
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

Closing the runtime wakes blocked producers, cancels search, syntax, picker, preview, file polling, and Git query jobs,
shuts down the language server, and joins owned threads before restoring terminal state. Input errors and worker
failures wake the main loop and unwind through cleanup. Input polling has a
50 ms shutdown check; the main inbox wait checks signal flags at most every
100 ms while idle. Cancellation is cooperative; service limits and remaining
synchronous work are documented in [search](search.md), [syntax](syntax.md), and
[language services](lsp.md), and [Git](git.md).
Git writes finish in order during teardown; quit commands report busy while a
write is pending, including long-running hooks and signing.

## Files and current limits

Loading and saving stream UTF-8. Saving writes a temporary sibling, flushes and
syncs it, then replaces the target atomically. Existing permissions are retained;
symlinks are followed and retained. By default, a save refuses to overwrite a
different existing file, a removed file, or contents changed since load/save.
The disk comparison runs on save, never during drawing. A failed write leaves
the buffer dirty and keeps the editor open.

Visible files are checked in the background every two seconds, even without
keyboard input. Unchanged metadata reuses cached text; changed files are read
as UTF-8 and compared with the last load/save. Clean buffers reload automatically,
keeping document identity, view modes, and undo history. Syntax, search, LSP,
and Git state follow the new document revision. Reloads replace the differing
middle region, preserve unchanged prefixes/suffixes, and map each view's selections
through the edit. A reload is one undo step; undoing it makes the buffer dirty,
and redoing it returns to the saved state.

If local edits are unsaved, disk changes produce a warning and leave the buffer
intact. `:reload!` explicitly accepts the disk version; `:w!` writes the local
version. Deletion, invalid UTF-8, and read errors also preserve the buffer.
Atomic replacement and files created after opening a new path are detected.
An explicit `:reload` reads immediately and refuses unsaved edits without `!`.
Metadata-only detection can miss edits on filesystems that do not report a changed
timestamp, size, or identity. Native filesystem notifications are tracked in TODO.md.

The dirty check compares shared rope identity in O(1); undoing to a savepoint
clears it. An independent edit that recreates identical bytes still counts as
modified. New files use the temporary file's owner-only permissions. Atomic
replacement creates a new inode, so other hard links, ownership, extended
attributes, and ACLs are not preserved. The content check is not a lock against
concurrent writers, and the parent directory is not synced for crash durability.

Split panes can show shared or different buffers; see [window mode](windows.md).
Buffers are retained while at least one pane displays them. Initial loads, explicit
reloads, and saves remain synchronous; automatic file checks run in the background. Rendering stops
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
