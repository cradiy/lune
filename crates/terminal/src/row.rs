use std::{fmt, iter::FusedIterator, ops::Range, slice};

use crate::{Cell, CellPosition, CellText, CellWidth, MAX_CELL_TEXT_BYTES, StorageError, StyleId};

#[derive(Debug)]
struct ExtendedText {
    column: usize,
    text: String,
}

/// An owned display row with read-only cell and text access.
/// Grid mutations preserve its width and wide-cell pairs.
#[derive(Debug)]
pub struct Row {
    cells: Vec<Cell>,
    extended: Vec<ExtendedText>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayUnit<'a> {
    pub column: usize,
    pub cell: Cell,
    pub text: CellText<'a>,
}

/// Complete display units traversed in physical column order.
pub struct DisplayUnits<'a> {
    column: usize,
    cells: &'a [Cell],
    extended: slice::Iter<'a, ExtendedText>,
}

impl<'a> Iterator for DisplayUnits<'a> {
    type Item = DisplayUnit<'a>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let cell = *self.cells.first()?;
        let column = self.column;
        let width = cell.width();
        self.cells = &self.cells[width..];
        self.column += width;
        let text = if cell.has_extended_text() {
            let entry = self.extended.next().expect("Extended text is present");
            debug_assert_eq!(entry.column, column);
            CellText::Extended(&entry.text)
        } else {
            CellText::Scalar(
                cell.character()
                    .expect("Display units start at a leading cell"),
            )
        };
        Some(DisplayUnit { column, cell, text })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let columns = self.cells.len();
        (columns.div_ceil(2), Some(columns))
    }
}

impl FusedIterator for DisplayUnits<'_> {}

/// Unpublished cell storage whose extended-text slots are filled during commit.
pub(crate) struct PreparedRow {
    row: Row,
    text_capacity: usize,
}

/// One retained destination row's source spans and post-transfer byte charge.
pub(crate) struct RowTransfer {
    pub(crate) target: usize,
    pub(crate) source: Range<CellPosition>,
    pub(crate) bytes: usize,
}

impl PreparedRow {
    /// Requests only the additional sparse slots needed by the prepared row.
    pub(crate) fn reserve_text(&mut self, entries: usize) -> Result<(), StorageError> {
        self.row.extended.try_reserve_exact(entries)?;
        Ok(())
    }

    pub(crate) fn new(columns: usize, style: StyleId) -> Result<Self, StorageError> {
        Ok(Self {
            row: Row::new(columns, style)?,
            text_capacity: 0,
        })
    }

    pub(crate) fn prepare_segment(
        &mut self,
        column: usize,
        source: &Row,
        range: Range<usize>,
    ) -> Result<(), StorageError> {
        self.row.copy_cells(column, source, range.clone());
        if source.extended.is_empty() {
            return Ok(());
        }
        let entries = source.extended_range(range.clone());
        self.reserve_text(entries.len())?;
        for entry in &source.extended[entries] {
            self.text_capacity = self.text_capacity.saturating_add(entry.text.capacity());
            self.row.extended.push(ExtendedText {
                column: column + entry.column - range.start,
                text: String::new(),
            });
        }
        Ok(())
    }

    pub(crate) fn set_wrap_end(&mut self, end: Option<usize>) {
        self.row.set_wrap_end(end);
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        self.row.metadata_bytes().saturating_add(self.text_capacity)
    }

    pub(crate) fn finish_preparation(self) -> (Row, usize, bool) {
        let bytes = self.retained_bytes();
        let has_text = !self.row.extended.is_empty();
        (self.row, bytes, has_text)
    }
}

impl Row {
    /// Moves disjoint source units into reserved destination slots during commit.
    pub(crate) fn transfer_segment(
        &mut self,
        column: usize,
        source: &mut Row,
        range: Range<usize>,
    ) {
        let entries = source.extended_range(range.clone());
        let start = self.extended.partition_point(|entry| entry.column < column);
        for (target, entry) in self.extended[start..start + entries.len()]
            .iter_mut()
            .zip(&mut source.extended[entries])
        {
            debug_assert_eq!(target.column, column + entry.column - range.start);
            debug_assert_eq!(target.text.capacity(), 0);
            debug_assert!(!entry.text.is_empty());
            target.text = std::mem::take(&mut entry.text);
        }
    }

