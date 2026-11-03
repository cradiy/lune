use std::{collections::BTreeSet, ops::Range};

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
    TabForward(usize),
    TabBackward(usize),
    SetTab,
    ClearTab,
    ClearTabs,
    AutoWrap(bool),
    OriginMode(bool),
    SaveCursor,
    RestoreCursor,
    Reset,
    InsertMode(bool),
}

struct SavedCursor {
    row: usize,
    column: usize,
    pending_width: Option<usize>,
    auto_wrap: bool,
    origin_mode: bool,
}

/// Uses complete display units and copying row edits. A column one past the
/// right edge represents delayed wrapping; the visible cursor is derived from it.
pub struct ScreenModel {
    storage: Model,
    row: usize,
    column: usize,
    region: Range<usize>,
    tabs: BTreeSet<usize>,
    auto_wrap: bool,
    origin_mode: bool,
    saved: SavedCursor,
    insert_mode: bool,
}

impl ScreenModel {
    pub fn new(columns: usize, rows: usize) -> Self {
        Self {
            storage: Model::new(columns, rows),
            row: 0,
            column: 0,
            region: 0..rows,
            tabs: (0..columns).step_by(8).collect(),
            auto_wrap: true,
            origin_mode: false,
            insert_mode: false,
            saved: SavedCursor {
                row: 0,
                column: 0,
                pending_width: None,
                auto_wrap: true,
                origin_mode: false,
            },
        }
    }

    pub fn check(&self, screen: &Screen) {
        self.storage.check(screen.grid());
        let cursor = screen.cursor();
        assert_eq!(cursor.row, self.row);
        assert_eq!(cursor.column, self.visible_column());
        assert_eq!(cursor.wrap_pending, self.column == self.storage.columns());
        assert_eq!(screen.scroll_region(), self.region);
        assert_eq!(screen.auto_wrap(), self.auto_wrap);
        assert_eq!(screen.origin_mode(), self.origin_mode);
        assert_eq!(screen.insert_mode(), self.insert_mode);
        assert!(!self.origin_mode || self.region.contains(&self.row));
        assert!(self.auto_wrap || self.column < self.storage.columns());
        for column in 0..self.storage.columns() {
            assert_eq!(
                screen.is_tab_stop(column),
                Some(self.tabs.contains(&column))
            );
        }
        assert!(self.row < self.storage.rows());
        assert!(self.column <= self.storage.columns());
        assert!(self.region.start < self.region.end && self.region.end <= self.storage.rows());
    }

