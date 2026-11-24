use super::support;

use crate::{InputModes, Key, KeySequence, Modifiers, Screen, StorageError};

#[test]
fn auto_repeat_is_session_state_with_a_hard_reset_default() {
    let mut processor = support::processor();
    let mut screen = Screen::new(4, 3).unwrap();
    let mut sink = |_: &mut Screen, _: &str, _| Ok::<_, StorageError>(());
    assert!(processor.auto_repeat_enabled());
    processor
        .advance(b"\x1b[?8;999l\x1b[8l", &mut screen, &mut sink)
        .unwrap();
    assert!(processor.auto_repeat_enabled());
    assert_eq!(processor.ignored_sequences(), 2);

    processor
        .advance(
            b"\x1b7\x1b[?8;1l\x1b8\x1b[?47h\x1b[!p",
            &mut screen,
            &mut sink,
        )
        .unwrap();
    let disabled = processor.input_modes();
    assert!(!processor.auto_repeat_enabled());
    assert_eq!(
        disabled
            .encode_key(Key::Up, Modifiers::default())
            .unwrap()
            .as_bytes(),
        b"\x1b[A"
    );
    processor
        .advance(b"\x1b[?47l\x1bc", &mut screen, &mut sink)
        .unwrap();
    assert!(processor.auto_repeat_enabled());
}

#[test]
fn character_keys_preserve_ascii_and_encode_control_aliases_with_alt_prefixes() {
    let controls = [
        ("\0 @`2", 0),
        ("\x01Aa", 1),
        ("\x02Bb", 2),
        ("\x03Cc", 3),
        ("\x04Dd", 4),
        ("\x05Ee", 5),
        ("\x06Ff", 6),
        ("\x07Gg", 7),
        ("\x08Hh", 8),
        ("\tIi", 9),
        ("\nJj", 10),
        ("\x0bKk", 11),
        ("\x0cLl", 12),
        ("\rMm", 13),
        ("\x0eNn", 14),
        ("\x0fOo", 15),
        ("\x10Pp", 16),
        ("\x11Qq", 17),
        ("\x12Rr", 18),
        ("\x13Ss", 19),
        ("\x14Tt", 20),
        ("\x15Uu", 21),
        ("\x16Vv", 22),
        ("\x17Ww", 23),
        ("\x18Xx", 24),
        ("\x19Yy", 25),
        ("\x1aZz", 26),
        ("\x1b[{3", 27),
        ("\x1c\\|4", 28),
        ("\x1d]}5", 29),
        ("\x1e^~6", 30),
        ("\x1f_/7", 31),
        ("\x7f?8", 127),
    ];
    for flags in 0..8 {
        let modifiers = Modifiers {
            shift: flags & 1 != 0,
            alt: flags & 2 != 0,
            control: flags & 4 != 0,
        };
        for ascii in 0..=127 {
            let character = char::from(ascii);
            let expected = if modifiers.control {
                controls
                    .iter()
                    .find_map(|(aliases, byte)| aliases.contains(character).then_some(*byte))
            } else {
                Some(ascii)
            };
            let actual = KeySequence::from_character(character, modifiers);
            if let Some(byte) = expected {
                let bytes = [0x1b, byte];
                assert_eq!(
                    actual.unwrap().as_bytes(),
                    &bytes[usize::from(!modifiers.alt)..],
                    "{character:?} {modifiers:?}"
                );
            } else {
                assert!(actual.is_none(), "{character:?} {modifiers:?}");
            }
        }
    }
}

#[test]
fn character_keys_preserve_utf8_boundaries_and_owned_bytes() {
    let characters = [
        '\u{80}',
        '\u{9f}',
        '\u{7ff}',
        '\u{800}',
        '\u{d7ff}',
        '\u{e000}',
        '\u{ffff}',
        '\u{10000}',
        '\u{10ffff}',
        'é',
        '水',
        '🦀',
        '\u{301}',
        '\u{200d}',
    ];
    let retained = KeySequence::from_character(
        '🦀',
        Modifiers {
            alt: true,
            ..Modifiers::default()
        },
    )
    .unwrap();
    for character in characters {
        for shift in [false, true] {
            for alt in [false, true] {
                let modifiers = Modifiers {
                    shift,
                    alt,
                    control: false,
                };
                let sequence = KeySequence::from_character(character, modifiers).unwrap();
                let bytes = sequence.as_bytes();
                assert_eq!(bytes.len(), character.len_utf8() + usize::from(alt));
                if alt {
                    assert_eq!(bytes[0], 0x1b);
                }
                let text = std::str::from_utf8(&bytes[usize::from(alt)..]).unwrap();
                let mut decoded = text.chars();
                assert_eq!(decoded.next(), Some(character));
                assert_eq!(decoded.next(), None);
                assert!(
                    KeySequence::from_character(
                        character,
                        Modifiers {
                            control: true,
                            ..modifiers
                        }
                    )
                    .is_none()
                );
            }
        }
    }
    assert_eq!(retained.as_bytes(), b"\x1b\xf0\x9f\xa6\x80");
}

