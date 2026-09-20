//! Worker-only matching. Result storage is bounded independently of document
//! size; navigation counts wrap arithmetically rather than repeating scans.

use super::SearchPrompt;
use crate::Error;
use std::{collections::VecDeque, ops::Range};
use vex_core::{
    ByteOffset, CharOffset, Rope, Selection, SelectionSet, grapheme,
    regex::{Cache, Regex},
};

const MAX_SELECTIONS: usize = 100_000;
const REVERSE_WINDOW: usize = 512;

pub(super) fn apply(
    text: &Rope,
    origins: &SelectionSet,
    regex: &Regex,
    operation: SearchPrompt,
    count: usize,
    extend: bool,
    cancelled: &impl Fn() -> bool,
) -> Result<Option<SelectionSet>, Error> {
    if matches!(operation, SearchPrompt::Forward | SearchPrompt::Backward) {
        if extend || origins.ranges().len() > 1 {
            return navigate_set(text, origins, regex, operation, count, extend, cancelled);
        }
        return navigate(text, origins, regex, operation, count, cancelled);
    }
    let mut result = Vec::new();
    let mut cache = Cache::default();
    for origin in origins.ranges() {
        if cancelled() {
            return Ok(None);
        }
        let span = ByteOffset(text.char_to_byte(origin.start().0))
            ..ByteOffset(text.char_to_byte(origin.end().0));
        if matches!(operation, SearchPrompt::Keep | SearchPrompt::Remove) {
            if regex
                .find(text.slice(..), span, &mut cache, cancelled)
                .is_some()
                != (operation == SearchPrompt::Remove)
            {
                push(&mut result, *origin)?;
            }
            continue;
        }
        if operation == SearchPrompt::Split && origin.is_empty() {
            push(&mut result, *origin)?;
            continue;
        }
        let mut at = span.start;
        for found in regex.matches(text.slice(..), span.clone(), cancelled) {
            if cancelled() {
                return Ok(None);
            }
            let selected = if operation == SearchPrompt::Select {
                // A zero-width match at the range's exclusive edge lies outside it.
                if found.is_empty() && found.end == span.end {
                    continue;
                }
                found.clone()
            } else {
                at..found.start
            };
            push(&mut result, normalized(text, selected)?)?;
            at = found.end;
        }
        if operation == SearchPrompt::Split && at < span.end {
            push(&mut result, normalized(text, at..span.end)?)?;
        }
    }
    if cancelled() || result.is_empty() {
        return Ok(None);
    }
    Ok(Some(SelectionSet::new(result, 0)?))
}

/// Preserve the effect of each intermediate merge. A counted command may
/// swallow an existing range and change where its following search starts.
/// Updating an ordered map avoids sorting/copying every selection per match.
#[allow(clippy::too_many_arguments)]
fn navigate_set(
    text: &Rope,
    origins: &SelectionSet,
    regex: &Regex,
    operation: SearchPrompt,
    mut remaining: usize,
    extend: bool,
    cancelled: &impl Fn() -> bool,
) -> Result<Option<SelectionSet>, Error> {
    let mut ranges: std::collections::BTreeMap<_, _> = origins
        .ranges()
        .iter()
        .map(|range| (range.start(), *range))
        .collect();
    let mut primary = origins.primary();
    let mut cycle = None;
    while remaining > 0 {
        if cancelled() {
            return Ok(None);
        }
        // Once a normal-mode command has merged away all other ranges, the
        // bounded-pass count calculation applies to the remaining work.
        let steps = if !extend && ranges.len() == 1 {
            remaining
        } else {
            1
        };
        let Some(next) = navigate(
            text,
            &SelectionSet::single(primary),
            regex,
            operation,
            steps,
            cancelled,
        )?
        else {
            return Ok(None);
        };
        if !extend {
            ranges.remove(&primary.start());
        }
        let mut selected = next.primary();
        let mut removed = 0;
        let mut unchanged = false;
        loop {
            let candidate = ranges
                .range(..=selected.start())
                .next_back()
                .filter(|(_, range)| {
                    range.start() == selected.start() || range.end() > selected.start()
                })
                .or_else(|| {
                    ranges
                        .range(selected.start()..)
                        .next()
                        .filter(|(_, range)| {
                            range.start() < selected.end() || range.start() == selected.start()
                        })
                })
                .map(|(&key, &range)| (key, range));
            let Some((key, overlap)) = candidate else {
                break;
            };
            ranges.remove(&key);
            removed += 1;
            let start = selected.start().min(overlap.start());
            let end = selected.end().max(overlap.end());
            selected = if primary.is_backward() {
                Selection::new(end, start)
            } else {
                Selection::new(start, end)
            };
            unchanged = removed == 1 && selected == overlap;
            if cancelled() {
                return Ok(None);
            }
        }
        let structural_change = if extend {
            !unchanged || removed != 1
        } else {
            removed != 0
        };
        if extend && ranges.len() >= MAX_SELECTIONS {
            return Err(Error::SelectionLimit);
        }
        ranges.insert(selected.start(), selected);
        primary = selected;
        remaining -= steps;
        if structural_change {
            cycle = None;
        }
        if let Some((start, before)) = cycle {
            if primary == start {
                remaining %= before - remaining;
                cycle = None;
            }
        } else {
            cycle = Some((primary, remaining));
        }
    }
    let ranges: Vec<_> = ranges.into_values().collect();
    let index = ranges
        .binary_search_by_key(&primary.start(), |range| range.start())
        .unwrap();
    Ok(Some(SelectionSet::new(ranges, index)?))
}

