# Vex command reference

Generated from command Rustdoc and the default keymap.

| Command | Bindings by mode | Description |
|---|---|---|
| `toggle_comments` | Normal: `<Space>c`, Normal: `<C-c>`, Select: `<Space>c`, Select: `<C-c>` | Toggle comments on selected lines, preferring line comments and recognizing existing block comments. Uses language delimiters, skips blank lines, preserves selections and mode, and creates one undo step. |
| `toggle_block_comments` | Normal: `<Space>C`, Select: `<Space>C` | Toggle block comments around selections, retaining their direction and selecting added delimiters. Languages with only line comments use those instead; plain text defaults to /* */. One undo step, with normal/select mode retained. |
| `rotate_view` | Normal: `<Space>ww`, Normal: `<Space>w<C-w>`, Normal: `<C-w>w`, Normal: `<C-w><C-w>`, Select: `<Space>ww`, Select: `<Space>w<C-w>`, Select: `<C-w>w`, Select: `<C-w><C-w>` | Focus the next window in layout order. A count advances multiple windows. |
| `vsplit` | Normal: `<Space>wv`, Normal: `<Space>w<C-v>`, Normal: `<C-w>v`, Normal: `<C-w><C-v>`, Select: `<Space>wv`, Select: `<Space>w<C-v>`, Select: `<C-w>v`, Select: `<C-w><C-v>` | Split the current window vertically, opening a shared view on the right. |
| `hsplit` | Normal: `<Space>ws`, Normal: `<Space>w<C-s>`, Normal: `<C-w>s`, Normal: `<C-w><C-s>`, Select: `<Space>ws`, Select: `<Space>w<C-s>`, Select: `<C-w>s`, Select: `<C-w><C-s>` | Split the current window horizontally, opening a shared view below. |
| `jump_view_left` | Normal: `<Space>wh`, Normal: `<Space>w<C-h>`, Normal: `<Space>w<Left>`, Normal: `<C-w>h`, Normal: `<C-w><C-h>`, Normal: `<C-w><Left>`, Select: `<Space>wh`, Select: `<Space>w<C-h>`, Select: `<Space>w<Left>`, Select: `<C-w>h`, Select: `<C-w><C-h>`, Select: `<C-w><Left>` | Focus the window to the left. |
| `jump_view_down` | Normal: `<Space>wj`, Normal: `<Space>w<C-j>`, Normal: `<Space>w<Down>`, Normal: `<C-w>j`, Normal: `<C-w><C-j>`, Normal: `<C-w><Down>`, Select: `<Space>wj`, Select: `<Space>w<C-j>`, Select: `<Space>w<Down>`, Select: `<C-w>j`, Select: `<C-w><C-j>`, Select: `<C-w><Down>` | Focus the window below. |
| `jump_view_up` | Normal: `<Space>wk`, Normal: `<Space>w<C-k>`, Normal: `<Space>w<Up>`, Normal: `<C-w>k`, Normal: `<C-w><C-k>`, Normal: `<C-w><Up>`, Select: `<Space>wk`, Select: `<Space>w<C-k>`, Select: `<Space>w<Up>`, Select: `<C-w>k`, Select: `<C-w><C-k>`, Select: `<C-w><Up>` | Focus the window above. |
| `jump_view_right` | Normal: `<Space>wl`, Normal: `<Space>w<C-l>`, Normal: `<Space>w<Right>`, Normal: `<C-w>l`, Normal: `<C-w><C-l>`, Normal: `<C-w><Right>`, Select: `<Space>wl`, Select: `<Space>w<C-l>`, Select: `<Space>w<Right>`, Select: `<C-w>l`, Select: `<C-w><C-l>`, Select: `<C-w><Right>` | Focus the window to the right. |
| `swap_view_left` | Normal: `<Space>wH`, Normal: `<C-w>H`, Select: `<Space>wH`, Select: `<C-w>H` | Swap the current window with the window to the left. |
| `swap_view_down` | Normal: `<Space>wJ`, Normal: `<C-w>J`, Select: `<Space>wJ`, Select: `<C-w>J` | Swap the current window with the window below. |
| `swap_view_up` | Normal: `<Space>wK`, Normal: `<C-w>K`, Select: `<Space>wK`, Select: `<C-w>K` | Swap the current window with the window above. |
| `swap_view_right` | Normal: `<Space>wL`, Normal: `<C-w>L`, Select: `<Space>wL`, Select: `<C-w>L` | Swap the current window with the window to the right. |
| `wclose` | Normal: `<Space>wq`, Normal: `<Space>w<C-q>`, Normal: `<C-w>q`, Normal: `<C-w><C-q>`, Select: `<Space>wq`, Select: `<Space>w<C-q>`, Select: `<C-w>q`, Select: `<C-w><C-q>` | Close this window, protecting the last view of unsaved text. Exit when no windows remain. |
| `wonly` | Normal: `<Space>wo`, Normal: `<Space>w<C-o>`, Normal: `<C-w>o`, Normal: `<C-w><C-o>`, Select: `<Space>wo`, Select: `<Space>w<C-o>`, Select: `<C-w>o`, Select: `<C-w><C-o>` | Keep only this window, protecting unsaved text in other buffers. |
| `goto_file_hsplit` | Normal: `<Space>wf`, Normal: `<C-w>f`, Select: `<Space>wf`, Select: `<C-w>f` | Open filenames in the selections in horizontal splits. Paths are relative to the current file. |
| `goto_file_vsplit` | Normal: `<Space>wF`, Normal: `<C-w>F`, Select: `<Space>wF`, Select: `<C-w>F` | Open filenames in the selections in vertical splits. Paths are relative to the current file. |
| `completion` | Insert: `<C-x>` | Request language-server completion at the insertion cursor. |
| `file_picker` | Normal: `<Space>f`, Select: `<Space>f` | Open a fuzzy file picker at the current project root. |
| `git_status` | Normal: `<Space>g`, Select: `<Space>g` | Open the repository status view with expandable staged and unstaged diffs. |
| `symbol_picker` | Normal: `<Space>s`, Select: `<Space>s` | Open a searchable picker of symbols in the current document using its language server. |
| `workspace_symbol_picker` | Normal: `<Space>S`, Select: `<Space>S` | Search workspace symbols using the current document's language server. |
| `jump_back` | Normal: `<C-o>`, Select: `<C-o>` | Return to the location before the last successful file, definition, or symbol jump. |
| `hover` | Normal: `<Space>k`, Normal: `K`, Select: `<Space>k`, Select: `K` | Show language-server documentation for the symbol at the primary cursor. |
| `goto_definition` | Normal: `gd`, Select: `gd` | Jump to the definition of the symbol at the primary cursor. |
| `goto_next_diagnostic` | Normal: `]d`, Select: `]d` | Move to the next diagnostic, wrapping and honoring the repeat count. |
| `goto_previous_diagnostic` | Normal: `[d`, Select: `[d` | Move to the previous diagnostic, wrapping and honoring the repeat count. |
| `search_forward` | Normal: `/`, Select: `/` | Begin a forward literal search from each selection's start, including the current position; accepts a match count. Integrations should open a prompt and send search_update, search_accept, or search_cancel. |
| `search_backward` | Normal: `?`, Select: `?` | Begin a backward literal search from each selection's start, including the current position; accepts a match count. Matches wrap at document boundaries. |
| `search_update` |  | Preview the context's literal text from the original selections. Empty or unmatched text restores them; selections expand to whole graphemes in normal and select modes. |
| `search_accept` |  | Accept a matching preview for n/N navigation, waiting for pending background work if needed. An empty query cancels; an unmatched query keeps the prompt open and preserves the previous accepted search. |
| `search_cancel` |  | Cancel a search preview and restore its original selections and preferred columns. The terminal integration also restores its saved viewport. |
| `search_next` | Normal: `n`, Select: `n` | Select the next literal match for each selection, following the accepted search direction and wrapping; accepts a count. Replaces ranges even in select mode. |
| `search_previous` | Normal: `N`, Select: `N` | Select the previous literal match for each selection, opposite the accepted search direction and wrapping; accepts a count. Replaces ranges even in select mode. |
| `move_right` | Normal: `l`, Normal: `<Right>`, Select: `l`, Select: `<Right>`, Insert: `<Right>` | Move right by graphemes, extending the selection in select mode. |
| `move_left` | Normal: `h`, Normal: `<Left>`, Select: `h`, Select: `<Left>`, Insert: `<Left>` | Move left by graphemes, extending the selection in select mode. |
| `move_down` | Normal: `j`, Normal: `<Down>`, Select: `j`, Select: `<Down>`, Insert: `<Down>` | Move down by logical lines, retaining each cursor's desired display column. |
| `move_up` | Normal: `k`, Normal: `<Up>`, Select: `k`, Select: `<Up>`, Insert: `<Up>` | Move up by logical lines, retaining each cursor's desired display column. |
| `page_cursor_half_up` | Normal: `<C-u>`, Select: `<C-u>` | Move cursors and scroll up by half the visible text height, retaining desired columns and extending selections in select mode. Counts multiply the distance; the frontend supplies the current viewport size. |
| `page_cursor_half_down` | Normal: `<C-d>`, Select: `<C-d>` | Move cursors and scroll down by half the visible text height, retaining desired columns and extending selections in select mode. Counts multiply the distance; the frontend supplies the current viewport size. |
| `move_word_forward` | Normal: `w`, Select: `w` | Select through the next word start; repeat counts span multiple words. |
| `move_word_backward` | Normal: `b`, Select: `b` | Select backward to a word start; repeat counts span multiple words. |
| `move_word_end` | Normal: `e`, Select: `e` | Select through the next word end, excluding following whitespace. |
| `move_long_word_forward` | Normal: `W`, Select: `W` | Select through the next whitespace-separated WORD start, keeping punctuation within each WORD; accepts a count. |
| `move_long_word_backward` | Normal: `B`, Select: `B` | Select backward to a whitespace-separated WORD start; accepts a count. |
| `move_long_word_end` | Normal: `E`, Select: `E` | Select through the next whitespace-separated WORD end, excluding following whitespace; accepts a count. |
| `find_next_char` | Normal: `f<char>`, Select: `f<char>` | Select through the next occurrence of the supplied character, across line boundaries without wrapping; accepts a count. Enter targets a logical line ending; Tab targets a tab. Unmatched selections stay unchanged. |
| `find_prev_char` | Normal: `F<char>`, Select: `F<char>` | Select backward through the supplied character, across line boundaries without wrapping; accepts a count. Unmatched selections stay unchanged. |
| `find_till_char` | Normal: `t<char>`, Select: `t<char>` | Select until just before the next supplied character, skipping an adjacent match so repeated finds advance; accepts a count and crosses lines. |
| `till_prev_char` | Normal: `T<char>`, Select: `T<char>` | Select backward until just after the previous supplied character, skipping an adjacent match so repeated finds advance; accepts a count and crosses lines. |
| `goto_first_nonwhitespace` | Normal: `gs`, Select: `gs` | Move to the first non-whitespace grapheme of each cursor's line. Whitespace-only lines keep their selections unchanged. |
| `goto_column` | Normal: `g\|`, Select: `g\|` | Move to the counted one-based grapheme column (default 1), clamped to each cursor's logical line. Tabs and wide graphemes each count as one column. |
| `goto_line` | Normal: `G`, Select: `G` | Move to the explicitly counted one-based line, clamping to the last content line. With no count, do nothing; select mode extends to the destination. |
| `goto_line_start` | Normal: `0`, Normal: `gh`, Normal: `<Home>`, Select: `0`, Select: `gh`, Select: `<Home>`, Insert: `<Home>` | Move to the beginning of the current logical line. |
| `goto_line_end` | Normal: `$`, Normal: `gl`, Normal: `<End>`, Select: `$`, Select: `gl`, Select: `<End>`, Insert: `<End>` | Move to the last grapheme of the line, or its end boundary in insert mode. |
| `goto_file_start` | Normal: `gg`, Select: `gg` | Move to the start of the document, or to the counted one-based line, clamped to the last content line. Select mode extends to the destination. |
| `goto_file_end` | Normal: `ge`, Select: `ge` | Move to the end-of-file boundary. |
| `select_line` | Normal: `x`, Select: `x` | Expand each selection to whole logical lines, including line endings, facing forward. If already line-aligned, extend below by the count; otherwise alignment counts as the first step.  Repeated x keeps earlier lines selected. Counts clamp at EOF and overlapping ranges merge while retaining the primary selection. |
| `select_all` | Normal: `%`, Select: `%` | Select the entire document as one forward range, retaining normal/select mode. Empty documents retain a single EOF cursor. |
| `collapse_selection` | Normal: `;`, Select: `;` | Collapse every selection to its displayed cursor, preserving multiple cursors and the primary. Normal/select cursors cover one whole grapheme, or remain empty at EOF. |
| `keep_primary_selection` | Normal: `,`, Select: `,` | Keep only the primary selection, preserving its direction and normal/select mode. |
| `extend_to_line_bounds` | Normal: `X`, Select: `X` | Expand selections to the full logical lines they touch, including line endings and preserving direction. Repeating this command does not add lines; a range ending at the next line's start excludes that line. |
| `trim_selections` | Normal: `_`, Select: `_` | Trim Unicode whitespace from selection edges without editing text or splitting graphemes. Remove empty/whitespace-only selections; retain the primary if it survives, otherwise use the last survivor.  If no selection survives, keep a single cursor at the original primary's displayed position. Retains normal/select mode and leaves undo history and the yank register unchanged. |
| `select_mode` | Normal: `v`, Select: `v` | Toggle select mode; movements in select mode retain the anchor grapheme. |
| `normal_mode` | Normal: `<Escape>`, Select: `<Escape>`, Insert: `<Escape>` | Enter normal mode. Leaving insert mode places the cursor on the preceding grapheme in the same line. |
| `insert_mode` | Normal: `i`, Select: `i` | Enter insert mode with a caret before every selection. |
| `append_mode` | Normal: `a`, Select: `a` | Enter insert mode with a caret after every selection. |
| `insert_at_line_start` | Normal: `I`, Select: `I` | Enter insert mode at the first non-whitespace character on each cursor's line, or its start if blank. Carets on the same line merge; does not infer indentation or use a count. |
| `insert_at_line_end` | Normal: `A`, Select: `A` | Enter insert mode before the line ending on each cursor's line. Carets on the same line merge; does not infer indentation or use a count. |
| `replace` | Normal: `r<char>`, Select: `r<char>` | Replace each selected grapheme with the following character in one undo step, retaining selection direction and returning to normal mode. Enter uses the buffer's line ending; Tab inserts a literal tab. Empty EOF cursors do nothing; ignores counts and leaves the yank register unchanged. |
| `indent` | Normal: `>`, Select: `>` | Indent each selected nonblank line once, using the buffer's language indentation settings. Counts add levels; spaces advance to an indent boundary. Retains selected text and returns to normal mode in one undo step. |
| `unindent` | Normal: `<`, Select: `<` | Remove up to a counted number of indentation levels from each selected line, measuring tabs at the buffer's tab stops. Retains selected text and returns to normal mode in one undo step. |
| `join_selections` | Normal: `J`, Select: `J` | Join lines within each selection; a single-line selection joins the next line. Remove the line break and following indentation, adding a separating space if needed. Shared joins happen once; counts are ignored, selections and normal/select mode are retained. Comment prefixes are kept literally. |
| `add_newline_above` | Normal: `[<Space>`, Select: `[<Space>` | Add a counted number of empty lines above each selection without entering insert mode. Shared insertion points are handled once; uses the buffer's line ending and retains selections on the original text in one undo step. |
| `add_newline_below` | Normal: `]<Space>`, Select: `]<Space>` | Add a counted number of empty lines below each selection without entering insert mode. Shared insertion points are handled once; uses the buffer's line ending and retains selections on the original text in one undo step. |
| `open_below` | Normal: `o`, Select: `o` | Open lines below each selection and enter insert mode, copying indentation. A count creates that many lines and carets; opening and subsequent typing share one undo step.  Uses the loaded line ending; multiple selections ending on the same line share the new lines. |
| `open_above` | Normal: `O`, Select: `O` | Open lines above each selection and enter insert mode, copying indentation. A count creates that many lines and carets; opening and subsequent typing share one undo step.  Uses the loaded line ending; multiple selections starting on the same line share the new lines. |
| `insert_newline` | Insert: `<Enter>` | Insert a newline at every insert caret, copying leading tabs and spaces before that caret. Uses the loaded line ending and continues the typing undo group; requires insert mode.  Indentation is copied literally, without language-specific increases or decreases. Pasted and directly inserted text remains unchanged. |
| `insert_text` | Insert: unbound printable characters, Tab; direct text events | Insert the context's text at all carets, continuing the typing undo group; requires insert mode. |
| `insert_paste` | Insert: bracketed paste; direct paste events | Insert the context's pasted text at all carets as a separate undo step; requires insert mode. |
| `yank` | Normal: `y`, Select: `y` | Copy selections to the shared internal register, retaining their order and leaving select mode. Does not edit text or undo history. |
| `paste_after` | Normal: `p`, Select: `p` | Paste the internal register after selections, selecting the inserted text in normal mode. Newline-terminated yanks paste below the selected lines; counts repeat each fragment in one undo step.  Fragments pair with selections in document order; extra destinations repeat the last fragment. Uses the destination's line endings without changing the register. |
| `paste_before` | Normal: `P`, Select: `P` | Paste the internal register before selections, selecting the inserted text in normal mode. Newline-terminated yanks paste above the selected lines; counts repeat each fragment in one undo step.  Fragments pair with selections in document order; extra destinations repeat the last fragment. Uses the destination's line endings without changing the register. |
| `replace_with_yanked` | Normal: `R`, Select: `R` | Replace selections with the internal register in one undo step, selecting the replacements in normal mode. Counts repeat each fragment; replacement leaves the register unchanged.  Fragments pair in document order, repeating the last for extra destinations. Replaces the exact ranges even for linewise yanks, using the destination's line endings. |
| `delete_selection` | Normal: `d`, Select: `d` | Cut selections into the shared internal register and delete them atomically, leaving normal-mode cursors at the edit locations. |
| `delete_selection_without_yank` |  | Delete selections without changing the yank register. Used for internal buffer cleanup; leaves normal-mode cursors at the edit locations. |
| `change_selection` | Normal: `c`, Select: `c` | Cut selections into the shared internal register and enter insert mode; the deletion and subsequent typing share one undo step. |
| `delete_backward` | Insert: `<C-h>`, Insert: `<Backspace>` | Delete preceding graphemes at all insert carets as a separate undo step; accepts a count. |
| `delete_forward` | Insert: `<Delete>` | Delete following graphemes at all insert carets as a separate undo step; accepts a count. |
| `undo` | Normal: `u`, Select: `u` | Undo edit groups and restore their selections; accepts a count of groups. |
| `redo` | Normal: `U`, Normal: `<C-r>`, Select: `U`, Select: `<C-r>` | Redo edit groups and restore their selections; accepts a count of groups. |
