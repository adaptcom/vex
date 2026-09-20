# Performance baselines

## Lazy bookmarks

On 2026-09-20, `cargo bench -p vex_core --bench editing --locked -- bookmarks --noplot`
measured 64 adjacent single-caret insertions followed by grouped undo:

| Document | No bookmark | Live bookmark |
| --- | ---: | ---: |
| 1 MiB | 31.9 µs | 32.1 µs |
| 100 MiB | 39.8 µs | 40.1 µs |

These Criterion central estimates use 30 samples, 500 ms warmup, and one second
measurement. The tracked cases retain a bookmark from before typing. Setup
clones a shared ASCII rope outside measurement; the timed batch includes edit
construction, history, undo, and dropping the document/bookmark. It excludes
selection restoration, workers, rendering, and terminal I/O. The measured
tracking cost for this compact typing group was under 1% of the central estimate.

An isolated comparison with the pre-journal binary also measured ordinary
single-edit/undo cycles without retained bookmarks:

| Document / carets | Before | After |
| --- | ---: | ---: |
| 1 MiB / 1 | 0.732 µs | 0.747 µs |
| 100 MiB / 1 | 0.876 µs | 0.845 µs |
| 1 MiB / 1,000 | 413 µs | 415 µs |
| 100 MiB / 1,000 | 718 µs | 740 µs |

This found a small cost in several measured cases, including 15 ns for the
one-caret 1 MiB cycle. These local results are not latency bounds or evidence
that every workload is faster. The journal retains text-free maps without
walking checkpoint selections on edits. Adjacent typing compacts; capturing a
new bookmark seals that boundary. Other changes retain a sequence proportional
to edits since the oldest live bookmark. Retiring metadata costs proportional
to the released nodes and uses iterative destruction to avoid stack growth.
Undo-history eviction does not expire a live bookmark.

Resolution runs on the navigation/picker worker, checks cancellation between
changes and selections, and never scans unchanged document text. Mapping and
normalization costs grow with changes and saved ranges. Final view restoration
still normalizes selections on the UI thread; very large result normalization
remains covered by the existing cancellation follow-up in TODO.md.

With lazy bookmarks integrated, the `jump_history` terminal-app benchmark
measured unchanged Ctrl-o/Ctrl-i round trips at **1.44 / 1.36 µs** for one
selection and **0.504 / 0.633 ms** for 1,000 selections in 1 / 100 MiB documents.
Opening the picker, entering a query, and cancelling measured **0.888 / 0.896 µs**
for one selection per checkpoint and **0.906 / 0.871 µs** for 1,000. All cases
use a full 32-entry history. They measure synchronous application work without
intervening edits, worker wakeup, remapping, drawing, or terminal I/O. Compared
with the earlier baseline below, small navigation requests now include journal
capture and revision validation; large selection restoration remains dominant.

## Jump history

On 2026-09-20, `cargo bench -p vex_term --bench rendering --locked -- jump_history --noplot`
measured a Ctrl-o/Ctrl-i round trip through a full 32-entry history:

| Document | One selection | 1,000 selections |
| --- | ---: | ---: |
| 1 MiB | 1.02 µs | 0.501 ms |
| 100 MiB | 0.959 µs | 0.644 ms |

These are Criterion central estimates with 30 samples, 500 ms warmup, and one
second measurement, over repeated ASCII lines. The benchmark includes command
dispatch, history traversal, and selection restoration/normalization. Initial
checkpoints, document construction, drawing, physical terminal output, and
switching buffers are excluded. Stored selections share immutable allocations;
traversal copies at most 32 handles, then restores the selected entry. Costs
scale with restored selections and grapheme lookups, without scanning document
text. These pre-journal measurements establish the original navigation baseline;
the lazy-bookmark costs are described above.

Opening the jump picker, pasting a six-character query, and cancelling with 32
checkpoints measured **0.767 / 0.755 µs** for one selection per checkpoint in
1 / 100 MiB buffers, and **0.736 / 0.756 µs** for 1,000 selections per checkpoint.
Reproduce with:

```sh
cargo bench -p vex_term --bench rendering --locked -- jump_history/picker_open_query_cancel --noplot
```

These 2026-09-20 measurements use the same Criterion settings. They include
catalog capture, query dispatch, cancellation, and releasing the capture. They
exclude fixture construction, worker execution, result delivery, drawing, and
terminal I/O. Opening clones checkpoint handles and one rope snapshot per
referenced buffer; query edits share the catalog. Label construction and fuzzy
ranking run on the existing picker worker, with at most 256 snippet scalars /
32 selection ranges read per checkpoint and at most 512 returned rows. Large
selection sets remain shared until a chosen checkpoint is restored.

`g.` captures the last undo group's text-free metadata in O(1), sharing its
position maps. The selection worker composes changes with cancellation; it does
not scan or flatten the document. Adjacent single-caret typing compacts to one
map, whose destination is a direct lookup. A 16-character typing group at 1,000
carets took **75.0 µs at 1 MiB** and **75.9 µs at 100 MiB** to resolve. Scheduling
and cancelling took **0.259 / 0.264 µs** with one selection and **126 / 152 µs**
with 1,000 selections, respectively. The latter includes selection normalization
when leaving the pending command. Reproduce with:

```sh
cargo bench -p vex_editor --bench commands --locked -- last_modification --noplot
```

These use the same Criterion settings and exclude worker scheduling, rendering,
and terminal I/O. More complex undo groups cost more according to the number of
maps and composed spans, independently of unchanged document text.

Because this changed undo metadata storage, isolated release binaries before
and after the change were also compared with the `edit_undo/source_(1|100)MiB/`
core benchmarks. The central estimates were **0.738 → 0.709 µs** (one caret,
1 MiB), **0.864 → 0.853 µs** (one caret, 100 MiB), **410 → 409 µs** (1,000 carets,
1 MiB), and **709 → 684 µs** (1,000 carets, 100 MiB). This local comparison found
no regression in the measured editing cases; it is not a general latency bound.
Single-map undo groups share their existing allocation without allocating an
additional container for navigation metadata.

## Workspace search input

`cargo bench -p vex_term --bench rendering --locked -- workspace_search_input --noplot`
recorded these Criterion central estimates on 2026-09-20 (30 samples, 500 ms
warmup, one second measurement):

