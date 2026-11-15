#![no_main]

#[path = "../../tests/support/processor.rs"]
mod support;

#[allow(
    dead_code,
    reason = "Shared oracle operations serve different fuzz targets."
)]
mod model;
#[allow(
    dead_code,
    reason = "Shared oracle operations serve different fuzz targets."
)]
mod screen_model;

use std::io::Write;

use libfuzzer_sys::fuzz_target;
use lune_terminal::{
    CellPosition, CellWidth, Color, CursorShape, EraseMode, InputModes, Key, Modifiers,
    MouseButton, MouseEvent, MouseTracking, PaletteEntry, ProcessFailure, Processor, Rgb, Screen,
    Style, StyleId, Underline,
};
use screen_model::{Command, ScreenModel};

fn count(wire: &mut Vec<u8>, final_byte: u8, bytes: &[u8; 8]) -> usize {
    if bytes[4] & 1 == 0 {
        wire.extend([0x1b, b'[', final_byte]);
        1
    } else {
        let value = bytes[3] as usize % 17;
        write!(wire, "\x1b[{value}{}", char::from(final_byte)).unwrap();
        value.max(1)
    }
}

struct Expected<'a> {
    screen: &'a ScreenModel,
    diagnostics: (u64, u64),
    values: &'a [Style],
    style: Style,
    input: InputModes,
    replies: &'a [u8],
    title: &'a str,
    title_changed: bool,
    colors: [Rgb; 3],
    palette_changed: bool,
}

fn drain_reply(processor: &mut Processor, output: &mut Vec<u8>, count: usize) {
    let count = count.min(processor.reply_bytes().len());
    output.extend_from_slice(&processor.reply_bytes()[..count]);
    assert!(processor.consume_reply(count));
}

