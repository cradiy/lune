use std::ops::Range;

use lune_terminal::{
    CellPosition, CellText, CellWidth, Grid, MAX_CELL_TEXT_BYTES, StorageError, StyleId,
};

#[derive(Clone, Debug)]
struct Unit {
    text: String,
    width: usize,
    style: StyleId,
    erased: bool,
}

fn blank(style: StyleId) -> Unit {
    Unit {
        text: " ".into(),
        width: 1,
        style,
        erased: true,
    }
}

/// Stores complete display units; continuation cells and sparse indices are
/// derived only when comparing with the grid.
pub struct Model {
    columns: usize,
    rows: Vec<Vec<Unit>>,
    wraps: Vec<Option<usize>>,
}

impl Model {
    pub fn new(columns: usize, rows: usize) -> Self {
        Self {
            columns,
            rows: vec![vec![blank(StyleId(0)); columns]; rows],
            wraps: vec![None; rows],
        }
    }

    pub fn columns(&self) -> usize {
        self.columns
    }

    pub fn rows(&self) -> usize {
        self.rows.len()
    }

    #[allow(dead_code, reason = "The screen target checks text extraction.")]
    pub fn text(&self, range: Range<CellPosition>) -> Result<String, StorageError> {
        for point in [range.start, range.end] {
            if point.row >= self.rows.len() || point.column > self.columns {
                return Err(StorageError::OutOfBounds);
            }
        }
        if (range.start.row, range.start.column) > (range.end.row, range.end.column) {
            return Err(StorageError::OutOfBounds);
        }
        let mut result = String::new();
        for row in range.start.row..=range.end.row {
            let start = if row == range.start.row {
                range.start.column
            } else {
                0
            };
            let end = if row == range.end.row {
                range.end.column
            } else {
                self.columns
            };
            let mut column = 0;
            for unit in &self.rows[row] {
                let next = column + unit.width;
                if start < end
                    && column < end
                    && next > start
                    && column < self.wraps[row].unwrap_or(self.columns)
                {
                    result.push_str(&unit.text);
                }
                column = next;
            }
            if row < range.end.row && self.wraps[row].is_none() {
                result.push('\n');
            }
        }
        Ok(result)
    }

    pub fn check(&self, grid: &Grid) {
        self.check_styles(grid, |actual, expected| assert_eq!(actual, expected));
    }

    pub fn check_styles(&self, grid: &Grid, mut check: impl FnMut(StyleId, StyleId)) {
        assert_eq!(grid.columns(), self.columns);
        assert_eq!(grid.rows(), self.rows.len());
        for (row, (expected, actual)) in self.rows.iter().zip(grid.iter_rows()).enumerate() {
            assert_eq!(actual.wrap_end(), self.wraps[row]);
            assert_eq!(actual.cells().len(), self.columns);
            let mut column = 0;
            for unit in expected {
                let cell = actual.cells()[column];
                let first = unit.text.chars().next().unwrap();
                let extended = first.len_utf8() != unit.text.len();
                assert_eq!(cell.width(), unit.width);
                assert_eq!(cell.is_erased(), unit.erased);
                check(cell.style(), unit.style);
                assert_eq!(cell.character(), Some(first));
                assert_eq!(cell.has_extended_text(), extended);
                let text = if extended {
                    CellText::Extended(&unit.text)
                } else {
                    CellText::Scalar(first)
                };
                assert_eq!(actual.text(column), Some(text));
                if unit.width == 2 {
                    let tail = actual.cells()[column + 1];
                    assert_eq!(tail.width(), 0);
                    assert!(!tail.is_erased());
                    check(tail.style(), unit.style);
                    assert_eq!(tail.character(), None);
                    assert!(!tail.has_extended_text());
                    assert_eq!(actual.text(column + 1), Some(CellText::Continuation));
                }
                column += unit.width;
            }
            assert_eq!(column, self.columns);
        }
    }

    #[allow(dead_code, reason = "Only the screen oracle establishes soft wraps.")]
    pub fn set_wrap(&mut self, row: usize, end: Option<usize>) {
        self.wraps[row] = end;
    }

    pub fn write(
        &mut self,
        row: usize,
        column: usize,
        text: &str,
        width: CellWidth,
        style: StyleId,
    ) -> Result<(), StorageError> {
        let width = if width == CellWidth::Single { 1 } else { 2 };
        let range = self.bounds(row, column, width)?;
        if text.is_empty() {
            return Err(StorageError::EmptyText);
        }
        if text.len() > MAX_CELL_TEXT_BYTES {
            return Err(StorageError::TextTooLong);
        }
        self.erase(row, range, style);
        let index = index_at(&self.rows[row], column);
        self.rows[row].splice(
            index..index + width,
            [Unit {
                text: text.into(),
                width,
                style,
                erased: false,
            }],
        );
        Ok(())
    }

    pub fn ascii(
        &mut self,
        row: usize,
        column: usize,
        bytes: &[u8],
        style: StyleId,
    ) -> Result<(), StorageError> {
        let range = self.bounds(row, column, bytes.len())?;
        if bytes.iter().any(|&byte| !(b' '..=b'~').contains(&byte)) {
            return Err(StorageError::InvalidAscii);
        }
        if !bytes.is_empty() {
            self.erase(row, range, style);
            let index = index_at(&self.rows[row], column);
            self.rows[row].splice(
                index..index + bytes.len(),
                bytes.iter().map(|&byte| Unit {
                    text: char::from(byte).to_string(),
                    width: 1,
                    style,
                    erased: false,
                }),
            );
        }
        Ok(())
    }

