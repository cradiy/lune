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
//! Grid resizing preserves the upper-left intersection. A wide unit cut by the new
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
//! and reuses row buffers. Primary full-screen upward scrolling can allocate
//! replacement buffers while its bounded history is filling.
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
//! [`Screen::revise_unit`] replaces an existing leading unit at a retained physical
//! position and returns its new position. It places the cursor after the revised
//! footprint. Same-width updates preserve soft wraps; width changes adjust the
//! footprint and move the suffix by only the width difference under insertion mode.
//! Growth at the right edge relocates the complete unit through normal wrapping and
//! region scrolling. With wrapping disabled, an expansion that cannot fit fails.
//! Every failure preserves both buffers and logical screen state. Consumers retain
//! the returned position while extending a grapheme and end that association when
//! protocol controls change its placement or interpretation.
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
//! retain connections only between surviving adjacent rows. Grid width changes clear
//! all joins; height changes retain surviving adjacent joins. Reset clears them.
//! Metadata updates do not allocate and fallible operations preserve it on error.
//!
//! [`Grid::text`] validates a half-open range of [`CellPosition`] boundaries and
//! returns a borrowed [`TextRange`] view. Formatting streams complete display units,
//! preserves selected spaces, joins soft wraps, skips their edge gaps, and inserts
//! newlines between hard rows. Reading uses fixed auxiliary space; the output sink
//! owns its allocation and failure behavior.
//! [`Screen::view`] borrows chronological primary history and visible rows, or
//! just visible alternate rows. [`ScreenView::text`] uses the same range rules,
//! validating endpoint columns against their own rows' captured widths. View
//! indices start at the oldest retained row and are fixed by the source borrow.
//! [`ScreenView::history_rows`] translates a physical cursor row into view space.
//! Construction, row lookup, and validation take constant time. Formatting visits
//! only selected rows, including soft-wrap connections across history and screen.
//! [`ScreenView::logical_lines`] groups retained rows connected by automatic
//! wrapping. [`ScreenView::logical_line`] finds the group containing a physical
//! view row. [`LogicalLine`] borrows its row range, rows, and text. History eviction
//! may leave the oldest group as a fragment. Double-ended traversal takes linear
//! time in physical rows and constant auxiliary space without allocating.
//! [`LogicalLine::reflow`] lays out retained units at a new column width through
//! borrowed [`ReflowRow`] values. Each row exposes source coordinates, occupied
//! columns, a soft-wrap boundary, and streamed text. Layout retains all spaces
//! and cell metadata while skipping source wrap gaps. A one-column target rejects
//! double-width units before traversal. Planning uses constant auxiliary space
//! without allocating; consumers own destination storage and padding.
//! [`ReflowRow::materialize`] builds an independently owned [`Row`] at the target
//! width. It copies complete cells and owned text, preserves erased state and
//! style keys, replaces source row markers, and fills unused columns with styled
//! erased spaces. Allocation failures release partial storage and preserve the
//! source. Retained rows remain valid after the source screen is released.
//! [`LogicalLine::map_position`] maps chronological source coordinates into
//! logical-line-relative output rows, with [`ReflowAffinity`] choosing either
//! side of shared boundaries. Source wrap gaps join the following content and
//! wide-unit column offsets are preserved. [`LogicalLine::map_cursor`] translates
//! delayed wrapping through a boundary after the source right edge.
//! [`LogicalLine::anchor`] validates a borrowed [`ReflowAnchor`] once for repeated
//! mapping at different widths. [`ReflowRow::map_anchor`] checks its source view
//! and maps it during row traversal. Both endpoints are included so consumers
//! can select their boundary affinity.
//! [`Screen::reflow`] prepares replacement primary history, live rows, alternate
//! storage and updated tab stops, then commits them together. Its latest primary window
//! contains the mapped live cursor, or the primary save slot while alternate
//! storage is active. Rows below that window are cropped. History retains an
//! adjacent bounded suffix; an oversized row clears the preceding suffix.
//! Current and saved cursors and the primary viewport are mapped or clamped.
//! Commit selects the full-screen scrolling region and preserves modes and
//! appearance. Preparation reserves cell headers and sparse text slots; commit
//! moves existing text allocations into retained rows. History accounts for
//! their allocated capacities. Allocation failure preserves the complete source
//! state and its allocations.
//! Three borrowed layout checkpoints bound preparation's prefix replay by the
//! history row limit and target height without allocating checkpoint storage.
//! [`Screen::viewport`] borrows a screen-height [`Viewport`] using window-local
//! row coordinates. Its offset is bounded by retained primary history. Zero
//! follows output; scrolling upward anchors the top retained row as captures
//! arrive. Eviction clamps an expired anchor to the oldest retained row. Skipped
//! oversized rows preserve the position. Viewport movement preserves buffers,
//! cursor, and modes without allocating. Alternate buffers follow live output
//! and preserve the primary position. History clearing and reset resume live
//! following; resizing preserves the primary top row and changes window height.
//! Successful captures update the position after storage preparation. Failed
//! writes and counted scrolls preserve it. [`Viewport::cursor`] maps the physical
//! cursor into the window when its row is included, with appearance kept separate.
//!
//! [`Processor`] executes byte streams through the parser and existing screen
//! operations. Text layout is supplied by an atomic [`TextSink`]. [`ProcessError`]
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
//! [`UnicodeWriter`] implements the sink with incremental extended-grapheme
//! composition and a fixed [`WidthPolicy`]. Protocol boundaries end a cluster;
//! input chunk boundaries retain it. Updates revise an existing unit through
//! [`Screen::revise_unit`], including late emoji width changes. Widths are clamped
//! to one or two columns and reduced to fit a narrow screen or a nonwrapping edge.
//! Orphan zero-width clusters retain their text in one column. Decoded C1 controls
//! establish a boundary and are ignored. Failed writes preserve text state and
//! screen contents for retry; the text limit remains [`MAX_CELL_TEXT_BYTES`].
//! Pass a mutable sink to advance, finish, and reset. External changes to live text
//! contents or placement require [`TextSink::boundary`] before subsequent input.
//! Viewport navigation preserves text placement and composition context.
//!
//! [`Screen::history`] borrows bounded chronological physical rows. Configure
//! [`HistoryLimits`] through [`Screen::set_history_limits`]; capture starts disabled.
//! Full-screen primary LF, IND/NEL, automatic wrapping, and explicit upward
//! scrolling transfer outgoing rows into history. Oldest rows are evicted first.
//! Retained rows preserve captured widths, Unicode text, styles, and soft wraps.
//! Batch writes reserve replacements before mutation; failures preserve history,
//! both buffers, and cursor state. Metadata and bounded transaction buffers are
//! separate from the retained row byte budget. Screen edits and width changes
//! maintain history-to-screen adjacency. Reset clears rows and preserves limits.
//! Style collection includes all retained history cells automatically.
//!
//! [`Style`] represents unresolved colors and character rendition independently
//! of [`StyleId`] storage. [`Style::apply_sgr`] interprets a parameter list
//! atomically with fixed auxiliary space. [`Color`] preserves default and indexed
//! colors for later theme/palette resolution. Screen callers can own style storage.
//! [`Processor`] owns its [`StyleTable`] and supplies its current key to the text
//! callback and screen operations. [`Processor::collect_styles`] traces the paired
//! screen and current/saved rendition, together with caller-supplied snapshot roots.
//! [`ProcessFailure`] distinguishes text, screen storage, rendition, and reply
//! failures. Failed completed controls remain buffered for retry before further input.
//! [`StyleTable`] interns values behind bounded, table-local IDs. Explicit
//! collection traces caller-owned roots, preserves retained IDs, and invalidates
//! reclaimed IDs through slot generations. Default rendition permanently uses
//! key zero; exhausted generations retire their slots. Lookup, repeated interning
//! hits, and collection make no allocation requests.

