# TODO

## Editing priorities

Ordered by everyday usefulness and dependencies after the shared internal yank
register, `y`/`p`/`P`/`R`, and cut behavior for `d`/`c`. Bindings must follow the
[Helix keymap](https://docs.helix-editor.com/keymap.html) rather than Vim.
The current goal includes previously held
items and Alt bindings. Keep editing work proportional to affected text and
selections, and measure operations that could affect input latency.

1. [x] Selection controls: `%` select all, `;` collapse, `,` keep primary, `X`
   extend to line boundaries, and `_` trim whitespace. Make repeated `x` extend
   to subsequent lines.
2. [x] Precise movement: `f`/`F`, `t`/`T`, `W`/`B`/`E`, `gs`, counted `gg`/`G`,
   and counted `g|`. Add command input for a following character without losing
   counts or cancellation.
3. [x] Everyday edits: `I`, `A`, `r`, `>`, `<`, `J`, and `[Space`/`]Space`.
   Add language indentation settings for indent/unindent.
4. [x] Comments: `<space>c`, `<space>C`, and normal/select-mode Ctrl-c, with
   language-specific line/block delimiters. Pending commands retain cancellation.
5. [x] Multiple selections and search: `s`, `S`, `K`, `*`, regex `/`/`?`,
   and accumulating matches with `n`/`N` in select mode. `K` is reserved for
   selection filtering; hover uses `<space>k`. `C` now copies selections to
   following lines, with counts and cancellable background scans.
6. [x] Buffer switching: retain ordinary buffers without visible panes, then add
   `<space>b`, `ga`, `gn`/`gp`, and `gm`. Add `gf` to open selected paths in the
   current pane. Preserve unsaved text, undo history, and per-view selections
   when switching files.
7. [x] Match mode: `mm`, `mi`/`ma` textobjects for words, paragraphs, and paired
   delimiters, followed by `ms`/`mr`/`md` surrounds.
   Textobjects and bracket matching now run through the cancellable selection
   worker with shared syntax trees. `ms`/`mr`/`md` add, replace, and remove
   surrounds; replacement previews delimiters before collecting its second input.
8. [x] Repeat the last insert with `.`. Logical command recording retains counts,
   text, motions, and accepted completion across buffers. Cooperative playback
   preserves queued input order and supports cancellation and undo checkpoints.

## Registers and input follow-ups

- [x] System clipboard commands: `<space>y`/`Y`, `<space>p`/`P`, and `<space>R`.
  Platform helpers, copy capture, and paste preparation run on the background
  worker, preserving fragment boundaries, input order, cancellation, and undo.
- [x] Named registers with `"<register>` and insert/prompt Ctrl-r. Preserve
  fragment boundaries when copying between editor selections.
  Ordinary names, `_`/`#`/`.`/`%`, automatic `/`/`:` registers, prompt/picker
  insertion, insert repeat, and searching with a chosen register are implemented.
  `+`/`*` clipboard reads, writes, cuts, changes, and searches use the workers;
  replay waits for current clipboard contents and preserves input/undo ordering.
- [x] Insert-mode word deletion and line kills: Ctrl-w, Ctrl-u, Ctrl-k, plus
  Ctrl-d/Ctrl-j aliases for Delete/Enter.
- [x] Prompt history, command/path completion, word movement, and line kills.
  History is bounded and shared across buffers. Prompt editing/drawing visits
  nearby graphemes. Command metadata and path suggestions use the picker worker;
  Tab/BackTab cycling preserves queued input and rejects stale results.
- [x] Decide whether to track explicit linewise selection intent: retain Helix's
  text-based rule. A final line without a trailing newline pastes as characters,
  including after `x`/`X`; registers do not carry a separate linewise flag.

## Project navigation and language tools

- [x] Workspace text search with `<space>/`, then `<space>'` to reopen the last
  picker with its query and selection intact.
  Regex search uses cancellable background reads and shared unsaved buffers,
  with debounced queries, bounded results, previews, and queued-input ordering.
  `<space>'` retains file/buffer/symbol/search queries, selected results, and scroll
  positions across acceptance/cancellation.
- [x] Bidirectional jump history: Ctrl-i, `<space>j`, and `g.` to return to the
  last modification. Ctrl-s records full selection checkpoints in normal/select
  mode; counted Ctrl-o/Ctrl-i traverse each pane's bounded history, including
  scratch buffers, and preserve a forward return path. `g.` locates the last
  undo group's primary change through cancellable background metadata composition.
  `<space>j` picks checkpoints from all panes with background filtering, previews,
  and full selection restoration. Saved selections lazily follow edits, grouped
  undo/redo, and reloads through a text-free journal. Navigation and picker
  remapping are cancellable, preserve queued input, and reject stale revisions.
- [x] LSP references (`gr`), type definition (`gy`), implementation (`gi`), and
  reference selections (`<space>h`). Multiple destinations (including `gd`)
  use the shared picker with filtering, syntax previews, and last-picker reopening.
  Document highlights retain the primary occurrence; range preparation and file
  loading run on workers with cancellation, revision guards, and queued input.
- [ ] LSP rename (`<space>r`), code actions (`<space>a`), and formatting (`=`).
  Support validated edits across open and hidden buffers before enabling workspace
  edits; respect revisions and undo boundaries.
  The shared workspace-edit path now prepares text and every pane's selections on
  a worker, preflights the whole batch, and preserves per-buffer undo and unsaved
  text. Rename now synchronizes captured buffers with the server, uses
  `prepareRename` where available, and opens a prefilled prompt. Its edits stay
  unsaved with per-buffer undo, cancellation, and stale-result guards.
  The server-command backend now processes `workspace/applyEdit` in order, applies
  prepared batches with cancellation and undo, and synchronizes resulting buffers
  before acknowledging success. Early command completion waits for its edits.
  Next: code-action menu/resolution and range formatting.
- [ ] Document/workspace diagnostic pickers (`<space>d`/`D`) and first/last
  diagnostic jumps (`[D`/`]D`). Retain diagnostics beyond the active document.
- [ ] Scrollable hover documentation and signature help.
  Hover now has a bordered Markdown popup, cached wrapping, and Helix-style
  Ctrl-u/Ctrl-d and PageUp/PageDown scrolling. Signature help remains.
- [ ] Git change navigation: `[g`/`]g`, `[G`/`]G`, and change textobjects.
- [ ] Remap cached Git gutter markers across line insertions/deletions while
  a background diff is pending.
- [ ] Tree-sitter textobjects and navigation between functions, types, arguments,
  comments, and tests (`[f`/`]f`, `[t`/`]t`, `[a`/`]a`, `[c`/`]c`, `[T`/`]T`).
  Extend the language registry with textobject queries and use revision-matched
  syntax results. Add paragraph navigation (`[p`/`]p`) independently.

## Later editing and interface work

- [ ] User/project indentation overrides and optional indentation detection for
  existing files. The registry now supplies spaces/tabs and tab display widths;
  add configuration (including EditorConfig) without rewriting existing text.
- [ ] Use language/buffer tab widths in file-picker and Git diff previews, which
  currently display tabs at four-column stops.
- [ ] Language-aware joining that removes repeated comment prefixes. `J`
  currently joins whitespace only and preserves comment markers literally.
- [ ] Add cancellable or bounded scanning for character finds and WORD motions
  on very large files or with many selections; preserve counts and input ordering.
  Selection-copy scans already run in the background, but cold column lookups
  and normalization of very large result sets still need finer cancellation.
- [ ] Replace two-second polling of visible files with filesystem notifications.
  Watch parent directories as well as files so atomic saves and deletion/recreation
  remain observable; coalesce notifications through the background event queue.
- [ ] Use finer-grained diffs for external reloads so cursors within several
  separated changes track nearby unchanged text more accurately.
- [ ] View mode `z`/sticky `Z`: center/top/bottom alignment, horizontal centering,
  and scrolling without moving selections. Add `gt`/`gc`/`gb` and Ctrl-b/Ctrl-f
  page aliases; keep logical/visual line movement explicit if soft wrapping arrives.
- [ ] Picker split-open (Ctrl-s/Ctrl-v), preview toggle (Ctrl-t), first/last result
  navigation, and `<space>F` for the working-directory file picker.
- [ ] Command palette (`<space>?`) backed by the existing documented registry.
- [ ] Case conversion, Ctrl-a/Ctrl-x number changes, selection alignment (`&`),
  and primary-selection rotation (`(`/`)`).
- [ ] Shell selection filters and output insertion (`|`, `!`, `$`). Use the
  event queue for subprocess results.
- [ ] Macro recording/replay (`Q`/`q`) and undo-tree history navigation.
- [x] Explicit insert undo checkpoints with Ctrl-s, without saving.
- [ ] Label-based word navigation (`gw`); debugger integration is a separate,
  lower-priority project.
- [ ] Remaining Alt bindings: modifiers are represented in the key model and
  terminal adapter; prompt word editing is implemented. Add selection reversal,
  splitting/merging/filtering, cursor-above,
  syntax expansion/siblings, non-yanking deletion, motion repeat, and related
  picker/insert/prompt variants.

## Developer tool integrations

Extend the pattern established by the Git view: each tool gets a pane,
documented commands, contextual keymaps, and a background service. Keep tool
integrations separate from the editor core and build on the existing event queue.

Start with a small GitHub PR/checks view, extract shared pieces as that second
integration needs them, then add Codex as a persistent session. Let concrete
integrations shape the interfaces before designing a full plugin system.

### GitHub

- [ ] Add a PR list and detail view backed by `gh` structured JSON output.
- [ ] Show CI checks and their status for the selected PR.
- [ ] Support navigation from changed files and review comments into editor buffers.
- [ ] Expand the integration to issues and review workflows.

Vex owns presentation and navigation; `gh` provides the GitHub interface.
References: [PR listing](https://cli.github.com/manual/gh_pr_list) and
[checks](https://cli.github.com/manual/gh_pr_checks).

### Shared integration infrastructure

- [ ] Generalize pane handling beyond document and Git-specific content.
- [ ] Give each tool pane its own state, commands, and keymap.
- [ ] Reuse lists, expandable sections, output logs, and editable drafts.
- [ ] Make service delivery rules explicit: replaceable snapshots, ordered
  operations, or ongoing event streams.
- [ ] Define service lifetimes independently of whether their pane is visible.

### Codex

- [ ] Connect to `codex app-server` through its bidirectional JSON protocol over
  standard input/output.
- [ ] Add a conversation pane with session creation and resumption.
- [ ] Send selected code and relevant diagnostics or task failures as context.
- [ ] Display streamed responses, tool activity, and approval requests from Codex.
- [ ] Connect resulting file changes to the existing Git review workflow.
- [x] Detect external file edits and reload clean buffers (visible panes, every
  two seconds; background reads, with protection for unsaved text).
- [ ] Reconcile external changes with unsaved editor text before replacing it.

Reference: [Codex App Server](https://learn.chatgpt.com/docs/app-server).

Example workflow: open a failing PR check → jump to the relevant code → send the
selection and failure to Codex → review its changes in the Git pane.

### Build and test tools

- [ ] Run build and test tasks in background services.
- [ ] Show task status and output in dedicated panes.
- [ ] Present structured failures with jumps to compiler errors and failing tests.
