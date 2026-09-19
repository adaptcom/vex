//! Mode-specific key sequences resolve to documented command functions.

use crate::{Command, Editor, Error, Mode, commands};
use std::{collections::BTreeMap, fmt};

/// Logical keys, independent of terminal escape sequences and input libraries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Escape,
    Enter,
    Tab,
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
}

impl Keymap {
    /// An empty map for custom bindings. The handler still reserves Escape and counts.
    pub fn empty() -> Self {
        Self {
            bindings: BTreeMap::new(),
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
                (vec![Char('w')], "move_word_forward"),
                (vec![Char('b')], "move_word_backward"),
                (vec![Char('e')], "move_word_end"),
                (vec![Char('x')], "select_line"),
                (vec![Char('v')], "select_mode"),
                (vec![Char('i')], "insert_mode"),
                (vec![Char('a')], "append_mode"),
                (vec![Char('d')], "delete_selection"),
                (vec![Char('c')], "change_selection"),
                (vec![Char('u')], "undo"),
                (vec![Char('U')], "redo"),
                (vec![Ctrl('r')], "redo"),
                (vec![Char('g'), Char('g')], "goto_file_start"),
                (vec![Char('g'), Char('e')], "goto_file_end"),
                (vec![Char('g'), Char('h')], "goto_line_start"),
                (vec![Char('g'), Char('l')], "goto_line_end"),
                (vec![Char('0')], "goto_line_start"),
                (vec![Char('$')], "goto_line_end"),
            ] {
                keymap
                    .bind(mode, keys, command)
                    .expect("valid default binding");
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
            .bind(Mode::Insert, vec![Delete], "delete_forward")
            .unwrap();
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
    count: Option<usize>,
    mode: Option<Mode>,
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
            count: None,
            mode: None,
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

    pub fn cancel(&mut self) {
        self.pending.clear();
        self.count = None;
    }

    pub fn handle(&mut self, editor: &mut Editor, key: Key) -> Result<Dispatch, Error> {
        if self.mode != Some(editor.mode()) {
            self.cancel();
        }
        self.mode = Some(editor.mode());
        if key == Key::Escape {
            self.cancel();
            editor.execute("normal_mode", 1)?;
            self.mode = Some(editor.mode());
            return Ok(Dispatch::Executed("normal_mode"));
        }
        if editor.mode() != Mode::Insert
            && self.pending.is_empty()
            && let Key::Char(ch @ '0'..='9') = key
            && (ch != '0' || self.count.is_some())
        {
            let next = self
                .count
                .unwrap_or(0)
                .checked_mul(10)
                .and_then(|n| n.checked_add(ch as usize - '0' as usize));
            if next.is_none() {
                self.cancel();
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
            let count = self.count.unwrap_or(1);
            self.cancel();
            let mut context = crate::CommandContext::new(editor);
            context.count = std::num::NonZeroUsize::new(count).unwrap();
            (command.run)(&mut context)?;
            self.mode = Some(editor.mode());
            return Ok(Dispatch::Executed(command.name));
        }
        if self
            .keymap
            .bindings
            .keys()
            .any(|(mode, keys)| *mode == editor.mode() && keys.starts_with(&self.pending))
        {
            return Ok(Dispatch::Pending);
        }
        let single = self.pending.len() == 1;
        self.cancel();
        if editor.mode() == Mode::Insert && single {
            let mut buffer = [0; 4];
            let text = match key {
                Key::Char(ch) if !ch.is_control() => ch.encode_utf8(&mut buffer),
                Key::Enter => "\n",
                Key::Tab => "\t",
                _ => return Ok(Dispatch::Ignored),
            };
            editor.insert_text(text)?;
            return Ok(Dispatch::Executed("insert_text"));
        }
        Ok(Dispatch::Ignored)
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
        assert_eq!(editor.document().text(), "20hjkl\n\t");
        assert_eq!(keys.count(), None);
        keys.handle(&mut editor, Key::Escape).unwrap();
        assert_eq!(
            editor.selections().primary().range(),
            CharOffset(7)..CharOffset(8)
        );
    }

    #[test]
    fn custom_bindings_resolve_to_the_same_documented_function() {
        let mut map = Keymap::default();
        map.bind(Mode::Normal, vec![Key::Char('z')], "move_word_forward")
            .unwrap();
        let binding = map
            .bindings()
            .find(|b| b.mode == Mode::Normal && b.keys == [Key::Char('z')])
            .unwrap();
        assert_eq!(binding.command.name, "move_word_forward");
        assert!(binding.command.description().contains("next word start"));
        let mut editor = Editor::new(Document::from("hello world"));
        let mut keys = KeyHandler::new(map);
        keys.handle(&mut editor, Key::Char('z')).unwrap();
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

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]
        #[test]
        fn arbitrary_key_sequences_preserve_grapheme_and_mode_invariants(
            keys in prop::collection::vec(prop_oneof![
                prop::sample::select("hjklwbevdciaxuU025g$".chars().map(Key::Char).collect::<Vec<_>>()),
                Just(Key::Escape), Just(Key::Backspace), Just(Key::Delete), Just(Key::Enter),
                Just(Key::Char('🦀')), Just(Key::Char('\u{301}')), Just(Key::Char('\u{200d}')),
                Just(Key::Down), Just(Key::Up), Just(Key::Left), Just(Key::Right),
            ], 0..150),
        ) {
            let mut editor = Editor::new(Document::from("e\u{301} 👩\u{200d}💻\r\n日本語\nlast"));
            let mut input = KeyHandler::default();
            for key in keys {
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
