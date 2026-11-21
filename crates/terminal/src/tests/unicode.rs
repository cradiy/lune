use super::support;

use super::{assert_invariants, snapshot};
use crate::{
    CellText, Cursor, ProcessFailure, Screen, ScreenBuffer, StorageError, StyleId, TextSink,
    UnicodeWriter, WidthPolicy,
};

fn text(screen: &Screen, row: usize, column: usize) -> CellText<'_> {
    screen.grid().row(row).unwrap().text(column).unwrap()
}

#[test]
fn unicode_clusters_and_widths_are_independent_of_byte_chunking() {
    let input = "a\u{301}❤\u{fe0f}👩🏽\u{200d}💻🇺🇸1\u{fe0f}\u{20e3}水".as_bytes();
    for split in 0..=input.len() {
        let mut screen = Screen::new(24, 3).unwrap();
        let mut processor = support::processor();
        let mut writer = UnicodeWriter::default();
        for part in [&input[..split], &input[split..]] {
            assert_eq!(
                processor.advance(part, &mut screen, &mut writer).unwrap(),
                part.len()
            );
        }
        processor.finish(&mut screen, &mut writer).unwrap();
        for (column, value, width) in [
            (0, "a\u{301}", 1),
            (1, "❤\u{fe0f}", 2),
            (3, "👩🏽\u{200d}💻", 2),
            (5, "🇺🇸", 2),
            (7, "1\u{fe0f}\u{20e3}", 2),
        ] {
            assert_eq!(text(&screen, 0, column), CellText::Extended(value));
            assert_eq!(screen.grid().row(0).unwrap().cells()[column].width(), width);
        }
        assert_eq!(text(&screen, 0, 9), CellText::Scalar('水'));
        assert_eq!(
            screen.cursor(),
            Cursor {
                row: 0,
                column: 11,
                wrap_pending: false
            }
        );
        assert_eq!(processor.parse_errors(), 0);
        assert_invariants(screen.grid());
    }
    for (policy, columns) in [(WidthPolicy::Narrow, 1), (WidthPolicy::Wide, 2)] {
        let mut screen = Screen::new(8, 2).unwrap();
        let mut writer = UnicodeWriter::new(policy);
        support::processor()
            .advance("·".as_bytes(), &mut screen, &mut writer)
            .unwrap();
        assert_eq!(screen.cursor().column, columns);
    }
}

#[test]
fn protocol_boundaries_finish_and_reset_end_cluster_extension() {
    for boundary in [
        &b"\x07"[..],
        b"\x7f",
        b"\x18",
        b"\x1b[0m",
        b"\x1b[?25l",
        b"\x1bZ",
        b"\x1b]0;title\x07",
        b"\x1bPpayload\x1b\\",
        b"\xc2\x80",
    ] {
        let mut input = b"a".to_vec();
        input.extend(boundary);
        input.extend("\u{301}".as_bytes());
        for split in 0..=input.len() {
            let mut screen = Screen::new(8, 2).unwrap();
            let mut processor = support::processor();
            let mut writer = UnicodeWriter::default();
            processor
                .advance(&input[..split], &mut screen, &mut writer)
                .unwrap();
            processor
                .advance(&input[split..], &mut screen, &mut writer)
                .unwrap();
            assert_eq!(text(&screen, 0, 0), CellText::Scalar('a'));
            assert_eq!(text(&screen, 0, 1), CellText::Scalar('\u{301}'));
            assert_eq!(screen.cursor().column, 2);
        }
    }
    let mut screen = Screen::new(8, 2).unwrap();
    let mut processor = support::processor();
    let mut writer = UnicodeWriter::default();
    processor.advance(b"a", &mut screen, &mut writer).unwrap();
    processor.finish(&mut screen, &mut writer).unwrap();
    processor
        .advance("\u{301}".as_bytes(), &mut screen, &mut writer)
        .unwrap();
    assert_eq!(screen.cursor().column, 2);
    processor.reset(&mut screen, &mut writer);
    processor
        .advance("\u{301}".as_bytes(), &mut screen, &mut writer)
        .unwrap();
    assert_eq!(text(&screen, 0, 0), CellText::Scalar('\u{301}'));
    processor
        .advance(b"\x1b[?47h", &mut screen, &mut writer)
        .unwrap();
    processor
        .advance("\u{301}".as_bytes(), &mut screen, &mut writer)
        .unwrap();
    assert_eq!(screen.active_buffer(), ScreenBuffer::Alternate);
    assert_eq!(text(&screen, 0, 1), CellText::Scalar('\u{301}'));
    processor
        .advance(b"\x1b[?47l", &mut screen, &mut writer)
        .unwrap();
    processor
        .advance("\u{301}".as_bytes(), &mut screen, &mut writer)
        .unwrap();
    assert_eq!(text(&screen, 0, 0), CellText::Scalar('\u{301}'));
    assert_eq!(text(&screen, 0, 2), CellText::Scalar('\u{301}'));
}

