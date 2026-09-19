use std::{
    io::{self, Read, Write},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::history::{History, State};
use crate::{Affinity, ByteOffset, CharOffset, Edit, Error, Rope, SelectionSet, Transaction};

/// Process-local identity, distinct even for documents containing identical text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DocumentId(u64);

impl DocumentId {
    fn next() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self(
            NEXT_ID
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                .expect("document identity counter exhausted"),
        )
    }
}

/// Monotonically increases on edits, undo, and redo; never rewinds with history.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Revision(u64);

impl Revision {
    pub fn get(self) -> u64 {
        self.0
    }

    fn next(self) -> Result<Self, Error> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(Error::RevisionExhausted)
    }
}

/// Immutable text and identity for work that can outlive an input event.
/// Cloning shares rope storage. Transactions prepared here are rejected if the
/// document has changed by the time they are applied.
#[derive(Clone, Debug)]
pub struct Snapshot {
    id: DocumentId,
    revision: Revision,
    text: Rope,
}

impl Snapshot {
    pub fn id(&self) -> DocumentId {
        self.id
    }

    pub fn revision(&self) -> Revision {
        self.revision
    }

    pub fn text(&self) -> &Rope {
        &self.text
    }

    pub fn transaction(&self, edits: impl IntoIterator<Item = Edit>) -> Result<Transaction, Error> {
        Transaction::new(self.id, self.revision, self.text.len_chars(), edits)
    }
}

/// Conservative extent of a revision's changes. Text before `start` and after
/// the corresponding end is unchanged. The first edit's old/new start is equal.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ChangeExtent {
    pub start: CharOffset,
    pub old_end: CharOffset,
    pub new_end: CharOffset,
}

impl ChangeExtent {
    pub fn reversed(self) -> Self {
        Self {
            start: self.start,
            old_end: self.new_end,
            new_end: self.old_end,
        }
    }
}

/// Text, revision, and bounded undo history, with no terminal or filesystem paths.
///
/// Selections belong to the caller (eventually a view). Applying a transaction
/// records that caller's selections in history; a future editor with several
/// views must map the other views' selections as well.
#[derive(Debug)]
pub struct Document {
    id: DocumentId,
    revision: Revision,
    text: Rope,
    history: History,
    pub(crate) change: ChangeExtent,
}

impl Document {
    pub fn id(&self) -> DocumentId {
        self.id
    }

    pub fn revision(&self) -> Revision {
        self.revision
    }

