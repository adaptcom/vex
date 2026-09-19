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
    Editor, Key, KeyHandler, Language, Mode, SearchCompletion, SearchDirection, SearchResult,
    SearchStatus,
};

mod language;
mod picker;

enum PromptKind {
    Command,
    Search {
        direction: SearchDirection,
        viewport: Viewport,
    },
}

struct ActivePrompt {
    input: Prompt,
    kind: PromptKind,
}

impl ActivePrompt {
    fn prefix(&self) -> char {
        match self.kind {
            PromptKind::Command => ':',
            PromptKind::Search {
                direction: SearchDirection::Forward,
                ..
            } => '/',
            PromptKind::Search {
                direction: SearchDirection::Backward,
                ..
            } => '?',
        }
    }
}

pub struct App {
    pub editor: Editor,
    files: FileState,
    keys: KeyHandler,
    viewport: Viewport,
    prompt: Option<ActivePrompt>,
    message: String,
    error: bool,
    quit: bool,
    size: (u16, u16),
    automatic_language: bool,
    language: language::State,
    picker: picker::State,
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
        editor.set_language(files.path().and_then(Language::from_path));
        Self {
            editor,
            files,
            keys: KeyHandler::default(),
            viewport: Viewport::default(),
            prompt: None,
            message: "i insert  / search  :w write  :q quit  :help".into(),
            error: false,
            quit: false,
            size,
            automatic_language: true,
            language: language::State::default(),
            picker: picker::State::default(),
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
        match self.editor.apply_search_result(result) {
            Ok(SearchCompletion::Ignored) => {
                if self.editor.search_direction().is_none()
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
                if self.editor.search_direction().is_none() {
                    self.prompt = None;
                }
                self.fail(error);
            }
        }
        true
    }

