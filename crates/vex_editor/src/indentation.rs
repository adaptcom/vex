//! Bounded whitespace inference at buffer creation and disk reload, never on a
//! keystroke. Ambiguous samples leave the language's defaults in charge.

use std::num::NonZeroUsize;
use vex_core::Rope;

use crate::{Editor, IndentStyle, Indentation, Language};

const MAX_LINES: usize = 1_000;
const MAX_PREFIX: usize = 128;
const MAX_WIDTH: usize = 8;

#[derive(Clone, Copy, Debug)]
pub(super) struct State {
    detected: Option<IndentStyle>,
    pub overridden: bool,
}

impl State {
    pub fn new(text: &Rope) -> Self {
        Self {
            detected: detect(text),
            overridden: false,
        }
    }

    pub fn resolve(self, language: Option<Language>) -> Indentation {
        let defaults = language.map(Language::indentation).unwrap_or_default();
        match self.detected {
            Some(IndentStyle::Spaces(width)) => Indentation::spaces(width),
            // Literal tabs reveal the style, but cannot tell us their display width.
            Some(IndentStyle::Tabs) => Indentation {
                style: IndentStyle::Tabs,
                ..defaults
            },
            None => defaults,
        }
    }
}

impl Editor {
    pub(super) fn refresh_indentation(&mut self) {
        self.indentation_state.detected = detect(self.document.text());
        if !self.indentation_state.overridden {
            self.apply_indentation(self.indentation_state.resolve(self.language()));
        }
    }
}

#[derive(Clone, Copy)]
enum Prefix {
    Spaces(usize),
    Tabs,
    Ignore,
    Unknown,
}

fn prefix(line: vex_core::RopeSlice<'_>) -> Prefix {
    let (mut spaces, mut tabs) = (0, 0);
    let mut mixed = false;
    for ch in line.chars().take(MAX_PREFIX + 1) {
        match ch {
            ' ' => spaces += 1,
            '\t' => {
                mixed |= spaces > 0;
                tabs += 1;
            }
            // Ignore blank lines and conventional block-comment continuation
            // stars, whose one-column offset is usually alignment.
            ch if ch.is_whitespace() || ch == '*' => return Prefix::Ignore,
            _ => {
                return if mixed {
                    Prefix::Unknown
                } else if tabs > 0 {
                    Prefix::Tabs
                } else {
                    Prefix::Spaces(spaces)
                };
            }
        }
    }
    if spaces + tabs > MAX_PREFIX {
        Prefix::Unknown
    } else {
        Prefix::Ignore
    }
}

