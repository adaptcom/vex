# Workspace text edits

LSP rename (`Space-r`) uses the shared workspace-edit path. Server commands use
the same path for `workspace/applyEdit`, with acknowledgements tied to actual
application results. The code-action menu and formatting are not enabled yet.

Before rename preparation and submission, the LSP worker synchronizes captured
open buffers belonging to its configured languages and workspace, including
hidden unsaved buffers. Unchanged snapshots are not resent. Wire versions are
tracked separately from editor revisions and accompany the response. The capture
is limited to 4,096 named buffers and synchronization to 64 MiB in total, with
the existing 8 MiB limit per document. Unrelated dirty buffers remain protected:
an edit targeting unsynchronized unsaved text fails the entire batch.
After application, the latest catalog snapshots are synchronized through a
separate service mailbox before following requests. This includes newly loaded
buffers; ordinary cursor requests cannot coalesce away that synchronization.

`App::workspace_edit_context` captures the origin and named buffers' immutable
text and view state before a request. `App::begin_workspace_edit` accepts a parsed
edit with the exact document versions synchronized by its protocol adapter.
Preparation shares the picker worker; it does not create another runtime,
thread pool, or dependency. Another pending editor operation must finish first.
Subsequent editing keys wait in FIFO order, while Escape/Ctrl-c can cancel when
next in that order. Resize and service events continue.

The worker resolves file identities, reuses captured unsaved buffers, and reads
previously unopened files. It converts UTF-16 positions, validates edit batches,
materializes the resulting ropes, and maps and normalizes every view's selections.
Cancellation is checked between reads, edits, and selections. Individual rope
insertions and selection sorts are not interruptible. Limits apply to decoded
requests: 4,096 documents, 65,536 text edits, and 16 MiB of replacement text and
paths. Exceeding a limit fails the entire request.

Before changing anything, the UI validates every retained document's identity,
revision, file path, views, modes, and selections. A file opened during preparation
also invalidates a proposed new buffer. Only after all checks succeed does the
UI install prepared text and view state. Each affected buffer gets one undo step;
new files become hidden buffers, focus stays put, and all edits remain unsaved.
An invalid batch changes neither text nor the buffer catalog. Undo operates on
each buffer independently, as with other editing commands.

`App::execute_lsp_command` accepts commands advertised by the active server.
Their opaque arguments stay in the LSP protocol and are bounded to 8 MiB before
cloning into an outgoing message. Captured buffers synchronize before execution.
Only a pending explicitly invoked command can request workspace application;
unsolicited requests receive `applied:false`.

Incoming protocol packets are handled in wire order. A command's response waits
for preceding application requests, including servers that return completion
early. At most eight further edit requests wait behind one being prepared.
Preparation has a ten-second deadline and cancellation token; immediately before
UI preflight, claiming the reply prevents a timeout from racing installation.
The service synchronizes the resulting active and hidden buffers before replying
`applied:true`, so subsequent server work sees the applied text. A failure after
installation while synchronizing ends the session rather than falsely reporting
that the editor rejected the change. Older coalesced active snapshots and buffer
catalog captures cannot overwrite the acknowledged snapshots.

Each server batch is independently validated and undoable. Failure or cancellation
of a later batch does not roll back earlier batches. A failed application remains
an editor error even if the server reports command success. Rejecting, dropping,
timing out, or cancelling an unapplied batch returns failure to the server.

The core `PreparedChange` retains regular undo/redo maps, change extents, and
bookmark remapping. `PreparedExternalEdit` additionally prepares all editor views.
Existing interactive text commands retain their direct application path.
Delivery work depends on affected buffers and selection metadata; it does not
repeat text insertion or grapheme scans. History eviction and dropping discarded
prepared data can still require memory reclamation proportional to that data.

The decoder follows the [LSP workspace-edit formats](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#workspaceEdit):

- `documentChanges` takes precedence over `changes`. Conflicting versions,
  malformed edits, non-file URIs, and overlapping replacements fail the batch.
- Multiple insertions at the same position preserve server order. They may be
  followed by one replacement at that position. No-op edits create no revision
  or undo entry.
- Columns beyond line end clamp to EOL according to LSP; invalid lines and
  offsets inside UTF-16 surrogate pairs are rejected for edits.
- A versioned edit needs the matching server version and editor snapshot.
  An unsaved buffer must have been synchronized before the request. For an open
  clean buffer without a known server snapshot, disk contents must still match.
  Unversioned unopened files use their current disk contents; the server supplies
  no older snapshot to compare. The normal save conflict checks remain active.
- Resource creation, file renaming/deletion, and confirmation-required change
  annotations are rejected before any text changes. These capabilities are not
  advertised. Paths that alias the same file within a batch are also rejected.

Inline tests exercise multi-buffer preparation, hidden unsaved text, independent
undo, cancellation, stale destination/view rejection, queued editing, UTF-16
coordinates, ordered insertions, and zero disk writes during application. Controlled
stdio servers also exercise multiple application requests, early command completion,
synchronization before acknowledgement, and stale captures after application.
