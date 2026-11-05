#![no_main]

use std::io::Write;

use libfuzzer_sys::fuzz_target;
use lune_terminal::{Color, Style, Underline};
use lune_vt::{Event, MAX_PARAMETERS, Parameter, Parser};

fn color(wire: &mut Vec<u8>, foreground: bool, bytes: &[u8]) -> Color {
    let code = if foreground { 38 } else { 48 };
    let colon = bytes[4] & 1 != 0;
    match bytes[0] % 3 {
        0 => {
            write!(wire, ";{}", code + 1).unwrap();
            Color::Default
        }
        1 => {
            if bytes[1] < 16 && bytes[4] & 2 != 0 {
                let base = if foreground { 30 } else { 40 };
                let n = bytes[1];
                let code = if n < 8 { base + n } else { base + 60 + n - 8 };
                write!(wire, ";{code}").unwrap();
            } else if colon {
                write!(wire, ";{code}:5:{}", bytes[1]).unwrap();
            } else {
                write!(wire, ";{code};5;{}", bytes[1]).unwrap();
            }
            Color::Indexed(bytes[1])
        }
        _ => {
            let [red, green, blue] = [bytes[1], bytes[2], bytes[3]];
            if colon {
                let space = if bytes[4] & 2 != 0 { "0" } else { "" };
                write!(wire, ";{code}:2:{space}:{red}:{green}:{blue}").unwrap();
            } else {
                write!(wire, ";{code};2;{red};{green};{blue}").unwrap();
            }
            Color::Rgb { red, green, blue }
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let mut seed = [0_u8; 12];
    let count = data.len().min(seed.len());
    seed[..count].copy_from_slice(&data[..count]);
    let mut wire = Vec::from(&b"\x1b[0"[..]);
    let mut expected = Style {
        bold: seed[0] & 1 != 0,
        faint: seed[0] & 2 != 0,
        italic: seed[0] & 4 != 0,
        blink: seed[0] & 8 != 0,
        inverse: seed[0] & 16 != 0,
        hidden: seed[0] & 32 != 0,
        strikethrough: seed[0] & 64 != 0,
        underline: match seed[1] % 3 {
            0 => Underline::None,
            1 => Underline::Single,
            _ => Underline::Double,
        },
        ..Style::default()
    };
    for (enabled, code) in [
        (expected.bold, 1),
        (expected.faint, 2),
        (expected.italic, 3),
        (expected.blink, 5),
        (expected.inverse, 7),
        (expected.hidden, 8),
        (expected.strikethrough, 9),
    ] {
        if enabled {
            write!(&mut wire, ";{code}").unwrap();
        }
    }
    match expected.underline {
        Underline::None => {}
        Underline::Single => wire.extend(b";4"),
        Underline::Double => wire.extend(b";21"),
    }
    expected.foreground = color(&mut wire, true, &seed[2..7]);
    expected.background = color(&mut wire, false, &seed[7..12]);
    wire.push(b'm');
    for chunk in [wire.len(), seed[1] as usize % 17 + 1] {
        let mut parser = Parser::default();
        let mut actual = Style {
            bold: !expected.bold,
            ..expected
        };
        let mut sequences = 0;
        for bytes in wire.chunks(chunk) {
            parser.advance(bytes, |event| match event {
                Event::Csi(sequence) => {
                    assert!(actual.apply_sgr(sequence.parameters));
                    sequences += 1;
                }
                _ => panic!("The encoder emits one complete SGR sequence"),
            });
        }
        assert_eq!(sequences, 1);
        assert_eq!(actual, expected);
    }

    let mut parameters = [Parameter::default(); MAX_PARAMETERS + 1];
    let length = (data.len() / 3).min(MAX_PARAMETERS);
    for (parameter, bytes) in parameters
        .iter_mut()
        .zip(data.as_chunks::<3>().0)
        .take(length)
    {
        *parameter = Parameter {
            value: (bytes[2] & 1 == 0).then_some(u16::from_le_bytes([bytes[0], bytes[1]])),
            is_subparameter: bytes[2] & 2 != 0,
        };
    }
    let mut actual = expected;
    if actual.apply_sgr(&parameters[..length]) {
        let once = actual;
        assert!(actual.apply_sgr(&parameters[..length]));
        assert_eq!(actual, once);
        parameters[length] = Parameter {
            value: Some(0),
            is_subparameter: false,
        };
        assert!(actual.apply_sgr(&parameters[..=length]));
        assert_eq!(actual, Style::default());
    } else {
        assert_eq!(actual, expected);
    }
});
