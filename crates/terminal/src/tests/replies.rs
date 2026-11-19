use super::support;

use crate::{
    CellWidth, ColorScheme, ProcessFailure, Processor, Screen, StorageError, StyleId, reply::Reply,
};

fn text(screen: &mut Screen, value: &str, style: StyleId) -> Result<(), StorageError> {
    if value.is_ascii() {
        screen.write_ascii(value.as_bytes(), style)
    } else {
        screen.write(value, CellWidth::Single, style)
    }
}

fn drain(processor: &mut Processor, output: &mut Vec<u8>) {
    while !processor.reply_bytes().is_empty() {
        let count = processor.reply_bytes().len().min(2);
        output.extend_from_slice(&processor.reply_bytes()[..count]);
        assert!(processor.consume_reply(count));
    }
}

fn drive(processor: &mut Processor, screen: &mut Screen, mut input: &[u8], output: &mut Vec<u8>) {
    loop {
        let result = processor.advance(input, screen, &mut text);
        drain(processor, output);
        match result {
            Ok(consumed) => {
                assert_eq!(consumed, input.len());
                break;
            }
            Err(error) => {
                assert!(matches!(error.error, ProcessFailure::ReplyPending));
                input = &input[error.consumed..];
            }
        }
    }
}

#[test]
fn color_scheme_queries_validate_the_header_and_use_the_host_preference() {
    let wire = b"\x1b[?996n\x1b[6n\x1b]11;#ffffff\x07\x1b[?996n\x1b[996n\x1b[>996n\x1b[?996;0n\x1b[?996:0n\x1b[?996 n\x1b[?997;1n";
    for split in 0..=wire.len() {
        let mut screen = Screen::new(8, 4).unwrap();
        screen.set_cursor(1, 2);
        let mut processor = support::processor();
        processor.set_color_scheme(Some(ColorScheme::Dark));
        let mut output = Vec::new();
        drive(&mut processor, &mut screen, &wire[..split], &mut output);
        drive(&mut processor, &mut screen, &wire[split..], &mut output);
        assert_eq!(output, b"\x1b[?997;1n\x1b[2;3R\x1b[?997;1n");
        assert_eq!(processor.ignored_sequences(), 6);
    }
}

#[test]
fn blocked_color_scheme_reply_keeps_its_value_across_host_updates() {
    let mut screen = Screen::new(8, 4).unwrap();
    let mut processor = support::processor();
    processor
        .advance(b"\x1b[?996n", &mut screen, &mut text)
        .unwrap();
    assert!(processor.reply_bytes().is_empty());
    processor.set_color_scheme(Some(ColorScheme::Dark));
    let wire = b"\x1b[5n\x1b[?996n\x1bc\x1b[?996n";
    let error = processor.advance(wire, &mut screen, &mut text).unwrap_err();
    assert!(matches!(error.error, ProcessFailure::ReplyPending));
    assert_eq!(error.consumed, b"\x1b[5n\x1b[?996n".len());
    let remaining = &wire[error.consumed..];
    assert!(processor.consume_reply(2));
    processor.set_color_scheme(Some(ColorScheme::Light));
    let error = processor
        .advance(remaining, &mut screen, &mut text)
        .unwrap_err();
    assert!(matches!(error.error, ProcessFailure::ReplyPending));
    assert_eq!(error.consumed, 0);
    let mut output = b"\x1b[".to_vec();
    drain(&mut processor, &mut output);
    drive(&mut processor, &mut screen, remaining, &mut output);
    assert_eq!(output, b"\x1b[0n\x1b[?997;1n\x1b[?997;2n");

    processor.set_color_scheme(None);
    output.clear();
    drive(&mut processor, &mut screen, b"\x1b[?996n", &mut output);
    assert!(output.is_empty());
}

