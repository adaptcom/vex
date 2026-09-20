# Window mode

Press `Ctrl-w` or `Space w` in normal/select mode, followed by a window command.
The bindings follow [Helix's window mode](https://docs.helix-editor.com/keymap.html#window-mode).
The prefix shows command help; Escape cancels it without changing editing mode.
Each command is an ordinary documented function in `vex_editor`, with layout
and file operations handled by the terminal frontend.

| Following key | Action |
|---|---|
| `v`, Ctrl-v | Split vertically; focus a shared view on the right |
| `s`, Ctrl-s | Split horizontally; focus a shared view below |
| `w`, Ctrl-w | Focus the next window in layout order |
| `h j k l`, arrows, Ctrl-h/j/k/l | Focus left/down/up/right |
| `H J K L` | Swap the current window left/down/up/right |
| `q`, Ctrl-q | Close the current window; exit if it is the last |
| `o`, Ctrl-o | Keep only the current window |
| `f`, `F` | Open selected filenames in horizontal/vertical splits |

Counts before the prefix repeat focus, rotation, and swap operations. Split and
close commands operate once. Outside this prefix, Ctrl-s records a jump checkpoint
in normal/select mode or an undo checkpoint in insert mode. Use `:w` to save and
`:q` to close a pane.

Each pane has independent selections, editing mode, preferred columns, and scroll
position. Views of the same file share text, undo history, language settings,
syntax caches, and the savepoint. Edits map other views' selections through the
actual changed ranges, including undo/redo and multi-cursor edits. Focusing a
different pane ends the current typing group. Half-page and Page Up/Down movements
use the focused pane's dimensions.

The command/search prompt and message line appear once, across the bottom of the
terminal. Each pane's status is embedded in a thin grey border on the normal
terminal background, with bold labels in all panes and dim grey labels in
inactive panes. Horizontal splits meet at that status border, with no extra row.
The border connects to vertical pane dividers with box-drawing junctions. Long
paths are shortened from the left, and a selection count appears only for
multiple selections. Only the
focused pane owns the terminal cursor. Completion and hover stay inside that
pane. The active cursor reverses the text's foreground/background colors,
including syntax colors. Unfocused cursors use dim grey text with no block
background. The file
picker floats over the complete terminal. Resizing too small to
fit the layout temporarily shows the focused pane alone and restores the splits
when space returns. There are at most 16 windows, each at least 12 columns by
3 rows; a split that would exceed these limits leaves the layout unchanged.
Split branches divide their available space equally. Adjustable split ratios,
mouse focus, and mouse resizing are not implemented.

## Jump history

Ctrl-s in normal/select mode records the full current selection set. Ctrl-o
(`jump_backward`) moves backward and Ctrl-i (`jump_forward`) moves forward;
both accept counts, following the [Helix keymap](https://docs.helix-editor.com/keymap.html#movement).
Tab also moves forward in normal/select mode because legacy terminals send the
same byte for Tab and Ctrl-i. Insert-mode Tab still inserts a tab, and picker
Tab still navigates results. `jump_back` remains an unbound command alias.

Each pane has its own history, capped at 32 entries. A backward jump saves the
live return location before leaving the newest entry, so Ctrl-i can return to
it. Saving a new checkpoint after going backward discards the old forward
branch. Consecutive identical checkpoints are deduplicated, and a backward
jump skips a checkpoint equal to the current selection. Out-of-range counts
leave the location unchanged.

History retains document identities and shared immutable selection sets;
it neither copies text nor retains undo snapshots. Hidden buffers and scratch
buffers are valid destinations. Closing a buffer removes its checkpoints from
every pane and adjusts the history position. Navigation retains normal/select
mode, selection direction, and the primary selection. Language services are
not needed. Saved scalar positions currently normalize to valid grapheme/text
bounds when revisited; remapping them through intervening edits remains on TODO.md.

## Different files

`:vsplit [PATH]` and `:hsplit [PATH]` open a file in a new pane, or duplicate the
current view when no path is given. Paths are literal, relative to the working
directory, and may contain spaces. Alternatively, split first and use `Space f`
to pick a different file in the new pane.

Window-mode `f`/`F` treats each selection as a filename. A single-character cursor
expands to the surrounding filename token. These paths are relative to the
current file's directory, or the working directory for scratch buffers. Explicit
selections support filenames containing spaces. Files must exist; all selected
files and required splits are checked before opening any. Shell expansion and
`file:line:column` parsing are not supported.

Opening a file that is already loaded reuses its buffer and undo history,
including unsaved edits. Switching files retains the old buffer, even when no
pane displays it. Each pane restores its own saved selections and scroll position
when returning to a buffer. Switching moves ownership without copying the text.

Buffer bindings work in normal/select mode and follow Helix:

| Key | Action |
|---|---|
| `Space b` | Pick a loaded buffer, including scratch and hidden buffers |
| `ga` | Return to the previous buffer accessed in this pane; repeat to toggle |
| `gn`, `gp` | Next/previous in opening order; counts wrap around |
| `gm` | Return to the last other buffer modified in this pane |
| `gf` | Open selected filenames in this pane; all selected files remain loaded |

`gf` uses the same relative-path rules and validation as window-mode `f`/`F`,
without creating splits. Up to 16 selected paths are accepted. All loads are
validated before switching; Ctrl-o returns to the origin, including scratch
buffers. `:bn`/`:bp` are aliases for next/previous navigation. Plain next/previous
lookups use the ordered buffer map; counts reduce modulo the number of buffers.

`:bc` (`:buffer-close`) releases the current buffer from all panes; unsaved edits
require `:bc!`. Panes showing it switch to another retained buffer. Closing the
last buffer creates an empty scratch buffer and keeps the editor running.

`:q` closes the focused pane, retaining its buffers. Closing the last pane checks
all buffers for unsaved text, including hidden buffers; `:q!` skips that check.
`:only` keeps the focused pane and retains buffers from the other panes;
`:only!` has the same retention behavior. `:qa` quits all panes, and `:qa!` permits
discarding all unsaved text. `:wq` saves and closes the focused pane.
Saving one shared buffer updates every view's modified indicator. Saving to a
path held by a different open buffer is rejected, even with `:w!`.

Syntax highlighting runs for every visible buffer on the existing background
worker, retaining a parser per visible buffer and delivering a complete batch of
results through the event queue. Search and language-service requests belong to
the focused pane and are canceled when focus changes. Language services still
maintain one active document session: switching to a different file changes that
session; switching between views of the same file keeps it. Concurrent LSP
sessions for multiple buffers remain future work.

Hidden buffers retain text, undo history, saved views, and cached colors, but
receive no syntax jobs, Git gutter jobs, or disk polls. Frame work visits only
visible panes; loaded-buffer matching and previews use the picker workers.
