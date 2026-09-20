//! Application state and file commands. Editing continues through vex_editor's
//! named command functions; terminal events contain no movement implementation.

use crate::{
    files::FileState,
    input::{self, Prompt},
    render::{self, Chrome, Viewport},
    screen::Frame,
};
use crossterm::event::{Event, KeyCode, KeyEventKind};
use std::{io, path::Path};
use vex_core::Document;
use vex_editor::{
    ApplicationAction, Editor, Key, KeyHandler, Language, Mode, SearchCompletion, SearchPrompt,
    SearchResult, SearchStatus,
};

mod clipboard;
mod completion;
mod git;
mod git_write;
mod jumps;
mod language;
mod picker;
mod prompt;
mod reload;
mod status;
mod windows;

enum PromptKind {
    Command,
    Search {
        operation: SearchPrompt,
        viewport: Viewport,
    },
}

struct ActivePrompt {
    input: Prompt,
    kind: PromptKind,
    register: char,
    history_position: Option<usize>,
    completion: prompt::Completion,
}

impl ActivePrompt {
    fn command() -> Self {
        Self {
            input: Prompt::default(),
            kind: PromptKind::Command,
            register: ':',
            history_position: None,
            completion: prompt::Completion::default(),
        }
    }
    fn prefix(&self) -> &'static str {
        match self.kind {
            PromptKind::Command => ":",
            PromptKind::Search { operation, .. } => operation.label(),
        }
    }
}

pub struct App {
    pub editor: Editor,
    files: FileState,
    keys: KeyHandler,
    viewport: Viewport,
    prompt: Option<ActivePrompt>,
    prompt_history: prompt::History,
    message: String,
    error: bool,
    quit: bool,
    size: (u16, u16),
    automatic_language: bool,
    language: language::State,
    picker: picker::State,
    completion: completion::State,
    clipboard: clipboard::State,
    windows: windows::State,
    git: git::State,
    status: status::State,
    git_write: git_write::State,
    reload: reload::State,
}

impl App {
    pub fn open(path: Option<&Path>, size: (u16, u16)) -> io::Result<Self> {
        let (document, files) = FileState::load(path)?;
        Ok(Self::new(document, files, size))
    }

    pub fn from_document(document: Document, size: (u16, u16)) -> Self {
        let files = FileState::scratch(&document);
        Self::new(document, files, size)
    }

    fn new(document: Document, files: FileState, size: (u16, u16)) -> Self {
        let mut editor = Editor::new(document);
        editor.set_display_name(files.display_name());
        editor.set_language(Language::detect(files.path(), editor.document().text()));
        let windows = windows::State::new(&editor);
        Self {
            editor,
            files,
            keys: KeyHandler::default(),
            viewport: Viewport::default(),
            prompt: None,
            prompt_history: prompt::History::default(),
            message: "i insert  / search  :w write  :q quit  :help".into(),
            error: false,
            quit: false,
            size,
            automatic_language: true,
            language: language::State::default(),
            picker: picker::State::default(),
            completion: completion::State::default(),
            clipboard: clipboard::State::default(),
            windows,
            git: git::State::default(),
            status: status::State::default(),
            git_write: git_write::State::default(),
            reload: reload::State::default(),
        }
    }

    pub fn size(&self) -> (u16, u16) {
        self.size
    }
    pub fn should_quit(&self) -> bool {
        self.quit
    }
    pub fn is_dirty(&self) -> bool {
        self.files.is_dirty(self.editor.document())
    }

    /// Deliver a worker completion on the same thread that handles input.
    /// Stale results neither move selections nor replace newer messages.
    pub fn handle_search_result(&mut self, result: SearchResult) -> bool {
        self.observe_buffer_revision();
        match self.editor.apply_search_result(result) {
            Ok(SearchCompletion::Ignored) => {
                if self.editor.search_prompt().is_none()
                    && self
                        .prompt
                        .as_ref()
                        .is_some_and(|p| matches!(p.kind, PromptKind::Search { .. }))
                {
                    self.prompt = None;
                    return true;
                }
                return false;
            }
            Ok(SearchCompletion::Accepted) => {
                self.prompt = None;
                self.clear_message();
            }
            Ok(SearchCompletion::Preview) => {
                self.clear_message();
                if self.editor.search_status() == Some(SearchStatus::NoMatch) {
                    self.fail("no matches");
                }
            }
            Ok(SearchCompletion::Navigation) => self.clear_message(),
            Err(error) => {
                self.keys.cancel(&mut self.editor);
                if self.editor.search_prompt().is_none() {
                    self.prompt = None;
                } else if let Some(ActivePrompt {
                    kind: PromptKind::Search { viewport, .. },
                    ..
                }) = &self.prompt
                {
                    self.viewport = *viewport;
                }
                self.fail(error);
            }
        }
        self.apply_application_action();
        self.observe_buffer_revision();
        true
    }

    /// Handle one event. Return whether the screen may have changed.
    pub fn handle(&mut self, event: Event) -> bool {
        self.handle_at(event, std::time::Instant::now())
    }

    /// Cooperatively replay before the next event batch, keeping background
    /// services, drawing, resize, and cancellation live during large counts.
    pub(crate) fn advance_repeat(&mut self) -> bool {
        if !self.editor.repeat_ready() {
            return false;
        }
        self.completion.clear();
        match self.editor.advance_repeat(64) {
            Ok(changed) => {
                self.apply_application_action();
                self.observe_buffer_revision();
                changed
            }
            Err(error) => {
                self.observe_buffer_revision();
                self.fail(error);
                true
            }
        }
    }

    fn handle_at(&mut self, event: Event, now: std::time::Instant) -> bool {
        self.invalidate_clipboard();
        self.observe_buffer_revision();
        let changed = self.handle_event_at(event, now);
        self.observe_buffer_revision();
        changed
    }

    fn handle_event_at(&mut self, event: Event, now: std::time::Instant) -> bool {
        if self.clipboard_waiting()
            && matches!(&event, Event::Key(key) if key.kind != KeyEventKind::Release
                && matches!(input::key(*key), Some(Key::Escape | Key::Ctrl('c'))))
        {
            self.cancel_clipboard();
            self.keys.cancel(&mut self.editor);
            self.clear_message();
            return true;
        }
        if matches!(event, Event::FocusGained) {
            self.refresh_git();
            self.refresh_status();
        }
        if let Some(redraw) = self.handle_picker_input(&event) {
            return redraw;
        }
        if let Some(redraw) = self.handle_commit_input(&event) {
            return redraw;
        }
        if let Some(redraw) = self.handle_status_input(&event) {
            return redraw;
        }
        if let Some(redraw) = self.handle_completion_input(&event) {
            return redraw;
        }
        let typing = self.completion_typing(&event);
        let redraw = self.handle_document_input(event);
        if let Some(typing) = typing {
            self.schedule_completion(typing, now);
        }
        redraw
    }