    /// Copies aligned units into the next segment of a fresh unpublished row.
    /// The caller supplies valid ranges and discards the destination on error.
    pub(crate) fn copy_segment(
        &mut self,
        column: usize,
        source: &Self,
        range: Range<usize>,
    ) -> Result<(), StorageError> {
        debug_assert!(
            self.extended
                .last()
                .is_none_or(|entry| entry.column < column)
        );
        self.copy_cells(column, source, range.clone());
        let entries = source.extended_range(range.clone());
        self.extended.try_reserve(entries.len())?;
        for entry in &source.extended[entries] {
            let mut text = String::new();
            text.try_reserve_exact(entry.text.len())?;
            text.push_str(&entry.text);
            self.extended.push(ExtendedText {
                column: column + entry.column - range.start,
                text,
            });
        }
        Ok(())
    }

    #[inline]
    pub(crate) fn has_extended(&self) -> bool {
        !self.extended.is_empty()
    }

    pub(crate) fn extended_count(&self, range: Range<usize>) -> usize {
        if self.extended.is_empty() {
            0
        } else {
            self.extended_range(range).len()
        }
    }

    fn extended_range(&self, range: Range<usize>) -> Range<usize> {
        self.extended
            .partition_point(|entry| entry.column < range.start)
            ..self
                .extended
                .partition_point(|entry| entry.column < range.end)
    }

    #[inline]
    fn copy_cells(&mut self, column: usize, source: &Self, range: Range<usize>) {
        self.cells[column..column + range.len()].copy_from_slice(&source.cells[range.clone()]);
        // Source edge markers can become interior cells in the destination.
        for edge in source.cells.len().saturating_sub(2)..source.cells.len() {
            if range.contains(&edge) {
                let cell = &mut self.cells[column + edge - range.start];
                *cell = cell.without_row_metadata();
            }
        }
    }

    pub(crate) fn bytes_without_last_unit(&self) -> usize {
        let column =
            self.cells.len() - 1 - usize::from(self.cells.last().unwrap().is_continuation());
        let removed = self
            .extended_index(column)
            .ok()
            .map_or(0, |index| self.extended[index].text.capacity());
        self.retained_bytes().saturating_sub(removed)
    }

    pub(crate) fn reset_write(
        &mut self,
        columns: usize,
        text: &str,
        width: CellWidth,
        style: StyleId,
    ) -> Result<(), StorageError> {
        debug_assert!(self.fits_columns(columns));
        if width.columns() > columns {
            return Err(StorageError::OutOfBounds);
        }
        let character = text.chars().next().ok_or(StorageError::EmptyText)?;
        if text.len() > MAX_CELL_TEXT_BYTES {
            return Err(StorageError::TextTooLong);
        }
        let owned = if text.len() > character.len_utf8() {
            if let Ok(index) = self.extended_index(0) {
                let entry = &mut self.extended[index];
                entry
                    .text
                    .try_reserve(text.len().saturating_sub(entry.text.len()))?;
                let mut owned = std::mem::take(&mut entry.text);
                owned.clear();
                owned.push_str(text);
                Some(owned)
            } else {
                let mut owned = String::new();
                owned.try_reserve_exact(text.len())?;
                if self.extended.capacity() == 0 {
                    self.extended.try_reserve(1)?;
                }
                owned.push_str(text);
                Some(owned)
            }
        } else {
            None
        };
        // Every fallible reservation precedes clearing or resizing the row.
        self.clear(style);
        self.resize_prepared(columns, style);
        let mut cell = Cell::scalar(character, width, style);
        if let Some(text) = owned {
            self.extended.push(ExtendedText { column: 0, text });
            cell.set_extended();
        }
        self.cells[0] = cell;
        if width == CellWidth::Double {
            self.cells[1] = Cell::continuation(style);
        }
        Ok(())
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        self.metadata_bytes()
            .saturating_add(self.extended.iter().fold(0_usize, |bytes, entry| {
                bytes.saturating_add(entry.text.capacity())
            }))
    }

    fn metadata_bytes(&self) -> usize {
        self.cells
            .capacity()
            .saturating_mul(size_of::<Cell>())
            .saturating_add(
                self.extended
                    .capacity()
                    .saturating_mul(size_of::<ExtendedText>()),
            )
    }

    pub(crate) fn fits_columns(&self, columns: usize) -> bool {
        self.cells.capacity() >= columns
    }

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

