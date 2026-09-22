//! Project-local ignore rules, owned by Vex. Last match wins; deeper files take
//! precedence. The walker prunes ignored directories, so their children cannot
//! be re-included. No Git process or third-party glob engine is involved.

use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::Arc,
};
use vex_editor::background::Cancellation;

#[derive(Default)]
pub(super) struct Scratch {
    text: Vec<char>,
    next: Vec<bool>,
    row: Vec<bool>,
}

#[derive(Debug)]
enum Token {
    Literal(char),
    Any,
    Star,
    Recursive,
    Directories,
    Class {
        negate: bool,
        ranges: Vec<(char, char)>,
    },
}

#[derive(Debug)]
struct Rule {
    tokens: Vec<Token>,
    literal: Option<String>,
    include: bool,
    directory: bool,
    anchored: bool,
}

impl Rule {
    fn parse(mut line: &str) -> Option<Self> {
        while line.ends_with(' ') {
            let slashes = line[..line.len() - 1]
                .chars()
                .rev()
                .take_while(|&c| c == '\\')
                .count();
            if slashes % 2 == 1 {
                break;
            }
            line = &line[..line.len() - 1];
        }
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let include = line.starts_with('!');
        if include {
            line = &line[1..];
        }
        let directory = line.ends_with('/');
        if directory {
            line = &line[..line.len() - 1];
        }
        let anchored = line.contains('/');
        line = line.strip_prefix('/').unwrap_or(line);
        if line.is_empty() {
            return None;
        }
        let chars: Vec<_> = line.chars().collect();
        let mut tokens = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '\\' => {
                    i += 1;
                    tokens.push(Token::Literal(*chars.get(i)?));
                }
                '?' => tokens.push(Token::Any),
                '*' => {
                    let start = i;
                    while chars.get(i + 1) == Some(&'*') {
                        i += 1;
                    }
                    let boundary = start == 0 || chars[start - 1] == '/';
                    if i > start && boundary && chars.get(i + 1) == Some(&'/') {
                        tokens.push(Token::Directories);
                        i += 1;
                    } else if i > start && boundary && i + 1 == chars.len() {
                        tokens.push(Token::Recursive);
                    } else {
                        tokens.push(Token::Star);
                    }
                }
                '[' => {
                    if let Some((token, end)) = class(&chars, i + 1) {
                        tokens.push(token);
                        i = end;
                    } else {
                        tokens.push(Token::Literal('['));
                    }
                }
                ch => tokens.push(Token::Literal(ch)),
            }
            i += 1;
        }
        let literal = tokens
            .iter()
            .map(|token| match token {
                Token::Literal(ch) => Some(*ch),
                _ => None,
            })
            .collect();
        Some(Self {
            tokens,
            literal,
            include,
            directory,
            anchored,
        })
    }

    fn matches(
        &self,
        text: &str,
        directory: bool,
        scratch: &mut Scratch,
        cancellation: &Cancellation,
    ) -> Option<bool> {
        if self.directory && !directory {
            return Some(false);
        }
        let text = if self.anchored {
            text
        } else {
            text.rsplit('/').next().unwrap_or(text)
        };
        if let Some(literal) = &self.literal {
            return Some(literal == text);
        }
        scratch.text.clear();
        scratch.text.extend(text.chars());
        let text = &scratch.text;
        // Iterative DP avoids recursive/exponential wildcard backtracking.
        let next = &mut scratch.next;
        let row = &mut scratch.row;
        next.resize(text.len() + 1, false);
        next.fill(false);
        row.resize(text.len() + 1, false);
        next[text.len()] = true;
        for token in self.tokens.iter().rev() {
            if cancellation.is_cancelled() {
                return None;
            }
            row.fill(false);
            let mut through_directory = false;
            for i in (0..=text.len()).rev() {
                let ch = text.get(i).copied();
                row[i] = match token {
                    Token::Star => next[i] || (ch.is_some_and(|c| c != '/') && row[i + 1]),
                    Token::Recursive => next[i] || (ch.is_some() && row[i + 1]),
                    Token::Directories => {
                        if ch == Some('/') {
                            through_directory |= next[i + 1];
                        }
                        next[i] || through_directory
                    }
                    Token::Literal(wanted) => ch == Some(*wanted) && next[i + 1],
                    Token::Any => ch.is_some_and(|c| c != '/') && next[i + 1],
                    Token::Class { negate, ranges } => {
                        ch.is_some_and(|c| {
                            c != '/'
                                && (ranges.iter().any(|&(start, end)| start <= c && c <= end)
                                    != *negate)
                        }) && next[i + 1]
                    }
                };
            }
            std::mem::swap(next, row);
        }
        Some(next[0])
    }
}

