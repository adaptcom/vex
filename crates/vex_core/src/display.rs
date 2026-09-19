//! Shared terminal-cell conventions for movement and rendering.

use std::num::NonZeroUsize;
use unicode_width::UnicodeWidthStr;

/// Render control characters and standalone zero-width clusters as a visible
/// replacement cell. Callers handle tabs and line endings separately. This also
/// prevents document contents from being interpreted as terminal escape codes.
pub fn visible(grapheme: &str) -> &str {
    if grapheme.chars().any(char::is_control) || grapheme.width() == 0 {
        "�"
    } else {
        grapheme
    }
}

/// Width of a displayed grapheme, including tab stops and replacement cells.
pub fn width(grapheme: &str, column: usize, tab_width: NonZeroUsize) -> usize {
    if grapheme == "\t" {
        tab_width.get() - column % tab_width.get()
    } else if grapheme.chars().any(char::is_control) {
        1
    } else {
        grapheme.width().max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn controls_and_standalone_marks_have_a_visible_cell() {
        let tabs = NonZeroUsize::new(4).unwrap();
        for input in ["\x1b", "\0", "\u{301}", "\u{200d}", "\u{85}"] {
            assert_eq!(visible(input), "�");
            assert_eq!(width(input, 0, tabs), 1);
        }
        assert_eq!(visible("e\u{301}"), "e\u{301}");
        assert_eq!(width("界", 0, tabs), 2);
        assert_eq!(width("\t", 3, tabs), 1);
    }
}