#[test]
fn primary_device_attributes_share_reply_order_and_preserve_screen_state() {
    let wire = b"\x1b[c\x1b[6n\x1b[0c\x1b[1c\x1b[0;c\x1b[0:0c\x1b[ c\x1b[?0c\x1b[>0c\x1b[=0c";
    for split in 0..=wire.len() {
        let mut screen = Screen::new(8, 4).unwrap();
        let mut processor = support::processor();
        processor
            .advance(b"\x1b[31m\x1b[3;8Hx", &mut screen, &mut text)
            .unwrap();
        let cells = super::snapshot(screen.grid());
        let cursor = screen.cursor();
        let style = processor.style_id();
        assert!(cursor.wrap_pending);

        let mut output = Vec::new();
        drive(&mut processor, &mut screen, &wire[..split], &mut output);
        drive(&mut processor, &mut screen, &wire[split..], &mut output);
        assert_eq!(output, b"\x1b[?6c\x1b[3;8R\x1b[?6c");
        assert_eq!(processor.ignored_sequences(), 7);
        assert_eq!(processor.style_id(), style);
        assert_eq!(screen.cursor(), cursor);
        assert_eq!(super::snapshot(screen.grid()), cells);
    }
}

#[test]
fn version_query_reports_the_component_build_and_shares_reply_order() {
    let wire = b"\x1b[>q\x1b[5n\x1b[>0q\x1b[>1q\x1b[>0;0q\x1b[>0:0q\x1b[>0 q\x1b[?0q\x1b[5 q";
    let identity = format!("lune-terminal({})", env!("CARGO_PKG_VERSION"));
    let expected = format!("\x1bP>|{identity}\x1b\\\x1b[0n\x1bP>|{identity}\x1b\\");
    for split in 0..=wire.len() {
        let mut screen = Screen::new(8, 4).unwrap();
        let mut processor = support::processor();
        let mut output = Vec::new();
        drive(&mut processor, &mut screen, &wire[..split], &mut output);
        drive(&mut processor, &mut screen, &wire[split..], &mut output);
        assert_eq!(output, expected.as_bytes());
        assert_eq!(processor.ignored_sequences(), 5);
        assert_eq!(screen.cursor_appearance().shape, crate::CursorShape::Bar);
    }
}

#[test]
fn text_area_query_reports_grid_dimensions_across_splits() {
    let wire = b"\x1b[2;4r\x1b[?6h\x1b[18t\x1b[?47h\x1b[18t\x1b[17t\x1b[19t\x1b[?18t\x1b[18;0t\x1b[18:0t\x1b[18 t\x1b[18n\x1b[6t";
    for split in 0..=wire.len() {
        let mut screen = Screen::new(12, 5).unwrap();
        let mut processor = support::processor();
        let mut output = Vec::new();
        drive(&mut processor, &mut screen, &wire[..split], &mut output);
        drive(&mut processor, &mut screen, &wire[split..], &mut output);
        assert_eq!(output, b"\x1b[8;5;12t\x1b[8;5;12t");
        assert_eq!(processor.ignored_sequences(), 8);
    }
}

#[test]
fn blocked_text_area_query_retains_dimensions_at_query_time() {
    let mut screen = Screen::new(12, 5).unwrap();
    let mut processor = support::processor();
    let wire = b"\x1b[5n\x1b[18t\x1b[18t";
    let error = processor.advance(wire, &mut screen, &mut text).unwrap_err();
    assert!(matches!(error.error, ProcessFailure::ReplyPending));
    assert_eq!(error.consumed, 9);
    assert_eq!(processor.reply_bytes(), b"\x1b[0n");
    screen.resize(131, 26, StyleId(0)).unwrap();
    assert!(processor.consume_reply(2));
    let remaining = &wire[error.consumed..];
    let error = processor
        .advance(remaining, &mut screen, &mut text)
        .unwrap_err();
    assert!(matches!(error.error, ProcessFailure::ReplyPending));
    assert_eq!(error.consumed, 0);
    let mut output = b"\x1b[".to_vec();
    drain(&mut processor, &mut output);
    drive(&mut processor, &mut screen, remaining, &mut output);
    assert_eq!(output, b"\x1b[0n\x1b[8;5;12t\x1b[8;26;131t");
}

