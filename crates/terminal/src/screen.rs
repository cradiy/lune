use std::ops::Range;

use crate::{CellWidth, Grid, MAX_CELL_TEXT_BYTES, StorageError, StyleId};
use crate::{grid::validate_dimensions, tabs::TabStops};

/// Cursor-relative extent for line or display erasure. Both directions include
/// the cursor cell and complete wide units intersecting the extent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EraseMode {
    /// Erase from the cursor through the end of the line or display.
    FromCursor,
    /// Erase from the start of the line or display through the cursor.
    ToCursor,
    /// Erase the entire line or display.
    All,
}

/// A zero-based cursor snapshot. Columns may address wide-unit continuations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub column: usize,
    /// The next display unit starts on the next line.
    pub wrap_pending: bool,
}

/// Screen storage with cursor placement and delayed automatic wrapping.
/// Callers supply complete display units, their widths, and style keys.
#[derive(Debug)]
pub struct Screen {
    grid: Grid,
    cursor: Cursor,
    saved_cursor: SavedCursor,
    scroll_region: Range<usize>,
    tabs: TabStops,
    auto_wrap: bool,
    origin_mode: bool,
}

#[derive(Clone, Copy, Debug)]
struct SavedCursor {
    row: usize,
    column: usize,
    wrap_pending: bool,
    auto_wrap: bool,
    origin_mode: bool,
}

impl Default for SavedCursor {
    fn default() -> Self {
        Self {
            row: 0,
            column: 0,
            wrap_pending: false,
            auto_wrap: true,
            origin_mode: false,
        }
    }
}

impl Screen {
    pub fn new(columns: usize, rows: usize) -> Result<Self, StorageError> {
        Ok(Self {
            grid: Grid::new(columns, rows)?,
            cursor: Cursor::default(),
            saved_cursor: SavedCursor::default(),
            scroll_region: 0..rows,
            tabs: TabStops::new(columns)?,
            auto_wrap: true,
            origin_mode: false,
        })
    }

    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    /// Replaces the saved position, origin mode, automatic-wrap mode, and pending
    /// wrapping. Rows are relative to the scrolling region when origin mode is
    /// enabled. Active state, contents, region, and tabs are preserved.
    /// The save slot initially holds the initial cursor state. This does not allocate.
    pub fn save_cursor(&mut self) {
        self.saved_cursor = SavedCursor {
            row: if self.origin_mode {
                self.cursor.row - self.scroll_region.start
            } else {
                self.cursor.row
            },
            column: self.cursor.column,
            wrap_pending: self.cursor.wrap_pending,
            auto_wrap: self.auto_wrap,
            origin_mode: self.origin_mode,
        };
    }

    /// Restores the save slot against the current dimensions and scrolling
    /// region. Coordinates clamp as in [`Self::set_cursor`]. Pending wrapping
    /// is restored only when saved coordinates fit unchanged in their addressing
    /// scope, the saved column is the current right edge, and automatic wrapping
    /// is enabled. Contents, region, tabs, and the saved slot are preserved.
    /// Repeated restores reuse the same state. This does not allocate.
    pub fn restore_cursor(&mut self) {
        let saved = self.saved_cursor;
        let height = if saved.origin_mode {
            self.scroll_region.len()
        } else {
            self.grid.rows()
        };
        self.auto_wrap = saved.auto_wrap;
        self.origin_mode = saved.origin_mode;
        self.set_cursor(saved.row, saved.column);
        self.cursor.wrap_pending = saved.wrap_pending
            && saved.auto_wrap
            && saved.row < height
            && saved.column == self.grid.columns() - 1;
    }

    /// Whether cursor addressing uses the scrolling region as its origin.
    /// Disabled initially. Cursor snapshots always use physical coordinates.
    pub fn origin_mode(&self) -> bool {
        self.origin_mode
    }

    /// Sets origin mode and homes the cursor, including repeated settings.
    /// Homing clears pending wrapping and preserves contents, tabs, automatic
    /// wrapping, and the scrolling region. This does not allocate.
    pub fn set_origin_mode(&mut self, enabled: bool) {
        self.origin_mode = enabled;
        self.set_cursor(0, 0);
    }

    /// Whether writes automatically wrap at the right edge. Enabled initially.
    pub fn auto_wrap(&self) -> bool {
        self.auto_wrap
    }

    /// Controls automatic wrapping, preserving contents, coordinates, region,
    /// and tab stops. Disabling clears pending wrapping; enabling preserves the
    /// current cursor state. This does not allocate.
    pub fn set_auto_wrap(&mut self, enabled: bool) {
        self.auto_wrap = enabled;
        self.cursor.wrap_pending &= enabled;
    }

