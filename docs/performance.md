# Performance baselines

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
