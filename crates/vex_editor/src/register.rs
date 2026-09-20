//! Session-shared yank storage and atomic, selection-aware paste operations.

use std::{
    borrow::Cow,
    collections::BTreeMap,
    ops::Range,
    sync::{Arc, Mutex},
};
use vex_core::{
    Affinity, CharOffset, Edit, Selection, SelectionSet, Snapshot, Transaction, grapheme, motion,
};

use crate::{CommandContext, Editor, Error, Mode};

pub type RegisterValues = Arc<[Arc<str>]>;
type Fragments = RegisterValues;

/// Internal text registers shared by all buffers in an editor session.
/// Clones share immutable text; undo/redo and buffer lifetimes do not own it.
/// A standalone [`Editor::new`] starts with its own empty register.
#[derive(Clone, Debug, Default)]
pub struct YankRegister {
    values: Arc<Mutex<StoredRegisters>>,
}

#[derive(Debug)]
struct StoredRegisters {
    values: BTreeMap<char, Fragments>,
    last_search: char,
}

impl Default for StoredRegisters {
    fn default() -> Self {
        Self {
            values: BTreeMap::new(),
            last_search: '/',
        }
    }
}

impl YankRegister {
    fn read(&self) -> Fragments {
        self.read_named('"')
    }

    fn read_named(&self, name: char) -> Fragments {
        self.values
            .lock()
            .expect("register lock")
            .values
            .get(&name)
            .cloned()
            .unwrap_or_default()
    }

    fn write_named(&self, name: char, values: Fragments) {
        if name != '_' {
            self.values
                .lock()
                .expect("register lock")
                .values
                .insert(name, values);
        }
    }

    pub(crate) fn last_search(&self) -> char {
        self.values.lock().expect("register lock").last_search
    }

    pub(crate) fn remember_search(
        &self,
        name: char,
        query: Arc<str>,
        activate: bool,
    ) -> Result<(), Error> {
        writable(name)?;
        let mut stored = self.values.lock().expect("register lock");
        if name != '_' {
            stored.values.insert(name, Arc::from([query]));
        }
        if activate {
            stored.last_search = name;
        }
        Ok(())
    }
}

pub(crate) fn writable(name: char) -> Result<(), Error> {
    match name {
        '#' | '.' | '%' => Err(Error::ReadOnlyRegister(name)),
        '+' | '*' => Err(Error::ExternalRegister(name)),
        _ => Ok(()),
    }
}

pub(crate) fn capture_for(editor: &Editor, name: char) -> Result<Fragments, Error> {
    writable(name)?;
    Ok(if name == '_' {
        Arc::from([])
    } else {
        capture(editor)
    })
}

impl Editor {
    /// Read a named register. Uppercase names are independent of lowercase.
    /// Dynamic registers return current selection indices (#) or text (.).
    /// Frontends supply a display name (%) and resolve clipboard registers (+/*).
    pub fn register(&self, name: char) -> Result<RegisterValues, Error> {
        Ok(match name {
            '_' => Arc::from([]),
            '#' => (1..=self.selections.ranges().len())
                .map(|index| Arc::from(index.to_string()))
                .collect(),
            '.' => capture(self),
            '%' => Arc::from([self.display_name.clone()]),
            '+' | '*' => return Err(Error::ExternalRegister(name)),
            '"' => self.yank_register.read(),
            _ => self.yank_register.read_named(name),
        })
    }

    pub fn set_register(&mut self, name: char, values: RegisterValues) -> Result<(), Error> {
        writable(name)?;
        self.yank_register.write_named(name, values);
        Ok(())
    }

    /// Set the buffer's display name for `%`, without accessing the filesystem.
    /// Frontends update this after opening a file or successfully saving as one.
    pub fn set_display_name(&mut self, name: Arc<str>) {
        self.display_name = name;
    }

    /// Read only the first fragment for a prompt. In particular, `.` does not
    /// copy every selected range when the caller needs just one.
    pub fn register_first(&self, name: char) -> Result<Option<Arc<str>>, Error> {
        if name == '%' {
            return Ok(Some(self.display_name.clone()));
        }
        if name == '.' {
            let selection = self.selections.ranges()[0];
            return Ok(Some(Arc::from(
                self.document
                    .text()
                    .slice(selection.start().0..selection.end().0)
                    .to_string(),
            )));
        }
        if name == '#' {
            return Ok(Some(Arc::from("1")));
        }
        Ok(self.register(name)?.first().cloned())
    }

    pub fn selected_register(&self) -> Option<char> {
        self.selected_register
    }

    pub fn clear_selected_register(&mut self) {
        self.selected_register = None;
    }

