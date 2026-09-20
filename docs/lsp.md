# Language services

Opening a named file with a configured language starts its server from `PATH`.
Install language servers and their toolchains separately; Vex does not download
them. Syntax highlighting works without a language server.

| Language | Command | Executable override | Status label |
|---|---|---|---|
| Rust | `rust-analyzer` | `VEX_RUST_ANALYZER` | `RA` |
| Markdown | `marksman server` | `VEX_MARKSMAN` | `Marksman` |
| Bash / POSIX shell | `bash-language-server start` | `VEX_BASH_LANGUAGE_SERVER` | `Bash` |
| TypeScript, TSX, JavaScript, JSX | `typescript-language-server --stdio` | `VEX_TYPESCRIPT_LANGUAGE_SERVER` | `TS` |

Overrides specify an executable path, with the listed arguments supplied
separately; they are not shell command strings. The server commands follow the
[Marksman](https://github.com/artempyanykh/marksman),
[Bash language server](https://github.com/bash-lsp/bash-language-server), and
[TypeScript language server](https://github.com/typescript-language-server/typescript-language-server)
documentation. TypeScript language server also needs TypeScript/tsserver installed;
Bash diagnostics may require ShellCheck.

Scratch buffers gain language services after saving to a recognized path or
saving with a manually selected language. `:language NAME` changes both syntax
and server selection; `:language text` stops language services. Changing language
on the same file also starts a fresh server session and discards old capabilities,
completion requests, and diagnostics.

The status line shows the server label with `starting`, `ready`, or `unavailable`, followed by
error and warning counts. Ready means the initialization handshake finished;
workspace loading and diagnostics may still be in progress. Missing servers,
protocol failures, and request errors leave editing and saving available.
`:lsp-restart` retries the current file after a failure or configuration change.

| Key / command | Behavior |
|---|---|
| Space-k / `:hover` | Show documentation at the primary cursor |
| `gd` / `:goto_definition` | Jump to a definition, or pick among several |
| `gy` / `:goto_type_definition` | Jump to a type definition, or pick among several |
| `gi` / `:goto_implementation` | Jump to an implementation, or pick among several |
| `gr` / `:goto_reference` | Find references across files, including the declaration |
| `Space-h` / `:select_references_to_symbol_under_cursor` | Select related occurrences in the current document |
| `Space-r` / `:rename_symbol` | Rename the symbol across files through a prefilled prompt |
| `Space-a` / `:code_action` | Choose and apply a language-server code action |
| `Space-s` / `:symbol_picker` | Pick a symbol from the current document |
| `Space-S` / `:workspace_symbol_picker` | Search symbols across the active server's workspace |
| `Ctrl-o` / `:jump_backward` | Move backward through the current pane's jump history |
| `Ctrl-i` / `:jump_forward` | Move forward through the current pane's jump history |
| `]d` / `:goto_next_diagnostic` | Next diagnostic, wrapping and accepting a count |
| `[d` / `:goto_previous_diagnostic` | Previous diagnostic, wrapping and accepting a count |
| `:lsp-restart` | Restart the server for the current file |

These keys apply in normal and select modes. Each editing action is an ordinary
documented command function in `vex_editor`, available to custom keymaps.

Hover uses a cursor-anchored box with the same border and bold title as other
popups. Markdown replies render headings, emphasis, links, lists, quotations,
tables, and code blocks; plaintext replies remain literal. Code is styled but
not parsed as a separate programming language. HTML is displayed literally,
and links are underlined text rather than terminal hyperlinks.
Ctrl-u/Ctrl-d and PageUp/PageDown scroll by half the visible panel. Escape/Ctrl-c
close it; other editing keys dismiss it and continue through normal key dispatch.
Long lines wrap within the panel, which leaves the surrounding file visible.
Diagnostic gutter markers and counts update asynchronously; the message at the
cursor appears on the bottom line when
no other message or prompt is active. Errors take precedence over other markers
on the same line. Editing clears diagnostics immediately until a current result
arrives.

Definition and symbol jumps open another local file in the focused pane, retaining
the old buffer and any unsaved edits. Each pane's [jump list](windows.md#jump-history)
retains up to 32 selection checkpoints. Returning to a loaded file reuses its buffer and history, including
when no pane displays it.

Definition, type, implementation, and reference requests jump directly for one
usable destination; multiple destinations open the shared floating picker with
fuzzy path/line filtering and syntax previews. Returned ranges remain selected,
with the cursor at the start. Location links use `targetRange`, following Helix.
`Space-'` reopens the picker with its filter and selected result. File loading and
range conversion use the existing picker worker, reusing shared unsaved buffers.
Navigation validates both its origin and loaded destination before switching.

`Space-h` uses `textDocument/documentHighlight`, as
[Helix does](https://github.com/helix-editor/helix/blob/master/helix-term/src/commands/lsp.rs),
and retains the occurrence containing the original primary cursor as primary.
It preserves normal/select mode. UTF-16 conversion, grapheme normalization, and
sorting happen on the language-service thread; installing the resulting selection
set does not repeat those scans. Subsequent editing keys wait in FIFO order for
navigation or reference selections. Escape/Ctrl-c can cancel when next in that
order; resize and service events continue. Empty results and errors release input
without changing selections. Stale replies cannot apply to a different revision,
selection, mode, or file session.

Location responses are capped at 65,536 entries and 8 MiB of path data; the picker
reports limits and skipped malformed/non-file locations, and ranks at most 512
visible results per query. Document highlight responses exceeding 65,536 ranges
fail explicitly rather than selecting only a prefix. These limits are separate
from the existing 8 MiB active-document LSP limit.

Language services maintain one active document session. Switching focus between
views of the same file keeps the session and cancels cursor-specific requests.
Focusing a different file changes the session. Diagnostics, hover, and completion
are shown in the focused pane. Rename synchronizes other captured buffers into
that session; retaining server sessions when switching files remains future work.

Project discovery uses the nearest configured marker (`.marksman.toml`,
`.shellcheckrc`, or a TypeScript/JavaScript project manifest), falling back to the
repository root and then the file's directory. Rust retains the outermost
`Cargo.toml` within the repository to include workspace members. Discovery never
crosses a `.git` boundary.

## Rename

`Space-r` asks the server to prepare a rename when it supports `prepareRename`,
then opens `rename-to:` on the shared prompt line. The server's placeholder or
range supplies the current name. Without preparation support, Vex uses the
primary selection or word under the cursor. Ctrl-u clears the prefilled name;
Enter submits and Escape/Ctrl-c cancels. Empty submission cancels without
reusing command/search history. Names are limited to one line and 4,096 bytes;
the server validates language-specific naming rules.

The LSP worker first synchronizes captured buffers for this server's languages
inside its project root, including unsaved hidden buffers. JSON conversion and
protocol work stay off the UI thread. Buffers with unchanged snapshots are not
resent; snapshots and wire versions accompany the resulting edits. The normal
8 MiB limit applies per document, with 4,096 captured buffers and 64 MiB of
participating text per request.

After application, a separate coalesced snapshot update synchronizes changed
hidden buffers before subsequent requests. Cursor movement and cancellation do
not discard that update. Ordinary keystrokes still submit only the active buffer.

The [workspace-edit worker](workspace-edits.md) prepares all text and view
selections. Application validates the entire batch before changing any buffer.
Focus stays in the original pane, edits remain unsaved, and each affected buffer
gets one undo step. Previously unopened files become hidden buffers. Use the
buffer picker to visit and save them. An invalid range, stale snapshot, or
unsynchronized dirty buffer rejects the entire batch. File creation/renaming/
deletion and confirmation-required annotations are currently unsupported.

Preparation, server requests, and edit delivery preserve subsequent key order;
resize and background events continue while waiting. Escape/Ctrl-c cancel when
next in input order. The editable prompt does not block the event loop. Changing
its original document, revision, mode, selection, or pane invalidates submission.

## Code actions

`Space-a` requests actions for the primary selection with the current overlapping
server diagnostics. Quick fixes sort first, followed by refactoring categories;
diagnostic fixes and preferred actions break ties while equal actions retain
server order. Disabled actions are omitted. The cursor-anchored menu uses the same
border, bold title, and grey selection as the other popups.

Up/Down, Ctrl-p/Ctrl-n, or BackTab/Tab cycle actions. Ctrl-u/Ctrl-d and
PageUp/PageDown move by half the visible rows. Enter applies the selected action;
Escape/Ctrl-c dismiss. Other editing keys close the menu and dispatch normally.
The first action is selected when the menu opens.

Vex resolves the chosen action when the server supports resolution and its edit
or command is missing. Opaque data is preserved. The server's literal edit is
prepared and validated as one workspace batch before any accompanying command
runs. That command sees the changed active and hidden buffers and may request
further edit batches. Each batch has its own per-buffer undo step; an error in a
later batch does not roll back an earlier applied batch. Changes remain unsaved.
Cancelling before application or changing the request's origin rejects its reply.
Cancelling or failing a literal edit also discards its accompanying command.

Only short labels and opaque tickets cross to the menu. Raw action payloads stay
on the LSP service, including when the menu closes. Lists are capped at 256
entries and 8 MiB of retained JSON; titles at 256 characters. Edit parsing,
resolution, JSON work, and workspace preparation run on workers. A replaced list,
changed document revision, or restarted server invalidates old action tickets.
Resource operations and confirmation-required annotations remain unsupported,
as described under [workspace edits](workspace-edits.md).

## Completion

In insert mode, completion opens automatically after typing at least two
identifier characters and pausing for 100 ms. Characters advertised by the server
(such as `.` and `:` with rust-analyzer) request completion immediately, even with
an empty prefix. `Ctrl-x` invokes the documented `completion` command immediately
at any prefix length. Completion supports a single caret in a named file with a completion-capable server.
Suggestions appear in a
bordered menu next to the cursor, above it when there is more room there. A
separate documentation box appears beside the menu when space permits. Selected
rows use the shared grey palette; drawing keeps the insertion cursor visible.

| Key while the menu is open | Behavior |
|---|---|
| Tab / Ctrl-n / Down | Select the next suggestion |
| Shift-Tab / Ctrl-p / Up | Select the previous suggestion |
| Enter | Accept the selected suggestion |
| Ctrl-c | Reject completion, staying in insert mode |
| Escape | Accept an explicitly selected suggestion, then enter normal mode |
| Other input after selecting | Accept the selection, then process that input |

No item is initially selected. Enter then inserts a newline, and Escape returns
to normal mode without inserting a suggestion. Typing identifier characters or
backspacing before selecting requests a fresh list for the new text. This also
handles incomplete server lists. `Ctrl-x` explicitly refreshes an open menu.
The bindings are [Helix-inspired](https://docs.helix-editor.com/keymap.html#completion-menu);
temporary insertion previews are not implemented.

Automatic requests start only after the server is ready and advertises completion
support. They show no loading box, waiting message, empty-result message, or
request-error message. Until suggestions arrive, Tab, arrows, and Return retain
their normal editing behavior. Choosing a suggestion makes acceptance explicit;
errors during acceptance are reported normally. Manual requests retain their
loading and error feedback.

Typing resets the automatic deadline and cancels obsolete requests. An open
unselected menu refreshes on identifier typing and Backspace; automatic refreshes
use the same delay, while manual sessions refresh immediately. Incomplete lists
are re-requested with the LSP incomplete-list trigger. Movement, paste, focus
loss, mode changes, dismissal, and buffer changes cancel pending automatic work.
Closing a menu leaves it closed until another eligible edit or `Ctrl-x`.

Configure this editor session with these commands (settings survive file changes
and server restarts, but are not saved to disk):

| Command | Effect |
|---|---|
| `:auto-completion` | Show the current settings |
| `:auto-completion on` / `off` | Enable/disable automatic requests; Ctrl-x remains available |
| `:auto-completion delay 250` | Set the typing delay in milliseconds, from 0 to 10000 |
| `:auto-completion min-length 3` | Set the prefix threshold, from 1 to 256 characters |

The existing terminal event loop waits until the next completion deadline and
checks it even without new input. Completion requests use the existing LSP
service; no extra timer thread is needed. Server trigger characters and trigger
contexts follow the [LSP completion protocol](https://github.com/microsoft/language-server-protocol/blob/gh-pages/_specifications/lsp/3.17/language/completion.md).

The LSP service filters candidates using the current word as a case-insensitive
subsequence of `filterText` (or the label), retaining the server's `sortText`
ordering. The initial word rule uses Unicode letters/numbers and underscore,
with a 256-character prefix limit. Repeated edit coordinates share validated
UTF-16 conversions, and menu label widths are cached when a list arrives.
It reads at most 16,384 candidates and retains at most 512 and 4 MiB of serialized
item payloads. Each item is limited to 64 KiB and 64 additional edits. Labels,
details, and documentation are bounded independently for drawing. A limited
list is marked in its title; narrow the prefix and request again.

Selection resolves documentation, details, and additional edits on the existing
LSP service thread. Documentation is displayed as plain text with wrapping.
Acceptance waits for resolution so replacement text and import edits apply in
one validated transaction and undo step. UTF-16 ranges must round-trip exactly;
invalid positions, reversed ranges, and overlapping edits cannot partly modify
the buffer. Plain `textEdit`, insert/replace edits (using the insert range), and
`insertText`/label fallbacks are supported. Snippet support is not advertised, and
unexpected snippet items are skipped. Completion commands from the server are
not executed. Snippet placeholders and multiple completion carets remain future work.

Early Tab/Enter can wait for the current list and its resolved item. Subsequent
editing keys remain in the event queue, while resize and service events continue.
Ctrl-c or Escape can cancel an acceptance when next in key order. An ordinary
key that implicitly accepted is retained and dispatched even if resolution fails
or is cancelled. Late replies must match the request, document revision, file
session, mode, and caret. Moving, editing, closing, restarting, or switching files
invalidates old results. A resolve failure leaves the original completion text
unchanged and reports the error.

## Protocol and runtime

`vex_lsp` handles JSON-RPC framing, initialization, capabilities, document
synchronization, requests, diagnostics, and shutdown independently of terminal
drawing. It implements the relevant parts of
[LSP 3.17](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/).
Servers receive their default configuration. Rust-analyzer includes its usual workspace
loading and Cargo checks. Project roots use the registry rules described above.
Settings and server-initiated workspace-edit requests receive explicit responses;
`workspace/applyEdit` and dynamic capability registration are not supported.
Rename response edits support versioned text changes with whole-batch validation.

A small, safe `Future` executor runs one service future on a dedicated thread.
That future waits concurrently for editor updates, protocol notifications, and
the active request response. `std::task::Wake`, a condition variable, and the
earliest registered deadline provide sleeping and wakeups. There is no Tokio,
general task scheduler, or OS async I/O driver. Three additional threads own
stdin writes, stdout reads, and bounded stderr capture for the active server.
All blocking pipe I/O stays outside `Future::poll`.

The output queue holds eight client messages. Synchronization and requests await
capacity with a waker and a ten-second deadline; no polling or blocking send runs
in the executor. Server-request replies have a separate bounded queue of 32 and
are written first, so synchronization traffic cannot crowd out configuration
responses or block the reader. Client document notifications retain FIFO order.

The UI sends cheap rope snapshots through a latest-update mailbox. Routine edits
debounce for 20 ms, with a 100 ms maximum batching delay; submitted requests and
session changes wake immediately. Text is serialized on the service thread.
`didOpen`, full-content `didChange`, `didSave` when supported, and `didClose` are
sent in order. Full replacement is supported even when the server advertises
incremental synchronization. Changes are sent before requests for their positions.
Undo and redo also advance the synchronized version.
When a save and subsequent typing coalesce, the saved snapshot is synchronized
before `didSave`, followed by the newer unsaved text. Save notifications therefore
describe the contents actually written to disk.

Positions use negotiated UTF-16 coordinates. A per-snapshot index distinguishes
LSP's LF/CRLF/bare-CR lines from Ropey's additional Unicode separators. Column
conversion uses the rope's UTF-16 index rather than scanning line prefixes. Surrogate
pairs, combining marks, escaped file URIs, and non-ASCII file names are covered by
source tests. Results are checked against the active file session and document
revision; interactive replies also check request identity, selections, and mode.
Edits, movement, and dismissal cancel obsolete requests. Completion navigation
keeps the list and requests documentation for the new selection.

The terminal inbox has a separate bounded FIFO for LSP events. Search and syntax
retain their separate latest-result slots. Input and ready services alternate so
diagnostic traffic cannot starve editing. Server failures are displayed in the
editor instead of terminating the terminal session.

Initialization has a 30-second deadline; interactive requests have
10-second deadlines. Commands pause that response deadline while their workspace
edit is prepared and applied, then resume it after acknowledgement. Dropped requests send `$/cancelRequest`. Closing attempts
`didClose`, `shutdown` (300 ms), and `exit`, with a 200 ms exit grace period, then
terminates and reaps the server and joins its I/O threads. On Unix the server has
its own process group so cleanup can also stop descendants that retain pipe
handles. This prevents inherited pipes from holding the reader joins open.

## Current limits and validation

One server session is active at a time; changing file identity, Save As, or
explicit restart starts a new session. Documents above 8 MiB stay editable but
do not start language services. Frames are limited to 32 MiB, headers to 8 KiB,
outgoing client messages to eight queued values and server-request replies to 32,
incoming service packets and UI
LSP events to 128 each, diagnostics to 512, and retained stderr to 8 KiB. An
overloaded transport reports an error and can be restarted.

Only diagnostics for the active file are displayed. Versioned diagnostics must
match the current synchronized version. Some servers omit diagnostic versions.
Their diagnostics are mapped to the
latest synchronized snapshot on a best-effort basis; freshness cannot be proven
without a version. Any subsequent edit clears them. Versioned stale results are
still rejected, and results from old language sessions are always discarded.
Full document sync, full line-index rebuilds, and JSON encoding still cost work
proportional to document size on the service thread. Definition/type/implementation/
reference destinations and workspace-edit files load on workers; some older
symbol-jump paths still load synchronously.

Signature help, formatting,
semantic tokens, multi-buffer server reuse, and configurable server settings are
future work.

Hover preparation runs on the LSP service thread using the bundled Markdown
grammars, with no additional third-party dependencies. It accepts modern
MarkupContent and legacy MarkedString replies, capped at 64 KiB of source and
32 parts and 4,096 prepared lines. Parsing has a shared 25 ms budget, bounded
nodes/depth, and cancellation; an exhausted budget falls back to literal text.
The popup caches wrapping until
its available width changes, and redraw visits only visible rows. Unsupported
or unusual Markdown may remain literal rather than matching a browser renderer.

The code-action menu uses the command/application backend through
`App::execute_lsp_command`: advertised server commands can request validated edits
across synchronized buffers. Ordered requests and replies preserve command
completion, cancellation, and synchronization before `applied:true`. See
[workspace edits](workspace-edits.md) for transaction boundaries and limits.

```sh
cargo test --workspace --locked
# Explicit check requiring rust-analyzer and a Rust toolchain:
cargo test -p vex_lsp --locked real_rust_analyzer_hover_definition_and_diagnostics -- --ignored
cargo build --release -p vex_term --locked
python3 tools/lsp_smoke.py
# Requires typescript-language-server and TypeScript/tsserver:
python3 tools/languages_smoke.py
```

Unit tests live with the source, including a controlled stdio server for protocol
ordering, Unicode positions, stale diagnostics, missing executables, and shutdown.
The explicit rust-analyzer test checks actual hover, definition, and diagnostics;
the PTY script also checks document/workspace symbol pickers, completion, documentation resolution, early acceptance,
undo, cross-file navigation, return jumps, missing-server behavior, and terminal
restoration. Source tests cover completion import edits, stale replies, invalid
coordinates, cancellation, popup clipping, and protocol ordering.

The language PTY script checks Markdown/shell/TSX rendering and actual TypeScript
diagnostics, hover, automatic completion, acceptance, saving, and shutdown.
Registry-driven stdio tests cover every server argument list and language ID,
including Markdown and Bash when their servers are not locally installed.
