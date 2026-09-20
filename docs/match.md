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

Examples: `miwd` deletes a word, `mawc` changes a word with its adjacent spaces,
and `2mip` selects two paragraphs. Word objects ignore counts. Whitespace-only
lines containing spaces/tabs remain part of a paragraph; empty lines separate
paragraphs. A cursor on the last separator before another paragraph selects that
following paragraph. At the end of the document, counted paragraph selection can
reach back to a preceding paragraph when no following paragraph exists.

These commands replace each selection with a forward range at its cursor,
preserving normal/select mode and the primary selection through any merges.
Word objects on whitespace or EOF produce an empty selection. Unicode grapheme
clusters remain intact, including combining marks and joined emoji.

The terminal runs scans through the ordered background selection worker over a
shared rope snapshot. Following editing keys wait for the result; Escape/Ctrl-c
can cancel a pending scan. Scans check cancellation between graphemes or lines;
individual grapheme-boundary lookups and final selection normalization remain
nonpreemptible. Revision, mode, and selection checks reject stale results.

Delimiter textobjects, bracket matching, and surround commands remain the next
parts of match mode tracked in [TODO.md](../TODO.md).
