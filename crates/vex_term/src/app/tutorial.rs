//! The tutorial is bundled text opened through ordinary buffer management.

use super::App;
use std::io;

const TEXT: &str = include_str!("../tutorial.txt");
const NAME: &str = "[tutorial]";

impl App {
    pub(super) fn open_tutorial(&mut self) -> io::Result<()> {
        self.open_named_scratch(NAME, TEXT)?;
        self.clear_message();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{draw, key, press};
    use crossterm::event::KeyCode;
    use vex_core::Document;
    use vex_editor::Mode;

    #[test]
    fn practice_resumes_without_losing_the_previous_files_edits_or_view() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("original.txt");
        std::fs::write(&path, "original\n").unwrap();
        let mut app = App::open(Some(&path), (80, 24)).unwrap();
        let original = app.editor.document().id();
        press(&mut app, "iunsaved ");
        key(&mut app, KeyCode::Esc);
        let selections = app.editor.selections().clone();

        app.execute("tutorial").unwrap();
        let tutorial = app.editor.document().id();
        assert_ne!(tutorial, original);
        assert_eq!(app.editor.document().text(), TEXT);
        assert_eq!(app.editor.mode(), Mode::Normal);
        assert!(!app.is_dirty());
        assert_eq!(app.files.path(), None);
        assert_eq!(app.files.target(), None);
        assert_eq!(app.editor.language(), None);
        assert_eq!(
            app.editor.register_first('%').unwrap().as_deref(),
            Some(NAME)
        );
        assert!(draw(&mut app).row_text(22).contains(NAME));

        press(&mut app, "inotes: ");
        key(&mut app, KeyCode::Esc);
        press(&mut app, "40j");
        draw(&mut app);
        let practice_view = app.viewport;
        let practice_selection = app.editor.selections().clone();
        let practice_text = app.editor.document().text().clone();
        app.execute("tutorial").unwrap();
        assert_eq!(app.editor.document().id(), tutorial);

        press(&mut app, "ga");
        assert_eq!(app.editor.document().id(), original);
        assert_eq!(app.editor.document().text(), "unsaved original\n");
        assert_eq!(app.editor.selections(), &selections);
        assert!(app.is_dirty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original\n");

        app.execute("tutorial").unwrap();
        assert_eq!(app.editor.document().id(), tutorial);
        assert_eq!(app.editor.document().text(), &practice_text);
        assert_eq!(app.editor.selections(), &practice_selection);
        assert_eq!(app.viewport, practice_view);
        let catalog = app.buffer_catalog();
        assert_eq!(catalog.len(), 2);
        assert!(
            catalog
                .iter()
                .any(|item| item.entry.value == tutorial && item.entry.label == "[tutorial]  [*+]")
        );
        assert!(app.execute("bc").is_err());
        app.execute("bc!").unwrap();
        assert_eq!(app.editor.document().id(), original);
        assert!(app.is_dirty());
        press(&mut app, "u");
        assert_eq!(app.editor.document().text(), "original\n");
        assert!(!app.is_dirty());

        app.execute("tutorial").unwrap();
        assert_ne!(app.editor.document().id(), tutorial);
        assert_eq!(app.editor.document().text(), TEXT);
        assert!(!app.is_dirty());
    }

    #[test]
    fn saving_notes_makes_a_normal_file_and_leaves_a_fresh_tutorial_available() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("my tutorial notes.txt");
        let mut app = App::from_document(Document::default(), (80, 24));
        app.execute("tutorial").unwrap();
        let notes = app.editor.document().id();
        press(&mut app, "iMy notes: ");
        key(&mut app, KeyCode::Esc);
        assert!(app.execute("w").is_err());
        assert!(app.is_dirty());
        app.execute(&format!("w {}", path.display())).unwrap();
        assert!(!app.is_dirty());
        assert!(!app.files.is_named_scratch(NAME));
        assert_eq!(
            app.editor.register_first('%').unwrap().as_deref(),
            path.to_str()
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("My notes: {TEXT}")
        );

        app.execute("tutorial").unwrap();
        assert_ne!(app.editor.document().id(), notes);
        assert_eq!(app.editor.document().text(), TEXT);
        assert_eq!(app.files.path(), None);
        press(&mut app, "ga");
        assert_eq!(app.editor.document().id(), notes);
        assert_eq!(app.files.path(), Some(path.as_path()));
        assert_eq!(
            app.editor.document().text(),
            format!("My notes: {TEXT}").as_str()
        );
    }