fn normalized(text: &Rope, bytes: Range<ByteOffset>) -> Result<Selection, Error> {
    let start = grapheme::floor(text, CharOffset(text.byte_to_char(bytes.start.0)))?;
    let end = grapheme::ceil(text, CharOffset(text.byte_to_char(bytes.end.0)))?;
    let end = if start == end {
        grapheme::next(text, start, 1)?
    } else {
        end
    };
    Ok(Selection::new(start, end))
}

fn push(result: &mut Vec<Selection>, selection: Selection) -> Result<(), Error> {
    if result.len() == MAX_SELECTIONS {
        return Err(Error::SelectionLimit);
    }
    result.push(selection);
    Ok(())
}

struct Scan {
    seen: usize,
    selected: Option<Selection>,
    needs_steps: bool,
}

/// Search backward through expanding windows separated by impassable LF bytes.
/// Cross-line patterns retain the full-prefix path; assertions always see the
/// complete rope, including at a window boundary.
fn scan(
    text: &Rope,
    regex: &Regex,
    span: Range<ByteOffset>,
    backward: bool,
    wanted: usize,
    cancelled: &impl Fn() -> bool,
) -> Result<Scan, Error> {
    if !backward || regex.can_cross_lf() {
        return scan_span(text, regex, span, backward, wanted, false, cancelled);
    }
    let mut result = Scan {
        seen: 0,
        selected: None,
        needs_steps: false,
    };
    let mut end = span.end;
    let mut lines = 1;
    loop {
        if cancelled() {
            return Ok(result);
        }
        let row = text.byte_to_line(end.0);
        let start = ByteOffset(text.line_to_byte((row + 1).saturating_sub(lines))).max(span.start);
        // Ropey also recognizes CR, NEL and Unicode separators. Only split at
        // LF, whose consuming transitions were checked during compilation.
        let start = if start > span.start && text.byte(start.0 - 1) != b'\n' {
            span.start
        } else {
            start
        };
        let part = scan_span(
            text,
            regex,
            start..end,
            true,
            wanted - result.seen,
            end != span.end,
            cancelled,
        )?;
        result.needs_steps |= part.needs_steps;
        result.seen += part.seen;
        result.selected = part.selected;
        if result.seen >= wanted || start == span.start || (wanted > 1 && result.needs_steps) {
            return Ok(result);
        }
        end = start;
        lines = if lines == 1 {
            2
        } else {
            lines.saturating_mul(16)
        };
    }
}

