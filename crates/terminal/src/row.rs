use std::{
    fmt,
    iter::FusedIterator,
    num::NonZeroU64,
    ops::Range,
    slice,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use crate::sizing::SizedBlock;
use crate::{
    Cell, CellPosition, CellText, CellWidth, GRAPHICS_PLACEHOLDER, MAX_CELL_TEXT_BYTES,
    SizedTextRow, StorageError, StyleId,
};

static NEXT_ROW_REVISION: AtomicU64 = AtomicU64::new(1);
const PLACEHOLDER_ABSENT: u64 = 1 << 62;
const PLACEHOLDER_PRESENT: u64 = 1 << 63;
const REVISION_MASK: u64 = !(PLACEHOLDER_ABSENT | PLACEHOLDER_PRESENT);

/// An opaque cache key for a physical row's cells, width, and extended text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RowRevision(NonZeroU64);

#[derive(Debug)]
struct ExtendedText {
    column: usize,
    text: String,
}

#[derive(Debug, Default)]
pub(crate) struct SizedRows {
    parts: Vec<SizedTextRow>,
    visible: AtomicUsize,
}

/// An owned display row with read-only cell and text access.
/// Grid mutations preserve its width and wide-cell pairs.
#[derive(Debug)]
pub struct Row {
    cells: Vec<Cell>,
    extended: Vec<ExtendedText>,
    sized: Option<Box<SizedRows>>,
    revision: AtomicU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayUnit<'a> {
    pub column: usize,
    pub cell: Cell,
    pub text: CellText<'a>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyleRun {
    pub columns: Range<usize>,
    pub style: StyleId,
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
        self.row
            .copy_sized_partitions(column, source, range.clone(), true)?;
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
        if entries.is_empty() {
            return;
        }
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
        self.invalidate_revision();
        source.invalidate_revision();
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
        self.copy_sized_partitions(column, source, range.clone(), false)?;
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
        self.invalidate_revision();
        // Source edge markers can become interior cells in the destination.
        let gap = source.cells.last().unwrap().row_wrap().unwrap_or(0);
        for edge in source.cells.len().saturating_sub(gap.max(1) + 1)..source.cells.len() {
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
        let sized = self.sized.as_ref().map_or(0, |rows| {
            rows.parts
                .iter()
                .filter(|part| part.columns().end > column)
                .map(SizedTextRow::retained_bytes)
                .sum()
        });
        self.retained_bytes().saturating_sub(removed + sized)
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
            .saturating_add(self.sized.as_ref().map_or(0, |sized| {
                size_of::<SizedRows>()
                    + sized.parts.capacity() * size_of::<SizedTextRow>()
                    + sized
                        .parts
                        .iter()
                        .map(SizedTextRow::retained_bytes)
                        .sum::<usize>()
            }))
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
            sized: None,
            revision: AtomicU64::new(0),
        })
    }

    /// Returns the current row revision. Moving the row preserves this key.
    /// Successful edits can publish a new key even when the visible values match.
    #[inline]
    pub fn revision(&self) -> RowRevision {
        if let Some(sized) = &self.sized {
            let visible = self.sized_text_rows().count();
            if sized.visible.swap(visible, Ordering::Relaxed) != visible {
                self.revision.store(0, Ordering::Relaxed);
            }
        }
        self.revision_from_packed(self.revision.load(Ordering::Relaxed))
    }

    pub fn sized_text_rows(&self) -> impl Iterator<Item = &SizedTextRow> {
        self.sized
            .iter()
            .flat_map(|sized| &sized.parts)
            .filter(|part| part.block.is_visible())
    }

    pub(crate) fn has_multiline_sized_text(&self) -> bool {
        self.sized_text_rows().any(|part| part.block.size.scale > 1)
    }

    pub(crate) fn sized_text_rows_from(
        &self,
        column: usize,
    ) -> impl Iterator<Item = &SizedTextRow> {
        let parts = self.sized.as_ref().map_or(&[][..], |sized| &sized.parts);
        let start = parts.partition_point(|part| part.columns().end <= column);
        parts[start..].iter().filter(|part| part.block.is_visible())
    }

    pub fn sized_text_at(&self, column: usize) -> Option<&SizedTextRow> {
        let parts = &self.sized.as_ref()?.parts;
        let index = parts
            .partition_point(|part| part.column <= column)
            .checked_sub(1)?;
        let part = &parts[index];
        (part.columns().contains(&column) && part.block.is_visible()).then_some(part)
    }

    pub(crate) fn reserve_sized(&mut self) -> Result<(), StorageError> {
        let parts = &mut self.sized.get_or_insert_with(Default::default).parts;
        parts.retain(|part| part.block.is_visible());
        parts.try_reserve(1)?;
        Ok(())
    }

    pub(crate) fn prepare_sized() -> Result<Box<SizedRows>, StorageError> {
        let mut sized = Box::<SizedRows>::default();
        sized.parts.try_reserve(1)?;
        Ok(sized)
    }

    pub(crate) fn install_prepared_sized(&mut self, sized: Box<SizedRows>) {
        debug_assert!(self.sized.as_ref().is_none_or(|rows| rows.parts.is_empty()));
        self.sized = Some(sized);
    }

    pub(crate) fn add_sized(&mut self, part: SizedTextRow, style: StyleId) {
        self.clear_range(part.columns(), style)
            .expect("The sized text fits");
        let parts = &mut self
            .sized
            .as_mut()
            .expect("The sized row was reserved")
            .parts;
        let index = parts.partition_point(|existing| existing.column < part.column);
        parts.insert(index, part);
        self.invalidate_revision();
    }

    pub(crate) fn replace_sized_block(&mut self, column: usize, block: Arc<SizedBlock>) {
        let parts = &mut self.sized.as_mut().expect("The sized row exists").parts;
        let index = parts
            .binary_search_by_key(&column, |part| part.column)
            .expect("The sized column exists");
        let columns = block.columns;
        let previous = std::mem::replace(&mut parts[index].block, block);
        if columns < previous.columns && self.wrap_end() == Some(column + previous.columns) {
            self.set_wrap_end(Some(column + columns));
        }
        self.invalidate_revision();
    }

    pub(crate) fn can_grow_sized_block(&self, added: usize, end: usize) -> bool {
        end <= self.cells.len()
            && self
                .sized_text_at(added)
                .is_none_or(|part| part.row_offset == 0)
    }

    pub(crate) fn grow_sized_block(
        &mut self,
        column: usize,
        added: usize,
        block: Arc<SizedBlock>,
        insert: bool,
    ) {
        let end = column + block.columns;
        let style = self.cells[column].style();
        let wrapped = self.wrap_end() == Some(added);
        if insert {
            self.insert_cells(added, end - added, style)
                .expect("The width fits");
        }
        self.clear_range(added..end, style).expect("The width fits");
        self.replace_sized_block(column, block);
        if wrapped && !insert {
            self.set_wrap_end(Some(end));
        }
    }

    pub(crate) fn remove_moved_sized_block(
        &mut self,
        column: usize,
        block: &Arc<SizedBlock>,
    ) -> bool {
        let wrap = self.wrap_end();
        let Some(sized) = &mut self.sized else {
            return false;
        };
        let Ok(index) = sized
            .parts
            .binary_search_by_key(&column, |part| part.column)
        else {
            return false;
        };
        if !Arc::ptr_eq(&sized.parts[index].block, block) {
            return false;
        }
        sized.parts.remove(index);
        if wrap == Some(column + block.columns) {
            self.set_wrap_end((column != 0).then_some(column));
        }
        self.invalidate_revision();
        true
    }

    fn copy_sized_partitions(
        &mut self,
        column: usize,
        source: &Self,
        range: Range<usize>,
        multiline: bool,
    ) -> Result<(), StorageError> {
        for part in source.sized_text_rows().filter(|part| {
            (multiline || part.size().scale == 1)
                && part.column >= range.start
                && part.columns().end <= range.end
        }) {
            self.reserve_sized()?;
            self.sized.as_mut().unwrap().parts.push(SizedTextRow {
                column: column + part.column - range.start,
                ..part.clone()
            });
        }
        Ok(())
    }

    pub(crate) fn erase_split_sized(&mut self) {
        for part in self.sized_text_rows().filter(|part| part.row_offset != 0) {
            part.block.erase();
        }
    }

    pub(crate) fn erase_sized(&mut self) {
        if let Some(sized) = &mut self.sized {
            for part in sized.parts.drain(..) {
                part.block.erase();
            }
        }
    }

    fn erase_sized_intersections(&mut self, range: Range<usize>) {
        if let Some(sized) = &mut self.sized {
            sized.parts.retain(|part| {
                if part.column < range.end && part.columns().end > range.start {
                    part.block.erase();
                    false
                } else {
                    true
                }
            });
        }
    }

    #[inline]
    fn revision_from_packed(&self, packed: u64) -> RowRevision {
        match NonZeroU64::new(packed & REVISION_MASK) {
            Some(revision) => RowRevision(revision),
            None => self.publish_revision(),
        }
    }

    #[cold]
    fn publish_revision(&self) -> RowRevision {
        let fresh = NEXT_ROW_REVISION
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                (value <= REVISION_MASK).then_some(value + 1)
            })
            .expect("Row revision space exhausted");
        let revision =
            match self
                .revision
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |packed| {
                    (packed & REVISION_MASK == 0).then_some(packed | fresh)
                }) {
                Ok(_) => fresh,
                Err(published) => published & REVISION_MASK,
            };
        RowRevision(NonZeroU64::new(revision).expect("A revision was published"))
    }

    /// A revision for rows that contain a Kitty graphics placeholder.
    #[inline]
    pub fn graphics_placeholder_revision(&self) -> Option<RowRevision> {
        let packed = self.revision.load(Ordering::Relaxed);
        match packed & !REVISION_MASK {
            PLACEHOLDER_PRESENT => Some(self.revision_from_packed(packed)),
            PLACEHOLDER_ABSENT => None,
            _ => self
                .scan_graphics_placeholders()
                .then(|| self.revision_from_packed(packed)),
        }
    }

    #[inline]
    pub(crate) fn has_graphics_placeholder(&self) -> bool {
        let packed = self.revision.load(Ordering::Relaxed);
        if packed > REVISION_MASK {
            packed & PLACEHOLDER_PRESENT != 0
        } else {
            self.scan_graphics_placeholders()
        }
    }

    #[cold]
    fn scan_graphics_placeholders(&self) -> bool {
        let present = self
            .cells
            .iter()
            .any(|cell| cell.has_character(GRAPHICS_PLACEHOLDER));
        let flag = if present {
            PLACEHOLDER_PRESENT
        } else {
            PLACEHOLDER_ABSENT
        };
        // Concurrent readers observe the same cells and publish the same flag.
        self.revision.fetch_or(flag, Ordering::Relaxed);
        present
    }

    #[inline]
    fn invalidate_revision(&mut self) {
        *self.revision.get_mut() = 0;
    }

    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    /// Groups consecutive physical columns by style within the clamped range.
    #[inline]
    pub fn style_runs(&self, columns: Range<usize>) -> impl Iterator<Item = StyleRun> + '_ {
        let end = columns.end.min(self.cells.len());
        let mut start = columns.start.min(end);
        self.cells[start..end]
            .chunk_by(|left, right| left.style() == right.style())
            .map(move |cells| {
                let end = start + cells.len();
                let run = StyleRun {
                    columns: start..end,
                    style: cells[0].style(),
                };
                start = end;
                run
            })
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
    /// wrapping. A complete display unit may leave excluded columns at the right edge.
    /// The last row of a [`crate::Grid`] returns `None`. A retained history row
    /// can preserve its connection to the following primary-screen row.
    /// A materialized [`crate::ReflowRow`] can retain an outgoing connection independently.
    pub fn wrap_end(&self) -> Option<usize> {
        let columns = self.cells.len();
        let gap = self.cells[columns - 1].row_wrap()?;
        let end = columns - gap;
        // Replacing the last used cell or any skipped cell cancels an early wrap.
        self.cells[end - 1..columns - 1]
            .iter()
            .all(|cell| cell.is_row_gap())
            .then_some(end)
    }

    pub(crate) fn set_wrap_end(&mut self, end: Option<usize>) {
        let columns = self.cells.len();
        debug_assert!(end.is_none_or(|end| (1..=columns).contains(&end)));
        if let Some(previous) = self.cells[columns - 1].row_wrap() {
            for cell in &mut self.cells[columns - previous - 1..columns - 1] {
                cell.set_row_gap(false);
            }
        }
        if let Some(end) = end {
            for cell in &mut self.cells[end - 1..columns - 1] {
                cell.set_row_gap(true);
            }
        }
        self.cells[columns - 1].set_row_wrap(end.map(|end| columns - end));
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

    // Starts at the last lookup's successor for known extended cells visited in order.
    pub(crate) fn next_extended_text(&self, column: usize, next: &mut usize) -> &str {
        let remaining = &self.extended[*next..];
        let index = if remaining
            .first()
            .is_some_and(|entry| entry.column == column)
        {
            0
        } else {
            remaining
                .binary_search_by_key(&column, |entry| entry.column)
                .expect("Extended text is present")
        };
        *next += index + 1;
        &remaining[index].text
    }

    pub(crate) fn written_columns(&self, range: Range<usize>) -> Option<Range<usize>> {
        if range.is_empty() {
            return None;
        }
        let mut first = range.end;
        let mut last = range.start;
        let cells = &self.cells[range.clone()];
        if let Some(start) = cells.iter().position(|cell| !cell.is_erased()) {
            first = range.start + start;
            last = range.end
                - cells[start..]
                    .iter()
                    .rev()
                    .take_while(|cell| cell.is_erased())
                    .count();
        }
        for part in self
            .sized_text_rows_from(range.start)
            .take_while(|part| part.column < range.end)
        {
            first = first.min(part.column.max(range.start));
            last = last.max(part.columns().end.min(range.end));
        }
        (first < last).then_some(first..last)
    }

    #[inline]
    pub(crate) fn write_text(
        &self,
        range: Range<usize>,
        output: &mut impl fmt::Write,
        include_text: impl Fn(&SizedTextRow) -> bool,
    ) -> fmt::Result {
        if self.sized.is_some() {
            return self.write_sized_text(range, output, include_text);
        }
        self.write_cell_text(range, output)
    }

    #[cold]
    fn write_sized_text(
        &self,
        range: Range<usize>,
        output: &mut impl fmt::Write,
        include_text: impl Fn(&SizedTextRow) -> bool,
    ) -> fmt::Result {
        if range.is_empty() {
            return Ok(());
        }
        let mut column = range.start;
        for part in self
            .sized_text_rows()
            .filter(|part| part.column < range.end && part.columns().end > range.start)
        {
            self.write_cell_text(column..part.column, output)?;
            if include_text(part) {
                output.write_str(part.text())?;
            } else {
                for _ in column.max(part.column)..part.columns().end.min(range.end) {
                    output.write_char(' ')?;
                }
            }
            column = part.columns().end;
        }
        self.write_cell_text(column..range.end, output)
    }

    fn write_cell_text(&self, range: Range<usize>, output: &mut impl fmt::Write) -> fmt::Result {
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
        } else if self.sized.is_some() {
            self.erase_sized_intersections(column..end);
        }
        self.cells[column] = cell;
        if width == CellWidth::Double {
            self.cells[column + 1] = Cell::continuation(style);
        }
        self.invalidate_revision();
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
            self.invalidate_revision();
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
        self.invalidate_revision();
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
            self.invalidate_revision();
        }
        Ok(())
    }

    #[inline]
    pub(crate) fn clear(&mut self, style: StyleId) {
        self.erase_sized();
        self.extended.clear();
        self.cells.fill(Cell::blank(style));
        self.invalidate_revision();
    }

    pub(crate) fn fill_scalar(&mut self, character: char, style: StyleId) {
        self.erase_sized();
        self.extended.clear();
        self.cells
            .fill(Cell::scalar(character, CellWidth::Single, style));
        self.invalidate_revision();
    }

    /// Replaces a recycled row with validated ASCII of exactly the row width.
    #[inline]
    pub(crate) fn reset_ascii(&mut self, bytes: &[u8], style: StyleId) {
        debug_assert_eq!(bytes.len(), self.cells.len());
        self.erase_sized();
        self.extended.clear();
        for (cell, &byte) in self.cells.iter_mut().zip(bytes) {
            *cell = Cell::scalar(char::from(byte), CellWidth::Single, style);
        }
        self.invalidate_revision();
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
        if self.sized.is_some() {
            self.shift_sized(column, count, true);
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
        self.invalidate_revision();
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
        if self.sized.is_some() {
            self.shift_sized(column, count, false);
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
        self.invalidate_revision();
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
            self.invalidate_revision();
        }
        if columns < self.cells.len() {
            self.erase_sized_intersections(columns..self.cells.len());
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
        if self.sized.is_some() {
            self.erase_sized_intersections(range.clone());
        }
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

    fn shift_sized(&mut self, column: usize, count: usize, insert: bool) {
        let width = self.cells.len();
        let Some(sized) = &mut self.sized else { return };
        sized.parts.retain_mut(|part| {
            let end = part.columns().end;
            if end <= column {
                return part.block.is_visible();
            }
            let source = if insert {
                column..width - count
            } else {
                column + count..width
            };
            if part.size().scale != 1 || part.column < source.start || end > source.end {
                part.block.erase();
                return false;
            }
            if insert {
                part.column += count;
            } else {
                part.column -= count;
            }
            part.block.is_visible()
        });
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
