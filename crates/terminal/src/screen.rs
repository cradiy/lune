use std::{
    num::NonZeroU32,
    ops::Range,
    sync::{Arc, Weak},
};

use lune_kitty_graphics::{
    DeleteTarget, EvictionRanking, ImageChange, ImageStore, PlacementError, PlacementPosition,
};
use lune_text::WidthPolicy;

mod reflow;

use crate::row::SizedRows;
use crate::sizing::{SizedBlock, SizedBudget, SizedWidth, joins_grapheme};
use crate::{
    CellPosition, CellWidth, GraphicsPlacement, Grid, HistoryLimits, MAX_CELL_TEXT_BYTES,
    PixelSize, RowHistory, ScreenView, SizedTextRow, StorageError, StyleId, TextSize, Viewport,
};
use crate::{graphics::GraphicsPlacements, grid::validate_dimensions, tabs::TabStops};

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

/// Geometry requested for the cursor at its current cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
}

/// Cursor presentation metadata. Timing and painting belong to the consumer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorAppearance {
    pub visible: bool,
    pub blinking: bool,
    pub shape: CursorShape,
}

impl Default for CursorAppearance {
    fn default() -> Self {
        Self {
            visible: true,
            blinking: true,
            shape: CursorShape::Block,
        }
    }
}

/// The two display buffers owned by a screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScreenBuffer {
    #[default]
    Primary,
    Alternate,
}

/// Primary/alternate storage with live cursor placement and delayed wrapping.
/// Callers supply complete display units, their widths, and style keys.
#[derive(Debug)]
pub struct Screen {
    grid: Grid,
    graphics: GraphicsPlacements,
    graphics_cell: Option<PixelSize>,
    history: RowHistory,
    history_capture: bool,
    display_offset: usize,
    inactive: InactiveBuffer,
    active_buffer: ScreenBuffer,
    cursor: Cursor,
    saved_cursor: SavedCursor,
    scroll_region: Range<usize>,
    tabs: TabStops,
    auto_wrap: bool,
    origin_mode: bool,
    insert_mode: bool,
    cursor_appearance: CursorAppearance,
    reverse_video: bool,
    sized_budget: Option<Arc<SizedBudget>>,
    // The physical cursor stays in the last column with automatic wrapping off.
    sized_right_edge: Option<Weak<SizedBlock>>,
}