    pub fn text(&self) -> &Rope {
        &self.text
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            id: self.id,
            revision: self.revision,
            text: self.text.clone(),
        }
    }

    /// Read UTF-8 without first collecting the entire file into a String.
    pub fn from_reader(reader: impl Read) -> io::Result<Self> {
        Rope::from_reader(reader).map(Self::from)
    }

    /// Stream text to a writer. The caller owns flushing and safe file replacement.
    pub fn write_to(&self, writer: impl Write) -> io::Result<()> {
        self.text.write_to(writer)
    }

    pub fn char_to_byte(&self, position: CharOffset) -> Result<ByteOffset, Error> {
        self.text
            .try_char_to_byte(position.0)
            .map(ByteOffset)
            .map_err(|_| Error::PositionOutOfBounds {
                position,
                len: self.text.len_chars(),
            })
    }

    /// Reject offsets inside a multi-byte scalar rather than rounding them down.
    pub fn byte_to_char(&self, position: ByteOffset) -> Result<CharOffset, Error> {
        if let Ok(index) = self.text.try_byte_to_char(position.0)
            && self.text.char_to_byte(index) == position.0
        {
            return Ok(CharOffset(index));
        }
        Err(Error::InvalidByteOffset {
            position,
            len: self.text.len_bytes(),
        })
    }

    pub fn transaction(&self, edits: impl IntoIterator<Item = Edit>) -> Result<Transaction, Error> {
        Transaction::new(self.id, self.revision, self.text.len_chars(), edits)
    }

    /// Build one atomic replacement per normalized selection, sharing inserted
    /// text between edits. Each selection becomes a caret after its replacement.
    pub fn replace_selections(
        &self,
        selections: &SelectionSet,
        text: impl Into<Arc<str>>,
    ) -> Result<Transaction, Error> {
        selections.validate(self.text.len_chars())?;
        let text = text.into();
        let transaction = self.transaction(
            selections
                .ranges()
                .iter()
                .map(|range| Edit::new(range.range(), Arc::clone(&text))),
        )?;
        // Explicit carets matter for adjacent selections: a generic endpoint at
        // their shared boundary would follow the next replacement as well.
        let carets = selections
            .ranges()
            .iter()
            .map(|range| {
                let head = transaction.map_position(range.start(), Affinity::After)?;
                Ok(crate::Selection::cursor(head))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let after = SelectionSet::new(carets, selections.primary_index())?;
        transaction.with_selections(after)
    }

    /// Apply all edits as one undo step. Validation completes before mutation.
    ///
    /// By default both selection endpoints follow inserted text. An explicit
    /// `Transaction::with_selections` overrides this. Returns whether edits were
    /// present; an empty transaction may still set explicit selections but does
    /// not advance the revision, add history, or discard redo.
    pub fn apply(
        &mut self,
        transaction: Transaction,
        selections: &mut SelectionSet,
    ) -> Result<bool, Error> {
        if transaction.document_id != self.id {
            return Err(Error::WrongDocument);
        }
        if transaction.revision != self.revision {
            return Err(Error::StaleRevision {
                expected: transaction.revision,
                actual: self.revision,
            });
        }
        selections.validate(self.text.len_chars())?;
        let after_selections = match &transaction.selections {
            Some(after) => after.clone(),
            None => transaction.map_selections(selections, Affinity::After)?,
        };
        if transaction.is_empty() {
            *selections = after_selections;
            return Ok(false);
        }
        let revision = self.revision.next()?;
        let before = State {
            text: self.text.clone(),
            selections: selections.clone(),
        };
        let mut text = self.text.clone();
        transaction.apply_to(&mut text);
        let after = State {
            text: text.clone(),
            selections: after_selections.clone(),
        };
        let change_start = transaction
            .edits()
            .next()
            .expect("nonempty transaction")
            .range()
            .start;
        let old_end = transaction
            .edits()
            .last()
            .expect("nonempty transaction")
            .range()
            .end;
        let change = ChangeExtent {
            start: change_start,
            old_end,
            new_end: CharOffset(text.len_chars() - (self.text.len_chars() - old_end.0)),
        };
        self.history.record(before, after, change);
        self.change = change;
        self.text = text;
        self.revision = revision;
        *selections = after_selections;
        Ok(true)
    }

    pub fn undo(&mut self, selections: &mut SelectionSet) -> Result<bool, Error> {
        if self.history.undo_depth() == 0 {
            return Ok(false);
        }
        let revision = self.revision.next()?;
        let (state, change) = self.history.undo().expect("checked undo history");
        self.change = change;
        self.text = state.text;
        *selections = state.selections;
        self.revision = revision;
        Ok(true)
    }

    pub fn redo(&mut self, selections: &mut SelectionSet) -> Result<bool, Error> {
        if self.history.redo_depth() == 0 {
            return Ok(false);
        }
        let revision = self.revision.next()?;
        let (state, change) = self.history.redo().expect("checked redo history");
        self.change = change;
        self.text = state.text;
        *selections = state.selections;
        self.revision = revision;
        Ok(true)
    }

    pub fn undo_depth(&self) -> usize {
        self.history.undo_depth()
    }

    pub fn redo_depth(&self) -> usize {
        self.history.redo_depth()
    }

    /// Default: 1,000 transactions. Zero disables retained history. Changing the
    /// limit discards redo and trims the oldest undo entries. This is an entry
    /// limit, not a byte budget; large edits can still retain substantial memory.
    pub fn set_history_limit(&mut self, limit: usize) {
        self.history.set_limit(limit);
    }
}

impl From<Rope> for Document {
    fn from(text: Rope) -> Self {
        Self {
            id: DocumentId::next(),
            revision: Revision::default(),
            text,
            history: History::default(),
            change: ChangeExtent::default(),
        }
    }
}

impl From<&str> for Document {
    fn from(text: &str) -> Self {
        Self::from(Rope::from_str(text))
    }
}

impl Default for Document {
    fn default() -> Self {
        Self::from(Rope::new())
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read, Write};

    use crate::{ByteOffset, CharOffset, Document, Edit, Error, Selection, SelectionSet};

    fn range(anchor: usize, head: usize) -> Selection {
        Selection::new(CharOffset(anchor), CharOffset(head))
    }

    fn replace(document: &mut Document, selections: &mut SelectionSet, text: &str) {
        let transaction = document.replace_selections(selections, text).unwrap();
        document.apply(transaction, selections).unwrap();
    }

    #[test]
    fn unicode_multi_selection_edit_is_one_undo_step() {
        let mut document = Document::from("café 日本語 🦀");
        let before = SelectionSet::new(vec![range(4, 0), range(5, 8)], 1).unwrap();
        let mut selections = before.clone();
        replace(&mut document, &mut selections, "λ");
        assert_eq!(document.text(), "λ λ 🦀");
        assert_eq!(selections.ranges(), &[range(1, 1), range(3, 3)]);
        assert_eq!(selections.primary_index(), 1);
        assert_eq!(document.undo_depth(), 1);
        assert_eq!(document.revision().get(), 1);
        let after = selections.clone();

        // Navigation between edits should not change the recorded undo selection.
        selections = SelectionSet::single(range(0, 0));
        assert!(document.undo(&mut selections).unwrap());
        assert_eq!(document.text(), "café 日本語 🦀");
        assert_eq!(selections, before);
        assert_eq!(document.revision().get(), 2);
        assert!(document.redo(&mut selections).unwrap());
        assert_eq!(document.text(), "λ λ 🦀");
        assert_eq!(selections, after);
        assert_eq!(document.revision().get(), 3);
    }

    #[test]
    fn adjacent_replacements_have_independent_carets() {
        let mut document = Document::from("abcd");
        let mut selections = SelectionSet::new(vec![range(0, 2), range(4, 2)], 1).unwrap();
        replace(&mut document, &mut selections, "xyz");
        assert_eq!(document.text(), "xyzxyz");
        assert_eq!(selections.ranges(), &[range(3, 3), range(6, 6)]);
        assert_eq!(selections.primary_index(), 1);
    }

    #[test]
    fn adjacent_deletions_collapse_and_keep_a_primary() {
        let mut document = Document::from("abcd");
        let before = SelectionSet::new(vec![range(0, 2), range(4, 2)], 1).unwrap();
        let mut selections = before.clone();
        replace(&mut document, &mut selections, "");
        assert_eq!(document.text(), "");
        assert_eq!(selections, SelectionSet::default());
        document.undo(&mut selections).unwrap();
        assert_eq!(selections, before);
    }

    #[test]
    fn unsorted_edits_use_original_coordinates() {
        let mut document = Document::from("abcdef");
        let mut selections = SelectionSet::single(range(5, 3));
        let transaction = document
            .transaction([
                Edit::insert(CharOffset(6), "!"),
                Edit::new(CharOffset(1)..CharOffset(3), "1234"),
                Edit::delete(CharOffset(4)..CharOffset(5)),
            ])
            .unwrap();
        document.apply(transaction, &mut selections).unwrap();
        assert_eq!(document.text(), "a1234df!");
        assert_eq!(selections.primary(), range(6, 5));
    }

    #[test]
    fn bad_current_or_result_selections_are_rejected() {
        let mut document = Document::from("abc");
        let mut invalid = SelectionSet::single(range(4, 4));
        let transaction = document
            .transaction([Edit::insert(CharOffset(0), "x")])
            .unwrap();
        assert!(document.apply(transaction, &mut invalid).is_err());
        assert_eq!(invalid.primary(), range(4, 4));
        assert_eq!(document.text(), "abc");
        assert_eq!(document.revision().get(), 0);
        assert_eq!(document.undo_depth(), 0);
        assert!(document.replace_selections(&invalid, "x").is_err());
        assert!(
            document
                .transaction([Edit::delete(CharOffset(0)..CharOffset(3))])
                .unwrap()
                .with_selections(SelectionSet::single(range(1, 1)))
                .is_err()
        );
    }

    #[test]
    fn explicit_result_selections_are_restored_by_redo() {
        let mut document = Document::from("abc");
        let mut selections = SelectionSet::default();
        let after = SelectionSet::single(range(4, 1));
        let transaction = document
            .transaction([Edit::insert(CharOffset(0), "!")])
            .unwrap()
            .with_selections(after.clone())
            .unwrap();
        document.apply(transaction, &mut selections).unwrap();
        assert_eq!(selections, after);
        document.undo(&mut selections).unwrap();
        assert_eq!(selections, SelectionSet::default());
        document.redo(&mut selections).unwrap();
        assert_eq!(selections, after);
    }

    #[test]
    fn empty_document_and_eof_are_editable() {
        let mut document = Document::default();
        let mut selections = SelectionSet::default();
        assert!(!document.undo(&mut selections).unwrap());
        assert!(!document.redo(&mut selections).unwrap());
        replace(&mut document, &mut selections, "🦀");
        replace(&mut document, &mut selections, "\r\n");
        assert_eq!(document.text(), "🦀\r\n");
        assert_eq!(selections.primary(), range(3, 3));
        document.undo(&mut selections).unwrap();
        document.undo(&mut selections).unwrap();
        assert_eq!(document.text(), "");
        assert_eq!(selections, SelectionSet::default());
        assert!(!document.undo(&mut selections).unwrap());
    }

    #[test]
    fn stale_and_wrong_document_edits_leave_all_state_unchanged() {
        let mut document = Document::from("abc");
        let other = Document::from("abc");
        let mut selections = SelectionSet::default();
        let wrong = other
            .transaction([Edit::insert(CharOffset(0), "bad")])
            .unwrap();
        assert_eq!(
            document.apply(wrong, &mut selections),
            Err(Error::WrongDocument)
        );
        let snapshot = document.snapshot();
        let stale = snapshot
            .transaction([Edit::insert(CharOffset(0), "stale")])
            .unwrap();
        replace(&mut document, &mut selections, "fresh");
        let revision = document.revision();
        assert!(matches!(
            document.apply(stale, &mut selections),
            Err(Error::StaleRevision { .. })
        ));
        assert_eq!(document.text(), "freshabc");
        assert_eq!(document.revision(), revision);
        assert_eq!(selections.primary(), range(5, 5));
        assert_eq!(document.undo_depth(), 1);
        assert_eq!(snapshot.text(), "abc");
        assert_eq!(snapshot.id(), document.id());
        assert_eq!(snapshot.revision().get(), 0);
    }

    #[test]
    fn undo_does_not_revalidate_old_transactions() {
        let mut document = Document::from("abc");
        let mut selections = SelectionSet::default();
        let old = document
            .transaction([Edit::insert(CharOffset(0), "old")])
            .unwrap();
        replace(&mut document, &mut selections, "new");
        document.undo(&mut selections).unwrap();
        assert_eq!(document.text(), "abc");
        assert!(matches!(
            document.apply(old, &mut selections),
            Err(Error::StaleRevision { .. })
        ));
        assert_eq!(document.redo_depth(), 1);
    }

    #[test]
    fn editing_after_undo_discards_redo() {
        let mut document = Document::default();
        let mut selections = SelectionSet::default();
        replace(&mut document, &mut selections, "a");
        replace(&mut document, &mut selections, "b");
        document.undo(&mut selections).unwrap();
        replace(&mut document, &mut selections, "c");
        assert_eq!(document.text(), "ac");
        assert!(!document.redo(&mut selections).unwrap());
        document.undo(&mut selections).unwrap();
        assert_eq!(document.text(), "a");
        document.undo(&mut selections).unwrap();
        assert_eq!(document.text(), "");
    }

    #[test]
    fn empty_edits_preserve_revision_history_and_redo() {
        let mut document = Document::from("abc");
        let mut selections = SelectionSet::default();
        replace(&mut document, &mut selections, "x");
        document.undo(&mut selections).unwrap();
        let revision = document.revision();
        let transaction = document
            .transaction([Edit::insert(CharOffset(0), "")])
            .unwrap();
        assert!(transaction.is_empty());
        assert!(!document.apply(transaction, &mut selections).unwrap());
        assert_eq!(document.revision(), revision);
        assert_eq!(document.undo_depth(), 0);
        assert_eq!(document.redo_depth(), 1);
        let transaction = document
            .transaction([])
            .unwrap()
            .with_selections(SelectionSet::single(range(2, 1)))
            .unwrap();
        assert!(!document.apply(transaction, &mut selections).unwrap());
        assert_eq!(selections.primary(), range(2, 1));
        assert_eq!(document.revision(), revision);
        assert_eq!(document.redo_depth(), 1);
    }

    #[test]
    fn history_limit_evicts_oldest_edits_and_can_disable_history() {
        let mut document = Document::default();
        let mut selections = SelectionSet::default();
        document.set_history_limit(2);
        for text in ["a", "b", "c"] {
            replace(&mut document, &mut selections, text);
        }
        assert_eq!(document.undo_depth(), 2);
        document.undo(&mut selections).unwrap();
        document.undo(&mut selections).unwrap();
        assert_eq!(document.text(), "a");
        assert!(!document.undo(&mut selections).unwrap());
        document.set_history_limit(0);
        assert_eq!(document.redo_depth(), 0);
        replace(&mut document, &mut selections, "z");
        assert_eq!(document.text(), "az");
        assert_eq!(document.undo_depth(), 0);
        assert!(!document.undo(&mut selections).unwrap());
    }

    #[test]
    fn scalar_and_byte_coordinates_are_explicit_and_checked() {
        let document = Document::from("aé🦀e\u{301}\r\n");
        let byte_offsets = [0, 1, 3, 7, 8, 10, 11, 12];
        for (character, byte) in byte_offsets.into_iter().enumerate() {
            assert_eq!(
                document.char_to_byte(CharOffset(character)).unwrap(),
                ByteOffset(byte)
            );
            assert_eq!(
                document.byte_to_char(ByteOffset(byte)).unwrap(),
                CharOffset(character)
            );
        }
        for byte in [2, 4, 5, 6, 9, 13, usize::MAX] {
            assert!(document.byte_to_char(ByteOffset(byte)).is_err());
        }
        assert!(document.char_to_byte(CharOffset(8)).is_err());
    }

    #[test]
    fn streaming_io_preserves_unicode_and_line_endings() {
        let input = "日本語\r\ne\u{301} 👩\u{200d}💻\n\r\n";
        let document = Document::from_reader(input.as_bytes()).unwrap();
        let mut output = Vec::new();
        document.write_to(&mut output).unwrap();
        assert_eq!(output, input.as_bytes());
        assert_eq!(
            Document::from_reader(&[0xff, 0xfe][..]).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn io_failures_are_reported() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("read failed"))
            }
        }
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("write failed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert!(Document::from_reader(Broken).is_err());
        assert!(Document::from("abc").write_to(Broken).is_err());
    }

    #[test]
    fn edits_across_rope_chunks_preserve_unicode_and_undo() {
        let original = "e\u{301} 日本語 👩\u{200d}💻\r\n".repeat(2_000);
        let mut document = Document::from(original.as_str());
        assert!(document.text().chunks().count() > 1);
        let mut selections =
            SelectionSet::new(vec![range(4_000, 1_000), range(10_000, 12_000)], 0).unwrap();
        let before = selections.clone();
        let mut expected: Vec<char> = original.chars().collect();
        expected.splice(10_000..12_000, "🦀\r\n".chars());
        expected.splice(1_000..4_000, "🦀\r\n".chars());
        replace(&mut document, &mut selections, "🦀\r\n");
        assert_eq!(
            document.text().to_string(),
            expected.into_iter().collect::<String>()
        );
        document.undo(&mut selections).unwrap();
        assert_eq!(document.text(), original.as_str());
        assert_eq!(selections, before);
    }

    #[test]
    fn a_thousand_carets_insert_once_each_in_one_transaction() {
        let original = "x".repeat(2_000);
        let mut document = Document::from(original.as_str());
        let mut selections = SelectionSet::new(
            (0..1_000)
                .map(|index| range(index * 2, index * 2))
                .collect(),
            500,
        )
        .unwrap();
        replace(&mut document, &mut selections, "λ");
        assert_eq!(document.text().to_string(), "λxx".repeat(1_000));
        assert_eq!(selections.primary(), range(1_501, 1_501));
        assert_eq!(document.undo_depth(), 1);
        document.undo(&mut selections).unwrap();
        assert_eq!(document.text(), original.as_str());
        assert_eq!(selections.primary(), range(1_000, 1_000));
    }
}

