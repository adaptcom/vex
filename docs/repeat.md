# Repeating an insert

`.` in normal or select mode repeats the last completed insert session at the
current selections. `3.` runs the whole session three times. This follows
[Helix's repeat-last-insert binding](https://docs.helix-editor.com/keymap.html).

The session starts with its entry command (`i`, `a`, `I`, `A`, `o`, `O`, or `c`)
and ends when leaving insert mode. Typing, literal paste, cursor movements,
deletions, Enter, and explicit insert undo checkpoints are recorded as documented
command calls with their arguments. Original command counts are retained: after
`2o`, typing, and Escape, `.` opens two lines again. `3.` repeats that complete
two-line operation three times, using the current selections each time.

Normal-mode edits and undo/redo leave this history intact. An empty insert
session replaces it too. The most recently completed session is shared across
buffers and panes, and survives closing its source buffer. Independent editor
sessions have independent histories. Rebinding keys does not change an existing
recording because it contains commands rather than terminal events.

Accepted completion records its main replacement relative to the insertion
caret. Replay applies the saved replacement at each current caret without
requesting completion again. Additional edits such as imports are applied only
when accepting the original completion. Invalid or overlapping replacement ranges
stop replay before that replacement is applied; earlier completed actions remain
undoable. Language-service and application-UI requests are not recorded.

A repeat, including its count, forms one undo step unless it contains explicit
Ctrl-s insert checkpoints. Escape or Ctrl-c stops a pending repeat and returns
to normal mode. The completed prefix remains undoable and does not overwrite
the recording. Ordinary queued editing keys wait until playback finishes;
cancellation follows their input order. Resize and background results remain live.

The recorder stores a shared immutable program and one text pool, retaining no
document snapshot and allocating no separate string for every typed character.
Individual input boundaries are preserved because a new insertion can combine
with an existing Unicode grapheme. The terminal advances at most 64 actions per
batch and yields after four milliseconds between actions. Individual commands
keep their existing complexity limits, so this is not a hard latency bound for
a large paste or a movement over a very long line. See the
[performance measurements](performance.md).

Frontends create buffers with `Editor::with_session(document, editor.session())`
to share registers and repeat history. They enable `set_deferred_repeat(true)`,
call `advance_repeat` between event batches, and defer ordinary input while
`repeat_pending()` is true. Standalone command calls are synchronous by default.