| Named buffer size | Loaded buffers | Open, paste query, cancel | Query character + Backspace |
| --- | ---: | ---: | ---: |
| 1 MiB | 1 | 7.80 µs | 0.152 µs |
| 100 MiB | 1 | 7.79 µs | 0.150 µs |
| 1 MiB | 32 | 8.53 µs | 0.150 µs |
| 100 MiB | 32 | 8.56 µs | 0.152 µs |

The 32-buffer cases keep the large file hidden and 31 small files loaded.
Opening captures paths and shared rope snapshots once; query edits share the
catalog through `Arc`. The input work does not scan or flatten buffer contents.
These cases include command dispatch, current-directory capture, cancellation,
and request setup. File loading, scanning, result delivery, drawing, and physical
terminal latency are excluded. They do not establish an end-to-end latency bound.

Workspace searches debounce for 150 ms, with immediate dispatch on early Enter.
The existing picker worker reuses file discovery across query changes and drops
its index when closed. Regex reads and scans check cancellation; retained results
are capped at 512, source reads at 128 MiB per file / 1 GiB per query, with a
cooperative ten-second query deadline. Single filesystem calls and compilation
are not preemptible. See [search behavior and limits](pickers.md#workspace-text-search).

The inline manual worker measurement can be repeated with:

```sh
cargo test --release -p vex_term workspace_worker_performance --locked -- --ignored --nocapture
```

Five searches for a unique match at EOF took a median **3.17 ms** over a 1 MiB
shared buffer and **140.61 ms** over a 100 MiB shared buffer. Each run includes
regex compilation and scanning; later runs reuse file discovery. The fixture
uses repeated ASCII lines and one named unsaved snapshot, so these numbers
exclude reading file contents from disk, worker scheduling, rendering, and
slow regex fallback patterns. They show the size-dependent work that stays on
the worker, separately from the input timings above.

## Prompt input and drawing

On 2026-09-20, `cargo bench -p vex_term --bench rendering -- prompt_input`
measured the following central estimates (30 samples, 500 ms warmup, one second
measurement). Prompt construction and initial paste are outside measurement.

| Prompt size | Append + Backspace | Left + draw + Right + draw, 120×40 |
| --- | ---: | ---: |
| 1 KiB | 33.0 ns | 80.6 µs |
| 64 KiB | 34.2 ns | 81.7 µs |
| 1 MiB | 33.3 ns | 81.6 µs |

These cases use ASCII words, a small document, and terminal output to a sink.
They exclude filesystem access, background completion, terminal I/O, and worker
wake latency. Cursor repair uses local grapheme boundary queries; drawing walks
back from the caret only far enough to fill the visible row. Unicode clusters
can require surrounding context, and inserting in the middle of a flat prompt
still shifts its trailing bytes. History is capped at 100 entries / 256 KiB.

The `prompt_input/completion_request_cancel` cases measure opening `:`, pasting
`write src/main`, requesting Tab completion, and cancelling. Request setup and
cancellation took **0.204 µs** with a 1 MiB document and **0.203 µs** with a 100 MiB
document (same Criterion settings). Document construction, worker wakeup, directory
reads, result application, drawing, and opening/saving files are excluded. The
request owns only a bounded prompt string, cursor, revision stamp, and cancellation
token. The existing picker worker processes the latest request; completion adds
no thread or dependency. Path jobs retain at most 128 candidates and inspect at
most 10,000 directory entries, with a cooperative 250 ms scan deadline. The
frontend skips completion for prompts larger than 8 KiB, before copying text.

Recorded on 2026-09-19 on the development machine: arm64, macOS 26.6.2,
Rust 1.98.1. CPU model was not available to the sandbox. These measurements are
a local baseline, not a hardware-independent performance guarantee.

```sh
cargo bench -p vex_core --bench editing --locked -- --noplot
```

The benchmark uses the workspace's optimized release profile (fat LTO, one
codegen unit). Criterion collects 30 samples per case with a 500 ms warmup and
one second of measurement. The table reports its central time estimates,
rounded; these are not per-event p95 latencies. Raw estimates and samples are
written under `target/criterion/` for comparison with later runs.

| Document | Insert at one caret, then undo | Insert at 1,000 carets, then undo |
|---|---:|---:|
| 1 MiB source text | 0.663 µs | 0.393 ms |
| 10 MiB source text | 0.707 µs | 0.505 ms |
| 100 MiB source text | 0.785 µs | 0.662 ms |
| 10 MiB single line | 0.714 µs | 0.515 ms |
| 10 MiB mixed Unicode | 0.762 µs | 0.465 ms |

| Other operation | Time |
|---|---:|
| Delete 10,000 scalars and undo in 10 MiB | 1.54 µs |
| Undo and redo a single edit in 10 MiB | 26.6 ns |
| Clone and drop a 10 MiB document snapshot | 3.29 ns |

The edit/undo cases include transaction construction, selection mapping,
copy-on-write rope edits, recording history, and restoring text and selections.
They insert a single ASCII character at evenly distributed scalar boundaries,
then undo so document size stays fixed. Caret positions may be inside grapheme
clusters: this measures the scalar-based storage layer. The Unicode fixture
includes CJK, combining characters, a joined emoji sequence, and CRLF.
Fixtures repeat a short pattern and are rounded up to a whole pattern.

Document creation and initial selection construction are outside the timed loop.
The same positions are reused after warmup, so these are cache-warm measurements;
they do not represent random access, sustained typing with growing history, file
loading, or rendering. A separate fixture rope stays alive during the tests,
exercising copy-on-write storage. History retains only the current edit/undo
cycle in those cases, and snapshot/undo figures do not include editing work.

Before setting an interactive latency target, add measurements for event receipt
through terminal flush at a fixed viewport size. Track tail latency, memory
retention during sustained editing, large pastes, cold startup, and scrolling
through long lines. The first application target remains under 5 ms at p95 for
ordinary editing in a 10 MiB file on a documented machine; the core measurements
above do not establish that end-to-end result.

## Insert recording and replay

Recorded on 2026-09-20 with the same optimized profile:

```sh
cargo bench -p vex_editor --bench commands --locked -- insert_repeat --noplot
```

