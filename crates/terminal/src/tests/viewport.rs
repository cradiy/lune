use super::snapshot;
use crate::{
    CellPosition, CellText, CellWidth, Cursor, HistoryLimits, Screen, ScreenBuffer, StorageError,
    StyleId, TextSink, UnicodeWriter,
};

fn point(row: usize, column: usize) -> CellPosition {
    CellPosition { row, column }
}
fn limits(rows: usize, bytes: usize) -> HistoryLimits {
    HistoryLimits {
        max_rows: rows,
        max_bytes: bytes,
    }
}
fn window(screen: &Screen) -> String {
    let view = screen.viewport();
    let end = view.row(view.rows() - 1).unwrap().cells().len();
    view.text(point(0, 0)..point(view.rows() - 1, end))
        .unwrap()
        .to_string()
}

#[test]
fn viewport_follows_then_anchors_retained_rows_and_clamps_after_eviction() {
    let mut screen = Screen::new(1, 2).unwrap();
    screen.set_history_limits(limits(8, 4096)).unwrap();
    screen.write_ascii(b"abcdefg", StyleId(1)).unwrap();
    assert_eq!(window(&screen), "fg");
    assert_eq!(screen.viewport().range(), 5..7);
    assert_eq!(screen.viewport().cursor(), Some(screen.cursor()));
    let before = snapshot(screen.grid());
    let cursor = screen.cursor();
    screen.scroll_viewport_up(2);
    assert_eq!(screen.viewport_offset(), 2);
    assert_eq!(window(&screen), "de");
    assert!(screen.viewport().cursor().is_none());
    assert!(matches!(
        screen.viewport().text(point(0, 0)..point(2, 0)),
        Err(StorageError::OutOfBounds)
    ));
    assert!(screen.viewport().row(usize::MAX).is_none());
    assert_eq!(
        screen
            .viewport()
            .iter_rows()
            .rev()
            .map(|row| row.text(0).unwrap())
            .collect::<Vec<_>>(),
        [CellText::Scalar('e'), CellText::Scalar('d')]
    );
    assert_eq!(snapshot(screen.grid()), before);
    assert_eq!(screen.cursor(), cursor);
    screen.write_ascii(b"hi", StyleId(1)).unwrap();
    assert_eq!(screen.viewport_offset(), 4);
    assert_eq!(window(&screen), "de");
    screen.write_ascii(b"jklmn", StyleId(1)).unwrap();
    assert_eq!(screen.viewport_offset(), 8);
    assert_eq!(window(&screen), "ef");
    screen.scroll_viewport_down(3);
    assert_eq!(window(&screen), "hi");
    screen.scroll_viewport_down(usize::MAX);
    assert_eq!(screen.viewport_offset(), 0);
    screen.write_ascii(b"o", StyleId(1)).unwrap();
    assert_eq!(window(&screen), "no");
    screen.scroll_viewport_up(usize::MAX);
    assert_eq!(screen.viewport_offset(), 8);
    screen.scroll_viewport_to_bottom();
    assert_eq!(window(&screen), "no");
}

