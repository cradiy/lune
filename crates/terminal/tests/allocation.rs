#[path = "support/allocator.rs"]
mod allocator;

use std::hint::black_box;

use lune_terminal::{CellText, CellWidth, Grid, Screen, StorageError, StyleId};

#[global_allocator]
static ALLOCATOR: allocator::TestAllocator = allocator::TestAllocator;

#[derive(Debug, PartialEq, Eq)]
struct SnapshotCell {
    text: String,
    width: usize,
    style: StyleId,
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    columns: usize,
    rows: Vec<Vec<SnapshotCell>>,
}

fn snapshot(grid: &Grid) -> Snapshot {
    let rows = grid
        .iter_rows()
        .map(|row| {
            assert_eq!(row.cells().len(), grid.columns());
            row.cells()
                .iter()
                .enumerate()
                .map(|(column, cell)| {
                    if cell.is_continuation() {
                        assert!(column > 0);
                        assert_eq!(row.cells()[column - 1].width(), 2);
                        assert_eq!(row.cells()[column - 1].style(), cell.style());
                    } else if cell.width() == 2 {
                        assert!(column + 1 < grid.columns());
                        assert!(row.cells()[column + 1].is_continuation());
                    }
                    let text = match row.text(column).unwrap() {
                        CellText::Scalar(character) => character.to_string(),
                        CellText::Extended(text) => text.to_owned(),
                        CellText::Continuation => String::new(),
                    };
                    SnapshotCell {
                        text,
                        width: cell.width(),
                        style: cell.style(),
                    }
                })
                .collect()
        })
        .collect();
    Snapshot {
        columns: grid.columns(),
        rows,
    }
}

fn populated() -> Grid {
    let mut grid = Grid::new(4, 3).unwrap();
    grid.write(0, 0, "水", CellWidth::Double, StyleId(1))
        .unwrap();
    grid.write(1, 2, "界\u{301}", CellWidth::Double, StyleId(2))
        .unwrap();
    grid.write_ascii(2, 0, b"last", StyleId(3)).unwrap();
    grid
}

fn extended() -> Grid {
    let mut grid = Grid::new(6, 3).unwrap();
    grid.write(0, 0, "e\u{301}", CellWidth::Single, StyleId(1))
        .unwrap();
    grid.write(0, 1, "水\u{301}", CellWidth::Double, StyleId(2))
        .unwrap();
    grid
}

fn dense() -> Grid {
    let mut grid = Grid::new(32, 2).unwrap();
    for column in 0..16 {
        grid.write(0, column, "e\u{301}", CellWidth::Single, StyleId(1))
            .unwrap();
    }
    grid
}

struct Scenario {
    name: &'static str,
    setup: fn() -> Grid,
    mutate: fn(&mut Grid) -> Result<(), StorageError>,
}

