use std::{collections::VecDeque, ops::Range};

use super::{Cursor, SavedCursor, Screen, ScreenBuffer};
use crate::grid::validate_dimensions;
use crate::{
    Cell, CellPosition, Grid, ReflowAffinity, Row, RowHistory, ScreenView, StorageError, StyleId,
};

struct Layout {
    window: Range<usize>,
    cursor: Cursor,
    saved: Cursor,
    anchor: Option<usize>,
}

fn resolve(saved: SavedCursor, region: Range<usize>, columns: usize, rows: usize) -> Cursor {
    let (top, height) = if saved.origin_mode {
        (region.start, region.len())
    } else {
        (0, rows)
    };
    Cursor {
        row: top + saved.row.min(height - 1),
        column: saved.column.min(columns - 1),
        wrap_pending: saved.wrap_pending
            && saved.auto_wrap
            && saved.row < height
            && saved.column == columns - 1,
    }
}

fn position(cursor: Cursor, history: usize) -> CellPosition {
    CellPosition {
        row: history + cursor.row,
        column: cursor.column + usize::from(cursor.wrap_pending),
    }
}

fn placed(position: CellPosition, columns: usize) -> Cursor {
    let wrap_pending = position.column == columns;
    Cursor {
        row: position.row,
        column: position.column - usize::from(wrap_pending),
        wrap_pending,
    }
}

fn clipped(mut cursor: Cursor, window: Range<usize>) -> Cursor {
    if !window.contains(&cursor.row) {
        cursor.wrap_pending = false;
    }
    cursor.row = cursor
        .row
        .saturating_sub(window.start)
        .min(window.len() - 1);
    cursor
}

fn layout(
    view: ScreenView<'_>,
    columns: usize,
    height: usize,
    pivot: Cursor,
    saved: Cursor,
    offset: usize,
) -> Result<Layout, StorageError> {
    let history = view.history_rows();
    let sources = [
        Some(position(pivot, history)),
        Some(position(saved, history)),
        (offset != 0).then_some(CellPosition {
            row: history - offset,
            column: 0,
        }),
    ];
    let affinities = [
        if pivot.wrap_pending {
            ReflowAffinity::Before
        } else {
            ReflowAffinity::After
        },
        if saved.wrap_pending {
            ReflowAffinity::Before
        } else {
            ReflowAffinity::After
        },
        ReflowAffinity::After,
    ];
    let mut mapped = [None; 3];
    let mut count = 0_usize;
    for line in view.logical_lines() {
        let mut anchors = [None; 3];
        for index in 0..3 {
            if let Some(source) = sources[index]
                && line.range().contains(&source.row)
            {
                anchors[index] = Some(line.anchor(source)?);
            }
        }
        for row in line.reflow(columns)? {
            for index in 0..3 {
                if mapped[index].is_none()
                    && let Some(anchor) = anchors[index]
                    && let Some(column) = row.map_anchor(anchor)?
                {
                    if affinities[index] == ReflowAffinity::After
                        && row.wrap_end().is_some()
                        && column == row.used_columns()
                    {
                        continue;
                    }
                    mapped[index] = Some(CellPosition { row: count, column });
                }
            }
            count = count.checked_add(1).ok_or(StorageError::CapacityExceeded)?;
        }
    }
    let cursor = placed(
        mapped[0].expect("The retained primary cursor maps"),
        columns,
    );
    let saved = placed(mapped[1].expect("The retained saved cursor maps"), columns);
    let start = count.saturating_sub(height).min(cursor.row);
    Ok(Layout {
        window: start..start + height,
        cursor: clipped(cursor, start..start + height),
        saved: clipped(saved, start..start + height),
        anchor: mapped[2].map(|point| point.row),
    })
}

