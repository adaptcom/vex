# Vex

A terminal text editor in Rust, inspired by Helix's selection-first editing model.

`vex_core` provides rope-backed documents, directional multiple selections,
atomic transactions, revision-checked snapshots, undo/redo, and grapheme-aware
movement. `vex_editor` adds normal/select/insert modes, documented command
functions, configurable keybindings, and repeat counts. Both work without a
terminal. There is no terminal application yet.

## Development

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo bench -p vex_core --bench editing --locked -- --noplot
cargo bench -p vex_editor --bench commands --locked -- --noplot
cargo run -p vex_editor --example command_reference --locked
```

Unit and property tests live in their source modules under `#[cfg(test)]`.
Criterion benchmarks live in each crate's `benches/` directory. The property tests check
random Unicode edits against flat text and snapshot history models, as well as
selection normalization and position mapping invariants. Additional checks compare
rope grapheme boundaries with flat Unicode segmentation and exercise arbitrary
key sequences through the editor.

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
  rejects offsets inside a multi-byte scalar. The editor keeps selections on
  grapheme boundaries; low-level transactions can edit at any scalar boundary.
- A `Selection` has an anchor and a head. Its text range is half-open, and equal
  endpoints represent an insertion caret. A normal-mode block cursor selects a
  whole grapheme. EOF is a valid boundary, including in empty documents.
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
  cursors. The editor currently has one active view; it will need to map inactive views through edits and
  undo/redo. Insert-session grouping, history branches, and a byte-based history
  budget are not implemented yet.

## Commands and keybindings

Keybindings resolve to named, ordinary Rust functions in
[`vex_editor::commands`](crates/vex_editor/src/commands.rs). A small declaration
macro emits each function's `///` documentation into both Rustdoc and the runtime
command registry. The registry exposes its stable name, description, and function
pointer for help, command search, and direct invocation. Motion calculations in
`vex_core::motion` and `vex_core::grapheme` do not depend on input events or modes.

```rust
use vex_core::Document;
use vex_editor::{Editor, Key, KeyHandler, Keymap};

fn main() -> Result<(), vex_editor::Error> {
    let mut editor = Editor::new(Document::from("hello world"));
    let mut keymap = Keymap::default();
    keymap.bind(vex_editor::Mode::Normal, vec![Key::Char('z')], "move_word_forward")?;
    let mut input = KeyHandler::new(keymap);

    input.handle(&mut editor, Key::Char('z'))?;
    editor.execute("delete_selection", 1)?;
    assert_eq!(editor.document().text(), "world");
    Ok(())
}
```

`Keymap::default()` supplies Vex's Helix-inspired defaults; `Keymap::empty()` starts
a custom map. Bindings can be a key or a sequence such as `gg`. Conflicting prefix
bindings are rejected, so dispatch needs no timeout. Escape cancels pending input
and enters normal mode. Digits build a repeat count outside insert mode; overflow
returns an error and clears the count. An unbound sequence also clears pending
input. Insert mode treats unbound printable characters as text; Enter inserts LF
and Tab inserts a literal tab. Whole text/paste events can call `Editor::insert_text`.

The implemented commands cover `hjkl`, arrows, `w`/`b`/`e`, line/document bounds,
line selection, mode changes, deletion/change, insertion, backspace, and undo/redo.
Normal-mode character movements place a block cursor; select-mode movements retain
the anchor, including when crossing it. Word movements select the traversed text.
All selections participate in a command. Vertical motion uses logical lines,
accounts for tabs and Unicode display width, and retains desired columns through
short lines. A merge of cursors clears their retained columns.

See the [generated command reference](docs/commands.md). Regenerate it with:

```sh
cargo run -p vex_editor --example command_reference --locked > docs/commands.md
```

This is an initial set of bindings inspired by Helix, without a compatibility
guarantee. Word categories currently group Unicode letters/numbers and underscore,
punctuation, and whitespace; language-specific segmentation is not implemented.
Each text event is an undo step; insert-session grouping is still pending. Vertical
layout scans the needed line prefix and has no layout cache or soft wrapping yet.

## Next milestone

Add a `vex_term` application for terminal input and viewport rendering. The first interactive
loop should open a file, select and edit text, undo, and save. Search, pickers,
Tree-sitter, and LSP follow that loop.

See [the benchmark notes](docs/performance.md) for the initial performance baseline.