#[test]
fn screen_failures_preserve_contents_cursor_and_pending_wrap() {
    fn pending(rows: usize, gap: bool) -> Screen {
        let mut screen = Screen::new(4, rows).unwrap();
        screen.write_ascii(b"top!", StyleId(1)).unwrap();
        screen.set_cursor(rows - 1, 2).unwrap();
        screen.write("旧", CellWidth::Double, StyleId(2)).unwrap();
        if gap {
            screen.set_cursor(rows - 1, 3).unwrap();
        }
        screen
    }

    type Case = (
        &'static str,
        fn() -> Screen,
        fn(&mut Screen) -> Result<(), StorageError>,
    );
    fn regional(region: std::ops::Range<usize>, gap: bool) -> Screen {
        let mut screen = pending(4, false);
        screen.set_scroll_region(region.clone()).unwrap();
        screen.set_cursor(region.end - 1, 2).unwrap();
        screen.write("旧", CellWidth::Double, StyleId(2)).unwrap();
        if gap {
            screen.set_cursor(region.end - 1, 3).unwrap();
        }
        screen
    }
    let cases: [Case; 10] = [
        (
            "write within row",
            || Screen::new(4, 2).unwrap(),
            |screen| screen.write("e\u{301}", CellWidth::Single, StyleId(3)),
        ),
        (
            "wrap into next row",
            || {
                let mut screen = pending(2, false);
                screen.set_cursor(0, 3).unwrap();
                screen.write("x", CellWidth::Single, StyleId(1)).unwrap();
                screen
            },
            |screen| screen.write("水\u{301}", CellWidth::Double, StyleId(3)),
        ),
        (
            "wrap and scroll",
            || pending(2, false),
            |screen| screen.write("水\u{301}", CellWidth::Double, StyleId(3)),
        ),
        (
            "wide edge and scroll",
            || pending(2, true),
            |screen| screen.write("水\u{301}", CellWidth::Double, StyleId(3)),
        ),
        (
            "single-row wrap",
            || pending(1, false),
            |screen| screen.write("水\u{301}", CellWidth::Double, StyleId(3)),
        ),
        (
            "grow recycled text",
            || {
                let mut screen = pending(2, false);
                screen.set_cursor(0, 0).unwrap();
                screen
                    .write("e\u{301}", CellWidth::Single, StyleId(1))
                    .unwrap();
                screen.set_cursor(1, 3).unwrap();
                screen.write("x", CellWidth::Single, StyleId(1)).unwrap();
                screen
            },
            |screen| {
                screen.write(
                    "e\u{301}\u{302}\u{303}\u{304}",
                    CellWidth::Double,
                    StyleId(3),
                )
            },
        ),
        (
            "resize with pending wrap",
            || regional(1..3, false),
            |screen| screen.resize(12, 6, StyleId(3)),
        ),
        (
            "regional wrap",
            || regional(1..3, false),
            |screen| screen.write("水\u{301}", CellWidth::Double, StyleId(3)),
        ),
        (
            "regional wide edge",
            || regional(1..3, true),
            |screen| screen.write("水\u{301}", CellWidth::Double, StyleId(3)),
        ),
        (
            "single-row region",
            || regional(1..2, false),
            |screen| screen.write("水\u{301}", CellWidth::Double, StyleId(3)),
        ),
    ];
    for (name, setup, mutate) in cases {
        let mut baseline = setup();
        let (result, measured) = allocator::measure(None, || mutate(&mut baseline));
        result.unwrap();
        assert!(measured.attempts > 0, "{name}");
        let expected = (
            snapshot(baseline.grid()),
            baseline.cursor(),
            baseline.scroll_region(),
        );
        for point in 0..measured.attempts {
            let mut screen = setup();
            let before = (
                snapshot(screen.grid()),
                screen.cursor(),
                screen.scroll_region(),
            );
            let (result, failed) = allocator::measure(Some(point), || mutate(&mut screen));
            assert!(
                matches!(result, Err(StorageError::Allocation(_))),
                "{name} at {point}: {result:?}"
            );
            assert_eq!(failed.failures, 1);
            assert_eq!(
                (
                    snapshot(screen.grid()),
                    screen.cursor(),
                    screen.scroll_region()
                ),
                before,
                "{name} at {point}"
            );
            mutate(&mut screen).unwrap();
            assert_eq!(
                (
                    snapshot(screen.grid()),
                    screen.cursor(),
                    screen.scroll_region()
                ),
                expected,
                "{name} retry at {point}"
            );
        }
        eprintln!("{name}: {} failure points covered", measured.attempts);
    }
}

