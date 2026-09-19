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
use vex_editor::{Editor, Key, KeyHandler, Mode};

pub struct App {
    pub editor: Editor,
    files: FileState,
    keys: KeyHandler,
    viewport: Viewport,
    prompt: Option<Prompt>,
    message: String,
    error: bool,
    quit: bool,
    size: (u16, u16),
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
        Self {
            editor: Editor::new(document),
            files,
            keys: KeyHandler::default(),
            viewport: Viewport::default(),
            prompt: None,
            message: "i insert  :w write  :q quit  :help".into(),
            error: false,
            quit: false,
            size,
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

    /// Handle one event. Return whether the screen may have changed.
    pub fn handle(&mut self, event: Event) -> bool {
        match event {
            Event::Resize(width, height) => {
                self.size = (width, height);
                true
            }
            Event::FocusGained => true,
            Event::Paste(text) => {
                self.clear_message();
                if let Some(prompt) = &mut self.prompt {
                    prompt.insert(&text);
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
                if let Some(prompt) = &mut self.prompt {
                    match key {
                        Key::Escape | Key::Ctrl('c') => {
                            self.prompt = None;
                            self.keys.cancel();
                        }
                        Key::Enter => {
                            let text = self.prompt.take().unwrap().text().to_owned();
                            if let Err(error) = self.execute(&text) {
                                self.fail(error);
                            }
                        }
                        _ => prompt.handle(key),
                    }
                } else {
                    match key {
                        Key::Char(':')
                            if self.editor.mode() != Mode::Insert
                                && self.keys.pending_keys().is_empty() =>
                        {
                            self.keys.cancel();
                            self.prompt = Some(Prompt::default());
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
        self.editor.execute(name, 1).map_err(io::Error::other)
    }

    pub fn paint(&mut self, frame: &mut Frame) -> io::Result<()> {
        let filename = self
            .files
            .path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "[scratch]".into());
        let pending = format!(
            "{}{}",
            self.keys.count().map(|n| n.to_string()).unwrap_or_default(),
            self.keys
                .pending_keys()
                .iter()
                .map(ToString::to_string)
                .collect::<String>()
        );
        render::paint(
            frame,
            &self.editor,
            &mut self.viewport,
            Chrome {
                filename: &filename,
                dirty: self.files.is_dirty(self.editor.document()),
                pending: &pending,
                message: &self.message,
                error: self.error,
                prompt: self.prompt.as_ref().map(|p| (p.text(), p.cursor())),
            },
        )
        .map_err(io::Error::other)
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
    /// Write the buffer atomically. Accepts an optional path; ! permits overwriting external changes or an existing destination.
    fn write_file(app, argument, force) ["write", "w"] {
        app.editor.finish_undo_group();
        let bytes = app.files.save(app.editor.document(), if argument.is_empty() { None } else { Some(Path::new(argument)) }, force)?;
        app.message = format!("wrote {bytes} bytes");
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

    /// Show basic keys, or the documentation for a named editing or file command.
    fn help(app, argument, force) ["help", "h"] {
        if force { return Err(io::Error::other("help does not accept !")); }
        app.message = if argument.is_empty() {
            "i/a insert  Esc normal  v select  hjkl/wbe move  u/U undo/redo  :w [PATH] write  :q[!] quit  :help COMMAND".into()
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
}