#[test]
fn unicode_width_changes_wrap_insert_shrink_and_fit_screen_limits() {
    for chunk in 1..=17 {
        for (input, columns, expected_column) in [
            ("abc❤\u{fe0f}", 4, 2),
            ("ab⌚\u{fe0e}", 4, 3),
            ("\x1b[?7labc水\u{301}", 4, 3),
            ("👩🏽\u{200d}💻", 1, 0),
            ("\u{17d8}", 4, 2),
        ] {
            let mut screen = Screen::new(columns, 3).unwrap();
            let mut processor = support::processor();
            let mut writer = UnicodeWriter::default();
            for bytes in input.as_bytes().chunks(chunk) {
                processor.advance(bytes, &mut screen, &mut writer).unwrap();
            }
            assert_eq!(screen.cursor().column, expected_column);
            match input {
                "abc❤\u{fe0f}" => {
                    assert_eq!(screen.grid().row(0).unwrap().wrap_end(), Some(3));
                    assert_eq!(text(&screen, 1, 0), CellText::Extended("❤\u{fe0f}"));
                }
                "ab⌚\u{fe0e}" => {
                    assert_eq!(text(&screen, 0, 2), CellText::Extended("⌚\u{fe0e}"));
                    assert!(screen.grid().row(0).unwrap().cells()[3].is_erased());
                    assert!(!screen.cursor().wrap_pending);
                }
                "\x1b[?7labc水\u{301}" => {
                    assert_eq!(text(&screen, 0, 3), CellText::Extended("水\u{301}"))
                }
                "👩🏽\u{200d}💻" => {
                    assert_eq!(text(&screen, 0, 0), CellText::Extended(input))
                }
                _ => assert_eq!(screen.grid().row(0).unwrap().cells()[0].width(), 2),
            }
            assert_invariants(screen.grid());
        }
    }
    let mut screen = Screen::new(8, 2).unwrap();
    let mut processor = support::processor();
    let mut writer = UnicodeWriter::default();
    processor
        .advance(
            "abcdef\r\x1b[C\x1b[4h❤\u{fe0f}".as_bytes(),
            &mut screen,
            &mut writer,
        )
        .unwrap();
    assert_eq!(text(&screen, 0, 1), CellText::Extended("❤\u{fe0f}"));
    assert_eq!(text(&screen, 0, 3), CellText::Scalar('b'));
    assert_eq!(text(&screen, 0, 7), CellText::Scalar('f'));
}

struct FaultSink {
    writer: UnicodeWriter,
    fail_at: usize,
    calls: usize,
}

impl TextSink for FaultSink {
    type Error = Option<StorageError>;
    fn boundary(&mut self) {
        self.writer.boundary();
    }
    fn write(
        &mut self,
        screen: &mut Screen,
        text: &str,
        style: StyleId,
    ) -> Result<(), Self::Error> {
        let fail = self.calls == self.fail_at;
        self.calls += 1;
        if fail {
            return Err(None);
        }
        self.writer.write(screen, text, style).map_err(Some)
    }
}

#[test]
fn decoded_replacement_control_order_and_writer_failures_retry_across_chunks() {
    let mut input = Vec::from("\u{600}".as_bytes());
    input.extend(b"\xe2\n\x80");
    input.extend("❤\u{fe0f}\x1b[31m👩🏽\u{200d}💻\x1b[?1049h水\x1b[?1049l e\u{301}".as_bytes());
    input.extend("\u{600}".as_bytes());
    input.extend(b"\xe2\x7f");
    input.extend("\u{600}".as_bytes());
    input.extend(b"\xe2\x1b[0m");
    input.extend(b"\xf0\x9f");
    let mut expected = Screen::new(16, 4).unwrap();
    let mut processor = support::processor();
    let mut writer = UnicodeWriter::default();
    processor
        .advance(&input, &mut expected, &mut writer)
        .unwrap();
    processor.finish(&mut expected, &mut writer).unwrap();
    assert_eq!(text(&expected, 0, 0), CellText::Extended("\u{600}�"));
    assert_eq!(text(&expected, 1, 2), CellText::Scalar('�'));
    let expected_errors = processor.parse_errors();
    for chunk in 1..=17 {
        for fail_at in 0..24 {
            let mut screen = Screen::new(16, 4).unwrap();
            let mut processor = support::processor();
            let mut sink = FaultSink {
                writer: UnicodeWriter::default(),
                calls: 0,
                fail_at,
            };
            for mut part in input.chunks(chunk) {
                loop {
                    match processor.advance(part, &mut screen, &mut sink) {
                        Ok(count) => {
                            assert_eq!(count, part.len());
                            break;
                        }
                        Err(error) => {
                            assert!(matches!(error.error, ProcessFailure::Text(None)));
                            part = &part[error.consumed..];
                        }
                    }
                }
            }
            while let Err(error) = processor.finish(&mut screen, &mut sink) {
                assert!(matches!(error, ProcessFailure::Text(None)));
            }
            for buffer in [ScreenBuffer::Primary, ScreenBuffer::Alternate] {
                assert_eq!(
                    snapshot(screen.buffer(buffer)),
                    snapshot(expected.buffer(buffer))
                );
            }
            assert_eq!(screen.cursor(), expected.cursor());
            assert_eq!(processor.parse_errors(), expected_errors);
            assert_invariants(screen.grid());
        }
    }
}

