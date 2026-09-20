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

Tests use private file-backed helpers and never access the desktop clipboard.
See [performance measurements](performance.md) for the scheduling cost.
