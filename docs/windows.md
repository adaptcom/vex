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
close commands operate once. Ctrl-s and Ctrl-q outside this prefix keep their
save/close behavior.

Each pane has independent selections, editing mode, preferred columns, and scroll
position. Views of the same file share text, undo history, language settings,
syntax caches, and the savepoint. Edits map other views' selections through the
actual changed ranges, including undo/redo and multi-cursor edits. Focusing a
different pane ends the current typing group. Half-page and Page Up/Down movements
use the focused pane's dimensions.

The command/search prompt and message line appear once, across the bottom of the
terminal. Each pane has only its own status line. Horizontal splits meet at the
upper pane's status line, with no extra divider row. Grey vertical separators
divide side-by-side panes. The focused status line is brighter, and only the
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

Opening a file that is already displayed reuses its buffer and undo history,
including unsaved edits. Opening another file in a pane is allowed while its old
buffer is dirty if another pane still displays that buffer. The last view of
unsaved text is protected. Buffers with no views are released; this is not yet a
hidden-buffer list.

`:q` closes the focused pane; `:q!` permits discarding its last unsaved view.
`:only` keeps the focused pane and refuses to discard other unsaved buffers;
`:only!` explicitly discards them. `:qa` quits all panes, and `:qa!` permits
discarding all unsaved text. `:wq` saves and closes the focused pane.
Saving one shared buffer updates every view's modified indicator. Saving to a
path held by a different open buffer is rejected, even with `:w!`.

Syntax highlighting runs for every visible buffer on the existing background
worker, retaining a parser per open buffer and delivering a complete batch of
results through the event queue. Search and language-service requests belong to
the focused pane and are canceled when focus changes. Language services still
maintain one active document session: switching to a different file changes that
session; switching between views of the same file keeps it. Concurrent LSP
sessions for multiple buffers remain future work.