    fn handle_document_input(&mut self, event: Event) -> bool {
        if matches!(&event, Event::Paste(_) | Event::FocusLost)
            || matches!(&event, Event::Key(key) if key.kind != KeyEventKind::Release)
        {
            self.dismiss_language_help();
        }
        match event {
            Event::Resize(width, height) => {
                self.size = (width, height);
                self.request_picker_preview();
                true
            }
            Event::FocusGained => true,
            Event::Paste(text) => {
                self.clear_message();
                self.keys.cancel(&mut self.editor);
                if let Some(mut prompt) = self.prompt.take() {
                    prompt.input.insert(&text);
                    self.preview_search(&prompt);
                    self.prompt = Some(prompt);
                } else if self.editor.mode() == Mode::Insert {
                    if let Err(error) = self.editor.insert_paste(&text) {
                        self.fail(error);
                    }
                } else {
                    self.fail("enter insert mode to paste");
                }
                true
            }
            Event::Key(event) if event.kind != KeyEventKind::Release => {
                if self.prompt.is_none()
                    && self.keys.pending_keys().is_empty()
                    && event.modifiers.is_empty()
                    && matches!(event.code, KeyCode::PageUp | KeyCode::PageDown)
                {
                    self.clear_message();
                    self.keys.cancel(&mut self.editor);
                    let count = usize::from(self.active_size().1).saturating_sub(2).max(1);
                    if let Err(error) = self.editor.execute(
                        if event.code == KeyCode::PageDown {
                            "move_down"
                        } else {
                            "move_up"
                        },
                        count,
                    ) {
                        self.fail(error);
                    }
                    return true;
                }
                let Some(key) = input::key(event) else {
                    return false;
                };
                self.clear_message();
                if self.prompt.is_some() {
                    self.handle_prompt_key(key);
                } else {
                    match key {
                        Key::Escape | Key::Ctrl('c') if self.clipboard_waiting() => {
                            self.cancel_clipboard();
                            self.keys.cancel(&mut self.editor);
                        }
                        Key::Escape | Key::Ctrl('c') if self.editor.repeat_pending() => {
                            self.editor.cancel_repeat();
                            self.keys.cancel(&mut self.editor);
                        }
                        Key::Ctrl('c') if self.editor.search_waiting() => {
                            // A pending scan is cancellable before subsequent
                            // editing keys. Do not dispatch normal-mode comments.
                            self.editor.finish_undo_group();
                            self.keys.cancel(&mut self.editor);
                        }
                        Key::Char(':')
                            if self.editor.mode() != Mode::Insert
                                && self.keys.pending_keys().is_empty() =>
                        {
                            self.keys.cancel(&mut self.editor);
                            self.prompt = Some(ActivePrompt::command());
                        }
                        _ => {
                            if let Err(error) = self.keys.handle(&mut self.editor, key) {
                                self.fail(error);
                            }
                        }
                    }
                }
                self.open_search_prompt();
                self.apply_application_action();
                true
            }
            _ => false,
        }
    }

    /// Dispatch a colon command. The remaining text is one literal path argument,
    /// so file names may contain spaces. A trailing ! on the command means force.
    pub fn execute(&mut self, text: &str) -> io::Result<()> {
        let result = self.execute_command(text);
        self.observe_buffer_revision();
        result
    }

