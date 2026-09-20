# TODO

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
- [ ] Detect external file edits and reload clean buffers.
- [ ] Reconcile external changes with unsaved editor text before replacing it.

Reference: [Codex App Server](https://learn.chatgpt.com/docs/app-server).

Example workflow: open a failing PR check → jump to the relevant code → send the
selection and failure to Codex → review its changes in the Git pane.

### Build and test tools

- [ ] Run build and test tasks in background services.
- [ ] Show task status and output in dedicated panes.
- [ ] Present structured failures with jumps to compiler errors and failing tests.
