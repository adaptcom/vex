//! Mode-specific key sequences resolve to documented command functions.

use crate::{Command, Editor, Error, Mode, commands};
use std::{collections::BTreeMap, fmt};

/// Logical keys, independent of terminal escape sequences and input libraries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Alt(char),
    Modified(Modifier, NamedKey),
    Escape,
    Enter,
    Tab,
    BackTab,
    PageUp,
    PageDown,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
}

/// Modifiers for non-character keys. Plain characters already carry their case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Modifier {
    Control,
    Alt,
    AltShift,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NamedKey {
    Escape,
    Enter,
    Tab,
    PageUp,
    PageDown,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Char(' ') => write!(f, "<Space>"),
            Self::Char(ch) => write!(f, "{ch}"),
            Self::Ctrl(ch) => write!(f, "<C-{ch}>"),
            Self::Alt(ch) => write!(f, "<A-{ch}>"),
            Self::Modified(modifier, key) => write!(
                f,
                "<{}-{key:?}>",
                match modifier {
                    Modifier::Control => "C",
                    Modifier::Alt => "A",
                    Modifier::AltShift => "A-S",
                }
            ),
            key => write!(f, "<{key:?}>"),
        }
    }
}

#[derive(Debug)]
pub struct Binding<'a> {
    pub mode: Mode,
    pub keys: &'a [Key],
    pub command: &'static Command,
}

#[derive(Debug)]
pub struct Keymap {
    bindings: BTreeMap<(Mode, Vec<Key>), &'static Command>,
    groups: BTreeMap<(Mode, Vec<Key>), Group>,
}

#[derive(Debug)]
struct Group {
    title: String,
    sticky: bool,
}

/// Available continuations of a prefix, with descriptions from command Rustdoc.
#[derive(Debug)]
pub struct KeyHints<'a> {
    pub title: &'a str,
    pub entries: Vec<(Key, &'a str)>,
}

impl Keymap {
    /// An empty map for custom bindings. The handler still reserves Escape and counts.
    pub fn empty() -> Self {
        Self {
            bindings: BTreeMap::new(),
            groups: BTreeMap::new(),
        }
    }

    /// Bind or replace a sequence using a command's stable name. Prefix ambiguity
    /// is rejected, so dispatch never depends on a timeout. Escape is reserved
    /// for normal_mode; leading count digits are reserved outside insert mode.
    pub fn bind(&mut self, mode: Mode, keys: Vec<Key>, name: &str) -> Result<(), Error> {
        if keys.is_empty() {
            return Err(Error::EmptyBinding);
        }
        let command = commands::find(name).ok_or_else(|| Error::UnknownCommand(name.into()))?;
        if (keys.contains(&Key::Escape) && (keys != [Key::Escape] || name != "normal_mode"))
            || (mode != Mode::Insert && matches!(keys[0], Key::Char('1'..='9')))
        {
            return Err(Error::ReservedBinding);
        }
        if self.bindings.keys().any(|(m, existing)| {
            *m == mode
                && existing != &keys
                && (existing.starts_with(&keys) || keys.starts_with(existing))
        }) {
            return Err(Error::ConflictingBinding);
        }
        self.bindings.insert((mode, keys), command);
        Ok(())
    }

    pub fn bindings(&self) -> impl Iterator<Item = Binding<'_>> {
        self.bindings
            .iter()
            .map(|((mode, keys), &command)| Binding {
                mode: *mode,
                keys,
                command,
            })
    }

    /// Name an existing prefix group for discovery. Bind its commands first.
    /// Prefixes can be nested; naming a group does not change its dispatch.
    pub fn name_group(&mut self, mode: Mode, prefix: Vec<Key>, title: &str) -> Result<(), Error> {
        if prefix.is_empty() {
            return Err(Error::EmptyBinding);
        }
        if !self
            .bindings
            .keys()
            .any(|(m, keys)| *m == mode && keys.len() > prefix.len() && keys.starts_with(&prefix))
        {
            return Err(Error::ConflictingBinding);
        }
        self.groups.insert(
            (mode, prefix),
            Group {
                title: title.into(),
                sticky: false,
            },
        );
        Ok(())
    }

    /// Name a prefix whose commands remain active until explicitly cancelled.
    pub fn name_sticky_group(
        &mut self,
        mode: Mode,
        prefix: Vec<Key>,
        title: &str,
    ) -> Result<(), Error> {
        self.name_group(mode, prefix.clone(), title)?;
        self.groups.get_mut(&(mode, prefix)).unwrap().sticky = true;
        Ok(())
    }

    fn hints(&self, mode: Mode, prefix: &[Key]) -> Option<KeyHints<'_>> {
        if prefix.is_empty() {
            return None;
        }
        let mut entries = BTreeMap::new();
        for ((m, keys), command) in &self.bindings {
            if *m != mode || !keys.starts_with(prefix) || keys.len() <= prefix.len() {
                continue;
            }
            let key = keys[prefix.len()];
            let description = if keys.len() == prefix.len() + 1 {
                command.description()
            } else {
                self.groups
                    .get(&(mode, keys[..prefix.len() + 1].to_vec()))
                    .map_or("More commands", |group| group.title.as_str())
            };
            entries.insert(key, description);
        }
        (!entries.is_empty()).then(|| KeyHints {
            title: self
                .groups
                .get(&(mode, prefix.to_vec()))
                .map_or("Keys", |group| group.title.as_str()),
            entries: entries.into_iter().collect(),
        })
    }
}

