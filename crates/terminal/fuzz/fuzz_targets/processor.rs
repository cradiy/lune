#![no_main]

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
use lune_terminal::{CellWidth, Color, EraseMode, Processor, Screen, Style, StyleId, Underline};
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

fn drive(
    wire: &[u8],
    chunk: usize,
    fail_at: usize,
    expected: &ScreenModel,
    diagnostics: (u64, u64),
    values: &[Style],
    current: Style,
) {
    let mut screen = Screen::new(8, 4).unwrap();
    let mut processor = Processor::default();
    let mut calls = 0;
    let mut available = false;
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
                    processor.collect_styles(&screen, []).unwrap();
                }
            }
        }
        processor.collect_styles(&screen, []).unwrap();
    }
    while processor.finish(&mut screen, &mut text).is_err() {}
    expected.check_styles(&screen, |actual, reference| {
        assert_eq!(
            processor.styles().get(actual),
            Some(&values[reference.0 as usize])
        );
    });
    assert_eq!(processor.styles().get(processor.style_id()), Some(&current));
    assert_eq!(
        (processor.parse_errors(), processor.ignored_sequences()),
        diagnostics
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
    let mut saved = Style::default();
    let mut charsets = [false; 2];
    let mut selected = 0;
    let mut saved_charsets = (charsets, selected);
    for bytes in data.as_chunks::<8>().0.iter().take(512) {
        let command = match bytes[0] % 48 {
            0 => {
                wire.extend(b"abc");
                ascii(
                    &mut expected,
                    b"abc",
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
                Some(Command::LineFeed)
            }
            8 => {
                wire.extend(b"\x1bM");
                Some(Command::ReverseIndex)
            }
            9 | 10 => {
                let n = bytes[3] % 4;
                let final_byte = if bytes[0] % 48 == 9 { 'K' } else { 'J' };
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
                let command = match bytes[0] % 48 {
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
                saved = current;
                saved_charsets = (charsets, selected);
                Some(Command::SaveCursor)
            }
            31 => {
                wire.extend(b"\x1b8");
                current = saved;
                (charsets, selected) = saved_charsets;
                Some(Command::RestoreCursor)
            }
            32 => {
                wire.extend(b"\x1bc");
                current = Style::default();
                saved = current;
                charsets = [false; 2];
                selected = 0;
                saved_charsets = (charsets, selected);
                Some(Command::Reset)
            }
            34 => {
                wire.extend(b"\x1b[?6;25h");
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
                wire.extend(b"\x1b]0;synthetic title\x07");
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
            _ => {
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
        };
        if let Some(command) = command
            && expected
                .apply(&command, reference_id(&mut values, current))
                .is_err()
        {
            diagnostics.1 += 1;
        }
    }
    wire.extend(b"\x1b8qx");
    current = saved;
    (charsets, selected) = saved_charsets;
    expected
        .apply(&Command::RestoreCursor, reference_id(&mut values, current))
        .unwrap();
    ascii(
        &mut expected,
        b"qx",
        reference_id(&mut values, current),
        charsets[selected],
    );
    drive(
        &wire,
        wire.len(),
        usize::MAX,
        &expected,
        diagnostics,
        &values,
        current,
    );
    drive(
        &wire,
        data.first().copied().unwrap_or(0) as usize % 17 + 1,
        data.get(4).copied().unwrap_or(0) as usize % 32,
        &expected,
        diagnostics,
        &values,
        current,
    );
});
