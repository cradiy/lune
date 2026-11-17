use super::{snapshot, support};
use crate::{
    Cursor, CursorAppearance, HistoryLimits, MAX_HISTORY_BYTES, PaletteEntry, ProcessFailure,
    Screen, ScreenBuffer, StyleId, UnicodeWriter,
};

#[test]
fn soft_reset_preserves_storage_and_restores_only_the_active_save_slot() {
    use ScreenBuffer::{Alternate, Primary};

    for active in [Primary, Alternate] {
        for chunk in [1, 17, usize::MAX] {
            let mut processor = support::processor();
            let mut screen = Screen::new(8, 4).unwrap();
            screen
                .set_history_limits(HistoryLimits {
                    max_rows: 8,
                    max_bytes: MAX_HISTORY_BYTES,
                })
                .unwrap();
            let mut writer = UnicodeWriter::default();
            processor
                .advance(
                    b"old\r\none\r\ntwo\r\nthree\r\nfour",
                    &mut screen,
                    &mut writer,
                )
                .unwrap();
            screen.scroll_viewport_up(1);
            assert!(!screen.history().is_empty());
            let history: Vec<_> = screen
                .history()
                .iter_rows()
                .map(|row| format!("{row:?}"))
                .collect();
            let inactive = if active == Primary {
                Alternate
            } else {
                Primary
            };
            screen.select_buffer(inactive);
            processor
                .advance(
                    b"\x1b[32m\x1b)0\x0e\x1b[2;3H\x1b7",
                    &mut screen,
                    &mut writer,
                )
                .unwrap();
            let inactive_style = processor.style_id();
            screen.select_buffer(active);
            let setup = "\x1b]2;synthetic title\x07\x1b]4;1;#123456\x07\x1b]10;#abcdef\x07\x1b[3g\x1b[1;3H\x1bH\x1b[2;4r\x1b[?6h\x1b[1;31m\x1b[4;20h\x1b[?1;66;67;1004;1003;1006;2004h\x1b[?12;25l\x1b[6 q\x1b[2;7H水\x1b7";
            for bytes in setup.as_bytes().chunks(chunk) {
                processor.advance(bytes, &mut screen, &mut writer).unwrap();
            }
            let primary = snapshot(screen.buffer(Primary));
            let alternate = snapshot(screen.buffer(Alternate));
            let cursor = screen.cursor();
            assert!(cursor.wrap_pending);
            let offset = screen.viewport_offset();
            let mut input = processor.input_modes();
            input.application_cursor = false;
            input.application_keypad = false;
            let foreground = processor.palette().color(PaletteEntry::Foreground);
            processor.take_window_title_change();
            processor.take_palette_change();
            for bytes in b"\x1b[!p".chunks(chunk) {
                processor.advance(bytes, &mut screen, &mut writer).unwrap();
            }
            assert_eq!(screen.active_buffer(), active);
            assert_eq!(snapshot(screen.buffer(Primary)), primary);
            assert_eq!(snapshot(screen.buffer(Alternate)), alternate);
            assert_eq!(
                screen
                    .history()
                    .iter_rows()
                    .map(|row| format!("{row:?}"))
                    .collect::<Vec<_>>(),
                history
            );
            assert_eq!(screen.viewport_offset(), offset);
            assert_eq!(
                screen.cursor(),
                Cursor {
                    wrap_pending: false,
                    ..cursor
                }
            );
            assert_eq!(screen.scroll_region(), 0..4);
            assert!(screen.auto_wrap() && !screen.origin_mode() && !screen.insert_mode());
            assert_eq!(screen.cursor_appearance(), CursorAppearance::default());
            assert_eq!(screen.is_tab_stop(2), Some(true));
            assert_eq!(screen.is_tab_stop(0), Some(false));
            assert_eq!(processor.input_modes(), input);
            assert_eq!(processor.style_id(), StyleId(0));
            assert_eq!(processor.window_title(), "synthetic title");
            assert_eq!(processor.take_window_title_change(), None);
            assert_eq!(
                processor.palette().color(PaletteEntry::Foreground),
                foreground
            );
            assert!(!processor.palette().is_overridden(PaletteEntry::Indexed(1)));
            assert!(processor.take_palette_change());
            processor
                .advance(b"\x1b[!p\x1b8q", &mut screen, &mut writer)
                .unwrap();
            assert!(!processor.take_palette_change());
            assert_eq!(
                screen.cursor(),
                Cursor {
                    column: 1,
                    ..Cursor::default()
                }
            );
            assert_eq!(
                screen.grid().row(0).unwrap().text(0),
                Some(crate::CellText::Scalar('q'))
            );
            assert_eq!(screen.grid().row(0).unwrap().cells()[0].style(), StyleId(0));
            screen.select_buffer(inactive);
            processor
                .advance(b"\x1b8q", &mut screen, &mut writer)
                .unwrap();
            assert_eq!(
                screen.cursor(),
                Cursor {
                    row: 1,
                    column: 3,
                    wrap_pending: false
                }
            );
            assert_eq!(
                screen.grid().row(1).unwrap().text(2),
                Some(crate::CellText::Scalar('─'))
            );
            assert_eq!(processor.style_id(), inactive_style);
            screen.select_buffer(active);
            let retained = screen.history().len();
            processor
                .advance(b"\x1b[4;1H\n", &mut screen, &mut writer)
                .unwrap();
            assert_eq!(
                screen.history().len(),
                retained + usize::from(active == Primary)
            );
            assert_eq!(
                (processor.parse_errors(), processor.ignored_sequences()),
                (0, 0)
            );
        }
    }
}

