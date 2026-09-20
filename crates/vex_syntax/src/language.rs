//! Built-in language registry shared by syntax, file previews, and LSP sessions.
//! Add a grammar dependency and one entry below to support another language.

use std::{path::Path, sync::OnceLock};
use vex_core::Rope;

use super::Configuration;

/// Default stdio server command and project discovery rules. Executables are
/// supplied by the user; arguments are passed directly without a shell.
#[derive(Clone, Copy, Debug)]
pub struct LanguageServer {
    pub command: &'static str,
    pub arguments: &'static [&'static str],
    pub environment: &'static str,
    pub label: &'static str,
    pub root_markers: &'static [&'static str],
    pub outermost_root: bool,
}

pub(super) struct Definition {
    name: &'static str,
    aliases: &'static [&'static str],
    extensions: &'static [&'static str],
    filenames: &'static [&'static str],
    interpreters: &'static [&'static str],
    language_id: &'static str,
    server: Option<LanguageServer>,
    grammar: fn() -> tree_sitter::Language,
    queries: &'static [&'static str],
    inline: Option<fn() -> Configuration>,
    compiled: OnceLock<Configuration>,
}

// Generate the enum and lookup from the same entries, so language additions
// cannot silently omit a detection, query, or LSP dispatch match arm.
macro_rules! languages {
    ($($variant:ident { $($field:ident: $value:expr),* $(,)? }),* $(,)?) => {
        /// Bundled language identities, independent of document text and UI state.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Language { $($variant),* }

        impl Language {
            pub const ALL: &'static [Self] = &[$(Self::$variant),*];

            fn definition(self) -> &'static Definition {
                match self {
                    $(Self::$variant => {
                        static DEFINITION: Definition = Definition {
                            $($field: $value,)*
                            compiled: OnceLock::new(),
                        };
                        &DEFINITION
                    }),*
                }
            }
        }
    };
}

const TYPESCRIPT_SERVER: Option<LanguageServer> = Some(LanguageServer {
    command: "typescript-language-server",
    arguments: &["--stdio"],
    environment: "VEX_TYPESCRIPT_LANGUAGE_SERVER",
    label: "TS",
    root_markers: &["tsconfig.json", "jsconfig.json", "package.json"],
    outermost_root: false,
});

languages! {
    Rust {
        name: "rust", aliases: &["rs"], extensions: &["rs"], filenames: &[], interpreters: &[],
        language_id: "rust",
        server: Some(LanguageServer {
            command: "rust-analyzer", arguments: &[], environment: "VEX_RUST_ANALYZER", label: "RA",
            root_markers: &["Cargo.toml"], outermost_root: true,
        }),
        grammar: || tree_sitter_rust::LANGUAGE.into(),
        queries: &[tree_sitter_rust::HIGHLIGHTS_QUERY], inline: None,
    },
    Markdown {
        name: "markdown", aliases: &["md"], extensions: &["md", "markdown", "mdown", "mkd"],
        filenames: &[], interpreters: &[], language_id: "markdown",
        server: Some(LanguageServer {
            command: "marksman", arguments: &["server"], environment: "VEX_MARKSMAN", label: "Marksman",
            root_markers: &[".marksman.toml"], outermost_root: false,
        }),
        grammar: || tree_sitter_md::LANGUAGE.into(),
        queries: &[tree_sitter_md::HIGHLIGHT_QUERY_BLOCK, "(inline) @vex.inline"],
        inline: Some(|| Configuration::new(tree_sitter_md::INLINE_LANGUAGE.into(), &[tree_sitter_md::HIGHLIGHT_QUERY_INLINE], None)),
    },
    Bash {
        name: "bash", aliases: &["sh", "shell", "shellscript"], extensions: &["sh", "bash"],
        filenames: &[".bashrc", ".bash_profile", ".bash_login", ".bash_logout", ".profile", "PKGBUILD"],
        interpreters: &["bash", "sh", "dash"], language_id: "shellscript",
        server: Some(LanguageServer {
            command: "bash-language-server", arguments: &["start"], environment: "VEX_BASH_LANGUAGE_SERVER", label: "Bash",
            root_markers: &[".shellcheckrc"], outermost_root: false,
        }),
        grammar: || tree_sitter_bash::LANGUAGE.into(),
        queries: &[tree_sitter_bash::HIGHLIGHT_QUERY], inline: None,
    },
    JavaScript {
        name: "javascript", aliases: &["js"], extensions: &["js", "mjs", "cjs"],
        filenames: &[], interpreters: &["node", "nodejs"], language_id: "javascript",
        server: TYPESCRIPT_SERVER,
        grammar: || tree_sitter_javascript::LANGUAGE.into(),
        queries: &[tree_sitter_javascript::HIGHLIGHT_QUERY, tree_sitter_javascript::JSX_HIGHLIGHT_QUERY], inline: None,
    },
    Jsx {
        name: "jsx", aliases: &["javascriptreact"], extensions: &["jsx"],
        filenames: &[], interpreters: &[], language_id: "javascriptreact",
        server: TYPESCRIPT_SERVER,
        grammar: || tree_sitter_javascript::LANGUAGE.into(),
        queries: &[tree_sitter_javascript::HIGHLIGHT_QUERY, tree_sitter_javascript::JSX_HIGHLIGHT_QUERY], inline: None,
    },
    TypeScript {
        name: "typescript", aliases: &["ts"], extensions: &["ts", "mts", "cts"],
        filenames: &[], interpreters: &[], language_id: "typescript",
        server: TYPESCRIPT_SERVER,
        grammar: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        queries: &[tree_sitter_javascript::HIGHLIGHT_QUERY, tree_sitter_typescript::HIGHLIGHTS_QUERY], inline: None,
    },
    Tsx {
        name: "tsx", aliases: &["typescriptreact"], extensions: &["tsx"],
        filenames: &[], interpreters: &[], language_id: "typescriptreact",
        server: TYPESCRIPT_SERVER,
        grammar: || tree_sitter_typescript::LANGUAGE_TSX.into(),
        queries: &[tree_sitter_javascript::HIGHLIGHT_QUERY, tree_sitter_javascript::JSX_HIGHLIGHT_QUERY, tree_sitter_typescript::HIGHLIGHTS_QUERY], inline: None,
    },
}

