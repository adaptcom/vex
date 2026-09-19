# Vex

A terminal text editor in Rust, inspired by Helix's selection-first editing model.

The first milestone is the terminal-independent `vex_core` library. It provides
rope-backed documents, directional multiple selections, atomic transactions,
revision-checked snapshots, and undo/redo that restores text and selections.
There is no terminal application yet.

## Development

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo bench -p vex_core --bench editing --locked -- --noplot
```

Unit and property tests live in their source modules under `#[cfg(test)]`.
Criterion benchmarks live in `crates/vex_core/benches/`. The property tests check
random Unicode edits against flat text and snapshot history models, as well as
selection normalization and position mapping invariants.

## Editing API

```rust
use vex_core::{CharOffset, Document, Selection, SelectionSet};

fn main() -> Result<(), vex_core::Error> {
    let mut document = Document::from("hello world");
    let mut selections = SelectionSet::single(Selection::new(
        CharOffset(6),
        CharOffset(11),
    ));

    let transaction = document.replace_selections(&selections, "Vex")?;
    document.apply(transaction, &mut selections)?;
    assert_eq!(document.text(), "hello Vex");

    document.undo(&mut selections)?;
    assert_eq!(document.text(), "hello world");
    Ok(())
}
```

Documents expose immutable rope access. `Document::from_reader` and `write_to`
stream UTF-8; the application will own file paths, flushing, and safe saves.
Snapshots share rope storage and can be sent to background workers. A transaction
built from an older snapshot is rejected, including after undo restores the same
text. Document identities prevent accidental application to another buffer.

## Core conventions

- `CharOffset` counts Unicode scalar values; `ByteOffset` counts UTF-8 bytes.
  Neither represents graphemes or terminal columns. Byte-to-character conversion
  rejects offsets inside a multi-byte scalar. Grapheme-aware motions are a later
  layer; low-level transactions can edit at any scalar boundary.
- A `Selection` has an anchor and a head. Its text range is half-open, and equal
  endpoints represent an insertion caret. A future normal-mode block cursor will
  select a whole grapheme. EOF is a valid boundary, including in empty documents.
- `SelectionSet` sorts and merges overlaps while retaining the primary selection
  and its direction. Adjacent ranges remain separate. Duplicate carets merge;
  a caret at a range's start or interior merges with it, while one at its end
  remains separate.
- Every edit in a `Transaction` uses the original revision's coordinates. Input
  order is irrelevant. Overlaps and shared starts are errors; adjacent edits are
  valid. All validation occurs before document mutation.
- Generic selection mapping uses `Affinity::After` on both endpoints. Explicit
  result selections can override it. `replace_selections` produces one caret
  after each replacement, including adjacent replacements. Mapping one position
  uses a binary search over edits and never scans the text.
- Each nonempty transaction is one undo step. Undo/redo restores the caller's
  selections and always advances the document revision. New edits discard redo.
  An empty insertion is a no-op; replacing text with identical text still counts
  as an edit. Empty transactions may change explicit selections without adding
  history or invalidating redo.
- History uses shared rope snapshots and retains up to 1,000 transactions by
  default. The configurable limit counts entries, not bytes. Setting it to zero
  disables retained history; changing it clears redo and trims old undo entries.
- Selections are owned by the caller, so future views can have independent
  cursors. The editor layer will need to map inactive views through edits and
  undo/redo. Insert-session grouping, history branches, and a byte-based history
  budget are not implemented yet.

## Next milestone

Add `vex_editor` for modes, grapheme-aware motions, views, and commands, then a
`vex_term` application for input and viewport rendering. The first interactive
loop should open a file, select and edit text, undo, and save. Search, pickers,
Tree-sitter, and LSP follow that loop.

See [the benchmark notes](docs/performance.md) for the initial performance baseline.