/// Retain a bounded reverse tail. Counts exceeding that tail need a second
/// pass, never an allocation proportional to every match in the document.
fn scan_span(
    text: &Rope,
    regex: &Regex,
    span: Range<ByteOffset>,
    backward: bool,
    wanted: usize,
    exclude_end: bool,
    cancelled: &impl Fn() -> bool,
) -> Result<Scan, Error> {
    if !backward {
        return scan_forward(text, regex, span, wanted, cancelled);
    }
    let mut seen = 0;
    let mut selected = None;
    let mut needs_steps = false;
    let mut tail = VecDeque::new();
    let keep = wanted.min(REVERSE_WINDOW);
    for found in regex.matches(text.slice(..), span.clone(), cancelled) {
        if cancelled() {
            break;
        }
        if found.end.0 == 0 || (exclude_end && found.is_empty() && found.end == span.end) {
            continue;
        }
        let range = normalized(text, found.clone())?;
        needs_steps |= found.is_empty()
            || text.char_to_byte(range.start().0) != found.start.0
            || text.char_to_byte(range.end().0) != found.end.0;
        seen += 1;
        if tail.len() == keep {
            tail.pop_front();
        }
        tail.push_back(range);
    }
    if seen >= wanted {
        selected = if wanted <= keep {
            tail.front().copied()
        } else if !needs_steps {
            scan_forward(text, regex, span, seen - wanted + 1, cancelled)?.selected
        } else {
            None
        };
    }
    Ok(Scan {
        seen,
        selected,
        needs_steps,
    })
}

fn scan_forward(
    text: &Rope,
    regex: &Regex,
    span: Range<ByteOffset>,
    wanted: usize,
    cancelled: &impl Fn() -> bool,
) -> Result<Scan, Error> {
    let mut at = span.start;
    let mut result = Scan {
        seen: 0,
        selected: None,
        needs_steps: false,
    };
    let mut cache = Cache::default();
    while let Some(found) = regex.find(text.slice(..), at..span.end, &mut cache, cancelled) {
        if cancelled() {
            break;
        }
        let range = normalized(text, found.clone())?;
        if found.end.0 != 0 {
            result.seen += 1;
            result.selected = Some(range);
            if range.is_empty() {
                result.seen = wanted;
            }
            if result.seen == wanted {
                break;
            }
        }
        let next = ByteOffset(text.char_to_byte(range.end().0));
        if next <= at {
            break;
        }
        at = next;
    }
    Ok(result)
}

/// Grapheme expansion and zero-width matches can change a subsequent reverse
/// prefix's matches. Step through those cases with constant-space cycle
/// detection; ordinary aligned matches keep the bounded-pass fast path.
fn navigate_steps(
    text: &Rope,
    origins: &SelectionSet,
    regex: &Regex,
    operation: SearchPrompt,
    mut remaining: usize,
    cancelled: &impl Fn() -> bool,
) -> Result<Option<SelectionSet>, Error> {
    let mut current = origins.clone();
    let mut checkpoint = current.primary();
    let mut power = 1usize;
    let mut length = 0;
    while remaining > 0 {
        if cancelled() {
            return Ok(None);
        }
        let Some(next) = navigate(text, &current, regex, operation, 1, cancelled)? else {
            return Ok(None);
        };
        current = next;
        remaining -= 1;
        length += 1;
        if current.primary() == checkpoint {
            remaining %= length;
            length = 0;
        }
        if length == power {
            checkpoint = current.primary();
            power = power.saturating_mul(2);
            length = 0;
        }
    }
    Ok(Some(current))
}

