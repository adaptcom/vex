//! Built-in language registry shared by syntax, file previews, and LSP sessions.
//! Add a grammar dependency and one entry below to support another language.

use std::{num::NonZeroUsize, path::Path, sync::OnceLock};
use vex_core::Rope;

use super::Configuration;

/// Whitespace added by one indentation level, separate from tab display width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndentStyle {
    Spaces(NonZeroUsize),
    Tabs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Indentation {
    pub style: IndentStyle,
    pub tab_width: NonZeroUsize,
}

impl Indentation {
    pub const fn spaces(width: NonZeroUsize) -> Self {
        Self {
            style: IndentStyle::Spaces(width),
            tab_width: width,
        }
    }
}

impl Default for Indentation {
    fn default() -> Self {
        Self::spaces(NonZeroUsize::new(4).unwrap())
    }
}

/// Ordered comment delimiters. The first entry is used for new comments;
/// existing comments prefer the longest matching delimiter.
#[derive(Clone, Copy, Debug, Default)]
pub struct Comments {
    pub line: &'static [&'static str],
    pub block: &'static [(&'static str, &'static str)],
}

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
    indentation: Indentation,
    comments: Comments,
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

const CLANGD_SERVER: Option<LanguageServer> = Some(LanguageServer {
    command: "clangd",
    arguments: &[],
    environment: "VEX_CLANGD",
    label: "Clangd",
    root_markers: &[
        ".clangd",
        "compile_commands.json",
        "compile_flags.txt",
        "CMakeLists.txt",
        "Makefile",
    ],
    outermost_root: false,
});

const JSON_SERVER: Option<LanguageServer> = Some(LanguageServer {
    command: "vscode-json-language-server",
    arguments: &["--stdio"],
    environment: "VEX_JSON_LANGUAGE_SERVER",
    label: "JSON",
    root_markers: &["package.json"],
    outermost_root: false,
});

const TWO_SPACES: Indentation = Indentation::spaces(NonZeroUsize::new(2).unwrap());
const FOUR_SPACES: Indentation = Indentation::spaces(NonZeroUsize::new(4).unwrap());
const C_COMMENTS: Comments = Comments {
    line: &["//"],
    block: &[("/*", "*/"), ("/**", "*/")],
};

