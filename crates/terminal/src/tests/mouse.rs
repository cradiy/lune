use super::support;

use crate::{
    CellPosition, InputModes, MAX_DIMENSION, Modifiers, MouseButton, MouseEvent, MouseFormat,
    MouseTracking, PixelPosition, Screen, ScreenBuffer, StorageError,
};
use MouseFormat::{Legacy, Sgr, SgrPixels};

#[test]
fn mouse_reports_match_button_motion_wheel_and_modifier_wire_codes() {
    use MouseButton::{Button8, Button9, Button10, Button11, Left, Middle, Right};
    use MouseEvent::{Motion, Press, Release, WheelDown, WheelLeft, WheelRight, WheelUp};
    let events = [
        (Press(Left), 0, 0, 'M', [false, true, true, true]),
        (Press(Middle), 1, 1, 'M', [false, true, true, true]),
        (Press(Right), 2, 2, 'M', [false, true, true, true]),
        (Release(Left), 3, 0, 'm', [false, true, true, true]),
        (Release(Middle), 3, 1, 'm', [false, true, true, true]),
        (Release(Right), 3, 2, 'm', [false, true, true, true]),
        (Motion(Some(Left)), 32, 32, 'M', [false, false, true, true]),
        (
            Motion(Some(Middle)),
            33,
            33,
            'M',
            [false, false, true, true],
        ),
        (Motion(Some(Right)), 34, 34, 'M', [false, false, true, true]),
        (Motion(None), 35, 35, 'M', [false, false, false, true]),
        (WheelUp, 64, 64, 'M', [false, true, true, true]),
        (WheelDown, 65, 65, 'M', [false, true, true, true]),
        (WheelLeft, 66, 66, 'M', [false, true, true, true]),
        (WheelRight, 67, 67, 'M', [false, true, true, true]),
        (Press(Button8), 128, 128, 'M', [false, true, true, true]),
        (Release(Button9), 3, 129, 'm', [false, true, true, true]),
        (Press(Button10), 130, 130, 'M', [false, true, true, true]),
        (
            Motion(Some(Button11)),
            163,
            163,
            'M',
            [false, false, true, true],
        ),
    ];
    for (index, mouse_tracking) in [
        MouseTracking::Disabled,
        MouseTracking::Buttons,
        MouseTracking::ButtonMotion,
        MouseTracking::AnyMotion,
    ]
    .into_iter()
    .enumerate()
    {
        for mouse_format in [Legacy, Sgr] {
            let modes = InputModes {
                mouse_tracking,
                mouse_format,
                ..InputModes::default()
            };
            for (flags, modifier_code) in [0, 4, 8, 12, 16, 20, 24, 28].into_iter().enumerate() {
                let modifiers = Modifiers {
                    shift: flags & 1 != 0,
                    alt: flags & 2 != 0,
                    control: flags & 4 != 0,
                };
                for (event, normal, sgr, suffix, enabled) in events {
                    let actual =
                        modes.encode_mouse(event, CellPosition { row: 19, column: 9 }, modifiers);
                    if !enabled[index] {
                        assert!(actual.is_none(), "{mouse_tracking:?} {event:?}");
                        continue;
                    }
                    let actual = actual.unwrap();
                    if mouse_format == Sgr {
                        assert_eq!(
                            actual.as_bytes(),
                            format!("\x1b[<{};10;20{suffix}", sgr + modifier_code).as_bytes()
                        );
                    } else {
                        assert_eq!(
                            actual.as_bytes(),
                            &[27, 91, 77, normal + modifier_code + 32, 42, 52]
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn mouse_coordinates_obey_wire_and_grid_bounds_without_truncation() {
    for mouse_format in [Legacy, Sgr] {
        let modes = InputModes {
            mouse_tracking: MouseTracking::AnyMotion,
            mouse_format,
            ..InputModes::default()
        };
        for row in [
            0,
            8,
            9,
            98,
            99,
            222,
            223,
            998,
            999,
            9998,
            9999,
            MAX_DIMENSION - 1,
            MAX_DIMENSION,
            usize::MAX,
        ] {
            for column in [
                0,
                9,
                99,
                222,
                223,
                999,
                9999,
                MAX_DIMENSION - 1,
                MAX_DIMENSION,
                usize::MAX,
            ] {
                let actual = modes.encode_mouse(
                    MouseEvent::Motion(Some(MouseButton::Button11)),
                    CellPosition { row, column },
                    Modifiers {
                        shift: true,
                        alt: true,
                        control: true,
                    },
                );
                let bound = if mouse_format == Sgr {
                    MAX_DIMENSION
                } else {
                    223
                };
                if row >= bound || column >= bound {
                    assert!(actual.is_none());
                } else if mouse_format == Sgr {
                    assert_eq!(
                        actual.unwrap().as_bytes(),
                        format!("\x1b[<191;{};{}M", column + 1, row + 1).as_bytes()
                    );
                } else {
                    assert_eq!(
                        actual.unwrap().as_bytes(),
                        &[27, 91, 77, 223, column as u8 + 33, row as u8 + 33]
                    );
                }
            }
        }
    }
}

#[test]
fn pixel_mouse_reports_use_one_based_axes_and_check_overflow() {
    let event = MouseEvent::Motion(Some(MouseButton::Button11));
    let modifiers = Modifiers {
        shift: true,
        alt: true,
        control: true,
    };
    let modes = InputModes {
        mouse_tracking: MouseTracking::ButtonMotion,
        mouse_format: SgrPixels,
        ..InputModes::default()
    };
    let position = PixelPosition { x: 0, y: 240 };
    let report = modes
        .encode_pixel_mouse(event, position, modifiers)
        .unwrap();
    assert_eq!(report.as_bytes(), b"\x1b[<191;1;241M");
    let max = u32::MAX - 1;
    assert_eq!(
        modes
            .encode_pixel_mouse(event, PixelPosition { x: max, y: max }, modifiers)
            .unwrap()
            .as_bytes(),
        b"\x1b[<191;4294967295;4294967295M"
    );
    for position in [
        PixelPosition { x: u32::MAX, y: 0 },
        PixelPosition { x: 0, y: u32::MAX },
    ] {
        assert!(
            modes
                .encode_pixel_mouse(event, position, modifiers)
                .is_none()
        );
    }
    assert!(
        modes
            .encode_mouse(event, CellPosition { row: 0, column: 0 }, modifiers)
            .is_none()
    );
    assert!(
        InputModes {
            mouse_format: Sgr,
            ..modes
        }
        .encode_pixel_mouse(event, position, modifiers)
        .is_none()
    );
    assert!(
        modes
            .encode_pixel_mouse(MouseEvent::Motion(None), position, modifiers)
            .is_none()
    );
}

#[test]
fn alternate_scroll_routes_vertical_wheel_steps_only_without_mouse_tracking() {
    let mut screen = Screen::new(8, 4).unwrap();
    let mut processor = support::processor();
    let mut text = |_: &mut Screen, _: &str, _| Ok::<_, StorageError>(());
    let wheel_up = MouseEvent::WheelUp;
    let wheel_down = MouseEvent::WheelDown;
    processor
        .advance(b"\x1b[?1049;1007;65535h\x1b[?1049h", &mut screen, &mut text)
        .unwrap();
    assert_eq!(processor.ignored_sequences(), 1);
    assert_eq!(screen.active_buffer(), ScreenBuffer::Alternate);
    assert!(
        processor
            .input_modes()
            .encode_alternate_scroll(wheel_up, screen.active_buffer())
            .is_none()
    );
    processor
        .advance(b"\x1b[?1007h", &mut screen, &mut text)
        .unwrap();
    let normal = processor.input_modes();
    let up = normal
        .encode_alternate_scroll(wheel_up, screen.active_buffer())
        .unwrap();
    assert_eq!(up.as_bytes(), b"\x1b[A");
    assert!(
        normal
            .encode_alternate_scroll(MouseEvent::WheelLeft, screen.active_buffer())
            .is_none()
    );
    processor
        .advance(b"\x1b[?1049l\x1b[?1;1006h", &mut screen, &mut text)
        .unwrap();
    assert!(
        processor
            .input_modes()
            .encode_alternate_scroll(wheel_down, screen.active_buffer())
            .is_none()
    );
    processor
        .advance(b"\x1b[?1049h", &mut screen, &mut text)
        .unwrap();
    let application = processor.input_modes();
    assert_eq!(
        application
            .encode_alternate_scroll(wheel_down, screen.active_buffer())
            .unwrap()
            .as_bytes(),
        b"\x1bOB"
    );
    processor
        .advance(b"\x1b[?1002h", &mut screen, &mut text)
        .unwrap();
    let tracked = processor.input_modes();
    assert!(
        tracked
            .encode_alternate_scroll(wheel_down, screen.active_buffer())
            .is_none()
    );
    assert_eq!(
        tracked
            .encode_mouse(
                wheel_down,
                CellPosition { row: 2, column: 4 },
                Modifiers::default(),
            )
            .unwrap()
            .as_bytes(),
        b"\x1b[<65;5;3M"
    );
    processor
        .advance(b"\x1b[?1002l", &mut screen, &mut text)
        .unwrap();
    assert_eq!(processor.input_modes(), application);
    processor.advance(b"\x1bc", &mut screen, &mut text).unwrap();
    assert_eq!(processor.input_modes(), InputModes::default());
    assert_eq!(
        normal.encode_alternate_scroll(wheel_up, ScreenBuffer::Alternate),
        Some(up)
    );
}

#[test]
fn mouse_mode_lists_switch_atomically_and_snapshots_survive_save_and_reset() {
    let cases: &[(&[u8], MouseTracking, MouseFormat, bool)] = &[
        (b"\x1b[?1000h", MouseTracking::Buttons, Legacy, false),
        (b"\x1b[?1002h", MouseTracking::ButtonMotion, Legacy, false),
        (b"\x1b[?1003h", MouseTracking::AnyMotion, Legacy, false),
        (b"\x1b[?1006h", MouseTracking::Disabled, Sgr, false),
        (
            b"\x1b[?1006;1016h",
            MouseTracking::Disabled,
            SgrPixels,
            false,
        ),
        (b"\x1b[?1016;1006h", MouseTracking::Disabled, Sgr, false),
        (
            b"\x1b[?1003;1016h",
            MouseTracking::AnyMotion,
            SgrPixels,
            false,
        ),
        (
            b"\x1b[?1003;1016h\x1b[?1006l",
            MouseTracking::AnyMotion,
            Legacy,
            false,
        ),
        (
            b"\x1b[?1000;1002;1003;1006h",
            MouseTracking::AnyMotion,
            Sgr,
            false,
        ),
        (
            b"\x1b[?1003;1002;1000h",
            MouseTracking::Buttons,
            Legacy,
            false,
        ),
        (
            b"\x1b[?1003;1006h\x1b[?1000l",
            MouseTracking::Disabled,
            Sgr,
            false,
        ),
        (
            b"\x1b[?1003;1006h\x1b[?1006l",
            MouseTracking::AnyMotion,
            Legacy,
            false,
        ),
        (
            b"\x1b[?1003;1006;999h",
            MouseTracking::Disabled,
            Legacy,
            true,
        ),
        (
            b"\x1b[?1003;1006h\x1b[?1000;1006;999l",
            MouseTracking::AnyMotion,
            Sgr,
            true,
        ),
        (b"\x1b[1000h", MouseTracking::Disabled, Legacy, true),
        (b"\x1b[?1003:0h", MouseTracking::Disabled, Legacy, true),
    ];
    for &(wire, tracking, format, ignored) in cases {
        for split in 0..=wire.len() {
            let mut screen = Screen::new(8, 4).unwrap();
            screen.write_ascii(b"content", crate::StyleId(0)).unwrap();
            screen.set_cursor(2, 3);
            let mut processor = support::processor();
            let mut text = |_: &mut Screen, _: &str, _| Ok::<_, StorageError>(());
            let before = super::snapshot(screen.grid());
            let cursor = screen.cursor();
            processor
                .advance(&wire[..split], &mut screen, &mut text)
                .unwrap();
            processor
                .advance(&wire[split..], &mut screen, &mut text)
                .unwrap();
            let snapshot = processor.input_modes();
            assert_eq!(
                (snapshot.mouse_tracking, snapshot.mouse_format),
                (tracking, format)
            );
            assert_eq!(processor.ignored_sequences(), u64::from(ignored));
            assert_eq!(super::snapshot(screen.grid()), before);
            assert_eq!(screen.cursor(), cursor);
            assert!(processor.reply_bytes().is_empty());
            processor
                .advance(b"\x1b7\x1b[?1049h\x1b[?1049l\x1b8", &mut screen, &mut text)
                .unwrap();
            assert_eq!(processor.input_modes(), snapshot);
            let report = snapshot.encode_mouse(
                MouseEvent::Press(MouseButton::Left),
                CellPosition { row: 0, column: 0 },
                Modifiers::default(),
            );
            processor
                .advance(
                    b"\x1b7\x1b[?1000;1006l\x1b8\x1b[?1049h\x1b[?1049l",
                    &mut screen,
                    &mut text,
                )
                .unwrap();
            assert_eq!(
                processor.input_modes().mouse_tracking,
                MouseTracking::Disabled
            );
            assert_eq!(processor.input_modes().mouse_format, Legacy);
            processor
                .advance(b"\x1b[?1002;1006h\x1bc", &mut screen, &mut text)
                .unwrap();
            assert_eq!(processor.input_modes(), InputModes::default());
            processor
                .advance(b"\x1b[?1003;1006h", &mut screen, &mut text)
                .unwrap();
            processor.reset(&mut screen, &mut text);
            assert_eq!(processor.input_modes(), InputModes::default());
            assert_eq!(
                snapshot.encode_mouse(
                    MouseEvent::Press(MouseButton::Left),
                    CellPosition { row: 0, column: 0 },
                    Modifiers::default()
                ),
                report
            );
        }
    }
}
