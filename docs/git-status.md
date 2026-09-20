# Repository status

Press Space-g, or run `:git_status`, to open the current file's repository in the
active pane. Scratch buffers use the working directory; commit buffers use their
repository. The view provides status, expandable diffs, file navigation,
whole-file staging/unstaging, and commit-message buffers.

The document stays in its pane behind the status view, retaining unsaved text,
selections, undo history, and scrolling. `q` or Escape returns to it. Opening status
again restores the repository's selection and expanded sections. Ctrl-w/Space-w
window commands continue working; splitting from status opens the retained
document in the new pane, leaving Git visible beside it. Multiple panes showing
the same repository currently share status selection and fold state.

Status content fills the pane without an outer box. The bottom status rule shows
`Git · repository`, with the same bold title and muted inactive style as document
panes. Shortcut help remains a boxed popup above the status line.

## Contents and controls

The header shows branch or detached HEAD, the current commit and subject, upstream,
ahead/behind counts when available, and the stash count. Ahead/behind uses locally
known refs; opening status does not fetch. Repositories before their first commit
show their staged and untracked files normally.

Sections list conflicts, unstaged changes, staged changes, untracked files, and
unsaved named buffers. A file with changes in both the index and working tree
appears in both Git sections. Unsaved buffers have their own section: the diffs
show actual Git index/disk contents, while the editor gutter compares buffer text
against HEAD.

| Key | Action |
|---|---|
| j/k, arrows | Move by displayed row |
| Ctrl-u/d, Page Up/Down | Move half/full page |
| Home/End | First/last row |
| n/p | Next/previous section, file, or hunk |
| Tab | Expand/collapse a section, file, or hunk |
| Enter | Visit the selected file/change; toggle a section heading |
| r | Refresh visible repository views |
| s on a file | Stage the unstaged or untracked file from disk |
| u on a file | Unstage the staged file, preserving working files |
| c c | Open/resume a commit message beside status |
| ? | Show/hide the boxed key reference |
| q, Escape, Ctrl-c | Dismiss help, then return to the document |
| Ctrl-w, Space-w | Window commands |
| : | Command prompt; editing/saving the hidden document is disabled |

`:git_toggle`, `:git_visit`, `:git_refresh`, and `:git_close` are documented
commands, also available through `:help COMMAND`. Standard pane close/quit
commands retain the editor's unsaved-change checks.

## Staging and committing

`s`/`:git_stage` and `u`/`:git_unstage` operate on whole file rows. They support
additions, modifications, deletions, and unstaging renames, including before the
first commit. Paths are passed literally. Staging reads saved disk contents;
save an unsaved editor buffer first to include its edits. Hunk/line, section,
unsaved-buffer, and conflict rows do not perform index writes. Staging and
unstaging keep the cursor row and scroll position as files move between sections,
clamping to the remaining rows if the list shrinks.

`c c`/`:git_commit` opens a regular editor buffer in a vertical split, falling
back to a horizontal split on narrow terminals. It starts in insert mode and
supports normal movements, selection, undo, and window commands. The status line
identifies it as `Git commit · repository`.

| Commit-buffer key | Action |
|---|---|
| Ctrl-c Ctrl-c | Submit the message and current index (`:git_commit_submit`) |
| Ctrl-c Ctrl-k | Return to status, retaining the draft (`:git_commit_cancel`) |
| Escape | Normal mode; also cancels an unfinished Ctrl-c prefix |

Messages and undo history are retained in memory for the session, including when
a draft pane closes. Up to 16 repository drafts are retained. `:w` is unnecessary
and does not submit a commit. Quitting the editor ends the session and drops these
drafts. Blank messages and messages larger than 1 MiB are rejected.

Commits use `git commit --file` with a private temporary message file, using the
index at execution time. Git's configured hooks and signing run normally. A
failed commit keeps the draft and records Git's output above the status sections;
Home scrolls to it. The command line reports the outcome even after leaving the
Git pane. Success returns an unchanged, focused draft to status; the next
composition starts empty. Edits made while a commit runs remain in the draft,
and a completion does not switch focus from a different pane.

Git runs without interactive standard input; terminal-based hook prompts are not
hosted by the editor. Output retains the last 64 KiB and is subject to the status
view's row limit. Hunk staging, amend, conflict-resolution commands, and remote
operations are future additions.

Expanding a tracked file loads its unified diff in the background. Hunks can then
be folded independently. Renames display both paths; mode-only and binary changes
show Git's descriptive output. Conflicts display the working tree compared with
the index's ours side; this is a review view, not a conflict-resolution tool.

Enter reuses an already open buffer, including unsaved text. Opening another file
keeps the usual save protection. Deleted files and directories remain inspectable
in status but cannot be opened as documents. Navigation uses the new-side line
number, preferring matching text within 200 lines when staged or unsaved changes
have shifted it; otherwise it clamps the position to the current document.

## Syntax highlighting

Diff code uses the existing Tree-sitter language registry, with green/red `+`/`-`
markers and grey row selection. Old and new source files are parsed separately,
so removals retain their original syntax and multiline context can begin outside
the displayed hunk. Language detection follows each side's filename and shebang.
Selected rows use the editor's grey selection style over syntax colors.

Parsing and highlight queries run on the status worker. Source sides are limited
to 2 MiB and use the syntax engine's parse/query budgets plus a 500 ms cooperative
highlighting budget per batch. Unsupported languages, limits, and unavailable
source sides fall back to ordinary diff colors. Each displayed line is checked
against the source before using its colors, so concurrent edits or Git filters
cannot assign highlights from different text.

## Refresh and architecture

Opening, `r`, saving, and terminal focus gain request refresh. While a Git pane is
open, an idle deadline also refreshes every two seconds after the last batch
finishes. Closing the last status pane stops polling. Up to 16 repository views
retain their UI state during the session; unused entries are evicted when needed.

`vex_git::status` reads porcelain v2 with NUL-delimited paths and bounded Git
commands. A dedicated worker and result slot keep repository queries independent
of gutter updates. Complete batches cover all displayed repositories, and
cancellation/request generations reject obsolete results. Git's optional index
refresh writes, pagers, external diff drivers, and textconv are disabled.

Writes use `vex_git::write` and a separate ordered worker with reliable FIFO
completion events. At most one operation per repository (16 total) is admitted;
another request reports busy. Refreshing or closing panes never cancels a write.
Completions refresh both status and gutter baselines, preserving navigation.
Hooks/signing do not use the queries' two-second timeout. Normal quit commands
wait for pending writes to complete; runtime teardown drains and joins the worker.

`sections` provides reusable navigation by stable row identity. Refresh preserves
selection and scroll anchors; if a row disappears, navigation falls back to a
surviving parent or nearby row. Hunk fold identity uses content rather than line
numbers. Documents and Git views are distinct pane content types, so status text
never enters an editable document or undo history.

Queries have two-second command deadlines. Status output is limited to 4 MiB and
10,000 file entries; up to 32 files can be expanded, with a 256 KiB/4,000-line
preview limit per diff and a 20,000-row view limit. Limits and errors are displayed
in the view; an unsuccessful refresh retains the last successful snapshot.

Source tests cover parsing, index/disk separation, folds, stale results, protected
buffers, split panes, and syntax context. `tools/git_status_smoke.py` checks the
real terminal, idle refresh, and that browsing leaves HEAD, the index, and files
unchanged.
`tools/git_write_smoke.py` exercises staging, unstaging, draft retention, failed
hooks, a running hook that outlives its pane, successful commits, and terminal
cleanup. Its mutations are confined to a temporary fixture repository.