| Document | Carets | Record nine characters + undo | Replay + undo |
|---|---:|---:|---:|
| 1 MiB | 1 | 6.97 µs | 6.49 µs |
| 100 MiB | 1 | 8.22 µs | 7.90 µs |
| 1 MiB | 1,000 | 3.83 ms | 3.78 ms |
| 100 MiB | 1,000 | 6.64 ms | 6.73 ms |

Each session enters insert mode, inserts `new_value` as nine separate text events,
and leaves insert mode. The timed loop includes command dispatch, rope edits,
selection mapping, recording or replay, and undo. Selection positions are evenly
distributed through repeated short ASCII source lines. Construction is outside
the loop, and undo restores the same positions for cache-warm measurements.
The fixture rope remains shared. These figures exclude the event queue, drawing,
terminal latency, and growing undo history; they are not p95 input latencies.

Recording stores command arguments and one text pool, without copying the
document or multiplying recorded text by the number of carets. Playback shares
the immutable recording across buffers. The terminal runs at most 64 actions
per batch, checking a four-millisecond deadline between actions, then services
input/background results and draws. An individual existing command or large
paste remains nonpreemptible, so the deadline is a cooperative scheduling bound.

## Clipboard scheduling

Recorded on 2026-09-20 with the same optimized profile:

```sh
cargo bench -p vex_term --bench rendering --locked -- clipboard_schedule_cancel --noplot
```

Scheduling `Space-y` for an entire selected buffer, then cancelling with Escape,
took 0.165 µs for both 1 MiB and 100 MiB source fixtures. The timed path includes
three input events, command dispatch, shared snapshot/selection capture, and
cancellation. Document construction and select-all happen before measurement.
No provider is started and no selected text is flattened: this measures the UI
handoff cost, excluding worker wakeup, text capture, clipboard processes, paste
preparation/application, drawing, and terminal latency. Actual copying remains
proportional to selected text on the worker.

With clipboard registers and resumable replay integrated, the same Space-y case
measured 0.208 µs at 1 MiB and 0.204 µs at 100 MiB. The additional request state
holds command arguments and shared selections, supporting stale-result rejection
and recording only successful clipboard edits.

The `clipboard_register_schedule_cancel` cases select the entire buffer before
timing, dispatch a register prefix and command, and then cancel with Escape:

| Buffer | `"+d` cut | `"+c` change | `"+P` paste | `"+n` search |
|---|---:|---:|---:|---:|
| 1 MiB | 0.461 µs | 0.452 µs | 0.460 µs | 0.453 µs |
| 100 MiB | 0.467 µs | 0.447 µs | 0.454 µs | 0.446 µs |

These include opening the bounded register helper, four input events, request
construction, and cancellation. They exclude drawing the helper, worker wakeup,
provider processes, selected-text capture, transaction preparation/application,
search compilation/scanning, and terminal latency. Reproduce both groups with
`cargo bench -p vex_term --bench rendering --locked -- clipboard --noplot`.

Clipboard replay exposes `repeat_ready()` separately from `repeat_pending()`.
While a clipboard result is pending, the event loop waits for input/completions
with its normal deadlines instead of spinning through a zero-duration timeout.

## Named registers

Recorded on 2026-09-20 with the same optimized profile, including file-name and
search-register integration:

```sh
cargo bench -p vex_editor --bench commands --locked -- registers --noplot
```

| Buffer and stored fragment | Read register | Open helper + Escape | Discard whole buffer + undo |
|---|---:|---:|---:|
| 1 MiB | 5.96 ns | 0.341 µs | 0.950 µs |
| 100 MiB | 5.95 ns | 0.339 µs | 0.965 µs |

The read clones an immutable register handle. The helper case dispatches `"`
and Escape, builds a bounded preview of one stored register plus dynamic labels,
and retrieves its hints; it does not draw. The discard case selects the entire
buffer before timing, dispatches `"_d`, and undoes the deletion. It does not copy
the selected text into a register, and each iteration restores the same rope
and selections. Fixtures and register contents are constructed outside timing.
These cache-warm central estimates exclude terminal input/output, rendering,
ordinary yank capture, actual paste preparation, and growing undo history.

Stored-register lookup is logarithmic in the number of names, independent of
stored text size. The popup snapshots at most 64 names and 48 characters per
first fragment once when opened, then reuses those previews across redraws.
Prompt insertion fetches only the first fragment, including for the dynamic
selection register. Ordinary yanks and pastes still scale with affected text.

With shared search registers, `search_next_previous` measured 1.30 µs at 1 MiB
and 1.14 µs at 100 MiB, versus the earlier 1.23/1.07 µs regex baseline. The
`background_search_schedule_cancel` case measured 0.222/0.219 µs, versus the
earlier 0.164/0.166 µs. These use the existing fixtures described below and the
same Criterion settings. Reproduce with the `search_next_previous` and
`background_search_schedule_cancel` filters. The former now reads the shared
register and checks the compiled cache by text identity; neither path copies
document contents on the UI thread. Compilation of a changed register value
and capture of a dynamic selection query happen on the search worker.

## Retained buffers

Recorded on 2026-09-20 on the same development machine and optimized profile:

```sh
cargo bench -p vex_term --bench rendering --locked -- buffers --noplot
```

| Document | Loaded buffers | Next/previous pair | Move and draw 120×40 |
|---|---:|---:|---:|
| 1 MiB | 2 | 0.432 µs | 50.5 µs |
| 1 MiB | 1,000 | 1.34 µs | 52.5 µs |
| 100 MiB | 2 | 0.431 µs | 53.0 µs |
| 100 MiB | 1,000 | 1.31 µs | 52.4 µs |

Navigation switches from a large scratch buffer to a small file and back,
including command dispatch, view restoration, and buffer bookkeeping. It uses
existing buffers without file I/O, text copying, or rendering. Initial loading,
catalog construction, and viewport indexing are outside the timed loop. Ordinary
next/previous lookup uses an ordered map, with counts reduced modulo buffer count.

The drawing cases move one cursor and render only the visible document, emitting
ANSI output to an in-memory sink. The other buffers are hidden. These results
exercise per-frame overhead from retention; they do not measure terminal display
latency, many visible panes, memory retained by undo, or closing large histories.
Hidden documents receive no syntax, Git gutter, or file polling jobs. Buffer-picker
ranking and preview parsing use the existing cancellable worker services.

