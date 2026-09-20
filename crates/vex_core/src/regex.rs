//! Cancellable regular expressions over rope chunks, without flattening text.
//!
//! regex-automata owns syntax and automaton construction. This adapter drives
//! bounded DFAs directly and falls back to a prioritized Thompson simulation
//! for patterns (notably Unicode word boundaries) that a DFA cannot handle.

use crate::{ByteOffset, RopeSlice};
use regex_automata::{
    Anchored,
    dfa::{Automaton, dense, regex::Regex as DfaRegex},
    nfa::thompson::{self, NFA, State, WhichCaptures},
    util::{look::Look, primitives::StateID, start, syntax},
};
use std::{fmt, ops::Range};

const MAX_PATTERN_BYTES: usize = 64 << 10;
const NFA_LIMIT: usize = 8 << 20;
const DFA_LIMIT: usize = 1 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub case_insensitive: bool,
    pub multi_line: bool,
    pub crlf: bool,
}

#[derive(Debug)]
pub struct Regex {
    nfa: NFA,
    dfa: Option<DfaRegex>,
    crosses_lf: bool,
}

impl Regex {
    /// Compile a UTF-8 pattern. Pattern and automaton sizes are bounded;
    /// compilation should run on a worker because it is not preemptible.
    pub fn new(pattern: &str, options: Options) -> Result<Self, Error> {
        if pattern.len() > MAX_PATTERN_BYTES {
            return Err(Error("regex exceeds 64 KiB".into()));
        }
        let syntax = syntax::Config::new()
            .case_insensitive(options.case_insensitive)
            .multi_line(options.multi_line)
            .crlf(options.crlf);
        let config = thompson::Config::new()
            .nfa_size_limit(Some(NFA_LIMIT))
            .which_captures(WhichCaptures::None);
        let nfa = NFA::compiler()
            .configure(config.clone())
            .syntax(syntax)
            .build(pattern)
            .map_err(|error| Error(error.to_string()))?;
        // Avoid spending determinization work on large Unicode NFAs. DFA size
        // limits are independent of the fallback's pattern-size budget.
        let dfa = (nfa.states().len() <= 2_048)
            .then(|| {
                DfaRegex::builder()
                    .syntax(syntax)
                    .thompson(config)
                    .dense(
                        dense::Config::new()
                            .unicode_word_boundary(true)
                            .dfa_size_limit(Some(DFA_LIMIT))
                            .determinize_size_limit(Some(DFA_LIMIT)),
                    )
                    .build(pattern)
                    .ok()
            })
            .flatten();
        let crosses_lf = can_consume_lf(&nfa);
        Ok(Self {
            nfa,
            dfa,
            crosses_lf,
        })
    }

    /// Whether any reachable consuming transition accepts LF. False proves
    /// matches stay within LF-delimited lines; true is conservative.
    pub fn can_cross_lf(&self) -> bool {
        self.crosses_lf
    }

    /// Whether a match can be empty (assertions may still restrict where).
    pub fn can_match_empty(&self) -> bool {
        self.nfa.has_empty()
    }

    /// Find a leftmost-first match wholly within a byte span. Assertions see
    /// the entire supplied slice, so a span does not create artificial anchors.
    /// Empty matches occur only at UTF-8 boundaries. Cancellation returns None.
    pub fn find(
        &self,
        text: RopeSlice<'_>,
        span: Range<ByteOffset>,
        cache: &mut Cache,
        cancelled: &impl Fn() -> bool,
    ) -> Option<Range<ByteOffset>> {
        if cancelled() || span.start > span.end || span.start.0 > text.len_bytes() {
            return None;
        }
        let span = span.start.0..span.end.0.min(text.len_bytes());
        let mut input = Input::new(text, span.start);
        let found = match &self.dfa {
            Some(dfa) => match find_dfa(dfa, &mut input, span.clone(), cancelled) {
                Ok(found) => found,
                Err(()) => self.find_nfa(&mut input, span, cache, cancelled),
            },
            None => self.find_nfa(&mut input, span, cache, cancelled),
        };
        if cancelled() {
            None
        } else {
            found.map(|range| ByteOffset(range.start)..ByteOffset(range.end))
        }
    }

