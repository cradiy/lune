use std::{collections::BTreeSet, ops::Range};

use lune_terminal::{
    CellPosition, CellWidth, CursorAppearance, CursorShape, EraseMode, MAX_CELL_TEXT_BYTES, Screen,
    ScreenBuffer, StorageError, StyleId,
};

use crate::model::Model;

pub enum Command<'a> {
    Write(&'a str, CellWidth),
    Revise(CellPosition, &'a str, CellWidth),
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
    SoftReset,
    InsertMode(bool),
    CursorVisible(bool),
    CursorBlink(bool),
    CursorShape(CursorShape, bool),
    SelectBuffer(bool),
}

#[derive(Clone, Copy)]
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
    storage: [Model; 2],
    active: usize,
    row: usize,
    column: usize,
    region: Range<usize>,
    tabs: BTreeSet<usize>,
    auto_wrap: bool,
    origin_mode: bool,
    saved: [SavedCursor; 2],
    insert_mode: bool,
    cursor_appearance: CursorAppearance,
}

impl ScreenModel {
    #[allow(
        dead_code,
        reason = "Unicode placement uses physical cursor and width bounds."
    )]
    pub fn placement(&self) -> (lune_terminal::Cursor, usize, bool) {
        (
            lune_terminal::Cursor {
                row: self.row,
                column: self.visible_column(),
                wrap_pending: self.column == self.storage[self.active].columns(),
            },
            self.storage[self.active].columns(),
            self.auto_wrap,
        )
    }

    #[allow(
        dead_code,
        reason = "Cursor reports are checked by the processor target."
    )]
    pub fn report_position(&self) -> (usize, usize) {
        let row = if self.origin_mode {
            self.region.clone().position(|row| row == self.row).unwrap()
        } else {
            self.row
        };
        (row + 1, self.visible_column() + 1)
    }

    pub fn is_alternate(&self) -> bool {
        self.active == 1
    }

    #[allow(
        dead_code,
        reason = "Mode queries are checked by the processor target."
    )]
    pub fn mode(&self, private: bool, selector: u16) -> Option<bool> {
        match (private, selector) {
            (false, 4) => Some(self.insert_mode),
            (true, 6) => Some(self.origin_mode),
            (true, 7) => Some(self.auto_wrap),
            (true, 12) => Some(self.cursor_appearance.blinking),
            (true, 25) => Some(self.cursor_appearance.visible),
            (true, 47 | 1047 | 1049) => Some(self.active == 1),
            _ => None,
        }
    }

    pub fn text(&self, range: Range<CellPosition>) -> Result<String, StorageError> {
        self.storage[self.active].text(range)
    }

    pub fn new(columns: usize, rows: usize) -> Self {
        Self {
            storage: [Model::new(columns, rows), Model::new(columns, rows)],
            active: 0,
            row: 0,
            column: 0,
            region: 0..rows,
            tabs: (0..columns).step_by(8).collect(),
            auto_wrap: true,
            origin_mode: false,
            insert_mode: false,
            cursor_appearance: CursorAppearance {
                visible: true,
                blinking: true,
                shape: CursorShape::Block,
            },
            saved: [SavedCursor {
                row: 0,
                column: 0,
                pending_width: None,
                auto_wrap: true,
                origin_mode: false,
            }; 2],
        }
    }

    pub fn check(&self, screen: &Screen) {
        self.check_styles(screen, |actual, expected| assert_eq!(actual, expected));
    }

    pub fn check_styles(&self, screen: &Screen, mut check: impl FnMut(StyleId, StyleId)) {
        self.storage[0].check_styles(screen.buffer(ScreenBuffer::Primary), &mut check);
        self.storage[1].check_styles(screen.buffer(ScreenBuffer::Alternate), &mut check);
        assert_eq!(
            screen.active_buffer() == ScreenBuffer::Alternate,
            self.is_alternate()
        );
        let cursor = screen.cursor();
        assert_eq!(cursor.row, self.row);
        assert_eq!(cursor.column, self.visible_column());
        assert_eq!(
            cursor.wrap_pending,
            self.column == self.storage[self.active].columns()
        );
        assert_eq!(screen.scroll_region(), self.region);
        assert_eq!(screen.auto_wrap(), self.auto_wrap);
        assert_eq!(screen.origin_mode(), self.origin_mode);
        assert_eq!(screen.insert_mode(), self.insert_mode);
        assert_eq!(screen.cursor_appearance(), self.cursor_appearance);
        assert!(!self.origin_mode || self.region.contains(&self.row));
        assert!(self.auto_wrap || self.column < self.storage[self.active].columns());
        for column in 0..self.storage[self.active].columns() {
            assert_eq!(
                screen.is_tab_stop(column),
                Some(self.tabs.contains(&column))
            );
        }
        assert!(self.row < self.storage[self.active].rows());
        assert!(self.column <= self.storage[self.active].columns());
        assert!(
            self.region.start < self.region.end
                && self.region.end <= self.storage[self.active].rows()
        );
    }

    pub fn apply(&mut self, command: &Command<'_>, style: StyleId) -> Result<(), StorageError> {
        match command {
            Command::SelectBuffer(alternate) => self.active = usize::from(*alternate),
            Command::CursorVisible(visible) => self.cursor_appearance.visible = *visible,
            Command::CursorBlink(blinking) => self.cursor_appearance.blinking = *blinking,
            Command::CursorShape(shape, blinking) => {
                self.cursor_appearance.shape = *shape;
                self.cursor_appearance.blinking = *blinking;
            }
            Command::InsertMode(enabled) => self.insert_mode = *enabled,
            Command::SoftReset => {
                self.column = self.visible_column();
                self.region = 0..self.storage[self.active].rows();
                self.auto_wrap = true;
                self.origin_mode = false;
                self.insert_mode = false;
                self.cursor_appearance = CursorAppearance::default();
                self.saved[self.active] = SavedCursor {
                    row: 0,
                    column: 0,
                    pending_width: None,
                    auto_wrap: true,
                    origin_mode: false,
                };
            }
            Command::Reset => {
                let columns = self.storage[self.active].columns();
                let rows = self.storage[self.active].rows();
                let mut reset = Self::new(columns, rows);
                for buffer in &mut reset.storage {
                    for row in 0..rows {
                        buffer.clear_row(row, 0..columns, style)?;
                    }
                }
                *self = reset;
            }
            Command::SaveCursor => {
                self.saved[self.active] = SavedCursor {
                    row: if self.origin_mode {
                        self.region.clone().position(|row| row == self.row).unwrap()
                    } else {
                        self.row
                    },
                    column: self.visible_column(),
                    pending_width: (self.column == self.storage[self.active].columns())
                        .then_some(self.column),
                    auto_wrap: self.auto_wrap,
                    origin_mode: self.origin_mode,
                };
            }
            Command::RestoreCursor => {
                self.auto_wrap = self.saved[self.active].auto_wrap;
                self.origin_mode = self.saved[self.active].origin_mode;
                let mut rows = if self.origin_mode {
                    self.region.clone()
                } else {
                    0..self.storage[self.active].rows()
                };
                let last = rows.end - 1;
                let destination = rows.nth(self.saved[self.active].row);
                self.row = destination.unwrap_or(last);
                self.column = (0..self.storage[self.active].columns())
                    .nth(self.saved[self.active].column)
                    .unwrap_or(self.storage[self.active].columns() - 1);
                if self.auto_wrap
                    && destination.is_some()
                    && self.saved[self.active].pending_width
                        == Some(self.storage[self.active].columns())
                {
                    self.column = self.storage[self.active].columns();
                }
            }
            Command::Write(text, width) => return self.write(text, *width, style),
            Command::Revise(position, text, width) => {
                return self.revise(*position, text, *width, style);
            }
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
                    0..self.storage[self.active].rows()
                };
                let last = rows.end - 1;
                self.row = rows.nth(*row).unwrap_or(last);
                self.column = (*column).min(self.storage[self.active].columns() - 1);
            }
            Command::Region(region) => {
                if region.start >= region.end || region.end > self.storage[self.active].rows() {
                    return Err(StorageError::OutOfBounds);
                }
                self.region = region.clone();
                self.row = if self.origin_mode { region.start } else { 0 };
                self.column = 0;
            }
            Command::Resize(columns, rows) => {
                if *columns == self.storage[self.active].columns()
                    && *rows == self.storage[self.active].rows()
                {
                    return Ok(());
                }
                let old_rows = self.storage[self.active].rows();
                let old_columns = self.storage[self.active].columns();
                let column = self.visible_column();
                self.storage[0].resize(*columns, *rows, style)?;
                self.storage[1].resize(*columns, *rows, style)?;
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
                self.storage[self.active].set_wrap(self.row, None);
                self.index(true, style);
            }
            Command::ReverseIndex => self.index(false, style),
            Command::EraseLine(mode) => self.erase(false, *mode, style),
            Command::EraseDisplay(mode) => self.erase(true, *mode, style),
            Command::InsertCells(count) | Command::DeleteCells(count) => {
                if *count != 0 {
                    self.storage[self.active].edit(
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
                    let end = column + (*count).min(self.storage[self.active].columns() - column);
                    self.storage[self.active].clear_row(self.row, column..end, style)?;
                    self.cancel_wrap();
                }
            }
            Command::InsertLines(count) | Command::DeleteLines(count) => {
                if *count != 0 && self.region.contains(&self.row) {
                    self.storage[self.active].scroll(
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
                    let rank = (*count).min(self.storage[self.active].columns()) - 1;
                    self.column = if matches!(command, Command::TabForward(_)) {
                        self.tabs
                            .range(column + 1..)
                            .nth(rank)
                            .copied()
                            .unwrap_or(self.storage[self.active].columns() - 1)
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
                self.storage[self.active].scroll(
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
                    for _ in 0..(*count).min(
                        self.storage[self.active]
                            .rows()
                            .max(self.storage[self.active].columns()),
                    ) {
                        match command {
                            Command::MoveUp(_) if self.row > 0 && self.row != self.region.start => {
                                self.row -= 1;
                            }
                            Command::MoveDown(_)
                                if self.row + 1 < self.storage[self.active].rows()
                                    && self.row + 1 != self.region.end =>
                            {
                                self.row += 1;
                            }
                            Command::MoveLeft(_) if self.column > 0 => self.column -= 1,
                            Command::MoveRight(_)
                                if self.column + 1 < self.storage[self.active].columns() =>
                            {
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

    fn revise(
        &mut self,
        position: CellPosition,
        text: &str,
        width: CellWidth,
        style: StyleId,
    ) -> Result<(), StorageError> {
        let storage = &mut self.storage[self.active];
        let previous = storage.unit_width(position)?;
        let next = if width == CellWidth::Single { 1 } else { 2 };
        if next > storage.columns()
            || (self.origin_mode && !self.region.contains(&position.row))
            || (!self.auto_wrap && position.column + next > storage.columns())
        {
            return Err(StorageError::OutOfBounds);
        }
        if text.is_empty() {
            return Err(StorageError::EmptyText);
        }
        if text.len() > MAX_CELL_TEXT_BYTES {
            return Err(StorageError::TextTooLong);
        }
        if position.column + next > storage.columns() {
            self.row = position.row;
            self.column = position.column;
            return self.write(text, width, style);
        }
        let join = storage.wrap(position.row);
        if self.insert_mode && next > previous {
            storage.edit(
                position.row,
                position.column + previous,
                next - previous,
                true,
                style,
            )?;
        }
        storage.write(position.row, position.column, text, width, style)?;
        if self.insert_mode && next < previous {
            storage.edit(
                position.row,
                position.column + next,
                previous - next,
                false,
                style,
            )?;
        }
        storage.set_wrap(position.row, if next == previous { join } else { None });
        self.row = position.row;
        self.column = if self.auto_wrap {
            position.column + next
        } else {
            (position.column + next).min(storage.columns() - 1)
        };
        Ok(())
    }

    fn write(&mut self, text: &str, width: CellWidth, style: StyleId) -> Result<(), StorageError> {
        let width = if width == CellWidth::Single { 1 } else { 2 };
        let columns = self.storage[self.active].columns();
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
                    self.storage[self.active].edit(self.row, self.column, width, true, style)?;
                }
                self.storage[self.active].write(self.row, self.column, text, cell_width, style)?;
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
                self.storage[self.active].clear_row(self.row, self.column..columns, style)?;
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
            self.storage[self.active].edit(self.row, self.column, width, true, style)?;
        }
        self.storage[self.active].write(self.row, self.column, text, cell_width, style)?;
        if let Some((row, end)) = connection {
            self.storage[self.active].set_wrap(row, Some(end));
        }
        self.column += width;
        Ok(())
    }

    fn index(&mut self, down: bool, style: StyleId) {
        if (down && self.row + 1 == self.region.end) || (!down && self.row == self.region.start) {
            self.storage[self.active]
                .scroll(self.region.clone(), 1, down, style)
                .unwrap();
        } else if down {
            self.row = (self.row + 1).min(self.storage[self.active].rows() - 1);
        } else {
            self.row = self.row.saturating_sub(1);
        }
        self.cancel_wrap();
    }

    fn erase(&mut self, display: bool, mode: EraseMode, style: StyleId) {
        let columns = self.storage[self.active].columns();
        let cursor = self.row * columns + self.visible_column();
        let scope = if display {
            0..self.storage[self.active].rows() * columns
        } else {
            self.row * columns..(self.row + 1) * columns
        };
        let range = match mode {
            EraseMode::FromCursor => cursor..scope.end,
            EraseMode::ToCursor => scope.start..cursor + 1,
            EraseMode::All => scope,
        };
        for row in 0..self.storage[self.active].rows() {
            let start = range.start.saturating_sub(row * columns).min(columns);
            let end = range.end.saturating_sub(row * columns).min(columns);
            self.storage[self.active]
                .clear_row(row, start..end, style)
                .unwrap();
        }
        self.cancel_wrap();
    }

    fn visible_column(&self) -> usize {
        self.column.min(self.storage[self.active].columns() - 1)
    }

    fn cancel_wrap(&mut self) {
        self.column = self.visible_column();
    }
}