## Textobject scans

Recorded on 2026-09-20 with the same machine, optimized profile, and Criterion
settings:

```sh
cargo bench -p vex_editor --bench commands --locked -- textobjects --noplot
```

| Document | Word + restore | Paragraph + restore | Schedule + cancel |
|---|---:|---:|---:|
| 1 MiB | 0.930 µs | 2.20 µs | 39.8 ns |
| 100 MiB | 0.877 µs | 1.71 µs | 37.7 ns |

The fixtures repeat short words and two-line paragraphs. Both sizes select the
same local shape near the middle of the buffer; these are cache-warm local scans,
not whole-file selection measurements. Scan cases include the documented command
function, synchronous selection application, and restoration of the original
selection. Scheduling calls the function directly and cancels before scanning;
it excludes key dispatch, worker wakeup, rendering, and terminal latency.

The terminal uses the existing ordered worker mailbox for these scans. Long
words/paragraphs and many selections can require substantial work, but do not
copy the full buffer or scan it on the input thread. Cancellation is checked
between graphemes/lines; individual Unicode boundary lookups remain nonpreemptible.

Surround addition uses two boundary inserts per selection, merging coincident
inserts without copying selected contents. On the same machine/profile:

```sh
cargo bench -p vex_editor --bench commands --locked -- surround_add --noplot
```

| Document | Surround whole buffer + undo | Surround 1,000 selections + undo |
|---|---:|---:|
| 1 MiB | 1.47 µs | 0.987 ms |
| 100 MiB | 2.03 µs | 1.36 ms |

These ASCII fixtures include transaction construction, rope updates, selection
mapping/normalization, and undo. The original rope remains shared during the run.
They exclude rendering, terminal latency, and repeated edits accumulating history.
The whole-buffer case changes only its two boundaries; it is not a measurement
of replacing all selected text.

Surround deletion and replacement use the selection worker for pair resolution
and transaction preparation. Only delimiter positions and short shared strings
are retained; replacement previews do not copy selected text. Collision detection
uses an ordered set, avoiding quadratic comparisons between cursors. Explicit
asymmetric pairs such as `md(` do not initialize or query syntax.

`cargo bench -p vex_editor --bench commands --locked -- surround_edit_undo --noplot`
measured the following on the same machine/profile:

| Buffer | Delete + undo, one cursor | Replace + undo, one cursor | Delete + undo, 1,000 cursors | Replace + undo, 1,000 cursors |
|---|---:|---:|---:|---:|
| 1 MiB | 1.08 µs | 1.47 µs | 0.742 ms | 1.13 ms |
| 100 MiB | 1.33 µs | 1.77 µs | 0.870 ms | 1.30 ms |

These synchronous command runs include local delimiter searches, replacement
preview construction, validation, rope edits, selection normalization, and undo.
Cursors occupy the first 1,000 short `(word)` lines; these are local edits, not
whole-file scans. They exclude rendering, terminal input dispatch, worker wakeup,
and accumulating undo history. Scans and edit construction check cancellation;
transaction sorting and final selection normalization are not preemptible.

## Command and movement baseline

Recorded on the same development machine on 2026-09-19 with the same Criterion
settings and optimized profile:

```sh
cargo bench -p vex_editor --bench commands --locked -- --noplot
```

| Document | Right then left, one cursor | Right then left, 1,000 cursors |
|---|---:|---:|
| 10 MiB source text | 0.561 µs | 0.946 ms |
| 100 MiB source text | 0.703 µs | 1.023 ms |
| 10 MiB mixed Unicode | 0.707 µs | 1.196 ms |
| 10 MiB single line | 0.598 µs | 0.941 ms |

Each iteration dispatches two logical keys through the default keymap and command
functions, finds grapheme boundaries, and updates all selections. Documents and
initial selections are created outside the timed loop. Positions are spread
through the document, then reused after warmup. These are cache-warm central
estimates for a pair of movements, not p95 latency or rendered frames.

One down/up pair on a repeated tab/CJK fixture takes about 2.01 µs using direct
named command invocation. It measures logical-line lookup, retained display
columns, and scanning short target-line prefixes. It does not measure vertical
movement at deep columns in exceptionally long lines. These original measurements
predate the layout cache; see the long-line measurements below.

## Comments and insert checkpoint follow-up

Measured on the same machine and release profile on 2026-09-19. Comment toggles
at `77db754` use unique selected lines and one transaction, without flattening
the document:

```sh
cargo bench -p vex_editor --bench commands --locked -- comment_selected_lines_and_undo --noplot
```

| Source size | One selected line, toggle then undo | 1,000 selected lines, toggle then undo |
|---|---:|---:|
| 1 MiB | 3.74 µs | 2.92 ms |
| 100 MiB | 3.47 µs | 3.99 ms |

The fixture, selections, and language setup are outside the timed loop; these
are Criterion central estimates using the command suite's settings. Sparse
selections span the buffer. They do not measure commenting a whole large file,
rendering, or terminal latency. Insert deletion similarly builds one transaction
over merged affected ranges and continues the typing undo group. Ctrl-s closes
that group without file I/O.

After the Unicode chunk-boundary fix and insert bindings, the same movement
fixtures measured 0.728 µs for a right/left pair with one cursor in 10 MiB of
Unicode, 1.20 ms for 1,000 cursors, and 2.43 µs for the tab/CJK down/up pair.
The earlier table predates several editor changes; it is not an isolated
before/after comparison of the Unicode fix. Reproduce these measurements with:

```sh
cargo bench -p vex_editor --bench commands --locked -- 'move_right_left/unicode_10MiB|move_down_up_tabs_unicode' --noplot
```

The chunk-boundary workaround preserves the segmentation cursor's running
state, uses an eight-byte stack buffer only when a scan crosses chunks, and
keeps complete-chunk queries on a short path. It neither flattens the rope nor
restarts a prefix scan for every cluster in a long regional-indicator run.