    /// Returns whether a physical column has a tab stop, or `None` outside the
    /// screen. Defaults are zero-based columns 0, 8, 16, and so on.
    pub fn is_tab_stop(&self, column: usize) -> Option<bool> {
        (column < self.grid.columns()).then(|| self.tabs.contains(column))
    }

    /// Sets a tab stop at the cursor's physical column. Contents, cursor state,
    /// and scrolling boundaries are preserved. This does not allocate.
    pub fn set_tab_stop(&mut self) {
        self.tabs.set(self.cursor.column);
    }

    /// Clears the cursor column's tab stop, preserving all other state.
    pub fn clear_tab_stop(&mut self) {
        self.tabs.clear(self.cursor.column);
    }

    /// Clears all active tab stops, preserving all other state.
    pub fn clear_tab_stops(&mut self) {
        self.tabs.clear_all();
    }

    /// Advances across `count` tab stops strictly after the cursor, stopping at
    /// the last column when stops are exhausted. Zero preserves all state;
    /// positive counts cancel pending wrapping, including at an edge. Physical
    /// columns can address wide-unit continuations. Row, contents, stops, and
    /// region are preserved. Counts through `usize::MAX` do not allocate.
    pub fn tab_forward(&mut self, count: usize) {
        if count != 0 {
            self.cursor.column = self
                .tabs
                .forward(self.cursor.column, count, self.grid.columns());
            self.cursor.wrap_pending = false;
        }
    }

    /// Moves across `count` tab stops strictly before the cursor, stopping at
    /// column zero when stops are exhausted. Other rules follow [`Self::tab_forward`].
    pub fn tab_backward(&mut self, count: usize) {
        if count != 0 {
            self.cursor.column = self.tabs.backward(self.cursor.column, count);
            self.cursor.wrap_pending = false;
        }
    }

    /// Returns the active exclusive row range. Single-row regions are valid.
    pub fn scroll_region(&self) -> Range<usize> {
        self.scroll_region.clone()
    }

    /// Sets a nonempty exclusive row range and homes the cursor according to
    /// origin mode, clearing pending wrapping. Single-row regions are valid.
    /// Invalid ranges preserve all screen state. This does not allocate.
    pub fn set_scroll_region(&mut self, region: Range<usize>) -> Result<(), StorageError> {
        if region.start >= region.end || region.end > self.grid.rows() {
            return Err(StorageError::OutOfBounds);
        }
        self.scroll_region = region;
        self.set_cursor(0, 0);
        Ok(())
    }

    /// Addresses a zero-based position, clearing pending wrapping. With origin
    /// mode enabled, rows are relative to the scrolling region's top and clamp
    /// at its bottom. Otherwise rows address the full screen. Columns clamp at
    /// the physical right edge and may address wide-unit continuations.
    /// Coordinates through `usize::MAX` are accepted without allocating.
    pub fn set_cursor(&mut self, row: usize, column: usize) {
        let (top, height) = if self.origin_mode {
            (self.scroll_region.start, self.scroll_region.len())
        } else {
            (0, self.grid.rows())
        };
        self.cursor = Cursor {
            row: top + row.min(height - 1),
            column: column.min(self.grid.columns() - 1),
            wrap_pending: false,
        };
    }

    /// Moves upward in physical rows, stopping at the scrolling region's top.
    /// A cursor already above that boundary stops at the physical screen top.
    /// Zero counts preserve all state; positive counts cancel pending wrapping,
    /// including at a boundary. Contents, column, and region are preserved.
    /// Counts up to `usize::MAX` are accepted. This does not allocate.
    #[inline]
    pub fn move_cursor_up(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        let top = if self.cursor.row < self.scroll_region.start {
            0
        } else {
            self.scroll_region.start
        };
        self.cursor.row = self.cursor.row.saturating_sub(count).max(top);
        self.cursor.wrap_pending = false;
    }

    /// Moves downward in physical rows, stopping at the scrolling region's bottom.
    /// A cursor already below that boundary stops at the physical screen bottom.
    /// Count, wrapping, content, column, and allocation rules follow
    /// [`Self::move_cursor_up`].
    #[inline]
    pub fn move_cursor_down(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        let bottom = if self.cursor.row >= self.scroll_region.end {
            self.grid.rows() - 1
        } else {
            self.scroll_region.end - 1
        };
        self.cursor.row = self.cursor.row.saturating_add(count).min(bottom);
        self.cursor.wrap_pending = false;
    }