#[test]
fn allocation_failures_preserve_contents_and_allow_retry() {
    let scenarios = [
        Scenario {
            name: "write new extended text over a continuation",
            setup: populated,
            mutate: |grid| grid.write(0, 1, "e\u{301}", CellWidth::Single, StyleId(4)),
        },
        Scenario {
            name: "grow existing text while replacing a neighboring wide unit",
            setup: extended,
            mutate: |grid| {
                grid.write(
                    0,
                    0,
                    "e\u{301}\u{302}\u{303}\u{304}",
                    CellWidth::Double,
                    StyleId(4),
                )
            },
        },
        Scenario {
            name: "grow the extended-text index",
            setup: dense,
            mutate: |grid| grid.write(0, 16, "e\u{301}", CellWidth::Single, StyleId(4)),
        },
        Scenario {
            name: "append first extended text through a continuation",
            setup: populated,
            mutate: |grid| grid.append_text(0, 1, "\u{301}"),
        },
        Scenario {
            name: "grow appended text",
            setup: extended,
            mutate: |grid| grid.append_text(0, 0, "\u{301}\u{302}\u{303}\u{304}"),
        },
        Scenario {
            name: "grow rows and columns",
            setup: populated,
            mutate: |grid| grid.resize(12, 6, StyleId(4)),
        },
        Scenario {
            name: "grow columns before discarding bottom rows",
            setup: populated,
            mutate: |grid| grid.resize(12, 1, StyleId(4)),
        },
        Scenario {
            name: "allocate new rows before cropping a wide unit",
            setup: populated,
            mutate: |grid| grid.resize(3, 6, StyleId(4)),
        },
    ];

    for scenario in scenarios {
        let mut baseline = (scenario.setup)();
        let (result, measured) =
            allocator::measure(None, || (scenario.mutate)(black_box(&mut baseline)));
        result.unwrap();
        assert!(
            measured.attempts > 0,
            "{} did not exercise allocation",
            scenario.name
        );
        let expected = snapshot(&baseline);

        for point in 0..measured.attempts {
            let mut grid = (scenario.setup)();
            let before = snapshot(&grid);
            let (result, failed) =
                allocator::measure(Some(point), || (scenario.mutate)(black_box(&mut grid)));
            assert!(
                matches!(result, Err(StorageError::Allocation(_))),
                "{} at {point}: {result:?}",
                scenario.name
            );
            assert_eq!(failed.failures, 1, "{} at {point}", scenario.name);
            assert_eq!(failed.attempts, point + 1, "{} at {point}", scenario.name);
            assert_eq!(snapshot(&grid), before, "{} at {point}", scenario.name);

            (scenario.mutate)(&mut grid).unwrap();
            assert_eq!(
                snapshot(&grid),
                expected,
                "{} retry at {point}",
                scenario.name
            );
            grid.write_ascii(0, 0, b"Z", StyleId(5)).unwrap();
            grid.append_text(0, 0, "\u{301}").unwrap();
            grid.resize(6, 4, StyleId(5)).unwrap();
            snapshot(&grid);
        }
        eprintln!(
            "{}: {} failure points covered",
            scenario.name, measured.attempts
        );
    }
}

