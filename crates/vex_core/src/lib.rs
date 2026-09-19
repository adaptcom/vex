//! The terminal-independent editing model for Vex.
//!
//! Edits use half-open ranges of Unicode scalar offsets. User-facing motions
//! must additionally respect grapheme boundaries; scalar offsets are neither
//! UTF-8 byte offsets nor terminal columns.
//!
//! ```
//! use vex_core::{CharOffset, Document, Selection, SelectionSet};
//!
//! let mut document = Document::from("hello world");
//! let mut selections = SelectionSet::single(Selection::new(
//!     CharOffset(6), CharOffset(11),
//! ));
//! let change = document.replace_selections(&selections, "Vex")?;
//! document.apply(change, &mut selections)?;
//! assert_eq!(document.text(), "hello Vex");
//! assert_eq!(selections.primary(), Selection::cursor(CharOffset(9)));
//! document.undo(&mut selections)?;
//! assert_eq!(document.text(), "hello world");
//! # Ok::<(), vex_core::Error>(())
//! ```

pub mod display;
mod document;
mod error;
pub mod grapheme;
mod history;
pub mod layout;
pub mod motion;
mod position;
mod selection;
mod transaction;

pub use document::{ChangeExtent, Document, DocumentId, Revision, Snapshot};
pub use error::Error;
pub use position::{ByteOffset, CharOffset};
pub use ropey::{Rope, RopeSlice};
pub use selection::{Selection, SelectionSet};
pub use transaction::{Affinity, Edit, Transaction};