#[cfg(test)]
mod property_tests {
    use crate::{CharOffset, Document, Selection, SelectionSet};
    use proptest::prelude::*;

    fn text(max: usize) -> impl Strategy<Value = String> {
        prop::collection::vec(any::<char>(), 0..max).prop_map(|chars| chars.into_iter().collect())
    }

    fn range(anchor: usize, head: usize) -> Selection {
        Selection::new(CharOffset(anchor), CharOffset(head))
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]
        #[test]
        fn multi_selection_replacement_matches_flat_text(
            original in text(120),
            pairs in prop::collection::vec((any::<usize>(), any::<usize>()), 1..25),
            primary in any::<usize>(),
            inserted in text(12),
        ) {
            let mut expected: Vec<char> = original.chars().collect();
            let len = expected.len();
            let before = SelectionSet::new(pairs.iter().map(|&(a, h)| {
                range(a % (len + 1), h % (len + 1))
            }).collect(), primary % pairs.len()).unwrap();
            for selection in before.ranges().iter().rev() {
                expected.splice(selection.start().0..selection.end().0, inserted.chars());
            }
            let mut document = Document::from(original.as_str());
            let mut selections = before.clone();
            let transaction = document.replace_selections(&selections, inserted.as_str()).unwrap();
            let changed = document.apply(transaction, &mut selections).unwrap();
            prop_assert_eq!(document.text().to_string(), expected.into_iter().collect::<String>());
            prop_assert!(selections.ranges().iter().all(|s| s.is_empty()));
            document.undo(&mut selections).unwrap();
            prop_assert_eq!(document.text().to_string(), original);
            if changed {
                prop_assert_eq!(selections, before);
            }
        }