    pub fn apply(&mut self, command: &Command<'_>, style: StyleId) -> Result<(), StorageError> {
        match command {
            Command::InsertMode(enabled) => self.insert_mode = *enabled,
            Command::Reset => {
                let columns = self.storage.columns();
                let rows = self.storage.rows();
                let mut reset = Self::new(columns, rows);
                for row in 0..rows {
                    reset.storage.clear_row(row, 0..columns, style)?;
                }
                *self = reset;
            }
            Command::SaveCursor => {
                self.saved = SavedCursor {
                    row: if self.origin_mode {
                        self.region.clone().position(|row| row == self.row).unwrap()
                    } else {
                        self.row
                    },
                    column: self.visible_column(),
                    pending_width: (self.column == self.storage.columns()).then_some(self.column),
                    auto_wrap: self.auto_wrap,
                    origin_mode: self.origin_mode,
                };
            }
            Command::RestoreCursor => {
                self.auto_wrap = self.saved.auto_wrap;
                self.origin_mode = self.saved.origin_mode;
                let mut rows = if self.origin_mode {
                    self.region.clone()
                } else {
                    0..self.storage.rows()
                };
                let last = rows.end - 1;
                let destination = rows.nth(self.saved.row);
                self.row = destination.unwrap_or(last);
                self.column = (0..self.storage.columns())
                    .nth(self.saved.column)
                    .unwrap_or(self.storage.columns() - 1);
                if self.auto_wrap
                    && destination.is_some()
                    && self.saved.pending_width == Some(self.storage.columns())
                {
                    self.column = self.storage.columns();
                }
            }
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
                let mut rows = if self.origin_mode {
                    self.region.clone()
                } else {
                    0..self.storage.rows()
                };
                let last = rows.end - 1;
                self.row = rows.nth(*row).unwrap_or(last);
                self.column = (*column).min(self.storage.columns() - 1);
            }
            Command::Region(region) => {
                if region.start >= region.end || region.end > self.storage.rows() {
                    return Err(StorageError::OutOfBounds);
                }
                self.region = region.clone();
                self.row = if self.origin_mode { region.start } else { 0 };
                self.column = 0;
            }
            Command::Resize(columns, rows) => {
                if *columns == self.storage.columns() && *rows == self.storage.rows() {
                    return Ok(());
                }
                let old_rows = self.storage.rows();
                let old_columns = self.storage.columns();
                let column = self.visible_column();
                self.storage.resize(*columns, *rows, style)?;
                self.tabs.retain(|column| column < columns);
                if *columns > old_columns {
                    self.tabs
                        .extend((old_columns..*columns).filter(|column| column.is_multiple_of(8)));
                }
                self.row = self.row.min(rows - 1);
                self.column = column.min(columns - 1);
                if *rows != old_rows {
                    self.region = 0..*rows;
                }
            }
            Command::CarriageReturn => self.column = 0,
            Command::Backspace => self.column = self.visible_column().saturating_sub(1),
            Command::LineFeed => {
                self.storage.set_wrap(self.row, None);
                self.index(true, style);
            }
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
            Command::AutoWrap(enabled) => {
                self.auto_wrap = *enabled;
                if !enabled {
                    self.cancel_wrap();
                }
            }
            Command::OriginMode(enabled) => {
                self.origin_mode = *enabled;
                self.row = if *enabled { self.region.start } else { 0 };
                self.column = 0;
            }
            Command::SetTab => {
                self.tabs.insert(self.visible_column());
            }
            Command::ClearTab => {
                self.tabs.remove(&self.visible_column());
            }
            Command::ClearTabs => self.tabs.clear(),
            Command::TabForward(count) | Command::TabBackward(count) => {
                if *count != 0 {
                    let column = self.visible_column();
                    let rank = (*count).min(self.storage.columns()) - 1;
                    self.column = if matches!(command, Command::TabForward(_)) {
                        self.tabs
                            .range(column + 1..)
                            .nth(rank)
                            .copied()
                            .unwrap_or(self.storage.columns() - 1)
                    } else {
                        self.tabs
                            .range(..column)
                            .rev()
                            .nth(rank)
                            .copied()
                            .unwrap_or(0)
                    };
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
        if !self.auto_wrap {
            if self.column + width <= columns {
                let cell_width = if width == 1 {
                    CellWidth::Single
                } else {
                    CellWidth::Double
                };
                if self.insert_mode {
                    self.storage
                        .edit(self.row, self.column, width, true, style)?;
                }
                self.storage
                    .write(self.row, self.column, text, cell_width, style)?;
                self.column = (self.column + width).min(columns - 1);
            }
            return Ok(());
        }
        let mut connection = None;
        if self.column + width > columns {
            let source = self.row;
            let end = self.column.min(columns);
            let scrolling = self.row + 1 == self.region.end;
            if self.column < columns {
                self.storage
                    .clear_row(self.row, self.column..columns, style)?;
            }
            self.index(true, style);
            connection = if scrolling && self.region.len() > 1 {
                Some((self.row - 1, end))
            } else if source != self.row {
                Some((source, end))
            } else {
                None
            };
            self.column = 0;
        }
        let cell_width = if width == 1 {
            CellWidth::Single
        } else {
            CellWidth::Double
        };
        if self.insert_mode {
            self.storage
                .edit(self.row, self.column, width, true, style)?;
        }
        self.storage
            .write(self.row, self.column, text, cell_width, style)?;
        if let Some((row, end)) = connection {
            self.storage.set_wrap(row, Some(end));
        }
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