fn class(chars: &[char], mut i: usize) -> Option<(Token, usize)> {
    let negate = matches!(chars.get(i), Some('!' | '^'));
    if negate {
        i += 1;
    }
    let mut ranges = Vec::new();
    loop {
        let mut ch = *chars.get(i)?;
        if ch == ']' && !ranges.is_empty() {
            return Some((Token::Class { negate, ranges }, i));
        }
        if ch == '[' && chars.get(i + 1) == Some(&':') {
            let end = (i + 2..chars.len().saturating_sub(1))
                .find(|&j| chars[j] == ':' && chars[j + 1] == ']')?;
            let name: String = chars[i + 2..end].iter().collect();
            let matches: fn(&u8) -> bool = match name.as_str() {
                "alnum" => u8::is_ascii_alphanumeric,
                "alpha" => u8::is_ascii_alphabetic,
                "blank" => |c| matches!(c, b' ' | b'\t'),
                "cntrl" => u8::is_ascii_control,
                "digit" => u8::is_ascii_digit,
                "graph" => u8::is_ascii_graphic,
                "lower" => u8::is_ascii_lowercase,
                "print" => |c| c.is_ascii_graphic() || *c == b' ',
                "punct" => u8::is_ascii_punctuation,
                "space" => u8::is_ascii_whitespace,
                "upper" => u8::is_ascii_uppercase,
                "xdigit" => u8::is_ascii_hexdigit,
                _ => return None,
            };
            ranges.extend(
                (0u8..=127)
                    .filter(matches)
                    .map(|c| (char::from(c), char::from(c))),
            );
            i = end + 2;
            continue;
        }
        if ch == '\\' {
            i += 1;
            ch = *chars.get(i)?;
        }
        if chars.get(i + 1) == Some(&'-') && chars.get(i + 2).is_some_and(|&c| c != ']') {
            i += 2;
            let mut end = chars[i];
            if end == '\\' {
                i += 1;
                end = *chars.get(i)?;
            }
            ranges.push((ch, end));
        } else {
            ranges.push((ch, ch));
        }
        i += 1;
    }
}

#[derive(Debug)]
pub(super) struct Rules {
    base: PathBuf,
    rules: Vec<Rule>,
    parent: Option<Arc<Rules>>,
}

impl Rules {
    pub fn new(base: PathBuf, text: &str, parent: Option<Arc<Self>>) -> Self {
        Self {
            base,
            rules: text.lines().filter_map(Rule::parse).collect(),
            parent,
        }
    }

    pub fn check(
        &self,
        path: &Path,
        directory: bool,
        scratch: &mut Scratch,
        cancellation: &Cancellation,
    ) -> Option<bool> {
        // Git metadata is always hidden, including worktree .git files.
        if path.file_name().is_some_and(|name| name == ".git") {
            return Some(true);
        }
        if let Ok(relative) = path.strip_prefix(&self.base) {
            let label = relative.to_string_lossy();
            #[cfg(windows)]
            let label = label.replace('\\', "/");
            for rule in self.rules.iter().rev() {
                if cancellation.is_cancelled() {
                    return None;
                }
                if rule.matches(&label, directory, scratch, cancellation)? {
                    return Some(!rule.include);
                }
            }
        }
        match &self.parent {
            Some(parent) => parent.check(path, directory, scratch, cancellation),
            None => Some(false),
        }
    }

