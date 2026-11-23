use std::{iter::FusedIterator, ops::Range};

use crate::{CellPosition, Cursor, Row, ScreenView, StorageError, TextRange};

/// A borrowed, screen-height window into chronological rows.
/// Coordinates are local to this window. Rows keep their captured widths;
/// consumers choose horizontal clipping. Construction, lookup, and range
/// validation use constant space without allocating. The source borrow fixes this window.
#[derive(Clone, Copy, Debug)]
pub struct Viewport<'a> {
    view: ScreenView<'a>,
    start: usize,
    height: usize,
    cursor: &'a Cursor,
}

impl<'a> Viewport<'a> {
    pub(crate) fn new(view: ScreenView<'a>, offset: usize, cursor: &'a Cursor) -> Self {
        let history = view.history_rows();
        let height = view.rows() - history;
        Self {
            view,
            start: history - offset,
            height,
            cursor,
        }
    }

    /// Rows above the live buffer. Zero follows the live buffer.
    pub fn offset(&self) -> usize {
        self.view.history_rows() - self.start
    }

    /// Row range in the complete [`ScreenView`].
    pub fn range(&self) -> Range<usize> {
        self.start..self.start + self.height
    }

    pub fn source(&self) -> ScreenView<'a> {
        self.view
    }

    pub fn rows(&self) -> usize {
        self.height
    }

    pub fn row(&self, index: usize) -> Option<&'a Row> {
        if index < self.height {
            self.view.row(self.start + index)
        } else {
            None
        }
    }

    pub fn iter_rows(
        &self,
    ) -> impl ExactSizeIterator<Item = &'a Row> + DoubleEndedIterator + FusedIterator + use<'a>
    {
        self.view.rows_in(self.range())
    }

    /// Visits viewport rows and, for virtual parents, the remaining live rows.
    pub fn rows_for_graphics(
        &self,
        virtual_parents: bool,
    ) -> impl Iterator<Item = &'a Row> + use<'a> {
        let hidden = if virtual_parents {
            self.offset().min(self.height)
        } else {
            0
        };
        self.iter_rows()
            .chain(self.view.iter_live_rows().skip(self.height - hidden))
    }

    /// Cursor coordinates when its physical row lies in this window. Appearance,
    /// including visibility mode, remains available from [`crate::Screen::cursor_appearance`].
    pub fn cursor(&self) -> Option<Cursor> {
        let row = self.offset() + self.cursor.row;
        (row < self.height).then_some(Cursor {
            row,
            ..*self.cursor
        })
    }

    /// Borrows a half-open text range using window-local row coordinates.
    /// Endpoint rows must lie inside this window; each column uses its row's width.
    /// Complete-unit, soft-wrap, and sink-failure rules follow [`ScreenView::text`].
    pub fn text(&self, mut range: Range<CellPosition>) -> Result<TextRange<'a>, StorageError> {
        if range.start.row >= self.height || range.end.row >= self.height {
            return Err(StorageError::OutOfBounds);
        }
        range.start.row += self.start;
        range.end.row += self.start;
        self.view.text(range)
    }

    /// Contracts a selection to the first and last occupied cells in the viewport.
    pub fn selection_bounds(&self, range: Range<CellPosition>) -> Option<Range<CellPosition>> {
        let mut bounds = self.selection_text(range).ok()?.content_bounds()?;
        bounds.start.row -= self.start;
        bounds.end.row -= self.start;
        Some(bounds)
    }

    /// Copies each selected row without unwritten padding outside its text.
    pub fn selection_text(
        &self,
        range: Range<CellPosition>,
    ) -> Result<TextRange<'a, true>, StorageError> {
        self.text(range).map(TextRange::trim_erased)
    }
}
