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
    ] {
        result.extend(modes.encode_key(key).unwrap());
    }
    result
}

#[test]
fn output_selected_input_modes_encode_keys_and_survive_as_independent_snapshots() {
    let program = b"\x1b[?1h\x1b=\x1b[20h\x1b7\x1b[?1;999l";
    let normal = b"\x1b[A\x1b[B\x1b[C\x1b[D\r0123456789.,-\r";
    let application = b"\x1bOA\x1bOB\x1bOC\x1bOD\r\n\x1bOp\x1bOq\x1bOr\x1bOs\x1bOt\x1bOu\x1bOv\x1bOw\x1bOx\x1bOy\x1bOn\x1bOl\x1bOm\x1bOM";
    let text = |_: &mut Screen, _: &str, _| Ok::<_, StorageError>(());
    for split in 0..=program.len() {
        let mut screen = Screen::new(8, 4).unwrap();
        screen.set_cursor(2, 3);
        let before = super::snapshot(screen.grid());
        let cursor = screen.cursor();
        let mut processor = Processor::default();
        assert_eq!(encoded(processor.input_modes()), normal);
        processor
            .advance(&program[..split], &mut screen, text)
            .unwrap();
        processor
            .advance(&program[split..], &mut screen, text)
            .unwrap();
        let snapshot = processor.input_modes();
        assert_eq!(encoded(snapshot), application);
        assert_eq!(super::snapshot(screen.grid()), before);
        assert_eq!(screen.cursor(), cursor);
        assert_eq!(processor.ignored_sequences(), 1);
        processor
            .advance(b"\x1b[?1l\x1b>\x1b[20l\x1b8", &mut screen, text)
            .unwrap();
        assert_eq!(encoded(processor.input_modes()), normal);
        assert_eq!(encoded(snapshot), application);
        processor
            .advance(b"\x1b[?1h\x1b=\x1b[20h\x1bc", &mut screen, text)
            .unwrap();
        assert_eq!(processor.input_modes(), InputModes::default());
        processor
            .advance(b"\x1b[?1h\x1b=\x1b[20h", &mut screen, text)
            .unwrap();
        processor.reset(&mut screen);
        assert_eq!(processor.input_modes(), InputModes::default());
    }
}

#[test]
fn cursor_keypad_and_new_line_modes_encode_independently_and_reject_invalid_digits() {
    let mut screen = Screen::new(8, 4).unwrap();
    let mut processor = Processor::default();
    let text = |_: &mut Screen, _: &str, _| Ok::<_, StorageError>(());
    processor.advance(b"\x1b[?1h", &mut screen, text).unwrap();
    let cursor = processor.input_modes();
    assert_eq!(cursor.encode_key(Key::Up), Some(&b"\x1bOA"[..]));
    assert_eq!(cursor.encode_key(Key::KeypadDigit(3)), Some(&b"3"[..]));
    assert_eq!(cursor.encode_key(Key::Enter), Some(&b"\r"[..]));
    processor
        .advance(b"\x1b[?1l\x1b=\x1b[20h", &mut screen, text)
        .unwrap();
    let keypad = processor.input_modes();
    assert_eq!(keypad.encode_key(Key::Up), Some(&b"\x1b[A"[..]));
    assert_eq!(keypad.encode_key(Key::KeypadDigit(3)), Some(&b"\x1bOs"[..]));
    assert_eq!(keypad.encode_key(Key::KeypadEnter), Some(&b"\x1bOM"[..]));
    assert_eq!(keypad.encode_key(Key::Enter), Some(&b"\r\n"[..]));
    for digit in 10..=255 {
        assert_eq!(cursor.encode_key(Key::KeypadDigit(digit)), None);
        assert_eq!(keypad.encode_key(Key::KeypadDigit(digit)), None);
    }
}

#[test]
fn editing_and_function_keys_preserve_distinct_sequences_across_all_mode_combinations() {
    for flags in 0..8 {
        let modes = InputModes {
            application_cursor: flags & 1 != 0,
            application_keypad: flags & 2 != 0,
            new_line: flags & 4 != 0,
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