mod cell;
mod charsets;
mod error;
mod grid;
mod history;
mod input;
mod lines;
mod processor;
mod reflow;
mod reply;
mod row;
mod screen;
mod style;
mod styles;
mod tabs;
mod text;
mod viewport;
mod writer;

#[cfg(test)]
mod tests;

pub use cell::{Cell, CellText, CellWidth, MAX_CELL_TEXT_BYTES, StyleId};
pub use error::StorageError;
pub use grid::{Grid, MAX_CELLS, MAX_DIMENSION};
pub use history::{HistoryLimits, MAX_HISTORY_BYTES, MAX_HISTORY_ROWS, RowHistory};
pub use input::{InputModes, Key};
pub use lines::{LogicalLine, LogicalLines};
pub use lune_text::WidthPolicy;
pub use processor::{ProcessError, ProcessFailure, Processor};
pub use reflow::{ReflowAffinity, ReflowAnchor, ReflowRow, ReflowRows};
pub use row::Row;
pub use screen::{Cursor, CursorAppearance, CursorShape, EraseMode, Screen, ScreenBuffer};
pub use style::{Color, Style, Underline};
pub use styles::{MAX_STYLES, StyleError, StyleTable};
pub use text::{CellPosition, ScreenView, TextRange};
pub use viewport::Viewport;
pub use writer::{TextSink, UnicodeWriter};
