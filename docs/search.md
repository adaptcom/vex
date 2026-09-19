# Literal search

| Input | Behavior in normal and select modes |
|---|---|
| `/` | Open a forward search prompt |
| `?` | Open a backward search prompt |
| Type or paste | Preview matches from the original selections |
| Enter | Accept the preview and remember the query and direction |
| Escape / Ctrl-c | Restore the original selections, preferred columns, and viewport |
| `n` | Select the next match in the accepted search direction |
| `N` | Select the next match in the opposite direction |
| `3n`, `2?`, etc. | Apply a match count, wrapping as needed |

Search is case-sensitive and literal: punctuation has no regex meaning, and
escape sequences are not interpreted. Prompt editing uses grapheme boundaries,
including arrows, Home/End, Backspace, and Delete. Paste cannot submit a search;
the prompt removes control characters, including line breaks. The core and editor
APIs can search literal line breaks and other valid UTF-8 text.

Each preview starts at the beginning of each original selection and includes
matches at that position. Typing more or backspacing reuses those same origins.
`n` and `N` move strictly past the current selection's first grapheme in their
direction, then wrap. Matches may overlap: searching `aba` in `ababa` visits
both occurrences. EOF never joins to the beginning to form a match.

Matches become selections, so `c` or `d` can immediately replace or delete them.
Endpoints expand to whole graphemes when a query matches inside a combining
sequence. Repeats skip additional matches starting in that same grapheme.
Backward searches produce backward selections. In select mode, search replaces
the ranges and preserves the mode. Each cursor searches independently; colliding
selections merge and the primary selection follows the usual normalization rules.
The primary match controls scrolling. Only the selected matches are highlighted.

An empty query restores the original selections; Enter then closes the prompt
without changing the previous accepted search. A missing query also restores the
original selections and displays `no matches` in the status line with a red
prompt prefix. Enter keeps that prompt open so it can be corrected. Cancellation
always retains the previous accepted query and direction. If the terminal was
resized, the restored viewport is adjusted as necessary to keep the cursor visible.

Search does not modify text, dirty state, revisions, or undo/redo history. Beginning
search separates typing groups. Accepted queries remain available after edits,
undo, and redo; each navigation reads the current document.

## API and implementation

`vex_core::search::Literal` compiles forward and reverse KMP failure tables. Its
iterator seeks into Ropey's bytes and streams across chunks, including matches
larger than a chunk. It copies only the query. Setup and retained memory are
linear in query bytes; scanning is linear in bytes visited and stops at the
requested match. Reverse search starts near its origin instead of scanning from
the beginning. The public iterator accepts a range of candidate starting bytes.

`vex_editor::commands` contains ordinary documented functions for `search_forward`,
`search_backward`, `search_update`, `search_accept`, `search_cancel`, `search_next`,
and `search_previous`. All appear in Rustdoc, runtime help, and the generated
[command reference](commands.md). `/`, `?`, `n`, and `N` use the normal keymap and
can be rebound.

The editor owns the accepted pattern and a preview snapshot of selections,
preferred columns, mode, and revision. Frontends observe `Editor::search_direction`,
edit their own prompt, send text via `Editor::update_search`, and call accept or
cancel. `Editor::search_status` distinguishes empty, matching, and missing queries.
The terminal saves the viewport separately. An intervening document revision or
mode change invalidates a preview; updating, accepting, or cancelling it returns
an error instead of installing stale coordinates.

Navigation does not allocate a list of every match. Counts larger than the number
of matches are reduced modulo that number after one traversal, followed by at
most one more traversal per original selection. Cursor-only prompt movements do
not rescan the document. Tests live alongside the core matcher, editor state,
and terminal event handling; property tests compare with flat-text match models.

## Current limits

Searching is synchronous. Nearby matches need only a small scan; a missing query
visits the entire buffer for each preview update. Large files, large match counts,
or many selections can delay input. There is no worker, cancellation budget,
regex mode, case folding, query history, or highlight of every visible occurrence
yet. See [measured search costs](performance.md#literal-search) for the current
baseline. Rendering and cold layout indexing have their own costs.
