# Vex

TODO: Write an actual README for humans.

A terminal text editor in Rust, inspired by Helix's selection-first editing model.

`vex_core` provides rope-backed documents, directional multiple selections,
atomic transactions, revision-checked snapshots, undo/redo, and grapheme-aware
movement and streaming regex search. `vex_editor` adds normal/select/insert modes, documented command
functions, configurable keybindings, and repeat counts. `vex_syntax` adds
Tree-sitter parsing and a shared language registry. `vex_lsp` adds language-server
integration over stdio with a small futures executor. These crates work without a
terminal. `vex_git` computes background Git diffs against HEAD. `vex_term` provides the interactive application, using Crossterm for
terminal I/O and our own viewport, cell grid, and incremental drawing.

## Run

```sh
cargo run --release -p vex_term --locked -- path/to/file
```

Omit the path for a scratch buffer. Press `i` to insert, `Esc` for normal mode,
`:w` to save (`:w PATH` for a scratch buffer), and `:q` to quit. Unsaved changes
require `:q!` to discard. `:wq` saves and quits. Use `:help` or
`:help move_word_forward` for command documentation.

Run `:tutorial` for an editable, built-in tutorial covering motions, selections,
everyday editing, registers, search, windows, and navigation. Your other buffers
stay open. Run it again to resume; close the practice buffer with `:bc!` before
opening a fresh copy. Use `:w PATH` to save your notes.