#[test]
fn status_strings_capture_rendition_margins_and_cursor_style_across_chunks() {
    for (rendition, report) in [
        (b"".as_slice(), b"0m".as_slice()),
        (
            b"\x1b[1;2;3;4:3;5;7;8;9;38;2;255;255;255;48;2;255;255;255;58;2;255;255;255m",
            b"0;1;2;3;4:3;5;7;8;9;38;2;255;255;255;48;2;255;255;255;58;2;255;255;255m",
        ),
        (b"\x1b[4;38:5:255;48:5:0m", b"0;4;38;5;255;48;5;0m"),
        (b"\x1b[4:2m", b"0;21m"),
        (b"\x1b[4:4;58:5:42m\x1b[59m", b"0;4:4m"),
        (b"\x1b[4:5m", b"0;4:5m"),
        (b"\x1b[58:5:42m", b"0;58;5;42m"),
    ] {
        let mut wire = rendition.to_vec();
        wire.extend(b"\x1bP$qm\x1b\\\x1b[2;4r\x1b[?6h\x1bP$qr\x1b\\\x1b[?25l\x1b[6 q\x1bP$q q\x1b\\\x1b[!p\x1bP$qm\x1b\\\x1bP$qr\x1b\\\x1bP$q q\x1b\\");
        let mut expected = b"\x1bP1$r".to_vec();
        expected.extend(report);
        expected.extend(b"\x1b\\\x1bP1$r2;4r\x1b\\\x1bP1$r6 q\x1b\\\x1bP1$r0m\x1b\\\x1bP1$r1;4r\x1b\\\x1bP1$r1 q\x1b\\");
        for chunk in [1, 2, 17, wire.len()] {
            let mut processor = support::processor();
            let mut screen = Screen::new(8, 4).unwrap();
            let before = super::snapshot(screen.grid());
            let mut output = Vec::new();
            for bytes in wire.chunks(chunk) {
                drive(&mut processor, &mut screen, bytes, &mut output);
            }
            assert_eq!(output, expected);
            assert_eq!(super::snapshot(screen.grid()), before);
            assert_eq!(
                (processor.parse_errors(), processor.ignored_sequences()),
                (0, 0)
            );
        }
    }
}

#[test]
fn status_string_rejections_cancellation_and_finish_preserve_request_boundaries() {
    let mut wire = b"\x1bP$q\x1b\\\x1bP$qxyz\x1b\\\x1bP$qm\x07\x1b\\\x1bP$qm\x18\x1bP1$qm\x1b\\\x1bP?$qm\x1b\\\x1bP$qm\x1b[31m\x1bP$qm".to_vec();
    wire.extend(vec![b'x'; 4096]);
    wire.extend(b"\x1b\\\x1bP$q qx\x1b\\\x1bP$q r\x1b\\\x1bP$qm");
    for chunk in [1, 17, wire.len()] {
        let mut processor = support::processor();
        let mut screen = Screen::new(8, 4).unwrap();
        let mut output = Vec::new();
        for bytes in wire.chunks(chunk) {
            drive(&mut processor, &mut screen, bytes, &mut output);
        }
        processor.finish(&mut screen, &mut text).unwrap();
        processor.finish(&mut screen, &mut text).unwrap();
        drain(&mut processor, &mut output);
        assert_eq!(output, b"\x1bP0$r\x1b\\".repeat(6));
        assert_eq!(
            (processor.parse_errors(), processor.ignored_sequences()),
            (1, 0)
        );
        drive(&mut processor, &mut screen, b"\x1bP$qm\x1b\\", &mut output);
        assert!(output.ends_with(b"\x1bP1$r0;38;5;1m\x1b\\"));
        processor
            .advance(b"\x1bP$q ", &mut screen, &mut text)
            .unwrap();
        processor.reset(&mut screen, &mut text);
        let mut output = Vec::new();
        drive(&mut processor, &mut screen, b"\x1bP$qm\x1b\\", &mut output);
        assert_eq!(output, b"\x1bP1$r0m\x1b\\");
    }
}