    /// Handle one event. Return whether the screen may have changed.
    pub fn handle(&mut self, event: Event) -> bool {
        if let Some(redraw) = self.handle_picker_input(&event) {
            return redraw;
        }
        if matches!(&event, Event::Paste(_))
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
                if let Some(mut prompt) = self.prompt.take() {
                    prompt.input.insert(&text);
                    self.preview_search(&prompt);
                    self.prompt = Some(prompt);
                } else if self.editor.mode() == Mode::Insert {
                    self.keys.cancel();
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
                    self.keys.cancel();
                    let count = usize::from(self.size.1).saturating_sub(3).max(1);
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
                        Key::Char(':')
                            if self.editor.mode() != Mode::Insert
                                && self.keys.pending_keys().is_empty() =>
                        {
                            self.keys.cancel();
                            self.prompt = Some(ActivePrompt {
                                input: Prompt::default(),
                                kind: PromptKind::Command,
                            });
                        }
                        Key::Ctrl('s') => {
                            self.keys.cancel();
                            if let Err(error) = write_file(self, "", false) {
                                self.fail(error);
                            }
                        }
                        Key::Ctrl('q') => {
                            self.keys.cancel();
                            if let Err(error) = quit(self, "", false) {
                                self.fail(error);
                            }
                        }
                        Key::Ctrl('c') => {
                            self.keys.cancel();
                            if let Err(error) = self.editor.execute("normal_mode", 1) {
                                self.fail(error);
                            }
                        }
                        Key::Enter if self.editor.mode() == Mode::Insert => {
                            if let Err(error) = self.editor.insert_text(self.files.newline()) {
                                self.fail(error);
                            }
                        }
                        _ => {
                            if let Err(error) = self.keys.handle(&mut self.editor, key) {
                                self.fail(error);
                            }
                        }
                    }
                }
                self.open_search_prompt();
                self.open_requested_picker();
                true
            }
            _ => false,
        }
    }

    /// Dispatch a colon command. The remaining text is one literal path argument,
    /// so file names may contain spaces. A trailing ! on the command means force.
    pub fn execute(&mut self, text: &str) -> io::Result<()> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(());
        }
        let split = text.find(char::is_whitespace).unwrap_or(text.len());
        let (name, argument) = text.split_at(split);
        let force = name.ends_with('!');
        let name = name.strip_suffix('!').unwrap_or(name);
        let argument = argument.trim();
        if let Some(command) = COMMANDS
            .iter()
            .find(|c| c.name == name || c.aliases.contains(&name))
        {
            return (command.run)(self, argument, force);
        }
        if !argument.is_empty() || force {
            return Err(io::Error::other("unknown command or unsupported arguments"));
        }
        self.editor.execute(name, 1).map_err(io::Error::other)?;
        self.open_search_prompt();
        self.open_requested_picker();
        Ok(())
    }

    pub fn paint(&mut self, frame: &mut Frame) -> io::Result<()> {
        self.open_requested_picker();
        self.refresh_diagnostics();
        if self.paint_active_picker(frame) {
            return Ok(());
        }
        self.open_search_prompt();
        let filename = self
            .files
            .path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "[scratch]".into());
        let mut pending = format!(
            "{}{}",
            self.keys.count().map(|n| n.to_string()).unwrap_or_default(),
            self.keys
                .pending_keys()
                .iter()
                .map(ToString::to_string)
                .collect::<String>()
        );
        if self.editor.search_pending() {
            pending.push_str(" searching...");
        }
        pending.push_str(&self.language_status());
        let diagnostic = self.diagnostic_message();
        render::paint(
            frame,
            &self.editor,
            &mut self.viewport,
            Chrome {
                filename: &filename,
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
        )
        .map_err(io::Error::other)?;
        self.paint_language(frame);
        self.paint_key_hints(frame);
        Ok(())
    }

    fn open_search_prompt(&mut self) {
        if self.editor.search_direction().is_none()
            && self
                .prompt
                .as_ref()
                .is_some_and(|p| matches!(p.kind, PromptKind::Search { .. }))
        {
            self.prompt = None;
        }
        if self.prompt.is_none()
            && let Some(direction) = self.editor.search_direction()
        {
            self.keys.cancel();
            self.prompt = Some(ActivePrompt {
                input: Prompt::default(),
                kind: PromptKind::Search {
                    direction,
                    viewport: self.viewport,
                },
            });
        }
    }

    fn preview_search(&mut self, prompt: &ActivePrompt) {
        if let PromptKind::Search { viewport, .. } = prompt.kind {
            if let Err(error) = self.editor.update_search(prompt.input.text()) {
                self.fail(error);
                return;
            }
            self.viewport = viewport;
            if self.editor.search_status() == Some(SearchStatus::NoMatch) {
                self.fail("no matches");
            }
        }
    }

    fn handle_prompt_key(&mut self, key: Key) {
        let mut prompt = self.prompt.take().unwrap();
        match key {
            Key::Escape | Key::Ctrl('c') => {
                self.keys.cancel();
                if let PromptKind::Search { viewport, .. } = prompt.kind {
                    match self.editor.execute("search_cancel", 1) {
                        Ok(()) => self.viewport = viewport,
                        Err(error) => self.fail(error),
                    }
                }
            }
            Key::Enter => match prompt.kind {
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
                            if self.editor.search_direction().is_some() {
                                self.prompt = Some(prompt);
                            }
                        }
                    }
                }
            },
            _ => {
                // Prompt keys only insert/remove bytes or move the caret.
                let before = prompt.input.text().len();
                prompt.input.handle(key);
                if before != prompt.input.text().len() {
                    self.preview_search(&prompt);
                } else if matches!(prompt.kind, PromptKind::Search { .. })
                    && self.editor.search_status() == Some(SearchStatus::NoMatch)
                {
                    self.fail("no matches");
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
}

macro_rules! commands {
    ($($(#[doc = $doc:literal])+ fn $function:ident($app:ident, $arg:ident, $force:ident) [$name:literal $(, $alias:literal)*] $body:block)+) => {
        $( $(#[doc = $doc])+ pub fn $function($app: &mut App, $arg: &str, $force: bool) -> io::Result<()> $body )+
        pub static COMMANDS: &[Command] = &[
            $(Command { name: $name, aliases: &[$($alias,)*], documentation: concat!($($doc, "\n",)+), run: $function },)+
        ];
    };
}

commands! {
    /// Restart rust-analyzer for the current Rust file after an error or configuration change.
    fn restart_lsp(app, argument, force) ["lsp-restart"] {
        if !argument.is_empty() || force { return Err(io::Error::other("lsp-restart takes no arguments")); }
        app.restart_language_server();
        Ok(())
    }

    /// Write the buffer atomically. Accepts an optional path; ! permits overwriting external changes or an existing destination.
    fn write_file(app, argument, force) ["write", "w"] {
        app.editor.finish_undo_group();
        let bytes = app.files.save(app.editor.document(), if argument.is_empty() { None } else { Some(Path::new(argument)) }, force)?;
        if app.automatic_language {
            let language = app.files.path().and_then(Language::from_path);
            if language != app.editor.language() { app.editor.set_language(language); }
        }
        app.message = format!("wrote {bytes} bytes");
        app.language.saved += 1;
        app.language.saved_snapshot = Some(app.editor.document().snapshot());
        Ok(())
    }

    /// Quit. Unsaved changes require :q! to discard them.
    fn quit(app, argument, force) ["quit", "q"] {
        if !argument.is_empty() { return Err(io::Error::other("quit takes no arguments")); }
        if app.is_dirty() && !force { return Err(io::Error::other("unsaved changes; use :w to save or :q! to discard")); }
        app.quit = true;
        Ok(())
    }

    /// Write and quit after a successful save. Accepts the same path and ! options as :write.
    fn write_quit(app, argument, force) ["write-quit", "wq", "x"] {
        write_file(app, argument, force)?;
        quit(app, "", false)
    }

    /// Show or set syntax language: rust, text, or auto (detect from the file extension).
    fn set_language(app, argument, force) ["language", "lang"] {
        if force { return Err(io::Error::other("language does not accept !")); }
        if !argument.is_empty() {
            let language = match argument {
                "rust" => Some(Language::Rust),
                "text" => None,
                "auto" => app.files.path().and_then(Language::from_path),
                _ => return Err(io::Error::other("supported languages: rust, text, auto")),
            };
            app.automatic_language = argument == "auto";
            app.editor.set_language(language);
        }
        app.message = format!("language: {}{}", app.editor.language().map_or("text", Language::name), if app.automatic_language { " (auto)" } else { "" });
        Ok(())
    }

    /// Show basic keys, or the documentation for a named editing or file command.
    fn help(app, argument, force) ["help", "h"] {
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

    fn press(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.handle(Event::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )));
        }
    }
    fn key(app: &mut App, code: KeyCode) {
        app.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    fn draw(app: &mut App) -> Frame {
        let mut frame = Frame::default();
        frame.reset(app.size.0, app.size.1).unwrap();
        app.paint(&mut frame).unwrap();
        frame
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
        assert_eq!(app.editor.selections().ranges().len(), 1);
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
            CharOffset(0)..CharOffset(3)
        );
        key(&mut app, KeyCode::Enter);
        press(&mut app, "2n");
        assert_eq!(app.editor.selections().primary().start(), CharOffset(12));
        press(&mut app, "nN");
        assert_eq!(app.editor.selections().primary().start(), CharOffset(12));
        press(&mut app, "?cat");
        assert!(draw(&mut app).row_text(7).starts_with("?cat"));
        key(&mut app, KeyCode::Enter);
        press(&mut app, "n");
        assert_eq!(app.editor.selections().primary().start(), CharOffset(8));
        press(&mut app, "d");
        assert_eq!(app.editor.document().text(), "cat bat  cat");
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
    fn search_commands_open_prompts_through_colon_or_custom_bindings() {
        use vex_editor::Keymap;
        let mut app = App::from_document(Document::from("one two one"), (40, 6));
        press(&mut app, ":search_backward");
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.prompt.as_ref().unwrap().prefix(), '?');
        key(&mut app, KeyCode::Esc);
        let mut map = Keymap::empty();
        map.bind(Mode::Normal, vec![Key::Char('s')], "search_forward")
            .unwrap();
        app.keys = KeyHandler::new(map);
        press(&mut app, "sone");
        assert_eq!(app.editor.search_status(), Some(SearchStatus::Match));
        key(&mut app, KeyCode::Enter);
        app.execute("help search_next").unwrap();
        assert!(app.message.contains("literal match"));
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
    fn saving_during_insert_keeps_the_savepoint_reachable_by_undo_and_redo() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file");
        std::fs::write(&path, "").unwrap();
        let mut app = App::open(Some(&path), (80, 24)).unwrap();
        press(&mut app, "ihello");
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        )));
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
        assert!(scratch.message.contains("rust, text, or auto"));
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
            frame.style_at(5, 0).unwrap()
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