    /// Return bounded previews for a register input helper. Never join fragments
    /// or scan a long first line just to render a popup.
    pub fn register_previews(&self) -> Vec<(char, String)> {
        let values = self.yank_register.values.lock().expect("register lock");
        values
            .values
            .iter()
            .take(64)
            .map(|(&name, fragments)| {
                let text = fragments.first().map_or("", AsRef::as_ref);
                (
                    name,
                    text.chars()
                        .take(48)
                        .take_while(|ch| !matches!(ch, '\r' | '\n'))
                        .collect(),
                )
            })
            .collect()
    }
}

pub(crate) fn capture(editor: &Editor) -> Fragments {
    editor
        .selections
        .ranges()
        .iter()
        .map(|selection| {
            Arc::from(
                editor
                    .document
                    .text()
                    .slice(selection.start().0..selection.end().0)
                    .to_string(),
            )
        })
        .collect()
}

/// Where supplied register fragments are inserted relative to selections.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Paste {
    Before,
    After,
    Replace,
    /// Insert at each caret without applying linewise paste behavior.
    Cursor,
}

/// Immutable preparation inputs for clipboard/register edits on a worker.
/// Cloning the snapshot shares rope storage rather than copying document text.
pub struct PastePlan {
    snapshot: Snapshot,
    selections: SelectionSet,
    newline: &'static str,
}

impl Editor {
    pub fn paste_plan(&self) -> PastePlan {
        PastePlan {
            snapshot: self.document.snapshot(),
            selections: self.selections.clone(),
            newline: self.newline(),
        }
    }

    /// Apply a prepared paste as one undo step without changing any register.
    /// The frontend must check view and selection identity before delivery;
    /// transaction validation rejects changed documents and revisions.
    pub fn apply_paste(&mut self, transaction: Transaction) -> Result<(), Error> {
        self.apply(transaction, false)?;
        self.mode = Mode::Normal;
        self.selections = self.normalized(self.selections.clone(), self.mode)?;
        self.preferred_columns = None;
        Ok(())
    }

    /// Insert prepared register fragments at carets while retaining insert mode.
    pub fn apply_register_insert(&mut self, transaction: Transaction) -> Result<(), Error> {
        if self.mode != Mode::Insert {
            return Err(Error::WrongMode {
                expected: Mode::Insert,
                actual: self.mode,
            });
        }
        self.apply(transaction, false)?;
        self.selections = self.normalized(self.selections.clone(), Mode::Insert)?;
        self.preferred_columns = None;
        Ok(())
    }
}