#[test]
fn status_strings_freeze_values_and_share_the_reply_slot_with_prior_queries() {
    use crate::{CursorAppearance, CursorShape};
    let mut processor = Processor::new(2, support::palette()).unwrap();
    let mut screen = Screen::new(8, 4).unwrap();
    processor
        .advance(b"\x1b[31m\x1b[2;4r\x1b[5n", &mut screen, &mut text)
        .unwrap();
    let red = processor.style_id();
    let wire = b"\x1bP$qr\x1b\\TAIL";
    let failure = processor.advance(wire, &mut screen, &mut text).unwrap_err();
    assert!(matches!(failure.error, ProcessFailure::ReplyPending));
    assert_eq!(failure.consumed, wire.len() - 4);
    screen.set_scroll_region(0..2).unwrap();
    screen.resize(8, 3, StyleId(0)).unwrap();
    assert!(processor.consume_reply(2));
    assert_eq!(processor.reply_bytes(), b"0n");
    let failure = processor.advance(&[], &mut screen, &mut text).unwrap_err();
    assert_eq!(failure.consumed, 0);
    assert!(processor.consume_reply(2));
    processor.advance(&[], &mut screen, &mut text).unwrap();
    assert_eq!(processor.reply_bytes(), b"\x1bP1$r2;4r\x1b\\");
    screen.set_cursor_appearance(CursorAppearance {
        visible: false,
        blinking: false,
        shape: CursorShape::Bar,
    });
    assert!(
        processor
            .advance(b"\x1bP$q q\x1b\\", &mut screen, &mut text)
            .is_err()
    );
    screen.set_cursor_appearance(CursorAppearance::default());
    let mut output = Vec::new();
    drain(&mut processor, &mut output);
    processor.advance(&[], &mut screen, &mut text).unwrap();
    assert_eq!(processor.reply_bytes(), b"\x1bP1$r6 q\x1b\\");
    assert!(
        processor
            .advance(b"\x1bP$qm\x1b\\", &mut screen, &mut text)
            .is_err()
    );
    drain(&mut processor, &mut output);
    processor.advance(&[], &mut screen, &mut text).unwrap();
    let captured = processor.reply_bytes().to_vec();
    assert_eq!(captured, b"\x1bP1$r0;38;5;1m\x1b\\");
    processor.advance(b"\x1bc", &mut screen, &mut text).unwrap();
    assert_eq!(processor.collect_styles(&screen, []).unwrap(), 1);
    assert!(processor.styles().get(red).is_none());
    assert_eq!(processor.reply_bytes(), captured);
    processor
        .advance(&wire[wire.len() - 4..], &mut screen, &mut text)
        .unwrap();
    drain(&mut processor, &mut output);
    assert_eq!(
        output,
        b"\x1bP1$r2;4r\x1b\\\x1bP1$r6 q\x1b\\\x1bP1$r0;38;5;1m\x1b\\"
    );
}

#[test]
fn query_replies_preserve_wire_order_origin_coordinates_and_wrapping_across_splits() {
    let input = b"\x1b[5nabc\x1b[6n\x1b[2;4r\x1b[?6h\x1b[2;8H!\x1b[?6nZ\x1b[6n\x1b[?6l\x1b[4;7H\x1b[?6n\x1b[n\x1b[0n\x1b[?5n\x1b[?6;1n\x1b[?6:0n\x1b[?6!n\x1b[6;5n\x1b[6:0n\x1b[6!n\x1b[>6n";
    for split in 0..=input.len() {
        let mut screen = Screen::new(8, 4).unwrap();
        let mut processor = support::processor();
        let mut output = Vec::new();
        drive(&mut processor, &mut screen, &input[..split], &mut output);
        drive(&mut processor, &mut screen, &input[split..], &mut output);
        processor.finish(&mut screen, &mut text).unwrap();
        processor.finish(&mut screen, &mut text).unwrap();
        drain(&mut processor, &mut output);
        assert_eq!(output, b"\x1b[0n\x1b[1;4R\x1b[?2;8R\x1b[3;2R\x1b[?4;7R");
        assert_eq!(
            (processor.parse_errors(), processor.ignored_sequences()),
            (0, 10)
        );
        assert_eq!(screen.cursor().row, 3);
        assert_eq!(screen.cursor().column, 6);
        assert!(!screen.cursor().wrap_pending);
        assert_eq!(
            screen.grid().row(2).unwrap().text(7),
            Some(crate::CellText::Scalar('!'))
        );
        assert_eq!(
            screen.grid().row(3).unwrap().text(0),
            Some(crate::CellText::Scalar('Z'))
        );
    }
}

