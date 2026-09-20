# Repository status

Press Space-g, or run `:git_status`, to open the current file's repository in the
active pane. Scratch buffers use the working directory. This initial version
provides status, expandable diffs, and file navigation. Git writes such as staging
and committing will be added separately.

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
| ? | Show/hide the boxed key reference |
| q, Escape, Ctrl-c | Dismiss help, then return to the document |
| Ctrl-w, Space-w | Window commands |
| : | Command prompt; editing/saving the hidden document is disabled |

`:git_toggle`, `:git_visit`, `:git_refresh`, and `:git_close` are documented
commands, also available through `:help COMMAND`. Standard pane close/quit
commands retain the editor's unsaved-change checks.

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
