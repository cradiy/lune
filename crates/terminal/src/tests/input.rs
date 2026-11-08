use crate::{InputModes, Key, Processor, Screen, StorageError};

fn encoded(modes: InputModes) -> Vec<u8> {
    let mut result = Vec::new();
    for key in [Key::Up, Key::Down, Key::Right, Key::Left, Key::Enter] {
        result.extend(modes.encode_key(key).unwrap());
    }
    for digit in 0..10 {
        result.extend(modes.encode_key(Key::KeypadDigit(digit)).unwrap());
    }
    for key in [
        Key::KeypadDecimal,
        Key::KeypadComma,
        Key::KeypadMinus,
        Key::KeypadEnter,
        Key::Tab,
        Key::Escape,
        Key::Backspace,
        Key::KeypadMultiply,
        Key::KeypadAdd,
        Key::KeypadDivide,
        Key::KeypadEqual,
    ] {
        result.extend(modes.encode_key(key).unwrap());
    }
    result
}

#[test]
fn output_selected_input_modes_encode_keys_and_survive_as_independent_snapshots() {
    let program = b"\x1b[?1;66;67;2004h\x1b[20h\x1b7\x1b[?1;2004;999l";
    let normal = b"\x1b[A\x1b[B\x1b[C\x1b[D\r0123456789.,-\r\t\x1b\x7f*+/=";
    let application = b"\x1bOA\x1bOB\x1bOC\x1bOD\r\n\x1bOp\x1bOq\x1bOr\x1bOs\x1bOt\x1bOu\x1bOv\x1bOw\x1bOx\x1bOy\x1bOn\x1bOl\x1bOm\x1bOM\t\x1b\x08\x1bOj\x1bOk\x1bOo\x1bOX";
    let mut text = |_: &mut Screen, _: &str, _| Ok::<_, StorageError>(());
    for split in 0..=program.len() {
        let mut screen = Screen::new(8, 4).unwrap();
        screen.set_cursor(2, 3);
        let before = super::snapshot(screen.grid());
        let cursor = screen.cursor();
        let mut processor = Processor::default();
        assert_eq!(encoded(processor.input_modes()), normal);
        processor
            .advance(&program[..split], &mut screen, &mut text)
            .unwrap();
        processor
            .advance(&program[split..], &mut screen, &mut text)
            .unwrap();
        let snapshot = processor.input_modes();
        assert!(snapshot.bracketed_paste);
        let mut paste = snapshot.encode_paste("水\nx");
        assert!(paste.consume(2));
        assert_eq!(encoded(snapshot), application);
        assert_eq!(super::snapshot(screen.grid()), before);
        assert_eq!(screen.cursor(), cursor);
        assert_eq!(processor.ignored_sequences(), 1);
        processor
            .advance(b"\x1b[?1049h\x1b[?1049l", &mut screen, &mut text)
            .unwrap();
        assert_eq!(processor.input_modes(), snapshot);
        processor
            .advance(b"\x1b[?1;66;67;2004l\x1b[20l\x1b8", &mut screen, &mut text)
            .unwrap();
        assert_eq!(encoded(processor.input_modes()), normal);
        assert!(!processor.input_modes().bracketed_paste);
        assert_eq!(encoded(snapshot), application);
        processor
            .advance(b"\x1b[?1;66;67;2004h\x1b[20h\x1bc", &mut screen, &mut text)
            .unwrap();
        assert_eq!(processor.input_modes(), InputModes::default());
        processor
            .advance(b"\x1b[?1;66;67;2004h\x1b[20h", &mut screen, &mut text)
            .unwrap();
        processor.reset(&mut screen, &mut text);
        assert_eq!(processor.input_modes(), InputModes::default());
        let mut remaining = Vec::<u8>::new();
        while !paste.is_empty() {
            remaining.extend(paste.bytes());
            assert!(paste.consume(paste.bytes().len()));
        }
        assert_eq!(remaining, "200~水\nx\x1b[201~".as_bytes());
    }
}