Selection copying (`C`) uses the same snapshot mailbox as search. In the release
benchmark below, scheduling and cancelling one request took 40 ns at 1 MiB and
39 ns at 100 MiB. That measures request construction and destruction only; it
excludes mailbox synchronization, worker scheduling, result delivery, and drawing.
The complete synchronous one-copy scan plus restoration of the original selection
took 2.62 µs and 2.20 µs respectively on the repeated short source-line fixture.
The destination is nearby in both cases. Scanning many short lines to find fitting
columns, copying many selections, and cold deep-column lookups still cost more.

```sh
cargo bench -p vex_editor --bench commands --locked -- copy_selection_next_line --noplot
```

For insert-mode Ctrl-u, restricting indentation discovery to text before the
caret removed a full-line scan. The following before/after measurements keep
the caret eight spaces into one long blank line and include deletion plus undo:

| Line size | Before | After |
|---|---:|---:|
| 1 MiB | 2.34 ms | 0.955 µs |
| 100 MiB | 222 ms | 1.10 µs |

```sh
cargo bench -p vex_editor --bench commands --locked -- kill_line_start_near_beginning --noplot
```

This benchmark uses ten flat-sampling Criterion samples, a 500 ms warmup, and a
one-second target extended for the expensive baseline. The fixture and initial
caret are outside the timed loop. Results are central estimates for this specific
case, excluding document loading, drawing, and terminal transport. Ctrl-u near
the end of a long indentation prefix still necessarily visits that prefix.

## Terminal viewport baseline

Recorded on the same development machine on 2026-09-19, with 30 samples, a
500 ms warmup, and one second of measurement per case. The environment had
`NO_COLOR=1`; runs with colors enabled may have different escape-encoding costs:

```sh
cargo bench -p vex_term --bench rendering --locked -- --noplot
```

| Document | Viewport | Full redraw | One move + incremental redraw |
|---|---|---:|---:|
| 1 MiB source text | 120 × 40 | 0.192 ms | 0.168 ms |
| 100 MiB source text | 120 × 40 | 0.207 ms | 0.188 ms |
| 10 MiB mixed Unicode | 120 × 40 | 0.107 ms | 0.089 ms |
| 10 MiB single line, at its start | 120 × 40 | 0.048 ms | 0.030 ms |
| 1 MiB source text | 240 × 80 | 0.456 ms | 0.400 ms |
| 100 MiB source text | 240 × 80 | 0.506 ms | 0.439 ms |
| 10 MiB mixed Unicode | 240 × 80 | 0.288 ms | 0.217 ms |
| 10 MiB single line, at its start | 240 × 80 | 0.155 ms | 0.091 ms |

Both cases include resetting the reused grid, viewport layout, status painting,
cell comparison, and encoding terminal commands. Full redraw invalidates the
previous frame each iteration. Movement alternates `l` and `h` through an
application key event, including command dispatch and cursor updates. The
renderer still paints and compares the viewport, then emits only changed cells.
An identical frame emits zero bytes, verified separately in source tests.

Output is written to `std::io::sink()`: these are CPU measurements, excluding
terminal transport, emulator rendering, event polling, and user-perceived
latency. The reported values are rounded Criterion central estimates, not p95.
Document construction and initial grid allocation are outside the timed loop;
these are cache-warm, single-cursor measurements near the document's start.
Unicode source rows are shorter than ASCII rows, so their lower time does not
mean Unicode processing is faster per character. The single-line fixture paints
only its visible beginning and otherwise empty rows.

File size has little effect on these cases. This original baseline predates the
layout cache and did not measure deep horizontal scrolling. Multi-cursor rendering
measurements and event-to-flush tail latency are still needed before claiming the
interactive latency target.

## Long-line layout cache

Measured on the same machine on 2026-09-19, in release mode with `NO_COLOR=1`.
The fixture has **two lines**, each approximately the stated byte size, and a
120 × 40 viewport positioned near their ends. The Unicode pattern includes tabs,
CJK, a combining mark, and a joined emoji. All output goes to `std::io::sink()`;
these measurements exclude input polling, terminal transport, and emulator work.

The quick probe separates the first draw from subsequent operations:

```sh
cargo run --release -p vex_term --example long_lines --locked -- 1 100
cargo run --release -p vex_term --example long_lines --locked -- 10 100
```

Before the cache, the same probe with three samples per operation showed the
cost of repeatedly scanning the hidden prefix. After measurements use 100
samples. The following values are medians, except the single first draw:

| 1 MiB per line | ASCII before | ASCII after | Unicode/tabs before | Unicode/tabs after |
|---|---:|---:|---:|---:|
| First draw | 477.3 ms | 6.47 ms | 189.1 ms | 18.90 ms |
| Horizontal move + draw | 442.1 ms | 0.166 ms | 188.6 ms | 0.052 ms |
| Vertical move + draw | 597.1 ms | 0.125 ms | 251.6 ms | 0.051 ms |
| Insert, draw, undo, draw | 911.7 ms | 0.192 ms | 385.5 ms | 0.100 ms |

At **10 MiB per line**, the first draw took 16.70 ms for ASCII and 157.97 ms for
Unicode/tabs. Subsequent horizontal-move medians were 0.084 ms and 0.053 ms;
insert/draw/undo/draw medians were 0.128 ms and 0.104 ms. First-draw values are
single observations, not latency percentiles. This probe has no timed warmup,
unlike the longer-running Criterion measurements below.

The `deep_line` Criterion group measures steady behavior with the initial index
built before timing. It uses the same 30 samples, 500 ms warmup, and one second
of measurement as the viewport benchmarks:

```sh
cargo bench -p vex_term --bench rendering --locked -- deep_line --noplot
```

The typing columns were rerun after adding undo grouping, using the filter
`deep_line/type`; movement columns retain the earlier cache baseline.

| Each line | Horizontal move + draw | Vertical move + draw | Type 1 / draw / undo / draw | Type 8 / draw / undo group / draw |
|---|---:|---:|---:|---:|
| 1 MiB ASCII | 0.062 ms | 0.063 ms | 0.123 ms | 0.124 ms |
| 1 MiB Unicode/tabs | 0.051 ms | 0.051 ms | 0.106 ms | 0.109 ms |
| 10 MiB ASCII | 0.058 ms | 0.059 ms | 0.118 ms | 0.119 ms |
| 10 MiB Unicode/tabs | 0.051 ms | 0.052 ms | 0.105 ms | 0.109 ms |