    /// Iterate non-overlapping matches. An empty match touching the previous
    /// match's end is skipped, as in regex-automata's string iterator.
    pub fn matches<'r, 't, F: Fn() -> bool>(
        &'r self,
        text: RopeSlice<'t>,
        span: Range<ByteOffset>,
        cancelled: F,
    ) -> Matches<'r, 't, F> {
        Matches {
            regex: self,
            text,
            at: span.start.0,
            end: span.end.0.min(text.len_bytes()),
            last_end: None,
            done: false,
            cache: Cache::default(),
            cancelled,
        }
    }

    fn find_nfa(
        &self,
        input: &mut Input<'_>,
        span: Range<usize>,
        cache: &mut Cache,
        cancelled: &impl Fn() -> bool,
    ) -> Option<Range<usize>> {
        if self.nfa.is_always_start_anchored() && span.start > 0 {
            return None;
        }
        cache.current.reset(self.nfa.states().len());
        cache.next.reset(self.nfa.states().len());
        let mut found = None;
        let mut steps = 0usize;
        for at in span.start..=span.end {
            if at % 4096 == 0 && cancelled() {
                return None;
            }
            if found.is_none() && input.is_boundary(at) {
                closure(
                    &self.nfa,
                    input,
                    at,
                    self.nfa.start_anchored(),
                    at,
                    &mut cache.current,
                    &mut cache.stack,
                    &mut steps,
                    cancelled,
                )?;
            }
            cache.next.clear();
            let byte = (at < span.end).then(|| input.byte(at)).flatten();
            for &(id, start) in &cache.current.states {
                let next = match self.nfa.state(id) {
                    State::ByteRange { trans } => {
                        byte.filter(|&b| trans.matches_byte(b)).map(|_| trans.next)
                    }
                    State::Sparse(trans) => byte.and_then(|b| trans.matches_byte(b)),
                    State::Dense(trans) => byte.and_then(|b| trans.matches_byte(b)),
                    State::Match { .. } => {
                        found = Some(start..at);
                        break;
                    }
                    _ => unreachable!("epsilon closure contains only consuming and match states"),
                };
                if let Some(next) = next {
                    closure(
                        &self.nfa,
                        input,
                        at + 1,
                        next,
                        start,
                        &mut cache.next,
                        &mut cache.stack,
                        &mut steps,
                        cancelled,
                    )?;
                }
            }
            if cache.next.states.is_empty()
                && (found.is_some() || self.nfa.is_always_start_anchored())
            {
                return found;
            }
            std::mem::swap(&mut cache.current, &mut cache.next);
        }
        found
    }
}

fn can_consume_lf(nfa: &NFA) -> bool {
    // Exclude the unanchored scan prefix, whose wildcard loop is not part of a
    // match. This graph walk runs once during worker-side compilation.
    let mut seen = vec![false; nfa.states().len()];
    let mut stack = vec![nfa.start_anchored()];
    while let Some(id) = stack.pop() {
        if std::mem::replace(&mut seen[id.as_usize()], true) {
            continue;
        }
        match nfa.state(id) {
            State::ByteRange { trans } => {
                if trans.matches_byte(b'\n') {
                    return true;
                }
                stack.push(trans.next);
            }
            State::Sparse(trans) => {
                if trans.matches_byte(b'\n').is_some() {
                    return true;
                }
                stack.extend(trans.transitions.iter().map(|t| t.next));
            }
            State::Dense(trans) => {
                if trans.matches_byte(b'\n').is_some() {
                    return true;
                }
                stack.extend(
                    trans
                        .transitions
                        .iter()
                        .copied()
                        .filter(|id| *id != StateID::ZERO),
                );
            }
            State::Look { next, .. } | State::Capture { next, .. } => stack.push(*next),
            State::Union { alternates } => stack.extend(alternates.iter().copied()),
            State::BinaryUnion { alt1, alt2 } => {
                stack.push(*alt1);
                stack.push(*alt2);
            }
            State::Fail | State::Match { .. } => {}
        }
    }
    false
}

/// Scratch memory depends on the pattern, not document length. Reusable across
/// searches and patterns; DFA searches do not initialize the fallback buffers.
#[derive(Debug, Default)]
pub struct Cache {
    current: States,
    next: States,
    stack: Vec<StateID>,
}

#[derive(Debug, Default)]
struct States {
    states: Vec<(StateID, usize)>,
    seen: Vec<u32>,
    generation: u32,
}
impl States {
    fn reset(&mut self, len: usize) {
        self.seen.resize(len, 0);
        self.clear();
    }
    fn clear(&mut self) {
        self.states.clear();
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.seen.fill(0);
            self.generation = 1;
        }
    }
    fn visit(&mut self, id: StateID) -> bool {
        let seen = &mut self.seen[id.as_usize()];
        if *seen == self.generation {
            return false;
        }
        *seen = self.generation;
        true
    }
}

