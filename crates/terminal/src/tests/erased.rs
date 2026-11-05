use crate::{CellPosition, CellText, CellWidth, Grid, Processor, Screen, StyleId};

fn erased(grid: &Grid, row: usize) -> Vec<bool> {
    grid.row(row)
        .unwrap()
        .cells()
        .iter()
        .map(|cell| cell.is_erased())
        .collect()
}

#[test]
fn erased_state_survives_cell_edits_cropping_and_scrolling_as_part_of_the_unit() {
    let mut grid = Grid::new(6, 2).unwrap();
    assert_eq!(erased(&grid, 0), [true; 6]);
    grid.write_ascii(0, 1, b" ", StyleId(0)).unwrap();
    grid.append_text(0, 0, "").unwrap();
    assert!(grid.row(0).unwrap().cells()[0].is_erased());
    grid.append_text(0, 0, "\u{301}").unwrap();
    assert!(!grid.row(0).unwrap().cells()[0].is_erased());
    assert_eq!(
        grid.row(0).unwrap().text(0),
        Some(CellText::Extended(" \u{301}"))
    );
    grid.insert_cells(0, 1, 2, StyleId(0)).unwrap();
    assert_eq!(erased(&grid, 0), [false, true, true, false, true, true]);
    grid.delete_cells(0, 0, 1, StyleId(0)).unwrap();
    assert_eq!(erased(&grid, 0), [true, true, false, true, true, true]);
    grid.write(0, 4, "水", CellWidth::Double, StyleId(0))
        .unwrap();
    grid.resize(5, 2, StyleId(0)).unwrap();
    assert_eq!(erased(&grid, 0), [true, true, false, true, true]);
    grid.resize(6, 3, StyleId(0)).unwrap();
    assert_eq!(erased(&grid, 0), [true, true, false, true, true, true]);
    assert_eq!(erased(&grid, 2), [true; 6]);
    grid.write_ascii(1, 1, b" ", StyleId(0)).unwrap();
    grid.scroll_up(0..2, 1, StyleId(0)).unwrap();
    assert_eq!(erased(&grid, 0), [true, false, true, true, true, true]);
    assert_eq!(erased(&grid, 1), [true; 6]);
    assert_ne!(
        grid.row(0).unwrap().cells()[0],
        grid.row(0).unwrap().cells()[1]
    );
    let view = grid
        .text(CellPosition { row: 0, column: 0 }..CellPosition { row: 0, column: 6 })
        .unwrap();
    assert_eq!(view.to_string(), "      ");
}

#[test]
fn styled_printed_spaces_and_wide_wrap_gaps_preserve_distinct_erasure_state() {
    let mut screen = Screen::new(4, 3).unwrap();
    let mut processor = Processor::default();
    processor
        .advance(b"\x1b[4;31m \x1b[K", &mut screen, |s, text, style| {
            s.write_ascii(text.as_bytes(), style)
        })
        .unwrap();
    let row = screen.grid().row(0).unwrap();
    assert_eq!(row.cells()[0].style(), row.cells()[1].style());
    assert!(!row.cells()[0].is_erased());
    assert!(row.cells()[1..].iter().all(|cell| cell.is_erased()));
    processor.reset(&mut screen);
    screen.write_ascii(b"abc", StyleId(0)).unwrap();
    screen.write("水", CellWidth::Double, StyleId(0)).unwrap();
    assert_eq!(screen.grid().row(0).unwrap().wrap_end(), Some(3));
    assert_eq!(erased(screen.grid(), 0), [false, false, false, true]);
    assert_eq!(erased(screen.grid(), 1), [false, false, true, true]);
    screen.set_cursor(0, 3);
    screen.write(" ", CellWidth::Single, StyleId(0)).unwrap();
    assert_eq!(screen.grid().row(0).unwrap().wrap_end(), None);
    assert_eq!(erased(screen.grid(), 0), [false; 4]);
}
