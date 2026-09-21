//! Terminal palette and style attributes. All colors use the terminal defaults
//! or its base 16 ANSI slots. Documents and previews share these styles.

use crossterm::style::Color;
use vex_syntax::{Highlight, markup::Attributes};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Style {
    #[default]
    Text,
    Syntax(Highlight),
    Markup(Attributes),
    /// Empty-line markers and other secondary gutter text.
    Gutter,
    LineNumber,
    ActiveLineNumber,
    GitAdded,
    GitModified,
    GitDeleted,
    Status,
    StatusLine,
    StatusBorder,
    InactiveStatus,
    Message,
    PopupTitle,
    Error,
    Selection,
    PrimaryCursor(Option<Highlight>),
    /// Keep the glyph's colors; the terminal draws the primary insert caret.
    InsertCursor(Option<Highlight>),
    /// Retain syntax so unfocused panes can remove the matching decoration.
    MatchingBracket(Option<Highlight>),
    /// Matching partner within selected text; retain the selection background.
    SelectedMatchingBracket,
    /// Matching partner under a secondary cursor; retain that cursor's background.
    MatchingBracketCursor,
    SecondaryCursor,
    InactiveCursor,
    PickerMatch,
    /// Prompt, selected-result, and preview markers in pickers and completion menus.
    PickerMarker,
}

impl Style {
    fn syntax(self) -> Option<Highlight> {
        match self {
            Self::Syntax(highlight) => Some(highlight),
            Self::PrimaryCursor(highlight) | Self::InsertCursor(highlight) => highlight,
            _ => None,
        }
    }

    fn attributes(self) -> Attributes {
        match self {
            Self::Markup(attributes) => attributes,
            Self::StatusLine
            | Self::InactiveStatus
            | Self::PopupTitle
            | Self::PickerMatch
            | Self::PickerMarker => Attributes::STRONG,
            Self::MatchingBracket(_)
            | Self::SelectedMatchingBracket
            | Self::MatchingBracketCursor => Attributes::STRONG | Attributes::LINK,
            _ => self
                .syntax()
                .map_or(Attributes::default(), syntax_attributes),
        }
    }

    pub(crate) fn bold(self) -> bool {
        self.attributes().contains(Attributes::STRONG)
    }

    pub(crate) fn reversed(self) -> bool {
        matches!(
            self,
            Self::PrimaryCursor(_) | Self::StatusLine | Self::InactiveStatus
        )
    }

    pub(crate) fn underlined(self) -> bool {
        self.attributes().contains(Attributes::LINK)
    }

    pub(crate) fn italic(self) -> bool {
        self.attributes().contains(Attributes::EMPHASIS)
    }

    pub(crate) fn crossed_out(self) -> bool {
        self.attributes().contains(Attributes::STRIKE)
    }

    pub(crate) fn colors(self) -> (Color, Color) {
        use Color::*;
        match self {
            Self::Text => (Reset, Reset),
            Self::Markup(attributes) => {
                if attributes.contains(Attributes::SELECTED) {
                    (Black, Grey)
                } else if attributes.contains(Attributes::CODE) {
                    // ANSI grey pairs depend on the terminal palette and can
                    // have almost no contrast. Code needs the same readable
                    // foreground/background as ordinary document text.
                    (Reset, Reset)
                } else if attributes.contains(Attributes::LINK) {
                    (DarkCyan, Reset)
                } else if attributes.contains(Attributes::MUTED) {
                    (DarkGrey, Reset)
                } else {
                    (Reset, Reset)
                }
            }
            Self::Syntax(highlight) => (syntax_color(highlight), Reset),
            Self::Gutter | Self::LineNumber => (Grey, Reset),
            Self::ActiveLineNumber => (DarkCyan, Reset),
            Self::GitAdded => (Green, Reset),
            Self::GitModified => (Yellow, Reset),
            Self::GitDeleted => (Red, Reset),
            Self::Status => (Black, Grey),
            Self::StatusLine => (Reset, Reset),
            Self::StatusBorder | Self::InactiveStatus => (DarkGrey, Reset),
            Self::Message | Self::PopupTitle => (DarkCyan, Reset),
            Self::Error => (Red, Reset),
            Self::Selection => (Reset, Grey),
            Self::MatchingBracket(_) => (Yellow, Reset),
            Self::SelectedMatchingBracket => (Yellow, Grey),
            Self::MatchingBracketCursor => (Yellow, DarkCyan),
            Self::PrimaryCursor(highlight) | Self::InsertCursor(highlight) => {
                highlight.map_or(Self::Text, Self::Syntax).colors()
            }
            Self::SecondaryCursor => (Black, DarkCyan),
            Self::InactiveCursor => (DarkGrey, Reset),
            Self::PickerMarker => (DarkCyan, Reset),
            Self::PickerMatch => (Blue, Reset),
        }
    }
}

/// Approximate Helix's github_light syntax with the terminal's base 16 palette.
/// Regular ANSI colors suit light backgrounds; dark yellow stands in for orange
/// and the theme's blue shades share the terminal's blue. Plain text inherits
/// the terminal foreground. No RGB or extended palette colors are imposed.
/// https://github.com/helix-editor/helix/blob/master/runtime/themes/github_light.toml
fn syntax_color(highlight: Highlight) -> Color {
    use Highlight::*;
    match highlight {
        Keyword | BuiltinVariable | Label => Color::DarkRed,
        Type | Namespace | Parameter => Color::DarkYellow,
        BuiltinType | Constant | Escape | Heading | Raw | String | Operator | Property | Link => {
            Color::DarkBlue
        }
        Function | Constructor => Color::DarkMagenta,
        Tag => Color::DarkGreen,
        Comment => Color::DarkGrey,
        Attribute => Color::Yellow,
        Punctuation | Variable | Emphasis | Strong | Strikethrough | LinkUrl | Embedded => {
            Color::Reset
        }
    }
}

/// Attributes shared by source text and its cursor cells. Keep color selection
/// separate so syntax decorations never introduce a background color.
fn syntax_attributes(highlight: Highlight) -> Attributes {
    match highlight {
        Highlight::Heading | Highlight::Strong => Attributes::STRONG,
        Highlight::Emphasis => Attributes::EMPHASIS,
        Highlight::Strikethrough => Attributes::STRIKE,
        Highlight::Link | Highlight::LinkUrl => Attributes::LINK,
        _ => Attributes::default(),
    }
}
