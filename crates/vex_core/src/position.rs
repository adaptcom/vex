/// A zero-based boundary between Unicode scalar values, including EOF.
/// This is not a byte offset, grapheme index, or display column.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CharOffset(pub usize);

/// A zero-based UTF-8 byte offset. Not every byte offset is a scalar boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteOffset(pub usize);