    /// Moves left in physical columns, stopping at column zero. Wide-unit
    /// continuations are addressable. Count, wrapping, content, and allocation
    /// rules follow [`Self::move_cursor_up`]; the row and region are preserved.
    #[inline]
    pub fn move_cursor_left(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        self.cursor.column = self.cursor.column.saturating_sub(count);
        self.cursor.wrap_pending = false;
    }

    /// Moves right in physical columns, stopping at the last column. Other
    /// movement rules follow [`Self::move_cursor_left`].
    #[inline]
    pub fn move_cursor_right(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        self.cursor.column = self
            .cursor
            .column
            .saturating_add(count)
            .min(self.grid.columns() - 1);
        self.cursor.wrap_pending = false;
    }

    /// Writes a complete display unit, then advances by its supplied width.
    /// With automatic wrapping enabled, filling the right edge leaves the cursor
    /// in the last column with wrapping pending. A double-width unit that cannot
    /// fit wraps whole; units intersecting the old right-edge gap are cleared
    /// using `style`.
    /// Wrapping at the region's bottom scrolls only that region. Outside the
    /// region, wrapping stops at the physical bottom and returns to column zero.
    /// Units wider than the grid fail. Every error preserves contents and cursor.
    /// With automatic wrapping disabled, a unit that cannot fit the remaining
    /// columns is validated and ignored. Writes ending at the right edge keep
    /// the cursor in the last column with pending wrapping clear.
    pub fn write(
        &mut self,
        text: &str,
        width: CellWidth,
        style: StyleId,
    ) -> Result<(), StorageError> {
        let columns = self.grid.columns();
        let width_columns = width.columns();
        if width_columns > columns {
            return Err(StorageError::OutOfBounds);
        }
        let gap = !self.cursor.wrap_pending && self.cursor.column + width_columns > columns;
        if gap && !self.auto_wrap {
            if text.is_empty() {
                return Err(StorageError::EmptyText);
            }
            if text.len() > MAX_CELL_TEXT_BYTES {
                return Err(StorageError::TextTooLong);
            }
            return Ok(());
        }
        let wrap = self.cursor.wrap_pending || gap;
        let (row, column) = if wrap {
            let row = if self.cursor.row == self.scroll_region.end - 1 {
                self.grid
                    .write_scrolling(self.scroll_region.clone(), text, width, style, gap)?;
                self.cursor.row
            } else {
                let row = (self.cursor.row + 1).min(self.grid.rows() - 1);
                self.grid.write(row, 0, text, width, style)?;
                if gap && (row != self.cursor.row || width_columns < columns) {
                    self.grid
                        .clear_row(self.cursor.row, columns - 1..columns, style)
                        .expect("The last column is in bounds");
                }
                row
            };
            (row, 0)
        } else {
            self.grid
                .write(self.cursor.row, self.cursor.column, text, width, style)?;
            (self.cursor.row, self.cursor.column)
        };
        self.advance(row, column + width_columns);
        Ok(())
    }

    /// Writes printable ASCII in row-sized batches. The entire input is validated
    /// before mutation. Empty input preserves pending wrapping. This does not allocate.
    /// With automatic wrapping disabled, excess bytes overwrite the last column;
    /// the final byte supplies that cell without writing intermediate values.
    #[inline]
    pub fn write_ascii(&mut self, mut bytes: &[u8], style: StyleId) -> Result<(), StorageError> {
        if bytes.iter().any(|&byte| !(b' '..=b'~').contains(&byte)) {
            return Err(StorageError::InvalidAscii);
        }
        if !self.auto_wrap && !bytes.is_empty() {
            let last = self.grid.columns() - 1;
            let prefix = bytes.len().min(last - self.cursor.column);
            if prefix != 0 {
                self.grid.write_ascii_validated(
                    self.cursor.row,
                    self.cursor.column,
                    &bytes[..prefix],
                    style,
                );
            }
            self.cursor.column += prefix;
            if prefix < bytes.len() {
                self.grid.write_ascii_validated(
                    self.cursor.row,
                    last,
                    &bytes[bytes.len() - 1..],
                    style,
                );
                self.cursor.column = last;
            }
            self.cursor.wrap_pending = false;
            return Ok(());
        }
        while !bytes.is_empty() {
            if self.cursor.wrap_pending {
                self.line_feed(style);
                self.cursor.column = 0;
            }
            let length = bytes.len().min(self.grid.columns() - self.cursor.column);
            self.grid.write_ascii_validated(
                self.cursor.row,
                self.cursor.column,
                &bytes[..length],
                style,
            );
            self.advance(self.cursor.row, self.cursor.column + length);
            bytes = &bytes[length..];
        }
        Ok(())
    }

