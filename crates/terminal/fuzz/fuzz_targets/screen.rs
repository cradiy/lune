#![no_main]

#[allow(
    dead_code,
    reason = "Shared oracle operations serve different fuzz targets."
)]
mod model;
mod screen_model;

use std::mem::discriminant;

use libfuzzer_sys::fuzz_target;
use lune_terminal::{
    CellPosition, CellWidth, CursorShape, EraseMode, MAX_CELL_TEXT_BYTES, MAX_DIMENSION, Screen,
    ScreenBuffer, StorageError, StyleId,
};
use screen_model::{Command, ScreenModel};

const TEXT: [&str; 8] = [
    "x",
    "水",
    "e\u{301}",
    "👩\u{200d}💻",
    "",
    "\u{10ffff}",
    "\u{301}",
    "ab",
];
const ASCII: [&[u8]; 8] = [
    b"abc",
    b"~",
    b"",
    b"a\nb",
    b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ!?",
    b"a\0b",
    b"\xff",
    b" ",
];

fn coordinate(byte: u8, length: usize) -> usize {
    match byte {
        252 => length - 1,
        253 => length,
        254 => 0,
        255 => usize::MAX,
        _ => byte as usize % (length + 2),
    }
}

fn dimension(byte: u8) -> usize {
    if byte == 255 {
        MAX_DIMENSION + 1
    } else {
        byte as usize % 17
    }
}

fn column_dimension(byte: u8) -> usize {
    match byte {
        249 => 63,
        250 => 64,
        251 => 65,
        252 => 127,
        253 => 128,
        254 => 129,
        _ => dimension(byte),
    }
}

fn apply(screen: &mut Screen, command: &Command<'_>, style: StyleId) -> Result<(), StorageError> {
    match command {
        Command::Write(text, width) => return screen.write(text, *width, style),
        Command::Revise(position, text, width) => {
            return screen
                .revise_unit(*position, text, *width, style)
                .map(|_| ());
        }
        Command::Ascii(bytes) => return screen.write_ascii(bytes, style),
        Command::Cursor(row, column) => screen.set_cursor(*row, *column),
        Command::Region(region) => return screen.set_scroll_region(region.clone()),
        Command::Resize(columns, rows) => return screen.resize(*columns, *rows, style),
        Command::CarriageReturn => screen.carriage_return(),
        Command::Backspace => screen.backspace(),
        Command::LineFeed => screen.line_feed(style),
        Command::ReverseIndex => screen.reverse_index(style),
        Command::EraseLine(mode) => screen.erase_line(*mode, style),
        Command::EraseDisplay(mode) => screen.erase_display(*mode, style),
        Command::InsertCells(count) => screen.insert_cells(*count, style),
        Command::DeleteCells(count) => screen.delete_cells(*count, style),
        Command::EraseCells(count) => screen.erase_cells(*count, style),
        Command::InsertLines(count) => screen.insert_lines(*count, style),
        Command::DeleteLines(count) => screen.delete_lines(*count, style),
        Command::MoveUp(count) => screen.move_cursor_up(*count),
        Command::MoveDown(count) => screen.move_cursor_down(*count),
        Command::MoveLeft(count) => screen.move_cursor_left(*count),
        Command::MoveRight(count) => screen.move_cursor_right(*count),
        Command::ScrollUp(count) => screen.scroll_up(*count, style),
        Command::ScrollDown(count) => screen.scroll_down(*count, style),
        Command::TabForward(count) => screen.tab_forward(*count),
        Command::TabBackward(count) => screen.tab_backward(*count),
        Command::SetTab => screen.set_tab_stop(),
        Command::ClearTab => screen.clear_tab_stop(),
        Command::ClearTabs => screen.clear_tab_stops(),
        Command::AutoWrap(enabled) => screen.set_auto_wrap(*enabled),
        Command::OriginMode(enabled) => screen.set_origin_mode(*enabled),
        Command::SaveCursor => screen.save_cursor(),
        Command::RestoreCursor => screen.restore_cursor(),
        Command::Reset => screen.reset(style),
        Command::InsertMode(enabled) => screen.set_insert_mode(*enabled),
        Command::SelectBuffer(alternate) => screen.select_buffer(if *alternate {
            ScreenBuffer::Alternate
        } else {
            ScreenBuffer::Primary
        }),
        Command::CursorVisible(visible) => {
            let mut appearance = screen.cursor_appearance();
            appearance.visible = *visible;
            screen.set_cursor_appearance(appearance);
        }
        Command::CursorBlink(blinking) => {
            let mut appearance = screen.cursor_appearance();
            appearance.blinking = *blinking;
            screen.set_cursor_appearance(appearance);
        }
        Command::CursorShape(shape, blinking) => {
            let mut appearance = screen.cursor_appearance();
            appearance.shape = *shape;
            appearance.blinking = *blinking;
            screen.set_cursor_appearance(appearance);
        }
    }
    Ok(())
}