#[allow(clippy::too_many_arguments)]
fn closure(
    nfa: &NFA,
    input: &mut Input<'_>,
    at: usize,
    id: StateID,
    origin: usize,
    states: &mut States,
    stack: &mut Vec<StateID>,
    steps: &mut usize,
    cancelled: &impl Fn() -> bool,
) -> Option<()> {
    stack.clear();
    stack.push(id);
    while let Some(id) = stack.pop() {
        *steps = steps.wrapping_add(1);
        if (*steps).is_multiple_of(1024) && cancelled() {
            return None;
        }
        if !states.visit(id) {
            continue;
        }
        match nfa.state(id) {
            State::Look { look, next } => {
                if input.look(nfa, *look, at) {
                    stack.push(*next);
                }
            }
            State::Capture { next, .. } => stack.push(*next),
            State::Union { alternates } => stack.extend(alternates.iter().rev()),
            State::BinaryUnion { alt1, alt2 } => {
                stack.push(*alt2);
                stack.push(*alt1);
            }
            State::Fail => {}
            _ => states.states.push((id, origin)),
        }
    }
    Some(())
}

struct Input<'a> {
    text: RopeSlice<'a>,
    chunk: &'a [u8],
    start: usize,
}
impl<'a> Input<'a> {
    fn new(text: RopeSlice<'a>, at: usize) -> Self {
        let (chunk, start, _, _) = text.chunk_at_byte(at);
        Self {
            text,
            chunk: chunk.as_bytes(),
            start,
        }
    }
    fn byte(&mut self, at: usize) -> Option<u8> {
        if at >= self.text.len_bytes() {
            return None;
        }
        if at < self.start || at >= self.start + self.chunk.len() {
            let (chunk, start, _, _) = self.text.chunk_at_byte(at);
            self.chunk = chunk.as_bytes();
            self.start = start;
        }
        Some(self.chunk[at - self.start])
    }
    fn is_boundary(&mut self, at: usize) -> bool {
        self.byte(at).is_none_or(|byte| byte & 0xc0 != 0x80)
    }
    fn look(&mut self, nfa: &NFA, look: Look, at: usize) -> bool {
        match look {
            Look::Start => return at == 0,
            Look::End => return at == self.text.len_bytes(),
            _ => {}
        }
        // At most one UTF-8 scalar on each side is needed for all supported
        // assertions. Absolute anchors are handled separately above.
        let start = at.saturating_sub(4);
        let end = at.saturating_add(4).min(self.text.len_bytes());
        let mut context = [0; 8];
        for (index, pos) in (start..end).enumerate() {
            context[index] = self.byte(pos).unwrap();
        }
        nfa.look_matcher()
            .matches(look, &context[..end - start], at - start)
    }
}

// A quit state or unsupported start requests the NFA fallback. Matches are
// delayed by one byte in a DFA, including the final look-ahead/EOI transition.
fn find_dfa(
    regex: &DfaRegex,
    input: &mut Input<'_>,
    span: Range<usize>,
    cancelled: &impl Fn() -> bool,
) -> Result<Option<Range<usize>>, ()> {
    let forward = regex.forward();
    let mut state = forward
        .start_state(
            &start::Config::new()
                .anchored(Anchored::No)
                .look_behind(span.start.checked_sub(1).and_then(|at| input.byte(at))),
        )
        .map_err(|_| ())?;
    let mut end = None;
    let mut stopped = false;
    for at in span.clone() {
        if at % 4096 == 0 && cancelled() {
            return Ok(None);
        }
        state = forward.next_state(state, input.byte(at).unwrap());
        if forward.is_match_state(state) {
            end = Some(at);
        }
        if forward.is_dead_state(state) {
            stopped = true;
            break;
        }
        if forward.is_quit_state(state) {
            return Err(());
        }
    }
    if !stopped {
        state = match input.byte(span.end) {
            Some(byte) => forward.next_state(state, byte),
            None => forward.next_eoi_state(state),
        };
        if forward.is_match_state(state) {
            end = Some(span.end);
        }
        if forward.is_quit_state(state) {
            return Err(());
        }
    }
    let Some(end) = end else {
        return Ok(None);
    };
    if !input.is_boundary(end) {
        return Err(());
    }
    let reverse = regex.reverse();
    let mut state = reverse
        .start_state(
            &start::Config::new()
                .anchored(Anchored::Yes)
                .look_behind(input.byte(end)),
        )
        .map_err(|_| ())?;
    let mut start = None;
    let mut stopped = false;
    for at in (span.start..end).rev() {
        if at % 4096 == 0 && cancelled() {
            return Ok(None);
        }
        state = reverse.next_state(state, input.byte(at).unwrap());
        if reverse.is_match_state(state) {
            start = Some(at + 1);
        }
        if reverse.is_dead_state(state) {
            stopped = true;
            break;
        }
        if reverse.is_quit_state(state) {
            return Err(());
        }
    }
    if !stopped {
        state = match span.start.checked_sub(1).and_then(|at| input.byte(at)) {
            Some(byte) => reverse.next_state(state, byte),
            None => reverse.next_eoi_state(state),
        };
        if reverse.is_match_state(state) {
            start = Some(span.start);
        }
        if reverse.is_quit_state(state) {
            return Err(());
        }
    }
    let start = start.ok_or(())?;
    if !input.is_boundary(start) {
        return Err(());
    }
    Ok(Some(start..end))
}

