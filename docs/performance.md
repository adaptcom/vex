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
movement at deep columns in exceptionally long lines; that needs a layout cache
and a separate benchmark.

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

File size has little effect on these cases. Deep horizontal scrolling remains
unbounded by viewport width because layout scans the hidden line prefix. A
long-line layout cache, multi-cursor rendering measurements, and event-to-flush
tail latency are still needed before claiming the interactive latency target.
