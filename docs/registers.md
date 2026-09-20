# Registers

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

The `%`, `+`, and `*` special registers still need frontend integration and
currently return an error. Use [Space clipboard commands](clipboard.md) for
system clipboard access. Automatic search/command registers (`/` and `:`) and
searching with a chosen register are also tracked in [TODO](../TODO.md).

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
