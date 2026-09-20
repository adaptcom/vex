# Match mode

Press `m` in normal/select mode to see available match commands. `mi` selects
inside an object and `ma` selects around it, following
[Helix textobjects](https://docs.helix-editor.com/textobjects.html). Escape or
Ctrl-c cancels either prefix without changing the selections or editing mode.

| Object | Inside | Around |
|---|---|---|
| `w` | Word or punctuation run at the cursor | Include following horizontal whitespace, or preceding whitespace if none follows |
| `W` | Non-whitespace WORD at the cursor | Include adjacent horizontal whitespace using the same rule |
| `p` | Paragraph, including its line endings | Also include its following empty lines |
| A delimiter, e.g. `(`, `]`, `"` | Contents of its enclosing pair | Include the delimiters |
| `m` | Contents of the closest enclosing pair | Include that pair; repeating `mam` expands outward |

Examples: `miwd` deletes a word, `mawc` changes a word with its adjacent spaces,
and `2mip` selects two paragraphs. Word objects ignore counts. Whitespace-only
lines containing spaces/tabs remain part of a paragraph; empty lines separate
paragraphs. A cursor on the last separator before another paragraph selects that
following paragraph. At the end of the document, counted paragraph selection can
reach back to a preceding paragraph when no following paragraph exists.

Word and paragraph commands replace each selection with a forward range at its
cursor, preserving normal/select mode and the primary selection through any merges.
Word objects on whitespace or EOF produce an empty selection. Unicode grapheme
clusters remain intact, including combining marks and joined emoji.

The terminal runs scans through the ordered background selection worker over a
shared rope snapshot. Following editing keys wait for the result; Escape/Ctrl-c
can cancel a pending scan. Scans check cancellation between graphemes, lines, or
delimiters; individual grapheme-boundary lookups and final selection normalization remain
nonpreemptible. Revision, mode, and selection checks reject stale results.

Delimiter objects preserve selection direction and leave ranges unchanged when
no pair exists. Either bracket identifies the same pair. Explicit pair counts
find outer delimiters from the cursor; a delimiter directly under the cursor
stays fixed while the other side moves outward. The closest-pair object considers
the whole selection. Plain text balances bracket kinds and requires the cursor
inside symmetric quotes; a cursor directly on a quote needs syntax to resolve it.

`mm` moves to the matching delimiter, or extends to it in select mode; counts
are ignored. With syntax, it can also move from inside a scope to its closing
delimiter. Without syntax, the cursor must be on a bracket. Syntax matching uses
revision-matched trees shared by the highlight and selection workers. A cold
request parses on the worker under the existing 25 ms/2 MiB limits. Failed parses
are remembered for that revision; unavailable syntax falls back to plain text.
Counts traverse nested syntax ancestors once. Plain-text scans visit each
direction at most once per bracket kind, using fixed-size nesting counters.

## Adding surrounds

`ms<char>` wraps every selection in a delimiter pair, selects the complete
result, and returns to normal mode. For example, `miwms)` wraps a word in
parentheses. Either opening or closing bracket chooses the same pair. Supported
brackets are `()`, `[]`, `{}`, `<>`, `‘’`, `“”`, `«»`, `「」`, and `（）`.
Other characters repeat on both sides, so `ms"` adds quotes and `msm` adds a
literal `m` to each side. Enter adds the buffer's line ending on each side.

Counts are ignored when adding surrounds, following [Helix's command behavior](https://github.com/helix-editor/helix/blob/master/helix-term/src/commands.rs).
The operation preserves selection direction and primary selection, leaves yank
registers unchanged, and makes one undo step. Adjacent selections and empty
cursors receive separate pairs. Only the inserted delimiters enter edit strings;
selected contents remain in rope storage, even for a whole-buffer selection.

Surround replacement/deletion remain tracked in [TODO.md](../TODO.md).