#[derive(Debug)]
struct InactiveBuffer {
    grid: Grid,
    graphics: GraphicsPlacements,
    saved_cursor: SavedCursor,
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
    /// Borrows the current display window, with row coordinates local to it.
    pub fn viewport(&self) -> Viewport<'_> {
        Viewport::new(self.view(), self.viewport_offset(), &self.cursor)
    }

    /// Primary rows above the live buffer. Alternate buffers report zero.
    /// Zero follows output; positive offsets keep the top retained row anchored.
    pub fn viewport_offset(&self) -> usize {
        if self.active_buffer == ScreenBuffer::Primary {
            self.display_offset
        } else {
            0
        }
    }

    /// Moves toward older primary rows, clamping to the oldest retained row.
    /// Counts through `usize::MAX` are accepted. Alternate-buffer calls preserve
    /// the primary position. Contents, cursor, and modes are preserved without allocating.
    /// Paired text sinks retain their composition context.
    pub fn scroll_viewport_up(&mut self, count: usize) {
        if self.active_buffer == ScreenBuffer::Primary {
            self.display_offset = self
                .display_offset
                .saturating_add(count)
                .min(self.history.len());
        }
    }

    /// Moves toward the live buffer, clamping at zero. Reaching zero resumes
    /// output following. Other rules follow [`Self::scroll_viewport_up`].
    pub fn scroll_viewport_down(&mut self, count: usize) {
        if self.active_buffer == ScreenBuffer::Primary {
            self.display_offset = self.display_offset.saturating_sub(count);
        }
    }

    /// Follows the live primary buffer. Alternate-buffer calls preserve the
    /// primary position. This preserves contents, cursor, and modes without allocating.
    pub fn scroll_viewport_to_bottom(&mut self) {
        if self.active_buffer == ScreenBuffer::Primary {
            self.display_offset = 0;
        }
    }

    fn history_captured(&mut self, captured: bool) {
        if captured {
            self.scroll_captured_history(1);
        } else {
            self.scroll_graphics(self.scroll_region.start, 1, false);
        }
    }

    fn scroll_captured_history(&mut self, lines: usize) {
        if lines == 0 {
            return;
        }
        if self.display_offset != 0 {
            self.display_offset = self
                .display_offset
                .saturating_add(lines)
                .min(self.history.len());
        }
        self.shift_graphics_with_history(-(lines as i64));
    }

    fn shift_graphics_with_history(&mut self, delta: i64) {
        if !self.graphics.is_empty() {
            let first_row = -(self.history.len() as i64);
            let cell = self.graphics_cell_extent();
            let last_row = if self.scroll_region.end == self.grid.rows() {
                i64::MAX
            } else {
                self.scroll_region.end as i64
            };
            self.graphics
                .scroll_with_history(first_row..last_row, delta, cell);
        }
    }

    fn capture_history(&mut self, style: StyleId) -> Result<bool, StorageError> {
        let captured = self
            .history
            .capture(&mut self.grid, self.scroll_region.end, style)?;
        self.history_captured(captured);
        Ok(captured)
    }

    /// Borrows chronological rows of the active buffer. Primary views include
    /// retained history; alternate views start at their first visible row.
    pub fn view(&self) -> ScreenView<'_> {
        ScreenView::new(
            &self.grid,
            (self.active_buffer == ScreenBuffer::Primary).then_some(&self.history),
        )
    }

    /// Borrows retained primary-screen rows, oldest first.
    pub fn history(&self) -> &RowHistory {
        &self.history
    }

    /// Sets bounded primary-screen capture and clamps the viewport to retained
    /// history. Either zero limit disables capture and resumes live following.
    pub fn set_history_limits(&mut self, limits: HistoryLimits) -> Result<(), StorageError> {
        self.history.set_limits(limits)?;
        self.discard_evicted_graphics();
        self.display_offset = self.display_offset.min(self.history.len());
        self.update_history_capture();
        Ok(())
    }

    /// Releases retained rows and follows the primary live buffer. Limits, both
    /// display buffers, cursor, and modes are preserved.
    pub fn clear_history(&mut self) {
        let primary = if self.active_buffer == ScreenBuffer::Primary {
            &mut self.grid
        } else {
            &mut self.inactive.grid
        };
        primary.split_sized_text(0);
        self.history.clear();
        self.discard_evicted_graphics();
        self.display_offset = 0;
    }

    fn update_history_capture(&mut self) {
        self.history_capture = self.active_buffer == ScreenBuffer::Primary
            && self.scroll_region.start == 0
            && self.history.enabled_for(self.grid.columns());
    }

    fn discard_evicted_graphics(&mut self) {
        let first_row = -(self.history.len() as i64);
        let cell = self.graphics_cell_extent();
        let primary = if self.active_buffer == ScreenBuffer::Primary {
            &mut self.graphics
        } else {
            &mut self.inactive.graphics
        };
        primary.discard_history_before(first_row, cell);
    }

    #[inline]
    fn scroll_graphics(&mut self, start: usize, count: usize, down: bool) {
        if !self.graphics.is_empty() && count != 0 {
            self.scroll_graphics_nonempty(start, count, down);
        }
    }

    #[cold]
    fn scroll_graphics_nonempty(&mut self, start: usize, count: usize, down: bool) {
        let end = self.scroll_region.end;
        let whole_screen = start == 0 && end == self.grid.rows();
        let moves = count.min(end - start) as i64;
        let cell = self.graphics_cell_extent();
        if whole_screen && down {
            let first_retained_row = -(self.view().history_rows() as i64);
            self.graphics.reverse_page(first_retained_row, moves, cell);
        } else {
            let end = if whole_screen { i64::MAX } else { end as i64 };
            let region = start as i64..end;
            let delta = if down { moves } else { -moves };
            self.graphics.scroll(region.clone(), region, delta, cell);
        }
    }

    fn disconnect_top(&mut self, top: usize) {
        if top == 0 && self.active_buffer == ScreenBuffer::Primary {
            self.history.disconnect();
        }
    }

    #[inline]
    pub(crate) fn history_capture_enabled(&self) -> bool {
        self.history_capture
    }

    #[inline]
    pub(crate) fn prepare_ascii_text(
        &mut self,
        length: usize,
        style: StyleId,
    ) -> Result<bool, StorageError> {
        if !self.history_capture || length == 0 {
            return Ok(false);
        }
        let Some(rows_before_scroll) = (self.scroll_region.end - 1).checked_sub(self.cursor.row)
        else {
            return Ok(false);
        };
        let columns = self.grid.columns();
        if self.grid.has_sized_text() {
            // Once scrolling starts, the new bottom row has no protected fragments.
            return self
                .history
                .prepare(columns, (length - 1) / columns + 1, style);
        }
        if !self.auto_wrap {
            return Ok(false);
        }
        let start = if self.cursor.wrap_pending {
            columns
        } else {
            self.cursor.column
        };
        let wraps = start.saturating_add(length - 1) / columns;
        let scrolls = wraps.saturating_sub(rows_before_scroll);
        self.history.prepare(columns, scrolls, style)
    }

    pub(crate) fn finish_text_reservation(&mut self) {
        self.history.finish();
    }

    /// Allocates two equal grids, each bounded by `MAX_CELLS`, and shared tabs.
    /// Both buffers start erased with default style; the primary starts active.
    pub fn new(columns: usize, rows: usize) -> Result<Self, StorageError> {
        Ok(Self {
            grid: Grid::new(columns, rows)?,
            graphics: GraphicsPlacements::default(),
            graphics_cell: None,
            history: RowHistory::default(),
            history_capture: false,
            display_offset: 0,
            inactive: InactiveBuffer {
                grid: Grid::new(columns, rows)?,
                graphics: GraphicsPlacements::default(),
                saved_cursor: SavedCursor::default(),
            },
            active_buffer: ScreenBuffer::Primary,
            cursor: Cursor::default(),
            saved_cursor: SavedCursor::default(),
            scroll_region: 0..rows,
            tabs: TabStops::new(columns)?,
            auto_wrap: true,
            origin_mode: false,
            insert_mode: false,
            cursor_appearance: CursorAppearance::default(),
            reverse_video: false,
            sized_budget: None,
            sized_right_edge: None,
        })
    }

    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    pub fn active_buffer(&self) -> ScreenBuffer {
        self.active_buffer
    }

    pub fn graphics_placements(&self) -> &[GraphicsPlacement] {
        self.graphics.as_slice()
    }

    #[cfg(test)]
    pub(crate) fn all_graphics_placements(&self) -> impl Iterator<Item = &GraphicsPlacement> {
        self.graphics
            .as_slice()
            .iter()
            .chain(self.inactive.graphics.as_slice())
    }

    #[inline]
    pub(crate) fn reap_graphics_orphans(&mut self, images: &mut [ImageStore; 2]) {
        let active = self.active_buffer as usize;
        if self.graphics.has_orphans() {
            self.graphics.reap_orphans(&mut images[active]);
        }
        if self.inactive.graphics.has_orphans() {
            self.inactive.graphics.reap_orphans(&mut images[1 - active]);
        }
    }

    #[inline]
    pub(crate) fn clear_live_graphics(&mut self, styles: &crate::StyleTable) {
        if !self.graphics.is_empty() {
            let history = (self.active_buffer == ScreenBuffer::Primary).then_some(&self.history);
            self.graphics
                .clear_live(ScreenView::new(&self.grid, history), styles);
        }
    }

    #[inline]
    pub(crate) fn clear_alternate_display(&mut self, style: StyleId) {
        debug_assert_eq!(self.active_buffer, ScreenBuffer::Alternate);
        if !self.graphics.is_empty() {
            self.graphics.clear_nonvirtual();
        }
        self.erase_display(EraseMode::All, style);
    }

    pub(crate) fn delete_placements(
        &mut self,
        target: DeleteTarget,
        styles: &crate::StyleTable,
        removed: impl FnMut(u32),
    ) {
        let history = (self.active_buffer == ScreenBuffer::Primary).then_some(&self.history);
        let view = ScreenView::new(&self.grid, history);
        self.graphics
            .delete(target, self.cursor, view, styles, removed);
    }

    pub fn graphics_revision(&self) -> Option<u64> {
        self.graphics.revision()
    }

    pub fn has_virtual_graphics(&self) -> bool {
        self.graphics.has_virtual()
    }

    pub fn has_virtual_graphics_parents(&self) -> bool {
        self.graphics.has_virtual_parents()
    }

    pub(crate) fn set_graphics_cell(&mut self, cell: Option<PixelSize>) {
        if self.graphics_cell != cell {
            self.graphics_cell = cell;
            let cell = self.graphics_cell_extent();
            self.graphics.set_cell_size(cell);
            self.inactive.graphics.set_cell_size(cell);
        }
    }

    pub(crate) fn graphics_cell_extent(&self) -> PixelSize {
        self.graphics_cell.unwrap_or(PixelSize {
            width: NonZeroU32::MIN,
            height: NonZeroU32::MIN,
        })
    }

    pub(crate) fn apply_image(&mut self, change: ImageChange<'_>) -> Result<(), PlacementError> {
        match change {
            ImageChange::RankEvictions { placement, ranking } => {
                self.rank_graphics_evictions(
                    placement.map(|placement| placement.position),
                    ranking,
                )?;
            }
            ImageChange::Remove { replaced, evicted } => {
                self.graphics.remove_images(replaced, evicted);
            }
            ImageChange::Place {
                placement,
                dimensions,
                replaces_image,
                anonymous,
                evicted,
            } => {
                let image = GraphicsPlacement::at_cursor(
                    self.cursor,
                    placement,
                    dimensions,
                    self.graphics_cell_extent(),
                    anonymous,
                )?;
                let [columns, rows] = image.cursor_travel(self.graphics_cell.is_some());
                self.graphics.insert(image, replaces_image, evicted)?;
                if placement.position == PlacementPosition::Cursor && !placement.keep_cursor {
                    self.move_cursor_right(columns);
                    self.move_cursor_down(rows);
                }
            }
        }
        Ok(())
    }

    #[cold]
    fn rank_graphics_evictions(
        &self,
        position: Option<PlacementPosition>,
        ranking: &mut EvictionRanking,
    ) -> Result<(), PlacementError> {
        for placement in self.graphics_placements() {
            ranking.mark_placed(placement.placement.image)?;
        }
        if let Some(PlacementPosition::Relative(parent)) = position {
            self.graphics
                .for_each_ancestor(parent, |key| ranking.protect(key))?;
        }
        Ok(())
    }

    /// Read-only storage for either buffer, including its soft-wrap connections.
    pub fn buffer(&self, buffer: ScreenBuffer) -> &Grid {
        if buffer == self.active_buffer {
            &self.grid
        } else {
            &self.inactive.grid
        }
    }

    /// Selects storage and its cursor save slot. Live cursor placement, pending
    /// wrapping, scrolling region, tabs, modes, and appearance are shared and
    /// preserved. The primary viewport position survives alternate visits; the
    /// alternate viewport follows its live buffer. Switching does not allocate.
    pub fn select_buffer(&mut self, buffer: ScreenBuffer) {
        if buffer == self.active_buffer {
            return;
        }
        self.sized_right_edge = None;
        std::mem::swap(&mut self.grid, &mut self.inactive.grid);
        std::mem::swap(&mut self.graphics, &mut self.inactive.graphics);
        std::mem::swap(&mut self.saved_cursor, &mut self.inactive.saved_cursor);
        self.active_buffer = buffer;
        self.update_history_capture();
    }

    pub(crate) fn style_ids(&self) -> impl Iterator<Item = StyleId> + '_ {
        [&self.grid, &self.inactive.grid]
            .into_iter()
            .flat_map(|grid| {
                grid.iter_rows()
                    .flat_map(|row| row.cells().iter().map(|cell| cell.style()))
            })
            .chain(self.history.style_ids())
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    /// A copyable presentation snapshot. Initially a visible blinking block.
    pub fn cursor_appearance(&self) -> CursorAppearance {
        self.cursor_appearance
    }

    /// Replaces presentation metadata while preserving contents, cursor placement,
    /// pending wrapping, modes, and the save slot. Resizing and cursor restores
    /// preserve this state; reset restores the default. This does not allocate.
    pub fn set_cursor_appearance(&mut self, appearance: CursorAppearance) {
        self.cursor_appearance = appearance;
    }

    /// Whether DECSCNM requests reverse video for the entire display.
    /// This presentation flag is shared by both buffers.
    pub fn reverse_video(&self) -> bool {
        self.reverse_video
    }

    pub(crate) fn set_reverse_video(&mut self, enabled: bool) {
        self.reverse_video = enabled;
    }

    /// Clears both buffers to spaces with `style`, selects the primary, and
    /// preserves dimensions and row buffers. Homes the cursor, clears pending
    /// wrapping, restores the full-screen scrolling region and default tab stops,
    /// enables automatic wrapping, and disables origin mode. Cursor appearance and
    /// both save slots return to defaults.
    /// History rows are released and their configured limits are preserved.
    /// The viewport follows the primary live buffer.
    /// Grid row, queue, tab, and text-index capacities are retained. This does not allocate.
    pub fn reset(&mut self, style: StyleId) {
        self.select_buffer(ScreenBuffer::Primary);
        self.grid.clear(style);
        self.inactive.grid.clear(style);
        self.graphics.clear();
        self.inactive.graphics.clear();
        self.clear_history();
        self.inactive.saved_cursor = SavedCursor::default();
        self.cursor = Cursor::default();
        self.sized_right_edge = None;
        self.saved_cursor = SavedCursor::default();
        self.scroll_region = 0..self.grid.rows();
        self.reset_tab_stops();
        self.auto_wrap = true;
        self.origin_mode = false;
        self.insert_mode = false;
        self.cursor_appearance = CursorAppearance::default();
        self.reverse_video = false;
        self.update_history_capture();
    }

    /// Restores cursor modes, appearance, full-screen margins, and the active
    /// save slot. Keeps physical cursor coordinates, both buffers, history,
    /// viewport, and tabs; cancels pending wrapping. This does not allocate.
    pub fn soft_reset(&mut self) {
        self.cancel_pending_wrap();
        self.saved_cursor = SavedCursor::default();
        self.scroll_region = 0..self.grid.rows();
        self.auto_wrap = true;
        self.origin_mode = false;
        self.insert_mode = false;
        self.cursor_appearance = CursorAppearance::default();
        self.update_history_capture();
    }

    #[cold]
    pub(crate) fn alignment_test(&mut self) {
        self.disconnect_top(0);
        self.grid.fill_scalar('E', StyleId(0));
        self.cursor = Cursor::default();
        self.sized_right_edge = None;
        self.origin_mode = false;
        self.scroll_region = 0..self.grid.rows();
        self.update_history_capture();
    }

    /// Whether writes insert columns and shift the row suffix. Disabled initially.
    pub fn insert_mode(&self) -> bool {
        self.insert_mode
    }

    /// Changes insertion behavior while preserving all other state, including
    /// pending wrapping and the save slot. Resizing and cursor restores preserve
    /// this mode; resetting disables it. This does not allocate.
    pub fn set_insert_mode(&mut self, enabled: bool) {
        self.insert_mode = enabled;
    }

    /// Replaces the active buffer's saved position, origin/automatic-wrap modes,
    /// and pending wrapping. Rows are relative to the region when origin mode is
    /// enabled. Active state, contents, region, and tabs are preserved.
    /// Each save slot initially holds the initial cursor state. This does not allocate.
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

    /// Restores the active buffer's save slot against the current dimensions and
    /// scrolling region. Coordinates clamp as in [`Self::set_cursor`]. Pending wrapping
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

    /// Restores the default stops at columns 9, 17, 25, and so on.
    pub fn reset_tab_stops(&mut self) {
        self.tabs.reset(self.grid.columns());
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
            self.cancel_pending_wrap();
        }
    }

    /// Moves across `count` tab stops strictly before the cursor, stopping at
    /// column zero when stops are exhausted. Other rules follow [`Self::tab_forward`].
    pub fn tab_backward(&mut self, count: usize) {
        if count != 0 {
            self.cursor.column = self.tabs.backward(self.cursor.column, count);
            self.cancel_pending_wrap();
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
        self.update_history_capture();
        self.set_cursor(0, 0);
        Ok(())
    }

    /// Addresses a zero-based position, clearing pending wrapping. With origin
    /// mode enabled, rows are relative to the scrolling region's top and clamp
    /// at its bottom. Otherwise rows address the full screen. Columns clamp at
    /// the physical right edge and may address wide-unit continuations.
    /// Coordinates through `usize::MAX` are accepted without allocating.
    pub fn set_cursor(&mut self, row: usize, column: usize) {
        self.sized_right_edge = None;
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
        self.cancel_pending_wrap();
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
        self.cancel_pending_wrap();
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
        self.cancel_pending_wrap();
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
        self.cancel_pending_wrap();
    }

    /// Writes a complete display unit, then advances by its supplied width.
    /// With automatic wrapping enabled, filling the right edge leaves the cursor
    /// in the last column with wrapping pending. A double-width unit that cannot
    /// fit wraps whole; units intersecting the old right-edge gap are cleared
    /// using `style`.
    /// Wrapping at the region's bottom scrolls only that region. Outside the
    /// region, wrapping stops at the physical bottom and returns to column zero.
    /// Insertion mode shifts the destination row's suffix by the unit's width
    /// and discards overflow, repairing split wide units. Text and index capacity
    /// are reserved before mutation. Units wider than the grid fail.
    /// Every error preserves contents and cursor.
    /// With automatic wrapping disabled, a unit that cannot fit the remaining
    /// columns is validated and ignored. Writes ending at the right edge keep
    /// the cursor in the last column with pending wrapping clear.
    pub fn write(
        &mut self,
        text: &str,
        width: CellWidth,
        style: StyleId,
    ) -> Result<(), StorageError> {
        self.write_unit(text, width, style).map(|_| ())
    }

    #[inline]
    pub(crate) fn write_unit(
        &mut self,
        text: &str,
        width: CellWidth,
        style: StyleId,
    ) -> Result<Option<CellPosition>, StorageError> {
        if self.grid.has_sized_text() {
            return self.write_unit_around_blocks(text, width, style);
        }
        self.write_unit_at_cursor(text, width, style)
    }

    #[inline]
    fn write_unit_at_cursor(
        &mut self,
        text: &str,
        width: CellWidth,
        style: StyleId,
    ) -> Result<Option<CellPosition>, StorageError> {
        if self.insert_mode {
            self.write_inserted(text, width, style)
        } else {
            self.write_mode::<false>(text, width, style)
        }
    }

    #[cold]
    fn write_unit_around_blocks(
        &mut self,
        text: &str,
        width: CellWidth,
        style: StyleId,
    ) -> Result<Option<CellPosition>, StorageError> {
        let previous = self.cursor;
        let Some(candidate) = self.skip_sized_lower_rows(previous, width.columns()) else {
            return Ok(None);
        };
        self.cursor = candidate;
        let result = self.write_unit_at_cursor(text, width, style);
        if result.is_err() {
            self.cursor = previous;
        }
        result
    }

    pub(crate) fn has_sized_text(&self) -> bool {
        self.grid.has_sized_text()
    }

    #[inline]
    pub(crate) fn has_sized_text_storage(&self) -> bool {
        self.sized_budget.is_some()
    }

    #[cold]
    pub(crate) fn extend_sized_text(
        &mut self,
        suffix: &str,
        policy: WidthPolicy,
    ) -> Result<bool, StorageError> {
        let view = self.view();
        let history = view.history_rows();
        let cursor_row = history + self.cursor.row;
        let past_edge = self.sized_right_edge.as_ref().is_some_and(|origin| {
            self.cursor.column + 1 == self.grid.columns()
                && view
                    .row(cursor_row)
                    .and_then(|row| row.sized_text_at(self.cursor.column))
                    .is_some_and(|part| std::ptr::eq(origin.as_ptr(), Arc::as_ptr(&part.block)))
        });
        let cursor_column = self.cursor.column + usize::from(self.cursor.wrap_pending || past_edge);
        let (row, column) = if cursor_column != 0 {
            (cursor_row, cursor_column - 1)
        } else {
            let Some(row) = cursor_row.checked_sub(1) else {
                return Ok(false);
            };
            let Some(end) = view.row(row).and_then(crate::Row::wrap_end) else {
                return Ok(false);
            };
            (row, end - 1)
        };
        let Some(part) = view.row(row).and_then(|row| row.sized_text_at(column)) else {
            return Ok(false);
        };
        if policy.columns(suffix) != 0 && !joins_grapheme(part.text(), suffix) {
            return Ok(false);
        }
        let column = part.column;
        let height = usize::from(part.size().scale);
        let Some(start) = row.checked_sub(part.row_offset) else {
            return Ok(false);
        };
        let rows = start..start + height;
        if !rows.clone().enumerate().all(|(offset, index)| {
            view.row(index)
                .and_then(|row| row.sized_text_at(column))
                .is_some_and(|candidate| {
                    candidate.row_offset == offset
                        && candidate.column == column
                        && candidate.belongs_to(part)
                })
        }) {
            return Ok(false);
        }
        let previous_columns = part.block.columns;
        let mut columns = part.block.columns_with_suffix(suffix);
        if columns > previous_columns && (start < history || columns > self.grid.columns()) {
            columns = previous_columns;
        }
        let Some(replacement) = part.block.append(suffix, columns)? else {
            return Ok(true);
        };
        if columns > previous_columns {
            let style = view.row(start).unwrap().cells()[column].style();
            let original = Arc::clone(&part.block);
            self.grow_sized_text(start - history, column, style, original, replacement)?;
            return Ok(true);
        }
        let retained = history.saturating_sub(start).min(height);
        let bytes = self.history.retained_bytes() - retained * part.retained_bytes()
            + retained * replacement.retained_bytes();
        if retained != 0 && bytes > self.history.limits().max_bytes {
            return Ok(true);
        }
        for row in rows {
            if row < history {
                self.history
                    .replace_sized_block(row, column, Arc::clone(&replacement));
            } else {
                self.grid
                    .row_mut(row - history)
                    .expect("The sized row is live")
                    .replace_sized_block(column, Arc::clone(&replacement));
            }
        }
        if columns < previous_columns && cursor_row == start && cursor_column > column + columns {
            let end = cursor_column.saturating_sub(previous_columns - columns);
            self.advance_sized(self.cursor.row, end.max(column + columns), &replacement);
        } else if past_edge {
            self.sized_right_edge = Some(Arc::downgrade(&replacement));
        }
        Ok(true)
    }

    fn grow_sized_text(
        &mut self,
        row: usize,
        column: usize,
        style: StyleId,
        original: Arc<SizedBlock>,
        replacement: Arc<SizedBlock>,
    ) -> Result<(), StorageError> {
        let added = column + original.columns;
        let end = column + replacement.columns;
        let fits = end <= self.grid.columns()
            && self
                .grid
                .row(row)
                .unwrap()
                .sized_text_at(added)
                .is_none_or(|part| part.row_offset == 0);
        if fits {
            let target = self.grid.row_mut(row).expect("The sized row is live");
            let wrapped = target.wrap_end() == Some(added);
            if self.insert_mode {
                target
                    .insert_cells(added, end - added, style)
                    .expect("The width fits");
            }
            target
                .clear_range(added..end, style)
                .expect("The width fits");
            target.replace_sized_block(column, Arc::clone(&replacement));
            if wrapped && !self.insert_mode {
                target.set_wrap_end(Some(end));
            }
            if self.cursor.row == row {
                self.advance_sized(row, end, &replacement);
            }
        } else {
            let origin = Cursor {
                row,
                column,
                wrap_pending: false,
            };
            if self.place_sized_text(origin, replacement, style)? {
                original.erase();
            }
        }
        Ok(())
    }

    fn skip_sized_lower_rows(&self, mut candidate: Cursor, width: usize) -> Option<Cursor> {
        let columns = self.grid.columns();
        loop {
            let wraps =
                candidate.wrap_pending || (self.auto_wrap && candidate.column + width > columns);
            let (row, column) = if wraps {
                if candidate.row == self.scroll_region.end - 1 {
                    return Some(candidate);
                }
                ((candidate.row + 1).min(self.grid.rows() - 1), 0)
            } else {
                (candidate.row, candidate.column)
            };
            let next = self
                .grid
                .row(row)
                .unwrap()
                .sized_text_rows_from(column)
                .take_while(|part| part.column < column + width)
                .find(|part| part.row_offset != 0);
            let Some(part) = next else {
                return Some(candidate);
            };
            let end = part.columns().end;
            candidate = Cursor {
                row,
                column: end.min(columns - 1),
                wrap_pending: end == columns || (!self.auto_wrap && end + width > columns),
            };
            if candidate.wrap_pending
                && row == self.grid.rows() - 1
                && row != self.scroll_region.end - 1
            {
                return None;
            }
        }
    }

    pub(crate) fn write_sized_text(
        &mut self,
        text: &str,
        size: TextSize,
        width: SizedWidth,
        style: StyleId,
    ) -> Result<(), StorageError> {
        if width.columns(size) > self.grid.columns() || usize::from(size.scale) > self.grid.rows() {
            return Ok(());
        }
        let budget = self.sized_budget.get_or_insert_with(Default::default);
        let Some(block) = SizedBlock::new(size, width, text, budget)? else {
            return Ok(());
        };
        self.place_sized_text(self.cursor, block, style).map(|_| ())
    }

    fn place_sized_text(
        &mut self,
        mut candidate: Cursor,
        block: Arc<SizedBlock>,
        style: StyleId,
    ) -> Result<bool, StorageError> {
        let height = usize::from(block.size.scale);
        let columns = self.grid.columns();
        let placement_columns = block.placement_columns();
        let width = if placement_columns <= columns {
            placement_columns
        } else {
            block.columns
        };
        if width > columns || height > self.grid.rows() || width == 0 {
            return Ok(false);
        }
        if !self.auto_wrap && !candidate.wrap_pending {
            candidate.column = candidate.column.min(columns - width);
        }
        if self.grid.has_sized_text() {
            let Some(adjusted) = self.skip_sized_lower_rows(candidate, width) else {
                return Ok(false);
            };
            candidate = adjusted;
        }
        let region = self.scroll_region.clone();
        let mut scroll = 0;
        let mut wrap = None;
        if candidate.wrap_pending || candidate.column + width > columns {
            let end = if candidate.wrap_pending {
                columns
            } else {
                candidate.column
            };
            if candidate.row == region.end - 1 {
                scroll = 1;
                wrap = Some((candidate.row, end));
                candidate.column = 0;
            } else if candidate.row + 1 < self.grid.rows() {
                wrap = Some((candidate.row, end));
                candidate.row += 1;
                candidate.column = 0;
            } else {
                candidate.column = 0;
            }
        }
        let in_region = region.contains(&candidate.row);
        let bottom = if in_region {
            if height > region.len() {
                return Ok(false);
            }
            region.end
        } else {
            self.grid.rows()
        };
        let overflow = (candidate.row + height).saturating_sub(bottom);
        if in_region {
            scroll += overflow;
        } else if overflow != 0 {
            return Ok(false);
        }
        let row = candidate.row - overflow;
        let column = candidate.column;
        let mut exposed: [Option<Box<SizedRows>>; 7] = std::array::from_fn(|_| None);
        for (offset, prepared) in exposed[..height].iter_mut().enumerate() {
            let target = row + offset;
            let source = if region.contains(&target) {
                target + scroll
            } else {
                target
            };
            if region.contains(&target) && source >= region.end {
                *prepared = Some(crate::Row::prepare_sized()?);
            } else {
                self.grid.row_mut(source)?.reserve_sized()?;
            }
        }
        self.scroll_up(scroll, style)?;
        if let Some((previous, end)) = wrap {
            if !region.contains(&previous) {
                self.grid.set_wrap_end(previous, Some(end));
            } else if previous >= region.start + scroll {
                self.grid.set_wrap_end(previous - scroll, Some(end));
            } else if self.history_capture && previous + 1 == scroll {
                self.history.connect_latest(end);
            }
        }
        for (row_offset, prepared) in exposed.into_iter().take(height).enumerate() {
            let target = self
                .grid
                .row_mut(row + row_offset)
                .expect("The placement fits");
            if let Some(prepared) = prepared {
                target.install_prepared_sized(prepared);
            }
            if self.insert_mode {
                target
                    .insert_cells(column, block.columns, style)
                    .expect("The placement fits");
            }
            target.add_sized(
                SizedTextRow {
                    column,
                    row_offset,
                    block: Arc::clone(&block),
                },
                style,
            );
        }
        if height > 1 {
            self.grid.mark_sized_text(row);
        }
        self.advance_sized(row, column + block.columns, &block);
        Ok(true)
    }

    /// Replaces a leading display unit and returns its resulting physical position.
    /// The caller retains this position while extending a grapheme and ends that
    /// association when controls change the text's placement or interpretation.
    /// Continuation positions and units wider than the screen are rejected.
    ///
    /// Equal widths preserve the row's soft-wrap connection. Width changes cancel
    /// it. Insertion mode moves the suffix only by the width difference; shrinking
    /// exposes styled erased cells at the right edge. Earlier discarded content
    /// stays discarded. Overwrite mode erases a released continuation in place.
    ///
    /// Growth across the right edge wraps the complete unit, clearing its old edge
    /// cell and establishing an early-wrap connection. Scrolling follows the active
    /// region. With automatic wrapping disabled, growth that cannot fit fails.
    /// Success places the cursor after the revised footprint, using physical rows
    /// independently of origin mode; origin mode requires the position to be
    /// inside the scrolling region. Every error preserves both grids, cursor,
    /// connections, and modes; text storage is reserved before any shift or scroll.
    pub fn revise_unit(
        &mut self,
        position: CellPosition,
        text: &str,
        width: CellWidth,
        style: StyleId,
    ) -> Result<CellPosition, StorageError> {
        let columns = self.grid.columns();
        let cell = self
            .grid
            .row(position.row)
            .and_then(|row| row.cells().get(position.column))
            .ok_or(StorageError::OutOfBounds)?;
        if cell.is_continuation()
            || width.columns() > columns
            || (self.origin_mode && !self.scroll_region.contains(&position.row))
        {
            return Err(StorageError::OutOfBounds);
        }
        if position.column + width.columns() <= columns {
            self.grid
                .revise(position, text, width, style, self.insert_mode)?;
            self.advance(position.row, position.column + width.columns());
            return Ok(position);
        }
        if !self.auto_wrap {
            return Err(StorageError::OutOfBounds);
        }
        let cursor = self.cursor;
        self.cursor = Cursor {
            row: position.row,
            column: position.column,
            wrap_pending: false,
        };
        let result = if self.insert_mode {
            self.write_mode::<true>(text, width, style)
        } else {
            self.write_mode::<false>(text, width, style)
        };
        if let Err(error) = result {
            self.cursor = cursor;
            return Err(error);
        }
        Ok(CellPosition {
            row: self.cursor.row,
            column: 0,
        })
    }

    #[inline(never)]
    fn write_inserted(
        &mut self,
        text: &str,
        width: CellWidth,
        style: StyleId,
    ) -> Result<Option<CellPosition>, StorageError> {
        self.write_mode::<true>(text, width, style)
    }

    #[inline]
    fn write_mode<const INSERT: bool>(
        &mut self,
        text: &str,
        width: CellWidth,
        style: StyleId,
    ) -> Result<Option<CellPosition>, StorageError> {
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
            return Ok(None);
        }
        let wrap = self.cursor.wrap_pending || gap;
        let (row, column) = if wrap {
            let row = if self.cursor.row == self.scroll_region.end - 1 {
                if self.history_capture {
                    let captured = self.history.write_scrolling(
                        &mut self.grid,
                        self.scroll_region.end,
                        text,
                        width,
                        style,
                        gap,
                    )?;
                    self.history_captured(captured);
                } else {
                    self.grid.write_scrolling(
                        self.scroll_region.clone(),
                        text,
                        width,
                        style,
                        gap,
                    )?;
                    self.scroll_graphics(self.scroll_region.start, 1, false);
                    self.disconnect_top(self.scroll_region.start);
                }
                self.cursor.row
            } else {
                let row = (self.cursor.row + 1).min(self.grid.rows() - 1);
                if INSERT {
                    self.grid.write_inserting(
                        row,
                        0,
                        text,
                        width,
                        style,
                        gap && row == self.cursor.row,
                    )?;
                } else {
                    self.grid.write(row, 0, text, width, style)?;
                }
                if gap
                    && (row != self.cursor.row || width_columns < columns)
                    && !(INSERT && row == self.cursor.row)
                {
                    self.grid
                        .clear_row(self.cursor.row, columns - 1..columns, style)
                        .expect("The last column is in bounds");
                }
                if row != self.cursor.row {
                    self.grid
                        .set_wrap_end(self.cursor.row, Some(columns - usize::from(gap)));
                }
                row
            };
            (row, 0)
        } else {
            if INSERT {
                self.grid.write_inserting(
                    self.cursor.row,
                    self.cursor.column,
                    text,
                    width,
                    style,
                    false,
                )?;
            } else {
                self.grid
                    .write(self.cursor.row, self.cursor.column, text, width, style)?;
            }
            (self.cursor.row, self.cursor.column)
        };
        self.advance(row, column + width_columns);
        Ok(Some(CellPosition { row, column }))
    }

    /// Writes printable ASCII in row-sized batches. The entire input is validated
    /// before mutation. Empty input preserves pending wrapping. History capture
    /// reserves all replacement rows before writing; every error preserves both
    /// buffers, history, and cursor. With capture disabled, this does not allocate.
    /// With automatic wrapping disabled, excess bytes overwrite the last column;
    /// the final byte supplies that cell without writing intermediate values.
    /// Insertion mode shifts the suffix once per row segment. This is equivalent
    /// to individual unit writes, including wide-unit repair and edge overwrites.
    /// Complete incoming rows replace recycled buffers directly when scrolling,
    /// releasing their owned text in one pass.
    #[inline]
    pub fn write_ascii(&mut self, bytes: &[u8], style: StyleId) -> Result<(), StorageError> {
        if bytes.iter().any(|&byte| !(b' '..=b'~').contains(&byte)) {
            return Err(StorageError::InvalidAscii);
        }
        if self.grid.has_sized_text() {
            return self.write_ascii_over_sized(bytes, style).map(|_| ());
        }
        if self.insert_mode {
            self.write_ascii_inserting(bytes, style)
        } else {
            self.write_ascii_mode::<false>(bytes, style)
        }
    }

    #[cold]
    pub(crate) fn write_ascii_over_sized(
        &mut self,
        bytes: &[u8],
        style: StyleId,
    ) -> Result<Option<CellPosition>, StorageError> {
        debug_assert!(bytes.iter().all(|byte| (b' '..=b'~').contains(byte)));
        let prepared = bytes.len() > 1 && self.prepare_ascii_text(bytes.len(), style)?;
        let result = self.write_ascii_around_blocks(bytes, style);
        if prepared {
            self.history.finish();
        }
        self.grid.refresh_sized_text();
        result
    }

    fn write_ascii_around_blocks(
        &mut self,
        mut bytes: &[u8],
        style: StyleId,
    ) -> Result<Option<CellPosition>, StorageError> {
        let columns = self.grid.columns();
        let mut last = None;
        while let Some((&first, tail)) = bytes.split_first() {
            let Some(cursor) = self.skip_sized_lower_rows(self.cursor, 1) else {
                return Ok(None);
            };
            if cursor.wrap_pending {
                last = self.write_unit(
                    char::from(first).encode_utf8(&mut [0; 4]),
                    CellWidth::Single,
                    style,
                )?;
                bytes = tail;
                continue;
            }
            let end = self
                .grid
                .row(cursor.row)
                .unwrap()
                .sized_text_rows_from(cursor.column)
                .find(|part| part.row_offset != 0 && part.column > cursor.column)
                .map_or(columns, |part| part.column);
            let fitting = (end - cursor.column).min(bytes.len());
            let count = if self.auto_wrap || end < columns {
                fitting
            } else {
                bytes.len()
            };
            self.cursor = cursor;
            if self.insert_mode {
                self.write_ascii_inserting(&bytes[..count], style)?;
            } else {
                self.write_ascii_mode::<false>(&bytes[..count], style)?;
            }
            last = Some(CellPosition {
                row: cursor.row,
                column: cursor.column + fitting - 1,
            });
            bytes = &bytes[count..];
        }
        Ok(last)
    }

    #[inline(never)]
    fn write_ascii_inserting(&mut self, bytes: &[u8], style: StyleId) -> Result<(), StorageError> {
        self.write_ascii_mode::<true>(bytes, style)
    }

    #[inline]
    fn write_ascii_mode<const INSERT: bool>(
        &mut self,
        bytes: &[u8],
        style: StyleId,
    ) -> Result<(), StorageError> {
        let available = self.grid.columns() - self.cursor.column;
        if !self.cursor.wrap_pending && bytes.len() <= available {
            if INSERT {
                self.grid
                    .insert_cells(self.cursor.row, self.cursor.column, bytes.len(), style)
                    .expect("The ASCII run fits");
            }
            self.grid
                .write_ascii_validated(self.cursor.row, self.cursor.column, bytes, style);
            self.advance(self.cursor.row, self.cursor.column + bytes.len());
            return Ok(());
        }
        if self.history_capture {
            self.write_ascii_with_history::<INSERT>(bytes, style)
        } else {
            self.write_ascii_at_boundary::<INSERT>(bytes, style)
        }
    }

    #[inline(never)]
    fn write_ascii_with_history<const INSERT: bool>(
        &mut self,
        bytes: &[u8],
        style: StyleId,
    ) -> Result<(), StorageError> {
        let prepared = self.prepare_ascii_text(bytes.len(), style)?;
        let result = self.write_ascii_at_boundary::<INSERT>(bytes, style);
        if prepared {
            self.history.finish();
        }
        result
    }

    // Share boundary handling while keeping fitting writes inline.
    #[inline(never)]
    fn write_ascii_at_boundary<const INSERT: bool>(
        &mut self,
        mut bytes: &[u8],
        style: StyleId,
    ) -> Result<(), StorageError> {
        if !self.auto_wrap && !bytes.is_empty() {
            let last = self.grid.columns() - 1;
            let prefix = bytes.len().min(last - self.cursor.column);
            if prefix != 0 {
                if INSERT {
                    self.grid
                        .insert_cells(self.cursor.row, self.cursor.column, prefix, style)
                        .expect("The ASCII prefix fits");
                }
                self.grid.write_ascii_validated(
                    self.cursor.row,
                    self.cursor.column,
                    &bytes[..prefix],
                    style,
                );
            }
            self.cursor.column += prefix;
            if prefix < bytes.len() {
                if INSERT {
                    self.grid
                        .insert_cells(self.cursor.row, last, 1, style)
                        .expect("The last column fits");
                }
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
                let source = self.cursor.row;
                if source == self.scroll_region.end - 1 {
                    let columns = self.grid.columns();
                    if !self.history_capture && bytes.len() >= columns {
                        self.grid.write_ascii_scrolling(
                            self.scroll_region.clone(),
                            &bytes[..columns],
                            style,
                        );
                        self.scroll_graphics(self.scroll_region.start, 1, false);
                        self.disconnect_top(self.scroll_region.start);
                        self.advance(source, columns);
                        bytes = &bytes[columns..];
                        continue;
                    }
                    if self.history_capture {
                        let captured = self.capture_history(style)?;
                        if self.scroll_region.end == 1 && captured {
                            self.history.connect_latest(columns);
                        }
                    } else {
                        self.grid
                            .scroll_up(self.scroll_region.clone(), 1, style)
                            .expect("The scrolling region is valid");
                        self.scroll_graphics(self.scroll_region.start, 1, false);
                        self.disconnect_top(self.scroll_region.start);
                    }
                    if self.scroll_region.len() > 1 {
                        self.grid.set_wrap_end(source - 1, Some(columns));
                    }
                } else {
                    self.cursor.row = (source + 1).min(self.grid.rows() - 1);
                    if self.cursor.row != source {
                        self.grid.set_wrap_end(source, Some(self.grid.columns()));
                    }
                }
                self.cursor.column = 0;
            }
            let length = bytes.len().min(self.grid.columns() - self.cursor.column);
            if INSERT {
                self.grid
                    .insert_cells(self.cursor.row, self.cursor.column, length, style)
                    .expect("The ASCII run fits");
            }
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
        self.cancel_pending_wrap();
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
        self.cancel_pending_wrap();
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
            self.cancel_pending_wrap();
        }
    }

    /// Scrolls the entire active region upward, discarding rows at its top and
    /// filling exposed bottom rows using `style`. Counts are clamped to the
    /// region's height; zero preserves all state. The cursor position, pending
    /// wrapping, and region boundaries are preserved, including when the cursor
    /// is outside the region. Retained rows keep their text and style; recycled
    /// row buffers are reused. Primary scrolling from row zero captures history
    /// within its configured limits, reserving replacements before mutation.
    /// Every error preserves both buffers, history, and cursor state.
    #[inline]
    pub fn scroll_up(&mut self, count: usize, style: StyleId) -> Result<(), StorageError> {
        if count == 0 {
            return Ok(());
        }
        if self.history_capture {
            let count = count.min(self.scroll_region.len());
            if count == 1 {
                return self.capture_history(style).map(|_| ());
            }
            let prepared = self.history.prepare(self.grid.columns(), count, style)?;
            let mut captured = 0;
            let result = (0..count).try_for_each(|_| {
                if self
                    .history
                    .capture(&mut self.grid, self.scroll_region.end, style)?
                {
                    captured += 1;
                } else {
                    self.scroll_captured_history(std::mem::take(&mut captured));
                    self.history_captured(false);
                }
                Ok(())
            });
            // Each capture adds at most one history row, so the source cutoff is monotonic.
            self.scroll_captured_history(captured);
            if prepared {
                self.history.finish();
            }
            result
        } else {
            self.grid
                .scroll_up(self.scroll_region.clone(), count, style)
                .expect("The scrolling region is valid");
            self.scroll_graphics(self.scroll_region.start, count, false);
            self.disconnect_top(self.scroll_region.start);
            Ok(())
        }
    }

    /// Scrolls the entire active region downward, discarding rows at its bottom
    /// and filling exposed top rows using `style`. Count, cursor, and region
    /// rules follow [`Self::scroll_up`]. This reuses buffers without allocating.
    #[inline]
    pub fn scroll_down(&mut self, count: usize, style: StyleId) {
        if count != 0 {
            self.disconnect_top(self.scroll_region.start);
        }
        self.grid
            .scroll_down(self.scroll_region.clone(), count, style)
            .expect("The scrolling region is valid");
        self.scroll_graphics(self.scroll_region.start, count, true);
    }

    pub(crate) fn unscroll(&mut self, count: usize, style: StyleId) -> Result<(), StorageError> {
        if count == 0 {
            return Ok(());
        }
        if self.active_buffer != ScreenBuffer::Primary
            || self.scroll_region.start != 0
            || self.history.is_empty()
        {
            self.scroll_down(count, style);
            return Ok(());
        }
        let count = count.min(self.grid.rows().max(self.history.len()));
        let restored = count.min(self.history.len());
        let blank = count - restored;
        self.history.restore(
            &mut self.grid,
            self.scroll_region.end,
            restored,
            blank,
            style,
        )?;
        self.grid.refresh_sized_text();
        self.display_offset = self.display_offset.saturating_sub(restored);
        self.shift_graphics_with_history(restored as i64);
        self.scroll_graphics(0, blank, true);
        Ok(())
    }

    /// Inserts blank rows at the cursor, shifting rows down through the scrolling
    /// region's bottom and discarding overflow. Counts are clamped to that suffix.
    /// Zero counts and cursors outside the region preserve all state. Effective
    /// edits cancel pending wrapping and preserve cursor coordinates and region
    /// boundaries. Recycled rows use `style`; retained rows keep their text and
    /// style. Cursor-anchored graphics retain their positions.
    /// Buffers are reused without allocating.
    pub fn insert_lines(&mut self, count: usize, style: StyleId) {
        if count == 0 || !self.scroll_region.contains(&self.cursor.row) {
            return;
        }
        self.disconnect_top(self.cursor.row);
        self.grid
            .scroll_down(self.cursor.row..self.scroll_region.end, count, style)
            .expect("The cursor suffix is a valid row range");
        self.cancel_pending_wrap();
    }

    /// Deletes rows at the cursor, shifting remaining rows up through the
    /// scrolling region's bottom and filling exposed rows using `style`.
    /// Count, cursor, region, and allocation rules follow [`Self::insert_lines`].
    pub fn delete_lines(&mut self, count: usize, style: StyleId) {
        if count == 0 || !self.scroll_region.contains(&self.cursor.row) {
            return;
        }
        self.disconnect_top(self.cursor.row);
        self.grid
            .scroll_up(self.cursor.row..self.scroll_region.end, count, style)
            .expect("The cursor suffix is a valid row range");
        self.cancel_pending_wrap();
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
            self.disconnect_top(self.cursor.row);
            self.grid
                .clear_rows(self.cursor.row..self.cursor.row + 1, style);
        } else {
            self.grid
                .clear_row(self.cursor.row, range, style)
                .expect("The cursor row and erasure range are in bounds");
        }
        self.cancel_pending_wrap();
    }

    /// Erases text in part or all of the physical display using `style`. The extent
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
                if self.cursor.row != 0 {
                    self.disconnect_top(0);
                }
                self.grid.clear_rows(0..self.cursor.row, style);
                self.erase_line(mode, style);
            }
            EraseMode::All => {
                self.disconnect_top(0);
                self.grid.clear(style);
            }
        }
        self.cancel_pending_wrap();
    }

    /// Moves to column zero and cancels pending wrapping.
    pub fn carriage_return(&mut self) {
        self.cursor.column = 0;
        self.cancel_pending_wrap();
    }

    /// Advances one row, scrolling at the region's bottom edge. Outside the
    /// region, movement stops at the physical bottom without scrolling. The
    /// column is preserved and pending wrapping is canceled. Exposed cells use `style`.
    /// Primary scrolling from row zero captures history. Allocation failures
    /// preserve both buffers, history, and cursor state.
    #[inline]
    pub fn line_feed(&mut self, style: StyleId) -> Result<(), StorageError> {
        if self.cursor.row == self.scroll_region.end - 1 {
            if self.history_capture {
                self.capture_history(style)?;
            } else {
                self.grid
                    .scroll_up(self.scroll_region.clone(), 1, style)
                    .expect("The scrolling region is valid");
                self.scroll_graphics(self.scroll_region.start, 1, false);
                self.disconnect_top(self.scroll_region.start);
            }
        } else if self.cursor.row + 1 < self.grid.rows() {
            self.grid.set_wrap_end(self.cursor.row, None);
            self.cursor.row += 1;
        }
        self.cancel_pending_wrap();
        Ok(())
    }

    /// Moves up one row, scrolling downward at the region's top edge. Outside
    /// the region, movement stops at the physical top without scrolling. The
    /// column is preserved and pending wrapping is canceled. Exposed cells use `style`.
    #[inline]
    pub fn reverse_index(&mut self, style: StyleId) {
        if self.cursor.row == self.scroll_region.start {
            self.disconnect_top(self.scroll_region.start);
            self.grid
                .scroll_down(self.scroll_region.clone(), 1, style)
                .expect("The scrolling region is valid");
            self.scroll_graphics(self.scroll_region.start, 1, true);
        } else {
            self.cursor.row = self.cursor.row.saturating_sub(1);
        }
        self.cancel_pending_wrap();
    }

    /// Moves one column left, stopping at column zero, and cancels pending wrapping.
    pub fn backspace(&mut self) {
        self.cursor.column = self.cursor.column.saturating_sub(1);
        self.cancel_pending_wrap();
    }

    /// Preserves both upper-left grid intersections and clamps cursor coordinates.
    /// A size change cancels pending wrapping; an unchanged size preserves it.
    /// Height changes reset the scrolling region to the full grid; width-only
    /// changes preserve it. Tab stops in the width intersection are preserved;
    /// cropped stops are discarded and added columns receive default stops.
    /// Height-only changes preserve stops. Every error preserves both buffers,
    /// cursor, region, and stops. Automatic wrapping and origin modes are preserved.
    /// The cursor remains in the active region while origin mode is enabled.
    /// Reserved tab capacity survives shrinking. The primary viewport keeps its
    /// top retained row; its window height follows the new grid height.
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
        let active = self.grid.prepare_resize(columns, rows, style)?;
        let inactive = self.inactive.grid.prepare_resize(columns, rows, style)?;
        self.grid.resize_prepared(active, style);
        self.inactive.grid.resize_prepared(inactive, style);
        self.tabs.resize_prepared(old_columns, columns);
        if old_columns != columns {
            self.history.disconnect();
        }
        if height_changed {
            self.scroll_region = 0..rows;
        }
        self.cursor.row = self.cursor.row.min(rows - 1);
        self.cursor.column = self.cursor.column.min(columns - 1);
        self.cancel_pending_wrap();
        self.update_history_capture();
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

    #[inline]
    fn cancel_pending_wrap(&mut self) {
        self.cursor.wrap_pending = false;
        self.sized_right_edge = None;
    }

    fn advance_sized(&mut self, row: usize, end: usize, block: &Arc<SizedBlock>) {
        self.advance(row, end);
        self.sized_right_edge = (end == self.grid.columns()).then(|| Arc::downgrade(block));
    }
}