    fn execute_command(&mut self, text: &str) -> io::Result<()> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(());
        }
        self.prompt_history.push(':', text);
        self.editor
            .set_register(':', std::sync::Arc::from([std::sync::Arc::from(text)]))
            .map_err(io::Error::other)?;
        let split = text.find(char::is_whitespace).unwrap_or(text.len());
        let (name, argument) = text.split_at(split);
        let force = name.ends_with('!');
        let name = name.strip_suffix('!').unwrap_or(name);
        let argument = argument.trim();
        if self.active_git_view().is_some()
            && !matches!(
                name,
                "git_status"
                    | "git_toggle"
                    | "git_visit"
                    | "git_refresh"
                    | "git_close"
                    | "git_stage"
                    | "git_unstage"
                    | "git_commit"
                    | "vsplit"
                    | "vs"
                    | "hsplit"
                    | "hs"
                    | "split"
                    | "sp"
                    | "only"
                    | "quit"
                    | "q"
                    | "quit-all"
                    | "qa"
                    | "qall"
                    | "help"
                    | "h"
            )
        {
            return Err(io::Error::other(
                "Git status cannot edit the retained document; q returns to it",
            ));
        }
        if let Some(command) = COMMANDS
            .iter()
            .find(|c| c.name == name || c.aliases.contains(&name))
        {
            return (command.run)(self, argument, force);
        }
        if !argument.is_empty() || force {
            return Err(io::Error::other("unknown command or unsupported arguments"));
        }
        self.editor.execute(name, 0).map_err(io::Error::other)?;
        self.open_search_prompt();
        self.apply_application_action();
        Ok(())
    }

    pub fn paint(&mut self, frame: &mut Frame) -> io::Result<()> {
        self.invalidate_clipboard();
        self.observe_buffer_revision();
        self.apply_application_action();
        self.refresh_diagnostics();
        self.open_search_prompt();
        self.paint_windows(frame)?;
        let diagnostic = if self.active_git_view().is_some() {
            String::new()
        } else {
            self.diagnostic_message()
        };
        render::paint_command_line(
            frame,
            if self.message.is_empty() {
                &diagnostic
            } else {
                &self.message
            },
            self.error,
            self.prompt
                .as_ref()
                .map(|p| (p.prefix(), p.input.text(), p.input.cursor())),
        );
        self.paint_prompt_completion(frame);
        self.paint_active_picker(frame);
        self.paint_key_hints(frame);
        Ok(())
    }

    fn paint_current_window(&mut self, frame: &mut Frame, reserved_bottom: u16) -> io::Result<()> {
        if let Some(key) = self.active_git_view().cloned() {
            self.status
                .views
                .get_mut(&key)
                .unwrap()
                .paint(frame, reserved_bottom, true);
            return Ok(());
        }
        let filename = self
            .commit_title(self.editor.document().id())
            .unwrap_or_else(|| {
                self.files
                    .path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "[scratch]".into())
            });
        let mut pending = format!(
            "{}{}",
            self.keys.count().map(|n| n.to_string()).unwrap_or_default(),
            self.keys
                .pending_keys()
                .iter()
                .map(ToString::to_string)
                .collect::<String>()
        );
        if let Some(progress) = self.editor.search_progress() {
            pending.push(' ');
            pending.push_str(progress);
        }
        pending.push_str(&self.language_status());
        if self
            .git_write
            .drafts
            .get(&self.editor.document().id())
            .is_some_and(|draft| self.git_operation_for(&draft.root))
        {
            pending.push_str(" Git busy");
        }
        let diagnostic = self.diagnostic_message();
        render::paint_view(
            frame,
            &self.editor,
            &mut self.viewport,
            Chrome {
                filename: &filename,
                title: self
                    .git_write
                    .drafts
                    .contains_key(&self.editor.document().id()),
                dirty: self.files.is_dirty(self.editor.document()),
                pending: &pending,
                message: if self.message.is_empty() {
                    &diagnostic
                } else {
                    &self.message
                },
                error: self.error,
                prompt: self
                    .prompt
                    .as_ref()
                    .map(|p| (p.prefix(), p.input.text(), p.input.cursor())),
            },
            reserved_bottom,
            self.git
                .gutter_diff(self.editor.document().id(), self.files.target()),
        )
        .map_err(io::Error::other)?;
        let body_height = frame.height().saturating_sub(1 + reserved_bottom);
        self.paint_language(frame, body_height);
        self.paint_completion(frame, body_height);
        Ok(())
    }

    fn apply_application_action(&mut self) {
        let result = match self.editor.take_application_action() {
            Some(ApplicationAction::Clipboard(kind, action, count)) => {
                self.begin_clipboard(kind, action, count);
                Ok(())
            }
            Some(ApplicationAction::ClipboardWrite(kind, text, activate)) => {
                self.begin_clipboard_search_write(kind, text, activate);
                Ok(())
            }
            Some(ApplicationAction::SaveSelection) => {
                self.record_jump();
                self.message = "jump checkpoint saved".into();
                Ok(())
            }
            Some(ApplicationAction::Jump { forward, count }) => self.navigate_jump(forward, count),
            Some(ApplicationAction::GitStatus) => self.open_git_status(),
            Some(ApplicationAction::FilePicker) => {
                self.open_file_picker();
                Ok(())
            }
            Some(ApplicationAction::LastPicker) => self.reopen_last_picker(),
            Some(ApplicationAction::GlobalSearch(register)) => self.open_workspace_search(register),
            Some(ApplicationAction::BufferPicker) => {
                self.open_buffer_picker();
                Ok(())
            }
            Some(ApplicationAction::Buffer(action, count)) => self.buffer_action(action, count),
            Some(ApplicationAction::DocumentSymbols) => {
                self.open_symbol_picker(false);
                Ok(())
            }
            Some(ApplicationAction::WorkspaceSymbols) => {
                self.open_symbol_picker(true);
                Ok(())
            }
            Some(ApplicationAction::HalfPageUp(count)) => self
                .scroll_half_page(false, count)
                .map_err(io::Error::other),
            Some(ApplicationAction::HalfPageDown(count)) => {
                self.scroll_half_page(true, count).map_err(io::Error::other)
            }
            Some(ApplicationAction::Window(action, count)) => self.window_action(action, count),
            None => Ok(()),
        };
        if let Err(error) = result {
            self.fail(error);
        }
    }

    fn scroll_half_page(&mut self, down: bool, count: usize) -> Result<(), vex_editor::Error> {
        let cursor_line = |editor: &Editor| -> Result<usize, vex_core::Error> {
            let text = editor.document().text();
            let cursor = if editor.mode() == Mode::Insert {
                editor.selections().primary().head
            } else {
                vex_core::motion::cursor(text, editor.selections().primary())?
            };
            Ok(text.char_to_line(cursor.0))
        };
        let distance = (usize::from(self.active_size().1).saturating_sub(1) / 2)
            .max(1)
            .saturating_mul(count);
        let before = cursor_line(&self.editor)?;
        self.editor
            .execute(if down { "move_down" } else { "move_up" }, distance)?;
        let after = cursor_line(&self.editor)?;
        // Move the view with the cursor, preserving its screen row away from
        // document edges. Drawing still enforces the normal visibility margin.
        self.viewport.top_line = if down {
            self.viewport
                .top_line
                .saturating_add(after.saturating_sub(before))
        } else {
            self.viewport
                .top_line
                .saturating_sub(before.saturating_sub(after))
        };
        Ok(())
    }

    fn open_search_prompt(&mut self) {
        if self.editor.search_prompt().is_none()
            && self
                .prompt
                .as_ref()
                .is_some_and(|p| matches!(p.kind, PromptKind::Search { .. }))
        {
            self.prompt = None;
        }
        if self.prompt.is_none()
            && let Some(operation) = self.editor.search_prompt()
        {
            self.keys.cancel(&mut self.editor);
            self.prompt = Some(ActivePrompt {
                input: Prompt::default(),
                register: self.editor.search_prompt_register().unwrap_or('/'),
                history_position: None,
                completion: prompt::Completion::default(),
                kind: PromptKind::Search {
                    operation,
                    viewport: self.viewport,
                },
            });
        }
    }

    fn preview_search(&mut self, prompt: &ActivePrompt) {
        if let PromptKind::Search { viewport, .. } = prompt.kind {
            self.viewport = viewport;
            if let Err(error) = self.editor.update_search(prompt.input.text()) {
                self.fail(error);
                return;
            }
            if self.editor.search_status() == Some(SearchStatus::NoMatch) {
                self.fail("no matches");
            }
        }
    }

    fn handle_prompt_key(&mut self, key: Key) {
        let mut prompt = self.prompt.take().unwrap();
        if let Some(name) = prompt.input.register_key(key) {
            if prompt.input.register_pending() {
                self.keys.cache_register_hints(&self.editor);
            }
            if let Some(name) = name {
                if let Some(kind) = vex_editor::ClipboardKind::from_register(name) {
                    self.prompt = Some(prompt);
                    self.begin_prompt_clipboard(kind);
                    return;
                }
                match self.editor.register_first(name) {
                    Ok(Some(value)) => {
                        prompt.input.insert(&value);
                        self.preview_search(&prompt);
                    }
                    Ok(None) => {}
                    Err(error) => self.fail(error),
                }
            }
            self.prompt = Some(prompt);
            return;
        }
        if matches!(prompt.kind, PromptKind::Command) {
            if matches!(key, Key::Tab | Key::BackTab) {
                prompt
                    .completion
                    .cycle(&mut prompt.input, key == Key::BackTab);
                self.prompt = Some(prompt);
                return;
            }
            if key == Key::Enter && prompt.completion.directory_selected(&prompt.input) {
                prompt.completion.recalculate(&prompt.input);
                self.prompt = Some(prompt);
                return;
            }
        }
        match key {
            Key::Escape | Key::Ctrl('c') => {
                self.keys.cancel(&mut self.editor);
                if let PromptKind::Search { viewport, .. } = prompt.kind {
                    match self.editor.execute("search_cancel", 1) {
                        Ok(()) => self.viewport = viewport,
                        Err(error) => self.fail(error),
                    }
                }
            }
            Key::Enter => {
                if prompt.input.text().is_empty()
                    && let Some(text) = self.prompt_history.last(prompt.register)
                {
                    prompt.input.insert(&text);
                    self.preview_search(&prompt);
                }
                if matches!(prompt.kind, PromptKind::Search { .. }) {
                    self.prompt_history
                        .push(prompt.register, prompt.input.text());
                }
                match prompt.kind {
                    PromptKind::Command => {
                        if let Err(error) = self.execute(prompt.input.text()) {
                            self.fail(error);
                        }
                    }
                    PromptKind::Search { viewport, .. } => {
                        let empty = self.editor.search_status() == Some(SearchStatus::Empty);
                        match self.editor.execute("search_accept", 1) {
                            Ok(()) => {
                                if empty {
                                    self.viewport = viewport;
                                }
                                if self.editor.search_pending() {
                                    self.prompt = Some(prompt);
                                }
                            }
                            Err(error) => {
                                self.fail(error);
                                if self.editor.search_prompt().is_some() {
                                    self.prompt = Some(prompt);
                                }
                            }
                        }
                    }
                }
            }
            Key::Up | Key::Ctrl('p') | Key::Down | Key::Ctrl('n') => {
                if let Some(text) = self.prompt_history.step(
                    prompt.register,
                    &mut prompt.history_position,
                    matches!(key, Key::Up | Key::Ctrl('p')),
                ) {
                    prompt.input.replace(&text);
                    self.preview_search(&prompt);
                }
                self.prompt = Some(prompt);
            }
            _ => {
                if prompt.input.handle(key) {
                    self.preview_search(&prompt);
                } else if matches!(prompt.kind, PromptKind::Search { .. })
                    && self.editor.search_status() == Some(SearchStatus::NoMatch)
                {
                    self.fail("no matches");
                } else if let Some(error) = self.editor.search_error().cloned() {
                    self.fail(error);
                }
                self.prompt = Some(prompt);
            }
        }
    }

    fn clear_message(&mut self) {
        self.message.clear();
        self.error = false;
    }
    fn fail(&mut self, error: impl std::fmt::Display) {
        self.message = error.to_string();
        self.error = true;
    }
}

pub struct Command {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub documentation: &'static str,
    pub run: fn(&mut App, &str, bool) -> io::Result<()>,
    pub completion: ArgumentCompletion,
}

/// Argument suggestions supplied by the command completion worker.
#[derive(Clone, Copy)]
pub enum ArgumentCompletion {
    None,
    Path,
    Language,
    Command,
    AutoCompletion,
}

