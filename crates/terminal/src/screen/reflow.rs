use std::{collections::VecDeque, ops::Range};

use super::{Cursor, SavedCursor, Screen, ScreenBuffer};
use crate::graphics::GraphicsAnchor;
use crate::grid::validate_dimensions;
use crate::reflow::normalize_position;
use crate::row::RowTransfer;
use crate::{
    Cell, CellPosition, Grid, ReflowAffinity, ReflowRows, Row, RowHistory, ScreenView,
    StorageError, StyleId,
};

struct Checkpoint<'a> {
    index: usize,
    source_row: usize,
    rows: ReflowRows<'a>,
}

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

fn layout<'a>(
    view: ScreenView<'a>,
    (columns, height): (usize, usize),
    pivot: Cursor,
    saved: Cursor,
    offset: usize,
    retained_rows: usize,
    image_anchors: &mut [GraphicsAnchor],
) -> Result<(Layout, Checkpoint<'a>), StorageError> {
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
    let mut anchors = [None; 3];
    for (index, source) in sources.into_iter().enumerate() {
        if let Some(source) = source {
            anchors[index] = Some((normalize_position(view, source)?, index));
        }
    }
    // Resolve preceding-row anchors first when several anchors share a boundary.
    anchors.sort_unstable_by_key(|anchor| {
        (
            anchor.is_none(),
            anchor.map(|(source, index)| (source, affinities[index] == ReflowAffinity::After)),
        )
    });
    let mut next_anchor = 0;
    let mut mapped = [None; 3];
    let mut next_image = 0;
    let mut count = 0_usize;
    let stride = retained_rows + height;
    let interval = stride.div_ceil(2);
    let mut checkpoint_at = 0;
    let mut checkpoints = [None, None, None];
    let mut slot = 0;
    let mut replay = None;
    for line in view.logical_lines() {
        let mut rows = line.reflow(columns)?;
        while let Some(row) = rows.next() {
            if mapped[0].is_none() && count == checkpoint_at {
                checkpoints[slot] = Some(Checkpoint {
                    index: count,
                    source_row: row.source_range().start.row,
                    rows: rows.replay_from(row),
                });
                slot = (slot + 1) % checkpoints.len();
                checkpoint_at = count.saturating_add(interval);
            }
            while let Some(Some((source, index))) = anchors.get(next_anchor) {
                let Some(column) = row.map_normalized_position(*source) else {
                    break;
                };
                if affinities[*index] == ReflowAffinity::After
                    && row.wrap_end().is_some()
                    && column == row.used_columns()
                {
                    break;
                }
                mapped[*index] = Some(CellPosition { row: count, column });
                next_anchor += 1;
            }
            if next_image < image_anchors.len() {
                let mut previous = row.source_range().start;
                let mut column = 0;
                let wrap_end = row.wrap_end();
                while let Some(anchor) = image_anchors.get_mut(next_image) {
                    if previous != anchor.source {
                        let Some(distance) = row.map_position_from(previous, anchor.source) else {
                            break;
                        };
                        column += distance;
                    }
                    if wrap_end == Some(column) {
                        break;
                    }
                    anchor.destination = Some(CellPosition { row: count, column });
                    previous = anchor.source;
                    next_image += 1;
                }
            }
            if replay.is_none() && mapped[0].is_some() {
                // The selected window starts no earlier than pivot - height + 1.
                // Three half-stride checkpoints bracket its candidate history.
                let first = count.saturating_sub(stride - 1);
                replay = checkpoints
                    .iter_mut()
                    .filter(|entry| entry.as_ref().is_some_and(|entry| entry.index <= first))
                    .max_by_key(|entry| entry.as_ref().unwrap().index)
                    .and_then(Option::take);
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
    let end = start + height;
    // Destinations are ordered by source, so only the tail can extend past the window.
    for anchor in image_anchors.iter_mut().rev() {
        let Some(destination) = &mut anchor.destination else {
            continue;
        };
        if anchor.source.row == view.rows() {
            destination.row = count
                .min(end)
                .saturating_add(destination.row)
                .min(i64::MAX as usize);
        } else if destination.row >= end {
            anchor.destination = None;
        } else {
            break;
        }
    }
    Ok((
        Layout {
            window: start..end,
            cursor: clipped(cursor, start..end),
            saved: clipped(saved, start..end),
            anchor: mapped[2].map(|point| point.row),
        },
        replay.expect("The primary cursor selects a replay checkpoint"),
    ))
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
    /// Layout keeps three borrowed iterator checkpoints. Row preparation resumes
    /// near the candidate history suffix, bounding prefix replay by the retained
    /// row limit and target height without allocating checkpoint storage.
    ///
    /// Commit resets the scrolling region to the whole screen, preserves modes,
    /// appearance and overlapping tab stops, and initializes added stops.
    /// Preparation reserves cell headers and extended-text slots. Sparse slots
    /// are reserved once per destination row across source segments. Commit moves
    /// existing text allocations into retained rows; history charges their full
    /// capacity. All allocations precede commit. Errors preserve both buffers,
    /// history, cursor slots, viewport, tabs and modes, including their allocations.
    pub fn reflow(
        &mut self,
        columns: usize,
        rows: usize,
        style: StyleId,
    ) -> Result<(), StorageError> {
        self.reflow_with(columns, rows, style, || Ok(()))
    }

    /// Runs `before_commit` after preparation and before changing the screen.
    /// An error leaves the screen untouched. The callback runs for unchanged sizes too.
    pub fn reflow_with<E: From<StorageError>>(
        &mut self,
        columns: usize,
        rows: usize,
        style: StyleId,
        before_commit: impl FnOnce() -> Result<(), E>,
    ) -> Result<(), E> {
        if columns == self.grid.columns() && rows == self.grid.rows() {
            return before_commit();
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
        let limits = self.history.limits();
        let retained_rows = if limits.max_bytes / size_of::<Cell>() < columns {
            0
        } else {
            limits.max_rows
        };
        let primary_graphics = if primary_active {
            &self.graphics
        } else {
            &self.inactive.graphics
        };
        let mut graphics_anchors = primary_graphics.reflow_anchors(view, primary.columns())?;
        let (plan, checkpoint) = layout(
            view,
            (columns, rows),
            pivot,
            saved,
            self.display_offset,
            retained_rows,
            &mut graphics_anchors,
        )?;
        let mut history = RowHistory::default();
        history.set_limits(limits)?;
        let skip = plan.window.start.saturating_sub(retained_rows);
        let mut storage = VecDeque::new();
        storage
            .try_reserve_exact(rows)
            .map_err(StorageError::from)?;
        let mut transfers: VecDeque<RowTransfer> = VecDeque::new();
        debug_assert!(checkpoint.index <= skip);
        let mut index = checkpoint.index;
        let source_row = checkpoint.source_row;
        let mut replay = Some(checkpoint.rows);
        let mut offset = 0;
        'copy: for line in view
            .logical_lines()
            .skip_while(|line| line.range().end <= source_row)
        {
            let rows = match replay.take() {
                Some(rows) => rows,
                None => line.reflow(columns)?,
            };
            for row in rows {
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
                        transfers.try_reserve(1).map_err(StorageError::from)?;
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
        before_commit()?;
        let graphics_cell = self.graphics_cell_extent();
        let (primary_graphics, alternate_graphics) = if primary_active {
            (&mut self.graphics, &mut self.inactive.graphics)
        } else {
            (&mut self.inactive.graphics, &mut self.graphics)
        };
        primary_graphics.commit_reflow(
            graphics_anchors,
            plan.window.clone(),
            history.len(),
            columns,
            graphics_cell,
        );
        alternate_graphics.crop_alternate(columns, rows);
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
        self.history.commit_reflow(history);
        self.display_offset = offset.min(self.history.len());
        self.scroll_region = 0..rows;
        if let Some(tabs) = tabs {
            self.tabs = tabs;
        }
        self.update_history_capture();
        Ok(())
    }
}