    /// Read one directory's bounded ignore files, sharing the discovery policy.
    pub(super) fn load(
        path: &Path,
        parent: Option<Arc<Rules>>,
        repository: bool,
        notice: &mut String,
    ) -> Arc<Rules> {
        let mut contents = String::new();
        // Repository exclusions have lower precedence than .gitignore; .ignore
        // lets projects configure this picker without altering Git's policy.
        let names: &[&str] = if repository {
            &[".git/info/exclude", ".gitignore", ".ignore"]
        } else {
            &[".gitignore", ".ignore"]
        };
        for name in names {
            let file = path.join(name);
            // Never open a symlink or special file as an ignore configuration.
            if !fs::symlink_metadata(&file).is_ok_and(|m| m.is_file()) {
                continue;
            }
            let result = File::open(&file).and_then(|file| {
                let mut bytes = Vec::new();
                file.take((64 << 10) + 1).read_to_end(&mut bytes)?;
                if bytes.len() > 64 << 10 {
                    return Err(io::Error::other("ignore file exceeds 64 KiB"));
                }
                String::from_utf8(bytes).map_err(io::Error::other)
            });
            match result {
                Ok(text) => {
                    contents.push_str(&text);
                    contents.push('\n');
                }
                Err(error) => *notice = format!("{}: {error}", crate::paths::display(&file)),
            }
        }
        Arc::new(Rules::new(path.into(), &contents, parent))
    }

    #[cfg(test)]
    fn ignored(&self, path: &Path, directory: bool) -> bool {
        self.check(
            path,
            directory,
            &mut Scratch::default(),
            &Cancellation::default(),
        )
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_patterns_cover_anchors_globstars_classes_escapes_and_directory_rules() {
        for (pattern, yes, no) in [
            ("*.rs", "src/main.rs", "src/main.rs.bak"),
            ("/main.rs", "main.rs", "src/main.rs"),
            ("a/**/b", "a/x/y/b", "a/xxb"),
            ("a/**/b", "a/b", "x/a/b"),
            ("**/foo/bar", "foo/bar", "foo/x/bar"),
            ("**/foo/bar", "x/foo/bar", "x/bar"),
            ("a/**", "a/b/c", "a"),
            ("a?c.[ch]", "src/abc.c", "src/abc.rs"),
            ("[!0-9].txt", "a.txt", "1.txt"),
            ("[[:digit:]].txt", "1.txt", "a.txt"),
            ("[![:alpha:]0-9].txt", "_.txt", "a.txt"),
            ("\\#literal", "#literal", "literal"),
            ("\\!literal", "!literal", "literal"),
            ("name\\ ", "name ", "name"),
            ("name   ", "name", "name "),
            ("file\\*", "file*", "fileanything"),
        ] {
            let rules = Rules::new(PathBuf::new(), pattern, None);
            assert!(rules.ignored(Path::new(yes), false), "{pattern}: {yes}");
            assert!(!rules.ignored(Path::new(no), false), "{pattern}: {no}");
        }
        let rules = Rules::new(PathBuf::new(), "build/\n#comment\ninvalid\\\n", None);
        assert!(rules.ignored(Path::new("src/build"), true));
        assert!(!rules.ignored(Path::new("src/build"), false));
    }

    #[test]
    fn nested_rules_override_parent_patterns_and_are_relative_to_their_directory() {
        let parent = Arc::new(Rules::new(
            PathBuf::new(),
            "*.log\n!keep.log\n/root.txt",
            None,
        ));
        let child = Rules::new(
            "src".into(),
            "!debug.log\nkeep.log\n/local.txt",
            Some(parent),
        );
        assert!(!child.ignored(Path::new("src/debug.log"), false));
        assert!(child.ignored(Path::new("src/keep.log"), false));
        assert!(child.ignored(Path::new("src/local.txt"), false));
        assert!(!child.ignored(Path::new("src/nested/local.txt"), false));
        assert!(!child.ignored(Path::new("src/root.txt"), false));
    }
}