#[test]
fn host_notices_wait_for_both_pending_input_events_and_reuse_drained_slots() {
    let mut screen = Screen::new(8, 2).unwrap();
    let mut processor = support::processor();
    let mut sink = FaultSink {
        writer: UnicodeWriter::default(),
        calls: 0,
        fail_at: 0,
    };
    processor
        .advance(b"\x1b[?2031;2048h", &mut screen, &mut sink)
        .unwrap();
    assert!(processor.consume_reply(processor.reply_bytes().len()));
    let error = processor
        .advance(b"\xc2\x1b", &mut screen, &mut sink)
        .unwrap_err();
    assert_eq!(error.consumed, 2);
    assert!(matches!(error.error, ProcessFailure::Text(None)));
    screen.resize(10, 3, StyleId(0)).unwrap();
    processor.update_geometry(&mut screen, None, None);
    processor.set_color_scheme(Some(crate::ColorScheme::Light));
    assert!(processor.reply_bytes().is_empty());

    let error = processor.advance(&[], &mut screen, &mut sink).unwrap_err();
    assert!(matches!(error.error, ProcessFailure::ReplyPending));
    assert_eq!(error.consumed, 0);
    assert_eq!(text(&screen, 0, 0), CellText::Scalar('�'));
    assert_eq!(processor.reply_bytes(), b"\x1b[48;3;10;0;0t");
    screen.resize(12, 4, StyleId(0)).unwrap();
    processor.update_geometry(&mut screen, None, None);
    assert!(processor.consume_reply(processor.reply_bytes().len()));
    assert_eq!(processor.reply_bytes(), b"\x1b[?997;2n");
    assert!(processor.consume_reply(processor.reply_bytes().len()));
    assert_eq!(processor.reply_bytes(), b"\x1b[48;4;12;0;0t");
}

#[test]
fn capacity_errors_and_reply_backpressure_preserve_text_state_for_recovery() {
    let mut screen = Screen::new(8, 2).unwrap();
    let mut processor = support::processor();
    let mut writer = UnicodeWriter::default();
    let long = String::from("e") + &"\u{301}".repeat(511);
    processor
        .advance(long.as_bytes(), &mut screen, &mut writer)
        .unwrap();
    let before = snapshot(screen.grid());
    let cursor = screen.cursor();
    let error = processor
        .advance("\u{301}".as_bytes(), &mut screen, &mut writer)
        .unwrap_err();
    assert_eq!(error.consumed, 2);
    assert!(matches!(
        error.error,
        ProcessFailure::Text(StorageError::TextTooLong)
    ));
    let error = processor
        .advance(b"x", &mut screen, &mut writer)
        .unwrap_err();
    assert_eq!(error.consumed, 0);
    assert!(matches!(
        error.error,
        ProcessFailure::Text(StorageError::TextTooLong)
    ));
    assert_eq!(snapshot(screen.grid()), before);
    assert_eq!(screen.cursor(), cursor);
    writer.boundary();
    processor.advance(&[], &mut screen, &mut writer).unwrap();
    assert_eq!(text(&screen, 0, 0), CellText::Extended(&long));
    assert_eq!(text(&screen, 0, 1), CellText::Scalar('\u{301}'));
    assert!(matches!(
        writer.write(&mut screen, "水月", StyleId(0)),
        Err(StorageError::InvalidText)
    ));
    processor
        .advance("\u{301}".as_bytes(), &mut screen, &mut writer)
        .unwrap();
    assert_eq!(text(&screen, 0, 1), CellText::Extended("\u{301}\u{301}"));
    processor.reset(&mut screen, &mut writer);
    let input = "❤\x1b[5n\x1b[6n\u{fe0f}".as_bytes();
    let error = processor
        .advance(input, &mut screen, &mut writer)
        .unwrap_err();
    assert!(matches!(error.error, ProcessFailure::ReplyPending));
    assert_eq!(processor.reply_bytes(), b"\x1b[0n");
    assert!(processor.consume_reply(4));
    processor
        .advance(&input[error.consumed..], &mut screen, &mut writer)
        .unwrap();
    assert_eq!(processor.reply_bytes(), b"\x1b[1;2R");
    assert_eq!(text(&screen, 0, 0), CellText::Scalar('❤'));
    assert_eq!(text(&screen, 0, 1), CellText::Scalar('\u{fe0f}'));
    assert_eq!(screen.cursor().column, 2);
}
