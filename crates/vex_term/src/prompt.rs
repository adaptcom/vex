//! Bounded command/path completion. All directory and metadata reads run on the
//! shared picker worker; jobs carry no document snapshots or application locks.

use crate::{
    app::{ArgumentCompletion, COMMANDS},
    input::PromptStamp,
};
use std::{
    collections::BTreeMap,
    ops::Range,
    path::{MAIN_SEPARATOR, Path},
    time::{Duration, Instant},
};
use vex_editor::{Language, background::Cancellation};

pub(crate) const MAX_QUERY_BYTES: usize = 8 * 1024;
const MAX_ITEMS: usize = 128;
const MAX_SCANNED: usize = 10_000;
const MAX_ITEM_BYTES: usize = 4096;

pub(crate) struct Job {
    pub stamp: PromptStamp,
    pub text: String,
    pub cursor: usize,
    pub cancellation: Cancellation,
}

#[derive(Debug)]
pub(crate) struct Item {
    pub text: String,
    pub description: &'static str,
    pub directory: bool,
}

pub(crate) struct Result {
    pub stamp: PromptStamp,
    pub range: Range<usize>,
    pub items: Vec<Item>,
    pub notice: String,
}

impl Job {
    pub fn run(self) -> Option<Result> {
        if self.cancellation.is_cancelled() {
            return None;
        }
        let mut result = Result {
            stamp: self.stamp,
            range: 0..0,
            items: Vec::new(),
            notice: String::new(),
        };
        if self.text.len() > MAX_QUERY_BYTES || !self.text.is_char_boundary(self.cursor) {
            return Some(result);
        }
        let start = self.text.len() - self.text.trim_start().len();
        let end = start
            + self.text[start..]
                .find(char::is_whitespace)
                .unwrap_or(self.text.len() - start);
        if self.cursor < start {
            return Some(result);
        }
        if self.cursor <= end {
            let prefix = &self.text[start..self.cursor];
            let forced = self.text[start..end].ends_with('!');
            result.range = start..end;
            result.items = commands(prefix.trim_end_matches('!'), forced);
        } else {
            let name = self.text[start..end].trim_end_matches('!');
            let Some(command) = COMMANDS
                .iter()
                .find(|command| command.name == name || command.aliases.contains(&name))
            else {
                return Some(result);
            };
            let argument = end + self.text[end..].len() - self.text[end..].trim_start().len();
            if self.cursor < argument {
                return Some(result);
            }
            result.range = argument..self.text.len();
            let prefix = &self.text[argument..self.cursor];
            match command.completion {
                ArgumentCompletion::None => {}
                ArgumentCompletion::Command => result.items = commands(prefix, false),
                ArgumentCompletion::Language => {
                    result.items = Language::ALL
                        .iter()
                        .map(|language| language.name())
                        .chain(["text", "auto"])
                        .filter(|name| name.starts_with(prefix))
                        .map(|name| Item {
                            text: name.into(),
                            description: "language",
                            directory: false,
                        })
                        .collect();
                }
                ArgumentCompletion::AutoCompletion => {
                    result.items = ["on", "off", "delay", "min-length"]
                        .into_iter()
                        .filter(|name| name.starts_with(prefix))
                        .map(|name| Item {
                            text: name.into(),
                            description: "automatic completion",
                            directory: false,
                        })
                        .collect();
                }
                ArgumentCompletion::Path => self.paths(argument, prefix, &mut result),
            }
        }
        result.items.sort_unstable_by(|a, b| a.text.cmp(&b.text));
        (!self.cancellation.is_cancelled()).then_some(result)
    }

