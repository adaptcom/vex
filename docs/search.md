# Regex search and selections

The bindings follow [Helix](https://docs.helix-editor.com/keymap.html).
All commands are documented functions in the editor registry.

| Input | Behavior in normal and select modes |
|---|---|
| `/` / `?` | Preview a forward / backward regex search |
| `n` / `N` | Search forward / backward, independently of the last prompt direction |
| `s` | Select regex matches within the current selections |
| `S` | Split selections on regex matches, excluding the separators |
| `K` | Keep selections containing a regex match |
| `*` | Remember selected text as escaped alternatives, adding detected word boundaries |
| Enter | Accept the preview and remember its regex |
| Escape / Ctrl-c | Restore original selections, preferred columns, and viewport |
| `3n`, `2?`, etc. | Count successive matches, wrapping when necessary |

Queries support character classes, alternation, repetitions, Unicode properties,
and inline flags such as `(?i)` or `(?s)`. Lowercase queries ignore case; a query
containing an uppercase character is case-sensitive unless overridden by flags.
Multiline anchors are enabled, with CRLF handling based on the document's line
ending. Lookaround and backreferences are not supported by the regex engine.

Navigation starts after the primary selection for forward searches and before
it for backward searches. Normal mode replaces only the primary selection;
select mode adds each visited match and makes it primary. Selection direction is
preserved. Intermediate merges affect subsequent counted searches, just as with
repeated single commands. Matches do not join the end and start of the document.
For example, `/aba` from the first character of `ababa` finds the second `aba`;
`n` wraps to the first, then keeps wrapping to that first match because searches
start after the selected range.

`s`, `S`, and `K` restrict matching to each selected range while keeping document
context for anchors and word boundaries. Select/split results face forward and
start with the first result primary; filtering preserves retained directions and
makes the first retained selection primary. Match endpoints expand to whole
Unicode graphemes. Empty selections become block cursors except at EOF. `s`
excludes empty matches at a selection's exclusive right edge. `*` changes no
selections; use `n`/`N` afterward. The unbound documented `search_selection` and
`remove_selections` commands provide literal selection search without boundaries
and inverse filtering, ready for the later Alt bindings.

Typing or backspacing always previews from the selections saved when the prompt
opened. An empty, invalid, or unmatched query restores those selections. Enter
reuses the latest submitted query for an empty prompt (or cancels when there is no
history); invalid or unmatched prompts remain editable and preserve
the previous accepted search. Cursor movement within an invalid prompt retains
its error. Prompt editing follows grapheme boundaries; paste strips control
characters and cannot submit the query.

Search changes neither document text nor revision, dirty state, or undo history.
Beginning a search closes a typing undo group. Accepted queries are reusable after
edits and undo; matches are computed against the current snapshot.

Accepted queries are stored in [registers](registers.md). `/` is the default;
`"a/` uses `a`. `n`/`N` read the last active search register across buffers,
and `"an`/`"aN` override it for one command. Changing a register's contents
changes the next search. `*` writes to the chosen register and makes it active;
selection prompts `s`/`S`/`K` write their queries without changing the active
register. Ctrl-r followed by a register name inserts its first fragment into
the search prompt and updates the preview.

## Worker integration

Regex compilation, matching, selection splitting/filtering, `*`, and `C` scans
run on the shared search worker. Typing a newer query cancels the old request.
Each buffer caches its last compiled query by immutable text identity and line
ending mode, so ordinary `n`/`N` reuse it without recompiling or comparing long
strings. A changed query compiles on the worker. Searching with `".n` also
captures the first selected range there, checking the 64 KiB limit before copying.
Enter can arrive before a result; subsequent editing keys wait for completion.
Resize/focus events continue to work. Escape or Ctrl-c cancels when it is the next
queued key, preserving the order of preceding edits.

Standalone editors default to synchronous execution. Frontends enable
`Editor::set_background_search(true)`, take `SearchJob`s with `take_search_job`,
and deliver worker results through `apply_search_result`. Each job owns a shared
rope snapshot, query, and selections. Results apply only if the request token,
document identity, revision, mode, and selections still match. Moving away and
back, or editing then undoing, still invalidates pending work.

Frontends use `Editor::search_prompt()` to choose `/`, `?`, `select:`, `split:`,
or `keep:` labels, and `update_search()` to preview input. `search_status()`
distinguishes empty, pending, matched, missing, and invalid input; `search_error()`
provides the error. `search_waiting()` identifies work whose result is required
before subsequent input. The terminal owns viewport restoration separately.

## Performance and limits

`vex_core::regex::Regex` compiles with regex-automata, already used by the syntax
dependencies. Vex drives its automata over rope chunks without flattening the
buffer. Bounded DFAs handle common patterns; a prioritized NFA simulation handles
Unicode word boundaries and other DFA fallbacks. Scratch space depends on the
pattern, not file size. Inline tests compare both paths against the library's
flat-string matcher, including chunk boundaries and Unicode assertions.

Reverse navigation scans nearby lines first when the compiled pattern cannot
consume LF. Patterns that can span lines require a forward scan of the prefix
to preserve regex match precedence. Single-selection navigation retains at most
512 reverse matches; large counts use another pass and arithmetic wrapping.
Reverse matches inside graphemes and zero-width matches use stepwise navigation
with constant-space cycle detection, preserving the effect of grapheme expansion.
Multiple-selection navigation applies intermediate merges in an ordered map,
with cycle detection for huge counts. It does not sort or copy the entire set
for each visited match.

Queries are limited to 64 KiB, NFA construction to 8 MiB, and DFA construction to
separate bounded budgets. Regex operations producing more than 100,000 selections
fail without applying partial results. Compilation is not preemptible but runs
on the worker; scanning checks cancellation at byte/state intervals and between
matches. Individual grapheme lookups, allocation, and final selection normalization
are not preemptible. Missing queries, cross-line reverse searches, long lines,
and very large selection sets can still delay results. See
[measured costs](performance.md#rope-regex-engine).

Up/Ctrl-p and Down/Ctrl-n recall session prompt history. Highlighting every
visible occurrence remains future work.
