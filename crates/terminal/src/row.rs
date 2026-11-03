use std::ops::Range;

use crate::{Cell, CellText, CellWidth, MAX_CELL_TEXT_BYTES, StorageError, StyleId};

#[derive(Debug)]
struct ExtendedText {
    column: usize,
    text: String,
}

/// A read-only row view. Grid mutations preserve its width and wide-cell pairs.
#[derive(Debug)]
pub struct Row {
    cells: Vec<Cell>,
    extended: Vec<ExtendedText>,
}

impl Row {
    pub(crate) fn new(columns: usize, style: StyleId) -> Result<Self, StorageError> {
        let mut cells = Vec::new();
        cells.try_reserve_exact(columns)?;
        cells.resize(columns, Cell::blank(style));
        Ok(Self {
            cells,
            extended: Vec::new(),
        })
    }

    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    pub fn text(&self, column: usize) -> Option<CellText<'_>> {
        let cell = self.cells.get(column)?;
        if cell.is_continuation() {
            Some(CellText::Continuation)
        } else if cell.has_extended_text() {
            let index = self
                .extended_index(column)
                .expect("Extended text is present");
            Some(CellText::Extended(&self.extended[index].text))
        } else {
            Some(CellText::Scalar(
                cell.character().expect("A scalar is present"),
            ))
        }
    }

    pub(crate) fn write(
        &mut self,
        column: usize,
        text: &str,
        width: CellWidth,
        style: StyleId,
    ) -> Result<(), StorageError> {
        match width {
            CellWidth::Single => self.write_width::<false>(column, text, style),
            CellWidth::Double => self.write_width::<true>(column, text, style),
        }
    }

    #[inline]
    fn write_width<const DOUBLE_WIDTH: bool>(
        &mut self,
        column: usize,
        text: &str,
        style: StyleId,
    ) -> Result<(), StorageError> {
        let width = if DOUBLE_WIDTH {
            CellWidth::Double
        } else {
            CellWidth::Single
        };
        let end = self.end(column, width.columns())?;
        let character = text.chars().next().ok_or(StorageError::EmptyText)?;
        if text.len() > MAX_CELL_TEXT_BYTES {
            return Err(StorageError::TextTooLong);
        }

        let mut cell = Cell::scalar(character, width, style);
        if text.len() > character.len_utf8() {
            self.write_extended(column..end, text, style)?;
            cell.set_extended();
        } else if !self.cells[column].is_inline_with_width(width) {
            self.prepare_overwrite(column..end, style, None);
        }
        self.cells[column] = cell;
        if width == CellWidth::Double {
            self.cells[column + 1] = Cell::continuation(style);
        }
        Ok(())
    }

    pub(crate) fn write_inserting(
        &mut self,
        column: usize,
        text: &str,
        width: CellWidth,
        style: StyleId,
        clear_gap: bool,
    ) -> Result<(), StorageError> {
        self.end(column, width.columns())?;
        let character = text.chars().next().ok_or(StorageError::EmptyText)?;
        if text.len() > MAX_CELL_TEXT_BYTES {
            return Err(StorageError::TextTooLong);
        }
        let owned = if text.len() > character.len_utf8() {
            let mut owned = String::new();
            owned.try_reserve_exact(text.len())?;
            self.extended.try_reserve(1)?;
            owned.push_str(text);
            Some(owned)
        } else {
            None
        };
        if clear_gap {
            let last = self.cells.len() - 1;
            self.clear_range(last..last + 1, style)
                .expect("The last column is in bounds");
        }
        self.insert_cells(column, width.columns(), style)
            .expect("The destination fits");
        let mut cell = Cell::scalar(character, width, style);
        if let Some(text) = owned {
            let index = self
                .extended_index(column)
                .expect_err("Inserted cells are blank");
            self.extended.insert(index, ExtendedText { column, text });
            cell.set_extended();
        }
        self.cells[column] = cell;
        if width == CellWidth::Double {
            self.cells[column + 1] = Cell::continuation(style);
        }
        Ok(())
    }

    // Keep allocation and sparse-index updates outside the scalar write path.
    #[inline(never)]
    fn write_extended(
        &mut self,
        range: Range<usize>,
        text: &str,
        style: StyleId,
    ) -> Result<(), StorageError> {
        let column = range.start;
        let reuse = self.extended_index(column).ok();
        let owned = if let Some(index) = reuse {
            let entry = &mut self.extended[index];
            entry
                .text
                .try_reserve(text.len().saturating_sub(entry.text.len()))?;
            None
        } else {
            let mut owned = String::new();
            owned.try_reserve_exact(text.len())?;
            self.extended.try_reserve(1)?;
            owned.push_str(text);
            Some(owned)
        };

        self.prepare_overwrite(range, style, reuse.map(|_| column));
        match owned {
            Some(text) => {
                let index = self
                    .extended_index(column)
                    .expect_err("The old text was removed");
                self.extended.insert(index, ExtendedText { column, text });
            }
            None => {
                let index = self
                    .extended_index(column)
                    .expect("Reusable text is present");
                let entry = &mut self.extended[index];
                entry.text.clear();
                entry.text.push_str(text);
            }
        }
        Ok(())
    }

    pub(crate) fn write_ascii(
        &mut self,
        column: usize,
        bytes: &[u8],
        style: StyleId,
    ) -> Result<(), StorageError> {
        self.end(column, bytes.len())?;
        if bytes.iter().any(|&byte| !(b' '..=b'~').contains(&byte)) {
            return Err(StorageError::InvalidAscii);
        }
        self.write_ascii_validated(column, bytes, style);
        Ok(())
    }

    /// The caller validates printable ASCII and the destination cell range.
    pub(crate) fn write_ascii_validated(&mut self, column: usize, bytes: &[u8], style: StyleId) {
        if !bytes.is_empty() {
            let end = column + bytes.len();
            self.prepare_overwrite(column..end, style, None);
            for (cell, &byte) in self.cells[column..end].iter_mut().zip(bytes) {
                *cell = Cell::scalar(char::from(byte), CellWidth::Single, style);
            }
        }
    }

    pub(crate) fn append_text(&mut self, column: usize, text: &str) -> Result<(), StorageError> {
        let cell = *self.cells.get(column).ok_or(StorageError::OutOfBounds)?;
        let column = if cell.is_continuation() {
            column - 1
        } else {
            column
        };
        if text.is_empty() {
            return Ok(());
        }

        match self.extended_index(column) {
            Ok(index) => {
                let entry = &mut self.extended[index];
                check_text_length(entry.text.len(), text.len())?;
                entry.text.try_reserve(text.len())?;
                entry.text.push_str(text);
            }
            Err(index) => {
                let character = self.cells[column]
                    .character()
                    .expect("A leading scalar is present");
                check_text_length(character.len_utf8(), text.len())?;
                let mut owned = String::new();
                owned.try_reserve_exact(character.len_utf8() + text.len())?;
                self.extended.try_reserve(1)?;
                owned.push(character);
                owned.push_str(text);
                self.extended.insert(
                    index,
                    ExtendedText {
                        column,
                        text: owned,
                    },
                );
                self.cells[column].set_extended();
            }
        }
        Ok(())
    }

    pub(crate) fn clear_range(
        &mut self,
        range: Range<usize>,
        style: StyleId,
    ) -> Result<(), StorageError> {
        if range.start > range.end || range.end > self.cells.len() {
            return Err(StorageError::OutOfBounds);
        }
        if !range.is_empty() {
            self.prepare_overwrite(range.clone(), style, None);
            self.cells[range].fill(Cell::blank(style));
        }
        Ok(())
    }

    pub(crate) fn clear(&mut self, style: StyleId) {
        self.extended.clear();
        self.cells.fill(Cell::blank(style));
    }

    pub(crate) fn insert_cells(
        &mut self,
        column: usize,
        count: usize,
        style: StyleId,
    ) -> Result<(), StorageError> {
        self.end(column, 0)?;
        let count = count.min(self.cells.len() - column);
        if count == 0 {
            return Ok(());
        }

        if self.cells[column].is_continuation() {
            self.prepare_overwrite(column..column + 1, style, None);
            self.cells[column] = Cell::blank(style);
        }
        let retained = self.cells.len() - count;
        self.prepare_overwrite(retained..self.cells.len(), style, None);
        self.cells.copy_within(column..retained, column + count);
        self.cells[column..column + count].fill(Cell::blank(style));
        let first = self.extended.partition_point(|entry| entry.column < column);
        for entry in &mut self.extended[first..] {
            entry.column += count;
        }
        Ok(())
    }

    pub(crate) fn delete_cells(
        &mut self,
        column: usize,
        count: usize,
        style: StyleId,
    ) -> Result<(), StorageError> {
        self.end(column, 0)?;
        let count = count.min(self.cells.len() - column);
        if count == 0 {
            return Ok(());
        }

        let end = column + count;
        self.prepare_overwrite(column..end, style, None);
        self.cells.copy_within(end.., column);
        let retained = self.cells.len() - count;
        self.cells[retained..].fill(Cell::blank(style));
        let first = self.extended.partition_point(|entry| entry.column < end);
        for entry in &mut self.extended[first..] {
            entry.column -= count;
        }
        Ok(())
    }

    pub(crate) fn reserve_columns(&mut self, columns: usize) -> Result<(), StorageError> {
        self.cells
            .try_reserve_exact(columns.saturating_sub(self.cells.len()))?;
        Ok(())
    }

    pub(crate) fn resize_prepared(&mut self, columns: usize, style: StyleId) {
        if columns < self.cells.len() {
            self.cells.truncate(columns);
            let text_columns = if self.cells[columns - 1].width() == 2 {
                self.cells[columns - 1] = Cell::blank(style);
                columns - 1
            } else {
                columns
            };
            let retained = self
                .extended
                .partition_point(|entry| entry.column < text_columns);
            self.extended.truncate(retained);
        } else {
            self.cells.resize(columns, Cell::blank(style));
        }
    }

    fn extended_index(&self, column: usize) -> Result<usize, usize> {
        self.extended
            .binary_search_by_key(&column, |entry| entry.column)
    }

    fn end(&self, column: usize, length: usize) -> Result<usize, StorageError> {
        match column.checked_add(length) {
            Some(end) if end <= self.cells.len() => Ok(end),
            _ => Err(StorageError::OutOfBounds),
        }
    }

    fn prepare_overwrite(&mut self, range: Range<usize>, style: StyleId, keep: Option<usize>) {
        let mut start = range.start;
        let mut end = range.end;
        if self.cells[start].is_continuation() {
            start -= 1;
            self.cells[start] = Cell::blank(style);
        }
        if self.cells[end - 1].width() == 2 {
            self.cells[end] = Cell::blank(style);
            end += 1;
        }
        if !self.extended.is_empty() {
            match keep {
                Some(column) => {
                    self.remove_extended(column + 1..end);
                    self.remove_extended(start..column);
                }
                None => self.remove_extended(start..end),
            }
        }
    }

    fn remove_extended(&mut self, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        let start = self
            .extended
            .partition_point(|entry| entry.column < range.start);
        let end = self
            .extended
            .partition_point(|entry| entry.column < range.end);
        if start != end {
            self.extended.drain(start..end);
        }
    }
}

fn check_text_length(current: usize, additional: usize) -> Result<(), StorageError> {
    match current.checked_add(additional) {
        Some(length) if length <= MAX_CELL_TEXT_BYTES => Ok(()),
        _ => Err(StorageError::TextTooLong),
    }
}
