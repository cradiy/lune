use lune_vt::{Event, Parameter, Parser};

use crate::{Color, Style, Underline};

fn sgr(style: &mut Style, parameters: &str) -> bool {
    let wire = format!("\x1b[{parameters}m");
    let mut accepted = None;
    Parser::default().advance(wire.as_bytes(), |event| match event {
        Event::Csi(sequence) => accepted = Some(style.apply_sgr(sequence.parameters)),
        _ => panic!("The fixture must contain one complete SGR sequence"),
    });
    accepted.unwrap()
}

#[test]
fn rendition_composes_in_order_and_resets_independent_attributes() {
    let mut style = Style::default();
    assert!(sgr(&mut style, "1;2;3;4;5;7;8;9;31;104"));
    assert_eq!(
        style,
        Style {
            foreground: Color::Indexed(1),
            background: Color::Indexed(12),
            underline_color: Color::Default,
            hyperlink: crate::HyperlinkId(0),
            bold: true,
            faint: true,
            italic: true,
            underline: Underline::Single,
            blink: true,
            inverse: true,
            hidden: true,
            strikethrough: true,
        }
    );
    assert!(sgr(&mut style, "22;4:2;27"));
    assert!(!style.bold && !style.faint && !style.inverse);
    assert!(style.italic && style.blink && style.hidden && style.strikethrough);
    assert_eq!(style.underline, Underline::Double);
    assert_eq!(style.foreground, Color::Indexed(1));
    assert_eq!(style.background, Color::Indexed(12));
    assert!(sgr(&mut style, "23;4:0;25;28;29"));
    assert_eq!(
        style,
        Style {
            foreground: Color::Indexed(1),
            background: Color::Indexed(12),
            ..Style::default()
        }
    );
    assert!(sgr(&mut style, "1;0;2;4:5;21;4:1"));
    assert_eq!(
        style,
        Style {
            faint: true,
            underline: Underline::Single,
            ..Style::default()
        }
    );
    assert!(sgr(&mut style, "1;;3"));
    assert_eq!(
        style,
        Style {
            italic: true,
            ..Style::default()
        }
    );
    assert!(sgr(&mut style, ""));
    assert_eq!(style, Style::default());
}

#[test]
fn colors_preserve_palette_identity_and_decode_both_extended_forms() {
    for index in 0..16 {
        let (foreground, background) = if index < 8 {
            (30 + index, 40 + index)
        } else {
            (90 + index - 8, 100 + index - 8)
        };
        let mut style = Style::default();
        assert!(sgr(&mut style, &format!("1;{foreground};{background}")));
        assert_eq!(style.foreground, Color::Indexed(index));
        assert_eq!(style.background, Color::Indexed(index));
        assert!(sgr(&mut style, "22"));
        assert_eq!(style.foreground, Color::Indexed(index));
    }
    for index in 0..=255 {
        let mut style = Style::default();
        assert!(sgr(
            &mut style,
            &format!("38;5;{index};48:5:{index};58:5:{index}")
        ));
        assert_eq!(style.foreground, Color::Indexed(index));
        assert_eq!(style.background, Color::Indexed(index));
        assert_eq!(style.underline_color, Color::Indexed(index));
    }
    for parameters in [
        "38;2;0;128;255;48:2::255:0:1;58;2;2;3;4;4:3;1",
        "38:2::0:128:255;48;2;255;0;1;58:2::2:3:4;4:3;1",
        "38:2:0:0:128:255;48:2:0:255:0:1;58:2:0:2:3:4;4:3;1",
        "38:2:0:128:255;48:2:255:0:1;58:2:2:3:4;4:3;1",
    ] {
        let mut style = Style::default();
        assert!(sgr(&mut style, parameters));
        assert_eq!(
            style,
            Style {
                foreground: Color::Rgb {
                    red: 0,
                    green: 128,
                    blue: 255
                },
                background: Color::Rgb {
                    red: 255,
                    green: 0,
                    blue: 1
                },
                underline_color: Color::Rgb {
                    red: 2,
                    green: 3,
                    blue: 4,
                },
                underline: Underline::Curly,
                bold: true,
                ..Style::default()
            }
        );
        assert!(sgr(&mut style, "24;39"));
        assert_eq!(style.foreground, Color::Default);
        assert_eq!(
            style.underline_color,
            Color::Rgb {
                red: 2,
                green: 3,
                blue: 4
            }
        );
        assert_eq!(
            style.background,
            Color::Rgb {
                red: 255,
                green: 0,
                blue: 1
            }
        );
        assert!(sgr(&mut style, "59;4:3"));
        assert_eq!(style.underline_color, Color::Default);
        assert_eq!(style.underline, Underline::Curly);
        assert!(sgr(&mut style, "24;49"));
        assert_eq!(
            style,
            Style {
                bold: true,
                ..Style::default()
            }
        );
    }
}