impl Language {
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|language| {
            let definition = language.definition();
            definition.name.eq_ignore_ascii_case(name)
                || definition
                    .aliases
                    .iter()
                    .any(|alias| alias.eq_ignore_ascii_case(name))
        })
    }

    pub fn from_path(path: &Path) -> Option<Self> {
        Self::ALL.iter().copied().find(|language| {
            let definition = language.definition();
            path.file_name().is_some_and(|name| {
                definition
                    .filenames
                    .iter()
                    .any(|candidate| name == *candidate)
            }) || path.extension().is_some_and(|extension| {
                definition
                    .extensions
                    .iter()
                    .any(|candidate| extension.eq_ignore_ascii_case(candidate))
            })
        })
    }

    /// Detect known paths first, then a bounded shebang prefix. This reads at
    /// most 256 characters even when the first line of a file is very large.
    pub fn detect(path: Option<&Path>, text: &Rope) -> Option<Self> {
        path.and_then(Self::from_path).or_else(|| {
            if text.len_chars() < 2 || text.char(0) != '#' || text.char(1) != '!' {
                return None;
            }
            let line: String = text
                .chars()
                .take(256)
                .take_while(|ch| *ch != '\n')
                .collect();
            let mut words = line.strip_prefix("#!")?.split_whitespace();
            let mut interpreter = words.next()?.rsplit('/').next()?;
            if interpreter == "env" {
                interpreter = words
                    .find(|word| !word.starts_with('-') && !word.contains('='))?
                    .rsplit('/')
                    .next()?;
            }
            Self::ALL
                .iter()
                .copied()
                .find(|language| language.definition().interpreters.contains(&interpreter))
        })
    }

    pub fn name(self) -> &'static str {
        self.definition().name
    }

    pub fn language_id(self) -> &'static str {
        self.definition().language_id
    }

    pub fn server(self) -> Option<&'static LanguageServer> {
        self.definition().server.as_ref()
    }

    pub(super) fn configuration(self) -> &'static Configuration {
        let definition = self.definition();
        definition.compiled.get_or_init(|| {
            Configuration::new(
                (definition.grammar)(),
                definition.queries,
                definition.inline.map(|inline| Box::new(inline())),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_detects_paths_aliases_and_shebangs() {
        for (path, expected) in [
            ("README.MD", Language::Markdown),
            ("notes.markdown", Language::Markdown),
            ("script.sh", Language::Bash),
            (".bashrc", Language::Bash),
            (".profile", Language::Bash),
            ("types.d.ts", Language::TypeScript),
            ("module.mts", Language::TypeScript),
            ("view.tsx", Language::Tsx),
            ("view.jsx", Language::Jsx),
            ("module.cjs", Language::JavaScript),
        ] {
            assert_eq!(
                Language::from_path(Path::new(path)),
                Some(expected),
                "{path}"
            );
        }
        for language in Language::ALL {
            assert_eq!(Language::from_name(language.name()), Some(*language));
            for alias in language.definition().aliases {
                assert_eq!(Language::from_name(alias), Some(*language));
            }
        }
        for source in [
            "#!/bin/bash\necho hi",
            "#!/usr/bin/env -S bash -eu\necho hi",
            "#!/bin/sh",
            "#!/usr/bin/env FOO=bar dash",
        ] {
            assert_eq!(
                Language::detect(Some(Path::new("script")), &Rope::from_str(source)),
                Some(Language::Bash)
            );
        }
        assert_eq!(
            Language::detect(None, &Rope::from_str("#!/usr/bin/env python bash")),
            None
        );
        assert_eq!(
            Language::detect(None, &Rope::from_str("ordinary text")),
            None
        );
        assert_eq!(Language::from_path(Path::new("notes.txt")), None);
    }
}
