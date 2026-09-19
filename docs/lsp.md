# Rust language services

Opening a named Rust file starts `rust-analyzer` from `PATH`. Install the server
and a Rust toolchain separately; Vex does not download them. To use a different
executable, set `VEX_RUST_ANALYZER` to its absolute path before starting Vex.
Scratch buffers gain language services after saving to a `.rs` path. Manual
`:language rust` also enables them for named files; `:language text` stops them.

The status line shows `RA:starting`, `RA:ready`, or `RA:unavailable`, followed by
error and warning counts. Ready means the initialization handshake finished;
workspace loading and diagnostics may still be in progress. Missing servers,
protocol failures, and request errors leave editing and saving available.
`:lsp-restart` retries the current file after a failure or configuration change.

| Key / command | Behavior |
|---|---|
| `K` / `:hover` | Show documentation at the primary cursor |
| `gd` / `:goto_definition` | Jump to the first definition returned by the server |
| `Ctrl-o` / `:jump_back` | Return to the previous definition-jump location |
| `]d` / `:goto_next_diagnostic` | Next diagnostic, wrapping and accepting a count |
| `[d` / `:goto_previous_diagnostic` | Previous diagnostic, wrapping and accepting a count |
| `:lsp-restart` | Restart the server for the current file |

These keys apply in normal and select modes. Each editing action is an ordinary
documented command function in `vex_editor`, available to custom keymaps.

Hover uses a plain-text panel of up to twelve lines. Any key dismisses the panel
and continues through normal key dispatch. Diagnostic gutter markers and counts
update asynchronously; the message at the cursor appears on the bottom line when
no other message or prompt is active. Errors take precedence over other markers
on the same line. Editing clears diagnostics immediately until a current result
arrives.

Definition jumps can open another local file. Since Vex currently owns one active
buffer, crossing files requires saving any unsaved changes first. The jump list
retains up to 32 paths and cursor positions, and returning reloads that file from
disk. File switching creates a fresh editor/history; it does not preserve hidden
buffers. Multiple definition results currently choose the first result.

## Protocol and runtime

`vex_lsp` handles JSON-RPC framing, initialization, capabilities, document
synchronization, requests, diagnostics, and shutdown independently of terminal
drawing. It implements the relevant parts of
[LSP 3.17](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/).
Rust-analyzer receives its default configuration, including its usual workspace
loading and Cargo checks. The workspace root is the outermost ancestor containing
`Cargo.toml` before the repository boundary, or the file's parent for standalone
files. Settings and workspace-edit requests receive explicit responses; workspace
edits and dynamic capability registration are not supported.

A small, safe `Future` executor runs one service future on a dedicated thread.
That future waits concurrently for editor updates, protocol notifications, and
the active request response. `std::task::Wake`, a condition variable, and the
earliest registered deadline provide sleeping and wakeups. There is no Tokio,
general task scheduler, or OS async I/O driver. Three additional threads own
stdin writes, stdout reads, and bounded stderr capture for the active server.
All blocking pipe I/O stays outside `Future::poll`.

The UI sends cheap rope snapshots through a latest-update mailbox. Routine edits
debounce for 20 ms, with a 100 ms maximum batching delay; explicit requests and
session changes wake immediately. Text is serialized on the service thread.
`didOpen`, full-content `didChange`, `didSave` when supported, and `didClose` are
sent in order. Full replacement is supported even when the server advertises
incremental synchronization. Changes are sent before requests for their positions.
Undo and redo also advance the synchronized version.
When a save and subsequent typing coalesce, the saved snapshot is synchronized
before `didSave`, followed by the newer unsaved text. Save notifications therefore
describe the contents actually written to disk.

Positions use negotiated UTF-16 coordinates. A per-snapshot index distinguishes
LSP's LF/CRLF/bare-CR lines from Ropey's additional Unicode separators. Surrogate
pairs, combining marks, escaped file URIs, and non-ASCII file names are covered by
source tests. Results are checked against the active file session and document
revision; hover and definition also check request identity, selections, and mode.
Subsequent input cancels an outstanding interactive request.

The terminal inbox has a separate bounded FIFO for LSP events. Search and syntax
retain their separate latest-result slots. Input and ready services alternate so
diagnostic traffic cannot starve editing. Server failures are displayed in the
editor instead of terminating the terminal session.

Initialization has a 30-second deadline; hover and definition requests have
10-second deadlines. Dropped requests send `$/cancelRequest`. Closing attempts
`didClose`, `shutdown` (300 ms), and `exit`, with a 200 ms exit grace period, then
terminates and reaps the server and joins its I/O threads. On Unix the server has
its own process group so cleanup can also stop descendants that retain pipe
handles. This prevents inherited pipes from holding the reader joins open.

## Current limits and validation

One server session is active at a time; changing file identity, Save As, or
explicit restart starts a new session. Documents above 8 MiB stay editable but
do not start language services. Frames are limited to 32 MiB, headers to 8 KiB,
outgoing messages to eight queued values, incoming service notifications and UI
LSP events to 128 each, diagnostics to 512, and retained stderr to 8 KiB. An
overloaded transport reports an error and can be restarted.

Only diagnostics for the active file are displayed. Versioned diagnostics must
match the current synchronized version. Unversioned diagnostics are accepted
before the first change and ignored afterward, since their freshness cannot be
established. Rust-analyzer supplies versions for open buffers. Full document
sync, full line-index rebuilds, and JSON encoding still cost work proportional to
document size on the service thread. File loading, including definition jumps,
remains synchronous.

Completion menus, signature help, references, rename, formatting, code actions,
semantic tokens, multi-buffer server reuse, and configurable server settings are
future work.

```sh
cargo test --workspace --locked
# Explicit check requiring rust-analyzer and a Rust toolchain:
cargo test -p vex_lsp --locked real_rust_analyzer_hover_definition_and_diagnostics -- --ignored
cargo build --release -p vex_term --locked
python3 tools/lsp_smoke.py
```

Unit tests live with the source, including a controlled stdio server for protocol
ordering, Unicode positions, stale diagnostics, missing executables, and shutdown.
The explicit rust-analyzer test checks actual hover, definition, and diagnostics;
the PTY script checks those features through the editor, cross-file navigation,
return jumps, missing-server behavior, and terminal restoration.