#[test]
fn blocked_replies_retain_captured_positions_and_partial_delivery_until_retry_or_reset() {
    let mut screen = Screen::new(8, 4).unwrap();
    let mut processor = support::processor();
    let input = b"\x1b[5nabc\x1b[?6nTAIL";
    let error = processor
        .advance(input, &mut screen, &mut text)
        .unwrap_err();
    assert!(matches!(error.error, ProcessFailure::ReplyPending));
    assert_eq!(error.consumed, 12);
    let suffix = &input[error.consumed..];
    assert_eq!(processor.reply_bytes(), b"\x1b[0n");
    assert!(!processor.consume_reply(usize::MAX));
    assert_eq!(processor.reply_bytes(), b"\x1b[0n");
    assert!(processor.consume_reply(0));
    assert!(processor.consume_reply(2));
    assert_eq!(processor.reply_bytes(), b"0n");
    let checkpoint = super::snapshot(screen.grid());
    let cursor = screen.cursor();
    for input in [suffix, &b"A"[..]] {
        let error = processor
            .advance(input, &mut screen, &mut text)
            .unwrap_err();
        assert!(matches!(error.error, ProcessFailure::ReplyPending));
        assert_eq!(error.consumed, 0);
        assert!(matches!(
            processor.finish(&mut screen, &mut text),
            Err(ProcessFailure::ReplyPending)
        ));
        assert_eq!(super::snapshot(screen.grid()), checkpoint);
        assert_eq!(screen.cursor(), cursor);
        assert_eq!(processor.reply_bytes(), b"0n");
    }
    screen.resize(4, 2, StyleId(0)).unwrap();
    screen.set_cursor(1, 3);
    assert!(processor.consume_reply(2));
    assert!(processor.reply_bytes().is_empty());
    assert_eq!(processor.advance(&[], &mut screen, &mut text).unwrap(), 0);
    assert_eq!(processor.reply_bytes(), b"\x1b[?1;4R");
    processor.finish(&mut screen, &mut text).unwrap();
    assert_eq!(processor.reply_bytes(), b"\x1b[?1;4R");
    processor.advance(suffix, &mut screen, &mut text).unwrap();
    assert_eq!(
        screen.grid().row(0).unwrap().text(3),
        Some(crate::CellText::Scalar('T'))
    );
    assert_eq!(
        screen.grid().row(1).unwrap().text(0),
        Some(crate::CellText::Scalar('A'))
    );
    let error = processor
        .advance(b"\x1b[5n", &mut screen, &mut text)
        .unwrap_err();
    assert!(matches!(error.error, ProcessFailure::ReplyPending));
    processor.reset(&mut screen, &mut text);
    assert!(processor.reply_bytes().is_empty());
    processor.advance(b"ok", &mut screen, &mut text).unwrap();
    assert!(processor.reply_bytes().is_empty());
    processor
        .advance(b"\x1b[5n\x1bc", &mut screen, &mut text)
        .unwrap();
    assert_eq!(processor.reply_bytes(), b"\x1b[0n");
    processor.finish(&mut screen, &mut text).unwrap();
    assert_eq!(processor.reply_bytes(), b"\x1b[0n");
    assert!(processor.consume_reply(4));
    processor.finish(&mut screen, &mut text).unwrap();
    assert!(processor.reply_bytes().is_empty());
}

#[test]
fn fixed_reply_storage_encodes_coordinate_digit_boundaries() {
    let bounds = [1, 9, 10, 99, 100, 999, 1000, 9999, 10000, 65535];
    let mut reply = Reply::default();
    for row in bounds {
        for column in bounds {
            for (private, marker) in [(false, ""), (true, "?")] {
                reply.position(row, column, private);
                assert_eq!(
                    reply.bytes(),
                    format!("\x1b[{marker}{row};{column}R").as_bytes()
                );
            }
        }
    }
}

