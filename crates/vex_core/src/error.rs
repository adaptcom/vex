use std::fmt;

use crate::{ByteOffset, CharOffset, Revision};

/// Invalid input is rejected before changing document, selections, or history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    EmptySelectionSet,
    InvalidPrimary {
        index: usize,
        count: usize,
    },
    PositionOutOfBounds {
        position: CharOffset,
        len: usize,
    },
    InvalidByteOffset {
        position: ByteOffset,
        len: usize,
    },
    ReversedRange {
        start: CharOffset,
        end: CharOffset,
    },
    OverlappingEdits,
    WrongDocument,
    StaleRevision {
        expected: Revision,
        actual: Revision,
    },
    LengthOverflow,
    RevisionExhausted,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySelectionSet => write!(f, "a selection set cannot be empty"),
            Self::InvalidPrimary { index, count } => {
                write!(f, "primary selection {index} is outside a set of {count}")
            }
            Self::PositionOutOfBounds { position, len } => {
                write!(
                    f,
                    "character offset {} exceeds document length {len}",
                    position.0
                )
            }
            Self::InvalidByteOffset { position, len } => {
                write!(
                    f,
                    "byte offset {} is not a UTF-8 boundary within {len} bytes",
                    position.0
                )
            }
            Self::ReversedRange { start, end } => {
                write!(
                    f,
                    "edit range starts at {} after its end {}",
                    start.0, end.0
                )
            }
            Self::OverlappingEdits => write!(f, "edits overlap or share a starting position"),
            Self::WrongDocument => write!(f, "transaction belongs to another document"),
            Self::StaleRevision { expected, actual } => {
                write!(
                    f,
                    "transaction expects revision {}, current revision is {}",
                    expected.get(),
                    actual.get()
                )
            }
            Self::LengthOverflow => write!(f, "edited document length exceeds addressable memory"),
            Self::RevisionExhausted => write!(f, "document revision counter is exhausted"),
        }
    }
}

impl std::error::Error for Error {}