fn assert_key(modes: InputModes, key: Key, modifiers: Modifiers, expected: &[u8]) {
    let sequence = modes.encode_key(key, modifiers).unwrap();
    assert_eq!(sequence.as_bytes(), expected, "{key:?} {modifiers:?}");
}

fn encoded(modes: InputModes) -> Vec<u8> {
    let mut result = Vec::new();
    for key in [Key::Up, Key::Down, Key::Right, Key::Left, Key::Enter] {
        result.extend_from_slice(
            modes
                .encode_key(key, Modifiers::default())
                .unwrap()
                .as_bytes(),
        );
    }
    for digit in 0..10 {
        result.extend_from_slice(
            modes
                .encode_key(Key::KeypadDigit(digit), Modifiers::default())
                .unwrap()
                .as_bytes(),
        );
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
        result.extend_from_slice(
            modes
                .encode_key(key, Modifiers::default())
                .unwrap()
                .as_bytes(),
        );
    }
    result
}

#[test]
fn output_selected_input_modes_encode_keys_and_survive_as_independent_snapshots() {
    let program = b"\x1b[?1;66;67;1004;2004h\x1b[20h\x1b7\x1b[?1;1004;2004;999l";
    let normal = b"\x1b[A\x1b[B\x1b[C\x1b[D\r0123456789.,-\r\t\x1b\x7f*+/=";
    let application = b"\x1bOA\x1bOB\x1bOC\x1bOD\r\n\x1bOp\x1bOq\x1bOr\x1bOs\x1bOt\x1bOu\x1bOv\x1bOw\x1bOx\x1bOy\x1bOn\x1bOl\x1bOm\x1bOM\t\x1b\x08\x1bOj\x1bOk\x1bOo\x1bOX";
    let mut text = |_: &mut Screen, _: &str, _| Ok::<_, StorageError>(());
    for split in 0..=program.len() {
        let mut screen = Screen::new(8, 4).unwrap();
        screen.set_cursor(2, 3);
        let before = super::snapshot(screen.grid());
        let cursor = screen.cursor();
        let mut processor = support::processor();
        assert_eq!(encoded(processor.input_modes()), normal);
        processor
            .advance(&program[..split], &mut screen, &mut text)
            .unwrap();
        processor
            .advance(&program[split..], &mut screen, &mut text)
            .unwrap();
        let snapshot = processor.input_modes();
        let key = snapshot.encode_key(Key::Up, Modifiers::default()).unwrap();
        assert!(snapshot.bracketed_paste);
        let focus = snapshot.encode_focus(true).unwrap();
        assert_eq!(snapshot.encode_focus(false), Some(&b"\x1b[O"[..]));
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
            .advance(
                b"\x1b[?1;66;67;1004;2004l\x1b[20l\x1b8",
                &mut screen,
                &mut text,
            )
            .unwrap();
        assert_eq!(encoded(processor.input_modes()), normal);
        assert!(!processor.input_modes().bracketed_paste);
        assert!(!processor.input_modes().focus_reporting);
        assert_eq!(encoded(snapshot), application);
        processor
            .advance(
                b"\x1b[?1;66;67;1004;2004h\x1b[20h\x1bc",
                &mut screen,
                &mut text,
            )
            .unwrap();
        assert_eq!(processor.input_modes(), InputModes::default());
        processor
            .advance(b"\x1b[?1;66;67;1004;2004h\x1b[20h", &mut screen, &mut text)
            .unwrap();
        processor.reset(&mut screen, &mut text);
        assert_eq!(processor.input_modes(), InputModes::default());
        let mut remaining = Vec::<u8>::new();
        while !paste.is_empty() {
            remaining.extend(paste.bytes());
            assert!(paste.consume(paste.bytes().len()));
        }
        assert_eq!(remaining, "200~水\nx\x1b[201~".as_bytes());
        assert_eq!(key.as_bytes(), b"\x1bOA");
        assert_eq!(focus, b"\x1b[I");
    }
}