fn detect(text: &Rope) -> Option<IndentStyle> {
    let mut lengths = [0usize; MAX_PREFIX + 1];
    let mut changes = [0usize; MAX_WIDTH + 1];
    let (mut spaces, mut tabs, mut mixed) = (0, 0, 0);
    let mut previous: Option<usize> = None;
    // Rope line iteration skips line bodies without copying or scanning them.
    for line in text.lines().take(MAX_LINES) {
        match prefix(line) {
            Prefix::Spaces(width) => {
                if width > 0 {
                    spaces += 1;
                    lengths[width] += 1;
                }
                if let Some(old) = previous {
                    let change = width.abs_diff(old);
                    if (2..=MAX_WIDTH).contains(&change) {
                        changes[change] += 1;
                    }
                }
                previous = Some(width);
            }
            Prefix::Tabs => {
                tabs += 1;
                previous = None;
            }
            Prefix::Unknown => {
                mixed += 1;
                previous = None;
            }
            Prefix::Ignore => {}
        }
    }
    let samples = spaces + tabs + mixed;
    if tabs > 0 && tabs * 4 >= samples * 3 {
        return Some(IndentStyle::Tabs);
    }
    if spaces == 0 || spaces * 4 < samples * 3 {
        return None;
    }
    let (mut best, mut votes, mut tied) = (None, 0, false);
    for width in 2..=MAX_WIDTH {
        // Prefer recurring level changes over the most common absolute depth:
        // many lines inside a nested block must not inflate the indent unit.
        let aligned: usize = lengths.iter().skip(width).step_by(width).sum();
        if changes[width] + lengths[width] < 2 || aligned * 4 < spaces * 3 {
            continue;
        }
        if best.is_none() || changes[width] > votes {
            best = Some(width);
            votes = changes[width];
            tied = false;
        } else if changes[width] == votes {
            tied = true;
        }
    }
    if tied {
        None
    } else {
        best.map(|width| IndentStyle::Spaces(NonZeroUsize::new(width).unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vex_core::{CharOffset, Document, Edit};

    fn sample(unit: &str, ending: &str) -> String {
        format!(
            "root{ending}{unit}one{ending}{unit}{unit}nested{ending}{unit}two{ending}end{ending}"
        )
    }

    #[test]
    fn detects_space_levels_and_tabs_across_line_endings_without_rewriting_text() {
        for ending in ["\n", "\r\n", "\r", "\u{2028}"] {
            for width in [2, 3, 4, 8] {
                let source = sample(&" ".repeat(width), ending);
                assert_eq!(
                    detect(&Rope::from_str(&source)),
                    Some(IndentStyle::Spaces(NonZeroUsize::new(width).unwrap()))
                );
            }
            for source in [
                sample("\t", ending),
                format!("\t  aligned{ending}\tother{ending}"),
            ] {
                assert_eq!(detect(&Rope::from_str(&source)), Some(IndentStyle::Tabs));
            }
        }
        for unit in ["  ", "    ", "        ", "\t"] {
            let source = sample(unit, "\r\n");
            let mut editor = Editor::new(Document::from(source.as_str()));
            editor.set_language(Some(Language::TypeScript));
            assert_eq!(editor.document().text(), source.as_str());
            assert_eq!(editor.document().undo_depth(), 0);
            editor.execute("insert_mode", 1).unwrap();
            editor.execute("insert_tab", 1).unwrap();
            assert_eq!(editor.document().text(), format!("{unit}{source}").as_str());
            editor.execute("normal_mode", 1).unwrap();
            editor.execute("undo", 1).unwrap();
            editor.execute("indent", 1).unwrap();
            assert_eq!(editor.document().text(), format!("{unit}{source}").as_str());
            editor.execute("unindent", 1).unwrap();
            assert_eq!(editor.document().text(), source.as_str());
        }
    }

    #[test]
    fn recurring_levels_outweigh_nested_depth_and_ignore_blank_and_comment_alignment() {
        let nested = format!(
            "root\n    outer\n{}    end\nend\n",
            "        nested\n".repeat(50)
        );
        let aligned = "root\n    first\n    second\n        nested\n                 aligned\n    third\n    fourth\nend\n";
        let comments =
            "/* docs\n * aligned\n * more\n */\nroot\n    first\n \t \n    second\nend\n";
        for source in [nested.as_str(), aligned, comments] {
            assert_eq!(
                detect(&Rope::from_str(source)),
                Some(IndentStyle::Spaces(NonZeroUsize::new(4).unwrap()))
            );
        }
        assert_eq!(
            detect(&Rope::from_str("\ta\n\tb\n\tc\n    stray\n")),
            Some(IndentStyle::Tabs)
        );
    }

    #[test]
    fn sparse_ambiguous_mixed_and_oversized_samples_fall_back_to_language_defaults() {
        for source in [
            "",
            "unindented\ntext\n",
            "    alone",
            " \t \r\n\t \n",
            "root\n  a\nroot\n   b\nroot\n",
            "\ta\n  b\n",
            "                 aligned\n                 other\n",
        ] {
            assert_eq!(detect(&Rope::from_str(source)), None, "{source:?}");
            let mut editor = Editor::new(Document::from(source));
            assert_eq!(editor.indentation(), Indentation::default());
            editor.set_language(Some(Language::TypeScript));
            assert_eq!(editor.indentation(), Language::TypeScript.indentation());
        }
        let late = format!("{}{}", "plain\n".repeat(MAX_LINES), sample("  ", "\n"));
        assert_eq!(detect(&Rope::from_str(&late)), None);
        assert_eq!(detect(&Rope::from_str(&" ".repeat(1 << 20))), None);
    }

    #[test]
    fn go_defaults_to_tabs_but_existing_spaces_take_precedence() {
        let mut empty = Editor::new(Document::default());
        empty.set_language(Some(Language::Go));
        empty.execute("insert_mode", 1).unwrap();
        empty.execute("insert_tab", 1).unwrap();
        assert_eq!(empty.document().text(), "\t");
        assert_eq!(empty.tab_width().get(), 4);

        let source = sample("  ", "\n");
        let mut existing = Editor::new(Document::from(source.as_str()));
        existing.set_language(Some(Language::Go));
        assert_eq!(
            existing.indentation(),
            Indentation::spaces(NonZeroUsize::new(2).unwrap())
        );
        existing.execute("insert_mode", 1).unwrap();
        existing.execute("insert_tab", 1).unwrap();
        assert_eq!(existing.document().text(), format!("  {source}").as_str());
    }

    #[test]
    fn reloads_refresh_detection_and_retain_explicit_buffer_overrides() {
        fn reload(editor: &mut Editor, text: &str) {
            let transaction = editor
                .document()
                .transaction([Edit::new(
                    CharOffset(0)..CharOffset(editor.document().text().len_chars()),
                    text,
                )])
                .unwrap();
            editor.apply_external_change(transaction).unwrap();
        }
        let mut editor = Editor::new(Document::from(sample("  ", "\n").as_str()));
        editor.set_language(Some(Language::Rust));
        assert_eq!(editor.tab_width().get(), 2);
        reload(&mut editor, &sample("\t", "\n"));
        assert_eq!(editor.indentation().style, IndentStyle::Tabs);
        assert_eq!(editor.tab_width().get(), 4);
        let custom = Indentation::spaces(NonZeroUsize::new(3).unwrap());
        editor.set_indentation(custom);
        reload(&mut editor, &sample("        ", "\n"));
        assert_eq!(editor.indentation(), custom);
        editor.set_language(Some(Language::Rust));
        assert_eq!(editor.indentation(), custom);
        editor.set_language(Some(Language::TypeScript));
        assert_eq!(editor.tab_width().get(), 8);
        let mut empty = Editor::new(Document::default());
        empty.execute("insert_mode", 1).unwrap();
        empty.insert_text(&sample("  ", "\n")).unwrap();
        assert_eq!(
            empty.tab_width().get(),
            4,
            "typing does not change the chosen style"
        );
    }

    #[test]
    #[ignore = "manual release-mode bounded indentation detection benchmark"]
    fn benchmark_indentation_detection() {
        use std::{hint::black_box, time::Instant};
        for (name, source) in [
            ("200,000 lines", "root\n    child\n".repeat(100_000)),
            ("1 MiB line", "x".repeat(1 << 20)),
            ("1 MiB whitespace prefix", " ".repeat(1 << 20)),
        ] {
            let text = Rope::from_str(&source);
            let mut times = Vec::with_capacity(1_000);
            for _ in 0..1_000 {
                let start = Instant::now();
                black_box(detect(black_box(&text)));
                times.push(start.elapsed());
            }
            times.sort_unstable();
            eprintln!("{name}: median {:?}, p95 {:?}", times[500], times[950]);
        }
    }
}