fn drive(wire: &[u8], chunk: usize, fail_at: usize, expected: &Expected<'_>) {
    let mut screen = Screen::new(8, 4).unwrap();
    let mut processor = support::processor();
    let mut calls = 0;
    let mut available = false;
    let mut replies = Vec::new();
    let mut text = |screen: &mut Screen, value: &str, style| {
        if !available && calls == fail_at {
            available = true;
            return Err(());
        }
        calls += 1;
        if value.is_ascii() {
            screen.write_ascii(value.as_bytes(), style).unwrap();
        } else {
            assert_eq!(value.chars().count(), 1);
            if value == "水" {
                screen.write("水\u{301}", CellWidth::Double, style).unwrap();
            } else {
                screen.write(value, CellWidth::Single, style).unwrap();
            }
        }
        Ok(())
    };
    for mut input in wire.chunks(chunk) {
        loop {
            match processor.advance(input, &mut screen, &mut text) {
                Ok(consumed) => {
                    assert_eq!(consumed, input.len());
                    break;
                }
                Err(error) => {
                    assert!(error.consumed <= input.len());
                    input = &input[error.consumed..];
                    match error.error {
                        ProcessFailure::ReplyPending => {
                            assert!(!processor.reply_bytes().is_empty());
                            let again = processor.advance(&[], &mut screen, &mut text).unwrap_err();
                            assert_eq!(again.consumed, 0);
                            assert!(matches!(again.error, ProcessFailure::ReplyPending));
                            drain_reply(&mut processor, &mut replies, chunk % 3 + 1);
                        }
                        ProcessFailure::Text(()) => {}
                        ProcessFailure::Screen(error) => {
                            panic!("Unexpected screen failure: {error}")
                        }
                        ProcessFailure::Style(error) => panic!("Unexpected style failure: {error}"),
                    }
                    processor.collect_styles(&screen, []).unwrap();
                }
            }
        }
        processor.collect_styles(&screen, []).unwrap();
    }
    loop {
        match processor.finish(&mut screen, &mut text) {
            Ok(()) => break,
            Err(ProcessFailure::ReplyPending) => drain_reply(&mut processor, &mut replies, 1),
            Err(ProcessFailure::Text(())) => {}
            Err(ProcessFailure::Screen(error)) => panic!("Unexpected screen failure: {error}"),
            Err(ProcessFailure::Style(error)) => panic!("Unexpected style failure: {error}"),
        }
    }
    while !processor.reply_bytes().is_empty() {
        drain_reply(&mut processor, &mut replies, chunk % 3 + 1);
    }
    assert_eq!(replies, expected.replies);
    expected.screen.check_styles(&screen, |actual, reference| {
        assert_eq!(
            processor.styles().get(actual),
            Some(&expected.values[reference.0 as usize])
        );
    });
    assert_eq!(
        processor.styles().get(processor.style_id()),
        Some(&expected.style)
    );
    assert_eq!(processor.input_modes(), expected.input);
    assert_eq!(processor.window_title(), expected.title);
    assert_eq!(
        processor.take_window_title_change(),
        expected.title_changed.then_some(expected.title)
    );
    assert_eq!(processor.take_window_title_change(), None);
    assert_eq!(
        [
            PaletteEntry::Foreground,
            PaletteEntry::Background,
            PaletteEntry::Cursor
        ]
        .map(|entry| processor.palette().color(entry)),
        expected.colors
    );
    assert_eq!(processor.take_palette_change(), expected.palette_changed);
    assert!(!processor.take_palette_change());
    for (event, normal, sgr, suffix, enabled) in [
        (
            MouseEvent::Release(MouseButton::Right),
            3,
            2,
            'm',
            expected.input.mouse_tracking != MouseTracking::Disabled,
        ),
        (
            MouseEvent::Motion(None),
            35,
            35,
            'M',
            expected.input.mouse_tracking == MouseTracking::AnyMotion,
        ),
    ] {
        let actual = processor.input_modes().encode_mouse(
            event,
            CellPosition { row: 2, column: 4 },
            Modifiers::default(),
        );
        if !enabled {
            assert!(actual.is_none());
        } else if expected.input.sgr_mouse {
            assert_eq!(
                actual.unwrap().as_bytes(),
                format!("\x1b[<{sgr};5;3{suffix}").as_bytes()
            );
        } else {
            assert_eq!(
                actual.unwrap().as_bytes(),
                &[27, 91, 77, normal + 32, 37, 35]
            );
        }
    }
    for (focused, bytes) in [(true, &b"\x1b[I"[..]), (false, &b"\x1b[O"[..])] {
        assert_eq!(
            processor.input_modes().encode_focus(focused),
            expected.input.focus_reporting.then_some(bytes)
        );
    }
    let flags = chunk % 8;
    let modifiers = Modifiers {
        shift: flags & 1 != 0,
        alt: flags & 2 != 0,
        control: flags & 4 != 0,
    };
    let function: [&[u8]; 8] = [
        b"\x1b[24~",
        b"\x1b[24;2~",
        b"\x1b[24;3~",
        b"\x1b[24;4~",
        b"\x1b[24;5~",
        b"\x1b[24;6~",
        b"\x1b[24;7~",
        b"\x1b[24;8~",
    ];
    assert_eq!(
        processor
            .input_modes()
            .encode_key(Key::Function(12), modifiers)
            .unwrap()
            .as_bytes(),
        function[flags]
    );
    let expected_paste = if expected.input.bracketed_paste {
        "\x1b[200~a水\n\x1b[201~".as_bytes()
    } else {
        "a水\n".as_bytes()
    };
    let mut paste = processor.input_modes().encode_paste("a水\n");
    let mut offset = 0;
    while !paste.is_empty() {
        let count = paste.bytes().len().min(chunk);
        assert_eq!(
            &paste.bytes()[..count],
            &expected_paste[offset..offset + count]
        );
        assert!(!paste.consume(usize::MAX));
        assert!(paste.consume(count));
        offset += count;
    }
    assert_eq!(offset, expected_paste.len());
    assert_eq!(
        (processor.parse_errors(), processor.ignored_sequences()),
        expected.diagnostics
    );
}