    fn paths(&self, argument: usize, prefix: &str, result: &mut Result) {
        let started = Instant::now();
        let component = prefix.rfind(std::path::is_separator).map_or(0, |i| i + 1);
        let directory = if component == 0 {
            "."
        } else {
            &prefix[..component]
        };
        let query = &prefix[component..];
        result.range.start = argument + component;
        let suffix = &self.text[self.cursor..];
        result.range.end =
            self.cursor + suffix.find(std::path::is_separator).unwrap_or(suffix.len());
        let existing_separator = result.range.end < self.text.len();
        let listing = match std::fs::read_dir(Path::new(directory)) {
            Ok(listing) => listing,
            Err(error) => {
                result.notice = format!("Cannot list directory: {error}");
                return;
            }
        };
        let mut candidates = BTreeMap::new();
        let mut limited = false;
        for (scanned, entry) in listing.enumerate() {
            if self.cancellation.is_cancelled() {
                return;
            }
            if scanned >= MAX_SCANNED || started.elapsed() >= Duration::from_millis(250) {
                limited = true;
                break;
            }
            let Ok(entry) = entry else {
                continue;
            };
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if !name.starts_with(query)
                || name.len() >= MAX_ITEM_BYTES
                || name.chars().any(char::is_control)
                || name.starts_with('.') && !query.starts_with('.')
            {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let directory = kind.is_dir() || kind.is_symlink() && entry.path().is_dir();
            if existing_separator && !directory {
                continue;
            }
            let mut text = name;
            if directory && !existing_separator {
                text.push(MAIN_SEPARATOR);
            }
            candidates.insert(text, directory);
            if candidates.len() > MAX_ITEMS {
                candidates.pop_last();
                limited = true;
            }
        }
        result.items = candidates
            .into_iter()
            .map(|(text, directory)| Item {
                text,
                directory,
                description: if directory { "directory" } else { "file" },
            })
            .collect();
        if limited {
            result.notice = "More matches available; narrow the path".into();
        }
    }
}

fn commands(prefix: &str, forced: bool) -> Vec<Item> {
    let mut names = BTreeMap::new();
    for command in COMMANDS {
        for name in std::iter::once(command.name).chain(command.aliases.iter().copied()) {
            if name.starts_with(prefix) {
                names.insert(name, command.documentation.trim());
            }
        }
    }
    for command in vex_editor::commands::COMMANDS {
        if !forced && command.name.starts_with(prefix) {
            names.entry(command.name).or_insert(command.description());
        }
    }
    // The registry is finite. Retain all its names so an empty prompt can cycle
    // through every documented command; directory results have a separate cap.
    names
        .into_iter()
        .map(|(name, description)| Item {
            text: if forced {
                format!("{name}!")
            } else {
                name.into()
            },
            description,
            directory: false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Prompt;

    fn job(text: &str) -> Job {
        Job {
            stamp: Prompt::default().stamp(),
            text: text.into(),
            cursor: text.len(),
            cancellation: Cancellation::default(),
        }
    }

    #[test]
    fn command_aliases_forces_arguments_and_mid_command_replacement() {
        let result = job("wri!").run().unwrap();
        assert!(result.items.iter().any(|item| item.text == "write!"));
        assert!(result.items.iter().all(|item| item.text.ends_with('!')));
        let result = job("help move_word").run().unwrap();
        assert!(
            result
                .items
                .iter()
                .any(|item| item.text == "move_word_forward")
        );
        let result = job("lang rus").run().unwrap();
        assert_eq!(result.items[0].text, "rust");
        let mut request = job("wri path with spaces");
        request.cursor = 2;
        let result = request.run().unwrap();
        assert_eq!(result.range, 0..3);
        assert!(result.items.iter().any(|item| item.text == "write"));
    }

    #[test]
    fn path_candidates_keep_literal_spaces_unicode_hidden_names_and_directories() {
        let root = tempfile::tempdir().unwrap();
        for name in ["a file.rs", "a🦀.rs", ".hidden", "b.rs"] {
            std::fs::write(root.path().join(name), "").unwrap();
        }
        std::fs::create_dir(root.path().join("a dir")).unwrap();
        let prefix = format!("open {}/", root.path().display());
        let result = job(&format!("{prefix}a")).run().unwrap();
        assert_eq!(result.range, prefix.len()..prefix.len() + 1);
        assert_eq!(
            result
                .items
                .iter()
                .map(|item| item.text.as_str())
                .collect::<Vec<_>>(),
            ["a dir/", "a file.rs", "a🦀.rs"]
        );
        assert!(result.items[0].directory);
        let hidden = job(&format!("{prefix}.")).run().unwrap();
        assert_eq!(hidden.items[0].text, ".hidden");
        let mut middle = job(&format!("{prefix}a/child"));
        middle.cursor = prefix.len() + 1;
        let result = middle.run().unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].text, "a dir");
        assert_eq!(result.range.end, prefix.len() + 1);
    }

    #[test]
    fn missing_directories_limits_and_cancellation_return_safely() {
        let root = tempfile::tempdir().unwrap();
        let request = job(&format!("open {}/missing/", root.path().display()));
        let result = request.run().unwrap();
        assert!(result.items.is_empty());
        assert!(result.notice.contains("Cannot list"));
        for n in 0..MAX_ITEMS + 10 {
            std::fs::write(root.path().join(format!("file{n:04}")), "").unwrap();
        }
        let result = job(&format!("open {}/", root.path().display()))
            .run()
            .unwrap();
        assert_eq!(result.items.len(), MAX_ITEMS);
        assert!(!result.notice.is_empty());
        let request = job("open ");
        request.cancellation.cancel();
        assert!(request.run().is_none());
    }
}