#[test]
fn viewport_preserves_primary_position_across_buffers_and_resize_and_clears_on_reset() {
    let mut screen = Screen::new(1, 3).unwrap();
    screen.set_history_limits(limits(8, 4096)).unwrap();
    screen.write_ascii(b"abcdef", StyleId(1)).unwrap();
    screen.set_cursor(0, 0);
    screen.scroll_viewport_up(1);
    assert_eq!(
        screen.viewport().cursor(),
        Some(Cursor {
            row: 1,
            column: 0,
            wrap_pending: false
        })
    );
    screen.resize(2, 4, StyleId(0)).unwrap();
    assert_eq!(screen.viewport().rows(), 4);
    assert_eq!(screen.viewport().row(0).unwrap().cells().len(), 1);
    assert_eq!(
        screen
            .viewport()
            .text(point(0, 0)..point(1, 1))
            .unwrap()
            .to_string(),
        "c\nd"
    );
    screen.select_buffer(ScreenBuffer::Alternate);
    assert_eq!(screen.viewport_offset(), 0);
    assert_eq!(screen.viewport().range(), 0..4);
    screen.scroll_viewport_up(usize::MAX);
    screen.scroll_viewport_down(usize::MAX);
    screen.scroll_viewport_to_bottom();
    screen.select_buffer(ScreenBuffer::Primary);
    assert_eq!(screen.viewport_offset(), 1);
    assert_eq!(
        screen.viewport().row(0).unwrap().text(0),
        Some(CellText::Scalar('c'))
    );
    let range = screen.viewport().range();
    assert!(screen.set_history_limits(limits(usize::MAX, 4096)).is_err());
    assert_eq!(screen.viewport().range(), range);
    screen.set_history_limits(limits(1, 4096)).unwrap();
    assert_eq!(screen.viewport_offset(), 1);
    assert_eq!(
        screen.viewport().row(0).unwrap().text(0),
        Some(CellText::Scalar('c'))
    );
    screen.select_buffer(ScreenBuffer::Alternate);
    screen.clear_history();
    screen.select_buffer(ScreenBuffer::Primary);
    assert_eq!(screen.viewport_offset(), 0);
    screen.scroll_up(1, StyleId(0)).unwrap();
    screen.scroll_viewport_up(usize::MAX);
    assert_eq!(screen.viewport_offset(), 1);
    screen.set_history_limits(limits(0, 4096)).unwrap();
    assert_eq!(screen.viewport_offset(), 0);
    screen.set_history_limits(limits(8, 4096)).unwrap();
    screen.scroll_up(1, StyleId(0)).unwrap();
    screen.scroll_viewport_up(1);
    screen.reset(StyleId(0));
    assert_eq!(screen.viewport_offset(), 0);
    assert!(screen.history().is_empty());
    assert_eq!(screen.viewport().rows(), 4);
}

#[test]
fn viewport_tracks_lf_counted_ascii_and_unicode_capture_and_byte_budget_evictions() {
    for case in 0..8 {
        let mut screen = Screen::new(4, 2).unwrap();
        screen.set_history_limits(limits(6, 4096)).unwrap();
        screen
            .write_ascii(b"abcdefghijklmnopqrst", StyleId(1))
            .unwrap();
        screen.scroll_viewport_up(2);
        let anchor = screen.viewport().row(0).unwrap().cells().as_ptr();
        match case {
            0 => screen.line_feed(StyleId(0)).unwrap(),
            1 => screen.scroll_up(usize::MAX, StyleId(0)).unwrap(),
            2 => screen.write_ascii(b"u", StyleId(2)).unwrap(),
            3 => {
                screen.set_cursor(1, 3);
                screen
                    .write("水\u{301}", CellWidth::Double, StyleId(2))
                    .unwrap();
            }
            4 => {
                let mut writer = UnicodeWriter::default();
                screen.set_cursor(1, 3);
                writer.write(&mut screen, "❤", StyleId(2)).unwrap();
                screen.scroll_viewport_up(1);
                screen.scroll_viewport_down(1);
                writer.write(&mut screen, "\u{fe0f}", StyleId(2)).unwrap();
                assert_eq!(
                    screen.grid().row(1).unwrap().text(0),
                    Some(CellText::Extended("❤\u{fe0f}"))
                );
            }
            5 => {
                screen.set_scroll_region(0..1).unwrap();
                screen.line_feed(StyleId(0)).unwrap();
            }
            _ => {
                screen.set_cursor(0, 0);
                screen
                    .write(
                        &(String::from("e") + &"\u{301}".repeat(160)),
                        CellWidth::Single,
                        StyleId(2),
                    )
                    .unwrap();
                let bytes = screen.grid().row(0).unwrap().retained_bytes();
                screen
                    .set_history_limits(limits(6, bytes - usize::from(case == 6)))
                    .unwrap();
                screen.set_cursor(1, 0);
                screen.line_feed(StyleId(0)).unwrap();
            }
        }
        if case == 7 {
            assert_eq!(screen.history().len(), 1);
            assert_eq!(screen.viewport_offset(), 1);
            assert_eq!(screen.viewport().range().start, 0);
        } else {
            assert_eq!(
                screen.viewport().row(0).unwrap().cells().as_ptr(),
                anchor,
                "case {case}"
            );
            assert_eq!(
                screen.viewport_offset(),
                if case == 1 {
                    4
                } else if case >= 5 {
                    2
                } else {
                    3
                },
                "case {case}"
            );
        }
    }
}