#[test]
fn focus_reporting_validates_modes_across_fragments_and_emits_only_on_request() {
    let cases: &[(&[u8], Option<bool>)] = &[
        (b"\x1b[?1004h", Some(true)),
        (b"\x1b[?1004l", Some(false)),
        (b"\x1b[?1004;1004h", Some(true)),
        (b"\x1b[?1004;1004l", Some(false)),
        (b"\x1b[?1004;999h", None),
        (b"\x1b[?1004;999l", None),
        (b"\x1b[1004h", None),
        (b"\x1b[?1004:1h", None),
        (b"\x1b[?1004;h", None),
        (b"\x1b[>1004h", None),
        (b"\x1b[?1004 h", None),
    ];
    for initially_enabled in [false, true] {
        for &(wire, change) in cases {
            for split in 0..=wire.len() {
                let mut screen = Screen::new(8, 4).unwrap();
                let mut processor = support::processor();
                let mut text = |_: &mut Screen, _: &str, _| Ok::<_, StorageError>(());
                if initially_enabled {
                    processor
                        .advance(b"\x1b[?1004h", &mut screen, &mut text)
                        .unwrap();
                }
                processor
                    .advance(&wire[..split], &mut screen, &mut text)
                    .unwrap();
                processor
                    .advance(&wire[split..], &mut screen, &mut text)
                    .unwrap();
                let modes = processor.input_modes();
                assert_eq!(modes.focus_reporting, change.unwrap_or(initially_enabled));
                assert_eq!(processor.ignored_sequences(), u64::from(change.is_none()));
                assert_eq!(processor.parse_errors(), 0);
                assert!(processor.reply_bytes().is_empty());
                for focused in [false, true] {
                    let expected = if modes.focus_reporting {
                        Some(if focused {
                            &b"\x1b[I"[..]
                        } else {
                            &b"\x1b[O"[..]
                        })
                    } else {
                        None
                    };
                    assert_eq!(modes.encode_focus(focused), expected);
                }
            }
        }
    }
}

