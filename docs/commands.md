# Vex command reference

Generated from command Rustdoc and the default keymap.

| Command | Bindings by mode | Description |
|---|---|---|
| `move_right` | Normal: `l`, Normal: `<Right>`, Select: `l`, Select: `<Right>`, Insert: `<Right>` | Move right by graphemes, extending the selection in select mode. |
| `move_left` | Normal: `h`, Normal: `<Left>`, Select: `h`, Select: `<Left>`, Insert: `<Left>` | Move left by graphemes, extending the selection in select mode. |
| `move_down` | Normal: `j`, Normal: `<Down>`, Select: `j`, Select: `<Down>`, Insert: `<Down>` | Move down by logical lines, retaining each cursor's desired display column. |
| `move_up` | Normal: `k`, Normal: `<Up>`, Select: `k`, Select: `<Up>`, Insert: `<Up>` | Move up by logical lines, retaining each cursor's desired display column. |
| `move_word_forward` | Normal: `w`, Select: `w` | Select through the next word start; repeat counts span multiple words. |
| `move_word_backward` | Normal: `b`, Select: `b` | Select backward to a word start; repeat counts span multiple words. |
| `move_word_end` | Normal: `e`, Select: `e` | Select through the next word end, excluding following whitespace. |
| `goto_line_start` | Normal: `0`, Normal: `gh`, Normal: `<Home>`, Select: `0`, Select: `gh`, Select: `<Home>`, Insert: `<Home>` | Move to the beginning of the current logical line. |
| `goto_line_end` | Normal: `$`, Normal: `gl`, Normal: `<End>`, Select: `$`, Select: `gl`, Select: `<End>`, Insert: `<End>` | Move to the last grapheme of the line, or its end boundary in insert mode. |
| `goto_file_start` | Normal: `gg`, Select: `gg` | Move to the start of the document. |
| `goto_file_end` | Normal: `ge`, Select: `ge` | Move to the end-of-file boundary. |
| `select_line` | Normal: `x`, Select: `x` | Select logical lines from each cursor, including line endings; accepts a count. |
| `select_mode` | Normal: `v`, Select: `v` | Toggle select mode; movements in select mode retain the anchor grapheme. |
| `normal_mode` | Normal: `<Escape>`, Select: `<Escape>`, Insert: `<Escape>` | Enter normal mode. Leaving insert mode places the cursor on the preceding grapheme in the same line. |
| `insert_mode` | Normal: `i`, Select: `i` | Enter insert mode with a caret before every selection. |
| `append_mode` | Normal: `a`, Select: `a` | Enter insert mode with a caret after every selection. |
| `insert_text` | Insert: unbound printable characters, Enter, Tab; direct text events | Insert the context's text at all carets, continuing the typing undo group; requires insert mode. |
| `insert_paste` | Insert: bracketed paste; direct paste events | Insert the context's pasted text at all carets as a separate undo step; requires insert mode. |
| `delete_selection` | Normal: `d`, Select: `d` | Delete selected text atomically, leaving normal-mode cursors at the edit locations. |
| `change_selection` | Normal: `c`, Select: `c` | Delete selected text and enter insert mode; the deletion and subsequent typing share one undo step. |
| `delete_backward` | Insert: `<Backspace>` | Delete preceding graphemes at all insert carets as a separate undo step; accepts a count. |
| `delete_forward` | Insert: `<Delete>` | Delete following graphemes at all insert carets as a separate undo step; accepts a count. |
| `undo` | Normal: `u`, Select: `u` | Undo edit groups and restore their selections; accepts a count of groups. |
| `redo` | Normal: `U`, Normal: `<C-r>`, Select: `U`, Select: `<C-r>` | Redo edit groups and restore their selections; accepts a count of groups. |