        #[test]
        fn history_matches_a_snapshot_model_across_edit_and_navigation_sequences(
            original in text(40),
            actions in prop::collection::vec((0u8..6, any::<usize>(), any::<usize>(), text(10)), 1..100),
        ) {
            let mut document = Document::from(original.as_str());
            let mut selections = SelectionSet::default();
            let mut model: (Vec<char>, SelectionSet) = (original.chars().collect(), selections.clone());
            let mut past = Vec::new();
            let mut future = Vec::new();
            let mut revision = 0;
            for (kind, a, h, inserted) in actions {
                match kind {
                    0..=2 => {
                        let a = a % (model.0.len() + 1);
                        let h = if kind == 0 { a } else { h % (model.0.len() + 1) };
                        let inserted = if kind == 1 { "" } else { inserted.as_str() };
                        selections = SelectionSet::single(range(a, h));
                        model.1 = selections.clone();
                        let before = model.clone();
                        model.0.splice(a.min(h)..a.max(h), inserted.chars());
                        let caret = a.min(h) + inserted.chars().count();
                        model.1 = SelectionSet::single(range(caret, caret));
                        let transaction = document.replace_selections(&selections, inserted).unwrap();
                        let changed = document.apply(transaction, &mut selections).unwrap();
                        prop_assert_eq!(changed, a != h || !inserted.is_empty());
                        if changed {
                            past.push((before, model.clone()));
                            future.clear();
                            revision += 1;
                        }
                    }
                    3 => {
                        let changed = document.undo(&mut selections).unwrap();
                        prop_assert_eq!(changed, !past.is_empty());
                        if let Some(entry) = past.pop() {
                            model = entry.0.clone();
                            future.push(entry);
                            revision += 1;
                        }
                    }
                    4 => {
                        let changed = document.redo(&mut selections).unwrap();
                        prop_assert_eq!(changed, !future.is_empty());
                        if let Some(entry) = future.pop() {
                            model = entry.1.clone();
                            past.push(entry);
                            revision += 1;
                        }
                    }
                    _ => {
                        selections = SelectionSet::single(range(a % (model.0.len() + 1), h % (model.0.len() + 1)));
                        model.1 = selections.clone();
                    }
                }
                prop_assert_eq!(document.text().to_string(), model.0.iter().collect::<String>());
                prop_assert_eq!(&selections, &model.1);
                prop_assert_eq!(document.revision().get(), revision);
                prop_assert_eq!(document.undo_depth(), past.len());
                prop_assert_eq!(document.redo_depth(), future.len());
            }
        }
    }
}
