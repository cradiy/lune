use std::{collections::VecDeque, ops::Range};

use super::{Cursor, SavedCursor, Screen, ScreenBuffer};
use crate::grid::validate_dimensions;
use crate::row::RowTransfer;
use crate::{
    Cell, CellPosition, Grid, ReflowAffinity, Row, RowHistory, ScreenView, StorageError, StyleId,
};

struct Layout {
    window: Range<usize>,
    cursor: Cursor,
    saved: Cursor,
    anchor: Option<usize>,
}

enum Source<'a> {
    Primary(&'a mut RowHistory, &'a mut Grid),
    Alternate(&'a mut Grid),
}

impl Source<'_> {
    fn row(&mut self, index: usize) -> &mut Row {
        match self {
            Self::Primary(history, grid) => {
                let count = history.len();
                if index < count {
                    history
                        .row_mut(index)
                        .expect("A retained source row exists")
                } else {
                    grid.row_mut(index - count)
                        .expect("A live source row exists")
                }
            }
            Self::Alternate(grid) => grid.row_mut(index).expect("An alternate source row exists"),
        }
    }
}

fn transfer_text(target: &mut Row, source: &mut Source<'_>, transfer: RowTransfer) {
    let mut column = 0;
    for index in transfer.source.start.row..=transfer.source.end.row {
        let row = source.row(index);
        let start = if index == transfer.source.start.row {
            transfer.source.start.column
        } else {
            0
        };
        let end = if index == transfer.source.end.row {
            transfer.source.end.column
        } else {
            row.wrap_end().unwrap_or(row.cells().len())
        };
        if start < end {
            target.transfer_segment(column, row, start..end);
            column += end - start;
        }
    }
    debug_assert_eq!(target.retained_bytes(), transfer.bytes);
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
    /// appearance and overlapping tab stops, and initializes added stops.
    /// Preparation reserves cell headers and extended-text slots. Commit moves
    /// existing text allocations into retained rows; history charges their full
    /// capacity. All allocations precede commit. Errors preserve both buffers,
    /// history, cursor slots, viewport, tabs and modes, including their allocations.
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
        let mut transfers: VecDeque<RowTransfer> = VecDeque::new();
        let mut index = 0;
        let mut offset = 0;
        'copy: for line in view.logical_lines() {
            for row in line.reflow(columns)? {
                if index == plan.window.end {
                    break 'copy;
                }
                if index >= skip {
                    let (owned, bytes, has_text) =
                        row.prepare_transfer(style)?.finish_preparation();
                    let mut retained = true;
                    if index < plan.window.start {
                        let kept = history.append_reflow(owned, bytes)?;
                        retained = kept;
                        let first = index + 1 - history.len();
                        while transfers.front().is_some_and(|entry| entry.target < first) {
                            transfers.pop_front();
                        }
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
                    if retained && has_text {
                        transfers.try_reserve(1)?;
                        transfers.push_back(RowTransfer {
                            target: index,
                            source: row.source_range(),
                            bytes,
                        });
                    }
                }
                index += 1;
            }
        }
        while storage.len() < rows {
            storage.push_back(Row::new(columns, style)?);
        }
        let mut primary = Grid::from_rows(columns, storage);
        let (mut alternate, alternate_transfers) = self
            .buffer(ScreenBuffer::Alternate)
            .prepare_resized(columns, rows, style)?;
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
        // Every allocation is complete. Move source payloads before publishing state.
        {
            let old_primary = if primary_active {
                &mut self.grid
            } else {
                &mut self.inactive.grid
            };
            let mut source = Source::Primary(&mut self.history, old_primary);
            let first = plan.window.start - history.len();
            for transfer in transfers {
                let target = if transfer.target < plan.window.start {
                    history
                        .row_mut(transfer.target - first)
                        .expect("A prepared history row exists")
                } else {
                    primary
                        .row_mut(transfer.target - plan.window.start)
                        .expect("A prepared primary row exists")
                };
                transfer_text(target, &mut source, transfer);
            }
        }
        {
            let old_alternate = if primary_active {
                &mut self.inactive.grid
            } else {
                &mut self.grid
            };
            let mut source = Source::Alternate(old_alternate);
            for transfer in alternate_transfers {
                let target = alternate
                    .row_mut(transfer.target)
                    .expect("A prepared alternate row exists");
                transfer_text(target, &mut source, transfer);
            }
        }
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