These are Criterion central estimates. Movement alternates neighboring positions
or lines. Typing always edits the first line, forcing the second line's cached
start to shift. The eight-character case processes eight distinct text events
before drawing, then undoes the whole group in one step before another draw.
Each edit still advances the revision and synchronizes layout; grouped undo uses
the combined change extent to retain unaffected indexes. Source tests separately
check cache reuse using scanned-character counters and compare results with flat
Unicode segmentation through edits, line joins, tab-width changes, and grouped
history operations.

The existing short-line viewport benchmarks were rerun too: at 120 × 40,
movement + drawing measured 0.169 ms for 1 MiB source, 0.178 ms for 100 MiB
source, and 0.088 ms for mixed Unicode, compared with 0.168, 0.188, and 0.089 ms
in the original baseline. Unscrolled rows bypass column lookup entirely.

The cache is bounded and lazy: at most 128 lines and 4,096 sparse checkpoints per
line, with small recent-position lists. It retains no historical text. Initial
queries still scan the necessary prefix, and an early edit in a huge line can
invalidate its later checkpoints. Large clusters and long Unicode lookbehind
remain possible costs. These measurements establish the improvement for indexed
positions, not a blanket bound on cold queries or end-to-end p95 latency.

## Delimiter navigation

`cargo bench -p vex_editor --bench commands --locked -- delimiter_matching --noplot`
measures two `match_brackets` commands, returning cursors to their starting
brackets. On this machine:

| Buffer | One local pair round trip | 1,000 local pair round trips | Schedule and cancel |
|---|---:|---:|---:|
| 1 MiB | 0.546 µs | 0.803 ms | 40.9 ns |
| 100 MiB | 0.698 µs | 1.006 ms | 45.1 ns |

The text contains short `(word)` lines; cursors occupy the first 1,000 lines.
This checks local navigation and rope-height overhead, not a scan across the
whole file. Synchronous command measurements include selection transformations
and request validation. Scheduling excludes worker wakeup/scanning; all timings
exclude key dispatch and rendering. Cached Rust syntax round trips measured
2.08 µs for 1 KiB and 2.32 µs for 64 KiB, excluding the first parse.

Plain-text matching scans borrowed rope chunks with cancellation at each scalar.
Closest-pair counts reuse reverse iterators and fixed nesting counters, so deeply
nested counts do not rescan each inner pair. A deterministic test bounds visits
for 4,096 nested pairs to 8,193 characters. Syntax counts traverse ancestors once;
bounded sibling lookups handle delimiters such as closure bars. Trees share
storage with highlighting, and matching jobs reuse them until the revision or
language changes. A missing tree parses on the worker under the existing 25 ms
and 2 MiB limits. Hidden buffers release their structural tree cache.

## Initial Rust syntax highlighting

Tree-sitter 0.27.0 with the bundled Rust 0.24.2 grammar, measured at 120 × 40
on the same development machine. The fixtures repeat syntactically valid Rust
functions with parameters, a type, a comment, and a numeric literal:

```sh
cargo bench -p vex_term --bench rendering --locked -- rust_syntax --noplot
```

| Rust source | First parse + draw | Cached movement + draw | Type 8 / draw / grouped undo / draw |
|---|---:|---:|---:|
| 64 KiB | 4.163 ms | 0.272 ms | 1.538 ms |
| 256 KiB | 14.965 ms | 0.314 ms | 3.210 ms |

These are Criterion central estimates from 30 samples, a 500 ms warmup, and a
one-second measurement target (extended by Criterion when needed). Color output
is enabled explicitly even if the benchmark runner sets `NO_COLOR`. Painting
includes cell-grid layout, syntax styling, selection/cursor overrides, and ANSI
output to an I/O sink; actual terminal display latency and disk reads are excluded.
The benchmark asserts that highlighted cells are present, so a budget fallback
cannot silently turn this into a plain-text measurement.

First draw constructs a new editor and syntax state over a shared input rope and
includes the initial parse, queries, grid, and output. The process-wide grammar
query has already initialized. Movement uses cached highlight ranges and does
not reparse. Typing changes a function name halfway through the file, processes
eight distinct input events before drawing, then undoes the group and draws again.
It includes two incremental parses and two sets of visible-range queries.

These historical measurements use the synchronous integration path, which remains
available for embedding and comparison. The terminal now uses a syntax worker.
Both paths retain the work limits: files above 2 MiB and parse/query
budget failures fall back to plain text. Syntax work still depends on the tree
and changed region; these figures do not establish constant time or p95 bounds.
Background parsing, more languages, and finer invalidation remain future work.
See [syntax behavior and limits](syntax.md).

The existing plain-text movement + draw cases at 120 × 40 measured 0.170 ms
for 1 MiB source and 0.177 ms for 100 MiB source after integration. Criterion
detected no significant change from their prior 0.169 ms and 0.178 ms estimates.

## Literal search

Recorded on 2026-09-19 on the same development machine and release profile:

```sh
cargo bench -p vex_core --bench search --locked -- --noplot
cargo bench -p vex_editor --bench commands --locked -- search_next_previous --noplot
```

| Buffer | Nearby forward match | Nearby backward match | Missing query, forward | Missing query, backward |
|---|---:|---:|---:|---:|
| 1 MiB | 66 ns | 70 ns | 2.10 ms | 1.34 ms |
| 10 MiB | 67 ns | 73 ns | 19.6 ms | 13.4 ms |
| 100 MiB | 91 ns | 100 ns | 197 ms | 134 ms |

The core fixture repeats `fn main() { let value = 123; }` with LF endings.
Nearby cases find `value` from the corresponding document edge; missing cases
search the whole buffer for `value_missing`. The query is compiled before timing,
and the rope is already in memory. Each measurement includes constructing the
streaming iterator and seeking into the rope. It excludes selection normalization,
prompt processing, rendering, and disk I/O. Core search uses 10 samples, a 200 ms
warmup, and a 500 ms measurement target extended for expensive cases.