#[test]
fn rejected_groups_preserve_rendition_without_reinterpreting_color_components() {
    let mut style = Style::default();
    assert!(sgr(
        &mut style,
        "1;2;3;21;5;7;8;9;38;2;7;8;9;48;5;255;58:5:2"
    ));
    let original = style;
    for parameters in [
        "0;38",
        "0;38;2",
        "0;38;2;1;2",
        "0;38;2;1;;3",
        "0;38;2;1;256;3",
        "0;38;5;",
        "0;48;5;65535",
        "0;38;3;1;2;3",
        "0;38:2:1:2",
        "0;38:2::1:2:256",
        "0;38:2:1:1:2:3",
        "0;38:2::1::3",
        "0;38;2;1:2:3",
        "0;38:2;1;2;3",
        "0;38:5:1:2",
        "0;38;5;1:2",
        "0;58:5:256",
        "0;58;2;1;2",
        "0;58;5;1:2",
        "0;59:0",
        "0;4:",
        "0;4:6",
        "0;4:3:0",
        "0;24:3",
        "0;999",
        "0:1",
    ] {
        assert!(!sgr(&mut style, parameters), "{parameters}");
        assert_eq!(style, original, "{parameters}");
    }
    assert!(!style.apply_sgr(&[Parameter {
        value: Some(0),
        is_subparameter: true
    }]));
    assert_eq!(style, original);
}

#[test]
fn parsed_sgr_sequences_preserve_style_transitions_across_chunk_boundaries() {
    let wire = b"\x1b[1;31m\x1b[48:2::0:128:255m\x1b[38;2;255;0;1;4m\x1b[38;5;256m\x1b[22;39m";
    let red = Color::Rgb {
        red: 255,
        green: 0,
        blue: 1,
    };
    let blue = Color::Rgb {
        red: 0,
        green: 128,
        blue: 255,
    };
    let expected = [
        (
            true,
            Style {
                foreground: Color::Indexed(1),
                bold: true,
                ..Style::default()
            },
        ),
        (
            true,
            Style {
                foreground: Color::Indexed(1),
                background: blue,
                bold: true,
                ..Style::default()
            },
        ),
        (
            true,
            Style {
                foreground: red,
                background: blue,
                bold: true,
                underline: Underline::Single,
                ..Style::default()
            },
        ),
        (
            false,
            Style {
                foreground: red,
                background: blue,
                bold: true,
                underline: Underline::Single,
                ..Style::default()
            },
        ),
        (
            true,
            Style {
                background: blue,
                underline: Underline::Single,
                ..Style::default()
            },
        ),
    ];
    for chunk in 1..=wire.len() {
        let mut parser = Parser::default();
        let mut style = Style::default();
        let mut transitions = Vec::new();
        for bytes in wire.chunks(chunk) {
            parser.advance(bytes, |event| match event {
                Event::Csi(sequence) => {
                    let accepted = style.apply_sgr(sequence.parameters);
                    transitions.push((accepted, style));
                }
                _ => panic!("The fixture contains only SGR sequences"),
            });
        }
        assert_eq!(transitions, expected, "chunk size {chunk}");
    }
}