#[test]
fn modified_keys_follow_pc_sequences_independently_of_output_modes() {
    let keys = [
        (Key::Up, "1", 'A'),
        (Key::Down, "1", 'B'),
        (Key::Right, "1", 'C'),
        (Key::Left, "1", 'D'),
        (Key::Home, "1", 'H'),
        (Key::End, "1", 'F'),
        (Key::Insert, "2", '~'),
        (Key::Delete, "3", '~'),
        (Key::PageUp, "5", '~'),
        (Key::PageDown, "6", '~'),
        (Key::Function(1), "1", 'P'),
        (Key::Function(2), "1", 'Q'),
        (Key::Function(3), "1", 'R'),
        (Key::Function(4), "1", 'S'),
        (Key::Function(5), "15", '~'),
        (Key::Function(6), "17", '~'),
        (Key::Function(7), "18", '~'),
        (Key::Function(8), "19", '~'),
        (Key::Function(9), "20", '~'),
        (Key::Function(10), "21", '~'),
        (Key::Function(11), "23", '~'),
        (Key::Function(12), "24", '~'),
    ];
    for flags in 0..64 {
        let modes = InputModes {
            application_cursor: flags & 1 != 0,
            application_keypad: flags & 2 != 0,
            new_line: flags & 4 != 0,
            backarrow: flags & 8 != 0,
            bracketed_paste: flags & 16 != 0,
            focus_reporting: flags & 32 != 0,
            ..InputModes::default()
        };
        for focused in [false, true] {
            let expected = modes.focus_reporting.then_some(if focused {
                &b"\x1b[I"[..]
            } else {
                &b"\x1b[O"[..]
            });
            assert_eq!(modes.encode_focus(focused), expected);
        }
        for (shift, alt, control, parameter) in [
            (true, false, false, 2),
            (false, true, false, 3),
            (true, true, false, 4),
            (false, false, true, 5),
            (true, false, true, 6),
            (false, true, true, 7),
            (true, true, true, 8),
        ] {
            let modifiers = Modifiers {
                shift,
                alt,
                control,
            };
            for (key, prefix, final_byte) in keys {
                let expected = format!("\x1b[{prefix};{parameter}{final_byte}");
                assert_key(modes, key, modifiers, expected.as_bytes());
            }
            if parameter == 2 {
                assert_key(modes, Key::Tab, modifiers, b"\x1b[Z");
            } else {
                assert!(modes.encode_key(Key::Tab, modifiers).is_none());
            }
            for key in [
                Key::Enter,
                Key::Escape,
                Key::Backspace,
                Key::KeypadEnter,
                Key::KeypadDigit(0),
                Key::KeypadDigit(9),
                Key::KeypadDigit(10),
                Key::Function(0),
                Key::Function(13),
                Key::Function(255),
            ] {
                assert!(
                    modes.encode_key(key, modifiers).is_none(),
                    "{key:?} {modifiers:?}"
                );
            }
        }
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
    let mut processor = support::processor();
    let mut text = |_: &mut Screen, _: &str, _| Ok::<_, StorageError>(());
    processor
        .advance(b"\x1b[?1h", &mut screen, &mut text)
        .unwrap();
    let cursor = processor.input_modes();
    assert_key(cursor, Key::Up, Modifiers::default(), b"\x1bOA");
    assert_key(cursor, Key::KeypadDigit(3), Modifiers::default(), b"3");
    assert_key(cursor, Key::Enter, Modifiers::default(), b"\r");
    processor
        .advance(b"\x1b[?1l\x1b=\x1b[20h", &mut screen, &mut text)
        .unwrap();
    let keypad = processor.input_modes();
    assert_key(keypad, Key::Up, Modifiers::default(), b"\x1b[A");
    assert_key(keypad, Key::KeypadDigit(3), Modifiers::default(), b"\x1bOs");
    assert_key(keypad, Key::KeypadEnter, Modifiers::default(), b"\x1bOM");
    assert_key(keypad, Key::Enter, Modifiers::default(), b"\r\n");
    assert_key(keypad, Key::Backspace, Modifiers::default(), b"\x7f");
    processor
        .advance(b"\x1b[?67h\x1b>", &mut screen, &mut text)
        .unwrap();
    let numeric = processor.input_modes();
    assert_key(numeric, Key::Backspace, Modifiers::default(), b"\x08");
    assert_key(numeric, Key::KeypadMultiply, Modifiers::default(), b"*");
    assert_key(numeric, Key::Enter, Modifiers::default(), b"\r\n");
    processor
        .advance(b"\x1b[?66;67;999l", &mut screen, &mut text)
        .unwrap();
    assert_eq!(processor.input_modes(), numeric);
    for digit in 10..=255 {
        assert_eq!(
            cursor.encode_key(Key::KeypadDigit(digit), Modifiers::default()),
            None
        );
        assert_eq!(
            keypad.encode_key(Key::KeypadDigit(digit), Modifiers::default()),
            None
        );
    }
}

#[test]
fn editing_and_function_keys_preserve_distinct_sequences_across_all_mode_combinations() {
    for flags in 0..64 {
        let modes = InputModes {
            application_cursor: flags & 1 != 0,
            application_keypad: flags & 2 != 0,
            new_line: flags & 4 != 0,
            backarrow: flags & 8 != 0,
            bracketed_paste: flags & 16 != 0,
            focus_reporting: flags & 32 != 0,
            ..InputModes::default()
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
            actual.extend_from_slice(
                modes
                    .encode_key(key, Modifiers::default())
                    .unwrap()
                    .as_bytes(),
            );
        }
        for number in 1..=12 {
            actual.extend_from_slice(
                modes
                    .encode_key(Key::Function(number), Modifiers::default())
                    .unwrap()
                    .as_bytes(),
            );
        }
        let mut expected = Vec::from(if modes.application_cursor {
            &b"\x1bOH\x1bOF"[..]
        } else {
            &b"\x1b[H\x1b[F"[..]
        });
        expected.extend(b"\x1b[2~\x1b[3~\x1b[5~\x1b[6~\x1bOP\x1bOQ\x1bOR\x1bOS\x1b[15~\x1b[17~\x1b[18~\x1b[19~\x1b[20~\x1b[21~\x1b[23~\x1b[24~");
        assert_eq!(actual, expected);
        assert_eq!(
            modes.encode_key(Key::Function(0), Modifiers::default()),
            None
        );
        for number in 13..=255 {
            assert_eq!(
                modes.encode_key(Key::Function(number), Modifiers::default()),
                None
            );
        }
    }
}

#[test]
fn kitty_keyboard_stacks_are_buffer_local_bounded_and_queryable_across_splits() {
    let wire = b"\x1b[?u\x1b[>1u\x1b[?u\x1b[=2;2u\x1b[?u\x1b[>8u\x1b[?u\x1b[?1049h\x1b[?u\x1b[>31u\x1b[=1;3u\x1b[?u\x1b[?1049l\x1b[?u\x1b[<u\x1b[?u\x1b[<9u\x1b[?u";
    let expected = b"\x1b[?0u\x1b[?1u\x1b[?3u\x1b[?8u\x1b[?0u\x1b[?30u\x1b[?8u\x1b[?3u\x1b[?0u";
    let mut text = |_: &mut Screen, _: &str, _| Ok::<_, StorageError>(());
    for split in 0..=wire.len() {
        let mut screen = Screen::new(8, 4).unwrap();
        let mut processor = support::processor();
        let mut output = Vec::new();
        for mut chunk in [&wire[..split], &wire[split..]] {
            loop {
                let result = processor.advance(chunk, &mut screen, &mut text);
                while !processor.reply_bytes().is_empty() {
                    output.extend_from_slice(processor.reply_bytes());
                    assert!(processor.consume_reply(processor.reply_bytes().len()));
                }
                match result {
                    Ok(consumed) => {
                        assert_eq!(consumed, chunk.len());
                        break;
                    }
                    Err(error) => {
                        assert!(matches!(error.error, crate::ProcessFailure::ReplyPending));
                        chunk = &chunk[error.consumed..];
                    }
                }
            }
        }
        assert_eq!(output, expected);
        assert_eq!(
            (processor.parse_errors(), processor.ignored_sequences()),
            (0, 0)
        );
    }

    // Overflowing the stack keeps the newest MAX_KITTY_KEYBOARD_STACK entries and RIS clears it.
    let mut screen = Screen::new(8, 4).unwrap();
    let mut processor = support::processor();
    for step in 1..=(crate::MAX_KITTY_KEYBOARD_STACK + 3) {
        let cmd = format!("\x1b[>{}u", step & 31);
        processor
            .advance(cmd.as_bytes(), &mut screen, &mut text)
            .unwrap();
    }
    assert_eq!(
        processor.input_modes().kitty_keyboard.bits(),
        ((crate::MAX_KITTY_KEYBOARD_STACK + 3) & 31) as u8
    );
    processor
        .advance(
            format!("\x1b[<{}u", crate::MAX_KITTY_KEYBOARD_STACK - 1).as_bytes(),
            &mut screen,
            &mut text,
        )
        .unwrap();
    assert_eq!(processor.input_modes().kitty_keyboard.bits(), 4);
    processor
        .advance(b"\x1b[<u", &mut screen, &mut text)
        .unwrap();
    assert_eq!(processor.input_modes().kitty_keyboard.bits(), 0);
    // Reject invalid modes and preserve SCORC (`CSI u`).
    processor
        .advance(b"\x1b[>5u\x1b[=1;4u\x1b[?1u\x1b[u", &mut screen, &mut text)
        .unwrap();
    assert_eq!(processor.input_modes().kitty_keyboard.bits(), 5);
    assert_eq!(processor.ignored_sequences(), 2);
    processor.advance(b"\x1bc", &mut screen, &mut text).unwrap();
    assert_eq!(processor.input_modes().kitty_keyboard.bits(), 0);
}

#[test]
fn kitty_keyboard_encodes_disambiguated_events_alternates_and_text() {
    use crate::{KeyEvent, KeyEventKind, KeyInput, KeyModifiers, KittyKeyboardFlags};

    let mut modes = InputModes {
        kitty_keyboard: KittyKeyboardFlags::new(KittyKeyboardFlags::DISAMBIGUATE),
        ..InputModes::default()
    };
    let press = |key, modifiers| KeyEvent {
        key,
        shifted_key: None,
        base_layout_key: None,
        modifiers,
        kind: KeyEventKind::Press,
        text: None,
    };

    // Disambiguate: Escape, Ctrl+I vs Tab, Shift+Tab, Super+a, F3, keypad.
    assert_eq!(
        modes
            .encode_key_event(press(KeyInput::Named(Key::Escape), KeyModifiers::default()))
            .unwrap()
            .as_bytes(),
        b"\x1b[27u"
    );
    assert_eq!(
        modes
            .encode_key_event(press(KeyInput::Named(Key::Tab), KeyModifiers::default()))
            .unwrap()
            .as_bytes(),
        b"\t"
    );
    assert_eq!(
        modes
            .encode_key_event(press(
                KeyInput::Character('i'),
                KeyModifiers {
                    control: true,
                    ..KeyModifiers::default()
                }
            ))
            .unwrap()
            .as_bytes(),
        b"\x1b[105;5u"
    );
    assert_eq!(
        modes
            .encode_key_event(press(
                KeyInput::Named(Key::Tab),
                KeyModifiers {
                    shift: true,
                    ..KeyModifiers::default()
                }
            ))
            .unwrap()
            .as_bytes(),
        b"\x1b[9;2u"
    );
    assert_eq!(
        modes
            .encode_key_event(press(
                KeyInput::Character('a'),
                KeyModifiers {
                    super_key: true,
                    ..KeyModifiers::default()
                }
            ))
            .unwrap()
            .as_bytes(),
        b"\x1b[97;9u"
    );
    assert_eq!(
        modes
            .encode_key_event(press(
                KeyInput::Named(Key::Function(3)),
                KeyModifiers::default()
            ))
            .unwrap()
            .as_bytes(),
        b"\x1b[13~"
    );
    assert_eq!(
        modes
            .encode_key_event(press(
                KeyInput::Named(Key::KeypadEnter),
                KeyModifiers::default()
            ))
            .unwrap()
            .as_bytes(),
        b"\x1b[57414u"
    );
    // Plain character without REPORT_ALL_KEYS sends raw UTF-8 text.
    assert_eq!(
        modes
            .encode_key_event(press(KeyInput::Character('a'), KeyModifiers::default()))
            .unwrap()
            .as_bytes(),
        b"a"
    );
    // Release is suppressed when REPORT_EVENTS is not set.
    assert!(
        modes
            .encode_key_event(KeyEvent {
                kind: KeyEventKind::Release,
                ..press(KeyInput::Character('a'), KeyModifiers::default())
            })
            .is_none()
    );

    // Full progressive enhancement flags (all 5 bits = 31).
    modes.kitty_keyboard = KittyKeyboardFlags::new(KittyKeyboardFlags::ALL);
    assert_eq!(
        modes
            .encode_key_event(KeyEvent {
                key: KeyInput::Character('a'),
                shifted_key: Some('A'),
                base_layout_key: Some('q'),
                modifiers: KeyModifiers {
                    shift: true,
                    control: true,
                    ..KeyModifiers::default()
                },
                kind: KeyEventKind::Repeat,
                text: Some("A"),
            })
            .unwrap()
            .as_bytes(),
        b"\x1b[97:65:113;6:2;65u"
    );
    // Release omits associated text even when REPORT_TEXT is active.
    assert_eq!(
        modes
            .encode_key_event(KeyEvent {
                key: KeyInput::Character('a'),
                shifted_key: Some('A'),
                base_layout_key: None,
                modifiers: KeyModifiers::default(),
                kind: KeyEventKind::Release,
                text: Some("a"),
            })
            .unwrap()
            .as_bytes(),
        b"\x1b[97;1:3u"
    );
    // Unmodified plain character with REPORT_ALL_KEYS + REPORT_TEXT includes ;1;codepoint.
    assert_eq!(
        modes
            .encode_key_event(KeyEvent {
                key: KeyInput::Character('a'),
                shifted_key: None,
                base_layout_key: None,
                modifiers: KeyModifiers::default(),
                kind: KeyEventKind::Press,
                text: Some("a"),
            })
            .unwrap()
            .as_bytes(),
        b"\x1b[97;1;97u"
    );
    // Base layout key without shifted key uses double colon.
    assert_eq!(
        modes
            .encode_key_event(KeyEvent {
                key: KeyInput::Character('й'),
                shifted_key: None,
                base_layout_key: Some('q'),
                modifiers: KeyModifiers {
                    control: true,
                    ..KeyModifiers::default()
                },
                kind: KeyEventKind::Press,
                text: None,
            })
            .unwrap()
            .as_bytes(),
        b"\x1b[1081::113;5u"
    );
}
