use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use lune_terminal::{Cell, CellWidth, Grid, StyleId};

fn measure(name: &str, units: usize, mut operation: impl FnMut()) {
    let mut samples = [0.0_f64; 3];
    for sample in &mut samples {
        let start = Instant::now();
        let mut iterations = 0_u64;
        while start.elapsed() < Duration::from_millis(200) {
            for _ in 0..64 {
                operation();
            }
            iterations += 64;
        }
        *sample = start.elapsed().as_nanos() as f64 / (iterations as f64 * units as f64);
    }
    samples.sort_by(f64::total_cmp);
    println!("{name:<34} {:>10.2} ns/unit", samples[1]);
}

fn main() {
    println!("Cell storage: {} bytes", size_of::<Cell>());
    let columns = 240;
    let ascii = vec![b'x'; columns];
    let mut grid = Grid::new(columns, 80).unwrap();
    measure("ASCII batch / cell", columns, || {
        black_box(&mut grid)
            .write_ascii(0, 0, black_box(&ascii), StyleId(1))
            .unwrap();
    });
    measure("scalar write / cell", columns, || {
        for column in 0..columns {
            black_box(&mut grid)
                .write(0, column, black_box("x"), CellWidth::Single, StyleId(1))
                .unwrap();
        }
    });
    for column in 0..columns {
        grid.write(0, column, "e\u{301}", CellWidth::Single, StyleId(1))
            .unwrap();
    }
    measure("dense extended rewrite / cell", columns, || {
        for column in 0..columns {
            black_box(&mut grid)
                .write(
                    0,
                    column,
                    black_box("e\u{301}"),
                    CellWidth::Single,
                    StyleId(1),
                )
                .unwrap();
        }
    });

    for rows in [24, 240, 4096] {
        let columns = 120;
        let mut grid = Grid::new(columns, rows).unwrap();
        let blank = grid.row(0).unwrap().cells()[0];
        let mut flat = vec![blank; columns * rows];
        measure(&format!("ring scroll / {rows} rows"), 1, || {
            black_box(&mut grid).scroll_up(1, StyleId(0));
        });
        measure(&format!("flat copy scroll / {rows} rows"), 1, || {
            let cells = black_box(&mut flat);
            cells.copy_within(columns.., 0);
            cells[(rows - 1) * columns..].fill(blank);
        });
    }
}