#[test]
fn warm_writes_edits_scrolling_and_column_growth_make_no_allocation_requests() {
    let mut grid = Grid::new(240, 80).unwrap();
    grid.resize(120, 80, StyleId(0)).unwrap();
    let ascii = [b'x'; 120];
    let (result, stats) = allocator::measure(Some(0), || {
        for _ in 0..32 {
            grid.write_ascii(0, 0, black_box(&ascii), StyleId(1))?;
            grid.write(1, 0, "水", CellWidth::Double, StyleId(2))?;
            grid.insert_cells(0, 17, 1, StyleId(0))?;
            grid.delete_cells(0, 17, 1, StyleId(0))?;
            grid.scroll_up(0..80, 17, StyleId(0))?;
            grid.scroll_down(1..79, 1, StyleId(0))?;
            grid.scroll_up(1..79, 17, StyleId(0))?;
            grid.scroll_down(0..80, 1, StyleId(0))?;
            grid.scroll_up(2..78, usize::MAX, StyleId(0))?;
            grid.clear_row(0, 0..120, StyleId(0))?;
            grid.resize(240, 80, StyleId(0))?;
            grid.resize(120, 80, StyleId(0))?;
        }
        Ok::<_, StorageError>(())
    });
    result.unwrap();
    assert_eq!(stats.attempts, 0);
    assert_eq!(stats.live_bytes, 0);
    snapshot(&grid);

    let mut grid = Grid::new(4, 9).unwrap();
    for row in 0..9 {
        grid.write(row, 1, "水\u{301}", CellWidth::Double, StyleId(2))
            .unwrap();
    }
    grid.scroll_up(0..9, 3, StyleId(0)).unwrap();
    let (result, stats) = allocator::measure(Some(0), || {
        for row in 0..6 {
            grid.insert_cells(row, 0, 1, StyleId(0))?;
            grid.delete_cells(row, 0, 1, StyleId(0))?;
        }
        grid.insert_cells(0, 2, 1, StyleId(0))?;
        grid.delete_cells(1, 1, 1, StyleId(0))?;
        grid.scroll_down(1..8, 2, StyleId(0))?;
        grid.scroll_up(1..8, 3, StyleId(0))?;
        grid.scroll_down(1..8, 4, StyleId(0))
    });
    result.unwrap();
    assert_eq!(stats.attempts, 0);
    assert!(stats.live_bytes <= 0);
    snapshot(&grid);

    let mut screen = Screen::new(240, 24).unwrap();
    screen.resize(120, 24, StyleId(0)).unwrap();
    let (result, stats) = allocator::measure(Some(0), || {
        for _ in 0..64 {
            screen.set_scroll_region(2..22)?;
            screen.set_cursor(21, 119)?;
            screen.write_ascii(&[b'x'; 240], StyleId(1))?;
            screen.backspace();
            screen.write("水", CellWidth::Double, StyleId(2))?;
            screen.carriage_return();
            screen.line_feed(StyleId(0));
            screen.set_cursor(2, 0)?;
            screen.reverse_index(StyleId(0));
            screen.set_scroll_region(0..24)?;
            screen.set_cursor(23, 0)?;
            screen.write_ascii(&[b'x'; 240], StyleId(1))?;
            screen.resize(240, 24, StyleId(0))?;
            screen.resize(120, 24, StyleId(0))?;
        }
        Ok::<_, StorageError>(())
    });
    result.unwrap();
    assert_eq!(stats.attempts, 0);
    assert_eq!(stats.live_bytes, 0);
    snapshot(screen.grid());
}

#[test]
fn resize_history_reaches_a_stable_requested_memory_high_water() {
    let ((initial, retained, peaks), released) = allocator::measure(None, || {
        let mut grid = Grid::new(128, 8).unwrap();
        let initial = allocator::stats().live_bytes;
        let mut retained = [0; 16];
        let mut peaks = [0; 16];
        for cycle in 0..16 {
            for (columns, rows) in [
                (128, 8),
                (64, 16),
                (32, 32),
                (16, 64),
                (8, 128),
                (4, 256),
                (2, 512),
                (1, 1024),
            ] {
                grid.resize(columns, rows, StyleId(0)).unwrap();
            }
            grid.resize(128, 8, StyleId(0)).unwrap();
            grid.write(0, 0, "水\u{301}", CellWidth::Double, StyleId(1))
                .unwrap();
            grid.append_text(0, 1, "\u{302}").unwrap();
            let measured = allocator::stats();
            retained[cycle] = measured.live_bytes;
            peaks[cycle] = measured.peak_bytes;
        }
        drop(grid);
        (initial, retained, peaks)
    });

    assert!(initial > 0);
    assert!(retained[0] >= initial);
    assert!(retained.iter().all(|&bytes| bytes == retained[0]));
    assert!(peaks[1..].iter().all(|&bytes| bytes == peaks[1]));
    assert_eq!(released.live_bytes, 0);
    eprintln!(
        "Requested heap bytes: initial={initial}, retained={}, peak={}, after_drop={}",
        retained[0], released.peak_bytes, released.live_bytes
    );
}