#[test]
fn soft_reset_rejects_headers_and_preserves_captured_replies_under_backpressure() {
    for split in 0..=4 {
        let mut processor = support::processor();
        let mut screen = Screen::new(8, 4).unwrap();
        let mut writer = UnicodeWriter::default();
        processor
            .advance(
                b"\x1b[2;4r\x1b[?6h\x1b[31m\x1b[2;8Hx\x1b]4;1;#123456\x07",
                &mut screen,
                &mut writer,
            )
            .unwrap();
        processor.take_palette_change();
        let cursor = screen.cursor();
        let style = processor.style_id();
        for wire in [
            "\x1b[0!p",
            "\x1b[?1!p",
            "\x1b[1;2!p",
            "\x1b[1:2!p",
            "\x1b[!!p",
            "\x1b[! p",
        ] {
            processor
                .advance(wire.as_bytes(), &mut screen, &mut writer)
                .unwrap();
            assert_eq!(screen.cursor(), cursor);
            assert_eq!(processor.style_id(), style);
            assert!(screen.origin_mode());
            assert!(!processor.take_palette_change());
        }
        processor
            .advance(b"\x1b[!\x18", &mut screen, &mut writer)
            .unwrap();
        assert_eq!(screen.cursor(), cursor);
        assert_eq!(processor.style_id(), style);
        processor
            .advance(b"\x1b[6n", &mut screen, &mut writer)
            .unwrap();
        assert_eq!(processor.reply_bytes(), b"\x1b[2;8R");
        assert!(processor.consume_reply(2));
        let wire = b"\x1b[!p";
        processor
            .advance(&wire[..split], &mut screen, &mut writer)
            .unwrap();
        processor
            .advance(&wire[split..], &mut screen, &mut writer)
            .unwrap();
        assert_eq!(processor.reply_bytes(), b"2;8R");
        let error = processor
            .advance(b"\x1b[6nTAIL", &mut screen, &mut writer)
            .unwrap_err();
        assert!(matches!(error.error, ProcessFailure::ReplyPending));
        assert_eq!(error.consumed, 4);
        assert!(processor.consume_reply(4));
        processor.advance(&[], &mut screen, &mut writer).unwrap();
        assert_eq!(processor.reply_bytes(), b"\x1b[3;8R");
        processor
            .advance(b"TAIL", &mut screen, &mut writer)
            .unwrap();
        assert_eq!(processor.reply_bytes(), b"\x1b[3;8R");
        assert_eq!(
            (processor.parse_errors(), processor.ignored_sequences()),
            (0, 6)
        );
    }
}
