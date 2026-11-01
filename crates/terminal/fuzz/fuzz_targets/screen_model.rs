use std::ops::Range;

use lune_terminal::{CellWidth, EraseMode, MAX_CELL_TEXT_BYTES, Screen, StorageError, StyleId};

use crate::model::Model;

pub enum Command<'a> {
    Write(&'a str, CellWidth),
    Ascii(&'a [u8]),
    Cursor(usize, usize),
    Region(Range<usize>),
    Resize(usize, usize),
    CarriageReturn,
    Backspace,
    LineFeed,
    ReverseIndex,
    EraseLine(EraseMode),
    EraseDisplay(EraseMode),
    InsertCells(usize),
    DeleteCells(usize),
    EraseCells(usize),
    InsertLines(usize),
    DeleteLines(usize),
    MoveUp(usize),
    MoveDown(usize),
    MoveLeft(usize),
    MoveRight(usize),
    ScrollUp(usize),
    ScrollDown(usize),
}

/// Uses complete display units and copying row edits. A column one past the
/// right edge represents delayed wrapping; the visible cursor is derived from it.
pub struct ScreenModel {
    storage: Model,
    row: usize,
    column: usize,
    region: Range<usize>,
}

impl ScreenModel {
    pub fn new(columns: usize, rows: usize) -> Self {
        Self {
            storage: Model::new(columns, rows),
            row: 0,
            column: 0,
            region: 0..rows,
        }
    }

    pub fn check(&self, screen: &Screen) {
        self.storage.check(screen.grid());
        let cursor = screen.cursor();
        assert_eq!(cursor.row, self.row);
        assert_eq!(cursor.column, self.visible_column());
        assert_eq!(cursor.wrap_pending, self.column == self.storage.columns());
        assert_eq!(screen.scroll_region(), self.region);
        assert!(self.row < self.storage.rows());
        assert!(self.column <= self.storage.columns());
        assert!(self.region.start < self.region.end && self.region.end <= self.storage.rows());
    }

    pub fn apply(&mut self, command: &Command<'_>, style: StyleId) -> Result<(), StorageError> {
        match command {
            Command::Write(text, width) => return self.write(text, *width, style),
            Command::Ascii(bytes) => {
                if bytes.iter().any(|&byte| !(b' '..=b'~').contains(&byte)) {
                    return Err(StorageError::InvalidAscii);
                }
                for &byte in *bytes {
                    self.write(&char::from(byte).to_string(), CellWidth::Single, style)?;
                }
            }
            Command::Cursor(row, column) => {
                if *row >= self.storage.rows() || *column >= self.storage.columns() {
                    return Err(StorageError::OutOfBounds);
                }
                self.row = *row;
                self.column = *column;
            }
            Command::Region(region) => {
                if region.start >= region.end || region.end > self.storage.rows() {
                    return Err(StorageError::OutOfBounds);
                }
                self.region = region.clone();
                self.cancel_wrap();
            }
            Command::Resize(columns, rows) => {
                if *columns == self.storage.columns() && *rows == self.storage.rows() {
                    return Ok(());
                }
                let old_rows = self.storage.rows();
                let column = self.visible_column();
                self.storage.resize(*columns, *rows, style)?;
                self.row = self.row.min(rows - 1);
                self.column = column.min(columns - 1);
                if *rows != old_rows {
                    self.region = 0..*rows;
                }
            }
            Command::CarriageReturn => self.column = 0,
            Command::Backspace => self.column = self.visible_column().saturating_sub(1),
            Command::LineFeed => self.index(true, style),
            Command::ReverseIndex => self.index(false, style),
            Command::EraseLine(mode) => self.erase(false, *mode, style),
            Command::EraseDisplay(mode) => self.erase(true, *mode, style),
            Command::InsertCells(count) | Command::DeleteCells(count) => {
                if *count != 0 {
                    self.storage.edit(
                        self.row,
                        self.visible_column(),
                        *count,
                        matches!(command, Command::InsertCells(_)),
                        style,
                    )?;
                    self.cancel_wrap();
                }
            }
            Command::EraseCells(count) => {
                if *count != 0 {
                    let column = self.visible_column();
                    let end = column + (*count).min(self.storage.columns() - column);
                    self.storage.clear_row(self.row, column..end, style)?;
                    self.cancel_wrap();
                }
            }
            Command::InsertLines(count) | Command::DeleteLines(count) => {
                if *count != 0 && self.region.contains(&self.row) {
                    self.storage.scroll(
                        self.row..self.region.end,
                        *count,
                        matches!(command, Command::DeleteLines(_)),
                        style,
                    )?;
                    self.cancel_wrap();
                }
            }
            Command::ScrollUp(count) | Command::ScrollDown(count) => {
                self.storage.scroll(
                    self.region.clone(),
                    *count,
                    matches!(command, Command::ScrollUp(_)),
                    style,
                )?;
            }
            Command::MoveUp(count)
            | Command::MoveDown(count)
            | Command::MoveLeft(count)
            | Command::MoveRight(count) => {
                if *count != 0 {
                    self.cancel_wrap();
                    // Walk individual cells until the directional barrier. The
                    // screen dimensions bound the steps needed for any count.
                    for _ in 0..(*count).min(self.storage.rows().max(self.storage.columns())) {
                        match command {
                            Command::MoveUp(_) if self.row > 0 && self.row != self.region.start => {
                                self.row -= 1;
                            }
                            Command::MoveDown(_)
                                if self.row + 1 < self.storage.rows()
                                    && self.row + 1 != self.region.end =>
                            {
                                self.row += 1;
                            }
                            Command::MoveLeft(_) if self.column > 0 => self.column -= 1,
                            Command::MoveRight(_) if self.column + 1 < self.storage.columns() => {
                                self.column += 1;
                            }
                            _ => break,
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn write(&mut self, text: &str, width: CellWidth, style: StyleId) -> Result<(), StorageError> {
        let width = if width == CellWidth::Single { 1 } else { 2 };
        let columns = self.storage.columns();
        if width > columns {
            return Err(StorageError::OutOfBounds);
        }
        if text.is_empty() {
            return Err(StorageError::EmptyText);
        }
        if text.len() > MAX_CELL_TEXT_BYTES {
            return Err(StorageError::TextTooLong);
        }
        if self.column + width > columns {
            if self.column < columns {
                self.storage
                    .clear_row(self.row, self.column..columns, style)?;
            }
            self.index(true, style);
            self.column = 0;
        }
        let cell_width = if width == 1 {
            CellWidth::Single
        } else {
            CellWidth::Double
        };
        self.storage
            .write(self.row, self.column, text, cell_width, style)?;
        self.column += width;
        Ok(())
    }

    fn index(&mut self, down: bool, style: StyleId) {
        if (down && self.row + 1 == self.region.end) || (!down && self.row == self.region.start) {
            self.storage
                .scroll(self.region.clone(), 1, down, style)
                .unwrap();
        } else if down {
            self.row = (self.row + 1).min(self.storage.rows() - 1);
        } else {
            self.row = self.row.saturating_sub(1);
        }
        self.cancel_wrap();
    }

    fn erase(&mut self, display: bool, mode: EraseMode, style: StyleId) {
        let columns = self.storage.columns();
        let cursor = self.row * columns + self.visible_column();
        let scope = if display {
            0..self.storage.rows() * columns
        } else {
            self.row * columns..(self.row + 1) * columns
        };
        let range = match mode {
            EraseMode::FromCursor => cursor..scope.end,
            EraseMode::ToCursor => scope.start..cursor + 1,
            EraseMode::All => scope,
        };
        for row in 0..self.storage.rows() {
            let start = range.start.saturating_sub(row * columns).min(columns);
            let end = range.end.saturating_sub(row * columns).min(columns);
            self.storage.clear_row(row, start..end, style).unwrap();
        }
        self.cancel_wrap();
    }

    fn visible_column(&self) -> usize {
        self.column.min(self.storage.columns() - 1)
    }

    fn cancel_wrap(&mut self) {
        self.column = self.visible_column();
    }
}
