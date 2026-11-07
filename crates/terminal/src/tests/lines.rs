use std::collections::VecDeque;

use crate::{CellText, CellWidth, HistoryLimits, Screen, ScreenBuffer, StyleId};

#[test]
fn logical_lines_partition_every_wrap_pattern_under_mixed_direction_traversal() {
    for mask in 0..128 {
        let mut screen = Screen::new(1, 8).unwrap();
        for row in 0..7 {
            if mask & (1 << row) != 0 {
                screen.set_cursor(row, 0);
                screen.write_ascii(b"xx", StyleId(1)).unwrap();
            }
        }
        let mut expected = Vec::new();
        let mut start = 0;
        for row in 0..8 {
            if row == 7 || mask & (1 << row) == 0 {
                expected.push(start..row + 1);
                start = row + 1;
            }
        }
        let view = screen.view();
        for row in 0..8 {
            let line = view.logical_line(row).unwrap();
            assert_eq!(
                line.range(),
                *expected.iter().find(|r| r.contains(&row)).unwrap()
            );
            assert_eq!(line.iter_rows().len(), line.range().len());
            assert!(std::ptr::eq(
                line.iter_rows().next().unwrap(),
                view.row(line.range().start).unwrap()
            ));
            assert!(std::ptr::eq(
                line.iter_rows().next_back().unwrap(),
                view.row(line.range().end - 1).unwrap()
            ));
        }
        assert!(view.logical_line(8).is_none());
        assert!(view.logical_line(usize::MAX).is_none());
        for directions in 0..1 << expected.len() {
            let mut remaining = VecDeque::from(expected.clone());
            let mut lines = view.logical_lines();
            for step in 0..expected.len() {
                let (lower, upper) = lines.size_hint();
                assert!(lower <= remaining.len() && upper.unwrap() >= remaining.len());
                let (actual, expected) = if directions & (1 << step) == 0 {
                    (lines.next(), remaining.pop_front())
                } else {
                    (lines.next_back(), remaining.pop_back())
                };
                assert_eq!(actual.unwrap().range(), expected.unwrap());
            }
            assert_eq!(lines.size_hint(), (0, Some(0)));
            assert!(lines.next().is_none());
            assert!(lines.next_back().is_none());
            assert!(lines.next().is_none());
        }
    }
}

#[test]
fn logical_lines_borrow_history_fragments_wide_gaps_and_mixed_captured_widths() {
    let mut screen = Screen::new(4, 2).unwrap();
    screen
        .set_history_limits(HistoryLimits {
            max_rows: 3,
            max_bytes: 4096,
        })
        .unwrap();
    screen.write_ascii(b"abc", StyleId(1)).unwrap();
    screen
        .write("水\u{301}", CellWidth::Double, StyleId(2))
        .unwrap();
    screen.write_ascii(b"xyZ", StyleId(3)).unwrap();
    screen.line_feed(StyleId(0)).unwrap();
    screen.carriage_return();
    screen
        .write("👩\u{200d}💻", CellWidth::Double, StyleId(4))
        .unwrap();
    let texts = |screen: &Screen| {
        screen
            .view()
            .logical_lines()
            .map(|line| (line.range(), line.text().to_string()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        texts(&screen),
        [
            (0..3, "abc水\u{301}xyZ   ".into()),
            (3..4, "👩\u{200d}💻  ".into())
        ]
    );
    let view = screen.view();
    let first = view.logical_line(1).unwrap();
    let mut rows = first.iter_rows();
    assert_eq!(rows.next().unwrap().wrap_end(), Some(3));
    let wide = rows.next().unwrap();
    assert_eq!(wide.text(0), Some(CellText::Extended("水\u{301}")));
    assert_eq!(wide.cells()[0].style(), StyleId(2));
    assert!(!wide.cells()[0].is_erased());
    assert_eq!(wide.cells()[1].width(), 0);
    assert!(rows.next().unwrap().cells()[3].is_erased());
    drop(rows);
    screen
        .set_history_limits(HistoryLimits {
            max_rows: 1,
            max_bytes: 4096,
        })
        .unwrap();
    assert_eq!(
        texts(&screen),
        [
            (0..2, "水\u{301}xyZ   ".into()),
            (2..3, "👩\u{200d}💻  ".into())
        ]
    );
    screen.resize(6, 2, StyleId(0)).unwrap();
    assert_eq!(
        texts(&screen),
        [
            (0..1, "水\u{301}xy".into()),
            (1..2, "Z     ".into()),
            (2..3, "👩\u{200d}💻    ".into())
        ]
    );
    screen.select_buffer(ScreenBuffer::Alternate);
    screen.set_cursor(0, 0);
    screen.write_ascii(b"abcdefg", StyleId(5)).unwrap();
    assert_eq!(texts(&screen), [(0..2, "abcdefg     ".into())]);
    screen.select_buffer(ScreenBuffer::Primary);
    screen.clear_history();
    assert_eq!(
        texts(&screen),
        [(0..1, "Z     ".into()), (1..2, "👩\u{200d}💻    ".into())]
    );
}