Everyday edits include `I`/`A` for insertion at line edges, `r<char>` to replace
selected characters, `>`/`<` for buffer indentation, `J` to join
lines, and `[Space`/`]Space` to add blank lines. See the
[editing controls](docs/terminal.md#controls) for selection and count behavior.
Use `"ay` to copy into a named register, `"ap` to paste it, and Ctrl-r `a` to
insert it while typing or editing a prompt. See [registers](docs/registers.md).

The interface includes line numbers, selection highlighting, cursor-following
scrolling, a status line, and an editable command prompt with history and
background command/path completion. Unicode graphemes,
wide characters, tabs, bracketed paste, and terminal resizing are supported.
Use `/` or `?` for incremental regex search, Enter to accept, Escape to restore
your selections and scroll position, and `n` / `N` to search forward/backward.
Use `s` to select matches, `S` to split selections, `K` to filter, and `*` to
remember selected text for search. Select mode accumulates navigation matches. See [search behavior](docs/search.md).
Interactive search runs on a worker, with cancellation and revision checks.
Rust, Python, Go, C/C++, Java, C#, Swift, Ruby, PHP, Lua, JavaScript/TypeScript,
JSX/TSX, HTML, CSS, JSON/JSONC, YAML, TOML, Nix, Markdown, and Bash/shell use syntax
colors automatically, including in file previews. Use `:language NAME` to override
detection, `:language text` for plain text, or `:language auto` to restore
filename/shebang detection. See [syntax support and adding languages](docs/syntax.md).
With the configured server on `PATH`, named files also get diagnostics (Space-d for the document, Space-D across files), Space-k for
Markdown hover (Ctrl-u/Ctrl-d scroll), `gd` / `gy` / `gi` for definitions, types, and implementations, `gr` for
references, Space-h to select related occurrences in the document, and Space-r
to rename a symbol across files. Space-a opens the code-action menu for the
primary selection. Rename and code actions preserve unsaved buffers and leave
changes unsaved, with undo in each affected buffer. `=` formats one selection
when the server supports range formatting; `:format`/`:fmt` formats the file. Use
`Ctrl-o` to jump back, `]d` / `[d` for next/previous diagnostics, and `[D` / `]D` for first/last. Servers persist across file and pane switches, sharing one process per workspace
and server configuration. See [language services and setup](docs/lsp.md).
Tracked files show live Git gutter markers, including unsaved changes, with
Helix-style bars and deletion overlines. See [Git gutter behavior](docs/git.md).
See [terminal usage and architecture](docs/terminal.md).

Use `miw`/`miW` to select words/WORDs and `mip` to select paragraphs; `ma` includes
adjacent whitespace. `mi(`/`ma(` select inside/around delimiters, `mim`/`mam`
choose the closest pair, and `mm` moves to the matching bracket. `ms<char>`
surrounds the selections, `mr<from><to>` replaces surrounds, and `md<char>`
removes them. Use `m` as the old delimiter to target the nearest pair.
See [match mode](docs/match.md).

Press Space-f for the fuzzy project file picker. Named key groups (`g`, `m`, Space,
`[`, `]`, Ctrl-w) show available bindings. Discovery, matching, and previews run in the
background using Vex's own implementations. See [key groups and pickers](docs/pickers.md).

Space-b picks from loaded buffers, including unsaved and hidden files. Use `ga`
for the previous buffer, `gn`/`gp` to cycle, `gm` for the last modified buffer,
and `gf` to open a selected path. Switching preserves text, undo, and each pane's
view. Use `:bc` to close a buffer (`:bc!` discards unsaved edits).

Use `Ctrl-w v` / `Ctrl-w s` for vertical/horizontal splits. Panes can share a
buffer or display different files, with independent cursors and scrolling.
`Space w` is an alias; see [window mode](docs/windows.md) for all bindings.
`Ctrl-w =` equalizes all split widths and heights.
Mouse support is enabled by default: scroll the pane under the pointer, drag
vertical dividers or horizontal status lines to resize splits, and use
`:mouse off` / `:mouse on` to toggle it. Scrolling preserves focus and selections;
keyboard navigation or editing brings an offscreen cursor back into view.

## Development

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo bench -p vex_core --bench editing --locked -- --noplot
cargo bench -p vex_core --bench search --locked -- --noplot
cargo bench -p vex_editor --bench commands --locked -- --noplot
cargo bench -p vex_term --bench rendering --locked -- --noplot
cargo run --release -p vex_term --example long_lines --locked -- 10 100
cargo run -p vex_editor --example command_reference --locked
cargo build --release -p vex_term --locked
python3 tools/terminal_smoke.py
python3 tools/picker_smoke.py
python3 tools/git_smoke.py # Requires Git.
python3 tools/lsp_smoke.py # Requires rust-analyzer and a Rust toolchain.
python3 tools/languages_smoke.py --syntax-only # All language fixtures; no servers needed.
python3 tools/languages_smoke.py --go # Requires gopls, Go, typescript-language-server, and TypeScript.
python3 tools/background_search_benchmark.py
```

Unit and property tests live in their source modules under `#[cfg(test)]`.
Criterion benchmarks live in each crate's `benches/` directory. The property tests check
random Unicode edits against flat text and snapshot history models, as well as
selection normalization and position mapping invariants. Additional checks compare
rope grapheme boundaries with flat Unicode segmentation and exercise arbitrary
key sequences through the editor and command prompt. The Unix pseudo-terminal
smoke script checks real input, saves, resize handling, and terminal restoration
on exit, signal, and panic.

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
stream UTF-8; the terminal application owns file paths, flushing, and atomic saves.
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
- `Document::apply` makes each nonempty transaction a separate undo step.
  `apply_grouped` joins consecutive edits until `finish_undo_group`, a standalone
  edit, or undo/redo. Each edit still advances the revision. Undo/redo restores
  the group's text and selections and advances the revision. New edits discard redo.
  An empty insertion is a no-op; replacing text with identical text still counts
  as an edit. Empty transactions may change explicit selections without adding
  history or invalidating redo.
- History uses shared rope snapshots and retains up to 1,000 undo steps by
  default. A group retains its initial and final states, releasing intermediate
  snapshots. The configurable limit counts groups, not bytes. Setting it to zero
  disables retained history; changing it closes the group, clears redo, and trims
  old undo entries.
- Selections are owned by the caller, so future views can have independent
  cursors. The editor currently has one active view; it will need to map inactive views through edits and
  undo/redo. History branches and a byte-based history budget are not implemented yet.

## Commands and keybindings

Keybindings resolve to named, ordinary Rust functions in
[`vex_editor::commands`](crates/vex_editor/src/commands.rs). A small declaration
macro emits each function's `///` documentation into both Rustdoc and the runtime
command registry. The registry exposes its stable name, description, and function
pointer for help, command search, and direct invocation. Commands can declare
`CommandInput::Character`; the key handler collects that argument and passes it
through `CommandContext::character`. Direct callers set the same context field.
`count_given` distinguishes an explicit count from its default; `Editor::execute`
treats a zero count as omitted. Motion calculations in
`vex_core::motion` and `vex_core::grapheme` do not depend on input events or modes.

```rust
use vex_core::Document;
use vex_editor::{Editor, Key, KeyHandler, Keymap};

fn main() -> Result<(), vex_editor::Error> {
    let mut editor = Editor::new(Document::from("hello world"));
    let mut keymap = Keymap::default();
    keymap.bind(vex_editor::Mode::Normal, vec![Key::Char('h')], "move_word_forward")?;
    let mut input = KeyHandler::new(keymap);

    input.handle(&mut editor, Key::Char('h'))?;
    editor.execute("delete_selection", 1)?;
    assert_eq!(editor.document().text(), "world");
    Ok(())
}
```

`Keymap::default()` supplies Vex's Helix-inspired defaults; `Keymap::empty()` starts
a custom map. Bindings can be a key or a sequence such as `gg`. Conflicting prefix
bindings are rejected, so dispatch needs no timeout. Prefixes show available
commands; `Keymap::name_group` gives custom groups a title, and
`Keymap::name_sticky_group` keeps a group's shortcuts active until cancelled.
Escape cancels pending input while preserving the mode, or enters normal mode
when no input is pending.
Digits build a repeat count outside insert mode; overflow
returns an error and clears the count. An unbound sequence also clears pending
input; sticky groups keep their prefix active. Insert mode treats unbound printable
characters through `insert_character`, which auto-closes `()`, `[]`, and `{}`.
An existing closer is stepped over; Backspace between an empty pair removes both.
Paste, completion, and `insert_text` preserve their literal text. A bracket's
matching partner uses bold yellow text with an underline. The bracket under the
cursor retains its syntax colors and ordinary cursor styling;
see [bracket behavior](docs/match.md#automatic-pairs-and-highlighting). Enter is bound to
`insert_newline`, which uses the loaded line ending (LF by default) and copies
the current line's indentation before the caret. Tab calls `insert_tab` to insert
one unit of the buffer's detected spaces/tabs at each caret. Indentation is sampled
on opening or reloading a file, with language defaults for ambiguous files.
Text events call `Editor::insert_text`; paste events call `Editor::insert_paste`
to get a separate undo step.

`.` in normal/select mode [repeats the last insert session](docs/repeat.md),
including its entry command, editing actions, and accepted completion text.
The recording is shared across buffers and uses command functions rather than
terminal keys. Counted playback yields between batches in the event loop.

`Space-y`/`Y` copy all selections / the primary selection to the system clipboard;
`Space-p`/`P`/`R` paste after / before / replacing selections. The
[clipboard service](docs/clipboard.md) preserves fragment boundaries and performs
clipboard I/O and paste preparation on a background worker.

The implemented commands cover `hjkl`, arrows, `w`/`b`/`e`, `W`/`B`/`E`,
cross-line `f`/`F`/`t`/`T`, `gs`, counted `gg`/`G` and `g|`, line/document bounds,
`Ctrl-u`/`Ctrl-d` for half-page movement and scrolling in normal/select mode,
`Ctrl-b`/`Ctrl-f` and Page Up/Down for full pages, `gt`/`gc`/`gb` for visible-window
jumps, and `z`/sticky `Z` for [view alignment and scrolling](docs/terminal.md#view-mode),
line selection, mode changes, deletion/change, insertion, `o`/`O` to open lines
below/above selections, insert-mode Backspace/`Ctrl-h`, and undo/redo.
Open-line commands copy leading tabs and spaces; a count creates a caret on each
new line. Multiple selections opening the same line share those carets.
Enter uses the same whitespace-copying rule, capped at the caret when splitting
leading whitespace. Tabs and spaces are preserved exactly across all file types;
language-specific indentation changes are not inferred yet. Direct text and paste
events preserve literal newlines without adding indentation.
Normal-mode character movements place a block cursor; select-mode movements retain
the anchor, including when crossing it. Word movements select the traversed text.
Editing and cursor movements normally act on all selections. View scrolling
preserves them until the primary cursor must move to stay visible.
Vertical motion uses logical lines,
accounts for tabs and Unicode display width, and retains desired columns through
short lines. A merge of cursors clears their retained columns.

See the [generated command reference](docs/commands.md). Regenerate it with:

```sh
cargo run -p vex_editor --example command_reference --locked > docs/commands.md
```

This is an initial set of bindings inspired by Helix, without a compatibility
guarantee. Word categories currently group Unicode letters/numbers and underscore,
punctuation, and whitespace; language-specific segmentation is not implemented.
Consecutive typing, including Enter, Tab, and insert-mode deletion, shares one undo
group. Movement, mode/selection changes, save attempts, paste, and normal-mode deletions separate
groups. `c` groups its deletion with the following replacement text; `o` and `O`
group the opened lines with the following typing. Undo/redo
counts refer to groups. Insert-mode Ctrl-s explicitly splits the group without
saving. Integrations call `Editor::finish_undo_group` at savepoints.

`C` adds copies of selections on following logical lines at the same display
columns, skipping lines that cannot fit both endpoints. Counts add copies;
multi-line selections advance by their height. The terminal uses a cancellable
worker and preserves the order of following edits. The corresponding upward
command is available by name until Alt bindings are added.

Vertical movement and rendering share a bounded, lazy display-column cache. Edits retain
the unaffected prefix and shift indexes for unchanged later lines; undo/redo
updates the same cache. The first visit to an unindexed prefix still scans it.
Soft wrapping is not implemented yet.

## Next milestone

The interactive loop can open, search, select, edit, undo groups, and save.
Search, syntax, file picking, and language services run through a shared event
queue. Completion opens automatically after two identifier characters and
a 100 ms pause, or immediately on server trigger characters and `Ctrl-x`.
The menu shows completion kinds alongside names. Automatic signature help marks
the active parameter, with Alt-p/Alt-n to cycle overloads. The menu and
documentation panels sit next to the cursor; see
[language services](docs/lsp.md#completion) for controls and session settings.
Special-register integration, more picker providers, and language
configuration files remain upcoming milestones. Further layout
work can address cold indexing and updates near the beginning of a huge line.

See [the benchmark notes](docs/performance.md) for the initial performance baseline.