    #[test]
    fn tutorial_reuses_a_buffer_already_visible_in_another_pane() {
        let mut app = App::from_document(Document::from("original"), (100, 24));
        let original = app.editor.document().id();
        app.execute("vsplit").unwrap();
        app.execute("tutorial").unwrap();
        let tutorial = app.editor.document().id();
        press(&mut app, "iNotes: ");
        key(&mut app, KeyCode::Esc);
        press(&mut app, " wh");
        assert_eq!(app.editor.document().id(), original);
        assert!(draw(&mut app).row_text(22).contains(NAME));
        app.execute("tutorial").unwrap();
        assert_eq!(app.editor.document().id(), tutorial);
        assert_eq!(app.buffer_catalog().len(), 2);
        assert_eq!(draw(&mut app).row_text(22).matches(NAME).count(), 2);
        press(&mut app, "u");
        assert_eq!(app.editor.document().text(), TEXT);
        app.execute("bc").unwrap();
        assert_eq!(app.editor.document().id(), original);
        assert_eq!(app.buffer_catalog().len(), 1);
        assert!(!draw(&mut app).row_text(22).contains(NAME));
    }

    #[test]
    fn tutorial_rejects_arguments_and_explains_how_to_restart() {
        let mut app = App::from_document(Document::from("original"), (80, 24));
        let original = app.editor.document().id();
        for command in ["tutorial!", "tutorial path.txt"] {
            assert!(app.execute(command).is_err());
            assert_eq!(app.editor.document().id(), original);
            assert_eq!(app.buffer_catalog().len(), 1);
        }
        app.execute("help tutorial").unwrap();
        assert!(app.message.contains(":bc!"));
    }

    // Execute the lesson recipes against their real practice rows, including
    // regex prompts and default bindings. This catches prose that promises an
    // outcome the keymap cannot produce, without recreating command unit tests.
    #[test]
    fn editing_exercises_produce_the_advertised_text() {
        for (example, keys, expected) in [
            (
                "findable practice text",
                "ihello \x1b",
                "hello findable practice text",
            ),
            ("old cabin", "miwcnew\x1b", "new cabin"),
            ("old cabin", "miwr_", "___ cabin"),
            ("cargo wagon", "miwyglmiwR", "cargo cargo"),
            ("first plank\nsecond plank", "J", "first plank second plank"),
            (
                "stone moss stone moss stone",
                "miw*vnncpebble\x1b",
                "pebble moss pebble moss pebble",
            ),
            ("cat dog cat", "xscat\ncfox\x1b", "fox dog fox"),
            ("oak,pine,birch", "x_S,\nKi\nctree\x1b", "oak,tree,tree"),
            ("amber\nazure\napple", "CCi>\x1b", ">amber\n>azure\n>apple"),
            ("bundle", "miwms)", "(bundle)"),
            ("bundle", "miwms)mr)]", "[bundle]"),
            (
                "repeat here\nrepeat there",
                "I>\x1bj.",
                ">repeat here\n>repeat there",
            ),
            (
                "crate crate crate",
                "xscrate\nms)",
                "(crate) (crate) (crate)",
            ),
        ] {
            let mut app = App::from_document(Document::default(), (80, 24));
            app.execute("tutorial").unwrap();
            // Workers are exercised in the PTY smoke test; execute search and
            // replay synchronously here so the recipes need no timing waits.
            app.editor.set_background_search(false);
            app.editor.set_deferred_repeat(false);
            press(&mut app, &format!("/^{}", example.lines().next().unwrap()));
            key(&mut app, KeyCode::Enter);
            press(&mut app, "gh");
            for ch in keys.chars() {
                match ch {
                    '\x1b' => key(&mut app, KeyCode::Esc),
                    '\n' => key(&mut app, KeyCode::Enter),
                    ch => press(&mut app, &ch.to_string()),
                }
            }
            let before = format!("\n{example}\n");
            assert_eq!(TEXT.matches(&before).count(), 1, "ambiguous practice row");
            let actual = app.editor.document().text().to_string();
            let expected = TEXT.replace(&before, &format!("\n{expected}\n"));
            assert_eq!(
                actual.lines().count(),
                expected.lines().count(),
                "{example}"
            );
            for (line, (actual, expected)) in actual.lines().zip(expected.lines()).enumerate() {
                assert_eq!(
                    actual,
                    expected,
                    "exercise: {example}, keys: {keys:?}, line: {}",
                    line + 1
                );
            }
            assert!(!app.error, "{example}: {}", app.message);
        }
    }
}
