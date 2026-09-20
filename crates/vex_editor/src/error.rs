use std::fmt;

use crate::Mode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Core(vex_core::Error),
    UnknownCommand(String),
    WrongMode { expected: Mode, actual: Mode },
    MissingText,
    MissingCharacter,
    EmptyBinding,
    ConflictingBinding,
    ReservedBinding,
    CountOverflow,
    RepeatChanged,
    RepeatCompletion,
    NoSearch,
    NoSearchPreview,
    SearchActive,
    SearchChanged,
    NoMatch,
    InvalidRegex(String),
    SelectionLimit,
    InvalidCompletion,
    EmptyYankRegister,
    SurroundNotFound,
    SurroundOverlap,
    SurroundAmbiguous,
    NoSurroundReplacement,
    SurroundChanged,
}

impl From<vex_core::Error> for Error {
    fn from(error: vex_core::Error) -> Self {
        Self::Core(error)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Core(error) => error.fmt(f),
            Self::UnknownCommand(name) => write!(f, "unknown command: {name}"),
            Self::WrongMode { expected, actual } => write!(
                f,
                "command requires {expected:?} mode, current mode is {actual:?}"
            ),
            Self::MissingText => write!(f, "command requires text in its command context"),
            Self::MissingCharacter => {
                write!(f, "command requires a character in its command context")
            }
            Self::EmptyYankRegister => write!(f, "nothing yanked; use y, d, or c first"),
            Self::SurroundNotFound => write!(f, "surround pair not found around every cursor"),
            Self::SurroundOverlap => {
                write!(f, "multiple cursors target the same surround delimiter")
            }
            Self::SurroundAmbiguous => write!(f, "cursor is on an ambiguous surround delimiter"),
            Self::NoSurroundReplacement => write!(f, "no surround replacement is awaiting input"),
            Self::SurroundChanged => {
                write!(f, "document or view changed during surround replacement")
            }
            Self::EmptyBinding => write!(f, "a keybinding cannot be empty"),
            Self::ConflictingBinding => write!(
                f,
                "a keybinding cannot also be the prefix of another binding"
            ),
            Self::ReservedBinding => {
                write!(f, "Escape and leading repeat-count digits are reserved")
            }
            Self::CountOverflow => write!(f, "repeat count is too large"),
            Self::RepeatChanged => write!(f, "document or view changed during insert replay"),
            Self::RepeatCompletion => write!(f, "recorded completion does not fit at every cursor"),
            Self::NoSearch => write!(f, "no accepted search; use / or ? first"),
            Self::NoSearchPreview => write!(f, "no active search preview"),
            Self::SearchActive => write!(f, "accept or cancel the active search first"),
            Self::SearchChanged => write!(
                f,
                "document or mode changed during search; search cancelled"
            ),
            Self::InvalidRegex(error) => write!(f, "invalid regex: {error}"),
            Self::SelectionLimit => write!(
                f,
                "regex operation exceeds 100,000 selections or 64 KiB query"
            ),
            Self::NoMatch => write!(f, "no matches"),
            Self::InvalidCompletion => write!(
                f,
                "completion requires one insertion caret inside the replacement range"
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Core(error) => Some(error),
            _ => None,
        }
    }
}