The editor benchmark measures a pair of documented `search_next` /
`search_previous` commands around the middle of the same fixture. It includes
command dispatch, matching, grapheme normalization, and selection updates for one
selection. The pair took **0.820 µs at 1 MiB** and **0.931 µs at 100 MiB**, using
the existing command suite's 30 samples, 500 ms warmup, and one-second target.
These are central estimates, not terminal input latency or p95 bounds.

Search has no document-sized allocation or match list. It stops when the requested
match is found, but missing queries still take linear time. These measurements
describe the original synchronous search path; many selections can each require
a traversal. Interactive search now runs on a cancellable worker, as measured
below. See [search behavior](search.md).

## Rope regex engine

Recorded on 2026-09-19 with the same machine and release profile:

```sh
cargo bench -p vex_core --bench search --locked -- regex_search --noplot
```

| Buffer | Nearby `value = [0-9]+` | Nearby `\bvalue\b` | Missing `value_missing[0-9]+` |
|---|---:|---:|---:|
| 1 MiB | 76.5 ns | 63.8 ns | 1.25 ms |
| 100 MiB | 77.0 ns | 63.6 ns | 125.9 ms |

These are Criterion central estimates with ten samples, a 200 ms warmup, and
a 500 ms target (extended for the full 100 MiB scan). The fixture repeats the
same short ASCII source line used by literal search. Compilation and rope
construction are outside timing; a reusable cache and constant-false cancellation
callback are supplied. Drawing, job transport, and real cancellation atomics are
excluded. These cases use the DFA path; Unicode word-boundary fallbacks can cost
more and are not represented by the ASCII boundary case.

The adapter visits rope chunks without copying the document. Compilation is
bounded to a 64 KiB query, an 8 MiB NFA, and independently limited DFA construction.
Unsupported DFA searches fall back to a prioritized NFA simulation, with scratch
space proportional to the compiled pattern. Both paths check cancellation while
scanning; NFA epsilon expansion checks it too. Regex compilation itself is not
preemptible and belongs on a worker. Inline tests compare both execution paths
against regex-automata's string matcher, including arbitrary byte spans, empty
matches, Unicode, and assertions crossing rope chunks.

After connecting regex to the editor, the `search_next_previous` command pair
measured **1.23 µs at 1 MiB** and **1.07 µs at 100 MiB**, using 30 samples,
a 500 ms warmup, and a one-second target. This repeats the original `value`
fixture with smart-case regex semantics; the earlier literal implementation
measured 0.820 and 0.931 µs. Nearby reverse matches use LF-separated windows
when the automaton cannot consume LF. Cross-line patterns still scan a prefix
and are not represented by these nearby-match numbers.

Scheduling two successive queries and cancelling them took **0.164 µs** and
**0.166 µs**, versus the original 0.140 and 0.141 µs. Both measure request
construction/destruction, excluding the worker and terminal. Reproduce with:

```sh
cargo bench -p vex_editor --bench commands --locked -- 'search_next_previous|background_search_schedule_cancel' --noplot
```

With the regex worker active on the 100 MiB missing-query PTY fixture, five runs
measured medians of **0.232 ms for resize/redraw** and **0.176 ms for cancellation/
redraw**. Ranges were 0.194–0.301 ms and 0.160–0.191 ms. Use the background-search
benchmark command below. These timings end at PTY output; they exclude physical
terminal display latency, do not instrument the worker's scheduling phase, and
are not p95 guarantees.

## Background search

Recorded on 2026-09-19 using the same machine and release profile:

```sh
cargo bench -p vex_editor --bench commands --locked -- background_search_schedule_cancel --noplot
cargo build --release -p vex_term --locked
python3 tools/background_search_benchmark.py --mib 100 --runs 5
```

Creating two successive deferred query requests and then cancelling took
**0.140 µs at 1 MiB** and **0.141 µs at 100 MiB**. The benchmark includes command
dispatch, immutable snapshot and selection capture, request cancellation, and
dropping both job payloads. It uses 30 samples, 500 ms warmup, and a one-second
measurement target. It excludes worker scheduling, compilation/scanning, and
drawing; these numbers measure work left on the editor thread for one selection.

The PTY benchmark opens a 100 MiB ASCII file with short logical lines, starts a
missing-query search, waits until the status shows it pending, then resizes and
cancels it. Five runs measured a median **0.193 ms for resize/redraw** and
**0.159 ms for cancellation/redraw**. Individual samples ranged from 0.178–0.262 ms
and 0.140–0.172 ms respectively. File loading is excluded. These measurements end
when the PTY receives the corresponding frame/cursor output; they do not include
the physical terminal's display latency or establish p95 guarantees. The worker's
exact scheduling phase is not instrumented.

The shared inbox wakes on worker completions while idle. It bounds terminal input
at 256 events; the worker retains one running and one replaceable pending job.
Cancellation is checked during query compilation, every 4096 scanned bytes,
during long KMP fallback chains, and between matches/selections. Allocation and
grapheme-boundary routines remain indivisible. Queued editing keys that depend on
a pending search destination wait for that result, with key order preserved.
Syntax now uses a separate worker in the same runtime; file I/O remains synchronous.

## Background syntax

Recorded on 2026-09-19 using the same release profile and Rust fixtures at 120 × 40:

```sh
cargo bench -p vex_term --bench rendering --locked -- background_syntax --noplot
```

| Rust source | First plain-text frame + syntax request | Type 8 / draw / request / grouped undo / draw / request |
|---|---:|---:|
| 64 KiB | 0.341 ms | 0.532 ms |
| 256 KiB | 0.375 ms | 0.599 ms |

These Criterion central estimates use 30 samples, 500 ms warmup, and a one-second
measurement target. First frame includes construction of the editor over a shared
rope, lazy language selection, cell-grid drawing, ANSI output to a sink, and
building/dropping the syntax request. The typing benchmark inserts eight characters
at the beginning of the file, undoes the group, and includes both frames and
requests. It exercises cancellation and the bounded edit-metadata log.

These measure work left on the UI thread. No worker runs in these benchmarks:
parsing, grammar initialization, queries, thread scheduling, completion delivery,
and physical terminal display latency are excluded. The earlier synchronous
first-draw numbers include completed colors; these first frames display plain
text while highlighting is pending. This is not a reduction in parser CPU cost
or a measurement of time until colors arrive.

