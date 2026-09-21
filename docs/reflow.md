# Text reflow

Select a paragraph with `mip`, then type `:reflow` and press Enter to wrap it at
80 display columns. Use `:reflow 72` for a different width. `x` selects a line,
and `%` selects the whole buffer; each selection is reflowed independently.
There is no default keybinding. `:help reflow` shows the command documentation.

Reflow edits the text, joining existing lines and inserting new line breaks
between words. It does not depend on the pane width or require a language
server. All selections change in one undo step: `u` undoes the operation and
`U` redoes it. The replacement text stays selected with the original direction
and primary selection. Normal/select mode is retained.

- Width counts terminal cells, including indentation and comment prefixes.
  Wide characters, combining marks, emoji, and the buffer's tab stops use the
  same widths as cursor movement and rendering.
- Blank lines are preserved. A change in indentation or line-comment prefix
  starts a separate paragraph. Matching prefixes continue on new lines; line
  comments use the buffer's language, including Rust's `///` and `//!` markers.
- Spaces and tabs between words become one space. Nonbreaking spaces remain
  inside words. Words wider than the requested width stay intact, so a long
  URL or identifier can exceed that width.
- New breaks use the buffer's line ending, including CRLF. Existing paragraph
  terminators, blank lines, and trailing spacing on a paragraph's final line
  are retained. Reflow does not add a final newline to an unterminated selection.
- Only selected text changes. Empty carets and already wrapped text add no
  undo entry and do not mark the buffer modified. Select text before invoking
  the command; an ordinary block cursor selects only one grapheme.

The width must be a positive integer. Extra arguments and `:reflow!` are
rejected before editing. Reflow handles text and language line-comment prefixes;
it does not parse Markdown lists, tables, fenced code, or block comments.
Select individual prose paragraphs when working in structured documents.

The terminal-independent `vex_editor::commands::reflow` command also accepts
the width as an explicit count. `Editor::execute("reflow", 0)` uses 80;
`Editor::execute("reflow", 72)` uses 72. Reflow runs synchronously over the
selected text and uses the ordinary transaction, history, view-mapping, and
cache-invalidation paths.