fn check(cancelled: &impl Fn() -> bool) -> Result<(), Error> {
    if cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

fn overflow() -> Error {
    vex_core::Error::LengthOverflow.into()
}

fn string_with_capacity(capacity: usize) -> Result<String, Error> {
    let mut text = String::new();
    text.try_reserve_exact(capacity).map_err(|_| overflow())?;
    Ok(text)
}

/// Normalize only line endings; share an unchanged, uncounted fragment directly.
fn prepare(
    value: &Arc<str>,
    newline: &str,
    count: usize,
    cancelled: &impl Fn() -> bool,
) -> Result<Arc<str>, Error> {
    let bytes = value.as_bytes();
    let convert = match newline {
        "\n" => value.contains('\r'),
        "\r" => value.contains('\n'),
        "\r\n" => bytes.iter().enumerate().any(|(i, &byte)| match byte {
            b'\r' => bytes.get(i + 1) != Some(&b'\n'),
            b'\n' => i == 0 || bytes[i - 1] != b'\r',
            _ => false,
        }),
        _ => unreachable!("supported document line ending"),
    };
    let text = if convert {
        let mut normalized = string_with_capacity(
            value
                .len()
                .checked_mul(newline.len())
                .ok_or_else(overflow)?,
        )?;
        let mut chars = value.chars().enumerate().peekable();
        while let Some((index, ch)) = chars.next() {
            if index % 1024 == 0 {
                check(cancelled)?;
            }
            match ch {
                '\r' => {
                    if chars.peek().is_some_and(|(_, ch)| *ch == '\n') {
                        chars.next();
                    }
                    normalized.push_str(newline);
                }
                '\n' => normalized.push_str(newline),
                _ => normalized.push(ch),
            }
        }
        Cow::Owned(normalized)
    } else {
        Cow::Borrowed(value.as_ref())
    };
    if count == 1 || text.is_empty() {
        return Ok(match text {
            Cow::Borrowed(_) => value.clone(),
            Cow::Owned(text) => Arc::from(text),
        });
    }
    let mut repeated = string_with_capacity(text.len().checked_mul(count).ok_or_else(overflow)?)?;
    for index in 0..count {
        if index % 1024 == 0 {
            check(cancelled)?;
        }
        repeated.push_str(&text);
    }
    Ok(repeated.into())
}

// Several selections on one line can target the same insertion boundary.
// Coalesce those edits, but retain a separate resulting range for each fragment.
struct Group {
    range: Range<CharOffset>,
    parts: Vec<Arc<str>>,
    chars: usize,
}

impl Group {
    fn text(&self) -> Result<Arc<str>, Error> {
        if self.parts.len() == 1 {
            return Ok(self.parts[0].clone());
        }
        let capacity = self.parts.iter().try_fold(0usize, |len, part| {
            len.checked_add(part.len()).ok_or_else(overflow)
        })?;
        let mut joined = string_with_capacity(capacity)?;
        for part in &self.parts {
            joined.push_str(part);
        }
        Ok(joined.into())
    }
}

pub(crate) fn paste(ctx: &mut CommandContext<'_>, action: Paste) -> Result<(), Error> {
    let name = ctx.register.unwrap_or('"');
    let editor = &mut *ctx.editor;
    let values = editor.register(name)?;
    if name == '_' {
        return Ok(());
    }
    if values.is_empty() {
        return Err(if name == '"' {
            Error::EmptyYankRegister
        } else {
            Error::EmptyRegister(name)
        });
    }
    let transaction = editor
        .paste_plan()
        .prepare(&values, action, ctx.count, &|| false)?;
    if action == Paste::Cursor {
        editor.apply_register_insert(transaction)
    } else {
        editor.apply_paste(transaction)
    }
}

impl PastePlan {
    /// Prepare edits without touching a live editor. Values pair with selections
    /// in document order; extra destinations repeat the last value.
    pub fn prepare(
        &self,
        values: &[Arc<str>],
        action: Paste,
        count: std::num::NonZeroUsize,
        cancelled: &impl Fn() -> bool,
    ) -> Result<Transaction, Error> {
        check(cancelled)?;
        if values.is_empty() {
            return Err(Error::EmptyYankRegister);
        }
        let linewise = matches!(action, Paste::Before | Paste::After)
            && values.iter().any(|value| value.ends_with(['\r', '\n']));
        let values = values
            .iter()
            .take(self.selections.ranges().len())
            .map(|value| {
                check(cancelled)?;
                let text = prepare(value, self.newline, count.get(), cancelled)?;
                let chars = text.chars().count();
                Ok((text, chars))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let text = self.snapshot.text();
        let mut groups: Vec<Group> = Vec::new();
        let mut spans = Vec::with_capacity(self.selections.ranges().len());
        for (index, &selection) in self.selections.ranges().iter().enumerate() {
            check(cancelled)?;
            let (value, chars) = &values[index.min(values.len() - 1)];
            let range = match action {
                Paste::Replace => selection.range(),
                Paste::Cursor => selection.head..selection.head,
                Paste::Before | Paste::After => {
                    let at = if linewise {
                        if action == Paste::Before {
                            motion::line_start(text, selection.start())?
                        } else {
                            let edge = if selection.is_empty() {
                                selection.end()
                            } else {
                                grapheme::previous(text, selection.end(), 1)?
                            };
                            CharOffset(text.line_to_char(
                                (text.char_to_line(edge.0) + 1).min(text.len_lines()),
                            ))
                        }
                    } else if action == Paste::Before {
                        selection.start()
                    } else {
                        selection.end()
                    };
                    at..at
                }
            };
            if !groups
                .last()
                .is_some_and(|group| range.is_empty() && group.range == range)
            {
                let mut group = Group {
                    range,
                    parts: Vec::new(),
                    chars: 0,
                };
                // A linewise append to an unterminated final line needs a separator.
                if linewise
                    && action == Paste::After
                    && group.range.start.0 == text.len_chars()
                    && text.len_chars() > 0
                    && !matches!(text.char(text.len_chars() - 1), '\r' | '\n')
                {
                    group.parts.push(Arc::from(self.newline));
                    group.chars = self.newline.len();
                }
                groups.push(group);
            }
            let group_index = groups.len() - 1;
            let group = &mut groups[group_index];
            spans.push((group_index, group.chars, *chars, selection.is_backward()));
            group.chars = group.chars.checked_add(*chars).ok_or_else(overflow)?;
            group.parts.push(value.clone());
        }
        let edits = groups
            .iter()
            .map(|group| {
                check(cancelled)?;
                Ok(Edit::new(group.range.clone(), group.text()?))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let transaction = self.snapshot.transaction(edits)?;
        let selections = spans
            .into_iter()
            .map(|(group, offset, len, backward)| {
                check(cancelled)?;
                let at = transaction
                    .map_position(groups[group].range.start, Affinity::Before)?
                    .0;
                let start = CharOffset(at.checked_add(offset).ok_or_else(overflow)?);
                let end = CharOffset(start.0.checked_add(len).ok_or_else(overflow)?);
                Ok(if backward {
                    Selection::new(end, start)
                } else {
                    Selection::new(start, end)
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let selections = SelectionSet::new(selections, self.selections.primary_index())?;
        let transaction = transaction.with_selections(selections)?;
        check(cancelled)?;
        Ok(transaction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use vex_core::Document;

    fn keys(editor: &mut Editor, text: &str) {
        let mut handler = crate::KeyHandler::default();
        for ch in text.chars() {
            handler.handle(editor, crate::Key::Char(ch)).unwrap();
        }
    }

    #[test]
    fn named_yanks_are_case_sensitive_and_shared_with_fragment_boundaries() {
        let mut source = Editor::new(Document::from("cat dog"));
        select(&mut source, vec![range(0, 3), range(4, 7)], 1);
        keys(&mut source, "\"ay");
        select(&mut source, vec![range(4, 7)], 0);
        keys(&mut source, "\"Ay");
        assert_eq!(
            source.register('a').unwrap().as_ref(),
            &[Arc::from("cat"), Arc::from("dog")]
        );
        assert_eq!(source.register('A').unwrap().as_ref(), &[Arc::from("dog")]);
        assert!(source.register('"').unwrap().is_empty());
        let mut target = Editor::with_session(Document::from("1 2 3"), source.session());
        select(&mut target, vec![range(0, 1), range(2, 3), range(4, 5)], 1);
        keys(&mut target, "\"aR");
        assert_eq!(target.document().text(), "cat dog dog");
        assert_eq!(target.selections().primary_index(), 1);
        target.execute("undo", 1).unwrap();
        assert_eq!(target.document().text(), "1 2 3");
        assert_eq!(target.register('a').unwrap(), source.register('a').unwrap());
    }

    #[test]
    fn reads_share_fragments_and_previews_are_bounded_without_flattening_values() {
        let mut editor = Editor::new(Document::from("first second"));
        select(&mut editor, vec![range(0, 5), range(6, 12)], 1);
        let values: RegisterValues =
            Arc::from([Arc::from("界".repeat(1 << 20)), Arc::from("tail")]);
        for name in (0x100..0x200).filter_map(char::from_u32) {
            editor.set_register(name, values.clone()).unwrap();
        }
        let read = editor.register('Ā').unwrap();
        assert!(Arc::ptr_eq(&read, &values));
        assert!(Arc::ptr_eq(
            &editor.register_first('Ā').unwrap().unwrap(),
            &values[0]
        ));
        let previews = editor.register_previews();
        assert_eq!(previews.len(), 64);
        assert!(previews.iter().all(|(_, text)| text == &"界".repeat(48)));
        assert_eq!(
            editor.register_first('.').unwrap().as_deref(),
            Some("first")
        );
        assert_eq!(editor.register_first('#').unwrap().as_deref(), Some("1"));
        assert!(editor.register_first('_').unwrap().is_none());
    }

    #[test]
    fn register_selection_is_used_by_one_command_and_keeps_counts() {
        for sequence in ["\"a3p", "3\"ap"] {
            let mut editor = Editor::new(Document::from("ab"));
            editor
                .set_register('a', Arc::from([Arc::from("X")]))
                .unwrap();
            editor
                .set_register('"', Arc::from([Arc::from("Y")]))
                .unwrap();
            keys(&mut editor, sequence);
            assert_eq!(editor.document().text(), "aXXXb");
            keys(&mut editor, "p");
            assert_eq!(editor.document().text(), "aXXXYb");
        }
        let mut editor = Editor::new(Document::from("ab"));
        editor
            .set_register('a', Arc::from([Arc::from("X")]))
            .unwrap();
        editor
            .set_register('"', Arc::from([Arc::from("Y")]))
            .unwrap();
        keys(&mut editor, "\"alp");
        assert_eq!(editor.document().text(), "abY");
    }

    #[test]
    fn literal_register_names_and_escape_keep_pending_input_separate_from_editing() {
        for name in ['3', ':', ' ', '界'] {
            let mut editor = Editor::new(Document::from("x"));
            editor
                .set_register(name, Arc::from([Arc::from("ok")]))
                .unwrap();
            keys(&mut editor, &format!("\"{name}P"));
            assert_eq!(editor.document().text(), "okx");
        }
        for cancel in [crate::Key::Escape, crate::Key::Ctrl('c')] {
            let mut editor = Editor::new(Document::from("abc"));
            editor.execute("select_mode", 1).unwrap();
            let mut handler = crate::KeyHandler::default();
            for ch in "\"a".chars() {
                handler.handle(&mut editor, crate::Key::Char(ch)).unwrap();
            }
            assert_eq!(editor.selected_register(), Some('a'));
            handler.handle(&mut editor, cancel).unwrap();
            assert_eq!(editor.selected_register(), None);
            assert_eq!(editor.mode(), Mode::Select);
            assert_eq!(editor.document().text(), "abc");
        }
    }

    #[test]
    fn named_cuts_and_changes_preserve_the_default_register_and_undo_groups() {
        let mut editor = Editor::new(Document::from("abc"));
        editor
            .set_register('"', Arc::from([Arc::from("old")]))
            .unwrap();
        keys(&mut editor, "\"ad");
        assert_eq!(editor.document().text(), "bc");
        assert_eq!(editor.register('a').unwrap()[0].as_ref(), "a");
        keys(&mut editor, "\"bc");
        editor.insert_text("X").unwrap();
        editor.execute("normal_mode", 1).unwrap();
        assert_eq!(editor.document().text(), "Xc");
        assert_eq!(editor.register('b').unwrap()[0].as_ref(), "b");
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document().text(), "bc");
        assert_eq!(editor.register('"').unwrap()[0].as_ref(), "old");
    }

    #[test]
    fn black_hole_discards_cuts_and_read_only_registers_fail_before_editing() {
        let mut editor = Editor::new(Document::from("abc"));
        editor
            .set_register('"', Arc::from([Arc::from("old")]))
            .unwrap();
        keys(&mut editor, "\"_d");
        assert_eq!(editor.document().text(), "bc");
        keys(&mut editor, "\"_p");
        assert_eq!(editor.document().text(), "bc");
        assert_eq!(editor.register('"').unwrap()[0].as_ref(), "old");
        for name in ['#', '.', '%'] {
            let mut ctx = crate::CommandContext::new(&mut editor);
            ctx.register = Some(name);
            assert_eq!(
                crate::commands::delete_selection(&mut ctx),
                Err(Error::ReadOnlyRegister(name))
            );
            assert_eq!(editor.document().text(), "bc");
        }
        select(&mut editor, vec![range(0, 1), range(1, 2)], 1);
        assert_eq!(
            editor.register('#').unwrap().as_ref(),
            &[Arc::from("1"), Arc::from("2")]
        );
        assert_eq!(
            editor.register('.').unwrap().as_ref(),
            &[Arc::from("b"), Arc::from("c")]
        );
    }

    #[test]
    fn insert_register_stays_at_carets_and_replays_the_named_command() {
        let mut editor = Editor::new(Document::from("ab\r\n"));
        editor
            .set_register('a', Arc::from([Arc::from("界\nX")]))
            .unwrap();
        editor.execute("append_mode", 1).unwrap();
        let mut handler = crate::KeyHandler::default();
        handler.handle(&mut editor, crate::Key::Ctrl('r')).unwrap();
        assert_eq!(handler.hints().unwrap().title, "Insert register");
        handler.handle(&mut editor, crate::Key::Char('a')).unwrap();
        assert_eq!(editor.mode(), Mode::Insert);
        assert_eq!(editor.document().text(), "a界\r\nXb\r\n");
        editor.execute("normal_mode", 1).unwrap();
        editor
            .set_register('a', Arc::from([Arc::from("Z")]))
            .unwrap();
        editor.execute("repeat_insert", 1).unwrap();
        assert_eq!(editor.document().text(), "a界\r\nXZb\r\n");
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document().text(), "a界\r\nXb\r\n");
    }

    #[test]
    fn insert_register_pairs_fragments_and_creates_an_explicit_undo_step() {
        let mut editor = Editor::new(Document::from("a b"));
        select(&mut editor, vec![range(0, 1), range(2, 3)], 1);
        editor
            .set_register('a', Arc::from([Arc::from("e\u{301}"), Arc::from("界")]))
            .unwrap();
        editor.execute("append_mode", 1).unwrap();
        editor.insert_text("x").unwrap();
        let mut ctx = crate::CommandContext::new(&mut editor);
        ctx.character = Some('a');
        ctx.count = std::num::NonZeroUsize::new(2).unwrap();
        crate::commands::insert_register(&mut ctx).unwrap();
        assert_eq!(editor.document().text(), "axe\u{301}e\u{301} bx界界");
        editor.execute("normal_mode", 1).unwrap();
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document().text(), "ax bx");
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document().text(), "a b");
    }

    #[test]
    fn prepared_pastes_reject_stale_revisions_and_other_documents() {
        let mut source = Editor::new(Document::from("abc"));
        let transaction = source
            .paste_plan()
            .prepare(
                &[Arc::from("X")],
                Paste::After,
                std::num::NonZeroUsize::MIN,
                &|| false,
            )
            .unwrap();
        let mut other = Editor::new(Document::from("abc"));
        assert!(other.apply_paste(transaction.clone()).is_err());
        source.execute("insert_mode", 1).unwrap();
        source.insert_text("z").unwrap();
        source.execute("normal_mode", 1).unwrap();
        assert!(source.apply_paste(transaction).is_err());
        assert_eq!(source.document().text(), "zabc");
        assert_eq!(other.document().text(), "abc");
    }

    #[test]
    fn counted_paste_preparation_observes_cancellation_before_finishing() {
        let source = Editor::new(Document::from("abc"));
        let calls = std::cell::Cell::new(0);
        let result = source.paste_plan().prepare(
            &[Arc::from("x")],
            Paste::After,
            std::num::NonZeroUsize::new(1_000_000).unwrap(),
            &|| {
                calls.set(calls.get() + 1);
                calls.get() > 6
            },
        );
        assert!(matches!(result, Err(Error::Cancelled)));
        assert_eq!(source.document().text(), "abc");
    }

    fn range(anchor: usize, head: usize) -> Selection {
        Selection::new(CharOffset(anchor), CharOffset(head))
    }

    fn select(editor: &mut Editor, ranges: Vec<Selection>, primary: usize) {
        editor
            .set_selections(SelectionSet::new(ranges, primary).unwrap())
            .unwrap();
    }

    fn yanked(text: &str) -> Editor {
        let mut editor = Editor::new(Document::from(text));
        let len = editor.document.text().len_chars();
        select(&mut editor, vec![range(0, len)], 0);
        editor.execute("yank", 1).unwrap();
        editor
    }

    fn destination(source: &Editor, text: &str) -> Editor {
        Editor::with_yank_register(Document::from(text), source.yank_register())
    }

    #[test]
    fn yank_preserves_text_selections_and_redo_then_exits_select_mode() {
        let mut editor = Editor::new(Document::from("abc"));
        editor.execute("insert_mode", 1).unwrap();
        editor.insert_text("X").unwrap();
        editor.execute("normal_mode", 1).unwrap();
        editor.execute("undo", 1).unwrap();
        select(&mut editor, vec![range(2, 0)], 0);
        editor.execute("select_mode", 1).unwrap();
        let revision = editor.document.revision();
        editor.execute("yank", 1).unwrap();
        assert_eq!(editor.document.text(), "abc");
        assert_eq!(editor.document.revision(), revision);
        assert_eq!(editor.selections.primary(), range(2, 0));
        assert_eq!(editor.mode(), Mode::Normal);
        assert_eq!(editor.document.undo_depth(), 0);
        assert_eq!(editor.document.redo_depth(), 1);
        editor.execute("redo", 1).unwrap();
        let mut other = destination(&editor, "");
        other.execute("paste_before", 1).unwrap();
        assert_eq!(other.document.text(), "ab");
    }

    #[test]
    fn character_paste_and_replace_preserve_direction_and_select_new_text() {
        let source = yanked("界e\u{301}");
        for (command, expected, selection) in [
            ("paste_after", "abc界e\u{301}d", range(6, 3)),
            ("paste_before", "a界e\u{301}bcd", range(4, 1)),
            ("replace_with_yanked", "a界e\u{301}d", range(4, 1)),
        ] {
            let mut editor = destination(&source, "abcd");
            select(&mut editor, vec![range(3, 1)], 0);
            editor.execute("select_mode", 1).unwrap();
            editor.execute(command, 1).unwrap();
            assert_eq!(editor.document.text(), expected);
            assert_eq!(editor.selections.primary(), selection);
            assert_eq!(editor.mode(), Mode::Normal);
            assert_eq!(editor.document.undo_depth(), 1);
            editor.execute("undo", 1).unwrap();
            assert_eq!(editor.document.text(), "abcd");
            assert_eq!(editor.selections.primary(), range(3, 1));
            editor.execute("redo", 1).unwrap();
            assert_eq!(editor.document.text(), expected);
            assert_eq!(editor.selections.primary(), selection);
        }
    }

    #[test]
    fn counts_repeat_text_in_one_step_without_changing_the_register() {
        let source = yanked("🦀");
        for command in ["paste_after", "paste_before", "replace_with_yanked"] {
            let mut editor = destination(&source, "ab");
            editor.execute(command, 3).unwrap();
            let expected = match command {
                "paste_after" => "a🦀🦀🦀b",
                "paste_before" => "🦀🦀🦀ab",
                _ => "🦀🦀🦀b",
            };
            assert_eq!(editor.document.text(), expected);
            assert_eq!(editor.document.undo_depth(), 1);
            editor.execute("undo", 1).unwrap();
            assert_eq!(editor.document.text(), "ab");
            assert_eq!(
                source.yank_register.read().as_ref(),
                [Arc::<str>::from("🦀")]
            );
        }
    }

    #[test]
    fn cut_and_change_capture_original_text_and_undo_does_not_rewind_the_register() {
        let mut editor = Editor::new(Document::from("one two"));
        select(&mut editor, vec![range(0, 3)], 0);
        editor.execute("delete_selection", 1).unwrap();
        assert_eq!(editor.document.text(), " two");
        editor.execute("undo", 1).unwrap();
        let mut other = destination(&editor, "");
        other.execute("paste_after", 1).unwrap();
        assert_eq!(other.document.text(), "one");

        select(&mut editor, vec![range(7, 4)], 0);
        editor.execute("change_selection", 1).unwrap();
        editor.insert_text("three").unwrap();
        editor.execute("normal_mode", 1).unwrap();
        assert_eq!(editor.document.text(), "one three");
        assert_eq!(editor.document.undo_depth(), 1);
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document.text(), "one two");
        assert_eq!(editor.selections.primary(), range(7, 4));
        let mut other = destination(&editor, "");
        other.execute("paste_before", 1).unwrap();
        assert_eq!(other.document.text(), "two");
    }

    #[test]
    fn insert_deletions_and_internal_cleanup_preserve_yanked_text() {
        let source = yanked("saved");
        let mut editor = destination(&source, "abc");
        editor.execute("append_mode", 1).unwrap();
        editor.execute("delete_backward", 1).unwrap();
        editor.execute("delete_forward", 1).unwrap();
        editor.execute("normal_mode", 1).unwrap();
        editor.execute("delete_selection_without_yank", 1).unwrap();
        editor.execute("paste_before", 1).unwrap();
        assert_eq!(editor.document.text(), "saved");
    }

    #[test]
    fn fragments_pair_in_document_order_and_repeat_the_last_for_extra_destinations() {
        let mut source = Editor::new(Document::from("A BB CCC"));
        select(&mut source, vec![range(0, 1), range(4, 2), range(5, 8)], 1);
        source.execute("yank", 1).unwrap();
        let mut editor = destination(&source, "1 2 3 4");
        select(
            &mut editor,
            vec![range(0, 1), range(3, 2), range(4, 5), range(6, 7)],
            1,
        );
        let before = editor.selections.clone();
        editor.execute("replace_with_yanked", 1).unwrap();
        assert_eq!(editor.document.text(), "A BB CCC CCC");
        assert_eq!(
            editor.selections.ranges(),
            &[range(0, 1), range(4, 2), range(5, 8), range(9, 12)]
        );
        assert_eq!(editor.selections.primary_index(), 1);
        editor.execute("undo", 1).unwrap();
        assert_eq!(editor.document.text(), "1 2 3 4");
        assert_eq!(editor.selections, before);
        let mut single = destination(&source, "x");
        single.execute("replace_with_yanked", 1).unwrap();
        assert_eq!(single.document.text(), "A");
    }

    #[test]
    fn one_fragment_broadcasts_to_adjacent_selections_without_swallowing_neighbors() {
        let source = yanked("YZ");
        for (command, expected, ranges) in [
            ("paste_before", "YZaYZb", vec![range(0, 2), range(3, 5)]),
            ("paste_after", "aYZbYZ", vec![range(1, 3), range(4, 6)]),
            (
                "replace_with_yanked",
                "YZYZ",
                vec![range(0, 2), range(2, 4)],
            ),
        ] {
            let mut editor = destination(&source, "ab");
            select(&mut editor, vec![range(0, 1), range(1, 2)], 1);
            editor.execute(command, 1).unwrap();
            assert_eq!(editor.document.text(), expected);
            assert_eq!(editor.selections.ranges(), ranges);
            assert_eq!(editor.selections.primary_index(), 1);
        }
    }

    #[test]
    fn linewise_paste_uses_outer_selected_lines_and_replace_uses_exact_ranges() {
        let source = yanked("one\n");
        for (command, expected) in [
            ("paste_after", "aa\nbb\none\ncc"),
            ("paste_before", "one\naa\nbb\ncc"),
            ("replace_with_yanked", "one\ncc"),
        ] {
            let mut editor = destination(&source, "aa\nbb\ncc");
            select(&mut editor, vec![range(6, 0)], 0);
            editor.execute(command, 1).unwrap();
            assert_eq!(editor.document.text(), expected);
        }
        let mut editor = destination(&source, "abc");
        select(&mut editor, vec![range(1, 2)], 0);
        editor.execute("replace_with_yanked", 1).unwrap();
        assert_eq!(editor.document.text(), "aone\nc");
    }

    #[test]
    fn linewise_paste_handles_empty_buffers_eof_and_unterminated_final_lines() {
        let source = yanked("row\n");
        for (text, cursor, command, expected, selection) in [
            ("", 0, "paste_after", "row\n", range(0, 4)),
            ("", 0, "paste_before", "row\n", range(0, 4)),
            ("end", 1, "paste_after", "end\nrow\n", range(4, 8)),
            ("end", 3, "paste_after", "end\nrow\n", range(4, 8)),
            ("end", 3, "paste_before", "row\nend", range(0, 4)),
            ("end\n", 4, "paste_after", "end\nrow\n", range(4, 8)),
        ] {
            let mut editor = destination(&source, text);
            select(&mut editor, vec![range(cursor, cursor)], 0);
            editor.execute(command, 1).unwrap();
            assert_eq!(editor.document.text(), expected);
            assert_eq!(editor.selections.primary(), selection);
        }
    }

    #[test]
    fn pasted_line_endings_follow_the_destination_but_the_register_retains_original_bytes() {
        for from in ["\n", "\r\n", "\r"] {
            let source = yanked(&format!("界{from}"));
            for to in ["\n", "\r\n", "\r"] {
                let mut editor = destination(&source, &format!("a{to}b{to}"));
                editor.execute("paste_after", 2).unwrap();
                assert_eq!(
                    editor.document.text().to_string(),
                    format!("a{to}界{to}界{to}b{to}")
                );
                assert_eq!(
                    editor.selections.primary(),
                    range(1 + to.len(), 3 + 3 * to.len())
                );
            }
            assert_eq!(source.yank_register.read()[0].as_ref(), format!("界{from}"));
        }
        let source = yanked("a\r\nb\rc\nd");
        let mut editor = destination(&source, "xy");
        editor.execute("paste_after", 1).unwrap();
        assert_eq!(editor.document.text(), "xa\nb\nc\ndy");
    }

    #[test]
    fn linewise_collisions_keep_each_fragment_direction_and_primary() {
        let mut source = Editor::new(Document::from("A\nB\n"));
        select(&mut source, vec![range(0, 2), range(2, 4)], 0);
        source.execute("yank", 1).unwrap();
        for (text, command, expected, ranges) in [
            (
                "xy\nz\n",
                "paste_after",
                "xy\nA\nB\nz\n",
                vec![range(3, 5), range(7, 5)],
            ),
            (
                "xy\nz\n",
                "paste_before",
                "A\nB\nxy\nz\n",
                vec![range(0, 2), range(4, 2)],
            ),
            (
                "xy",
                "paste_after",
                "xy\nA\nB\n",
                vec![range(3, 5), range(7, 5)],
            ),
        ] {
            let mut editor = destination(&source, text);
            select(&mut editor, vec![range(0, 1), range(2, 1)], 1);
            editor.execute(command, 1).unwrap();
            assert_eq!(editor.document.text(), expected);
            assert_eq!(editor.selections.ranges(), ranges);
            assert_eq!(editor.selections.primary_index(), 1);
            editor.execute("undo", 1).unwrap();
            assert_eq!(editor.document.text(), text);
        }
    }

    #[test]
    fn empty_and_unrepresentable_pastes_fail_without_mutating_editor_state() {
        let mut empty = Editor::new(Document::from("abc"));
        assert_eq!(
            empty.execute("paste_after", 1),
            Err(Error::EmptyYankRegister)
        );
        assert_eq!(empty.document.undo_depth(), 0);
        let source = yanked("xy");
        let mut editor = destination(&source, "abc");
        editor.execute("select_mode", 1).unwrap();
        let revision = editor.document.revision();
        let selections = editor.selections.clone();
        assert_eq!(editor.execute("paste_after", usize::MAX), Err(overflow()));
        assert_eq!(editor.document.text(), "abc");
        assert_eq!(editor.document.revision(), revision);
        assert_eq!(editor.selections, selections);
        assert_eq!(editor.mode(), Mode::Select);
        assert_eq!(editor.document.undo_depth(), 0);
        assert_eq!(source.yank_register.read()[0].as_ref(), "xy");
    }

    #[test]
    fn empty_fragments_are_distinct_from_an_unused_register() {
        let source = yanked("");
        let mut editor = destination(&source, "ab");
        editor.execute("paste_after", usize::MAX).unwrap();
        assert_eq!(editor.document.text(), "ab");
        assert_eq!(editor.document.undo_depth(), 0);
        select(&mut editor, vec![range(0, 1), range(1, 2)], 1);
        editor.execute("replace_with_yanked", 1).unwrap();
        assert_eq!(editor.document.text(), "");
        assert_eq!(editor.selections.primary(), range(0, 0));
    }

    proptest! {
        #[test]
        fn adjacent_pastes_preserve_graphemes_and_round_trip_undo(
            saved in "[ab界🦀\r\n\u{301}]{1,12}",
            count in 1usize..4,
            command in prop::sample::select(vec!["paste_after", "paste_before", "replace_with_yanked"]),
            primary in 0usize..4,
            backward in any::<bool>(),
        ) {
            let source = yanked(&saved);
            let mut editor = destination(&source, "abcd");
            let ranges = (0..4).map(|i| if backward { range(i + 1, i) } else { range(i, i + 1) }).collect();
            select(&mut editor, ranges, primary);
            let before = editor.selections.clone();
            editor.execute(command, count).unwrap();
            let result = editor.document.text().to_string();
            let after = editor.selections.clone();
            for selection in after.ranges() {
                prop_assert_eq!(grapheme::floor(editor.document.text(), selection.start()).unwrap(), selection.start());
                prop_assert_eq!(grapheme::ceil(editor.document.text(), selection.end()).unwrap(), selection.end());
            }
            editor.execute("undo", 1).unwrap();
            prop_assert_eq!(editor.document.text().to_string(), "abcd");
            prop_assert_eq!(&editor.selections, &before);
            editor.execute("redo", 1).unwrap();
            prop_assert_eq!(editor.document.text().to_string(), result);
            prop_assert_eq!(editor.selections, after);
        }
    }
}