impl Screen {
    /// Resizes both buffers, reflowing chronological primary rows and preserving
    /// the alternate buffer's upper-left intersection. Unchanged dimensions are
    /// allocation-free. Source text sinks require a boundary before resizing.
    /// A one-column target rejects double-width primary units before allocation.
    ///
    /// The primary window is the latest window containing its mapped cursor;
    /// while alternate storage is active, the primary save slot selects that
    /// window. Rows beyond its bottom are cropped and missing rows are padded.
    /// Current and saved positions map or clamp into the new live dimensions.
    /// History retains the adjacent suffix within its row and byte budgets;
    /// an oversized row clears its preceding suffix. Expired viewport anchors
    /// clamp to the oldest retained row; output following stays enabled at zero.
    ///
    /// Commit resets the scrolling region to the whole screen, preserves modes,
    /// appearance and overlapping tab stops, and initializes added stops. All
    /// allocations precede commit. Errors preserve both buffers, history, cursor
    /// slots, viewport, tabs and modes, including their existing allocations.
    pub fn reflow(
        &mut self,
        columns: usize,
        rows: usize,
        style: StyleId,
    ) -> Result<(), StorageError> {
        if columns == self.grid.columns() && rows == self.grid.rows() {
            return Ok(());
        }
        validate_dimensions(columns, rows)?;
        let primary_active = self.active_buffer == ScreenBuffer::Primary;
        let primary = self.buffer(ScreenBuffer::Primary);
        let primary_slot = if primary_active {
            self.saved_cursor
        } else {
            self.inactive.saved_cursor
        };
        let alternate_slot = if primary_active {
            self.inactive.saved_cursor
        } else {
            self.saved_cursor
        };
        let saved = resolve(
            primary_slot,
            self.scroll_region.clone(),
            primary.columns(),
            primary.rows(),
        );
        let pivot = if primary_active { self.cursor } else { saved };
        let view = ScreenView::new(primary, Some(&self.history));
        let plan = layout(view, columns, rows, pivot, saved, self.display_offset)?;
        let limits = self.history.limits();
        let mut history = RowHistory::default();
        history.set_limits(limits)?;
        let skip = if limits.max_bytes / size_of::<Cell>() < columns {
            plan.window.start
        } else {
            plan.window.start.saturating_sub(limits.max_rows)
        };
        let mut storage = VecDeque::new();
        storage.try_reserve_exact(rows)?;
        let mut index = 0;
        let mut offset = 0;
        'copy: for line in view.logical_lines() {
            for row in line.reflow(columns)? {
                if index == plan.window.end {
                    break 'copy;
                }
                if index >= skip {
                    let owned = row.materialize(style)?;
                    if index < plan.window.start {
                        let kept = history.append_reflow(owned)?;
                        if let Some(anchor) = plan.anchor
                            && index >= anchor
                        {
                            offset = if index == anchor && kept {
                                1
                            } else if kept {
                                (offset + 1).min(history.len())
                            } else {
                                0
                            };
                        }
                    } else {
                        storage.push_back(owned);
                    }
                }
                index += 1;
            }
        }
        while storage.len() < rows {
            storage.push_back(Row::new(columns, style)?);
        }
        let primary = Grid::from_rows(columns, storage);
        let alternate = self
            .buffer(ScreenBuffer::Alternate)
            .resized_copy(columns, rows, style)?;
        let tabs = if columns == self.grid.columns() {
            None
        } else {
            Some(self.tabs.resized_copy(self.grid.columns(), columns)?)
        };
        let alt_saved = resolve(
            alternate_slot,
            self.scroll_region.clone(),
            self.grid.columns(),
            self.grid.rows(),
        );
        let alt_saved = Cursor {
            row: alt_saved.row.min(rows - 1),
            column: alt_saved.column.min(columns - 1),
            wrap_pending: alt_saved.wrap_pending
                && alt_saved.row < rows
                && alt_saved.column == columns - 1,
        };
        let primary_slot = SavedCursor {
            row: plan.saved.row,
            column: plan.saved.column,
            wrap_pending: plan.saved.wrap_pending,
            ..primary_slot
        };
        let alternate_slot = SavedCursor {
            row: alt_saved.row,
            column: alt_saved.column,
            wrap_pending: alt_saved.wrap_pending,
            ..alternate_slot
        };
        let alternate_cursor = Cursor {
            row: self.cursor.row.min(rows - 1),
            column: self.cursor.column.min(columns - 1),
            wrap_pending: false,
        };
        // Prepared state is fully owned; every operation below is infallible.
        if primary_active {
            self.grid = primary;
            self.inactive.grid = alternate;
            self.saved_cursor = primary_slot;
            self.inactive.saved_cursor = alternate_slot;
            self.cursor = plan.cursor;
        } else {
            self.grid = alternate;
            self.inactive.grid = primary;
            self.saved_cursor = alternate_slot;
            self.inactive.saved_cursor = primary_slot;
            self.cursor = alternate_cursor;
        }
        self.history = history;
        self.display_offset = offset.min(self.history.len());
        self.scroll_region = 0..rows;
        if let Some(tabs) = tabs {
            self.tabs = tabs;
        }
        self.update_history_capture();
        Ok(())
    }
}
