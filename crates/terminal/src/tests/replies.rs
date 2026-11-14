use crate::{CellWidth, ProcessFailure, Processor, Screen, StorageError, StyleId, reply::Reply};

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
fn query_replies_preserve_wire_order_origin_coordinates_and_wrapping_across_splits() {
    let input = b"\x1b[5nabc\x1b[6n\x1b[2;4r\x1b[?6h\x1b[2;8H!\x1b[6nZ\x1b[6n\x1b[?6l\x1b[4;7H\x1b[6n\x1b[n\x1b[0n\x1b[?5n\x1b[?6n\x1b[6;5n\x1b[6:0n\x1b[6!n\x1b[>6n";
    for split in 0..=input.len() {
        let mut screen = Screen::new(8, 4).unwrap();
        let mut processor = Processor::default();
        let mut output = Vec::new();
        drive(&mut processor, &mut screen, &input[..split], &mut output);
        drive(&mut processor, &mut screen, &input[split..], &mut output);
        processor.finish(&mut screen, &mut text).unwrap();
        processor.finish(&mut screen, &mut text).unwrap();
        drain(&mut processor, &mut output);
        assert_eq!(output, b"\x1b[0n\x1b[1;4R\x1b[2;8R\x1b[3;2R\x1b[4;7R");
        assert_eq!(
            (processor.parse_errors(), processor.ignored_sequences()),
            (0, 8)
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
    let mut processor = Processor::default();
    let input = b"\x1b[5nabc\x1b[6nTAIL";
    let error = processor
        .advance(input, &mut screen, &mut text)
        .unwrap_err();
    assert!(matches!(error.error, ProcessFailure::ReplyPending));
    assert_eq!(error.consumed, 11);
    let suffix = &input[error.consumed..];
    assert_eq!(processor.reply_bytes(), b"\x1b[0n");
    assert!(!processor.consume_reply(usize::MAX));
    assert_eq!(processor.reply_bytes(), b"\x1b[0n");
    assert!(processor.consume_reply(0));
    assert!(processor.consume_reply(2));
    assert_eq!(processor.reply_bytes(), b"0n");
    let checkpoint = super::snapshot(screen.grid());
    let cursor = screen.cursor();
    for _ in 0..2 {
        let error = processor
            .advance(suffix, &mut screen, &mut text)
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
    assert_eq!(processor.reply_bytes(), b"\x1b[1;4R");
    processor.finish(&mut screen, &mut text).unwrap();
    assert_eq!(processor.reply_bytes(), b"\x1b[1;4R");
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
    for row in bounds {
        for column in bounds {
            let reply = Reply::position(row, column);
            assert_eq!(reply.bytes(), format!("\x1b[{row};{column}R").as_bytes());
        }
    }
}

#[test]
fn mode_queries_report_defaults_changes_and_unknown_selectors_across_splits() {
    for (private, selector, default) in [
        (false, 4, 2),
        (false, 20, 2),
        (true, 1, 2),
        (true, 6, 2),
        (true, 7, 1),
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
            let mut processor = Processor::default();
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
        let mut processor = Processor::default();
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
    let mut processor = Processor::default();
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
