use std::{iter::FusedIterator, ops::Range};

use crate::{CellPosition, Row, ScreenView, TextRange};

impl<'a> ScreenView<'a> {
    /// Groups retained rows connected by automatic wrapping. Hard row breaks
    /// separate groups. The oldest group may be a fragment after history eviction.
    /// Construction uses constant space; traversal visits each physical row once
    /// without allocating, including when alternating traversal directions.
    #[inline]
    pub fn logical_lines(&self) -> LogicalLines<'a> {
        LogicalLines {
            view: *self,
            remaining: 0..self.rows(),
        }
    }

    /// Finds the retained logical line containing a chronological row index.
    /// Returns `None` outside this view. Work is proportional to the number of
    /// physical rows in the returned line; auxiliary space is constant.
    pub fn logical_line(&self, row: usize) -> Option<LogicalLine<'a>> {
        (row < self.rows()).then(|| LogicalLine {
            view: *self,
            start: line_start(*self, 0, row),
            end: line_end(*self, row + 1, self.rows()),
        })
    }
}

/// A borrowed, nonempty group of retained rows connected by automatic wrapping.
/// Coordinates use the source [`ScreenView`]. View edges bound the group even
/// when history eviction has removed its original prefix. Captured row widths,
/// cell styles, erased state, and owned text remain available through its rows.
#[derive(Clone, Copy, Debug)]
pub struct LogicalLine<'a> {
    view: ScreenView<'a>,
    start: usize,
    end: usize,
}

impl<'a> LogicalLine<'a> {
    /// Exclusive physical row range in the source view.
    #[inline]
    pub fn range(&self) -> Range<usize> {
        self.start..self.end
    }

    pub fn iter_rows(
        &self,
    ) -> impl ExactSizeIterator<Item = &'a Row> + DoubleEndedIterator + FusedIterator + use<'a>
    {
        self.view
            .iter_rows()
            .skip(self.start)
            .take(self.end - self.start)
    }

    /// Borrows all retained text in this line. Formatting omits soft-wrap gaps,
    /// preserves spaces, and emits no final newline. Output sink failure and
    /// complete-unit rules follow [`ScreenView::text`].
    pub fn text(&self) -> TextRange<'a> {
        let last = self.end - 1;
        self.view
            .text(
                CellPosition {
                    row: self.start,
                    column: 0,
                }..CellPosition {
                    row: last,
                    column: self
                        .view
                        .row(last)
                        .expect("A line row exists")
                        .cells()
                        .len(),
                },
            )
            .expect("A logical line has valid text boundaries")
    }
}

/// Double-ended traversal of retained logical lines. Each physical row is
/// examined at most once. The upper size hint counts remaining physical rows;
/// the exact number of logical lines requires traversing their boundaries.
#[derive(Clone, Debug)]
pub struct LogicalLines<'a> {
    view: ScreenView<'a>,
    remaining: Range<usize>,
}

impl<'a> Iterator for LogicalLines<'a> {
    type Item = LogicalLine<'a>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let start = self.remaining.start;
        // Keep single-row groups inline and scan longer groups separately.
        let end = if start + 1 == self.remaining.end
            || self
                .view
                .row(start)
                .expect("A line row exists")
                .wrap_end()
                .is_none()
        {
            start + 1
        } else {
            line_end(self.view, start + 2, self.remaining.end)
        };
        self.remaining.start = end;
        Some(LogicalLine {
            view: self.view,
            start,
            end,
        })
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let rows = self.remaining.len();
        (usize::from(rows != 0), Some(rows))
    }
}

impl DoubleEndedIterator for LogicalLines<'_> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let end = self.remaining.end;
        let start = if end - 1 == self.remaining.start
            || self
                .view
                .row(end - 2)
                .expect("A line row exists")
                .wrap_end()
                .is_none()
        {
            end - 1
        } else {
            line_start(self.view, self.remaining.start, end - 2)
        };
        self.remaining.end = start;
        Some(LogicalLine {
            view: self.view,
            start,
            end,
        })
    }
}

impl FusedIterator for LogicalLines<'_> {}

fn line_start(view: ScreenView<'_>, lower: usize, mut row: usize) -> usize {
    while row > lower
        && view
            .row(row - 1)
            .expect("A line row exists")
            .wrap_end()
            .is_some()
    {
        row -= 1;
    }
    row
}

fn line_end(view: ScreenView<'_>, mut end: usize, upper: usize) -> usize {
    while end < upper
        && view
            .row(end - 1)
            .expect("A line row exists")
            .wrap_end()
            .is_some()
    {
        end += 1;
    }
    end
}