/// Vex's default bindings are inspired by Helix's selection-first editing model.
impl Default for Keymap {
    fn default() -> Self {
        use Key::*;
        let mut keymap = Self::empty();
        for mode in [Mode::Normal, Mode::Select] {
            for (keys, command) in [
                (vec![Char('h')], "move_left"),
                (vec![Char('l')], "move_right"),
                (vec![Char('j')], "move_down"),
                (vec![Char('k')], "move_up"),
                (vec![Ctrl('u')], "page_cursor_half_up"),
                (vec![Ctrl('d')], "page_cursor_half_down"),
                (vec![Ctrl('b')], "page_up"),
                (vec![Ctrl('f')], "page_down"),
                (vec![Char('w')], "move_word_forward"),
                (vec![Char('b')], "move_word_backward"),
                (vec![Char('e')], "move_word_end"),
                (vec![Char('W')], "move_long_word_forward"),
                (vec![Char('B')], "move_long_word_backward"),
                (vec![Char('E')], "move_long_word_end"),
                (vec![Char('f')], "find_next_char"),
                (vec![Char('F')], "find_prev_char"),
                (vec![Char('t')], "find_till_char"),
                (vec![Char('T')], "till_prev_char"),
                (vec![Char('x')], "select_line"),
                (vec![Char('X')], "extend_to_line_bounds"),
                (vec![Char('%')], "select_all"),
                (vec![Char(';')], "collapse_selection"),
                (vec![Char(',')], "keep_primary_selection"),
                (vec![Char('_')], "trim_selections"),
                (vec![Char('v')], "select_mode"),
                (vec![Char('i')], "insert_mode"),
                (vec![Char('a')], "append_mode"),
                (vec![Char('"')], "select_register"),
                (vec![Char(' '), Char('y')], "yank_to_clipboard"),
                (
                    vec![Char(' '), Char('Y')],
                    "yank_main_selection_to_clipboard",
                ),
                (vec![Char(' '), Char('p')], "paste_clipboard_after"),
                (vec![Char(' '), Char('P')], "paste_clipboard_before"),
                (
                    vec![Char(' '), Char('R')],
                    "replace_selections_with_clipboard",
                ),
                (vec![Char('.')], "repeat_insert"),
                (vec![Char('I')], "insert_at_line_start"),
                (vec![Char('A')], "insert_at_line_end"),
                (vec![Char('r')], "replace"),
                (vec![Char('m'), Char('i')], "select_textobject_inner"),
                (vec![Char('m'), Char('a')], "select_textobject_around"),
                (vec![Char('m'), Char('s')], "surround_add"),
                (vec![Char('=')], "format_selections"),
                (vec![Char('>')], "indent"),
                (vec![Char('<')], "unindent"),
                (vec![Char('J')], "join_selections"),
                (vec![Char('['), Char(' ')], "add_newline_above"),
                (vec![Char(']'), Char(' ')], "add_newline_below"),
                (vec![Char('o')], "open_below"),
                (vec![Char('O')], "open_above"),
                (vec![Char('y')], "yank"),
                (vec![Char('p')], "paste_after"),
                (vec![Char('P')], "paste_before"),
                (vec![Char('R')], "replace_with_yanked"),
                (vec![Char('d')], "delete_selection"),
                (vec![Char('c')], "change_selection"),
                (vec![Char('u')], "undo"),
                (vec![Char('U')], "redo"),
                (vec![Char('s')], "select_regex"),
                (vec![Char('S')], "split_selection"),
                (vec![Char('K')], "keep_selections"),
                (vec![Char('*')], "search_selection_detect_word_boundaries"),
                (vec![Char('/')], "search_forward"),
                (vec![Char('?')], "search_backward"),
                (vec![Char('n')], "search_next"),
                (vec![Char('N')], "search_previous"),
                (vec![Char('C')], "copy_selection_on_next_line"),
                (vec![Char('m'), Char('m')], "match_brackets"),
                (vec![Char('m'), Char('d')], "surround_delete"),
                (vec![Char('m'), Char('r')], "surround_replace"),
                (vec![Char(' '), Char('f')], "file_picker"),
                (vec![Char(' '), Char('e')], "file_browser"),
                (vec![Char(' '), Char('b')], "buffer_picker"),
                (vec![Char(' '), Char('j')], "jumplist_picker"),
                (vec![Char(' '), Char('\'')], "last_picker"),
                (vec![Char(' '), Char('/')], "global_search"),
                (vec![Char(' '), Char('s')], "symbol_picker"),
                (vec![Char(' '), Char('d')], "diagnostics_picker"),
                (vec![Char(' '), Char('D')], "workspace_diagnostics_picker"),
                (vec![Char(' '), Char('S')], "workspace_symbol_picker"),
                (vec![Char(' '), Char('k')], "hover"),
                (vec![Char(' '), Char('r')], "rename_symbol"),
                (vec![Char(' '), Char('a')], "code_action"),
                (vec![Char(' '), Char('c')], "toggle_comments"),
                (vec![Char(' '), Char('C')], "toggle_block_comments"),
                (vec![Ctrl('c')], "toggle_comments"),
                (vec![Ctrl('o')], "jump_backward"),
                (vec![Ctrl('i')], "jump_forward"),
                (vec![Tab], "jump_forward"),
                (vec![Ctrl('s')], "save_selection"),
                (vec![Char('g'), Char('d')], "goto_definition"),
                (vec![Char('g'), Char('y')], "goto_type_definition"),
                (vec![Char('g'), Char('i')], "goto_implementation"),
                (vec![Char('g'), Char('r')], "goto_reference"),
                (
                    vec![Char(' '), Char('h')],
                    "select_references_to_symbol_under_cursor",
                ),
                (vec![Char(']'), Char('d')], "goto_next_diagnostic"),
                (vec![Char('['), Char('D')], "goto_first_diagnostic"),
                (vec![Char(']'), Char('D')], "goto_last_diagnostic"),
                (vec![Char('['), Char('d')], "goto_previous_diagnostic"),
                (vec![Char('g'), Char('g')], "goto_file_start"),
                (vec![Char('g'), Char('a')], "goto_last_accessed_file"),
                (vec![Char('g'), Char('m')], "goto_last_modified_file"),
                (vec![Char('g'), Char('.')], "goto_last_modification"),
                (vec![Char('g'), Char('n')], "goto_next_buffer"),
                (vec![Char('g'), Char('p')], "goto_previous_buffer"),
                (vec![Char('g'), Char('f')], "goto_file"),
                (vec![Char('G')], "goto_line"),
                (vec![Char('g'), Char('|')], "goto_column"),
                (vec![Char('g'), Char('s')], "goto_first_nonwhitespace"),
                (vec![Char('g'), Char('e')], "goto_file_end"),
                (vec![Char('g'), Char('h')], "goto_line_start"),
                (vec![Char('g'), Char('l')], "goto_line_end"),
                (vec![Char('g'), Char('t')], "goto_window_top"),
                (vec![Char('g'), Char('c')], "goto_window_center"),
                (vec![Char('g'), Char('b')], "goto_window_bottom"),
            ] {
                keymap
                    .bind(mode, keys, command)
                    .expect("valid default binding");
            }
            for prefix in [Char('z'), Char('Z')] {
                for (key, command) in [
                    (Char('z'), "align_view_center"),
                    (Char('c'), "align_view_center"),
                    (Char('t'), "align_view_top"),
                    (Char('b'), "align_view_bottom"),
                    (Char('m'), "align_view_middle"),
                    (Char('j'), "scroll_down"),
                    (Down, "scroll_down"),
                    (Char('k'), "scroll_up"),
                    (Up, "scroll_up"),
                    (Ctrl('f'), "page_down"),
                    (PageDown, "page_down"),
                    (Ctrl('b'), "page_up"),
                    (PageUp, "page_up"),
                    (Ctrl('u'), "page_cursor_half_up"),
                    (Ctrl('d'), "page_cursor_half_down"),
                ] {
                    keymap.bind(mode, vec![prefix, key], command).unwrap();
                }
                if prefix == Char('Z') {
                    keymap
                        .name_sticky_group(mode, vec![prefix], "View (sticky)")
                        .unwrap();
                } else {
                    keymap.name_group(mode, vec![prefix], "View").unwrap();
                }
            }
            for prefix in [vec![Ctrl('w')], vec![Char(' '), Char('w')]] {
                for (key, command) in [
                    (Char('w'), "rotate_view"),
                    (Ctrl('w'), "rotate_view"),
                    (Char('v'), "vsplit"),
                    (Ctrl('v'), "vsplit"),
                    (Char('s'), "hsplit"),
                    (Ctrl('s'), "hsplit"),
                    (Char('='), "equalize_splits"),
                    (Char('f'), "goto_file_hsplit"),
                    (Char('F'), "goto_file_vsplit"),
                    (Char('q'), "wclose"),
                    (Ctrl('q'), "wclose"),
                    (Char('o'), "wonly"),
                    (Ctrl('o'), "wonly"),
                    (Char('h'), "jump_view_left"),
                    (Ctrl('h'), "jump_view_left"),
                    (Left, "jump_view_left"),
                    (Char('H'), "swap_view_left"),
                    (Char('j'), "jump_view_down"),
                    (Ctrl('j'), "jump_view_down"),
                    (Down, "jump_view_down"),
                    (Char('J'), "swap_view_down"),
                    (Char('k'), "jump_view_up"),
                    (Ctrl('k'), "jump_view_up"),
                    (Up, "jump_view_up"),
                    (Char('K'), "swap_view_up"),
                    (Char('l'), "jump_view_right"),
                    (Ctrl('l'), "jump_view_right"),
                    (Right, "jump_view_right"),
                    (Char('L'), "swap_view_right"),
                ] {
                    let mut keys = prefix.clone();
                    keys.push(key);
                    keymap.bind(mode, keys, command).unwrap();
                }
                keymap.name_group(mode, prefix, "Window").unwrap();
            }
            for (key, title) in [
                ('g', "Goto"),
                ('m', "Match"),
                (' ', "Space"),
                ('[', "Previous"),
                (']', "Next"),
            ] {
                keymap.name_group(mode, vec![Char(key)], title).unwrap();
            }
        }
        for mode in [Mode::Normal, Mode::Select, Mode::Insert] {
            for (key, command) in [
                (Left, "move_left"),
                (Right, "move_right"),
                (Up, "move_up"),
                (Down, "move_down"),
                (Home, "goto_line_start"),
                (End, "goto_line_end"),
                (PageUp, "page_up"),
                (PageDown, "page_down"),
                (Escape, "normal_mode"),
            ] {
                keymap
                    .bind(mode, vec![key], command)
                    .expect("valid default binding");
            }
        }
        keymap
            .bind(Mode::Insert, vec![Backspace], "delete_backward")
            .unwrap();
        keymap
            .bind(Mode::Insert, vec![Ctrl('h')], "delete_backward")
            .unwrap();
        keymap
            .bind(Mode::Insert, vec![Delete], "delete_forward")
            .unwrap();
        keymap
            .bind(Mode::Insert, vec![Enter], "insert_newline")
            .unwrap();
        keymap.bind(Mode::Insert, vec![Tab], "insert_tab").unwrap();
        keymap
            .bind(Mode::Insert, vec![Ctrl('x')], "completion")
            .unwrap();
        for (key, command) in [
            (Ctrl('w'), "delete_word_backward"),
            (Ctrl('u'), "kill_to_line_start"),
            (Ctrl('k'), "kill_to_line_end"),
            (Ctrl('d'), "delete_forward"),
            (Ctrl('j'), "insert_newline"),
            (Ctrl('s'), "commit_undo_checkpoint"),
            (Ctrl('r'), "insert_register"),
        ] {
            keymap.bind(Mode::Insert, vec![key], command).unwrap();
        }
        keymap
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dispatch {
    Pending,
    Executed(&'static str),
    Ignored,
}

/// Stateful key sequences and repeat counts. Text and commands can also be sent
/// directly to Editor; this adapter contains no editing implementation.
#[derive(Debug)]
pub struct KeyHandler {
    keymap: Keymap,
    pending: Vec<Key>,
    sticky: Vec<Key>,
    count: Option<usize>,
    mode: Option<Mode>,
    character_command: Option<&'static Command>,
    register_hints: Vec<(Key, String)>,
}

impl Default for KeyHandler {
    fn default() -> Self {
        Self::new(Keymap::default())
    }
}

impl KeyHandler {
    pub fn new(keymap: Keymap) -> Self {
        Self {
            keymap,
            pending: Vec::new(),
            sticky: Vec::new(),
            count: None,
            mode: None,
            character_command: None,
            register_hints: Vec::new(),
        }
    }

    pub fn keymap(&self) -> &Keymap {
        &self.keymap
    }
    pub fn pending_keys(&self) -> &[Key] {
        &self.pending
    }
    pub fn count(&self) -> Option<usize> {
        self.count
    }

    /// Snapshot bounded register previews once when a register prefix opens.
    /// Prompt and picker frontends can use the same helper without dispatching
    /// an editing command or copying complete register contents every frame.
    pub fn cache_register_hints(&mut self, editor: &Editor) {
        let mut hints: BTreeMap<char, String> = [
            ('"', "Last yanked text"),
            ('_', "Discard values"),
            ('#', "Selection indices"),
            ('.', "Current selections"),
            ('%', "Current file name"),
            ('+', "System clipboard"),
            ('*', "Primary clipboard"),
        ]
        .into_iter()
        .map(|(key, text)| (key, text.to_owned()))
        .collect();
        hints.extend(editor.register_previews());
        self.register_hints = hints
            .into_iter()
            .map(|(key, text)| (Key::Char(key), text))
            .collect();
        self.register_hints.push((Key::Escape, "Cancel".into()));
    }

    pub fn register_hints(&self, title: &'static str) -> KeyHints<'_> {
        KeyHints {
            title,
            entries: self
                .register_hints
                .iter()
                .map(|(key, text)| (*key, text.as_str()))
                .collect(),
        }
    }

    pub fn hints(&self) -> Option<KeyHints<'_>> {
        if let Some(command) = self.character_command
            && matches!(
                command.input,
                crate::CommandInput::RegisterSelect | crate::CommandInput::RegisterInsert
            )
        {
            return Some(self.register_hints(
                if command.input == crate::CommandInput::RegisterSelect {
                    "Select register"
                } else {
                    "Insert register"
                },
            ));
        }
        if let Some(command) = self.character_command
            && matches!(
                command.input,
                crate::CommandInput::SurroundDelete
                    | crate::CommandInput::SurroundReplace
                    | crate::CommandInput::SurroundReplacement
            )
        {
            let replacement = command.input == crate::CommandInput::SurroundReplacement;
            let mut entries = vec![
                (Key::Char('('), "Parentheses (either bracket)"),
                (Key::Char('['), "Square brackets"),
                (Key::Char('{'), "Braces"),
                (Key::Char('<'), "Angle brackets"),
                (Key::Char('"'), "Quotes or any character"),
                (Key::Escape, "Cancel"),
            ];
            if !replacement {
                entries.insert(0, (Key::Char('m'), "Nearest matching pair"));
            }
            return Some(KeyHints {
                title: match command.input {
                    crate::CommandInput::SurroundDelete => "Delete surrounding pair of",
                    crate::CommandInput::SurroundReplace => "Replace surrounding pair of",
                    _ => "Replace with a pair of",
                },
                entries,
            });
        }
        if self
            .character_command
            .is_some_and(|command| command.input == crate::CommandInput::SurroundAdd)
        {
            return Some(KeyHints {
                title: "Surround selections with",
                entries: vec![
                    (Key::Char('('), "Parentheses (either bracket)"),
                    (Key::Char('['), "Square brackets"),
                    (Key::Char('{'), "Braces"),
                    (Key::Char('<'), "Angle brackets"),
                    (Key::Char('"'), "Quotes or any character"),
                    (Key::Enter, "Line endings"),
                    (Key::Escape, "Cancel"),
                ],
            });
        }
        if let Some(command) = self.character_command
            && matches!(
                command.input,
                crate::CommandInput::TextobjectInner | crate::CommandInput::TextobjectAround
            )
        {
            return Some(KeyHints {
                title: if command.input == crate::CommandInput::TextobjectInner {
                    "Match inside"
                } else {
                    "Match around"
                },
                entries: vec![
                    (Key::Char('w'), "Word"),
                    (Key::Char('W'), "WORD"),
                    (Key::Char('p'), "Paragraph"),
                    (Key::Char('m'), "Closest surrounding pair"),
                    (Key::Char('('), "Delimiter pair (either bracket)"),
                    (Key::Char('"'), "Quotes or another delimiter"),
                    (Key::Escape, "Cancel"),
                ],
            });
        }
        if self.character_command.is_some() {
            return Some(KeyHints {
                title: "Character",
                entries: vec![
                    (Key::Enter, "Line ending"),
                    (Key::Tab, "Tab"),
                    (Key::Escape, "Cancel"),
                ],
            });
        }
        let mut hints = self.keymap.hints(self.mode?, &self.pending)?;
        if !self.sticky.is_empty() {
            hints.entries.push((Key::Escape, "Exit sticky mode"));
        }
        Some(hints)
    }

    /// Cancel input and restore any active surround preview.
    pub fn cancel(&mut self, editor: &mut Editor) {
        editor.clear_selected_register();
        editor.cancel_surround();
        self.reset();
    }

    fn reset(&mut self) {
        self.sticky.clear();
        self.reset_sequence();
    }

    fn reset_sequence(&mut self) {
        self.pending.clone_from(&self.sticky);
        self.count = None;
        self.character_command = None;
        self.register_hints.clear();
    }

    pub fn handle(&mut self, editor: &mut Editor, key: Key) -> Result<Dispatch, Error> {
        if self
            .character_command
            .is_some_and(|command| command.input == crate::CommandInput::SurroundReplacement)
            && !editor.replacing_surround()
        {
            self.reset();
        }
        if self.mode != Some(editor.mode()) {
            self.cancel(editor);
        }
        self.mode = Some(editor.mode());
        if key == Key::Escape
            || (key == Key::Ctrl('c')
                && (!self.pending.is_empty() || editor.selected_register().is_some()))
        {
            if !self.pending.is_empty()
                || self.count.is_some()
                || editor.selected_register().is_some()
            {
                self.cancel(editor);
                return Ok(Dispatch::Ignored);
            }
            self.cancel(editor);
            editor.execute("normal_mode", 1)?;
            self.mode = Some(editor.mode());
            return Ok(Dispatch::Executed("normal_mode"));
        }
        if let Some(command) = self.character_command {
            let character = match key {
                Key::Char(ch) if !ch.is_control() => ch,
                Key::Enter
                    if matches!(
                        command.input,
                        crate::CommandInput::Character | crate::CommandInput::SurroundAdd
                    ) =>
                {
                    '\n'
                }
                Key::Tab if command.input == crate::CommandInput::Character => '\t',
                _ => {
                    self.cancel(editor);
                    return Ok(Dispatch::Ignored);
                }
            };
            return self.invoke(editor, command, Some(character));
        }
        if editor.mode() != Mode::Insert
            && self.pending == self.sticky
            && let Key::Char(ch @ '0'..='9') = key
            && (ch != '0' || self.count.is_some())
        {
            let next = self
                .count
                .unwrap_or(0)
                .checked_mul(10)
                .and_then(|n| n.checked_add(ch as usize - '0' as usize));
            if next.is_none() {
                self.cancel(editor);
                return Err(Error::CountOverflow);
            }
            self.count = next;
            return Ok(Dispatch::Pending);
        }
        self.pending.push(key);
        if let Some(command) = self
            .keymap
            .bindings
            .get(&(editor.mode(), self.pending.clone()))
            .copied()
        {
            if command.input != crate::CommandInput::None {
                if matches!(
                    command.input,
                    crate::CommandInput::RegisterSelect | crate::CommandInput::RegisterInsert
                ) {
                    self.cache_register_hints(editor);
                }
                self.character_command = Some(command);
                return Ok(Dispatch::Pending);
            }
            return self.invoke(editor, command, None);
        }
        if self
            .keymap
            .bindings
            .keys()
            .any(|(mode, keys)| *mode == editor.mode() && keys.starts_with(&self.pending))
        {
            if self
                .keymap
                .groups
                .get(&(editor.mode(), self.pending.clone()))
                .is_some_and(|group| group.sticky)
            {
                self.sticky.clone_from(&self.pending);
            }
            return Ok(Dispatch::Pending);
        }
        if !self.sticky.is_empty() {
            // An unbound key must not fall through to editing or exit sticky mode.
            self.reset_sequence();
            return Ok(Dispatch::Ignored);
        }
        let single = self.pending.len() == 1;
        self.cancel(editor);
        if editor.mode() == Mode::Insert && single {
            let character = match key {
                Key::Char(ch) if !ch.is_control() => ch,
                _ => return Ok(Dispatch::Ignored),
            };
            editor.insert_character(character)?;
            return Ok(Dispatch::Executed("insert_character"));
        }
        Ok(Dispatch::Ignored)
    }

    fn invoke(
        &mut self,
        editor: &mut Editor,
        command: &'static Command,
        character: Option<char>,
    ) -> Result<Dispatch, Error> {
        let count = self.count;
        let continuation = (command.input == crate::CommandInput::SurroundReplace)
            .then(|| std::mem::take(&mut self.pending));
        self.reset_sequence();
        let mut context = crate::CommandContext::new(editor);
        context.count = std::num::NonZeroUsize::new(count.unwrap_or(1)).unwrap();
        context.count_given = count.is_some();
        context.character = character;
        (command.run)(&mut context)?;
        if self.mode != Some(editor.mode()) {
            self.reset();
        }
        if command.input == crate::CommandInput::RegisterSelect {
            self.count = count;
        }
        if let Some(mut pending) = continuation {
            pending.push(Key::Char(character.expect("collected surround character")));
            self.pending = pending;
            self.character_command = commands::find("surround_replace_finish");
        }
        self.mode = Some(editor.mode());
        Ok(Dispatch::Executed(command.name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use vex_core::{CharOffset, Document, grapheme};

    fn press(handler: &mut KeyHandler, editor: &mut Editor, keys: &str) {
        for key in keys.chars() {
            handler.handle(editor, Key::Char(key)).unwrap();
        }
    }

    #[test]
    fn view_prefixes_are_one_shot_or_sticky_and_cancel_without_editing() {
        use crate::{ApplicationAction, ViewAction};
        for mode in [Mode::Normal, Mode::Select] {
            let mut editor = Editor::new(Document::from("one\ntwo\nthree"));
            if mode == Mode::Select {
                editor.execute("select_mode", 1).unwrap();
            }
            let mut keys = KeyHandler::default();
            let before = editor.selections().clone();
            press(&mut keys, &mut editor, "3z");
            assert_eq!(keys.hints().unwrap().title, "View");
            press(&mut keys, &mut editor, "j");
            assert_eq!(
                editor.take_application_action(),
                Some(ApplicationAction::View(ViewAction::ScrollDown, 3))
            );
            assert!(keys.hints().is_none());
            assert!(keys.pending_keys().is_empty());
            assert_eq!(editor.selections(), &before);

            press(&mut keys, &mut editor, "2Zk");
            assert_eq!(
                editor.take_application_action(),
                Some(ApplicationAction::View(ViewAction::ScrollUp, 2))
            );
            assert_eq!(keys.hints().unwrap().title, "View (sticky)");
            press(&mut keys, &mut editor, "10j");
            assert_eq!(
                editor.take_application_action(),
                Some(ApplicationAction::View(ViewAction::ScrollDown, 10))
            );
            press(&mut keys, &mut editor, "j");
            assert_eq!(
                editor.take_application_action(),
                Some(ApplicationAction::View(ViewAction::ScrollDown, 1))
            );
            // Unbound editing keys do not leak into the underlying mode.
            press(&mut keys, &mut editor, "di:");
            assert_eq!(editor.mode(), mode);
            assert_eq!(editor.document().text().to_string(), "one\ntwo\nthree");
            assert_eq!(editor.selections(), &before);
            assert!(keys.hints().is_some());
            keys.handle(&mut editor, Key::Escape).unwrap();
            assert!(keys.hints().is_none());
            assert_eq!(editor.mode(), mode);
            press(&mut keys, &mut editor, "j");
            assert_ne!(editor.selections(), &before);
            assert!(editor.take_application_action().is_none());
        }
    }

    #[test]
    fn sticky_groups_are_remappable_and_reset_on_mode_changes() {
        let mut map = Keymap::empty();
        map.bind(
            Mode::Normal,
            vec![Key::Char('q'), Key::Char('i')],
            "insert_mode",
        )
        .unwrap();
        map.name_sticky_group(Mode::Normal, vec![Key::Char('q')], "Custom")
            .unwrap();
        let mut keys = KeyHandler::new(map);
        let mut editor = Editor::new(Document::from("text"));
        press(&mut keys, &mut editor, "qiZz");
        assert!(keys.pending_keys().is_empty());
        assert!(keys.hints().is_none());
        assert_eq!(editor.document().text().to_string(), "Zztext");
    }

    #[test]
    fn jump_bindings_forward_counts_to_the_application_in_both_modes() {
        for mode in [Mode::Normal, Mode::Select] {
            let mut editor = Editor::new(Document::from("text"));
            if mode == Mode::Select {
                editor.execute("select_mode", 1).unwrap();
            }
            press(&mut KeyHandler::default(), &mut editor, " j");
            assert_eq!(
                editor.take_application_action(),
                Some(crate::ApplicationAction::JumpPicker)
            );
            assert_eq!(editor.mode(), mode);
            for (key, forward, name) in [
                (Key::Ctrl('o'), false, "jump_backward"),
                (Key::Ctrl('i'), true, "jump_forward"),
                (Key::Tab, true, "jump_forward"),
            ] {
                let mut editor = Editor::new(Document::from("text"));
                if mode == Mode::Select {
                    editor.execute("select_mode", 1).unwrap();
                }
                let mut keys = KeyHandler::default();
                press(&mut keys, &mut editor, "12");
                assert_eq!(
                    keys.handle(&mut editor, key).unwrap(),
                    Dispatch::Executed(name)
                );
                assert_eq!(
                    editor.take_application_action(),
                    Some(crate::ApplicationAction::Jump { forward, count: 12 })
                );
                assert!(editor.take_language_action().is_none());
                assert_eq!(editor.mode(), mode);
            }
        }
    }

    #[test]
    fn character_arguments_preserve_counts_and_treat_digits_spaces_and_prefixes_literally() {
        for (argument, source, expected) in [
            ('1', "a1b1c1", "a1b1"),
            (':', "a:b:c:d", "a:b:"),
            (' ', "a b c d", "a b "),
            ('g', "agbgcg", "agbg"),
        ] {
            let mut editor = Editor::new(Document::from(source));
            let mut keys = KeyHandler::default();
            press(&mut keys, &mut editor, "2f");
            assert_eq!(keys.count(), Some(2));
            assert_eq!(keys.pending_keys(), &[Key::Char('f')]);
            assert_eq!(keys.hints().unwrap().title, "Character");
            assert_eq!(
                keys.handle(&mut editor, Key::Char(argument)).unwrap(),
                Dispatch::Executed("find_next_char")
            );
            let selection = editor.selections().primary();
            assert_eq!(
                editor
                    .document()
                    .text()
                    .slice(selection.start().0..selection.end().0),
                expected
            );
            assert!(keys.pending_keys().is_empty());
            assert_eq!(keys.count(), None);
            assert_eq!(editor.document().revision().get(), 0);
        }
        let mut editor = Editor::new(Document::from("a\tb\r\nc"));
        let mut keys = KeyHandler::default();
        press(&mut keys, &mut editor, "f");
        keys.handle(&mut editor, Key::Tab).unwrap();
        assert_eq!(editor.selections().primary().end(), CharOffset(2));
        press(&mut keys, &mut editor, "f");
        keys.handle(&mut editor, Key::Enter).unwrap();
        assert_eq!(editor.selections().primary().end(), CharOffset(5));
    }

    #[test]
    fn pending_character_cancellation_keeps_select_mode_and_custom_bindings_work() {
        for cancel in [Key::Escape, Key::Ctrl('c'), Key::Left, Key::PageDown] {
            let mut editor = Editor::new(Document::from("abcabc"));
            let mut keys = KeyHandler::default();
            press(&mut keys, &mut editor, "v2f");
            let selections = editor.selections().clone();
            assert_eq!(keys.handle(&mut editor, cancel).unwrap(), Dispatch::Ignored);
            assert_eq!(editor.selections(), &selections);
            assert_eq!(editor.mode(), Mode::Select);
            assert!(keys.pending_keys().is_empty());
            assert_eq!(keys.count(), None);
        }
        let mut map = Keymap::empty();
        map.bind(Mode::Normal, vec![Key::Char('q')], "find_next_char")
            .unwrap();
        let mut keys = KeyHandler::new(map);
        let mut editor = Editor::new(Document::from("a!b!"));
        press(&mut keys, &mut editor, "2q!");
        assert_eq!(editor.selections().primary().end(), CharOffset(4));
        press(&mut keys, &mut editor, "q");
        editor.execute("insert_mode", 1).unwrap();
        press(&mut keys, &mut editor, "x");
        assert_eq!(editor.document().text(), "xa!b!");
    }

    #[test]
    fn counted_line_and_grapheme_column_jumps_match_their_default_behavior() {
        let mut editor = Editor::new(Document::from("\t界e\u{301}x\r\n  next\r\nlast\r\n"));
        let mut keys = KeyHandler::default();
        press(&mut keys, &mut editor, "3g|");
        assert_eq!(editor.selections().primary().start(), CharOffset(2));
        press(&mut keys, &mut editor, "G");
        assert_eq!(editor.selections().primary().start(), CharOffset(2));
        press(&mut keys, &mut editor, "999g|");
        assert_eq!(editor.selections().primary().start(), CharOffset(5));
        press(&mut keys, &mut editor, "2gggs");
        assert_eq!(editor.selections().primary().start(), CharOffset(9));
        press(&mut keys, &mut editor, "999G");
        assert_eq!(editor.selections().primary().start(), CharOffset(15));
        press(&mut keys, &mut editor, "v1G");
        assert_eq!(editor.mode(), Mode::Select);
        assert_eq!(editor.selections().primary().head, CharOffset(0));
        assert_eq!(editor.selections().primary().anchor, CharOffset(16));
        keys.handle(&mut editor, Key::Escape).unwrap();
        press(&mut keys, &mut editor, "ge");
        assert_eq!(
            editor.selections().primary().head.0,
            editor.document().text().len_chars()
        );
        press(&mut keys, &mut editor, "gg");
        assert_eq!(editor.selections().primary().start(), CharOffset(0));
    }

    #[test]
    fn find_and_till_ranges_follow_direction_and_select_mode_anchors() {
        let mut editor = Editor::new(Document::from("a:b:c:d"));
        let mut keys = KeyHandler::default();
        press(&mut keys, &mut editor, "t:");
        assert_eq!(
            editor.selections().primary(),
            vex_core::Selection::new(CharOffset(0), CharOffset(3))
        );
        press(&mut keys, &mut editor, "t:");
        assert_eq!(
            editor.selections().primary(),
            vex_core::Selection::new(CharOffset(2), CharOffset(5))
        );
        press(&mut keys, &mut editor, "F:");
        assert_eq!(
            editor.selections().primary(),
            vex_core::Selection::new(CharOffset(5), CharOffset(3))
        );
        press(&mut keys, &mut editor, "T:");
        assert_eq!(
            editor.selections().primary(),
            vex_core::Selection::new(CharOffset(4), CharOffset(2))
        );
        press(&mut keys, &mut editor, ";vf:");
        assert_eq!(
            editor.selections().primary(),
            vex_core::Selection::new(CharOffset(2), CharOffset(4))
        );
        press(&mut keys, &mut editor, "F:");
        assert_eq!(
            editor.selections().primary(),
            vex_core::Selection::new(CharOffset(3), CharOffset(1))
        );
        let before = editor.selections().clone();
        press(&mut keys, &mut editor, "9fx");
        assert_eq!(editor.selections(), &before);
    }

    #[test]
    fn word_bindings_and_blank_line_start_work_in_both_selection_modes() {
        let mut editor = Editor::new(Document::from("foo.bar baz-qux\n \t\n"));
        let mut keys = KeyHandler::default();
        press(&mut keys, &mut editor, "E");
        assert_eq!(
            editor.selections().primary(),
            vex_core::Selection::new(CharOffset(0), CharOffset(7))
        );
        press(&mut keys, &mut editor, "ggW");
        assert_eq!(
            editor.selections().primary(),
            vex_core::Selection::new(CharOffset(0), CharOffset(8))
        );
        press(&mut keys, &mut editor, "glB");
        assert_eq!(
            editor.selections().primary(),
            vex_core::Selection::new(CharOffset(15), CharOffset(8))
        );
        press(&mut keys, &mut editor, "ggv2E");
        assert_eq!(
            editor.selections().primary(),
            vex_core::Selection::new(CharOffset(0), CharOffset(15))
        );
        editor.execute("normal_mode", 1).unwrap();
        press(&mut keys, &mut editor, "2ggl");
        let before = editor.selections().clone();
        press(&mut keys, &mut editor, "gs");
        assert_eq!(editor.selections(), &before);
    }

    #[test]
    fn yank_paste_bindings_work_in_normal_select_and_insert_modes() {
        for (key, expected) in [('p', "aab"), ('P', "aab"), ('R', "ab")] {
            for select in [false, true] {
                let mut editor = Editor::new(Document::from("ab"));
                let mut keys = KeyHandler::default();
                press(&mut keys, &mut editor, "y");
                if select {
                    press(&mut keys, &mut editor, "v");
                }
                keys.handle(&mut editor, Key::Char(key)).unwrap();
                assert_eq!(editor.document().text(), expected);
                assert_eq!(editor.mode(), Mode::Normal);
            }
        }
        let mut editor = Editor::new(Document::from(""));
        let mut keys = KeyHandler::default();
        press(&mut keys, &mut editor, "iypPR");
        assert_eq!(editor.document().text(), "ypPR");
    }

    #[test]
    fn window_prefix_aliases_cancel_and_dispatch_documented_functions() {
        use crate::{ApplicationAction, WindowAction};
        for mode in [Mode::Normal, Mode::Select] {
            for prefix in [vec![Key::Ctrl('w')], vec![Key::Char(' '), Key::Char('w')]] {
                let mut editor = Editor::new(Document::from("text"));
                if mode == Mode::Select {
                    editor.execute("select_mode", 1).unwrap();
                }
                let mut keys = KeyHandler::default();
                press(&mut keys, &mut editor, "3");
                for &key in &prefix {
                    keys.handle(&mut editor, key).unwrap();
                }
                assert_eq!(keys.hints().unwrap().title, "Window");
                keys.handle(&mut editor, Key::Escape).unwrap();
                assert_eq!(editor.mode(), mode);
                assert!(editor.take_application_action().is_none());
                assert_eq!(keys.count(), None);
                for (key, expected) in [
                    (Key::Char('v'), WindowAction::SplitVertical),
                    (Key::Ctrl('s'), WindowAction::SplitHorizontal),
                    (Key::Char('='), WindowAction::Equalize),
                    (Key::Left, WindowAction::FocusLeft),
                    (Key::Ctrl('q'), WindowAction::Close),
                    (Key::Char('L'), WindowAction::SwapRight),
                ] {
                    for &key in &prefix {
                        keys.handle(&mut editor, key).unwrap();
                    }
                    let Dispatch::Executed(name) = keys.handle(&mut editor, key).unwrap() else {
                        panic!("window command did not execute");
                    };
                    assert!(!commands::find(name).unwrap().description().is_empty());
                    assert_eq!(
                        editor.take_application_action(),
                        Some(ApplicationAction::Window(expected, 1))
                    );
                }
                press(&mut keys, &mut editor, "3");
                for &key in &prefix {
                    keys.handle(&mut editor, key).unwrap();
                }
                keys.handle(&mut editor, Key::Ctrl('w')).unwrap();
                assert_eq!(
                    editor.take_application_action(),
                    Some(ApplicationAction::Window(WindowAction::Rotate, 3))
                );
            }
        }
    }

    #[test]
    fn half_page_bindings_are_documented_counted_and_remappable() {
        use crate::ApplicationAction;
        for mode in [Mode::Normal, Mode::Select] {
            let mut editor = Editor::new(Document::from("text"));
            if mode == Mode::Select {
                editor.execute("select_mode", 1).unwrap();
            }
            let mut keys = KeyHandler::default();
            press(&mut keys, &mut editor, "3");
            assert_eq!(
                keys.handle(&mut editor, Key::Ctrl('d')).unwrap(),
                Dispatch::Executed("page_cursor_half_down")
            );
            assert_eq!(
                editor.take_application_action(),
                Some(ApplicationAction::HalfPageDown(3))
            );
            assert_eq!(keys.count(), None);
            keys.handle(&mut editor, Key::Ctrl('u')).unwrap();
            assert_eq!(
                editor.take_application_action(),
                Some(ApplicationAction::HalfPageUp(1))
            );
            assert_eq!(editor.mode(), mode);
        }
        let mut map = Keymap::empty();
        map.bind(Mode::Normal, vec![Key::Char('z')], "page_cursor_half_down")
            .unwrap();
        assert!(
            map.bindings()
                .next()
                .unwrap()
                .command
                .description()
                .contains("half the visible text height")
        );
        let mut editor = Editor::new(Document::from("text"));
        KeyHandler::new(map)
            .handle(&mut editor, Key::Char('z'))
            .unwrap();
        assert_eq!(
            editor.take_application_action(),
            Some(ApplicationAction::HalfPageDown(1))
        );
        editor.execute("insert_mode", 1).unwrap();
        let mut keys = KeyHandler::default();
        assert_eq!(
            keys.handle(&mut editor, Key::Ctrl('d')).unwrap(),
            Dispatch::Executed("delete_forward")
        );
        assert_eq!(
            keys.handle(&mut editor, Key::Ctrl('u')).unwrap(),
            Dispatch::Executed("kill_to_line_start")
        );
        assert!(editor.take_application_action().is_none());
    }

    #[test]
    fn diagnostic_picker_bindings_keep_normal_and_select_modes() {
        for mode in [Mode::Normal, Mode::Select] {
            for (keys, workspace) in [(" d", false), (" D", true)] {
                let mut editor = Editor::new(Document::from("fn example() {}"));
                if mode == Mode::Select {
                    editor.execute("select_mode", 1).unwrap();
                }
                press(&mut KeyHandler::default(), &mut editor, keys);
                assert_eq!(
                    editor.take_application_action(),
                    Some(crate::ApplicationAction::DiagnosticPicker(workspace))
                );
                assert_eq!(editor.mode(), mode);
            }
        }
    }

    #[test]
    fn language_navigation_bindings_are_documented_in_normal_and_select_modes() {
        for mode in [Mode::Normal, Mode::Select] {
            for (keys, action) in [
                ("gd", crate::LanguageAction::Definition),
                ("gy", crate::LanguageAction::TypeDefinition),
                ("gi", crate::LanguageAction::Implementation),
                ("gr", crate::LanguageAction::References),
                (" h", crate::LanguageAction::DocumentHighlights),
                (" r", crate::LanguageAction::Rename),
                (" a", crate::LanguageAction::CodeAction),
                ("=", crate::LanguageAction::FormatSelections),
                ("]d", crate::LanguageAction::NextDiagnostic),
                ("[d", crate::LanguageAction::PreviousDiagnostic),
                ("[D", crate::LanguageAction::FirstDiagnostic),
                ("]D", crate::LanguageAction::LastDiagnostic),
            ] {
                let mut editor = Editor::new(Document::from("name"));
                if mode == Mode::Select {
                    editor.execute("select_mode", 1).unwrap();
                }
                press(&mut KeyHandler::default(), &mut editor, keys);
                assert_eq!(editor.take_language_action(), Some(action));
                assert_eq!(editor.mode(), mode);
                assert_eq!(editor.document().text(), "name");
            }
        }
    }

    #[test]
    fn named_groups_use_command_docs_and_cancel_without_changing_the_editing_mode() {
        let mut editor = Editor::new(Document::from("abc"));
        editor.execute("select_mode", 1).unwrap();
        let mut keys = KeyHandler::default();
        press(&mut keys, &mut editor, " z"); // Unknown Space-z leaves the group.
        assert!(keys.hints().is_none());
        press(&mut keys, &mut editor, "3g");
        let hints = keys.hints().unwrap();
        assert_eq!(hints.title, "Goto");
        assert!(hints.entries.iter().any(|(key, doc)| *key == Key::Char('d')
            && *doc == commands::find("goto_definition").unwrap().description()));
        keys.handle(&mut editor, Key::Escape).unwrap();
        assert_eq!(editor.mode(), Mode::Select);
        assert!(keys.hints().is_none());
        assert_eq!(keys.count(), None);
        press(&mut keys, &mut editor, " f");
        assert_eq!(
            editor.take_application_action(),
            Some(crate::ApplicationAction::FilePicker)
        );
        assert!(keys.hints().is_none());
        keys.handle(&mut editor, Key::Escape).unwrap();
        assert_eq!(editor.mode(), Mode::Normal);
    }

    #[test]
    fn nested_custom_groups_are_discoverable_and_keep_repeat_counts() {
        let mut map = Keymap::empty();
        map.bind(
            Mode::Normal,
            vec![Key::Char('+'), Key::Char('m'), Key::Char('r')],
            "move_right",
        )
        .unwrap();
        map.name_group(Mode::Normal, vec![Key::Char('+')], "Custom")
            .unwrap();
        map.name_group(Mode::Normal, vec![Key::Char('+'), Key::Char('m')], "Move")
            .unwrap();
        let mut editor = Editor::new(Document::from("abcd"));
        let mut keys = KeyHandler::new(map);
        press(&mut keys, &mut editor, "2+");
        assert_eq!(keys.hints().unwrap().entries, [(Key::Char('m'), "Move")]);
        press(&mut keys, &mut editor, "m");
        assert_eq!(keys.hints().unwrap().title, "Move");
        press(&mut keys, &mut editor, "r");
        assert_eq!(editor.selections().primary().start(), CharOffset(2));
    }

    #[test]
    fn selection_then_edit_works_through_key_dispatch() {
        let mut editor = Editor::new(Document::from("hello world"));
        let mut keys = KeyHandler::default();
        press(&mut keys, &mut editor, "wd");
        assert_eq!(editor.document().text(), "world");
        press(&mut keys, &mut editor, "u");
        assert_eq!(editor.document().text(), "hello world");
        press(&mut keys, &mut editor, "cHi ");
        keys.handle(&mut editor, Key::Escape).unwrap();
        assert_eq!(editor.document().text(), "Hi world");
        assert_eq!(editor.mode(), Mode::Normal);
        assert_eq!(
            editor.selections().primary().range(),
            CharOffset(2)..CharOffset(3)
        );
    }

    #[test]
    fn selection_controls_dispatch_documented_functions_and_remain_literal_in_insert_mode() {
        for mode in [Mode::Normal, Mode::Select] {
            for (key, name) in [
                ('%', "select_all"),
                (';', "collapse_selection"),
                (',', "keep_primary_selection"),
                ('X', "extend_to_line_bounds"),
                ('_', "trim_selections"),
                ('x', "select_line"),
            ] {
                let mut editor = Editor::new(Document::from(" one \r\ntwo"));
                let mut keys = KeyHandler::default();
                if mode == Mode::Select {
                    press(&mut keys, &mut editor, "v");
                }
                assert_eq!(
                    keys.handle(&mut editor, Key::Char(key)).unwrap(),
                    Dispatch::Executed(name)
                );
                assert_eq!(editor.mode(), mode);
                assert!(!commands::find(name).unwrap().description().is_empty());
            }
        }
        let mut editor = Editor::new(Document::default());
        let mut keys = KeyHandler::default();
        press(&mut keys, &mut editor, "i%;,X_x");
        assert_eq!(editor.document().text(), "%;,X_x");
    }

    #[test]
    fn repeated_line_selection_and_trimming_work_with_cut_paste_and_undo() {
        let mut editor = Editor::new(Document::from("one\ntwo\nthree\n"));
        let mut keys = KeyHandler::default();
        press(&mut keys, &mut editor, "xxd");
        assert_eq!(editor.document().text(), "three\n");
        press(&mut keys, &mut editor, "P");
        assert_eq!(editor.document().text(), "one\ntwo\nthree\n");
        press(&mut keys, &mut editor, "uu");
        assert_eq!(editor.document().text(), "one\ntwo\nthree\n");
        assert_eq!(
            editor.selections().primary().range(),
            CharOffset(0)..CharOffset(8)
        );

        let mut editor = Editor::new(Document::from("  alpha  \n beta \n"));
        let mut keys = KeyHandler::default();
        press(&mut keys, &mut editor, "X_");
        assert_eq!(
            editor.selections().primary().range(),
            CharOffset(2)..CharOffset(7)
        );
        press(&mut keys, &mut editor, "y;");
        assert_eq!(
            editor.selections().primary().range(),
            CharOffset(6)..CharOffset(7)
        );
        press(&mut keys, &mut editor, "%R");
        assert_eq!(editor.document().text(), "alpha");
    }

    #[test]
    fn counts_prefixes_cancel_and_do_not_leak() {
        let mut editor = Editor::new(Document::from("one two three four"));
        let mut keys = KeyHandler::default();
        press(&mut keys, &mut editor, "2w");
        assert_eq!(
            editor.selections().primary().range(),
            CharOffset(0)..CharOffset(8)
        );
        assert_eq!(keys.count(), None);
        assert_eq!(
            keys.handle(&mut editor, Key::Char('g')).unwrap(),
            Dispatch::Pending
        );
        assert_eq!(keys.pending_keys(), &[Key::Char('g')]);
        assert_eq!(
            keys.handle(&mut editor, Key::Char('?')).unwrap(),
            Dispatch::Ignored
        );
        assert!(keys.pending_keys().is_empty());
        press(&mut keys, &mut editor, "9g");
        keys.handle(&mut editor, Key::Escape).unwrap();
        assert!(keys.pending_keys().is_empty());
        assert_eq!(keys.count(), None);
        press(&mut keys, &mut editor, "ggl");
        assert_eq!(
            editor.selections().primary().range(),
            CharOffset(1)..CharOffset(2)
        );
        press(&mut keys, &mut editor, "ge");
        assert_eq!(editor.selections().primary().head, CharOffset(18));
        assert_eq!(editor.document().revision().get(), 0);
    }

    #[test]
    fn insert_mode_treats_digits_and_motion_letters_as_text() {
        let mut editor = Editor::new(Document::default());
        let mut keys = KeyHandler::default();
        press(&mut keys, &mut editor, "i20hjkl🦀");
        keys.handle(&mut editor, Key::Backspace).unwrap();
        keys.handle(&mut editor, Key::Enter).unwrap();
        keys.handle(&mut editor, Key::Tab).unwrap();
        assert_eq!(editor.document().text(), "20hjkl\n    ");
        assert_eq!(keys.count(), None);
        keys.handle(&mut editor, Key::Escape).unwrap();
        assert_eq!(
            editor.selections().primary().range(),
            CharOffset(10)..CharOffset(11)
        );
    }

    #[test]
    fn tabs_at_multiple_carets_group_with_typing_and_repeat_across_languages() {
        use crate::Language;
        use vex_core::{Selection, SelectionSet};
        for language in std::iter::once(None).chain(Language::ALL.iter().copied().map(Some)) {
            let source = "é\r\n界";
            let mut editor = Editor::new(Document::from(source));
            editor.set_language(language);
            let indent = match editor.indentation().style {
                crate::IndentStyle::Spaces(width) => " ".repeat(width.get()),
                crate::IndentStyle::Tabs => "\t".into(),
            };
            let width = indent.len();
            editor
                .set_selections(
                    SelectionSet::new(
                        vec![
                            Selection::cursor(CharOffset(0)),
                            Selection::cursor(CharOffset(3)),
                        ],
                        1,
                    )
                    .unwrap(),
                )
                .unwrap();
            let mut keys = KeyHandler::default();
            press(&mut keys, &mut editor, "i");
            assert_eq!(
                keys.handle(&mut editor, Key::Tab).unwrap(),
                Dispatch::Executed("insert_tab")
            );
            press(&mut keys, &mut editor, "x");
            let expected = format!("{indent}xé\r\n{indent}x界");
            assert_eq!(editor.document().text(), expected.as_str());
            assert_eq!(
                editor.selections().ranges(),
                &[
                    Selection::cursor(CharOffset(width + 1)),
                    Selection::cursor(CharOffset(3 + 2 * (width + 1))),
                ]
            );
            assert_eq!(editor.selections().primary_index(), 1);
            keys.handle(&mut editor, Key::Escape).unwrap();
            press(&mut keys, &mut editor, "u");
            assert_eq!(editor.document().text(), source);
            press(&mut keys, &mut editor, ".");
            assert_eq!(editor.document().text(), expected.as_str());
            press(&mut keys, &mut editor, "u");
            assert_eq!(editor.document().text(), source);
        }
    }

    #[test]
    fn open_line_bindings_enter_insert_mode_and_group_following_typing() {
        for mode in [Mode::Normal, Mode::Select] {
            for (key, command, expected) in [
                ('o', "open_below", "one\nx\nx\ntwo"),
                ('O', "open_above", "x\nx\none\ntwo"),
            ] {
                let mut editor = Editor::new(Document::from("one\ntwo"));
                let mut keys = KeyHandler::default();
                if mode == Mode::Select {
                    press(&mut keys, &mut editor, "v");
                }
                press(&mut keys, &mut editor, "2");
                assert_eq!(
                    keys.handle(&mut editor, Key::Char(key)).unwrap(),
                    Dispatch::Executed(command)
                );
                assert_eq!(editor.mode(), Mode::Insert);
                assert_eq!(keys.count(), None);
                press(&mut keys, &mut editor, "x");
                keys.handle(&mut editor, Key::Escape).unwrap();
                assert_eq!(editor.document().text(), expected);
                assert_eq!(editor.document().undo_depth(), 1);
                press(&mut keys, &mut editor, "u");
                assert_eq!(editor.document().text(), "one\ntwo");
            }
        }
    }

    #[test]
    fn backspace_aliases_delete_whole_graphemes_only_in_insert_mode() {
        for key in [Key::Backspace, Key::Ctrl('h')] {
            let mut editor = Editor::new(Document::from("e\u{301}👩\u{200d}💻\r\n"));
            let mut keys = KeyHandler::default();
            for prefix in ["l", "v"] {
                press(&mut keys, &mut editor, prefix);
                assert_eq!(keys.handle(&mut editor, key).unwrap(), Dispatch::Ignored);
                assert_eq!(editor.document().undo_depth(), 0);
            }
            keys.handle(&mut editor, Key::Escape).unwrap();
            press(&mut keys, &mut editor, "gei");
            for expected in ["e\u{301}👩\u{200d}💻", "e\u{301}", "", ""] {
                assert_eq!(
                    keys.handle(&mut editor, key).unwrap(),
                    Dispatch::Executed("delete_backward")
                );
                assert_eq!(editor.document().text(), expected);
            }
        }
    }

    #[test]
    fn enter_and_open_lines_retain_loaded_line_endings_after_deletion() {
        for newline in ["\n", "\r\n", "\r"] {
            let mut editor = Editor::new(Document::from(format!("a{newline}").as_str()));
            let mut keys = KeyHandler::default();
            press(&mut keys, &mut editor, "xd");
            assert_eq!(editor.document().text(), "");
            press(&mut keys, &mut editor, "o");
            assert_eq!(editor.document().text(), newline);
            keys.handle(&mut editor, Key::Backspace).unwrap();
            assert_eq!(editor.document().text(), "");
            keys.handle(&mut editor, Key::Enter).unwrap();
            assert_eq!(editor.document().text(), newline);
            keys.handle(&mut editor, Key::Escape).unwrap();
            press(&mut keys, &mut editor, "O");
            assert_eq!(editor.document().text(), newline.repeat(2).as_str());
        }
    }

    #[test]
    fn enter_uses_a_remappable_documented_newline_command() {
        let mut editor = Editor::new(Document::from("  one"));
        let mut keys = KeyHandler::default();
        press(&mut keys, &mut editor, "gei");
        assert_eq!(
            keys.handle(&mut editor, Key::Enter).unwrap(),
            Dispatch::Executed("insert_newline")
        );
        press(&mut keys, &mut editor, "two");
        assert_eq!(editor.document().text(), "  one\n  two");
        assert_eq!(editor.document().undo_depth(), 1);

        let mut map = Keymap::default();
        map.bind(Mode::Insert, vec![Key::Enter], "delete_backward")
            .unwrap();
        let mut keys = KeyHandler::new(map);
        assert_eq!(
            keys.handle(&mut editor, Key::Enter).unwrap(),
            Dispatch::Executed("delete_backward")
        );
        assert_eq!(editor.document().text(), "  one\n  tw");
    }

    #[test]
    fn typing_sessions_and_counts_operate_on_undo_groups() {
        let mut editor = Editor::new(Document::default());
        let mut keys = KeyHandler::default();
        press(&mut keys, &mut editor, "ihello");
        keys.handle(&mut editor, Key::Escape).unwrap();
        press(&mut keys, &mut editor, "a world");
        keys.handle(&mut editor, Key::Escape).unwrap();
        assert_eq!(editor.document().text(), "hello world");
        assert_eq!(editor.document().undo_depth(), 2);
        press(&mut keys, &mut editor, "u");
        assert_eq!(editor.document().text(), "hello");
        press(&mut keys, &mut editor, "u2U");
        assert_eq!(editor.document().text(), "hello world");
        press(&mut keys, &mut editor, "2u");
        assert_eq!(editor.document().text(), "");
        // Typing on a new branch cannot keep the abandoned redo groups.
        press(&mut keys, &mut editor, "ifresh");
        keys.handle(&mut editor, Key::Escape).unwrap();
        assert_eq!(editor.document().redo_depth(), 0);
        press(&mut keys, &mut editor, "u");
        assert_eq!(editor.document().text(), "");
    }

    #[test]
    fn custom_bindings_resolve_to_the_same_documented_function() {
        let mut map = Keymap::default();
        map.bind(Mode::Normal, vec![Key::Char('h')], "move_word_forward")
            .unwrap();
        let binding = map
            .bindings()
            .find(|b| b.mode == Mode::Normal && b.keys == [Key::Char('h')])
            .unwrap();
        assert_eq!(binding.command.name, "move_word_forward");
        assert!(binding.command.description().contains("next word start"));
        let mut editor = Editor::new(Document::from("hello world"));
        let mut keys = KeyHandler::new(map);
        keys.handle(&mut editor, Key::Char('h')).unwrap();
        assert_eq!(
            editor.selections().primary().range(),
            CharOffset(0)..CharOffset(6)
        );
    }

    #[test]
    fn binding_errors_do_not_replace_existing_bindings() {
        let mut map = Keymap::default();
        let count = map.bindings().count();
        assert_eq!(
            map.bind(Mode::Normal, vec![Key::Char('g')], "move_right"),
            Err(Error::ConflictingBinding)
        );
        assert_eq!(
            map.bind(
                Mode::Normal,
                vec![Key::Char('h'), Key::Char('h')],
                "move_right"
            ),
            Err(Error::ConflictingBinding)
        );
        assert_eq!(
            map.bind(Mode::Normal, vec![Key::Char('4')], "move_right"),
            Err(Error::ReservedBinding)
        );
        assert_eq!(
            map.bind(Mode::Insert, vec![Key::Escape], "move_right"),
            Err(Error::ReservedBinding)
        );
        assert_eq!(
            map.bind(Mode::Normal, vec![], "move_right"),
            Err(Error::EmptyBinding)
        );
        assert!(matches!(
            map.bind(Mode::Normal, vec![Key::Char('z')], "missing"),
            Err(Error::UnknownCommand(_))
        ));
        assert_eq!(map.bindings().count(), count);
        map.bind(Mode::Normal, vec![Key::Char('h')], "move_right")
            .unwrap();
        assert_eq!(map.bindings().count(), count);
    }

    #[test]
    fn count_overflow_and_external_mode_changes_clear_pending_input() {
        let mut editor = Editor::new(Document::from("abc"));
        let mut keys = KeyHandler::default();
        press(&mut keys, &mut editor, &usize::MAX.to_string());
        assert_eq!(
            keys.handle(&mut editor, Key::Char('9')),
            Err(Error::CountOverflow)
        );
        assert_eq!(keys.count(), None);
        press(&mut keys, &mut editor, "2g");
        editor.execute("insert_mode", 1).unwrap();
        keys.handle(&mut editor, Key::Char('h')).unwrap();
        assert_eq!(editor.document().text(), "habc");
        assert_eq!(keys.count(), None);
        assert!(keys.pending_keys().is_empty());
    }

    #[test]
    fn everyday_edit_bindings_dispatch_counts_and_line_prefixes() {
        for mode in [Mode::Normal, Mode::Select] {
            for (sequence, command, after) in [
                ("I!", "insert_at_line_start", "  !one\n two"),
                ("A!", "insert_at_line_end", "  one!\n two"),
                ("2>", "indent", "        one\n two"),
                ("2<", "unindent", "one\n two"),
                ("J", "join_selections", "  one two"),
                ("2[ ", "add_newline_above", "\n\n  one\n two"),
                ("2] ", "add_newline_below", "  one\n\n\n two"),
            ] {
                let mut editor = Editor::new(Document::from("  one\n two"));
                if mode == Mode::Select {
                    editor.execute("select_mode", 1).unwrap();
                }
                let mut keys = KeyHandler::default();
                let binding = keys
                    .keymap()
                    .bindings()
                    .find(|binding| binding.mode == mode && binding.command.name == command)
                    .unwrap();
                assert!(!binding.command.description().is_empty());
                press(&mut keys, &mut editor, sequence);
                assert_eq!(editor.document().text(), after, "{sequence}");
                assert_eq!(keys.count(), None);
                assert!(keys.pending_keys().is_empty());
                if editor.mode() == Mode::Insert {
                    keys.handle(&mut editor, Key::Escape).unwrap();
                }
                press(&mut keys, &mut editor, "u");
                assert_eq!(editor.document().text(), "  one\n two");
            }
        }
    }

    #[test]
    fn replace_waits_for_a_literal_character_and_cancels_without_editing() {
        for cancel in [Key::Escape, Key::Ctrl('c'), Key::Left] {
            let mut editor = Editor::new(Document::from("e\u{301}🦀\r\n"));
            let mut keys = KeyHandler::default();
            press(&mut keys, &mut editor, "v2r");
            assert_eq!(keys.hints().unwrap().title, "Character");
            keys.handle(&mut editor, cancel).unwrap();
            assert_eq!(editor.mode(), Mode::Select);
            assert_eq!(editor.document().undo_depth(), 0);
            assert!(keys.pending_keys().is_empty());
            assert_eq!(keys.count(), None);
        }
        for (argument, expected) in [
            (Key::Char(':'), ":🦀\r\n"),
            (Key::Char('5'), "5🦀\r\n"),
            (Key::Tab, "\t🦀\r\n"),
            (Key::Enter, "\r\n🦀\r\n"),
        ] {
            let mut editor = Editor::new(Document::from("e\u{301}🦀\r\n"));
            let mut keys = KeyHandler::default();
            press(&mut keys, &mut editor, "v99r");
            assert_eq!(
                keys.handle(&mut editor, argument).unwrap(),
                Dispatch::Executed("replace")
            );
            assert_eq!(editor.document().text(), expected);
            assert_eq!(editor.mode(), Mode::Normal);
            assert_eq!(keys.count(), None);
            press(&mut keys, &mut editor, "u");
            assert_eq!(editor.document().text(), "e\u{301}🦀\r\n");
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]
        #[test]
        fn arbitrary_key_sequences_preserve_grapheme_and_mode_invariants(
            keys in prop::collection::vec(prop_oneof![
                prop::sample::select("hjklwbeWBEfFtTvdciaIArJxX%;,_uU025gGs|$".chars().map(Key::Char).collect::<Vec<_>>()),
                Just(Key::Escape), Just(Key::Backspace), Just(Key::Delete), Just(Key::Enter),
                Just(Key::Char('🦀')), Just(Key::Char('\u{301}')), Just(Key::Char('\u{200d}')),
                Just(Key::Down), Just(Key::Up), Just(Key::Left), Just(Key::Right),
            ], 0..150),
        ) {
            let mut editor = Editor::new(Document::from("e\u{301} 👩\u{200d}💻\r\n日本語\nlast"));
            let mut input = KeyHandler::default();
            for key in keys {
                // The frontend routes keys to a prompt while it is open. This
                // key-dispatch fuzzer closes prompts before its next command.
                if editor.search_prompt().is_some() {
                    editor.execute("search_cancel", 1).unwrap();
                }
                let result = input.handle(&mut editor, key);
                prop_assert!(result.is_ok() || result == Err(Error::CountOverflow));
                let text = editor.document().text();
                for selection in editor.selections().ranges() {
                    prop_assert!(selection.end().0 <= text.len_chars());
                    prop_assert!(grapheme::is_boundary(text, selection.anchor).unwrap());
                    prop_assert!(grapheme::is_boundary(text, selection.head).unwrap());
                    if editor.mode() == Mode::Insert {
                        prop_assert!(selection.is_empty());
                    } else {
                        prop_assert!(!selection.is_empty() || selection.head.0 == text.len_chars());
                    }
                }
            }
        }
    }
}