pub struct Matches<'r, 't, F> {
    regex: &'r Regex,
    text: RopeSlice<'t>,
    at: usize,
    end: usize,
    last_end: Option<usize>,
    done: bool,
    cache: Cache,
    cancelled: F,
}
impl<F: Fn() -> bool> Iterator for Matches<'_, '_, F> {
    type Item = Range<ByteOffset>;
    fn next(&mut self) -> Option<Self::Item> {
        while !self.done {
            let Some(found) = self.regex.find(
                self.text,
                ByteOffset(self.at)..ByteOffset(self.end),
                &mut self.cache,
                &self.cancelled,
            ) else {
                self.done = true;
                return None;
            };
            if found.is_empty() && self.last_end == Some(found.end.0) {
                self.at = found.end.0 + 1;
                if self.at > self.end {
                    self.done = true;
                }
                continue;
            }
            self.at = found.end.0;
            self.last_end = Some(found.end.0);
            return Some(found);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Rope;
    use proptest::prelude::*;
    use std::cell::Cell;

    #[test]
    fn line_crossing_analysis_excludes_only_the_unanchored_scan_prefix() {
        for (pattern, crosses) in [
            ("value", false),
            (r"\b\w+\b", false),
            ("(?m)^.*$", false),
            (r"[^\n]+", false),
            ("", false),
            ("(?s:.)", true),
            (r"\s+", true),
            (r"a\r?\nb", true),
            ("a|\\x0A", true),
        ] {
            assert_eq!(
                Regex::new(pattern, Options::default())
                    .unwrap()
                    .can_cross_lf(),
                crosses,
                "{pattern}"
            );
        }
    }

    fn compare(pattern: &str, source: &str, span: Range<usize>, options: Options) {
        let flat = regex_automata::meta::Regex::builder()
            .syntax(
                syntax::Config::new()
                    .case_insensitive(options.case_insensitive)
                    .multi_line(options.multi_line)
                    .crlf(options.crlf),
            )
            .build(pattern)
            .unwrap();
        let rope = Rope::from_str(source);
        let regex = Regex::new(pattern, options).unwrap();
        let expected = flat
            .find_iter(regex_automata::Input::new(source).range(span.clone()))
            .map(|found| ByteOffset(found.start())..ByteOffset(found.end()))
            .collect::<Vec<_>>();
        let found = regex
            .matches(
                rope.slice(..),
                ByteOffset(span.start)..ByteOffset(span.end),
                || false,
            )
            .collect::<Vec<_>>();
        assert_eq!(found, expected, "{pattern:?}, {source:?}, {span:?}");
        // Check the fallback separately, including patterns handled by a DFA.
        let regex = Regex {
            nfa: regex.nfa,
            dfa: None,
            crosses_lf: regex.crosses_lf,
        };
        let found = regex
            .matches(
                rope.slice(..),
                ByteOffset(span.start)..ByteOffset(span.end),
                || false,
            )
            .collect::<Vec<_>>();
        assert_eq!(found, expected, "NFA: {pattern:?}, {source:?}, {span:?}");
    }

    #[test]
    fn regex_syntax_priority_anchors_unicode_and_empty_matches_agree_with_flat_engine() {
        let source = "ababa ABC ab12\r\nαβ界 e\u{301} 👩\u{200d}💻\nKelvin K ſ\n";
        for pattern in [
            "",
            "a|ab",
            "ab|a",
            "a.*b",
            "a.*?b",
            "a*",
            "a+?",
            "(a?)*",
            "(?m)^|$",
            "(?mR)^.*$",
            r"\b\w+\b",
            r"\B",
            r"\Aab.*",
            r"\z",
            "(?i)k|s",
            r"[α-ω]+|\p{Han}",
            r"(?s:.*)",
            r"[a-z&&[^b]]+",
            "(?:aba){1,2}",
        ] {
            for start in 0..=source.len() {
                compare(pattern, source, start..source.len(), Options::default());
            }
        }
    }

    #[test]
    fn matches_cross_chunks_and_slice_assertions_have_real_context() {
        let source = format!(
            "{}e\u{301}\r\n{}αβ{}",
            "a".repeat(1021),
            "b".repeat(1100),
            "界".repeat(400)
        );
        for pattern in [
            r"a+e\pM",
            r"(?m)^b+α",
            r"\bαβ界+\b",
            r"(?s)a.*β",
            r"(?mR)$",
            r"\B",
        ] {
            compare(pattern, &source, 0..source.len(), Options::default());
            compare(pattern, &source, 1000..2300, Options::default());
        }
        let text = Rope::from_str("prefixab suffix");
        let regex = Regex::new(r"\Aab\z", Options::default()).unwrap();
        assert!(
            regex
                .find(
                    text.slice(..),
                    ByteOffset(6)..ByteOffset(8),
                    &mut Cache::default(),
                    &|| false
                )
                .is_none()
        );
        assert_eq!(
            regex.find(
                text.slice(6..8),
                ByteOffset(0)..ByteOffset(2),
                &mut Cache::default(),
                &|| false
            ),
            Some(ByteOffset(0)..ByteOffset(2))
        );
    }

    #[test]
    fn cancellation_and_limits_stop_work_and_caches_remain_reusable() {
        let rope = Rope::from_str(&"a".repeat(100_000));
        for pattern in ["z", r"\b\w+z", "(?s).+z"] {
            let regex = Regex::new(pattern, Options::default()).unwrap();
            let checks = Cell::new(0);
            let cancel = || {
                checks.set(checks.get() + 1);
                checks.get() > 3
            };
            let mut cache = Cache::default();
            assert!(
                regex
                    .find(
                        rope.slice(..),
                        ByteOffset(0)..ByteOffset(rope.len_bytes()),
                        &mut cache,
                        &cancel
                    )
                    .is_none()
            );
            assert!(checks.get() < 10);
            let small = Rope::from_str("z");
            let plain = Regex::new("z", Options::default()).unwrap();
            assert_eq!(
                plain.find(
                    small.slice(..),
                    ByteOffset(0)..ByteOffset(1),
                    &mut cache,
                    &|| false
                ),
                Some(ByteOffset(0)..ByteOffset(1))
            );
        }
        assert!(Regex::new("[", Options::default()).is_err());
        assert!(Regex::new(&"x".repeat(MAX_PATTERN_BYTES + 1), Options::default()).is_err());
        assert!(Regex::new("a{100000000}", Options::default()).is_err());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]
        #[test]
        fn chunked_matching_agrees_with_flat_regex(
            parts in prop::collection::vec(prop_oneof![Just("a"), Just("b"), Just(" "), Just("界"), Just("α"), Just("\r\n"), Just("e\u{301}")], 0..80),
            atom in prop::sample::select(vec!["a", "[ab]", "\\w", "\\b", "界", ".", "(?:a|ab)", "[α-ω]", "\\s", "(?:a?)"]),
            suffix in prop::sample::select(vec!["", "?", "*", "+", "{1,3}", "+?", "*?", "|b"]),
            begin in any::<usize>(), end in any::<usize>(), insensitive in any::<bool>(), crlf in any::<bool>(),
        ) {
            let source = parts.concat();
            let start = begin % (source.len() + 1);
            let end = end % (source.len() + 1);
            compare(&format!("{atom}{suffix}"), &source, start.min(end)..start.max(end), Options { case_insensitive: insensitive, multi_line: true, crlf });
        }
    }
}