    /// Inserts `count` blank columns at the cursor, shifting the suffix right
    /// and discarding overflow. Counts are clamped to the remaining columns;
    /// zero preserves all state. Split wide units are cleared with `style`.
    /// Cursor coordinates and region boundaries are preserved. Positive counts
    /// cancel pending wrapping. Retained units keep their text and style.
    /// This does not allocate.
    pub fn insert_cells(&mut self, count: usize, style: StyleId) {
        if count == 0 {
            return;
        }
        self.grid
            .insert_cells(self.cursor.row, self.cursor.column, count, style)
            .expect("The cursor position is in bounds");
        self.cursor.wrap_pending = false;
    }

    /// Deletes `count` columns at the cursor, shifting the suffix left and
    /// filling the right edge with blanks using `style`. Intersecting wide
    /// units are cleared completely. Count, cursor, region, and allocation
    /// rules follow [`Self::insert_cells`].
    pub fn delete_cells(&mut self, count: usize, style: StyleId) {
        if count == 0 {
            return;
        }
        self.grid
            .delete_cells(self.cursor.row, self.cursor.column, count, style)
            .expect("The cursor position is in bounds");
        self.cursor.wrap_pending = false;
    }

    /// Erases `count` columns starting at the cursor, including complete wide
    /// units intersecting either boundary, using `style` for all cleared cells.
    /// Count, cursor, region, and allocation rules follow [`Self::insert_cells`].
    pub fn erase_cells(&mut self, count: usize, style: StyleId) {
        if count == 0 {
            return;
        }
        let remaining = self.grid.columns() - self.cursor.column;
        if count >= remaining {
            self.erase_line(EraseMode::FromCursor, style);
        } else {
            self.grid
                .clear_row(
                    self.cursor.row,
                    self.cursor.column..self.cursor.column + count,
                    style,
                )
                .expect("The cursor row and erasure range are in bounds");
            self.cursor.wrap_pending = false;
        }
    }

    /// Scrolls the entire active region upward, discarding rows at its top and
    /// filling exposed bottom rows using `style`. Counts are clamped to the
    /// region's height; zero preserves all state. The cursor position, pending
    /// wrapping, and region boundaries are preserved, including when the cursor
    /// is outside the region. Retained rows keep their text and style; recycled
    /// row buffers are reused without allocating.
    #[inline]
    pub fn scroll_up(&mut self, count: usize, style: StyleId) {
        self.grid
            .scroll_up(self.scroll_region.clone(), count, style)
            .expect("The scrolling region is valid");
    }

    /// Scrolls the entire active region downward, discarding rows at its bottom
    /// and filling exposed top rows using `style`. Count, cursor, region, and
    /// allocation rules follow [`Self::scroll_up`].
    #[inline]
    pub fn scroll_down(&mut self, count: usize, style: StyleId) {
        self.grid
            .scroll_down(self.scroll_region.clone(), count, style)
            .expect("The scrolling region is valid");
    }

    /// Inserts blank rows at the cursor, shifting rows down through the scrolling
    /// region's bottom and discarding overflow. Counts are clamped to that suffix.
    /// Zero counts and cursors outside the region preserve all state. Effective
    /// edits cancel pending wrapping and preserve cursor coordinates and region
    /// boundaries. Recycled rows use `style`; retained rows keep their text and
    /// style. Buffers are reused without allocating.
    pub fn insert_lines(&mut self, count: usize, style: StyleId) {
        if count == 0 || !self.scroll_region.contains(&self.cursor.row) {
            return;
        }
        self.grid
            .scroll_down(self.cursor.row..self.scroll_region.end, count, style)
            .expect("The cursor suffix is a valid row range");
        self.cursor.wrap_pending = false;
    }

    /// Deletes rows at the cursor, shifting remaining rows up through the
    /// scrolling region's bottom and filling exposed rows using `style`.
    /// Count, cursor, region, and allocation rules follow [`Self::insert_lines`].
    pub fn delete_lines(&mut self, count: usize, style: StyleId) {
        if count == 0 || !self.scroll_region.contains(&self.cursor.row) {
            return;
        }
        self.grid
            .scroll_up(self.cursor.row..self.scroll_region.end, count, style)
            .expect("The cursor suffix is a valid row range");
        self.cursor.wrap_pending = false;
    }