fn navigate(
    text: &Rope,
    origins: &SelectionSet,
    regex: &Regex,
    operation: SearchPrompt,
    count: usize,
    cancelled: &impl Fn() -> bool,
) -> Result<Option<SelectionSet>, Error> {
    let primary = origins.primary();
    let backward = operation == SearchPrompt::Backward;
    if backward && count > 1 && regex.can_match_empty() {
        return navigate_steps(text, origins, regex, operation, count, cancelled);
    }
    let boundary = ByteOffset(text.char_to_byte(if backward {
        primary.start().0
    } else {
        primary.end().0
    }));
    let whole = ByteOffset(0)..ByteOffset(text.len_bytes());
    let first = if backward {
        whole.start..boundary
    } else {
        boundary..whole.end
    };
    let mut result = scan(text, regex, first, backward, count, cancelled)?;
    if cancelled() {
        return Ok(None);
    }
    if count > 1 && result.needs_steps {
        return navigate_steps(text, origins, regex, operation, count, cancelled);
    }
    if result.seen < count {
        let remaining = count - result.seen;
        let cycle = scan(text, regex, whole.clone(), backward, remaining, cancelled)?;
        if count > 1 && cycle.needs_steps {
            return navigate_steps(text, origins, regex, operation, count, cancelled);
        }
        if cancelled() || cycle.seen == 0 {
            return Ok(None);
        }
        result.selected = if cycle.seen >= remaining {
            cycle.selected
        } else {
            let remainder = (remaining - 1) % cycle.seen + 1;
            scan(text, regex, whole, backward, remainder, cancelled)?.selected
        };
    }
    if cancelled() {
        return Ok(None);
    }
    Ok(result.selected.map(|range| {
        SelectionSet::single(if primary.is_backward() {
            Selection::new(range.end(), range.start())
        } else {
            range
        })
    }))
}