macro_rules! commands {
    (@completion) => { ArgumentCompletion::None };
    (@completion $kind:ident) => { ArgumentCompletion::$kind };
    ($($(#[doc = $doc:literal])+ fn $function:ident($app:ident, $arg:ident, $force:ident) [$name:literal $(, $alias:literal)*] $(complete $completion:ident)? $body:block)+) => {
        $( $(#[doc = $doc])+ pub fn $function($app: &mut App, $arg: &str, $force: bool) -> io::Result<()> $body )+
        pub static COMMANDS: &[Command] = &[
            $(Command { name: $name, aliases: &[$($alias,)*], documentation: concat!($($doc, "\n",)+), run: $function, completion: commands!(@completion $($completion)?) },)+
        ];
    };
}

commands! {
    /// Open PATH in the current pane, retaining unsaved buffers and recording the previous location.
    fn open_file(app, argument, force) ["open", "o", "edit", "e"] complete Path {
        if force || argument.is_empty() { return Err(io::Error::other("open requires a path and does not accept !")); }
        app.open_window_from_picker(Path::new(argument))
    }
    /// Reload the current file from disk as one undo step. Unsaved edits require :reload!; failed reads keep the buffer intact.
    fn reload_file(app, argument, force) ["reload"] {
        if !argument.is_empty() { return Err(io::Error::other("reload takes no arguments")); }
        app.reload_current_file(force)
    }

    /// Stage the selected whole file from disk. Save unsaved buffers first; hunk staging is not yet supported.
    fn git_stage(app, argument, force) ["git_stage"] {
        if !argument.is_empty() || force { return Err(io::Error::other("git_stage takes no arguments")); }
        app.change_git_index(true)
    }
    /// Unstage the selected whole file, preserving working files.
    fn git_unstage(app, argument, force) ["git_unstage"] {
        if !argument.is_empty() || force { return Err(io::Error::other("git_unstage takes no arguments")); }
        app.change_git_index(false)
    }
    /// Open or resume this repository's commit message in an editor pane. Ctrl-c Ctrl-c submits; Ctrl-c Ctrl-k returns and retains the draft.
    fn git_commit(app, argument, force) ["git_commit"] {
        if !argument.is_empty() || force { return Err(io::Error::other("git_commit takes no arguments")); }
        app.begin_git_commit()
    }
    /// Commit the current index using the active commit message. Hooks and signing follow Git configuration; failures retain the draft.
    fn git_commit_submit(app, argument, force) ["git_commit_submit"] {
        if !argument.is_empty() || force { return Err(io::Error::other("git_commit_submit takes no arguments")); }
        app.submit_git_commit()
    }
    /// Return to Git status, retaining this commit draft and undo history for the session. An already submitted commit continues in the background.
    fn git_commit_cancel(app, argument, force) ["git_commit_cancel"] {
        if !argument.is_empty() || force { return Err(io::Error::other("git_commit_cancel takes no arguments")); }
        app.cancel_git_commit()
    }
    /// Open the repository status view, preserving the document behind it.
    fn git_status(app, argument, force) ["git_status"] {
        if !argument.is_empty() || force { return Err(io::Error::other("git_status takes no arguments")); }
        app.open_git_status()
    }
    /// Expand or collapse the selected Git section, file, or hunk.
    fn git_toggle(app, argument, force) ["git_toggle"] {
        if !argument.is_empty() || force { return Err(io::Error::other("git_toggle takes no arguments")); }
        app.toggle_git_section()
    }
    /// Open the selected Git file at the reviewed change, protecting unsaved buffers.
    fn git_visit(app, argument, force) ["git_visit"] {
        if !argument.is_empty() || force { return Err(io::Error::other("git_visit takes no arguments")); }
        app.visit_git_change()
    }
    /// Refresh visible repository status views in the background, preserving navigation and folds.
    fn git_refresh(app, argument, force) ["git_refresh"] {
        if !argument.is_empty() || force { return Err(io::Error::other("git_refresh takes no arguments")); }
        app.refresh_status(); Ok(())
    }
    /// Return from Git status to the document retained in this pane.
    fn git_close(app, argument, force) ["git_close"] {
        if !argument.is_empty() || force { return Err(io::Error::other("git_close takes no arguments")); }
        app.close_git_status(); Ok(())
    }
    /// Split vertically, optionally opening PATH in the new right-hand window.
    fn vertical_split(app, argument, force) ["vsplit", "vs"] complete Path {
        if force { return Err(io::Error::other("vsplit does not accept !")); }
        app.split_with_path(true, argument)
    }

    /// Split horizontally, optionally opening PATH in the new lower window.
    fn horizontal_split(app, argument, force) ["hsplit", "hs", "split", "sp"] complete Path {
        if force { return Err(io::Error::other("hsplit does not accept !")); }
        app.split_with_path(false, argument)
    }

    /// Keep only the current window, retaining all buffers and unsaved edits. Accepts ! for compatibility.
    fn only(app, argument, force) ["only"] {
        if !argument.is_empty() { return Err(io::Error::other("only takes no arguments")); }
        app.only_window(force)
    }

    /// Quit all windows. Use ! to discard unsaved buffers.
    fn quit_all(app, argument, force) ["quit-all", "qa", "qall"] {
        if !argument.is_empty() { return Err(io::Error::other("quit-all takes no arguments")); }
        app.quit_all(force)
    }

    /// Show or configure automatic completion for this session: on, off, delay MS (0..10000), or min-length N (1..256). Defaults to on, 100 ms, and 2 characters; Ctrl-x always remains available.
    fn auto_completion(app, argument, force) ["auto-completion"] complete AutoCompletion {
        if force { return Err(io::Error::other("auto-completion does not accept !")); }
        app.configure_completion(argument)
    }

    /// Restart the configured language server for the current file after an error or configuration change.
    fn restart_lsp(app, argument, force) ["lsp-restart"] {
        if !argument.is_empty() || force { return Err(io::Error::other("lsp-restart takes no arguments")); }
        app.restart_language_server();
        Ok(())
    }

    /// Write the buffer atomically. Accepts an optional path; ! permits overwriting external changes or an existing destination.
    fn write_file(app, argument, force) ["write", "w"] complete Path {
        if app.git_write.drafts.contains_key(&app.editor.document().id()) {
            return Err(io::Error::other("commit drafts are retained in memory; Ctrl-c Ctrl-c commits, Ctrl-c Ctrl-k returns"));
        }
        app.check_save_target(argument)?;
        app.editor.finish_undo_group();
        let bytes = app.files.save(app.editor.document(), if argument.is_empty() { None } else { Some(Path::new(argument)) }, force)?;
        app.editor.set_display_name(app.files.display_name());
        if app.automatic_language {
            let language = Language::detect(app.files.path(), app.editor.document().text());
            if language != app.editor.language() { app.editor.set_language(language); }
        }
        app.message = format!("wrote {bytes} bytes");
        app.language.saved += 1;
        app.language.saved_snapshot = Some(app.editor.document().snapshot());
        app.refresh_git();
        app.refresh_status();
        Ok(())
    }

    /// Close the current window, retaining its buffers. Quitting the last window protects all unsaved buffers unless :q! is used.
    fn quit(app, argument, force) ["quit", "q"] {
        if !argument.is_empty() { return Err(io::Error::other("quit takes no arguments")); }
        app.close_window(force)
    }

    /// Close the current buffer in all panes. Unsaved changes require :bc!; closing the final buffer creates a scratch buffer.
    fn buffer_close(app, argument, force) ["buffer-close", "bc", "bclose"] {
        if !argument.is_empty() { return Err(io::Error::other("buffer-close takes no arguments")); }
        app.close_buffer(force)
    }

    /// Switch to the next loaded buffer in opening order, wrapping around.
    fn buffer_next(app, argument, force) ["buffer-next", "bn", "bnext"] {
        if !argument.is_empty() || force { return Err(io::Error::other("buffer-next takes no arguments or !")); }
        app.buffer_action(vex_editor::BufferAction::Next, 1)
    }

    /// Switch to the previous loaded buffer in opening order, wrapping around.
    fn buffer_previous(app, argument, force) ["buffer-previous", "bp", "bprevious"] {
        if !argument.is_empty() || force { return Err(io::Error::other("buffer-previous takes no arguments or !")); }
        app.buffer_action(vex_editor::BufferAction::Previous, 1)
    }

    /// Write and quit after a successful save. Accepts the same path and ! options as :write.
    fn write_quit(app, argument, force) ["write-quit", "wq", "x"] complete Path {
        write_file(app, argument, force)?;
        quit(app, "", false)
    }

    /// Show or set the language by its registry name or alias; text disables language support, and auto detects the filename or shebang.
    fn set_language(app, argument, force) ["language", "lang"] complete Language {
        if force { return Err(io::Error::other("language does not accept !")); }
        if !argument.is_empty() {
            let language = match argument {
                "text" => None,
                "auto" => Language::detect(app.files.path(), app.editor.document().text()),
                _ => Some(Language::from_name(argument).ok_or_else(|| io::Error::other(format!("supported languages: {}, text, auto", Language::ALL.iter().map(|language| language.name()).collect::<Vec<_>>().join(", "))))?),
            };
            app.automatic_language = argument == "auto";
            app.editor.set_language(language);
        }
        app.message = format!("language: {}{}", app.editor.language().map_or("text", Language::name), if app.automatic_language { " (auto)" } else { "" });
        Ok(())
    }

    /// Show basic keys, or the documentation for a named editing or file command.
    fn help(app, argument, force) ["help", "h"] complete Command {
        if force { return Err(io::Error::other("help does not accept !")); }
        app.message = if argument.is_empty() {
            "i/a insert  Esc normal  v select  hjkl/wbe move  /? search  n/N next/previous  u/U undo/redo  :w [PATH] write  :q[!] quit  :help COMMAND".into()
        } else if let Some(command) = vex_editor::commands::find(argument) {
            command.description().into()
        } else if let Some(command) = COMMANDS.iter().find(|c| c.name == argument || c.aliases.contains(&argument)) {
            command.documentation.trim().into()
        } else { return Err(io::Error::other("unknown command")); };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    pub(super) fn press(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.handle(Event::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )));
        }
    }
    pub(super) fn key(app: &mut App, code: KeyCode) {
        app.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    pub(super) fn draw(app: &mut App) -> Frame {
        let mut frame = Frame::default();
        frame.reset(app.size.0, app.size.1).unwrap();
        app.paint(&mut frame).unwrap();
        frame
    }

    #[test]
    fn half_page_keys_scroll_with_the_cursor_use_counts_and_follow_resize() {
        let source = "abcdefgh\n".repeat(100);
        let mut app = App::from_document(Document::from(source.as_str()), (80, 12));
        press(&mut app, "20j3l");
        let original = draw(&mut app).cursor.unwrap();
        let viewport = app.viewport;
        let selections = app.editor.selections().clone();
        let revision = app.editor.document().revision();
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(app.editor.selections().primary().start().0, 25 * 9 + 3);
        assert_eq!(app.viewport.top_line, viewport.top_line + 5);
        assert_eq!(draw(&mut app).cursor.unwrap(), original);
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(app.editor.selections(), &selections);
        assert_eq!(app.viewport, viewport);
        press(&mut app, "3");
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(app.editor.selections().primary().start().0, 35 * 9 + 3);
        assert_eq!(app.keys.count(), None);
        app.handle(Event::Resize(80, 22));
        draw(&mut app);
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(app.editor.selections().primary().start().0, 25 * 9 + 3);
        assert_eq!(app.editor.document().revision(), revision);
        assert!(!app.is_dirty());
    }

    #[test]
    fn half_pages_preserve_desired_columns_and_selection_anchors_and_clamp_at_edges() {
        use vex_core::{CharOffset, Selection, SelectionSet};
        let mut app = App::from_document(Document::from("a\t界z\nx\nx\nx\n123456z"), (80, 6));
        let original = SelectionSet::single(Selection::new(CharOffset(3), CharOffset(4)));
        app.editor.set_selections(original.clone()).unwrap();
        app.execute("page_cursor_half_down").unwrap();
        app.execute("page_cursor_half_down").unwrap();
        assert_eq!(app.editor.selections().primary().start().0, 17);
        assert_eq!(app.editor.display_column(CharOffset(17)).unwrap(), 6);
        app.execute("page_cursor_half_up").unwrap();
        app.execute("page_cursor_half_up").unwrap();
        assert_eq!(app.editor.selections(), &original);
        press(&mut app, "v");
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(app.editor.selections().primary().anchor, CharOffset(3));
        assert_eq!(app.editor.mode(), Mode::Select);
        assert!(app.editor.selections().primary().head > CharOffset(4));
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(app.editor.selections(), &original);
        key(&mut app, KeyCode::Esc);
        press(&mut app, &usize::MAX.to_string());
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(app.editor.selections().primary().start().0, 17);
        draw(&mut app);
        press(&mut app, &usize::MAX.to_string());
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(app.editor.selections(), &original);
        draw(&mut app);
        assert_eq!(app.viewport.top_line, 0);
        app.handle(Event::Resize(1, 1));
        app.execute("page_cursor_half_down").unwrap();
        assert_eq!(
            app.editor
                .document()
                .text()
                .char_to_line(app.editor.selections().primary().start().0),
            1
        );
        draw(&mut app);
        app.execute("help page_cursor_half_down").unwrap();
        assert!(app.message.contains("half the visible text height"));
    }

    #[test]
    fn background_surround_replacement_previews_accepts_literal_colons_and_clears_failed_input() {
        let mut app = App::from_document(Document::from("(abc)"), (60, 16));
        app.editor.set_background_search(true);
        press(&mut app, "lvmr(");
        let result = app.editor.take_search_job().unwrap().run().unwrap();
        app.handle_search_result(result);
        assert_eq!(app.keys.hints().unwrap().title, "Replace with a pair of");
        assert_eq!(app.editor.selections().ranges().len(), 2);
        draw(&mut app);
        press(&mut app, ":");
        assert!(app.prompt.is_none());
        assert_eq!(app.editor.document().text(), "(abc)");
        let result = app.editor.take_search_job().unwrap().run().unwrap();
        app.handle_search_result(result);
        assert_eq!(app.editor.document().text(), ":abc:");
        assert_eq!(app.editor.mode(), Mode::Normal);
        assert!(app.is_dirty());
        press(&mut app, "mr(");
        let result = app.editor.take_search_job().unwrap().run().unwrap();
        app.handle_search_result(result);
        assert!(app.keys.hints().is_none());
        assert!(app.message.contains("surround pair not found"));
        press(&mut app, ":");
        assert!(app.prompt.is_some());
    }

    #[test]
    fn pending_surround_scans_and_previews_cancel_without_leaving_select_mode() {
        for completed in [false, true] {
            let mut app = App::from_document(Document::from("(abc)"), (60, 16));
            app.editor.set_background_search(true);
            press(&mut app, "lv");
            let before = app.editor.selections().clone();
            press(&mut app, "mr(");
            let result = app.editor.take_search_job().unwrap().run().unwrap();
            let late = if completed {
                app.handle_search_result(result);
                None
            } else {
                Some(result)
            };
            app.handle(Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            )));
            if let Some(result) = late {
                assert!(!app.handle_search_result(result));
            }
            assert_eq!(app.editor.mode(), Mode::Select);
            assert_eq!(app.editor.selections(), &before);
            assert!(app.keys.hints().is_none());
            assert_eq!(app.editor.document().text(), "(abc)");
        }
    }

    #[test]
    fn background_preview_cancel_and_external_edits_do_not_leave_stale_prompts() {
        let source = format!("origin\n{}{}target", "short\n".repeat(20), "x".repeat(120));
        let mut app = App::from_document(Document::from(source.as_str()), (30, 8));
        app.editor.set_background_search(true);
        draw(&mut app);
        let viewport = app.viewport;
        let selections = app.editor.selections().clone();
        press(&mut app, "/target");
        assert!(draw(&mut app).row_text(6).contains("searching..."));
        let result = app.editor.take_search_job().unwrap().run().unwrap();
        app.handle_search_result(result);
        draw(&mut app);
        assert!(app.viewport.top_line > viewport.top_line);
        assert!(app.viewport.left_column > viewport.left_column);
        key(&mut app, KeyCode::Esc);
        draw(&mut app);
        assert_eq!(app.viewport, viewport);
        assert_eq!(app.editor.selections(), &selections);
        press(&mut app, "/target");
        let job = app.editor.take_search_job().unwrap();
        app.editor.execute("delete_selection", 1).unwrap();
        assert!(job.run().is_none());
        draw(&mut app);
        assert!(app.prompt.is_none());
    }

    #[test]
    fn search_preview_scrolls_and_cancel_restores_selections_and_both_axes() {
        use vex_core::{CharOffset, Selection, SelectionSet};
        let source = format!(
            "{}\n{}{}needle\n",
            "x".repeat(100),
            "short\n".repeat(40),
            "y".repeat(140)
        );
        let mut app = App::from_document(Document::from(source.as_str()), (30, 8));
        let selections = SelectionSet::new(
            vec![
                Selection::new(CharOffset(25), CharOffset(20)),
                Selection::new(CharOffset(80), CharOffset(85)),
            ],
            1,
        )
        .unwrap();
        app.editor.set_selections(selections.clone()).unwrap();
        app.editor.execute("select_mode", 1).unwrap();
        draw(&mut app);
        let viewport = app.viewport;
        assert!(viewport.left_column > 0);
        press(&mut app, "/needle");
        let frame = draw(&mut app);
        assert!(frame.row_text(7).starts_with("/needle"));
        assert!(app.viewport.top_line > viewport.top_line);
        assert!(app.viewport.left_column > viewport.left_column);
        assert_eq!(app.editor.selections().ranges().len(), 3);
        key(&mut app, KeyCode::Esc);
        draw(&mut app);
        assert_eq!(app.viewport, viewport);
        assert_eq!(app.editor.selections(), &selections);
        assert_eq!(app.editor.mode(), Mode::Select);
        assert!(app.prompt.is_none());
        assert!(!app.is_dirty());
    }

    #[test]
    fn search_events_accept_repeat_reverse_wrap_and_edit_the_selected_match() {
        use vex_core::CharOffset;
        let mut app = App::from_document(Document::from("cat bat cat cat"), (50, 8));
        press(&mut app, "/cat");
        assert_eq!(
            app.editor.selections().primary().range(),
            CharOffset(8)..CharOffset(11)
        );
        key(&mut app, KeyCode::Enter);
        press(&mut app, "2n");
        assert_eq!(app.editor.selections().primary().start(), CharOffset(0));
        press(&mut app, "nN");
        assert_eq!(app.editor.selections().primary().start(), CharOffset(0));
        press(&mut app, "?cat");
        assert!(draw(&mut app).row_text(7).starts_with("?cat"));
        key(&mut app, KeyCode::Enter);
        press(&mut app, "n");
        assert_eq!(app.editor.selections().primary().start(), CharOffset(0));
        press(&mut app, "d");
        assert_eq!(app.editor.document().text(), " bat cat cat");
        press(&mut app, "u");
        assert!(!app.is_dirty());
    }

    #[test]
    fn unmatched_search_reports_failure_and_backspace_recovers_from_the_origin() {
        use vex_core::CharOffset;
        let mut app = App::from_document(Document::from("x cat cater"), (70, 8));
        let original = app.editor.selections().clone();
        press(&mut app, "/cater");
        assert_eq!(app.editor.selections().primary().start(), CharOffset(6));
        key(&mut app, KeyCode::Backspace);
        key(&mut app, KeyCode::Backspace);
        assert_eq!(app.editor.selections().primary().start(), CharOffset(2));
        press(&mut app, "z");
        assert_eq!(app.editor.selections(), &original);
        let frame = draw(&mut app);
        assert!(frame.row_text(6).contains("no matches"));
        assert_eq!(frame.style_at(0, 7), Some(crate::screen::Style::Error));
        key(&mut app, KeyCode::Enter);
        assert!(app.prompt.is_some());
        key(&mut app, KeyCode::Left);
        assert!(app.error);
        key(&mut app, KeyCode::Delete);
        assert!(!app.error);
        assert_eq!(app.editor.selections().primary().start(), CharOffset(2));
        key(&mut app, KeyCode::Enter);
        assert!(app.prompt.is_none());
        press(&mut app, "n");
        assert_eq!(app.editor.selections().primary().start(), CharOffset(6));
    }

    #[test]
    fn search_prompt_paste_unicode_resize_empty_accept_and_control_c_are_safe() {
        let query = "界e\u{301}🦀".repeat(15);
        let source = format!("before\n{query}\nafter");
        let mut app = App::from_document(Document::from(source.as_str()), (12, 5));
        let original = app.editor.selections().clone();
        press(&mut app, "/");
        app.handle(Event::Paste(format!("{query}\r\n")));
        assert_eq!(app.prompt.as_ref().unwrap().input.text(), query);
        assert_eq!(app.editor.search_status(), Some(SearchStatus::Match));
        let frame = draw(&mut app);
        assert!(frame.cursor.unwrap().x < 12);
        for size in [(1, 1), (0, 0), (80, 24)] {
            app.handle(Event::Resize(size.0, size.1));
            draw(&mut app);
        }
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(app.editor.selections(), &original);
        assert!(app.prompt.is_none());
        press(&mut app, "/");
        key(&mut app, KeyCode::Enter);
        assert!(app.prompt.is_none());
        assert_eq!(app.editor.selections(), &original);
        assert!(!app.should_quit());
        assert!(!app.is_dirty());
        press(&mut app, "i/?nN");
        assert!(app.editor.document().text().to_string().starts_with("/?nN"));
    }

    #[test]
    fn control_c_comments_document_text_and_cancels_prefixes_and_prompts() {
        let mut app = App::from_document(Document::from("let x = 1;\n"), (80, 24));
        app.editor.set_language(Some(Language::Rust));
        let control_c = || Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        app.handle(control_c());
        assert_eq!(app.editor.document().text(), "// let x = 1;\n");
        press(&mut app, "r");
        app.handle(control_c());
        assert_eq!(app.editor.document().text(), "// let x = 1;\n");
        assert!(app.keys.pending_keys().is_empty());
        press(&mut app, ":write");
        app.handle(control_c());
        assert!(app.prompt.is_none());
        app.handle(control_c());
        assert_eq!(app.editor.document().text(), "let x = 1;\n");
    }

    #[test]
    fn search_commands_open_prompts_through_colon_or_custom_bindings() {
        use vex_editor::Keymap;
        let mut app = App::from_document(Document::from("one two one"), (40, 6));
        press(&mut app, ":search_backward");
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.prompt.as_ref().unwrap().prefix(), "?");
        key(&mut app, KeyCode::Esc);
        let mut map = Keymap::empty();
        map.bind(Mode::Normal, vec![Key::Char('s')], "search_forward")
            .unwrap();
        app.keys = KeyHandler::new(map);
        press(&mut app, "sone");
        assert_eq!(app.editor.search_status(), Some(SearchStatus::Match));
        key(&mut app, KeyCode::Enter);
        app.execute("help search_next").unwrap();
        assert!(app.message.contains("Search forward"));
    }

    #[test]
    fn selection_prompts_draw_labels_and_recover_from_invalid_regexes() {
        let mut app = App::from_document(Document::from("one two three"), (40, 6));
        press(&mut app, "%s[");
        assert_eq!(app.editor.search_status(), Some(SearchStatus::Invalid));
        assert!(draw(&mut app).row_text(5).starts_with("select: ["));
        key(&mut app, KeyCode::Left);
        assert!(app.error);
        key(&mut app, KeyCode::Enter);
        assert!(app.prompt.is_some());
        key(&mut app, KeyCode::Delete);
        press(&mut app, "\\w+");
        assert!(!app.error);
        assert_eq!(app.editor.selections().ranges().len(), 3);
        for width in [1, 6, 9, 40] {
            app.handle(Event::Resize(width, 6));
            assert!(draw(&mut app).cursor.unwrap().x < width);
        }
        key(&mut app, KeyCode::Enter);
        press(&mut app, "Ko");
        assert!(draw(&mut app).row_text(5).starts_with("keep: o"));
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.editor.selections().ranges().len(), 2);
        press(&mut app, "d");
        assert_eq!(app.editor.document().text(), "  three");
        press(&mut app, "u%S ");
        assert!(draw(&mut app).row_text(5).starts_with("split:  "));
        assert_eq!(app.editor.selections().ranges().len(), 3);
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.editor.selections().ranges().len(), 1);
        assert!(!app.is_dirty());
    }

    #[test]
    fn open_edit_save_and_quit_through_real_events() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file");
        std::fs::write(&path, "hello world\r\n").unwrap();
        let mut app = App::open(Some(&path), (80, 24)).unwrap();
        press(&mut app, "wci");
        app.handle(Event::Paste("🦀".into()));
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.editor.document().text(), "i🦀\r\nworld\r\n");
        assert!(app.is_dirty());
        press(&mut app, ":q");
        key(&mut app, KeyCode::Enter);
        assert!(!app.should_quit());
        assert!(app.error);
        press(&mut app, ":wq");
        key(&mut app, KeyCode::Enter);
        assert!(app.should_quit());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "i🦀\r\nworld\r\n");
    }

    #[test]
    fn prompt_paths_with_spaces_help_and_failed_writes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("new file.txt");
        let mut app = App::open(None, (80, 24)).unwrap();
        press(&mut app, "ihello");
        key(&mut app, KeyCode::Esc);
        assert!(app.execute("wq").is_err());
        assert!(!app.should_quit());
        app.execute(&format!("w {}", path.display())).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        assert!(!app.is_dirty());
        app.execute("help move_word_forward").unwrap();
        assert_eq!(
            app.message,
            vex_editor::commands::find("move_word_forward")
                .unwrap()
                .description()
        );
        assert!(app.execute("q unwanted").is_err());
        app.execute("q").unwrap();
        assert!(app.should_quit());
    }

    #[test]
    fn paste_in_normal_or_prompt_modes_cannot_execute_commands() {
        let mut app = App::open(None, (80, 24)).unwrap();
        app.handle(Event::Paste(":q!\r\n".into()));
        assert!(!app.should_quit());
        assert_eq!(app.editor.document().text(), "");
        press(&mut app, ":");
        app.handle(Event::Paste("q!\r\n".into()));
        assert!(!app.should_quit());
        key(&mut app, KeyCode::Esc);
        assert!(app.prompt.is_none());
        assert!(!app.should_quit());
    }

    #[test]
    fn a_colon_in_insert_mode_is_text_and_resize_is_safe() {
        let mut app = App::open(None, (80, 24)).unwrap();
        press(&mut app, "i:w");
        assert_eq!(app.editor.document().text(), ":w");
        app.handle(Event::Resize(1, 1));
        let mut frame = Frame::default();
        frame.reset(1, 1).unwrap();
        app.paint(&mut frame).unwrap();
        assert_eq!(app.size(), (1, 1));
    }

    #[test]
    fn character_find_arguments_bypass_prompts_and_cancellation_keeps_select_mode() {
        let mut app = App::from_document(Document::from("a:b\tc\r\nnext"), (80, 24));
        press(&mut app, "f:");
        assert!(app.prompt.is_none());
        assert_eq!(
            app.editor.selections().primary().end(),
            vex_core::CharOffset(2)
        );
        press(&mut app, "f");
        key(&mut app, KeyCode::Tab);
        assert_eq!(
            app.editor.selections().primary().end(),
            vex_core::CharOffset(4)
        );
        press(&mut app, "f");
        key(&mut app, KeyCode::Enter);
        assert_eq!(
            app.editor.selections().primary().end(),
            vex_core::CharOffset(7)
        );
        press(&mut app, "v2f");
        let before = app.editor.selections().clone();
        app.handle(Event::Resize(81, 25));
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(app.editor.mode(), Mode::Select);
        assert_eq!(app.editor.selections(), &before);
        assert!(app.keys.pending_keys().is_empty());
        assert_eq!(app.keys.count(), None);
        press(&mut app, ":");
        assert!(app.prompt.is_some());
    }

    #[test]
    fn saving_during_insert_keeps_the_savepoint_reachable_by_undo_and_redo() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file");
        std::fs::write(&path, "").unwrap();
        let mut app = App::open(Some(&path), (80, 24)).unwrap();
        press(&mut app, "ihello");
        app.execute("w").unwrap();
        assert!(!app.is_dirty());
        assert_eq!(app.editor.mode(), Mode::Insert);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        press(&mut app, " world");
        assert_eq!(app.editor.document().undo_depth(), 2);
        app.editor.execute("undo", 1).unwrap();
        assert_eq!(app.editor.document().text(), "hello");
        assert!(!app.is_dirty());
        app.editor.execute("undo", 1).unwrap();
        assert_eq!(app.editor.document().text(), "");
        assert!(app.is_dirty());
        app.editor.execute("redo", 1).unwrap();
        assert!(!app.is_dirty());
        app.editor.execute("redo", 1).unwrap();
        assert!(app.is_dirty());
        app.editor.execute("undo", 1).unwrap();
        press(&mut app, "!");
        assert_eq!(app.editor.document().redo_depth(), 0);
        app.editor.execute("undo", 1).unwrap();
        assert!(!app.is_dirty());
        app.execute("q").unwrap();
        assert!(app.should_quit());
    }

    #[test]
    fn prompt_register_insertion_uses_the_first_fragment_at_the_cursor_without_submitting() {
        use std::sync::Arc;
        let mut app = App::from_document(Document::from("abc"), (80, 24));
        app.editor
            .set_register(
                'a',
                Arc::from([Arc::from("界e\u{301}\n"), Arc::from("unused")]),
            )
            .unwrap();
        press(&mut app, ":xy");
        key(&mut app, KeyCode::Home);
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('r'),
            KeyModifiers::CONTROL,
        )));
        assert!(app.prompt.as_ref().unwrap().input.register_pending());
        let mut frame = Frame::default();
        frame.reset(80, 24).unwrap();
        app.paint(&mut frame).unwrap();
        assert!((0..24).any(|row| frame.row_text(row).contains("Insert register")));
        press(&mut app, "a");
        assert_eq!(app.prompt.as_ref().unwrap().input.text(), "界e\u{301}xy");
        assert_eq!(
            app.prompt.as_ref().unwrap().input.cursor(),
            "界e\u{301}".len()
        );
        assert_eq!(app.editor.document().text(), "abc");
        assert!(!app.should_quit());
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('r'),
            KeyModifiers::CONTROL,
        )));
        key(&mut app, KeyCode::Esc);
        assert!(app.prompt.is_some());
        assert!(!app.prompt.as_ref().unwrap().input.register_pending());
        key(&mut app, KeyCode::Esc);
        assert!(app.prompt.is_none());
    }

    #[test]
    fn register_insertion_updates_search_previews() {
        use std::sync::Arc;
        let mut app = App::from_document(Document::from("find cat"), (80, 24));
        app.editor
            .set_register('a', Arc::from([Arc::from("cat")]))
            .unwrap();
        press(&mut app, "/");
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('r'),
            KeyModifiers::CONTROL,
        )));
        press(&mut app, "a");
        assert_eq!(app.prompt.as_ref().unwrap().input.text(), "cat");
        assert_eq!(
            app.editor.selections().primary().range(),
            vex_core::CharOffset(5)..vex_core::CharOffset(8)
        );
        key(&mut app, KeyCode::Enter);
        assert!(app.prompt.is_none());
        assert_eq!(app.editor.document().text(), "find cat");
    }

    #[test]
    fn file_and_command_registers_follow_save_as_switching_and_prompt_insertion() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.txt");
        let second = directory.path().join("second.txt");
        let mut app = App::from_document(Document::from("abc"), (80, 24));
        assert_eq!(
            app.editor.register_first('%').unwrap().as_deref(),
            Some("[scratch]")
        );
        let save = format!("w {}", first.display());
        app.execute(&save).unwrap();
        assert_eq!(
            app.editor.register_first('%').unwrap().unwrap().as_ref(),
            first.to_str().unwrap()
        );
        assert_eq!(
            app.editor.register_first(':').unwrap().as_deref(),
            Some(save.as_str())
        );
        app.execute(&format!("w {}", second.display())).unwrap();
        assert_eq!(
            app.editor.register_first('%').unwrap().unwrap().as_ref(),
            second.to_str().unwrap()
        );
        app.execute(&format!("vsplit {}", first.display())).unwrap();
        assert_eq!(
            app.editor.register_first('%').unwrap().unwrap().as_ref(),
            first.to_str().unwrap()
        );
        app.execute("jump_view_left").unwrap();
        assert_eq!(
            app.editor.register_first('%').unwrap().unwrap().as_ref(),
            second.to_str().unwrap()
        );
        let ctrl_r = || Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        press(&mut app, ":");
        app.handle(ctrl_r());
        press(&mut app, ":");
        assert_eq!(app.prompt.as_ref().unwrap().input.text(), "jump_view_left");
        key(&mut app, KeyCode::Esc);
        press(&mut app, "i");
        app.handle(ctrl_r());
        press(&mut app, "%");
        assert_eq!(app.editor.mode(), Mode::Insert);
        assert_eq!(
            app.editor.document().text().to_string(),
            format!("{}abc", second.display())
        );
        key(&mut app, KeyCode::Esc);
        press(&mut app, "u");
        assert_eq!(app.editor.document().text(), "abc");
    }

    #[test]
    fn large_insert_repeats_keep_resize_and_cancellation_live() {
        for cancel in [
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            let mut app = App::from_document(Document::from("a"), (80, 24));
            app.editor.set_deferred_repeat(true);
            press(&mut app, "aX");
            key(&mut app, KeyCode::Esc);
            press(&mut app, "1000000.");
            assert!(app.input_waiting());
            assert!(app.advance_repeat());
            assert!(app.editor.document().text().len_chars() > 2);
            app.handle(Event::Resize(50, 12));
            assert_eq!(app.size(), (50, 12));
            draw(&mut app);
            assert!(app.editor.repeat_pending());
            app.handle(Event::Key(cancel));
            assert!(!app.input_waiting());
            assert_eq!(app.editor.mode(), Mode::Normal);
            press(&mut app, "u");
            assert_eq!(app.editor.document().text(), "aX");
            press(&mut app, ".");
            while app.editor.repeat_pending() {
                app.advance_repeat();
            }
            assert_eq!(app.editor.document().text(), "aXX");
        }
    }

    #[test]
    fn control_s_creates_undo_and_jump_checkpoints_without_saving() {
        let mut app = App::from_document(Document::from("abc def"), (80, 24));
        let control_s = || Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        let selection = vex_core::SelectionSet::new(
            vec![
                vex_core::Selection::new(vex_core::CharOffset(0), vex_core::CharOffset(2)),
                vex_core::Selection::new(vex_core::CharOffset(7), vex_core::CharOffset(4)),
            ],
            1,
        )
        .unwrap();
        app.editor.set_selections(selection.clone()).unwrap();
        app.handle(control_s());
        assert!(!app.error, "{}", app.message);
        press(&mut app, ",gg");
        app.execute("jump_back").unwrap();
        app.take_lsp_update(); // Navigation works with language services disabled.
        assert_eq!(app.editor.selections(), &selection);
        press(&mut app, ",ihello");
        app.handle(control_s());
        assert!(app.is_dirty());
        assert_eq!(app.editor.mode(), Mode::Insert);
        assert!(!app.error, "{}", app.message);
        press(&mut app, " world");
        app.editor.execute("undo", 1).unwrap();
        assert_eq!(app.editor.document().text(), "abc hellodef");
        app.editor.execute("undo", 1).unwrap();
        assert_eq!(app.editor.document().text(), "abc def");
    }

    #[test]
    fn failed_save_closes_typing_without_marking_the_buffer_saved() {
        let mut app = App::open(None, (80, 24)).unwrap();
        press(&mut app, "iab");
        assert!(app.execute("wq").is_err());
        assert!(!app.should_quit());
        assert!(app.is_dirty());
        press(&mut app, "cd");
        assert_eq!(app.editor.document().undo_depth(), 2);
        app.editor.execute("undo", 1).unwrap();
        assert_eq!(app.editor.document().text(), "ab");
        assert!(app.is_dirty());
        app.editor.execute("undo", 1).unwrap();
        assert!(!app.is_dirty());
    }

    #[test]
    fn bracketed_paste_has_its_own_undo_step_between_typed_text() {
        let mut app = App::open(None, (80, 24)).unwrap();
        press(&mut app, "iab");
        app.handle(Event::Paste("e\u{301}🦀\r\n".into()));
        press(&mut app, "cd");
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.editor.document().undo_depth(), 3);
        press(&mut app, "u");
        assert_eq!(app.editor.document().text(), "abe\u{301}🦀\r\n");
        press(&mut app, "u");
        assert_eq!(app.editor.document().text(), "ab");
        press(&mut app, "u");
        assert!(!app.is_dirty());
        press(&mut app, "3U");
        assert_eq!(app.editor.document().text(), "abe\u{301}🦀\r\ncd");
    }

    #[test]
    fn language_registry_controls_open_shebangs_and_manual_aliases() {
        let directory = tempfile::tempdir().unwrap();
        for (name, source, expected) in [
            ("README.md", "# Title", Language::Markdown),
            (".bashrc", "echo hello", Language::Bash),
            ("script", "#!/usr/bin/env bash\necho hello", Language::Bash),
            ("main.ts", "const n: number = 1;", Language::TypeScript),
            ("view.tsx", "const view = <div />;", Language::Tsx),
        ] {
            let path = directory.path().join(name);
            std::fs::write(&path, source).unwrap();
            let mut app = App::open(Some(&path), (80, 24)).unwrap();
            assert_eq!(app.editor.language(), Some(expected));
            let revision = app.editor.document().revision();
            app.execute("language text").unwrap();
            app.execute("language auto").unwrap();
            assert_eq!(app.editor.language(), Some(expected));
            app.execute("language shell").unwrap();
            assert_eq!(app.editor.language(), Some(Language::Bash));
            app.execute("language ts").unwrap();
            assert_eq!(app.editor.language(), Some(Language::TypeScript));
            assert_eq!(app.editor.document().revision(), revision);
            assert!(!app.is_dirty());
        }
    }

    #[test]
    fn language_detection_follows_save_as_and_respects_manual_overrides() {
        let directory = tempfile::tempdir().unwrap();
        let rust = directory.path().join("file.rs");
        let text = directory.path().join("file.txt");
        std::fs::write(&rust, "fn main() {}").unwrap();
        let mut app = App::open(Some(&rust), (80, 24)).unwrap();
        assert_eq!(app.editor.language(), Some(Language::Rust));
        app.execute(&format!("w {}", text.display())).unwrap();
        assert_eq!(app.editor.language(), None);
        app.execute("language rust").unwrap();
        assert!(!app.automatic_language);
        app.execute("w").unwrap();
        assert_eq!(app.editor.language(), Some(Language::Rust));
        app.execute("lang auto").unwrap();
        assert!(app.automatic_language);
        assert_eq!(app.editor.language(), None);
        let revision = app.editor.document().revision();
        assert!(app.execute("lang unknown").is_err());
        assert!(app.execute("lang! rust").is_err());
        assert!(app.automatic_language);
        assert_eq!(app.editor.document().revision(), revision);
        assert!(!app.is_dirty());
        let mut scratch = App::open(None, (80, 24)).unwrap();
        assert_eq!(scratch.editor.language(), None);
        scratch.execute("language rust").unwrap();
        assert_eq!(scratch.editor.language(), Some(Language::Rust));
        scratch.execute("language text").unwrap();
        assert_eq!(scratch.editor.language(), None);
        scratch.execute("help language").unwrap();
        assert!(scratch.message.contains("registry name or alias"));
    }

    #[test]
    fn real_edit_events_recolor_rust_and_history_restores_highlights() {
        use crate::screen::Style;
        use vex_editor::Highlight;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file.rs");
        std::fs::write(&path, "fn main() {}\n").unwrap();
        let mut app = App::open(Some(&path), (40, 6)).unwrap();
        let mut frame = Frame::default();
        let mut paint_style = |app: &mut App| {
            frame.reset(40, 6).unwrap();
            app.paint(&mut frame).unwrap();
            frame.style_at(8, 0).unwrap()
        };
        assert_eq!(paint_style(&mut app), Style::Syntax(Highlight::Function));
        press(&mut app, "i//");
        key(&mut app, KeyCode::Esc);
        assert_eq!(paint_style(&mut app), Style::Syntax(Highlight::Comment));
        press(&mut app, "u");
        assert_eq!(paint_style(&mut app), Style::Syntax(Highlight::Function));
        press(&mut app, "U");
        assert_eq!(paint_style(&mut app), Style::Syntax(Highlight::Comment));
        app.execute("lang text").unwrap();
        assert_eq!(paint_style(&mut app), Style::Text);
    }
}
