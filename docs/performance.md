# Core performance baseline

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