    pub fn append(&mut self, row: usize, column: usize, text: &str) -> Result<(), StorageError> {
        self.bounds(row, column, 1)?;
        let index = index_at(&self.rows[row], column);
        let unit = &mut self.rows[row][index];
        if unit.text.len() + text.len() > MAX_CELL_TEXT_BYTES {
            return Err(StorageError::TextTooLong);
        }
        unit.text.push_str(text);
        if !text.is_empty() {
            unit.erased = false;
        }
        if !text.is_empty() && self.wraps[row].is_some_and(|end| column >= end) {
            self.wraps[row] = None;
        }
        Ok(())
    }

    pub fn clear_row(
        &mut self,
        row: usize,
        range: Range<usize>,
        style: StyleId,
    ) -> Result<(), StorageError> {
        if row >= self.rows.len() || range.start > range.end || range.end > self.columns {
            return Err(StorageError::OutOfBounds);
        }
        if range == (0..self.columns) && row > 0 {
            self.wraps[row - 1] = None;
        }
        self.erase(row, range, style);
        Ok(())
    }

    pub fn clear(&mut self, style: StyleId) {
        self.rows.fill(vec![blank(style); self.columns]);
        self.wraps.fill(None);
    }

    pub fn resize(
        &mut self,
        columns: usize,
        rows: usize,
        style: StyleId,
    ) -> Result<(), StorageError> {
        if columns == 0 || rows == 0 {
            return Err(StorageError::InvalidDimensions);
        }
        // Screen inputs include bitmap word boundaries through 129 columns;
        // storage inputs stay within 16 columns. Both bound height at 16.
        if columns > 129 || rows > 16 {
            return Err(StorageError::CapacityExceeded);
        }
        if columns != self.columns {
            self.wraps.fill(None);
        }
        self.wraps.resize(rows, None);
        self.wraps[rows - 1] = None;
        self.rows.truncate(rows);
        for row in &mut self.rows {
            remap(row, columns, style, |start, end| {
                (end <= columns).then_some(start)
            });
        }
        self.rows.resize(rows, vec![blank(style); columns]);
        self.columns = columns;
        Ok(())
    }

    pub fn scroll(
        &mut self,
        region: Range<usize>,
        count: usize,
        up: bool,
        style: StyleId,
    ) -> Result<(), StorageError> {
        if region.start > region.end || region.end > self.rows.len() {
            return Err(StorageError::OutOfBounds);
        }
        let count = count.min(region.len());
        if count == 0 {
            return Ok(());
        }
        let previous = self.wraps.clone();
        if region.start > 0 {
            self.wraps[region.start - 1] = None;
        }
        for row in region.clone() {
            self.wraps[row] = if up && row + count + 1 < region.end {
                previous[row + count]
            } else if !up && row >= region.start + count && row + 1 < region.end {
                previous[row - count]
            } else {
                None
            };
        }
        if up {
            for row in region.start..region.end - count {
                self.rows[row] = self.rows[row + count].clone();
            }
            for row in region.end - count..region.end {
                self.rows[row] = vec![blank(style); self.columns];
            }
        } else {
            for row in (region.start + count..region.end).rev() {
                self.rows[row] = self.rows[row - count].clone();
            }
            for row in region.start..region.start + count {
                self.rows[row] = vec![blank(style); self.columns];
            }
        }
        Ok(())
    }

    pub fn edit(
        &mut self,
        row: usize,
        column: usize,
        count: usize,
        insert: bool,
        style: StyleId,
    ) -> Result<(), StorageError> {
        self.bounds(row, column, 0)?;
        let count = count.min(self.columns - column);
        if count == 0 {
            return Ok(());
        }
        self.wraps[row] = None;
        let columns = self.columns;
        remap(&mut self.rows[row], columns, style, |start, end| {
            if end <= column {
                Some(start)
            } else if insert && start >= column && end <= columns - count {
                Some(start + count)
            } else if !insert && start >= column + count {
                Some(start - count)
            } else {
                None
            }
        });
        Ok(())
    }

    fn bounds(
        &self,
        row: usize,
        column: usize,
        length: usize,
    ) -> Result<Range<usize>, StorageError> {
        if row < self.rows.len()
            && let Some(end) = column
                .checked_add(length)
                .filter(|&end| end <= self.columns)
        {
            return Ok(column..end);
        }
        Err(StorageError::OutOfBounds)
    }

    fn erase(&mut self, row: usize, range: Range<usize>, style: StyleId) {
        if range.is_empty() {
            return;
        }
        let mut position = 0;
        for unit in &self.rows[row] {
            let end = position + unit.width;
            if position < range.end
                && end > range.start
                && self.wraps[row].is_some_and(|boundary| end >= boundary)
            {
                self.wraps[row] = None;
            }
            position = end;
        }
        remap(&mut self.rows[row], self.columns, style, |start, end| {
            (end <= range.start || start >= range.end).then_some(start)
        });
    }
}

fn index_at(row: &[Unit], column: usize) -> usize {
    let mut end = 0;
    row.iter()
        .position(|unit| {
            end += unit.width;
            column < end
        })
        .unwrap()
}

/// Rebuilds a row by mapping complete units to destination columns. Unoccupied
/// columns become styled blanks, including fragments of clipped wide units.
fn remap(
    row: &mut Vec<Unit>,
    columns: usize,
    style: StyleId,
    mut target: impl FnMut(usize, usize) -> Option<usize>,
) {
    let mut placed = vec![None; columns];
    let mut start = 0;
    for unit in std::mem::take(row) {
        let end = start + unit.width;
        if let Some(column) = target(start, end) {
            assert!(column + unit.width <= columns);
            placed[column] = Some(unit);
        }
        start = end;
    }
    let mut column = 0;
    while column < columns {
        let unit = placed[column].take().unwrap_or_else(|| blank(style));
        column += unit.width;
        row.push(unit);
    }
}