fn reference_id(values: &mut Vec<Style>, style: Style) -> StyleId {
    let index = values
        .iter()
        .position(|&value| value == style)
        .unwrap_or_else(|| {
            values.push(style);
            values.len() - 1
        });
    StyleId(index as u32)
}

fn ascii(expected: &mut ScreenModel, bytes: &[u8], style: StyleId, graphics: bool) {
    for &byte in bytes {
        let character = if graphics {
            match byte {
                b'a' => '▒',
                b'b' => '␉',
                b'c' => '␌',
                b'q' => '─',
                b'x' => '│',
                b'_' => '\u{a0}',
                _ => char::from(byte),
            }
        } else {
            char::from(byte)
        };
        expected
            .apply(
                &Command::Write(character.encode_utf8(&mut [0; 4]), CellWidth::Single),
                style,
            )
            .unwrap();
    }
}

fuzz_target!(|data: &[u8]| {
    let mut wire = Vec::new();
    let mut expected = ScreenModel::new(8, 4);
    let mut diagnostics = (0_u64, 0_u64);
    let mut values = vec![Style::default()];
    let mut current = Style::default();
    let mut saved = [Style::default(); 2];
    let mut charsets = [false; 2];
    let mut selected = 0;
    let mut saved_charsets = [(charsets, selected); 2];
    let mut input = InputModes::default();
    let mut replies = Vec::new();
    let mut title = String::new();
    let mut title_changed = false;
    let base_colors = [
        Rgb {
            red: 7,
            green: 12,
            blue: 34,
        },
        Rgb {
            red: 0,
            green: 12,
            blue: 34,
        },
        Rgb {
            red: 15,
            green: 12,
            blue: 34,
        },
    ];
    let mut colors = base_colors;
    let mut palette_changed = false;
    for bytes in data.as_chunks::<8>().0.iter().take(512) {
        let index = usize::from(expected.is_alternate());
        let command = match bytes[0] % 64 {
            0 => {
                const ASCII: &[u8; 65] =
                    b"abcA^qx_abcA^qx_abcA^qx_abcA^qx_abcA^qx_abcA^qx_abcA^qx_abcA^qx_A";
                let length =
                    [3, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65][bytes[1] as usize % 13];
                let run = &ASCII[..length];
                wire.extend(run);
                ascii(
                    &mut expected,
                    run,
                    reference_id(&mut values, current),
                    charsets[selected],
                );
                None
            }
            1 => {
                wire.extend("水".as_bytes());
                Some(Command::Write("水\u{301}", CellWidth::Double))
            }
            2 => {
                let row = bytes[1] as usize % 7;
                let column = bytes[2] as usize % 12;
                write!(&mut wire, "\x1b[{};{}H", row + 1, column + 1).unwrap();
                Some(Command::Cursor(row, column))
            }
            3 => {
                let top = bytes[1] as usize % 4;
                let bottom = bytes[2] as usize % 4 + 1;
                write!(&mut wire, "\x1b[{};{bottom}r", top + 1).unwrap();
                Some(Command::Region(top..bottom))
            }
            4 => {
                wire.extend(b"\xf0\x9fx");
                expected
                    .apply(
                        &Command::Write("�", CellWidth::Single),
                        reference_id(&mut values, current),
                    )
                    .unwrap();
                ascii(
                    &mut expected,
                    b"x",
                    reference_id(&mut values, current),
                    charsets[selected],
                );
                None
            }
            5 => {
                wire.push(b'\r');
                Some(Command::CarriageReturn)
            }
            6 => {
                wire.push(0x08);
                Some(Command::Backspace)
            }
            7 => {
                wire.push(b'\n');
                if input.new_line {
                    expected
                        .apply(&Command::CarriageReturn, reference_id(&mut values, current))
                        .unwrap();
                }
                Some(Command::LineFeed)
            }
            8 => {
                wire.extend(b"\x1bM");
                Some(Command::ReverseIndex)
            }
            9 | 10 => {
                let n = bytes[3] % 4;
                let final_byte = if bytes[0] % 64 == 9 { 'K' } else { 'J' };
                write!(&mut wire, "\x1b[{n}{final_byte}").unwrap();
                let mode = match n {
                    0 => Some(EraseMode::FromCursor),
                    1 => Some(EraseMode::ToCursor),
                    2 => Some(EraseMode::All),
                    _ => None,
                };
                if let Some(mode) = mode {
                    Some(if final_byte == 'K' {
                        Command::EraseLine(mode)
                    } else {
                        Command::EraseDisplay(mode)
                    })
                } else {
                    diagnostics.1 += 1;
                    None
                }
            }
            11 => Some(Command::InsertCells(count(&mut wire, b'@', bytes))),
            12 => Some(Command::DeleteCells(count(&mut wire, b'P', bytes))),
            13 => Some(Command::EraseCells(count(&mut wire, b'X', bytes))),
            14 => Some(Command::InsertLines(count(&mut wire, b'L', bytes))),
            15 => Some(Command::DeleteLines(count(&mut wire, b'M', bytes))),
            16 => {
                wire.extend(b"\x1bE");
                expected
                    .apply(&Command::CarriageReturn, reference_id(&mut values, current))
                    .unwrap();
                Some(Command::LineFeed)
            }
            17 => Some(Command::MoveUp(count(&mut wire, b'A', bytes))),
            18 => Some(Command::MoveDown(count(&mut wire, b'B', bytes))),
            19 => Some(Command::MoveLeft(count(&mut wire, b'D', bytes))),
            20 => Some(Command::MoveRight(count(&mut wire, b'C', bytes))),
            21 => Some(Command::ScrollUp(count(&mut wire, b'S', bytes))),
            22 => Some(Command::ScrollDown(count(&mut wire, b'T', bytes))),
            23 => Some(Command::TabForward(count(&mut wire, b'I', bytes))),
            24 => Some(Command::TabBackward(count(&mut wire, b'Z', bytes))),
            25 => {
                wire.extend(b"\x1bH");
                Some(Command::SetTab)
            }
            26 => {
                wire.extend(b"\x1b[g");
                Some(Command::ClearTab)
            }
            27 => {
                wire.extend(b"\x1b[3g");
                Some(Command::ClearTabs)
            }
            28 | 29 | 33 => {
                let enabled = bytes[3] & 1 != 0;
                let final_byte = if enabled { 'h' } else { 'l' };
                let command = match bytes[0] % 64 {
                    28 => {
                        write!(&mut wire, "\x1b[?7{final_byte}").unwrap();
                        Command::AutoWrap(enabled)
                    }
                    29 => {
                        write!(&mut wire, "\x1b[?6{final_byte}").unwrap();
                        Command::OriginMode(enabled)
                    }
                    _ => {
                        write!(&mut wire, "\x1b[4{final_byte}").unwrap();
                        Command::InsertMode(enabled)
                    }
                };
                Some(command)
            }
            30 => {
                wire.extend(b"\x1b7");
                saved[index] = current;
                saved_charsets[index] = (charsets, selected);
                Some(Command::SaveCursor)
            }
            31 => {
                wire.extend(b"\x1b8");
                current = saved[index];
                (charsets, selected) = saved_charsets[index];
                Some(Command::RestoreCursor)
            }
            32 => {
                wire.extend(b"\x1bc");
                palette_changed |= colors != base_colors;
                colors = base_colors;
                title_changed |= !title.is_empty();
                title.clear();
                current = Style::default();
                saved = [current; 2];
                charsets = [false; 2];
                selected = 0;
                saved_charsets = [(charsets, selected); 2];
                input = InputModes::default();
                Some(Command::Reset)
            }
            34 => {
                wire.extend(b"\x1b[?6;1003;1004;1006;999h");
                diagnostics.1 += 1;
                None
            }
            35 => {
                wire.extend(b"\x1b[65536A");
                diagnostics.0 += 1;
                None
            }
            36 => {
                wire.extend(b"\x1b[1:2A");
                diagnostics.1 += 1;
                None
            }
            37 => {
                match bytes[1] % 8 {
                    0 => {
                        let next = format!("title-{}-🌙;{}", bytes[2], bytes[3]);
                        let selector = if bytes[4] & 1 == 0 { 0 } else { 2 };
                        write!(&mut wire, "\x1b]{selector};{next}").unwrap();
                        wire.extend(if bytes[5] & 1 == 0 {
                            &b"\x07"[..]
                        } else {
                            b"\x1b\\"
                        });
                        title_changed |= title != next;
                        title = next;
                    }
                    1 => {
                        wire.extend(b"\x1b]2;\x07");
                        title_changed |= !title.is_empty();
                        title.clear();
                    }
                    2 => wire.extend(b"\x1b]2;cancelled\x18"),
                    3 => {
                        wire.extend(b"\x1b]8;;link\x07");
                        diagnostics.1 += 1;
                    }
                    4 => {
                        wire.extend(b"\x1b]2;\xff\x1b\\");
                        diagnostics.1 += 1;
                    }
                    5 => {
                        let role = usize::from(bytes[2] % 3);
                        let next = Rgb {
                            red: bytes[3],
                            green: bytes[4],
                            blue: bytes[5],
                        };
                        write!(
                            &mut wire,
                            "\x1b]{};rgb:{:02x}/{:02x}/{:02x}\x1b\\",
                            role + 10,
                            next.red,
                            next.green,
                            next.blue
                        )
                        .unwrap();
                        palette_changed |= colors[role] != next;
                        colors[role] = next;
                    }
                    6 => {
                        let role = usize::from(bytes[2] % 3);
                        let terminator = if bytes[3] & 1 == 0 { "\x07" } else { "\x1b\\" };
                        write!(&mut wire, "\x1b]{};?{terminator}", role + 10).unwrap();
                        let color = colors[role];
                        write!(
                            &mut replies,
                            "\x1b]{};rgb:{:04x}/{:04x}/{:04x}{terminator}",
                            role + 10,
                            u16::from(color.red) * 257,
                            u16::from(color.green) * 257,
                            u16::from(color.blue) * 257
                        )
                        .unwrap();
                    }
                    _ => {
                        let role = usize::from(bytes[2] % 3);
                        write!(&mut wire, "\x1b]{}\x07", role + 110).unwrap();
                        palette_changed |= colors[role] != base_colors[role];
                        colors[role] = base_colors[role];
                    }
                }
                None
            }
            38 => {
                let index = bytes[1] % 16;
                let color_code = if index < 8 {
                    30 + index
                } else {
                    90 + index - 8
                };
                let bold = bytes[2] & 1 != 0;
                let intensity = if bold { 1 } else { 22 };
                write!(&mut wire, "\x1b[{color_code};{intensity}m").unwrap();
                current.foreground = Color::Indexed(index);
                current.bold = bold;
                if !bold {
                    current.faint = false;
                }
                None
            }
            39 => {
                let [red, green, blue] = [bytes[1], bytes[2], bytes[3]];
                let index = bytes[4];
                if bytes[5] & 1 == 0 {
                    write!(&mut wire, "\x1b[38;2;{red};{green};{blue};48:5:{index}m").unwrap();
                } else {
                    write!(&mut wire, "\x1b[38:2::{red}:{green}:{blue};48;5;{index}m").unwrap();
                }
                current.foreground = Color::Rgb { red, green, blue };
                current.background = Color::Indexed(index);
                None
            }
            40 => {
                wire.extend(b"\x1b[0m");
                current = Style::default();
                None
            }
            41 => {
                wire.extend(b"\x1b[1;38;5;256m");
                diagnostics.1 += 1;
                None
            }
            42 => {
                let enabled = bytes[1] & 1 != 0;
                if enabled {
                    wire.extend(b"\x1b[1;2;3;4;5;7;8;9m");
                } else {
                    wire.extend(b"\x1b[22;23;24;25;27;28;29m");
                }
                current.bold = enabled;
                current.faint = enabled;
                current.italic = enabled;
                current.blink = enabled;
                current.inverse = enabled;
                current.hidden = enabled;
                current.strikethrough = enabled;
                current.underline = if enabled {
                    Underline::Single
                } else {
                    Underline::None
                };
                None
            }
            43 => {
                let slot = bytes[1] as usize & 1;
                let graphics = bytes[2] & 1 != 0;
                wire.extend([
                    0x1b,
                    if slot == 0 { b'(' } else { b')' },
                    if graphics { b'0' } else { b'B' },
                ]);
                charsets[slot] = graphics;
                None
            }
            44 => {
                selected = bytes[1] as usize & 1;
                wire.push(if selected == 0 { 0x0f } else { 0x0e });
                None
            }
            45 => {
                let byte = b"qx_abcA^"[bytes[1] as usize % 8];
                wire.push(byte);
                ascii(
                    &mut expected,
                    &[byte],
                    reference_id(&mut values, current),
                    charsets[selected],
                );
                None
            }
            46 => {
                wire.extend(b"\x1b(A");
                diagnostics.1 += 1;
                None
            }
            47 => {
                wire.extend(b"\xf0\x9f\x0eq");
                expected
                    .apply(
                        &Command::Write("�", CellWidth::Single),
                        reference_id(&mut values, current),
                    )
                    .unwrap();
                selected = 1;
                ascii(
                    &mut expected,
                    b"q",
                    reference_id(&mut values, current),
                    charsets[selected],
                );
                None
            }
            48 => {
                let enabled = bytes[1] & 1 != 0;
                wire.extend(if enabled {
                    b"\x1b[4;20h"
                } else {
                    b"\x1b[4;20l"
                });
                input.new_line = enabled;
                Some(Command::InsertMode(enabled))
            }
            49 => {
                wire.push([b'\n', 0x0b, 0x0c][bytes[1] as usize % 3]);
                if input.new_line {
                    expected
                        .apply(&Command::CarriageReturn, reference_id(&mut values, current))
                        .unwrap();
                }
                Some(Command::LineFeed)
            }
            50 => {
                input.application_cursor = bytes[1] & 1 != 0;
                wire.extend(if input.application_cursor {
                    b"\x1b[?1h"
                } else {
                    b"\x1b[?1l"
                });
                None
            }
            51 => {
                input.application_keypad = bytes[1] & 1 != 0;
                wire.extend(if input.application_keypad {
                    b"\x1b="
                } else {
                    b"\x1b>"
                });
                None
            }
            52 | 53 => {
                let enabled = bytes[1] & 1 != 0;
                let mode = if bytes[0] % 64 == 52 { 25 } else { 12 };
                write!(&mut wire, "\x1b[?{mode}{}", if enabled { 'h' } else { 'l' }).unwrap();
                Some(if mode == 25 {
                    Command::CursorVisible(enabled)
                } else {
                    Command::CursorBlink(enabled)
                })
            }
            54 => {
                let selector = bytes[1] % 9;
                write!(&mut wire, "\x1b[{selector} q").unwrap();
                let style = match selector {
                    0 | 1 => Some((CursorShape::Block, true)),
                    2 => Some((CursorShape::Block, false)),
                    3 => Some((CursorShape::Underline, true)),
                    4 => Some((CursorShape::Underline, false)),
                    5 => Some((CursorShape::Bar, true)),
                    6 => Some((CursorShape::Bar, false)),
                    _ => None,
                };
                if let Some((shape, blinking)) = style {
                    Some(Command::CursorShape(shape, blinking))
                } else {
                    diagnostics.1 += 1;
                    None
                }
            }
            55 | 56 => {
                let enabled = bytes[1] & 1 != 0;
                let mode = if bytes[1] & 8 != 0 {
                    match bytes[2] % 4 {
                        0 => {
                            input.mouse_tracking = if enabled {
                                MouseTracking::Buttons
                            } else {
                                MouseTracking::Disabled
                            };
                            1000
                        }
                        1 => {
                            input.mouse_tracking = if enabled {
                                MouseTracking::ButtonMotion
                            } else {
                                MouseTracking::Disabled
                            };
                            1002
                        }
                        2 => {
                            input.mouse_tracking = if enabled {
                                MouseTracking::AnyMotion
                            } else {
                                MouseTracking::Disabled
                            };
                            1003
                        }
                        _ => {
                            input.sgr_mouse = enabled;
                            1006
                        }
                    }
                } else if bytes[0] % 64 == 55 {
                    input.application_keypad = enabled;
                    66
                } else if bytes[1] & 4 != 0 {
                    input.focus_reporting = enabled;
                    1004
                } else if bytes[1] & 2 == 0 {
                    input.backarrow = enabled;
                    67
                } else {
                    input.bracketed_paste = enabled;
                    2004
                };
                write!(&mut wire, "\x1b[?{mode}{}", if enabled { 'h' } else { 'l' }).unwrap();
                None
            }
            57 => {
                if bytes[1] & 1 == 0 {
                    wire.extend(b"\x1b[5n");
                    replies.extend(b"\x1b[0n");
                } else {
                    let private = bytes[1] & 2 != 0;
                    let selector = [
                        0, 1, 4, 6, 7, 12, 20, 25, 47, 66, 67, 1000, 1002, 1003, 1004, 1006, 1047,
                        1048, 1049, 2004, 65535,
                    ][usize::from(bytes[2] % 21)];
                    let enabled = match (private, selector) {
                        (false, 20) => Some(input.new_line),
                        (true, 1) => Some(input.application_cursor),
                        (true, 66) => Some(input.application_keypad),
                        (true, 67) => Some(input.backarrow),
                        (true, 1000) => Some(input.mouse_tracking == MouseTracking::Buttons),
                        (true, 1002) => Some(input.mouse_tracking == MouseTracking::ButtonMotion),
                        (true, 1003) => Some(input.mouse_tracking == MouseTracking::AnyMotion),
                        (true, 1004) => Some(input.focus_reporting),
                        (true, 1006) => Some(input.sgr_mouse),
                        (true, 2004) => Some(input.bracketed_paste),
                        _ => expected.mode(private, selector),
                    };
                    let status = match enabled {
                        Some(true) => 1,
                        Some(false) => 2,
                        None => 0,
                    };
                    let marker = if private { "?" } else { "" };
                    write!(&mut wire, "\x1b[{marker}{selector}$p").unwrap();
                    write!(&mut replies, "\x1b[{marker}{selector};{status}$y").unwrap();
                }
                None
            }
            58 => {
                wire.extend(b"\x1b[6n");
                let (row, column) = expected.report_position();
                write!(&mut replies, "\x1b[{row};{column}R").unwrap();
                None
            }
            59 => {
                let invalid: [&[u8]; 14] = [
                    b"\x1b[n",
                    b"\x1b[0n",
                    b"\x1b[?5n",
                    b"\x1b[?6n",
                    b"\x1b[6;5n",
                    b"\x1b[6:0n",
                    b"\x1b[6!n",
                    b"\x1b[>6n",
                    b"\x1b[?1004;1$p",
                    b"\x1b[?1004:1$p",
                    b"\x1b[>1004$p",
                    b"\x1b[?1004$$p",
                    b"\x1b[?1004p",
                    b"\x1b[?1004 $p",
                ];
                wire.extend(invalid[bytes[1] as usize % invalid.len()]);
                diagnostics.1 += 1;
                None
            }
            60 => {
                let enabled = bytes[1] & 1 != 0;
                wire.extend(if enabled { b"\x1b[?47h" } else { b"\x1b[?47l" });
                Some(Command::SelectBuffer(enabled))
            }
            61 => {
                let enabled = bytes[1] & 1 != 0;
                wire.extend(if enabled {
                    b"\x1b[?1047h"
                } else {
                    b"\x1b[?1047l"
                });
                if !enabled && expected.is_alternate() {
                    expected
                        .apply(
                            &Command::EraseDisplay(EraseMode::All),
                            reference_id(&mut values, current),
                        )
                        .unwrap();
                }
                Some(Command::SelectBuffer(enabled))
            }
            62 => {
                let enabled = bytes[1] & 1 != 0;
                wire.extend(if enabled {
                    b"\x1b[?1048h"
                } else {
                    b"\x1b[?1048l"
                });
                if enabled {
                    saved[index] = current;
                    saved_charsets[index] = (charsets, selected);
                    Some(Command::SaveCursor)
                } else {
                    current = saved[index];
                    (charsets, selected) = saved_charsets[index];
                    Some(Command::RestoreCursor)
                }
            }
            _ => {
                let enabled = bytes[1] & 1 != 0;
                wire.extend(if enabled {
                    b"\x1b[?1049h"
                } else {
                    b"\x1b[?1049l"
                });
                if enabled {
                    saved[index] = current;
                    saved_charsets[index] = (charsets, selected);
                    expected
                        .apply(&Command::SaveCursor, reference_id(&mut values, current))
                        .unwrap();
                    expected
                        .apply(
                            &Command::SelectBuffer(true),
                            reference_id(&mut values, current),
                        )
                        .unwrap();
                    Some(Command::EraseDisplay(EraseMode::All))
                } else {
                    expected
                        .apply(
                            &Command::SelectBuffer(false),
                            reference_id(&mut values, current),
                        )
                        .unwrap();
                    current = saved[0];
                    (charsets, selected) = saved_charsets[0];
                    Some(Command::RestoreCursor)
                }
            }
        };
        if let Some(command) = command
            && expected
                .apply(&command, reference_id(&mut values, current))
                .is_err()
        {
            diagnostics.1 += 1;
        }
    }
    for pass in 0..2 {
        let index = usize::from(expected.is_alternate());
        wire.extend(b"\x1b8qx");
        current = saved[index];
        (charsets, selected) = saved_charsets[index];
        expected
            .apply(&Command::RestoreCursor, reference_id(&mut values, current))
            .unwrap();
        ascii(
            &mut expected,
            b"qx",
            reference_id(&mut values, current),
            charsets[selected],
        );
        if pass == 0 {
            let alternate = !expected.is_alternate();
            wire.extend(if alternate {
                b"\x1b[?47h"
            } else {
                b"\x1b[?47l"
            });
            expected
                .apply(
                    &Command::SelectBuffer(alternate),
                    reference_id(&mut values, current),
                )
                .unwrap();
        }
    }
    let oracle = Expected {
        screen: &expected,
        diagnostics,
        title: &title,
        title_changed,
        colors,
        palette_changed,
        values: &values,
        style: current,
        input,
        replies: &replies,
    };
    drive(&wire, wire.len(), usize::MAX, &oracle);
    drive(
        &wire,
        data.first().copied().unwrap_or(0) as usize % 17 + 1,
        data.get(4).copied().unwrap_or(0) as usize % 32,
        &oracle,
    );
});
