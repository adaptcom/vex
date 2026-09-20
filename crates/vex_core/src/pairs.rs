//! Delimiter identities shared by textobjects, surrounds, and syntax matching.

/// Asymmetric pairs accepted by match mode, in either input direction.
pub const BRACKETS: &[(char, char)] = &[
    ('(', ')'),
    ('[', ']'),
    ('{', '}'),
    ('<', '>'),
    ('‘', '’'),
    ('“', '”'),
    ('«', '»'),
    ('「', '」'),
    ('（', '）'),
];

/// Resolve either side of a bracket to its opening/closing pair. Any other
/// character surrounds symmetrically, including quotes and arbitrary Unicode.
pub fn pair(character: char) -> (char, char) {
    BRACKETS
        .iter()
        .copied()
        .find(|&(open, close)| character == open || character == close)
        .unwrap_or((character, character))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn either_side_of_a_bracket_and_literal_characters_resolve_consistently() {
        for &(open, close) in BRACKETS {
            assert_eq!(pair(open), (open, close));
            assert_eq!(pair(close), (open, close));
        }
        for ch in ['"', '\'', '`', '|', 'm', '界', ' '] {
            assert_eq!(pair(ch), (ch, ch));
        }
    }
}