Source tests check independent worker progress, completion wakeups, cancellation,
stale-result rejection, incremental reuse across coalesced edits, and bounded
caches. The PTY smoke test waits for initial, edited, and undo-restored colors
without sending extra input to wake the loop.

## Language services

Rust-analyzer runs in a child process. The UI submits shared rope snapshots;
JSON encoding, UTF-16 indexing, and protocol handling run on the LSP service
thread, with separate threads for blocking pipe I/O. Updates debounce for 20 ms
with a 100 ms maximum batching delay. The terminal gives input a turn between
background completions, including LSP events.

The initial implementation sends full document contents and rebuilds its line
index after changes, so worker CPU and allocation costs scale with document
size. Documents above 8 MiB skip LSP. Server startup, Cargo checks, and analysis
costs have not been benchmarked; the existing rendering numbers do not measure
them. See [language services](lsp.md) for queue limits and correctness checks.

## File picker

File discovery, ignore-rule evaluation, fuzzy scoring, and previews run on
background workers. Query changes reuse the current file index and cancel older
matching work. The main thread receives at most 512 ranked entries per result;
drawing composes the visible editor viewport with the floating picker. The grid
diff emits only changed cells. Picker content drawing visits visible rows and
columns. Preview parsing and highlighting use the existing syntax budgets on
the preview worker, limited to the first 64 KiB / 200 lines of source; drawing
uses the returned spans without parsing.
Ignore and fuzzy matching reuse scratch buffers. Match-position traces are
computed only for retained results, and wildcard matching uses iterative dynamic
programming rather than recursive backtracking.

These are implementation bounds, not measured latency guarantees. Individual
filesystem calls and candidate scores are not preemptible; index construction
and each new query still scale with the candidate set. The current picker has
not had a large-workspace latency benchmark recorded. See [picker limits and
validation](pickers.md) for cancellation, memory limits, and terminal checks.


## LSP navigation and reference selections

Navigation response parsing and document-highlight preparation run on the LSP
service thread. Destination reads and UTF-16 range conversion share the picker
worker; query ranking retains at most 512 rows in a heap. Shared snapshots retain
unsaved contents without copying whole files. File reads check cancellation on
each buffered read; destination line indexing checks every 4,096 scalars.

A local release-mode microbenchmark compared 128 UTF-16 lookups on a long line
containing ASCII and supplementary Unicode characters, with the line index
already built, against an equivalent sequential reference scan:

| Text size | Indexed lookups | Sequential reference scans |
|---|---:|---:|
| 1 MiB | 38.5 µs | 102.3 ms |
| 8 MiB | 32.3 µs | 788.3 ms |

Installing worker-prepared selections averaged 29–199 ns over 50 applications
for 1, 1,000, and 65,536 ranges in 1 MiB and 100 MiB documents. These small times
are near timer/allocator noise: they show that delivery avoids repeated text
scanning, not end-to-end editing latency. Normalization, server latency, file
loading, worker wakeup, and rendering are excluded. Large normalized selection
sets still incur their usual costs when drawing and editing. Sorting and an
individual grapheme normalization are not interruptible; cancellation is checked
between ranges. No additional runtime or production dependency was introduced.

Reproduce these isolated measurements with:

```sh
cargo test -p vex_editor -p vex_lsp --release --locked benchmark_ -- --ignored --nocapture
```

## Prepared workspace edits

Text replacement, sticky cursor mapping, and grapheme normalization for all views
run before UI delivery. A local release microbenchmark measured a single
`Editor::apply_external_edit` call with three views, using 50 fresh documents per
case. The edit count also equals the selection count in each view:

| Document size | Edits / selections per view | Median delivery | Sample p95 |
|---|---:|---:|---:|
| 1 MiB | 1 | 0.917 µs | 3.000 µs |
| 1 MiB | 1,000 | 3.125 µs | 9.000 µs |
| 100 MiB | 1 | 0.167 µs | 0.250 µs |
| 100 MiB | 1,000 | 1.959 µs | 2.166 µs |

These short measurements are sensitive to allocator, cache, and scheduling noise;
the larger document is not inherently faster. They exclude plan capture, worker
preparation, frontend batch preflight, I/O, and rendering. The cases use plain
text with no language parse, and teardown is outside the measured interval.
Delivery still validates and copies selection metadata for history. Text content
is installed from its prepared rope without repeating insertions or scans.

```sh
cargo test -p vex_editor --release --locked benchmark_external_edit_delivery -- --ignored --nocapture
```

See [workspace edit behavior and remaining integration work](workspace-edits.md).

## Rename submission

The UI captures shared ropes and view metadata; buffer-to-string conversion,
server synchronization, UTF-16 conversion, and edit preparation run on workers.
A local release microbenchmark timed `rename_symbol` plus `take_lsp_update`,
using one named buffer, three views, and 500 submissions per case:

| Document size | Selections per view | Median submission | Sample p95 |
|---|---:|---:|---:|
| 1 MiB | 1 | 1.250 µs | 1.583 µs |
| 1 MiB | 1,000 | 6.125 µs | 6.625 µs |
| 8 MiB | 1 | 0.375 µs | 0.667 µs |
| 8 MiB | 1,000 | 2.875 µs | 3.125 µs |

These short, warmed measurements are sensitive to allocator/cache noise; the
larger file is not inherently faster. They exclude setup, cancellation/destruction,
drawing, service wakeup, JSON/pipe I/O, server latency, and edit preparation and
application. Capture still scales with the number of open buffers, views, and
selections. This measures UI submission, not end-to-end rename latency. These
samples were rechecked after adding ordered server-command application.

Protocol tests synchronize 160 buffers through an eight-message output queue,
check that unchanged snapshots are not resent, and verify shutdown interrupts
an output-capacity wait when the server stops reading. Server-request replies
have separate bounded capacity so synchronization cannot crowd them out.
Workspace-application acknowledgements follow the normal FIFO instead, after
the resulting `didChange` messages. The service awaits UI acknowledgement without
blocking input handling or pipe readers; edit preparation reuses the existing
worker. Ordinary typing does not capture the full buffer catalog. Server-command
validation counts serialized bytes without allocating an extra JSON byte buffer.

```sh
cargo test -p vex_term --release --locked benchmark_rename_submission -- --ignored --nocapture
```
