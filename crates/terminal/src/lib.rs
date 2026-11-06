//! Terminal screen storage and incremental protocol execution.
//!
//! [`Grid`] stores explicit-width display units in rows of compact [`Cell`] values.
//! Single scalars are inline; multi-scalar text is owned by its row. Mutations
//! preserve paired wide cells. Callers provide display widths and opaque style
//! keys; terminal interpretation owns their meaning.
//! [`Cell::is_erased`] distinguishes initialized/erased positions from printed
//! spaces. Erasure, exposed rows/columns, and repaired wide fragments establish
//! erased single-width spaces. Cell edits retain the state of moved units;
//! nonempty text appends establish text. Extraction emits spaces for both states.
//!
//! Resizing preserves the upper-left intersection. A wide unit cut by the new
//! right edge is cleared. Scrolling an exclusive row range recycles row buffers
//! and clears exposed rows. Cells outside the range remain unchanged. Fallible
//! operations reserve storage before changing visible contents.
//! In-row insertion and deletion shift cells and their owned text without
//! allocating. Wide units split by an edit boundary are cleared completely.
//!
//! [`MAX_CELLS`] bounds the logical cell count per grid. Cell buffers and the row queue
//! retain capacity after shrinking. [`MAX_CELL_TEXT_BYTES`] bounds the UTF-8
//! payload of each display unit. Protocol interpretation consumes [`lune_vt::Event`].
//!
//! [`Screen`] combines storage with a cell-addressed [`Cursor`], delayed wrapping,
//! a vertical scrolling region, carriage return, line feed, reverse index, and
//! backspace. ASCII runs are written in batches;
//! other display units use caller-supplied widths. Writes and resizing preserve
//! grid contents, cursor state, and the scrolling region on error.
//! Each screen allocates primary and alternate grids, each bounded by [`MAX_CELLS`].
//! [`Screen::select_buffer`] swaps storage and the buffer's cursor save slot while
//! preserving live cursor placement, region, tabs, modes, and appearance.
//! [`Screen::buffer`] reads either grid. Resize reserves both grids before committing;
//! failure preserves both logical states. Reset selects the primary and clears both.
//! [`Processor`] executes private modes 47, 1047, 1048, and 1049. Current rendition
//! and character sets remain live across switches; each buffer saves them separately.
//! Style collection traces both grids and save slots. Switching does not allocate.
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
//! [`Screen::save_cursor`] replaces a fixed save slot with the position, origin
//! and automatic-wrap modes, and pending wrapping. Saved rows follow the selected addressing origin.
//! [`Screen::restore_cursor`] applies that state to the current region and size,
//! clamping coordinates. Pending wrapping survives only at an unchanged valid
//! position on the current right edge. The slot initially holds the initial
//! cursor state and persists across resizing and repeated restores. Both
//! operations preserve contents, scrolling boundaries, and tabs without allocating.
//! [`Screen::reset`] fills the current dimensions with styled spaces and restores
//! the initial cursor, save slot, full-screen region, default tabs, and modes.
//! Row buffers, the row queue, text indices, and the tab bitmap retain capacity;
//! owned text is released. Repeated resets do not allocate.
//! Insertion mode starts disabled. Enabling it shifts the row suffix right by
//! each written unit's width, discards overflow, and repairs split wide units.
//! ASCII batches shift once per row segment after validating the full input.
//! Scalar insertion reserves owned text and index capacity before changing cells.
//! Mode changes preserve pending wrapping; resizing and cursor restores preserve
//! insertion mode. Resetting disables it.
//! [`CursorAppearance`] records visibility, blinking, and [`CursorShape`] separately
//! from placement. Presentation changes preserve contents and pending wrapping;
//! resizing and cursor restores preserve appearance. Reset restores a visible
//! blinking block. [`Processor`] executes DEC modes 12/25 and DECSCUSR values 0–6.
//! Consumers own cursor timing and painting.
//!
//! [`Row::wrap_end`] records an actual automatic-wrap connection to the following
//! row and its exclusive column boundary. A double-width unit that wraps early
//! excludes its one-column gap. Pending wrapping alone leaves the row unjoined.
//! Writes and erasure reaching the join boundary, including wide-unit repairs,
//! cancel that row's outgoing connection. Earlier cell edits and width-preserving
//! text appends retain it; appending into an excluded gap cancels it. Cell shifts
//! cancel the edited row's outgoing connection. Clearing complete rows also
//! disconnects the preceding row. Explicit line feed cancels the source join;
//! other cursor movement preserves stored joins. Scrolling and whole-row edits
//! retain connections only between surviving adjacent rows. Width changes clear
//! all joins; height changes retain surviving adjacent joins. Reset clears them.
//! Metadata updates do not allocate and fallible operations preserve it on error.
//!
//! [`Grid::text`] validates a half-open range of [`CellPosition`] boundaries and
//! returns a borrowed [`GridText`] view. Formatting streams complete display units,
//! preserves selected spaces, joins soft wraps, skips their edge gaps, and inserts
//! newlines between hard rows. Reading uses fixed auxiliary space; the output sink
//! owns its allocation and failure behavior.
//!
//! [`Processor`] executes byte streams through the parser and existing screen
//! operations. Text layout is supplied by an atomic callback. [`ProcessError`]
//! reports the consumed input prefix; failed decoded text is retained in fixed
//! storage for retry before further input. ASCII runs and complete UTF-8 scalars
//! are dispatched directly in the parser's ground state. Lexical errors recover
//! through the parser and are counted, while unsupported ESC/CSI forms are counted
//! and ignored. Finishing flushes incomplete lexical input exactly once.
//! DSR status (`CSI 5 n`) and cursor-position (`CSI 6 n`) queries publish replies
//! through [`Processor::reply_bytes`]. Coordinates are one-based and relative to
//! the scrolling region under origin mode; pending wrapping reports the last
//! physical column. One fixed slot holds at most 14 output bytes. Another complete
//! reply can wait in pending-event storage. [`ProcessFailure::ReplyPending`]
//! pauses execution until output is acknowledged with [`Processor::consume_reply`].
//! Partial acknowledgement preserves the unsent suffix, and retry uses the same
//! consumed-prefix rules as text failures. Replies retain query-time coordinates.
//! Finishing and RIS preserve issued replies; explicit reset discards all output.
//! G0/G1 ASCII and DEC Special Graphics designation, SI/SO selection, and cursor
//! save/restore determine how printable ASCII maps to Unicode text callbacks.
//! Mapped scalar failures retain the mapped value and own its source byte.
//! ANSI new-line mode selects vertical-only or column-zero movement for LF and
//! its VT/FF aliases. Explicit IND/NEL retain their own movement semantics.
//! [`Processor::input_modes`] captures application cursor/keypad, new-line,
//! and backarrow modes. [`InputModes::encode_key`] encodes unmodified control,
//! cursor, editing, F1–F12, and numeric-keypad keys into static byte slices without allocating.
//! Backspace sends DEL by default and BS under DECBKM; Tab sends HT and Escape sends ESC.
//! Snapshots and encoded bytes remain valid across later mode changes and partial writes.
//!
//! [`Style`] represents unresolved colors and character rendition independently
//! of [`StyleId`] storage. [`Style::apply_sgr`] interprets a parameter list
//! atomically with fixed auxiliary space. [`Color`] preserves default and indexed
//! colors for later theme/palette resolution. Screen callers can own style storage.
//! [`Processor`] owns its [`StyleTable`] and supplies its current key to the text
//! callback and screen operations. [`Processor::collect_styles`] traces the paired
//! screen and current/saved rendition, together with caller-supplied snapshot roots.
//! [`ProcessFailure`] distinguishes text errors from rendition storage errors;
//! failed style changes remain buffered for retry before further input.
//! [`StyleTable`] interns values behind bounded, table-local IDs. Explicit
//! collection traces caller-owned roots, preserves retained IDs, and invalidates
//! reclaimed IDs through slot generations. Default rendition permanently uses
//! key zero; exhausted generations retire their slots. Lookup, repeated interning
//! hits, and collection make no allocation requests.

mod cell;
mod charsets;
mod error;
mod grid;
mod input;
mod processor;
mod reply;
mod row;
mod screen;
mod style;
mod styles;
mod tabs;
mod text;

#[cfg(test)]
mod tests;

pub use cell::{Cell, CellText, CellWidth, MAX_CELL_TEXT_BYTES, StyleId};
pub use error::StorageError;
pub use grid::{Grid, MAX_CELLS, MAX_DIMENSION};
pub use input::{InputModes, Key};
pub use processor::{ProcessError, ProcessFailure, Processor};
pub use row::Row;
pub use screen::{Cursor, CursorAppearance, CursorShape, EraseMode, Screen, ScreenBuffer};
pub use style::{Color, Style, Underline};
pub use styles::{MAX_STYLES, StyleError, StyleTable};
pub use text::{CellPosition, GridText};
