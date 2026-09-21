# Git gutter

Git integration is read-only: background diffs and gutter markers. Use an
external Git client for repository status, staging, and commits.

Named, tracked UTF-8 files show changes against their committed contents in
`HEAD`. The comparison uses the current buffer, including unsaved edits, so both
staged and unstaged changes appear. Saving or staging keeps the markers; committing
the same text clears them after the baseline refreshes.

The marker shapes and placement follow
[Helix's gutter](https://github.com/helix-editor/helix/blob/master/helix-view/src/gutter.rs):

| Marker | Color | Meaning |
|---|---|---|
| `▍` | Green | Added lines |
| `▍` | Yellow | Modified lines |
| `▔` | Red | Removed lines immediately above this boundary |

A deletion at the end of the file appears on its trailing empty line. Deleting
all text puts the overline on the empty buffer's first line. Diagnostics, line
numbers, and Git markers have separate gutter columns. Panes narrower than 12
cells hide diagnostics and Git markers to leave room for text; below 8 cells,
line numbers also disappear.

No configuration is required when `git` is on `PATH`. Scratch buffers, files
outside repositories, and paths absent from `HEAD` have no markers. This includes
untracked files and repositories before their first commit. Linked worktrees
are supported. Missing Git and unavailable baselines quietly leave an empty gutter.

## Background work

`vex_git` owns a cached committed baseline and a bounded Myers line diff for each
visible buffer. It uses read-only Git plumbing for repository discovery and blob
loading, then compares rope snapshots in memory. Editing and drawing never run
Git. No external dependencies were added; the crate uses the workspace's existing
snapshot, cancellation, and temporary-file support.

Edits debounce for 150 ms, with a 500 ms maximum wait during continuous typing.
The gutter keeps its last accepted markers visible during this delay and while
the worker runs, then replaces them together when a current result arrives.
Marker positions can briefly reflect the previous diff after inserting or deleting
lines. Changing a buffer's path immediately hides the old path's markers.
Opening a file, saving, and terminal focus gain request a baseline refresh.
An idle deadline also probes repositories every two seconds after completed work,
so commits and branch switches are noticed without a keypress. A slow pending
batch is allowed to finish before the next periodic probe. HEAD is queried once
per repository in a batch; unchanged blobs and document revisions reuse cached
data. Ordinary edits use the cached baseline without starting Git.

One persistent worker uses the existing cancellable, replaceable mailbox and a
dedicated result slot in the event queue. Requests and results batch visible
buffers; panes sharing a buffer share its diff. Request generation, document
identity, revision, and path checks reject stale results. Rendering only looks
up markers for visible lines. Hunks retain old and new line ranges for future
navigation and reset commands.

## Limits

Each Git command has a two-second deadline and bounded output. Cancellation and
shutdown kill and reap the current child. Git commands do not run shell pipelines,
write the index or working tree, or fetch missing objects.

Baseline and buffer limits are 8 MiB and 200,000 rope lines. The diff search has a
100 ms budget, four million work steps, and a maximum edit distance of 1,024;
simple whole-range additions/deletions/replacements have a direct fast path.
Exceeding a limit clears markers for that revision rather than showing estimated
hunks. Binary/NUL-containing and non-UTF-8 baselines are skipped. CRLF and LF are
equivalent for comparison; a missing final newline remains a change. Git attribute
filters and custom working-tree encodings are not applied in this initial version.

Unit and property tests live in the source files. `tools/git_smoke.py` checks real
terminal markers and refresh after an external commit with no additional input.
