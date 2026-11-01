//! Screen storage for terminal state.
//!
//! [`Grid`] stores explicit-width display units in rows of compact [`Cell`] values.
//! Single scalars are inline; multi-scalar text is owned by its row. Mutations
//! preserve paired wide cells. Callers provide display widths and opaque style
//! keys; terminal interpretation owns their meaning.
//!
//! Resizing preserves the upper-left intersection. A wide unit cut by the new
//! right edge is cleared. Scrolling an exclusive row range recycles row buffers
//! and clears exposed rows. Rows outside the range remain unchanged. Fallible
//! operations reserve storage before changing visible contents.
//! In-row insertion and deletion shift cells and their owned text without
//! allocating. Wide units split by an edit boundary are cleared completely.
//!
//! [`MAX_CELLS`] bounds the logical cell count. Cell buffers and the row queue
//! retain capacity after shrinking. [`MAX_CELL_TEXT_BYTES`] bounds the UTF-8
//! payload of each display unit. Protocol interpretation consumes [`lune_vt::Event`].
//!
//! [`Screen`] combines storage with a cell-addressed [`Cursor`], delayed wrapping,
//! a vertical scrolling region, carriage return, line feed, reverse index, and
//! backspace. ASCII runs are written in batches;
//! other display units use caller-supplied widths. Writes and resizing preserve
//! grid contents, cursor state, and the scrolling region on error.
//! [`EraseMode`] selects cursor-inclusive line or display erasure. Erasure
//! preserves cursor coordinates and region boundaries, cancels pending wrapping,
//! and fills complete affected display units with styled spaces without allocating.
//! Cursor-relative cell insertion, deletion, and counted erasure operate in
//! physical columns. They clamp counts at the right edge, preserve coordinates
//! and scrolling boundaries, and cancel pending wrapping for positive counts.

mod cell;
mod error;
mod grid;
mod row;
mod screen;

#[cfg(test)]
mod tests;

pub use cell::{Cell, CellText, CellWidth, MAX_CELL_TEXT_BYTES, StyleId};
pub use error::StorageError;
pub use grid::{Grid, MAX_CELLS, MAX_DIMENSION};
pub use row::Row;
pub use screen::{Cursor, EraseMode, Screen};