fuzz_target!(|data: &[u8]| {
    let mut screen = Screen::new(8, 4).unwrap();
    let mut model = ScreenModel::new(8, 4);
    model.check(&screen);
    let long_text = "x".repeat(MAX_CELL_TEXT_BYTES + 1);
    for bytes in data.as_chunks::<8>().0.iter().take(512) {
        let rows = screen.grid().rows();
        let columns = screen.grid().columns();
        let row = coordinate(bytes[1], rows);
        let column = coordinate(bytes[2], columns);
        let count = if bytes[3] == 255 {
            usize::MAX
        } else {
            bytes[3] as usize
        };
        let style = StyleId(u32::from_le_bytes(bytes[4..8].try_into().unwrap()));
        let mode = match bytes[3] % 3 {
            0 => EraseMode::FromCursor,
            1 => EraseMode::ToCursor,
            _ => EraseMode::All,
        };
        let operation = bytes[0] % 40;
        if operation == 34 {
            let range = if bytes[7] & 1 != 0 {
                CellPosition { row: 0, column: 0 }..CellPosition {
                    row: rows - 1,
                    column: columns,
                }
            } else {
                CellPosition { row, column }..CellPosition {
                    row: coordinate(bytes[5], rows),
                    column: coordinate(bytes[6], columns),
                }
            };
            match (screen.grid().text(range.clone()), model.text(range)) {
                (Ok(actual), Ok(expected)) => assert_eq!(actual.to_string(), expected),
                (Err(actual), Err(expected)) => {
                    assert_eq!(discriminant(&actual), discriminant(&expected))
                }
                results => panic!("Text extraction differs from model: {results:?}"),
            }
            model.check(&screen);
            continue;
        }
        let command = match operation {
            0 | 39 => {
                let text = match bytes[4] % 10 {
                    8 => &long_text[..MAX_CELL_TEXT_BYTES],
                    9 => &long_text,
                    index => TEXT[index as usize],
                };
                let width = if bytes[3] & 1 == 0 {
                    CellWidth::Single
                } else {
                    CellWidth::Double
                };
                if operation == 39 {
                    Command::Revise(CellPosition { row, column }, text, width)
                } else {
                    Command::Write(text, width)
                }
            }
            1 => Command::Ascii(ASCII[bytes[4] as usize % ASCII.len()]),
            2 => Command::Cursor(row, column),
            3 => Command::Region(row..coordinate(bytes[2], rows)),
            4 => Command::Resize(column_dimension(bytes[1]), dimension(bytes[2])),
            5 => Command::CarriageReturn,
            6 => Command::Backspace,
            7 => Command::LineFeed,
            8 => Command::ReverseIndex,
            9 => Command::EraseLine(mode),
            10 => Command::EraseDisplay(mode),
            11 => Command::InsertCells(count),
            12 => Command::DeleteCells(count),
            13 => Command::EraseCells(count),
            14 => Command::InsertLines(count),
            15 => Command::DeleteLines(count),
            16 => Command::Resize(columns, rows),
            17 => Command::MoveUp(count),
            18 => Command::MoveDown(count),
            19 => Command::MoveLeft(count),
            20 => Command::MoveRight(count),
            21 => Command::ScrollUp(count),
            22 => Command::ScrollDown(count),
            23 => Command::TabForward(count),
            24 => Command::TabBackward(count),
            25 => Command::SetTab,
            26 => Command::ClearTab,
            27 => Command::ClearTabs,
            28 => Command::AutoWrap(bytes[3] & 1 != 0),
            29 => Command::OriginMode(bytes[3] & 1 != 0),
            30 => Command::SaveCursor,
            31 => Command::RestoreCursor,
            32 => Command::Reset,
            33 => Command::InsertMode(bytes[3] & 1 != 0),
            35 => Command::CursorVisible(bytes[3] & 1 != 0),
            36 => Command::CursorBlink(bytes[3] & 1 != 0),
            37 => Command::CursorShape(
                [CursorShape::Block, CursorShape::Underline, CursorShape::Bar]
                    [bytes[1] as usize % 3],
                bytes[3] & 1 != 0,
            ),
            _ => Command::SelectBuffer(bytes[3] & 1 != 0),
        };
        match (
            apply(&mut screen, &command, style),
            model.apply(&command, style),
        ) {
            (Ok(()), Ok(())) => {}
            (Err(actual), Err(expected)) => {
                assert_eq!(discriminant(&actual), discriminant(&expected))
            }
            results => panic!("Screen result differs from model: {results:?}"),
        }
        model.check(&screen);
    }
    screen.restore_cursor();
    model.apply(&Command::RestoreCursor, StyleId(0)).unwrap();
    model.check(&screen);
});
