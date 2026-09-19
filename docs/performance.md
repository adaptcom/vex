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
