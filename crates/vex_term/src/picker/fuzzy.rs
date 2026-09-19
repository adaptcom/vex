//! Owned fuzzy ranking. Each whitespace-separated word must be a subsequence.
//! Smart case, path/word boundaries, and consecutive characters affect scoring.

const MISS: i32 = i32::MIN / 4;

pub(super) struct Query {
    words: Vec<Vec<char>>,
    sensitive: bool,
}

impl Query {
    pub fn new(text: &str) -> Self {
        let sensitive = text.chars().any(char::is_uppercase);
        Self {
            words: text
                .split_whitespace()
                .map(|word| word.chars().map(|c| fold(c, sensitive)).collect())
                .collect(),
            sensitive,
        }
    }
}

fn fold(ch: char, sensitive: bool) -> char {
    if sensitive {
        ch
    } else {
        ch.to_lowercase().next().unwrap_or(ch)
    }
}

/// Scratch storage is reused across candidates. Traces are built only for the
/// retained results, not for every discovered path. Indices are UTF-8 offsets.
#[derive(Default)]
pub(super) struct Matcher {
    chars: Vec<(usize, char)>,
    previous: Vec<i32>,
    current: Vec<i32>,
    trace: Vec<usize>,
}

impl Matcher {
    pub fn score(&mut self, text: &str, query: &Query) -> Option<i32> {
        self.run(text, query, None)
    }

    pub fn indices(&mut self, text: &str, query: &Query) -> Vec<usize> {
        let mut indices = Vec::new();
        self.run(text, query, Some(&mut indices));
        indices.sort_unstable();
        indices.dedup();
        indices
    }

    fn run(
        &mut self,
        text: &str,
        query: &Query,
        mut indices: Option<&mut Vec<usize>>,
    ) -> Option<i32> {
        if query.words.is_empty() {
            return Some(0);
        }
        self.chars.clear();
        self.chars.extend(text.char_indices());
        let n = self.chars.len();
        self.previous.resize(n, MISS);
        self.current.resize(n, MISS);
        let basename = self
            .chars
            .iter()
            .rposition(|(_, c)| matches!(c, '/' | '\\'))
            .map_or(0, |i| i + 1);
        let mut total = 0;
        for word in &query.words {
            // Cheap rejection before the dynamic program, especially valuable
            // while a query filters a large directory tree.
            let mut needle = 0;
            for &(_, ch) in &self.chars {
                if fold(ch, query.sensitive) == word[needle] {
                    needle += 1;
                    if needle == word.len() {
                        break;
                    }
                }
            }
            if needle != word.len() {
                return None;
            }
            if indices.is_some() {
                self.trace.resize(n * word.len(), usize::MAX);
            }
            self.previous.fill(MISS);
            for (row, &wanted) in word.iter().enumerate() {
                self.current.fill(MISS);
                let mut best = (MISS, usize::MAX);
                for (column, &(_, ch)) in self.chars.iter().enumerate() {
                    if column > 0 && self.previous[column - 1] != MISS {
                        let score = self.previous[column - 1] + column as i32 - 1;
                        if score > best.0 {
                            best = (score, column - 1);
                        }
                    }
                    if fold(ch, query.sensitive) != wanted {
                        continue;
                    }
                    let boundary = column == 0 || {
                        let previous = self.chars[column - 1].1;
                        !previous.is_alphanumeric()
                            || (previous.is_lowercase() && ch.is_uppercase())
                    };
                    let reward =
                        16 + if boundary { 12 } else { 0 } + if column >= basename { 4 } else { 0 };
                    let (score, predecessor) = if row == 0 {
                        (reward - (column.min(32) / 4) as i32, usize::MAX)
                    } else {
                        let mut candidate = (best.0 - column as i32 + 1, best.1);
                        if column > 0
                            && self.previous[column - 1] != MISS
                            && self.previous[column - 1] + 10 >= candidate.0
                        {
                            candidate = (self.previous[column - 1] + 10, column - 1);
                        }
                        if candidate.1 == usize::MAX {
                            continue;
                        }
                        (candidate.0 + reward, candidate.1)
                    };
                    self.current[column] = score;
                    if indices.is_some() {
                        self.trace[row * n + column] = predecessor;
                    }
                }
                std::mem::swap(&mut self.previous, &mut self.current);
            }
            let (end, score) = self
                .previous
                .iter()
                .enumerate()
                .filter(|(_, score)| **score != MISS)
                .map(|(i, score)| (i, *score - (n - i - 1).min(16) as i32))
                .max_by_key(|(i, score)| (*score, std::cmp::Reverse(*i)))?;
            total += score;
            if let Some(indices) = &mut indices {
                let mut column = end;
                for row in (0..word.len()).rev() {
                    indices.push(self.chars[column].0);
                    column = self.trace[row * n + column];
                }
            }
        }
        Some(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_contiguous_words_and_basenames_and_handles_smart_case_and_unicode() {
        let mut matcher = Matcher::default();
        let query = Query::new("app");
        assert!(
            matcher.score("src/app.rs", &query)
                > matcher.score("src/a_long_path_parser.rs", &query)
        );
        assert!(matcher.score("src/app.rs", &query) > matcher.score("app/src/other.rs", &query));
        assert!(matcher.score("src/App.rs", &query).is_some());
        assert!(matcher.score("src/app.rs", &Query::new("App")).is_none());
        assert_eq!(
            matcher.indices("src/界🦀e\u{301}.rs", &Query::new("界🦀e")),
            vec![4, 7, 11]
        );
        assert!(
            matcher
                .score("src/files/picker.rs", &Query::new("pick src"))
                .is_some()
        );
        assert_eq!(matcher.score("anything", &Query::new("   ")), Some(0));
        assert!(matcher.score("ab", &Query::new("aaa")).is_none());
    }

    proptest::proptest! {
        #[test]
        fn matching_agrees_with_a_subsequence_oracle_and_indices_spell_the_query(
            text in "[a-zA-Z/_.]{0,80}", needle in "[a-z]{1,12}"
        ) {
            let mut remaining = needle.chars().peekable();
            for ch in text.chars() {
                if remaining.peek() == Some(&ch.to_ascii_lowercase()) { remaining.next(); }
            }
            let mut matcher = Matcher::default();
            let query = Query::new(&needle);
            proptest::prop_assert_eq!(matcher.score(&text, &query).is_some(), remaining.next().is_none());
            if matcher.score(&text, &query).is_some() {
                let indices = matcher.indices(&text, &query);
                let matched: String = indices.iter().map(|&i| text[i..].chars().next().unwrap().to_ascii_lowercase()).collect();
                proptest::prop_assert_eq!(matched, needle);
                proptest::prop_assert!(indices.windows(2).all(|pair| pair[0] < pair[1]));
            }
        }
    }
}