languages! {
    Rust {
        name: "rust", aliases: &["rs"], extensions: &["rs"], filenames: &[], interpreters: &[],
        language_id: "rust",
        indentation: FOUR_SPACES,
        comments: Comments { line: &["//", "///", "//!"], block: &[("/*", "*/"), ("/**", "*/"), ("/*!", "*/")] },
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
        indentation: TWO_SPACES,
        comments: Comments { line: &[], block: &[("<!--", "-->")] },
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
        indentation: TWO_SPACES,
        comments: Comments { line: &["#"], block: &[] },
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
        indentation: TWO_SPACES,
        comments: C_COMMENTS,
        server: TYPESCRIPT_SERVER,
        grammar: || tree_sitter_javascript::LANGUAGE.into(),
        queries: &[tree_sitter_javascript::HIGHLIGHT_QUERY, tree_sitter_javascript::JSX_HIGHLIGHT_QUERY], inline: None,
    },
    Jsx {
        name: "jsx", aliases: &["javascriptreact"], extensions: &["jsx"],
        filenames: &[], interpreters: &[], language_id: "javascriptreact",
        indentation: TWO_SPACES,
        comments: C_COMMENTS,
        server: TYPESCRIPT_SERVER,
        grammar: || tree_sitter_javascript::LANGUAGE.into(),
        queries: &[tree_sitter_javascript::HIGHLIGHT_QUERY, tree_sitter_javascript::JSX_HIGHLIGHT_QUERY], inline: None,
    },
    TypeScript {
        name: "typescript", aliases: &["ts"], extensions: &["ts", "mts", "cts"],
        filenames: &[], interpreters: &[], language_id: "typescript",
        indentation: TWO_SPACES,
        comments: C_COMMENTS,
        server: TYPESCRIPT_SERVER,
        grammar: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        queries: &[tree_sitter_javascript::HIGHLIGHT_QUERY, tree_sitter_typescript::HIGHLIGHTS_QUERY], inline: None,
    },
    Tsx {
        name: "tsx", aliases: &["typescriptreact"], extensions: &["tsx"],
        filenames: &[], interpreters: &[], language_id: "typescriptreact",
        indentation: TWO_SPACES,
        comments: C_COMMENTS,
        server: TYPESCRIPT_SERVER,
        grammar: || tree_sitter_typescript::LANGUAGE_TSX.into(),
        queries: &[tree_sitter_javascript::HIGHLIGHT_QUERY, tree_sitter_javascript::JSX_HIGHLIGHT_QUERY, tree_sitter_typescript::HIGHLIGHTS_QUERY], inline: None,
    },
    Python {
        name: "python", aliases: &["py"], extensions: &["py", "pyi", "pyw"],
        filenames: &["SConstruct", "SConscript"], interpreters: &["python", "python2", "python3", "pypy", "pypy3"],
        language_id: "python", indentation: FOUR_SPACES,
        comments: Comments { line: &["#"], block: &[] },
        server: Some(LanguageServer {
            command: "pyright-langserver", arguments: &["--stdio"], environment: "VEX_PYRIGHT", label: "Pyright",
            root_markers: &["pyproject.toml", "pyrightconfig.json", "setup.py", "setup.cfg", "Pipfile", "requirements.txt"], outermost_root: false,
        }),
        grammar: || tree_sitter_python::LANGUAGE.into(),
        queries: &[tree_sitter_python::HIGHLIGHTS_QUERY], inline: None,
    },
    Go {
        name: "go", aliases: &["golang"], extensions: &["go"], filenames: &[], interpreters: &[],
        language_id: "go", indentation: Indentation { style: IndentStyle::Tabs, tab_width: NonZeroUsize::new(4).unwrap() },
        comments: C_COMMENTS,
        server: Some(LanguageServer {
            command: "gopls", arguments: &[], environment: "VEX_GOPLS", label: "Go",
            root_markers: &["go.work", "go.mod"], outermost_root: true,
        }),
        grammar: || tree_sitter_go::LANGUAGE.into(),
        queries: &[tree_sitter_go::HIGHLIGHTS_QUERY], inline: None,
    },
    C {
        name: "c", aliases: &[], extensions: &["c", "h"], filenames: &[], interpreters: &[],
        language_id: "c", indentation: FOUR_SPACES, comments: C_COMMENTS,
        server: CLANGD_SERVER,
        grammar: || tree_sitter_c::LANGUAGE.into(),
        queries: &[tree_sitter_c::HIGHLIGHT_QUERY], inline: None,
    },
    Cpp {
        name: "cpp", aliases: &["c++", "cxx"], extensions: &["cpp", "cc", "cxx", "c++", "hpp", "hh", "hxx", "h++", "ipp", "tpp", "C", "H"],
        filenames: &[], interpreters: &[], language_id: "cpp", indentation: FOUR_SPACES, comments: C_COMMENTS,
        server: CLANGD_SERVER,
        grammar: || tree_sitter_cpp::LANGUAGE.into(),
        queries: &[tree_sitter_c::HIGHLIGHT_QUERY, tree_sitter_cpp::HIGHLIGHT_QUERY], inline: None,
    },
    Java {
        name: "java", aliases: &[], extensions: &["java"], filenames: &[], interpreters: &[],
        language_id: "java", indentation: FOUR_SPACES, comments: C_COMMENTS,
        server: Some(LanguageServer {
            command: "jdtls", arguments: &[], environment: "VEX_JDTLS", label: "Java",
            root_markers: &["pom.xml", "build.gradle", "build.gradle.kts", "settings.gradle", "settings.gradle.kts", "build.xml"], outermost_root: false,
        }),
        grammar: || tree_sitter_java::LANGUAGE.into(),
        queries: &[tree_sitter_java::HIGHLIGHTS_QUERY], inline: None,
    },
    CSharp {
        name: "c-sharp", aliases: &["csharp", "cs", "c#"], extensions: &["cs", "csx"], filenames: &[], interpreters: &[],
        language_id: "csharp", indentation: FOUR_SPACES, comments: C_COMMENTS,
        server: Some(LanguageServer {
            command: "csharp-ls", arguments: &[], environment: "VEX_CSHARP_LS", label: "C#",
            root_markers: &["global.json", "Directory.Build.props", "Directory.Build.targets", "nuget.config"], outermost_root: false,
        }),
        grammar: || tree_sitter_c_sharp::LANGUAGE.into(),
        queries: &[tree_sitter_c_sharp::HIGHLIGHTS_QUERY], inline: None,
    },
    Swift {
        name: "swift", aliases: &[], extensions: &["swift"], filenames: &[], interpreters: &["swift"],
        language_id: "swift", indentation: FOUR_SPACES, comments: C_COMMENTS,
        server: Some(LanguageServer {
            command: "sourcekit-lsp", arguments: &[], environment: "VEX_SOURCEKIT_LSP", label: "Swift",
            root_markers: &["Package.swift", ".sourcekit-lsp/config.json", "compile_commands.json"], outermost_root: false,
        }),
        grammar: || tree_sitter_swift::LANGUAGE.into(),
        queries: &[tree_sitter_swift::HIGHLIGHTS_QUERY], inline: None,
    },
    Ruby {
        name: "ruby", aliases: &["rb"], extensions: &["rb", "rake", "gemspec", "ru"],
        filenames: &["Gemfile", "Rakefile", "Guardfile", "Vagrantfile", "Brewfile", "Podfile", "Fastfile", "Appfile"],
        interpreters: &["ruby"], language_id: "ruby", indentation: TWO_SPACES,
        comments: Comments { line: &["#"], block: &[] },
        server: Some(LanguageServer {
            command: "ruby-lsp", arguments: &[], environment: "VEX_RUBY_LSP", label: "Ruby",
            root_markers: &["Gemfile", ".ruby-version"], outermost_root: false,
        }),
        grammar: || tree_sitter_ruby::LANGUAGE.into(),
        queries: &[tree_sitter_ruby::HIGHLIGHTS_QUERY], inline: None,
    },
    Php {
        name: "php", aliases: &[], extensions: &["php", "phtml", "php3", "php4", "php5", "php7", "php8", "phps"],
        filenames: &[], interpreters: &["php"], language_id: "php", indentation: FOUR_SPACES,
        comments: Comments { line: &["//", "#"], block: &[("/*", "*/"), ("/**", "*/")] },
        server: Some(LanguageServer {
            command: "intelephense", arguments: &["--stdio"], environment: "VEX_INTELEPHENSE", label: "PHP",
            root_markers: &["composer.json", ".php-version"], outermost_root: false,
        }),
        grammar: || tree_sitter_php::LANGUAGE_PHP.into(),
        queries: &[tree_sitter_php::HIGHLIGHTS_QUERY], inline: None,
    },
    Lua {
        name: "lua", aliases: &[], extensions: &["lua"], filenames: &[".luacheckrc"],
        interpreters: &["lua", "luajit"], language_id: "lua", indentation: TWO_SPACES,
        comments: Comments { line: &["--"], block: &[("--[[", "]]")] },
        server: Some(LanguageServer {
            command: "lua-language-server", arguments: &[], environment: "VEX_LUA_LANGUAGE_SERVER", label: "Lua",
            root_markers: &[".luarc.json", ".luarc.jsonc", ".luacheckrc", "stylua.toml", ".stylua.toml"], outermost_root: false,
        }),
        grammar: || tree_sitter_lua::LANGUAGE.into(),
        queries: &[tree_sitter_lua::HIGHLIGHTS_QUERY], inline: None,
    },
    Html {
        name: "html", aliases: &["htm"], extensions: &["html", "htm"], filenames: &[], interpreters: &[],
        language_id: "html", indentation: TWO_SPACES,
        comments: Comments { line: &[], block: &[("<!--", "-->")] },
        server: Some(LanguageServer {
            command: "vscode-html-language-server", arguments: &["--stdio"], environment: "VEX_HTML_LANGUAGE_SERVER", label: "HTML",
            root_markers: &["package.json"], outermost_root: false,
        }),
        grammar: || tree_sitter_html::LANGUAGE.into(),
        queries: &[tree_sitter_html::HIGHLIGHTS_QUERY], inline: None,
    },
    Css {
        name: "css", aliases: &[], extensions: &["css"], filenames: &[], interpreters: &[],
        language_id: "css", indentation: TWO_SPACES,
        comments: Comments { line: &[], block: &[("/*", "*/")] },
        server: Some(LanguageServer {
            command: "vscode-css-language-server", arguments: &["--stdio"], environment: "VEX_CSS_LANGUAGE_SERVER", label: "CSS",
            root_markers: &["package.json"], outermost_root: false,
        }),
        grammar: || tree_sitter_css::LANGUAGE.into(),
        queries: &[tree_sitter_css::HIGHLIGHTS_QUERY], inline: None,
    },
    Json {
        name: "json", aliases: &[], extensions: &["json"], filenames: &[".babelrc", ".prettierrc", ".eslintrc"], interpreters: &[],
        language_id: "json", indentation: TWO_SPACES,
        comments: Comments { line: &[], block: &[] }, server: JSON_SERVER,
        grammar: || tree_sitter_json::LANGUAGE.into(),
        queries: &[tree_sitter_json::HIGHLIGHTS_QUERY], inline: None,
    },
    Jsonc {
        name: "jsonc", aliases: &["json-with-comments"], extensions: &["jsonc"],
        filenames: &["tsconfig.json", "jsconfig.json"], interpreters: &[],
        language_id: "jsonc", indentation: TWO_SPACES,
        comments: C_COMMENTS, server: JSON_SERVER,
        grammar: || tree_sitter_json::LANGUAGE.into(),
        queries: &[tree_sitter_json::HIGHLIGHTS_QUERY], inline: None,
    },
    Yaml {
        name: "yaml", aliases: &["yml"], extensions: &["yaml", "yml"], filenames: &[".clangd", ".clang-format", ".clang-tidy"], interpreters: &[],
        language_id: "yaml", indentation: TWO_SPACES,
        comments: Comments { line: &["#"], block: &[] },
        server: Some(LanguageServer {
            command: "yaml-language-server", arguments: &["--stdio"], environment: "VEX_YAML_LANGUAGE_SERVER", label: "YAML",
            root_markers: &[".yamllint", ".yamllint.yaml", ".yamllint.yml"], outermost_root: false,
        }),
        grammar: || tree_sitter_yaml::LANGUAGE.into(),
        queries: &[tree_sitter_yaml::HIGHLIGHTS_QUERY], inline: None,
    },
    Toml {
        name: "toml", aliases: &[], extensions: &["toml"], filenames: &["Cargo.lock", "uv.lock", "Pipfile", "poetry.lock"], interpreters: &[],
        language_id: "toml", indentation: TWO_SPACES,
        comments: Comments { line: &["#"], block: &[] },
        server: Some(LanguageServer {
            command: "taplo", arguments: &["lsp", "stdio"], environment: "VEX_TAPLO", label: "TOML",
            root_markers: &["taplo.toml", ".taplo.toml", "Cargo.toml", "pyproject.toml"], outermost_root: false,
        }),
        grammar: || tree_sitter_toml_ng::LANGUAGE.into(),
        queries: &[tree_sitter_toml_ng::HIGHLIGHTS_QUERY], inline: None,
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
        // Explicit names (tsconfig.json) precede generic extensions (.json).
        // Exact extension matches preserve the C/C++ distinction of .c/.C;
        // other case variants such as README.MD retain the usual fallback.
        path.file_name()
            .and_then(|name| {
                Self::ALL.iter().copied().find(|language| {
                    language
                        .definition()
                        .filenames
                        .iter()
                        .any(|candidate| name == *candidate)
                })
            })
            .or_else(|| {
                let extension = path.extension()?;
                Self::ALL
                    .iter()
                    .copied()
                    .find(|language| {
                        language
                            .definition()
                            .extensions
                            .iter()
                            .any(|candidate| extension == *candidate)
                    })
                    .or_else(|| {
                        Self::ALL.iter().copied().find(|language| {
                            language
                                .definition()
                                .extensions
                                .iter()
                                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
                        })
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
            Self::ALL.iter().copied().find(|language| {
                language.definition().interpreters.iter().any(|candidate| {
                    interpreter == *candidate
                        || interpreter.strip_prefix(candidate).is_some_and(|suffix| {
                            // Recognize python3.13 and lua5.4, not python-helper.
                            !suffix.is_empty()
                                && suffix.starts_with(|ch: char| ch.is_ascii_digit())
                                && suffix.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
                        })
                })
            })
        })
    }

    pub fn name(self) -> &'static str {
        self.definition().name
    }

    pub fn language_id(self) -> &'static str {
        self.definition().language_id
    }

    pub fn indentation(self) -> Indentation {
        self.definition().indentation
    }

    pub fn comments(self) -> Comments {
        self.definition().comments
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
            ("main.py", Language::Python),
            ("types.pyi", Language::Python),
            ("SConstruct", Language::Python),
            ("main.go", Language::Go),
            ("main.c", Language::C),
            ("header.h", Language::C),
            ("main.C", Language::Cpp),
            ("header.H", Language::Cpp),
            ("main.cpp", Language::Cpp),
            ("header.hpp", Language::Cpp),
            ("Main.java", Language::Java),
            ("Main.cs", Language::CSharp),
            ("Package.swift", Language::Swift),
            ("app.rb", Language::Ruby),
            ("Gemfile", Language::Ruby),
            ("Rakefile", Language::Ruby),
            ("index.php", Language::Php),
            ("config.lua", Language::Lua),
            (".luacheckrc", Language::Lua),
            ("index.html", Language::Html),
            ("style.css", Language::Css),
            ("package.json", Language::Json),
            ("tsconfig.json", Language::Jsonc),
            ("jsconfig.json", Language::Jsonc),
            ("settings.jsonc", Language::Jsonc),
            ("config.yaml", Language::Yaml),
            (".clangd", Language::Yaml),
            ("Cargo.toml", Language::Toml),
            ("Cargo.lock", Language::Toml),
            ("Pipfile", Language::Toml),
            ("uv.lock", Language::Toml),
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
        for (source, expected) in [
            ("#!/usr/bin/python3.13\nprint('hi')", Language::Python),
            ("#!/usr/bin/env -S python3 -u", Language::Python),
            ("#!/usr/bin/env python bash", Language::Python),
            ("#!/usr/bin/env ruby", Language::Ruby),
            ("#!/usr/bin/env php", Language::Php),
            ("#!/usr/bin/env swift", Language::Swift),
            ("#!/usr/bin/lua5.4", Language::Lua),
            ("#!/usr/bin/env luajit", Language::Lua),
        ] {
            assert_eq!(
                Language::detect(None, &Rope::from_str(source)),
                Some(expected),
                "{source}"
            );
            assert_eq!(
                Language::detect(Some(Path::new("source.rs")), &Rope::from_str(source)),
                Some(Language::Rust)
            );
        }
        for interpreter in ["python-helper", "python3-config", "pythonx", "perl"] {
            assert_eq!(
                Language::detect(
                    None,
                    &Rope::from_str(&format!("#!/usr/bin/env {interpreter}"))
                ),
                None
            );
        }
        assert_eq!(
            Language::detect(None, &Rope::from_str("ordinary text")),
            None
        );
        assert_eq!(Language::from_path(Path::new("notes.txt")), None);
    }

    #[test]
    fn registry_names_and_file_rules_are_unambiguous() {
        let mut names = std::collections::HashSet::new();
        let mut extensions = std::collections::HashSet::new();
        let mut filenames = std::collections::HashSet::new();
        for language in Language::ALL {
            let definition = language.definition();
            for name in std::iter::once(&definition.name).chain(definition.aliases) {
                assert!(
                    names.insert(name.to_ascii_lowercase()),
                    "duplicate language name: {name}"
                );
            }
            for extension in definition.extensions {
                assert!(
                    extensions.insert(extension),
                    "duplicate extension: {extension}"
                );
                assert_eq!(
                    Language::from_path(Path::new(&format!("file.{extension}"))),
                    Some(*language)
                );
            }
            for filename in definition.filenames {
                assert!(filenames.insert(filename), "duplicate filename: {filename}");
                assert_eq!(Language::from_path(Path::new(filename)), Some(*language));
            }
        }
    }
}
