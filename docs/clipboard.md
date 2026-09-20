# System clipboard

Vex follows the [Helix clipboard bindings](https://docs.helix-editor.com/keymap.html#space-mode)
in normal and select mode:

| Key | Action |
|---|---|
| `Space-y` | Copy all selections |
| `Space-Y` | Copy only the primary selection |
| `Space-p` | Paste after selections |
| `Space-P` | Paste before selections |
| `Space-R` | Replace selections |

Copy leaves the internal `y`/`p` register unchanged. Multiple selections are
written to the clipboard in document order, separated by the platform's newline.
Their boundaries are retained in memory. A subsequent clipboard read must match
all those bytes exactly to reuse the separate fragments; changed clipboard text
is treated as one fragment for every destination. Extra destination selections
repeat the last fragment. Copying only the primary selection always produces one
fragment. These operations leave select mode.

Paste honors counts, preserves the primary selection, and uses the destination
buffer's line endings. Newline-terminated fragments paste above/below the selected
lines; replacement uses the exact selected ranges. Each paste is one undo step.
Counts do not affect copying. Clipboard reads preserve whitespace, Unicode, and
trailing newlines rather than trimming them.

The `+` [register](registers.md) uses this same clipboard and fragment cache.
Use `"+y`, `"+p`/`P`/`R`, `"+d`/`c`, or insert-mode Ctrl-r `+`. `*` addresses
the separate primary clipboard on Wayland or X11, with its own cache. It requires
wl-clipboard or xclip/xsel and a running display server; it does not silently
redirect to the system clipboard when primary selection is unavailable.

Clipboard cuts prepare both the copy and deletion on the worker, then delete
only after the write succeeds and the destination still matches. `"+c` enters
insert mode at that point, and the cut plus subsequent typing is one undo step.
Ctrl-r inserts at carets, ignoring linewise placement, and stays in insert mode.
`.` replays the logical clipboard command and waits for its current result.

Ctrl-r also inserts the first clipboard fragment into prompts/picker queries,
without submitting or opening anything. Control filtering happens on the worker.
Command/search prompt reads are limited to 64 KiB of filtered text; picker
queries retain their 1 KiB limit. A replaced prompt, moved prompt cursor, changed
query, or reopened picker invalidates an older read. Escape cancels a pending
read first, leaving the prompt/picker open.

`"+n`/`N` reads a query, then hands it to the regex worker. `"+/`, `"+?`, and
`"+*` write the accepted/derived pattern and make `+` active for subsequent
`n`/`N` only after success. Selection prompts can write queries without changing
the active register. `*` supports the same operations. These worker handoffs keep
following editing keys queued until the complete command finishes.

Vex uses these command helpers without an additional clipboard dependency:

| Environment | Helpers |
|---|---|
| tmux | `tmux load-buffer -w` / `tmux save-buffer` |
| macOS | `pbcopy` / `pbpaste` |
| Termux | `termux-clipboard-set` / `termux-clipboard-get` |
| Wayland | `wl-copy` / `wl-paste` from wl-clipboard |
| X11 | `xclip`, falling back to `xsel` |
| Windows / WSL | `win32yank.exe`, falling back to PowerShell clipboard commands |

Selection follows that order where the relevant environment and helpers are
available. Under tmux, paste reads the tmux buffer. Without a supported provider,
the command reports an error and leaves the document intact. The current provider
is detected on the worker's first request and reused for the session.

Copy capture, clipboard I/O, line-ending conversion, and paste transaction
preparation run on one background worker. Requests share immutable rope snapshots;
only selected text is copied, and the fragment cache retains no source document.
Ordinary input waits in order for completion, so a queued paste cannot overtake
a copy and a queued edit/save cannot overtake a paste. Resize and other background
results remain live. Escape or Ctrl-c cancels when next in input order; a copy
already accepted by the OS may still have changed its clipboard.

Late paste results cannot edit a changed document, revision, view, mode, or
selection. Helper failures, invalid UTF-8, cancellation, and timeouts release
queued input without applying an edit. Processes have a two-second deadline;
copy/read data is limited to 128 MiB, with a 256 MiB conservative bound for
counted paste text across destinations, including line-ending conversion space.
The main thread applies the completed transaction and normalizes selections;
large rope edits, allocation, and Unicode boundary work are not preemptible.
Clipboard reads during replay suspend runnable playback, allowing the event loop
to wait for a completion instead of polling with a zero timeout. Cancellation
returns to normal mode and preserves the completed prefix as an undoable group.

Tests use private file-backed helpers and never access the desktop clipboard.
See [performance measurements](performance.md) for the scheduling cost.