#[test]
fn mode_queries_report_defaults_changes_and_unknown_selectors_across_splits() {
    for (private, selector, default) in [
        (false, 4, 2),
        (false, 20, 2),
        (true, 1, 2),
        (true, 5, 2),
        (true, 6, 2),
        (true, 7, 1),
        (true, 8, 1),
        (true, 12, 1),
        (true, 25, 1),
        (true, 47, 2),
        (true, 66, 2),
        (true, 67, 2),
        (true, 1000, 2),
        (true, 1002, 2),
        (true, 1003, 2),
        (true, 1004, 2),
        (true, 1006, 2),
        (true, 1047, 2),
        (true, 1049, 2),
        (true, 2004, 2),
    ] {
        let marker = if private { "?" } else { "" };
        let input = format!(
            "\x1b[{marker}{selector}$p\x1b[{marker}{selector}h\x1b[{marker}{selector}$p\x1b[{marker}{selector}l\x1b[{marker}{selector}$p"
        );
        let expected = format!(
            "\x1b[{marker}{selector};{default}$y\x1b[{marker}{selector};1$y\x1b[{marker}{selector};2$y"
        );
        for split in 0..=input.len() {
            let mut screen = Screen::new(8, 4).unwrap();
            let mut processor = support::processor();
            let mut output = Vec::new();
            drive(
                &mut processor,
                &mut screen,
                &input.as_bytes()[..split],
                &mut output,
            );
            drive(
                &mut processor,
                &mut screen,
                &input.as_bytes()[split..],
                &mut output,
            );
            assert_eq!(output, expected.as_bytes(), "{private} {selector} {split}");
            assert_eq!(
                (processor.parse_errors(), processor.ignored_sequences()),
                (0, 0)
            );
        }
    }
    let input = b"\x1b[$p\x1b[?$p\x1b[999$p\x1b[?65535$p\x1b[?1048$p\x1b[1004$p\x1b[?1004;1$p\x1b[?1004:1$p\x1b[>1004$p\x1b[?1004$$p\x1b[?1004p\x1b[?1004 $p";
    for split in 0..=input.len() {
        let mut screen = Screen::new(8, 4).unwrap();
        screen.write_ascii(b"unchanged", StyleId(0)).unwrap();
        let before = super::snapshot(screen.grid());
        let cursor = screen.cursor();
        let mut processor = support::processor();
        let mut output = Vec::new();
        drive(&mut processor, &mut screen, &input[..split], &mut output);
        drive(&mut processor, &mut screen, &input[split..], &mut output);
        assert_eq!(
            output,
            b"\x1b[0;0$y\x1b[?0;0$y\x1b[999;0$y\x1b[?65535;0$y\x1b[?1048;0$y\x1b[1004;0$y"
        );
        assert_eq!(
            (processor.parse_errors(), processor.ignored_sequences()),
            (0, 6)
        );
        assert_eq!(screen.cursor(), cursor);
        assert_eq!(super::snapshot(screen.grid()), before);
        assert_eq!(processor.input_modes(), crate::InputModes::default());
    }
}

#[test]
fn mode_replies_capture_state_before_backpressure_and_follow_stream_reset_rules() {
    let mut screen = Screen::new(8, 4).unwrap();
    let mut processor = support::processor();
    let input = b"\x1b[5n\x1b[?7$p\x1b[?7$pTAIL";
    let error = processor
        .advance(input, &mut screen, &mut text)
        .unwrap_err();
    assert!(matches!(error.error, ProcessFailure::ReplyPending));
    assert_eq!(error.consumed, b"\x1b[5n\x1b[?7$p".len());
    let suffix = &input[error.consumed..];
    screen.set_auto_wrap(false);
    screen.resize(6, 3, StyleId(0)).unwrap();
    assert!(processor.consume_reply(1));
    let error = processor
        .advance(suffix, &mut screen, &mut text)
        .unwrap_err();
    assert!(matches!(error.error, ProcessFailure::ReplyPending));
    assert_eq!(error.consumed, 0);
    assert!(matches!(
        processor.finish(&mut screen, &mut text),
        Err(ProcessFailure::ReplyPending)
    ));
    assert_eq!(processor.reply_bytes(), b"[0n");
    assert!(processor.consume_reply(3));
    processor.advance(&[], &mut screen, &mut text).unwrap();
    assert_eq!(processor.reply_bytes(), b"\x1b[?7;1$y");
    processor.finish(&mut screen, &mut text).unwrap();
    assert!(processor.consume_reply(2));
    assert!(!processor.consume_reply(usize::MAX));
    assert_eq!(processor.reply_bytes(), b"?7;1$y");
    let mut output = Vec::new();
    drain(&mut processor, &mut output);
    drive(&mut processor, &mut screen, suffix, &mut output);
    assert_eq!(output, b"?7;1$y\x1b[?7;2$y");
    assert_eq!(
        screen.grid().row(0).unwrap().text(0),
        Some(crate::CellText::Scalar('T'))
    );
    processor
        .advance(b"\x1b[?1004h\x1b[?1004$p\x1bc", &mut screen, &mut text)
        .unwrap();
    assert_eq!(processor.input_modes(), crate::InputModes::default());
    assert_eq!(processor.reply_bytes(), b"\x1b[?1004;1$y");
    processor.reset(&mut screen, &mut text);
    assert!(processor.reply_bytes().is_empty());
    processor
        .advance(b"\x1b[?1004$p", &mut screen, &mut text)
        .unwrap();
    assert_eq!(processor.reply_bytes(), b"\x1b[?1004;2$y");
}
