use crate::{
    CellText, CellWidth, Cursor, CursorAppearance, CursorShape, HistoryLimits, Screen,
    ScreenBuffer, StorageError, StyleId,
};

fn rows(screen: &Screen, buffer: ScreenBuffer) -> Vec<String> {
    screen
        .buffer(buffer)
        .iter_rows()
        .map(|row| {
            (0..row.cells().len())
                .filter_map(|column| match row.text(column).unwrap() {
                    CellText::Scalar(value) => Some(value.to_string()),
                    CellText::Extended(value) => Some(value.to_owned()),
                    CellText::Continuation => None,
                })
                .collect()
        })
        .collect()
}

fn history(screen: &Screen) -> Vec<String> {
    screen
        .history()
        .iter_rows()
        .map(|row| {
            (0..row.cells().len())
                .filter_map(|column| match row.text(column).unwrap() {
                    CellText::Scalar(value) => Some(value.to_string()),
                    CellText::Extended(value) => Some(value.to_owned()),
                    CellText::Continuation => None,
                })
                .collect()
        })
        .collect()
}

#[test]
fn rejected_reflow_commit_leaves_source_rows_in_place() {
    let mut screen = Screen::new(4, 2).unwrap();
    screen
        .write("e\u{301}", CellWidth::Single, StyleId(1))
        .unwrap();
    let Some(CellText::Extended(before)) = screen.grid().row(0).unwrap().text(0) else {
        panic!("expected extended text");
    };
    let before = before.as_ptr();
    let cursor = screen.cursor();

    let result = screen.reflow_with(3, 3, StyleId::default(), || Err(StorageError::OutOfBounds));
    assert!(matches!(result, Err(StorageError::OutOfBounds)));
    assert_eq!((screen.grid().columns(), screen.grid().rows()), (4, 2));
    assert_eq!(screen.cursor(), cursor);
    let Some(CellText::Extended(after)) = screen.grid().row(0).unwrap().text(0) else {
        panic!("expected extended text");
    };
    assert_eq!(after.as_ptr(), before);
}

#[test]
fn graphics_anchors_reflow_with_primary_text_and_alternate_crop() {
    let mut screen = Screen::new(6, 3).unwrap();
    screen
        .write_ascii(b"abcdefghi", StyleId::default())
        .unwrap();
    let mut processor = super::support::processor();
    let mut text =
        |screen: &mut Screen, value: &str, style| screen.write_ascii(value.as_bytes(), style);
    processor
        .advance(
            b"\x1b[1;5H\x1b_Ga=T,i=1,p=1,s=1,v=1,C=1,q=1;/wAA/w==\x1b\\\x1b_Ga=T,i=2,P=1,Q=1,H=1,V=1,s=1,v=1,q=1;AAD//w==\x1b\\\x1b[1;1H",
            &mut screen,
            &mut text,
        )
        .unwrap();
    assert!(processor.reply_bytes().is_empty());
    screen.save_cursor();
    let root_serial = screen.graphics_placements()[0].serial();
    screen.select_buffer(ScreenBuffer::Alternate);
    processor
        .advance(
            b"\x1b[1;6H\x1b_Ga=T,i=3,s=1,v=1,C=1,q=1;AAAA/w==\x1b\\\x1b[1;2H\x1b_Ga=T,i=4,s=1,v=1,C=1,q=1;AAAA/w==\x1b\\",
            &mut screen,
            &mut text,
        )
        .unwrap();
    assert!(processor.reply_bytes().is_empty());
    let snapshot = |screen: &Screen| {
        screen
            .all_graphics_placements()
            .map(|image| (image.serial(), image.row, image.column))
            .collect::<Vec<_>>()
    };
    let before = snapshot(&screen);
    let rejected = screen.reflow_with(3, 5, StyleId::default(), || Err(StorageError::OutOfBounds));
    assert!(matches!(rejected, Err(StorageError::OutOfBounds)));
    assert_eq!(snapshot(&screen), before);
    screen.reflow(3, 5, StyleId::default()).unwrap();
    let [remaining] = screen.graphics_placements() else {
        panic!("One alternate placement stays")
    };
    assert_eq!(remaining.placement.image.id(), 4);
    assert_eq!((remaining.row, remaining.column), (0, 1));
    screen.select_buffer(ScreenBuffer::Primary);

    let [root, child] = screen.graphics_placements() else {
        panic!("Primary placements stay")
    };
    assert_eq!(root.serial(), root_serial);
    assert_eq!((root.row, root.column), (1, 1));
    assert_eq!(
        screen
            .grid()
            .row(root.row as usize)
            .unwrap()
            .text(root.column as usize),
        Some(CellText::Scalar('e'))
    );
    assert_eq!((child.row, child.column), (2, 2));
}

