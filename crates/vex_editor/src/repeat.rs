//! Logical insert recording, shared across buffers, with cooperative playback.

use std::{
    num::NonZeroUsize,
    ops::Range,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use vex_core::{Affinity, CharOffset, Edit, Revision, Selection, SelectionSet};

use crate::{Command, CommandContext, Editor, Error, Mode, ViewId, YankRegister, commands};

/// Shared editing state for a frontend's buffers. Cloning shares registers and
/// the last completed insert without retaining any document snapshots.
#[derive(Clone, Debug, Default)]
pub struct Session {
    pub(crate) yank: YankRegister,
    history: Arc<Mutex<Option<Arc<Program>>>>,
}

impl Session {
    pub(crate) fn with_yank_register(yank: YankRegister) -> Self {
        Self {
            yank,
            ..Self::default()
        }
    }
}

#[derive(Debug)]
enum Action {
    Command {
        command: &'static Command,
        count: NonZeroUsize,
        explicit: bool,
        character: Option<char>,
        register: Option<char>,
        text: Option<Range<usize>>,
    },
    Completion {
        before: usize,
        after: usize,
        text: Range<usize>,
    },
}

#[derive(Debug, Default)]
struct Program {
    actions: Vec<Action>,
    // Amortize allocation across typing events without merging their semantic
    // boundaries: adjacent insertions can move across existing combining marks.
    text: String,
}

impl Program {
    fn text(&mut self, text: &str) -> Range<usize> {
        let start = self.text.len();
        self.text.push_str(text);
        start..self.text.len()
    }

    fn command(&mut self, command: &'static Command, ctx: &CommandContext<'_>) {
        let text = ctx.text.map(|text| self.text(text));
        self.actions.push(Action::Command {
            command,
            count: ctx.count,
            explicit: ctx.count_given,
            character: ctx.character,
            register: ctx.register,
            text,
        });
    }
}

#[derive(Debug)]
struct Playback {
    program: Arc<Program>,
    next: usize,
    remaining: usize,
    revision: Revision,
    view: ViewId,
}

#[derive(Debug)]
pub(crate) struct Recorder {
    pub session: Session,
    building: Option<Program>,
    depth: usize,
    pub stepping: bool,
    pub service: bool,
    playback: Option<Playback>,
    deferred: bool,
}

impl Recorder {
    pub fn new(session: Session) -> Self {
        Self {
            session,
            building: None,
            depth: 0,
            stepping: false,
            service: false,
            playback: None,
            deferred: false,
        }
    }

    fn publish(&mut self) {
        if let Some(program) = self.building.take() {
            *self.session.history.lock().unwrap() = Some(Arc::new(program));
        }
    }
}

// Public command functions enter here as well as keymap/registry dispatch.
// Only the outermost call is recorded when one command implements another.
pub(crate) fn invoke(
    ctx: &mut CommandContext<'_>,
    command: &'static Command,
    run: impl FnOnce(&mut CommandContext<'_>) -> Result<(), Error>,
) -> Result<(), Error> {
    if ctx.editor.repeat_pending() && !ctx.editor.recorder.stepping {
        ctx.editor.cancel_repeat();
    }
    let before = ctx.editor.mode;
    if ctx.editor.recorder.depth == 0 {
        let selected = ctx.editor.selected_register.take();
        if ctx.register.is_none() {
            ctx.register = selected;
        }
        ctx.editor.recorder.service = false;
    }
    ctx.editor.recorder.depth += 1;
    let result = run(ctx);
    ctx.editor.recorder.depth -= 1;
    if result.is_err()
        || ctx.editor.recorder.depth != 0
        || ctx.editor.recorder.stepping
        || ctx.editor.repeat_pending()
        || ctx.editor.recorder.service
    {
        return result;
    }
    if before != Mode::Insert && ctx.editor.mode == Mode::Insert {
        ctx.editor.recorder.building = Some(Program::default());
    }
    if before == Mode::Insert || ctx.editor.mode == Mode::Insert {
        if let Some(mut program) = ctx.editor.recorder.building.take() {
            program.command(command, ctx);
            ctx.editor.recorder.building = Some(program);
        }
        if ctx.editor.mode != Mode::Insert {
            ctx.editor.recorder.publish();
        }
    }
    result
}

pub(crate) fn start(ctx: &mut CommandContext<'_>) -> Result<(), Error> {
    let editor = &mut *ctx.editor;
    if editor.mode == Mode::Insert {
        return Err(Error::WrongMode {
            expected: Mode::Normal,
            actual: editor.mode,
        });
    }
    editor.finish_undo_group();
    let Some(program) = editor.recorder.session.history.lock().unwrap().clone() else {
        return Ok(());
    };
    editor.recorder.playback = Some(Playback {
        program,
        next: 0,
        remaining: ctx.count.get(),
        revision: editor.document.revision(),
        view: editor.active_view(),
    });
    if !editor.recorder.deferred {
        while editor.repeat_pending() {
            editor.advance_repeat(64)?;
        }
    }
    Ok(())
}

impl Editor {
    pub fn session(&self) -> Session {
        self.recorder.session.clone()
    }

    /// Frontends should enable this and advance playback between event batches.
    /// Standalone command calls replay synchronously by default.
    pub fn set_deferred_repeat(&mut self, deferred: bool) {
        self.recorder.deferred = deferred;
    }

    pub fn repeat_pending(&self) -> bool {
        self.recorder.playback.is_some()
    }

    /// Run at most `max_actions` logical actions, yielding after four milliseconds
    /// between actions. An individual command retains its own complexity bounds.
    pub fn advance_repeat(&mut self, max_actions: usize) -> Result<bool, Error> {
        let Some(mut playback) = self.recorder.playback.take() else {
            return Ok(false);
        };
        if playback.revision != self.document.revision() || playback.view != self.active_view() {
            self.recorder.playback = Some(playback);
            self.cancel_repeat();
            return Err(Error::RepeatChanged);
        }
        let started = Instant::now();
        self.recorder.stepping = true;
        let mut result = Ok(());
        let mut ran = false;
        for _ in 0..max_actions {
            let program = &playback.program;
            result = match &program.actions[playback.next] {
                Action::Command {
                    command,
                    count,
                    explicit,
                    character,
                    register,
                    text,
                } => {
                    let mut ctx = CommandContext::new(self);
                    ctx.count = *count;
                    ctx.count_given = *explicit;
                    ctx.character = *character;
                    ctx.register = *register;
                    ctx.text = text.as_ref().map(|range| &program.text[range.clone()]);
                    (command.run)(&mut ctx)
                }
                Action::Completion {
                    before,
                    after,
                    text,
                } => self.repeat_completion(*before, *after, &program.text[text.clone()]),
            };
            ran = true;
            if result.is_err() {
                break;
            }
            playback.next += 1;
            if playback.next == program.actions.len() {
                playback.next = 0;
                playback.remaining -= 1;
                if playback.remaining == 0 {
                    break;
                }
            }
            if started.elapsed() >= Duration::from_millis(4) {
                break;
            }
        }
        self.recorder.stepping = false;
        playback.revision = self.document.revision();
        if playback.remaining != 0 {
            self.recorder.playback = Some(playback);
        }
        if result.is_err() {
            self.cancel_repeat();
        }
        if !self.repeat_pending() {
            self.document.finish_undo_group();
        }
        result.map(|()| ran)
    }

    /// Stop at the completed prefix, returning to normal mode. Completed edits
    /// remain one undo step (except explicit insert checkpoints).
    pub fn cancel_repeat(&mut self) {
        if self.recorder.playback.take().is_none() {
            return;
        }
        self.recorder.stepping = true;
        let _ = commands::normal_mode(&mut CommandContext::new(self));
        self.recorder.stepping = false;
        self.document.finish_undo_group();
    }

    pub(crate) fn record_completion(&mut self, cursor: CharOffset, edit: &Edit) {
        if self.recorder.stepping {
            return;
        }
        if let Some(program) = &mut self.recorder.building {
            let text = program.text(edit.text());
            program.actions.push(Action::Completion {
                before: cursor.0 - edit.range().start.0,
                after: edit.range().end.0 - cursor.0,
                text,
            });
        }
    }

    fn repeat_completion(&mut self, before: usize, after: usize, text: &str) -> Result<(), Error> {
        let text: Arc<str> = text.into();
        let edits = self
            .selections
            .ranges()
            .iter()
            .map(|selection| {
                let head = selection.head.0;
                let start = head.checked_sub(before).ok_or(Error::RepeatCompletion)?;
                let end = head
                    .checked_add(after)
                    .filter(|&end| end <= self.document.text().len_chars())
                    .ok_or(Error::RepeatCompletion)?;
                Ok(Edit::new(CharOffset(start)..CharOffset(end), text.clone()))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let transaction = self.document.transaction(edits)?;
        let carets = self
            .selections
            .ranges()
            .iter()
            .map(|selection| {
                transaction
                    .map_position(CharOffset(selection.head.0 - before), Affinity::After)
                    .map(Selection::cursor)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let transaction = transaction
            .with_selections(SelectionSet::new(carets, self.selections.primary_index())?)?;
        self.apply(transaction, true)?;
        self.selections = self.normalized(self.selections.clone(), Mode::Insert)?;
        self.preferred_columns = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Key, KeyHandler, Keymap, LanguageAction};
    use vex_core::Document;

    fn command(editor: &mut Editor, name: &str) {
        editor.execute(name, 1).unwrap();
    }

    fn at(editor: &mut Editor, position: usize) {
        editor
            .set_selections(SelectionSet::single(Selection::cursor(CharOffset(
                position,
            ))))
            .unwrap();
    }

    fn record(editor: &mut Editor, entry: &str, text: &str) {
        command(editor, entry);
        editor.insert_text(text).unwrap();
        command(editor, "normal_mode");
    }

    #[test]
    fn entry_commands_repeat_at_the_new_selections() {
        for (entry, expected) in [
            ("insert_mode", "  Zabc\n  Zdef\n"),
            ("append_mode", "  aZbc\n  dZef\n"),
            ("insert_at_line_start", "  Zabc\n  Zdef\n"),
            ("insert_at_line_end", "  abcZ\n  defZ\n"),
            ("change_selection", "  Zbc\n  Zef\n"),
            ("open_below", "  abc\n  Z\n  def\n  Z\n"),
            ("open_above", "  Z\n  abc\n  Z\n  def\n"),
        ] {
            let mut editor = Editor::new(Document::from("  abc\n  def\n"));
            at(&mut editor, 2);
            record(&mut editor, entry, "Z");
            let second = editor
                .document
                .text()
                .chars()
                .position(|ch| ch == 'd')
                .unwrap();
            at(&mut editor, second);
            let before = editor.document.snapshot();
            command(&mut editor, "repeat_insert");
            assert_eq!(editor.document.text(), expected, "{entry}");
            assert_eq!(editor.mode, Mode::Normal);
            command(&mut editor, "undo");
            assert_eq!(editor.document.text(), before.text());
        }
    }

    #[test]
    fn counts_replay_the_recorded_command_and_repeat_whole_sessions() {
        let mut editor = Editor::new(Document::from("a\nb\n"));
        editor.execute("open_below", 2).unwrap();
        editor.insert_text("X").unwrap();
        command(&mut editor, "normal_mode");
        assert_eq!(editor.document.text(), "a\nX\nX\nb\n");
        at(&mut editor, 6);
        command(&mut editor, "repeat_insert");
        assert_eq!(editor.document.text(), "a\nX\nX\nb\nX\nX\n");

        let mut editor = Editor::new(Document::from("a"));
        record(&mut editor, "append_mode", "X");
        editor.execute("repeat_insert", 3).unwrap();
        assert_eq!(editor.document.text(), "aXXXX");
        command(&mut editor, "undo");
        assert_eq!(editor.document.text(), "aX");
        command(&mut editor, "redo");
        assert_eq!(editor.document.text(), "aXXXX");
    }

    #[test]
    fn dot_is_bound_in_normal_and_select_modes_and_empty_history_does_nothing() {
        let mut editor = Editor::new(Document::from("a"));
        let mut keys = KeyHandler::default();
        keys.handle(&mut editor, Key::Char('.')).unwrap();
        assert_eq!(editor.document.text(), "a");
        record(&mut editor, "append_mode", "X");
        for key in [
            Key::Char('2'),
            Key::Char('.'),
            Key::Char('v'),
            Key::Char('.'),
        ] {
            keys.handle(&mut editor, key).unwrap();
        }
        assert_eq!(editor.document.text(), "aXXXX");
        assert_eq!(editor.mode, Mode::Normal);
    }

    #[test]
    fn normal_edits_and_undo_do_not_overwrite_the_insert_but_empty_insert_does() {
        let mut editor = Editor::new(Document::from("a"));
        record(&mut editor, "append_mode", "X");
        command(&mut editor, "delete_selection");
        command(&mut editor, "repeat_insert");
        assert_eq!(editor.document.text(), "aX");
        command(&mut editor, "undo");
        command(&mut editor, "repeat_insert");
        assert_eq!(editor.document.text(), "aX");
        command(&mut editor, "insert_mode");
        command(&mut editor, "normal_mode");
        command(&mut editor, "repeat_insert");
        assert_eq!(editor.document.text(), "aX");
    }

    #[test]
    fn direct_calls_record_commands_and_rebinding_does_not_change_the_program() {
        let mut editor = Editor::new(Document::from("abc"));
        commands::append_mode(&mut CommandContext::new(&mut editor)).unwrap();
        editor.insert_text("xy").unwrap();
        commands::delete_backward(&mut CommandContext::new(&mut editor)).unwrap();
        commands::normal_mode(&mut CommandContext::new(&mut editor)).unwrap();
        let mut keymap = Keymap::default();
        keymap
            .bind(Mode::Insert, vec![Key::Backspace], "delete_forward")
            .unwrap();
        let mut keys = KeyHandler::new(keymap);
        keys.handle(&mut editor, Key::Char('.')).unwrap();
        assert_eq!(editor.document.text(), "axxbc");
    }

    #[test]
    fn motions_deletions_newlines_and_literal_paste_replay_as_actions() {
        let mut original = Editor::new(Document::from("  abc\r\n"));
        command(&mut original, "insert_at_line_end");
        original.insert_text(" last word").unwrap();
        command(&mut original, "delete_word_backward");
        command(&mut original, "insert_newline");
        original.insert_paste("e\u{301}🦀\r\n").unwrap();
        command(&mut original, "move_left");
        command(&mut original, "delete_backward");
        command(&mut original, "normal_mode");
        let mut replay = Editor::with_session(Document::from("  abc\r\n"), original.session());
        command(&mut replay, "repeat_insert");
        assert_eq!(replay.document.text(), original.document.text());
        assert_eq!(replay.selections, original.selections);
        command(&mut replay, "undo");
        assert_eq!(replay.document.text(), "  abc\r\n");
    }

    #[test]
    fn separate_typing_events_keep_grapheme_boundary_semantics() {
        let mut original = Editor::new(Document::default());
        command(&mut original, "insert_mode");
        original.insert_text("a").unwrap();
        original.insert_text("b").unwrap();
        command(&mut original, "normal_mode");
        let mut target = Editor::with_session(Document::from("\u{301}x"), original.session());
        command(&mut target, "repeat_insert");
        assert_eq!(target.document.text(), "a\u{301}bx");
    }

    #[test]
    fn completion_replays_locally_at_each_caret_without_reapplying_imports() {
        let mut editor = Editor::new(Document::from("\npr"));
        at(&mut editor, 2);
        command(&mut editor, "append_mode");
        command(&mut editor, "completion");
        // Even an undrained service request must not suppress subsequent input.
        editor.insert_text("i").unwrap();
        editor
            .apply_completion(
                Edit::new(CharOffset(1)..CharOffset(4), "print界"),
                vec![Edit::insert(CharOffset(0), "use io;\n")],
            )
            .unwrap();
        command(&mut editor, "normal_mode");
        let session = editor.session();
        drop(editor);
        let mut target = Editor::with_session(Document::from("pr\npr"), session);
        target
            .set_selections(
                SelectionSet::new(
                    vec![
                        Selection::cursor(CharOffset(1)),
                        Selection::cursor(CharOffset(4)),
                    ],
                    1,
                )
                .unwrap(),
            )
            .unwrap();
        command(&mut target, "repeat_insert");
        assert_eq!(target.document.text(), "print界\nprint界");
        assert_eq!(target.take_language_action(), None);
        assert_eq!(target.selections.primary_index(), 1);
        command(&mut target, "undo");
        assert_eq!(target.document.text(), "pr\npr");
    }

    #[test]
    fn invalid_completion_stops_without_losing_the_saved_program() {
        let mut source = Editor::new(Document::from("prefix"));
        command(&mut source, "insert_at_line_end");
        source
            .apply_completion(Edit::new(CharOffset(0)..CharOffset(6), "value"), vec![])
            .unwrap();
        command(&mut source, "normal_mode");
        let mut target = Editor::with_session(Document::from("x"), source.session());
        assert_eq!(
            target.execute("repeat_insert", 1),
            Err(Error::RepeatCompletion)
        );
        assert!(!target.repeat_pending());
        assert_eq!(target.mode, Mode::Normal);
        assert_eq!(target.document.text(), "x");
        let mut valid = Editor::with_session(Document::from("abcdef"), target.session());
        command(&mut valid, "repeat_insert");
        assert_eq!(valid.document.text(), "value");
    }

    #[test]
    fn explicit_checkpoints_survive_replay() {
        let mut source = Editor::new(Document::default());
        command(&mut source, "insert_mode");
        source.insert_text("a").unwrap();
        command(&mut source, "commit_undo_checkpoint");
        source.insert_text("b").unwrap();
        command(&mut source, "normal_mode");
        let mut target = Editor::with_session(Document::default(), source.session());
        command(&mut target, "repeat_insert");
        assert_eq!(target.document.text(), "ab");
        command(&mut target, "undo");
        assert_eq!(target.document.text(), "a");
        command(&mut target, "undo");
        assert_eq!(target.document.text(), "");
    }

    #[test]
    fn deferred_replay_yields_and_cancel_keeps_the_completed_prefix_undoable() {
        let mut editor = Editor::new(Document::from("a"));
        record(&mut editor, "append_mode", "X");
        editor.set_deferred_repeat(true);
        editor.execute("repeat_insert", usize::MAX).unwrap();
        assert_eq!(editor.document.text(), "aX");
        assert!(!editor.advance_repeat(0).unwrap());
        assert!(editor.advance_repeat(2).unwrap());
        assert_eq!(editor.document.text(), "aXX");
        assert_eq!(editor.mode, Mode::Insert);
        editor.cancel_repeat();
        assert!(!editor.repeat_pending());
        assert_eq!(editor.mode, Mode::Normal);
        command(&mut editor, "undo");
        assert_eq!(editor.document.text(), "aX");
        editor.set_deferred_repeat(false);
        command(&mut editor, "repeat_insert");
        assert_eq!(editor.document.text(), "aXX");
    }

    #[test]
    fn external_edits_and_view_changes_cancel_replay_before_mapping_cursors() {
        let mut editor = Editor::new(Document::from("a"));
        record(&mut editor, "append_mode", "X");
        editor.set_deferred_repeat(true);
        let other = editor.duplicate_view();
        command(&mut editor, "repeat_insert");
        editor.advance_repeat(2).unwrap();
        assert!(editor.focus_view(other));
        assert!(!editor.repeat_pending());
        command(&mut editor, "normal_mode");
        command(&mut editor, "repeat_insert");
        editor.advance_repeat(2).unwrap();
        let transaction = editor
            .document
            .transaction([Edit::insert(CharOffset(0), "z")])
            .unwrap();
        editor.apply_external_change(transaction).unwrap();
        assert!(!editor.repeat_pending());
        assert_eq!(editor.mode, Mode::Normal);
        assert!(!editor.advance_repeat(64).unwrap());
        assert_eq!(editor.take_language_action(), None::<LanguageAction>);
    }
}
