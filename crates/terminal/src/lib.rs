//! Screen storage for terminal state.
//!
//! [`Grid`] stores explicit-width display units in rows of compact [`Cell`] values.
//! Single scalars are inline; multi-scalar text is owned by its row. Mutations
//! preserve paired wide cells. Callers provide display widths and opaque style
//! keys; terminal interpretation owns their meaning.
//!
//! Resizing preserves the upper-left intersection. A wide unit cut by the new
//! right edge is cleared. Full-screen scrolling recycles row buffers. Fallible
//! operations reserve storage before changing visible contents.
//!
//! [`MAX_CELLS`] bounds the logical cell count. Cell buffers and the row queue
//! retain capacity after shrinking. [`MAX_CELL_TEXT_BYTES`] bounds the UTF-8
//! payload of each display unit. Protocol interpretation consumes [`lune_vt::Event`].

mod cell;
mod error;
mod grid;
mod row;

#[cfg(test)]
mod tests;

pub use cell::{Cell, CellText, CellWidth, MAX_CELL_TEXT_BYTES, StyleId};
pub use error::StorageError;
pub use grid::{Grid, MAX_CELLS, MAX_DIMENSION};
pub use row::Row;