    /// Erases part or all of the physical cursor row using `style`. Cursor
    /// coordinates and scrolling boundaries are preserved; pending wrapping is
    /// canceled. Cell and text-index buffers retain capacity. This does not allocate.
    pub fn erase_line(&mut self, mode: EraseMode, style: StyleId) {
        let columns = self.grid.columns();
        let range = match mode {
            EraseMode::FromCursor => self.cursor.column..columns,
            EraseMode::ToCursor => 0..self.cursor.column + 1,
            EraseMode::All => 0..columns,
        };
        if range.start == 0 && range.end == columns {
            self.grid
                .clear_rows(self.cursor.row..self.cursor.row + 1, style);
        } else {
            self.grid
                .clear_row(self.cursor.row, range, style)
                .expect("The cursor row and erasure range are in bounds");
        }
        self.cursor.wrap_pending = false;
    }

    /// Erases part or all of the physical display using `style`. The extent
    /// includes rows outside the scrolling region. Cursor coordinates and region
    /// boundaries are preserved; pending wrapping is canceled. This does not allocate.
    pub fn erase_display(&mut self, mode: EraseMode, style: StyleId) {
        match mode {
            EraseMode::FromCursor => {
                self.erase_line(mode, style);
                self.grid
                    .clear_rows(self.cursor.row + 1..self.grid.rows(), style);
            }
            EraseMode::ToCursor => {
                self.grid.clear_rows(0..self.cursor.row, style);
                self.erase_line(mode, style);
            }
            EraseMode::All => self.grid.clear(style),
        }
        self.cursor.wrap_pending = false;
    }

    /// Moves to column zero and cancels pending wrapping.
    pub fn carriage_return(&mut self) {
        self.cursor.column = 0;
        self.cursor.wrap_pending = false;
    }

    /// Advances one row, scrolling at the region's bottom edge. Outside the
    /// region, movement stops at the physical bottom without scrolling. The
    /// column is preserved and pending wrapping is canceled. Exposed cells use `style`.
    #[inline]
    pub fn line_feed(&mut self, style: StyleId) {
        if self.cursor.row == self.scroll_region.end - 1 {
            self.grid
                .scroll_up(self.scroll_region.clone(), 1, style)
                .expect("The scrolling region is valid");
        } else {
            self.cursor.row = (self.cursor.row + 1).min(self.grid.rows() - 1);
        }
        self.cursor.wrap_pending = false;
    }

    /// Moves up one row, scrolling downward at the region's top edge. Outside
    /// the region, movement stops at the physical top without scrolling. The
    /// column is preserved and pending wrapping is canceled. Exposed cells use `style`.
    #[inline]
    pub fn reverse_index(&mut self, style: StyleId) {
        if self.cursor.row == self.scroll_region.start {
            self.grid
                .scroll_down(self.scroll_region.clone(), 1, style)
                .expect("The scrolling region is valid");
        } else {
            self.cursor.row = self.cursor.row.saturating_sub(1);
        }
        self.cursor.wrap_pending = false;
    }

    /// Moves one column left, stopping at column zero, and cancels pending wrapping.
    pub fn backspace(&mut self) {
        self.cursor.column = self.cursor.column.saturating_sub(1);
        self.cursor.wrap_pending = false;
    }

    /// Preserves the upper-left grid intersection and clamps cursor coordinates.
    /// A size change cancels pending wrapping; an unchanged size preserves it.
    /// Height changes reset the scrolling region to the full grid; width-only
    /// changes preserve it. Tab stops in the width intersection are preserved;
    /// cropped stops are discarded and added columns receive default stops.
    /// Height-only changes preserve stops. Every error preserves contents, cursor,
    /// region, and stops. Automatic wrapping and origin modes are preserved.
    /// The cursor remains in the active region while origin mode is enabled.
    /// Reserved tab capacity survives shrinking.
    pub fn resize(
        &mut self,
        columns: usize,
        rows: usize,
        style: StyleId,
    ) -> Result<(), StorageError> {
        if columns == self.grid.columns() && rows == self.grid.rows() {
            return Ok(());
        }
        validate_dimensions(columns, rows)?;
        self.tabs.reserve_columns(columns)?;
        let old_columns = self.grid.columns();
        let height_changed = rows != self.grid.rows();
        self.grid.resize(columns, rows, style)?;
        self.tabs.resize_prepared(old_columns, columns);
        if height_changed {
            self.scroll_region = 0..rows;
        }
        self.cursor.row = self.cursor.row.min(rows - 1);
        self.cursor.column = self.cursor.column.min(columns - 1);
        self.cursor.wrap_pending = false;
        Ok(())
    }

    #[inline]
    fn advance(&mut self, row: usize, end: usize) {
        self.cursor = Cursor {
            row,
            column: end.min(self.grid.columns() - 1),
            wrap_pending: end == self.grid.columns() && self.auto_wrap,
        };
    }
}
