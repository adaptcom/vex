# Registers

Linewise paste follows the register's text: `p`/`P` use whole-line placement when
any fragment ends in a line ending. Selecting an unterminated final line with
`x` or `X` does not invent a newline or attach a linewise flag. For example,
yanking `last` from that final line and pasting after `a` in `ab` produces
`alastb`. This follows the text-based test in
[Helix's paste implementation](https://github.com/helix-editor/helix/blob/master/helix-term/src/commands.rs).
It also keeps named and clipboard registers consistent when only text crosses
the clipboard boundary. `R` still replaces the exact selected ranges.

In normal or select mode, `"<register>` chooses a register for the next command:
`"ay` copies selections to `a`, `"ap` pastes them after the selections, and
`"aR` replaces the selections. `d` and `c` also respect the chosen register.
Without a prefix these commands use `"`, the shared last-yanked-text register.
Choosing `a` leaves `"` unchanged. Names are case-sensitive: `a` and `A` are
independent, following [Helix registers](https://docs.helix-editor.com/registers.html).

A register name is one literal printable Unicode character, including a digit,
space, or punctuation. Counts may appear before or after the prefix, for example
`3"ap` and `"a3p`. A completed command consumes the choice, including movements
and commands that fail. Pending key groups retain it. Escape or Ctrl-c cancels
the choice without changing the document or leaving select mode.

In insert mode, Ctrl-r followed by the name inserts the register at each caret
and stays in insert mode. Fragments pair with selections in document order;
extra destinations repeat the last fragment. This insertion always happens at
the caret, even if a fragment ends in a newline, and forms its own undo step.
Line endings follow the destination buffer. `.` records the register name and
reads its current contents when repeating the insert.

Ctrl-r also works in command/search prompts and picker queries. It inserts only
the first fragment at the prompt cursor, removes control characters, and never
submits or opens a result. The picker retains its query-size limit. Escape
cancels a pending Ctrl-r prefix; another Escape closes the prompt or picker.
The boxed register helper shows short previews while collecting the name.

| Register | Contents |
|---|---|
| `"` | Last yanked or cut text |
| `_` | Discard writes; pasting does nothing |
| `#` | Current selection indices, starting at 1; read-only |
| `.` | Current selection contents; read-only |
| `%` | Current file name, or `[scratch]`; read-only |
| `/` | Last accepted search, unless a different register was chosen |
| `:` | Last submitted nonempty colon command |
| `+` | System clipboard, shared with the Space clipboard commands |
| `*` | Primary clipboard where a Wayland/X11 provider is available |

`"a/` or `"a?` stores an accepted search in `a`. `"an` and `"aN` search with
the first fragment of `a` as a regex. Plain `n`/`N` use the register from the
last accepted `/`/`?` or `*`, including after switching buffers. Updating that
register changes subsequent searches; a one-command `"an` override leaves the
active search register unchanged. `s`/`S`/`K` store accepted queries in the chosen
register (default `/`) without changing which register is active. Cancelled or
invalid prompts leave stored queries intact.

Ctrl-r `%`, `/`, or `:` inserts the file name, saved search, or command just
like an ordinary register. The file-name value follows successful save-as and
buffer switching. `+` and `*` use the [background clipboard service](clipboard.md)
for all reads/writes, including `d`/`c`, insert/prompt Ctrl-r, and searches.
The primary clipboard has its own fragment cache and requires a supported display
server; it does not alias the system clipboard on platforms without one.

Cuts apply only after copying succeeds. Insert replay waits for each clipboard
operation and reads the current contents, preserving undo grouping and input
order. Command/search prompt reads are limited to 64 KiB after stripping controls;
picker reads use the existing 1 KiB query limit. Search reads retain literal
control characters and obey the regex engine's 64 KiB pattern limit.

Stored fragments are immutable and shared across buffers without text copies.
Undo/redo and closing the source buffer do not rewind or discard registers.
The helper snapshots at most 64 stored names and 48 characters of each first
fragment when opened; drawing reuses these previews. Reading a stored register
clones an `Arc`, and copying into `_` never captures selected text. Actual yanks
and paste preparation still cost work proportional to the selected/inserted text.

Commands are ordinary documented functions (`select_register`, `insert_register`,
`yank`, etc.). Embedders can read or write `RegisterValues` through
`Editor::register` and `Editor::set_register`, and can pass an explicit register
in `CommandContext`. Use `Editor::with_session` to share registers across buffers.
The frontend supplies the file name with `Editor::set_display_name`; the core
does not resolve paths or perform filesystem I/O when reading `%`.