#[test]
fn screen_reflow_moves_text_allocations_and_budgets_their_retained_capacity() {
    let mut screen = Screen::new(4, 2).unwrap();
    screen
        .set_history_limits(HistoryLimits {
            max_rows: 8,
            max_bytes: 4096,
        })
        .unwrap();
    for _ in 0..12 {
        screen
            .write("e\u{301}", CellWidth::Single, StyleId(1))
            .unwrap();
    }
    screen.save_cursor();
    let before = screen
        .view()
        .iter_rows()
        .flat_map(|row| {
            (0..row.cells().len()).filter_map(move |column| {
                if let CellText::Extended(text) = row.text(column).unwrap() {
                    Some(text.as_ptr())
                } else {
                    None
                }
            })
        })
        .collect::<Vec<_>>();
    screen.select_buffer(ScreenBuffer::Alternate);
    screen.set_cursor(0, 0);
    screen
        .write("👩\u{200d}💻", CellWidth::Double, StyleId(2))
        .unwrap();
    screen
        .write("e\u{301}", CellWidth::Single, StyleId(3))
        .unwrap();
    let pointers = [0, 2].map(
        |column| match screen.grid().row(0).unwrap().text(column).unwrap() {
            CellText::Extended(text) => text.as_ptr(),
            _ => unreachable!(),
        },
    );
    screen.select_buffer(ScreenBuffer::Primary);
    screen.restore_cursor();
    screen.reflow(3, 2, StyleId(9)).unwrap();
    let after = screen
        .view()
        .iter_rows()
        .flat_map(|row| {
            (0..row.cells().len()).filter_map(move |column| {
                if let CellText::Extended(text) = row.text(column).unwrap() {
                    Some(text.as_ptr())
                } else {
                    None
                }
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(after, before);
    let after = [0, 2].map(|column| {
        match screen
            .buffer(ScreenBuffer::Alternate)
            .row(0)
            .unwrap()
            .text(column)
            .unwrap()
        {
            CellText::Extended(text) => text.as_ptr(),
            _ => unreachable!(),
        }
    });
    assert_eq!(after, pointers);

    let mut bounded = Screen::new(2, 2).unwrap();
    bounded
        .set_history_limits(HistoryLimits {
            max_rows: 8,
            max_bytes: 512,
        })
        .unwrap();
    bounded
        .write(
            &(String::from("e") + &"\u{301}".repeat(511)),
            CellWidth::Single,
            StyleId(1),
        )
        .unwrap();
    bounded.set_cursor(0, 0);
    bounded
        .write("e\u{301}", CellWidth::Single, StyleId(1))
        .unwrap();
    assert!(bounded.grid().row(0).unwrap().retained_bytes() > 512);
    bounded.set_cursor(1, 0);
    bounded.write_ascii(b"bb", StyleId(1)).unwrap();
    bounded.reflow(2, 1, StyleId(9)).unwrap();
    assert!(bounded.history().is_empty());
    assert_eq!(rows(&bounded, ScreenBuffer::Primary), ["bb"]);
}

#[test]
fn screen_reflow_bounds_sparse_capacity_across_source_segments() {
    let mut screen = Screen::new(119, 3).unwrap();
    screen
        .set_history_limits(HistoryLimits {
            max_rows: 8,
            max_bytes: 1 << 20,
        })
        .unwrap();
    for _ in 0..119 * 3 {
        screen
            .write("e\u{301}", CellWidth::Single, StyleId(1))
            .unwrap();
    }
    screen.reflow(80, 2, StyleId(9)).unwrap();
    let full_row_bytes = screen.grid().row(0).unwrap().retained_bytes();
    assert_eq!(
        screen
            .history()
            .iter_rows()
            .map(|row| row.retained_bytes())
            .collect::<Vec<_>>(),
        vec![full_row_bytes; 3]
    );
}

#[test]
fn screen_reflow_orders_pending_and_following_anchors_at_the_same_boundary() {
    let mut screen = Screen::new(4, 2).unwrap();
    screen
        .set_history_limits(HistoryLimits {
            max_rows: 4,
            max_bytes: 4096,
        })
        .unwrap();
    screen.write_ascii(b"abcd", StyleId(1)).unwrap();
    screen.save_cursor();
    screen.write_ascii(b"efghijkl", StyleId(1)).unwrap();
    screen.set_cursor(1, 0);
    screen.scroll_viewport_up(1);
    screen.reflow(2, 2, StyleId(9)).unwrap();
    assert_eq!(rows(&screen, ScreenBuffer::Primary), ["ij", "kl"]);
    assert_eq!(history(&screen), ["ab", "cd", "ef", "gh"]);
    assert_eq!(screen.viewport_offset(), 4);
    assert_eq!(
        screen.cursor(),
        Cursor {
            row: 0,
            column: 0,
            wrap_pending: false
        }
    );
    screen.restore_cursor();
    assert_eq!(
        screen.cursor(),
        Cursor {
            row: 0,
            column: 1,
            wrap_pending: false
        }
    );
}

#[test]
fn screen_reflow_replays_suffixes_across_hard_lines_and_early_cursor_windows() {
    for (early, keep_history) in [(false, true), (true, true), (false, false), (true, false)] {
        let mut screen = Screen::new(9, 3).unwrap();
        screen
            .set_history_limits(HistoryLimits {
                max_rows: 4,
                max_bytes: 4096,
            })
            .unwrap();
        for digit in b'0'..=b'6' {
            if digit != b'0' {
                screen.carriage_return();
                screen.line_feed(StyleId(9)).unwrap();
            }
            screen.write_ascii(&[digit; 9], StyleId(1)).unwrap();
        }
        screen.save_cursor();
        screen.scroll_viewport_up(4);
        if early {
            screen.set_cursor(0, 0);
        }
        if !keep_history {
            screen
                .set_history_limits(HistoryLimits {
                    max_rows: 4,
                    max_bytes: 0,
                })
                .unwrap();
        }
        screen.reflow(2, 3, StyleId(9)).unwrap();
        assert_eq!(
            rows(&screen, ScreenBuffer::Primary),
            if early {
                ["44", "44", "44"]
            } else {
                ["66", "66", "6 "]
            }
        );
        let expected = if !keep_history {
            vec![]
        } else if early {
            vec!["33", "33", "33", "3 "]
        } else {
            vec!["55", "5 ", "66", "66"]
        };
        assert_eq!(history(&screen), expected);
        assert_eq!(screen.viewport_offset(), if keep_history { 4 } else { 0 });
        assert_eq!(
            screen.cursor(),
            Cursor {
                row: if early { 0 } else { 2 },
                column: usize::from(!early),
                wrap_pending: false,
            }
        );
        screen.restore_cursor();
        assert_eq!(
            screen.cursor(),
            Cursor {
                row: 2,
                column: 1,
                wrap_pending: false
            }
        );
    }
}

#[test]
fn screen_reflow_preserves_primary_content_cursors_and_viewport_then_pulls_history_on_growth() {
    let mut screen = Screen::new(4, 2).unwrap();
    screen
        .set_history_limits(HistoryLimits {
            max_rows: 8,
            max_bytes: 4096,
        })
        .unwrap();
    screen.write_ascii(b"abcdefghijkl", StyleId(1)).unwrap();
    screen.save_cursor();
    screen.scroll_viewport_up(1);
    screen.reflow(3, 2, StyleId(9)).unwrap();
    assert_eq!(rows(&screen, ScreenBuffer::Primary), ["ghi", "jkl"]);
    assert_eq!(history(&screen), ["abc", "def"]);
    assert_eq!(screen.viewport_offset(), 2);
    assert_eq!(
        screen.cursor(),
        Cursor {
            row: 1,
            column: 2,
            wrap_pending: true
        }
    );
    screen.set_cursor(0, 0);
    screen.restore_cursor();
    assert_eq!(
        screen.cursor(),
        Cursor {
            row: 1,
            column: 2,
            wrap_pending: true
        }
    );
    screen
        .set_history_limits(HistoryLimits {
            max_rows: 1,
            max_bytes: 4096,
        })
        .unwrap();
    screen.reflow(6, 3, StyleId(9)).unwrap();
    assert_eq!(
        rows(&screen, ScreenBuffer::Primary),
        ["defghi", "jkl   ", "      "]
    );
    assert!(screen.history().is_empty());
    assert_eq!(screen.viewport_offset(), 0);
    assert_eq!(
        screen.cursor(),
        Cursor {
            row: 1,
            column: 3,
            wrap_pending: false
        }
    );
    screen.restore_cursor();
    assert_eq!(
        screen.cursor(),
        Cursor {
            row: 1,
            column: 3,
            wrap_pending: false
        }
    );
    screen.set_cursor(2, 0);
    screen.line_feed(StyleId(9)).unwrap();
    assert_eq!(history(&screen), ["defghi"]);
    assert_eq!(
        screen.grid().row(0).unwrap().text(0),
        Some(CellText::Scalar('j'))
    );
}

#[test]
fn screen_reflow_handles_active_alternate_origin_save_slots_and_shared_state() {
    let mut screen = Screen::new(4, 4).unwrap();
    screen
        .set_history_limits(HistoryLimits {
            max_rows: 3,
            max_bytes: 4096,
        })
        .unwrap();
    screen
        .write_ascii(b"abcdefghijklmnopqrst", StyleId(1))
        .unwrap();
    screen.scroll_viewport_up(1);
    screen.set_scroll_region(1..4).unwrap();
    screen.set_origin_mode(true);
    screen.set_cursor(1, 3);
    screen.write_ascii(b"p", StyleId(1)).unwrap();
    screen.save_cursor();
    screen.select_buffer(ScreenBuffer::Alternate);
    screen.set_origin_mode(false);
    screen.write_ascii(b"MNOPQRST", StyleId(2)).unwrap();
    screen.save_cursor();
    screen.set_cursor(0, 1);
    screen.set_tab_stop();
    screen.restore_cursor();
    screen.set_insert_mode(true);
    let appearance = CursorAppearance {
        visible: false,
        blinking: false,
        shape: CursorShape::Bar,
    };
    screen.set_cursor_appearance(appearance);
    screen.reflow(3, 3, StyleId(9)).unwrap();
    assert_eq!(screen.active_buffer(), ScreenBuffer::Alternate);
    assert_eq!(
        rows(&screen, ScreenBuffer::Alternate),
        ["MNO", "QRS", "   "]
    );
    assert_eq!(rows(&screen, ScreenBuffer::Primary), ["p  ", "qrs", "t  "]);
    assert_eq!(history(&screen), ["ghi", "jkl", "mno"]);
    assert_eq!(
        screen.cursor(),
        Cursor {
            row: 1,
            column: 2,
            wrap_pending: false
        }
    );
    assert_eq!(screen.scroll_region(), 0..3);
    assert!(screen.insert_mode() && screen.auto_wrap() && !screen.origin_mode());
    assert_eq!(screen.cursor_appearance(), appearance);
    assert_eq!(screen.is_tab_stop(1), Some(true));
    assert_eq!(screen.viewport_offset(), 0);
    screen.select_buffer(ScreenBuffer::Primary);
    assert_eq!(screen.viewport_offset(), 3);
    screen.restore_cursor();
    assert!(screen.origin_mode());
    assert_eq!(
        screen.cursor(),
        Cursor {
            row: 0,
            column: 1,
            wrap_pending: false
        }
    );
    screen.select_buffer(ScreenBuffer::Alternate);
    screen.restore_cursor();
    assert!(!screen.origin_mode());
    assert_eq!(
        screen.cursor(),
        Cursor {
            row: 1,
            column: 2,
            wrap_pending: false
        }
    );
}

#[test]
fn screen_reflow_bounds_contiguous_history_crops_below_cursor_and_rejects_narrow_wide_units() {
    let mut screen = Screen::new(2, 4).unwrap();
    screen
        .set_history_limits(HistoryLimits {
            max_rows: 8,
            max_bytes: 64,
        })
        .unwrap();
    for (index, text) in ["aa", "bb", "cc", "dd"].into_iter().enumerate() {
        screen.set_cursor(index, 0);
        screen.write_ascii(text.as_bytes(), StyleId(1)).unwrap();
    }
    screen.set_cursor(1, 0);
    screen
        .write(
            &(String::from("e") + &"\u{301}".repeat(511)),
            CellWidth::Single,
            StyleId(2),
        )
        .unwrap();
    screen.set_cursor(3, 1);
    screen.write_ascii(b"d", StyleId(1)).unwrap();
    screen.reflow(2, 1, StyleId(9)).unwrap();
    assert_eq!(history(&screen), ["cc"]);
    assert!(screen.history().retained_bytes() <= 64);
    assert_eq!(rows(&screen, ScreenBuffer::Primary), ["dd"]);
    screen.reflow(1, 1, StyleId(9)).unwrap();
    assert_eq!(rows(&screen, ScreenBuffer::Primary), ["d"]);
    screen.set_cursor(0, 0);
    screen.reflow(4, 2, StyleId(9)).unwrap();
    screen.set_cursor(0, 0);
    screen
        .write("水\u{301}", CellWidth::Double, StyleId(3))
        .unwrap();
    let before = super::snapshot(screen.grid());
    assert!(matches!(
        screen.reflow(1, 2, StyleId(9)),
        Err(StorageError::OutOfBounds)
    ));
    assert_eq!(super::snapshot(screen.grid()), before);
    for (width, height) in [(0, 1), (1, 0), (usize::MAX, 1), (4096, 4096)] {
        assert!(screen.reflow(width, height, StyleId(9)).is_err());
        assert_eq!(super::snapshot(screen.grid()), before);
    }
    screen.set_cursor(0, 0);
    screen.reflow(2, 1, StyleId(9)).unwrap();
    assert_eq!(
        screen.grid().row(0).unwrap().text(0),
        Some(CellText::Extended("水\u{301}"))
    );
    assert_eq!(screen.grid().row(0).unwrap().wrap_end(), None);

    let mut bounded = Screen::new(4, 2).unwrap();
    bounded
        .set_history_limits(HistoryLimits {
            max_rows: 8,
            max_bytes: 64,
        })
        .unwrap();
    bounded
        .write_ascii(b"abcdefghijklmnop", StyleId(1))
        .unwrap();
    bounded.scroll_viewport_up(2);
    bounded.reflow(3, 2, StyleId(9)).unwrap();
    assert_eq!(history(&bounded), ["ghi", "jkl"]);
    assert_eq!(bounded.viewport_offset(), 2);
    assert!(bounded.history().retained_bytes() <= 64);
}