#[test]
fn paste_streams_preserve_payloads_and_progress_under_partial_writes() {
    for bracketed_paste in [false, true] {
        let modes = InputModes {
            bracketed_paste,
            ..InputModes::default()
        };
        for payload in ["", "a水e\u{301}\n\r\t\0"] {
            let mut expected = Vec::<u8>::new();
            if bracketed_paste {
                expected.extend(b"\x1b[200~");
            }
            expected.extend(payload.as_bytes());
            if bracketed_paste {
                expected.extend(b"\x1b[201~");
            }
            for limit in 1..=9 {
                let mut paste = modes.encode_paste(payload);
                let mut actual = Vec::<u8>::new();
                while !paste.is_empty() {
                    let before = paste;
                    assert!(paste.consume(0));
                    assert!(!paste.consume(paste.bytes().len() + 1));
                    assert_eq!(paste.bytes(), before.bytes());
                    let count = limit.min(paste.bytes().len());
                    actual.extend(&paste.bytes()[..count]);
                    assert!(paste.consume(count));
                }
                assert_eq!(actual, expected);
                assert!(paste.bytes().is_empty());
                assert!(paste.consume(0));
                assert!(!paste.consume(1));
            }
        }
        let payload = "水".repeat(4096);
        let mut paste = modes.encode_paste(&payload);
        if bracketed_paste {
            assert!(paste.consume(6));
        }
        assert_eq!(paste.bytes().as_ptr(), payload.as_ptr());
        assert_eq!(paste.bytes().len(), payload.len());
        assert!(paste.consume(payload.len()));
        if bracketed_paste {
            assert_eq!(paste.bytes(), b"\x1b[201~");
            assert!(paste.consume(6));
        }
        assert!(paste.is_empty());
    }
}

#[test]
fn cursor_keypad_and_new_line_modes_encode_independently_and_reject_invalid_digits() {
    let mut screen = Screen::new(8, 4).unwrap();
    let mut processor = Processor::default();
    let mut text = |_: &mut Screen, _: &str, _| Ok::<_, StorageError>(());
    processor
        .advance(b"\x1b[?1h", &mut screen, &mut text)
        .unwrap();
    let cursor = processor.input_modes();
    assert_eq!(cursor.encode_key(Key::Up), Some(&b"\x1bOA"[..]));
    assert_eq!(cursor.encode_key(Key::KeypadDigit(3)), Some(&b"3"[..]));
    assert_eq!(cursor.encode_key(Key::Enter), Some(&b"\r"[..]));
    processor
        .advance(b"\x1b[?1l\x1b=\x1b[20h", &mut screen, &mut text)
        .unwrap();
    let keypad = processor.input_modes();
    assert_eq!(keypad.encode_key(Key::Up), Some(&b"\x1b[A"[..]));
    assert_eq!(keypad.encode_key(Key::KeypadDigit(3)), Some(&b"\x1bOs"[..]));
    assert_eq!(keypad.encode_key(Key::KeypadEnter), Some(&b"\x1bOM"[..]));
    assert_eq!(keypad.encode_key(Key::Enter), Some(&b"\r\n"[..]));
    assert_eq!(keypad.encode_key(Key::Backspace), Some(&b"\x7f"[..]));
    processor
        .advance(b"\x1b[?67h\x1b>", &mut screen, &mut text)
        .unwrap();
    let numeric = processor.input_modes();
    assert_eq!(numeric.encode_key(Key::Backspace), Some(&b"\x08"[..]));
    assert_eq!(numeric.encode_key(Key::KeypadMultiply), Some(&b"*"[..]));
    assert_eq!(numeric.encode_key(Key::Enter), Some(&b"\r\n"[..]));
    processor
        .advance(b"\x1b[?66;67;999l", &mut screen, &mut text)
        .unwrap();
    assert_eq!(processor.input_modes(), numeric);
    for digit in 10..=255 {
        assert_eq!(cursor.encode_key(Key::KeypadDigit(digit)), None);
        assert_eq!(keypad.encode_key(Key::KeypadDigit(digit)), None);
    }
}

#[test]
fn editing_and_function_keys_preserve_distinct_sequences_across_all_mode_combinations() {
    for flags in 0..32 {
        let modes = InputModes {
            application_cursor: flags & 1 != 0,
            application_keypad: flags & 2 != 0,
            new_line: flags & 4 != 0,
            backarrow: flags & 8 != 0,
            bracketed_paste: flags & 16 != 0,
        };
        let mut actual = Vec::<u8>::new();
        for key in [
            Key::Home,
            Key::End,
            Key::Insert,
            Key::Delete,
            Key::PageUp,
            Key::PageDown,
        ] {
            actual.extend(modes.encode_key(key).unwrap());
        }
        for number in 1..=12 {
            actual.extend(modes.encode_key(Key::Function(number)).unwrap());
        }
        let mut expected = Vec::from(if modes.application_cursor {
            &b"\x1bOH\x1bOF"[..]
        } else {
            &b"\x1b[H\x1b[F"[..]
        });
        expected.extend(b"\x1b[2~\x1b[3~\x1b[5~\x1b[6~\x1bOP\x1bOQ\x1bOR\x1bOS\x1b[15~\x1b[17~\x1b[18~\x1b[19~\x1b[20~\x1b[21~\x1b[23~\x1b[24~");
        assert_eq!(actual, expected);
        assert_eq!(modes.encode_key(Key::Function(0)), None);
        for number in 13..=255 {
            assert_eq!(modes.encode_key(Key::Function(number)), None);
        }
    }
}