pub(super) fn selection_pattern(
    text: &Rope,
    origins: &SelectionSet,
    boundaries: bool,
    cancelled: &impl Fn() -> bool,
) -> Result<String, Error> {
    let mut fragments = std::collections::BTreeSet::new();
    let mut bytes = 0;
    let word = |at: usize| {
        text.get_char(at)
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
    };
    for range in origins.ranges() {
        if cancelled() {
            return Ok(String::new());
        }
        let mut fragment = String::new();
        if boundaries
            && word(range.start().0)
            && (range.start().0 == 0 || !word(range.start().0 - 1))
        {
            fragment.push_str(r"\b");
        }
        for (i, ch) in text
            .slice(range.start().0..range.end().0)
            .chars()
            .enumerate()
        {
            if i % 4096 == 0 && cancelled() {
                return Ok(String::new());
            }
            if r"\.*+?()|[]{}^$#&-~".contains(ch) {
                fragment.push('\\');
            }
            fragment.push(ch);
            if fragment.len() + bytes > 65_536 {
                return Err(Error::SelectionLimit);
            }
        }
        if boundaries && range.end().0 > 0 && word(range.end().0 - 1) && !word(range.end().0) {
            fragment.push_str(r"\b");
        }
        if !fragments.contains(&fragment) {
            bytes += fragment.len() + 1;
        }
        if bytes > 65_536 {
            return Err(Error::SelectionLimit);
        }
        fragments.insert(fragment);
    }
    Ok(fragments.into_iter().collect::<Vec<_>>().join("|"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vex_core::regex::Options;

    #[test]
    fn counts_match_repeated_commands_with_anchors_unicode_and_multiline_patterns() {
        for source in [
            "ababa\r\naaa\r\n",
            "a e\u{301}\u{301} e\u{301}\n界ab\n",
            "ae\u{301}x a\n",
            "a\rb\u{2028}a\nb\na\n",
        ] {
            let text = Rope::from_str(source);
            for pattern in [
                "a",
                "a+",
                "a?",
                "a|",
                "ae|a|\u{301}x",
                ".",
                "^",
                "$",
                "^|$",
                r"\b",
                r"\p{M}",
                "(?s)a.*a",
                "(?s:.)",
            ] {
                let regex = Regex::new(
                    pattern,
                    Options {
                        multi_line: true,
                        crlf: true,
                        ..Options::default()
                    },
                )
                .unwrap();
                for operation in [SearchPrompt::Forward, SearchPrompt::Backward] {
                    for position in 0..=text.len_chars() {
                        if !grapheme::is_boundary(&text, CharOffset(position)).unwrap() {
                            continue;
                        }
                        let origin = SelectionSet::single(
                            vex_core::motion::block(&text, CharOffset(position)).unwrap(),
                        );
                        for extend in [false, true] {
                            let mut repeated = origin.clone();
                            for count in 1..12 {
                                let next =
                                    apply(&text, &repeated, &regex, operation, 1, extend, &|| {
                                        false
                                    })
                                    .unwrap();
                                let counted = apply(
                                    &text,
                                    &origin,
                                    &regex,
                                    operation,
                                    count,
                                    extend,
                                    &|| false,
                                )
                                .unwrap();
                                assert_eq!(
                                    counted, next,
                                    "{source:?} {pattern:?} {operation:?} {position} {count} {extend}"
                                );
                                let Some(next) = next else {
                                    break;
                                };
                                repeated = next;
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn excessive_selection_results_fail_without_partial_changes() {
        let text = Rope::from_str(&"a ".repeat(MAX_SELECTIONS + 1));
        let origins =
            SelectionSet::single(Selection::new(CharOffset(0), CharOffset(text.len_chars())));
        let regex = Regex::new("a", Options::default()).unwrap();
        assert_eq!(
            apply(
                &text,
                &origins,
                &regex,
                SearchPrompt::Select,
                1,
                false,
                &|| false
            ),
            Err(Error::SelectionLimit)
        );
        assert_eq!(
            selection_pattern(&text, &origins, false, &|| false),
            Err(Error::SelectionLimit)
        );
    }

    #[test]
    fn counted_searches_apply_intermediate_merges_and_keep_primary_direction() {
        let text = Rope::from_str("abc abc abc abc");
        let regex = Regex::new("abc", Options::default()).unwrap();
        for origins in [
            vec![
                Selection::new(CharOffset(0), CharOffset(1)),
                Selection::new(CharOffset(2), CharOffset(8)),
            ],
            vec![
                Selection::new(CharOffset(8), CharOffset(0)),
                Selection::new(CharOffset(10), CharOffset(11)),
            ],
        ] {
            let origin = SelectionSet::new(origins, 0).unwrap();
            for extend in [true, false] {
                for operation in [SearchPrompt::Forward, SearchPrompt::Backward] {
                    let mut repeated = origin.clone();
                    for count in 1..30 {
                        repeated = apply(&text, &repeated, &regex, operation, 1, extend, &|| false)
                            .unwrap()
                            .unwrap();
                        let counted =
                            apply(&text, &origin, &regex, operation, count, extend, &|| false)
                                .unwrap()
                                .unwrap();
                        assert_eq!(counted, repeated, "{operation:?} {extend} {count}");
                    }
                }
            }
        }
    }

    #[test]
    fn reverse_counts_exceeding_the_tail_window_and_wrap_counts_stay_bounded() {
        let text = Rope::from_str(&"cat\n".repeat(4000));
        let regex = Regex::new("cat", Options::default()).unwrap();
        let origin = SelectionSet::single(Selection::cursor(CharOffset(text.len_chars())));
        for count in [1, 513, 700, 3999, 4000, 4001, usize::MAX] {
            let found = apply(
                &text,
                &origin,
                &regex,
                SearchPrompt::Backward,
                count,
                false,
                &|| false,
            )
            .unwrap()
            .unwrap();
            let start = (3999 - (count - 1) % 4000) * 4;
            assert_eq!(
                found.primary(),
                Selection::new(CharOffset(start), CharOffset(start + 3))
            );
        }
    }

    #[test]
    fn reverse_matches_inside_graphemes_can_expose_shorter_earlier_alternatives() {
        let text = Rope::from_str("ae\u{301}x");
        let regex = Regex::new("ae|a|\u{301}x", Options::default()).unwrap();
        let origin = SelectionSet::single(Selection::cursor(CharOffset(4)));
        for (count, expected) in [(1, (1, 4)), (2, (0, 1)), (3, (1, 4)), (usize::MAX, (1, 4))] {
            let found = apply(
                &text,
                &origin,
                &regex,
                SearchPrompt::Backward,
                count,
                false,
                &|| false,
            )
            .unwrap()
            .unwrap();
            assert_eq!(
                found.primary(),
                Selection::new(CharOffset(expected.0), CharOffset(expected.1))
            );
        }
        let regex = Regex::new("a|", Options::default()).unwrap();
        let text = Rope::from_str("ab");
        let origin = SelectionSet::single(Selection::cursor(CharOffset(0)));
        let found = apply(
            &text,
            &origin,
            &regex,
            SearchPrompt::Forward,
            2,
            false,
            &|| false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            found.primary(),
            Selection::new(CharOffset(1), CharOffset(2))
        );
    }
}
