#![no_main]

mod model;

use std::mem::discriminant;

use libfuzzer_sys::fuzz_target;
use lune_terminal::{CellWidth, Grid, MAX_DIMENSION, StyleId};
use model::Model;

const TEXT: [&str; 8] = [
    "x",
    "水",
    "e\u{301}",
    "👩\u{200d}💻",
    "",
    "\u{10ffff}",
    "\u{301}",
    "abcdefghijklmnopqrstuvwxyz012345",
];
const ASCII: [&[u8]; 4] = [b"abc", b"~", b"", b"a\nb"];

fn count(byte: u8) -> usize {
    if byte == u8::MAX {
        usize::MAX
    } else {
        byte as usize
    }
}

fn dimension(byte: u8) -> usize {
    if byte == u8::MAX {
        MAX_DIMENSION + 1
    } else {
        byte as usize % 17
    }
}

fuzz_target!(|data: &[u8]| {
    let mut grid = Grid::new(8, 4).unwrap();
    let mut model = Model::new(8, 4);
    for bytes in data.as_chunks::<8>().0.iter().take(512) {
        let row = bytes[1] as usize % (model.rows() + 2);
        let column = if bytes[2] == u8::MAX {
            usize::MAX
        } else {
            bytes[2] as usize % (model.columns() + 2)
        };
        let count = count(bytes[3]);
        let style = StyleId(u32::from_le_bytes(bytes[4..8].try_into().unwrap()));
        let text = TEXT[bytes[4] as usize % TEXT.len()];
        let (actual, expected) = match bytes[0] % 11 {
            0 => {
                let width = if bytes[3] & 1 == 0 {
                    CellWidth::Single
                } else {
                    CellWidth::Double
                };
                (
                    grid.write(row, column, text, width, style),
                    model.write(row, column, text, width, style),
                )
            }
            1 => {
                let text = ASCII[bytes[4] as usize % ASCII.len()];
                (
                    grid.write_ascii(row, column, text, style),
                    model.ascii(row, column, text, style),
                )
            }
            2 => (
                grid.append_text(row, column, text),
                model.append(row, column, text),
            ),
            3 => {
                let end = bytes[3] as usize % (grid.columns() + 2);
                (
                    grid.clear_row(row, column..end, style),
                    model.clear_row(row, column..end, style),
                )
            }
            4 => (
                grid.insert_cells(row, column, count, style),
                model.edit(row, column, count, true, style),
            ),
            5 => (
                grid.delete_cells(row, column, count, style),
                model.edit(row, column, count, false, style),
            ),
            6 | 7 => {
                let up = bytes[0] % 11 == 6;
                let end = bytes[2] as usize % (grid.rows() + 2);
                let actual = if up {
                    grid.scroll_up(row..end, count, style)
                } else {
                    grid.scroll_down(row..end, count, style)
                };
                (actual, model.scroll(row..end, count, up, style))
            }
            8 => {
                let columns = dimension(bytes[1]);
                let rows = dimension(bytes[2]);
                (
                    grid.resize(columns, rows, style),
                    model.resize(columns, rows, style),
                )
            }
            9 => {
                grid.clear(style);
                model.clear(style);
                (Ok(()), Ok(()))
            }
            _ => {
                let up = bytes[4] & 1 == 0;
                let end = grid.rows();
                let actual = if up {
                    grid.scroll_up(0..end, count, style)
                } else {
                    grid.scroll_down(0..end, count, style)
                };
                (actual, model.scroll(0..end, count, up, style))
            }
        };
        match (actual, expected) {
            (Ok(()), Ok(())) => {}
            (Err(actual), Err(expected)) => {
                assert_eq!(discriminant(&actual), discriminant(&expected))
            }
            results => panic!("Storage result differs from model: {results:?}"),
        }
        model.check(&grid);
    }
});