    /// Borrows units intersecting a clamped half-open column range.
    /// A wide unit crossing either edge is returned whole, with its leading column.
    #[inline]
    pub fn display_units(&self, columns: Range<usize>) -> DisplayUnits<'_> {
        let mut end = columns.end.min(self.cells.len());
        let mut start = columns.start.min(end);
        if start < end {
            start -= usize::from(self.cells[start].is_continuation());
            end += usize::from(
                self.cells
                    .get(end)
                    .is_some_and(|cell| cell.is_continuation()),
            );
        }
        let first = if start < end && start != 0 {
            self.extended.partition_point(|entry| entry.column < start)
        } else {
            0
        };
        DisplayUnits {
            column: start,
            cells: &self.cells[start..end],
            extended: self.extended[first..].iter(),
        }
    }

    /// Exclusive column where this row joins the next row through automatic
    /// wrapping. A wide unit may leave one excluded column at the right edge.
    /// The last row of a [`crate::Grid`] returns `None`. A retained history row
    /// can preserve its connection to the following primary-screen row.
    /// A materialized [`crate::ReflowRow`] can retain an outgoing connection independently.
    pub fn wrap_end(&self) -> Option<usize> {
        let columns = self.cells.len();
        // A gap join requires both edge markers. Replacing either cell leaves
        // any remaining marker inactive until an explicit connection is set.
        match self.cells[columns - 1].row_wrap() {
            Some(false) => Some(columns),
            Some(true) if columns > 1 && self.cells[columns - 2].is_row_gap() => Some(columns - 1),
            _ => None,
        }
    }

    pub(crate) fn set_wrap_end(&mut self, end: Option<usize>) {
        let columns = self.cells.len();
        debug_assert!(end.is_none_or(|end| end > 0 && (end == columns || end + 1 == columns)));
        let gap = end.is_some_and(|end| end != columns);
        if columns > 1 {
            self.cells[columns - 2].set_row_gap(gap);
        }
        self.cells[columns - 1].set_row_wrap(end.map(|_| gap));
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

    pub(crate) fn write_text(
        &self,
        range: Range<usize>,
        output: &mut impl fmt::Write,
    ) -> fmt::Result {
        if range.start >= range.end {
            return Ok(());
        }
        let start = range.start - usize::from(self.cells[range.start].is_continuation());
        let first = self.extended.partition_point(|entry| entry.column < start);
        let mut extended = self.extended[first..].iter();
        for cell in &self.cells[start..range.end] {
            if cell.has_extended_text() {
                output.write_str(&extended.next().expect("Extended text is present").text)?;
            } else if let Some(character) = cell.character() {
                output.write_char(character)?;
            }
        }
        Ok(())
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

    pub(crate) fn revise(
        &mut self,
        column: usize,
        text: &str,
        width: CellWidth,
        style: StyleId,
        insert: bool,
    ) -> Result<(), StorageError> {
        let previous = self
            .cells
            .get(column)
            .ok_or(StorageError::OutOfBounds)?
            .width();
        if previous == 0 {
            return Err(StorageError::OutOfBounds);
        }
        self.end(column, width.columns())?;
        let join = self.wrap_end();
        if insert && width.columns() > previous {
            // Reserve and publish the revised text in the existing footprint.
            // The following shift preserves this entry and its capacity, making
            // the final width update infallible even for extended text.
            self.write(column, text, CellWidth::Single, style)?;
            self.insert_cells(column + previous, width.columns() - previous, style)
                .expect("The additional columns fit");
            self.write(column, text, width, style)
                .expect("Revised text storage was reserved before the shift");
        } else {
            self.write(column, text, width, style)?;
            if insert && width.columns() < previous {
                self.delete_cells(column + width.columns(), previous - width.columns(), style)
                    .expect("The released columns fit");
            }
        }
        if previous == width.columns() {
            self.set_wrap_end(join);
        } else {
            self.set_wrap_end(None);
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
    #[inline]
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
        if self.wrap_end().is_some_and(|end| column >= end) {
            self.set_wrap_end(None);
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

    #[inline]
    pub(crate) fn clear(&mut self, style: StyleId) {
        self.extended.clear();
        self.cells.fill(Cell::blank(style));
    }

    pub(crate) fn fill_scalar(&mut self, character: char, style: StyleId) {
        self.extended.clear();
        self.cells
            .fill(Cell::scalar(character, CellWidth::Single, style));
    }

    /// Replaces a recycled row with validated ASCII of exactly the row width.
    #[inline]
    pub(crate) fn reset_ascii(&mut self, bytes: &[u8], style: StyleId) {
        debug_assert_eq!(bytes.len(), self.cells.len());
        self.extended.clear();
        for (cell, &byte) in self.cells.iter_mut().zip(bytes) {
            *cell = Cell::scalar(char::from(byte), CellWidth::Single, style);
        }
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

        self.set_wrap_end(None);

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

        self.set_wrap_end(None);

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
        if columns != self.cells.len() {
            self.set_wrap_end(None);
        }
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
