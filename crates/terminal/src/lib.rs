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
//! Whole-row insertion and deletion affect the cursor row through the scrolling
//! region's bottom. They reuse row buffers and preserve retained text ownership;
//! zero counts and cursors outside the region preserve all state.
//! Relative cursor movement addresses physical cells and stops at directional
//! scrolling boundaries or screen edges. It preserves contents and the region;
//! zero counts preserve all state and positive counts cancel pending wrapping.
//! Explicit upward and downward scrolling affects the entire active region,
//! preserves cursor coordinates and pending wrapping even outside the region,
//! and recycles row buffers without allocating.
//! Horizontal tab stops use one bit per column and default to columns 0, 8, 16,
//! and so on. Stops can be set or cleared at the cursor, or cleared together.
//! Forward and backward tabulation preserve contents and clamp at screen edges;
//! positive counts cancel pending wrapping and zero preserves all state.
//! Width changes retain overlapping stops, discard cropped stops, and initialize
//! added columns with defaults. Resizing preserves stops on error.
//! Automatic wrapping starts enabled. Disabling it clears pending wrapping;
//! output then stays on its physical row and repeated writes replace the last
//! column. Valid display units that cannot fit the remaining columns are ignored,
//! while units wider than the entire grid fail. Resizing preserves this mode.
//! Origin mode starts disabled. [`Screen::set_cursor`] takes zero-based rows
//! relative to the scrolling region when enabled and the full screen otherwise;
//! coordinates clamp at the applicable edges. Cursor snapshots stay physical.
//! Setting origin mode or a valid scrolling region homes the cursor and clears
//! pending wrapping. Resizing preserves origin mode and keeps the cursor in
//! bounds; a height change resets the scrolling region to the full screen.

mod cell;
mod error;
mod grid;
mod row;
mod screen;
mod tabs;

#[cfg(test)]
mod tests;

pub use cell::{Cell, CellText, CellWidth, MAX_CELL_TEXT_BYTES, StyleId};
pub use error::StorageError;
pub use grid::{Grid, MAX_CELLS, MAX_DIMENSION};
pub use row::Row;
pub use screen::{Cursor, EraseMode, Screen};
