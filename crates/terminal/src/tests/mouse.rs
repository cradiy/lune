use crate::{
    CellPosition, InputModes, MAX_DIMENSION, Modifiers, MouseButton, MouseEvent, MouseTracking,
    Processor, Screen, StorageError,
};

#[test]
fn mouse_reports_match_button_motion_wheel_and_modifier_wire_codes() {
    use MouseButton::{Left, Middle, Right};
    use MouseEvent::{Motion, Press, Release, WheelDown, WheelUp};
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
        for sgr_mouse in [false, true] {
            let modes = InputModes {
                mouse_tracking,
                sgr_mouse,
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
                    if sgr_mouse {
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
    for sgr_mouse in [false, true] {
        let modes = InputModes {
            mouse_tracking: MouseTracking::AnyMotion,
            sgr_mouse,
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
                    MouseEvent::WheelDown,
                    CellPosition { row, column },
                    Modifiers {
                        shift: true,
                        alt: true,
                        control: true,
                    },
                );
                let bound = if sgr_mouse { MAX_DIMENSION } else { 223 };
                if row >= bound || column >= bound {
                    assert!(actual.is_none());
                } else if sgr_mouse {
                    assert_eq!(
                        actual.unwrap().as_bytes(),
                        format!("\x1b[<93;{};{}M", column + 1, row + 1).as_bytes()
                    );
                } else {
                    assert_eq!(
                        actual.unwrap().as_bytes(),
                        &[27, 91, 77, 125, column as u8 + 33, row as u8 + 33]
                    );
                }
            }
        }
    }
}

#[test]
fn mouse_mode_lists_switch_atomically_and_snapshots_survive_save_and_reset() {
    let cases: &[(&[u8], MouseTracking, bool, bool)] = &[
        (b"\x1b[?1000h", MouseTracking::Buttons, false, false),
        (b"\x1b[?1002h", MouseTracking::ButtonMotion, false, false),
        (b"\x1b[?1003h", MouseTracking::AnyMotion, false, false),
        (b"\x1b[?1006h", MouseTracking::Disabled, true, false),
        (
            b"\x1b[?1000;1002;1003;1006h",
            MouseTracking::AnyMotion,
            true,
            false,
        ),
        (
            b"\x1b[?1003;1002;1000h",
            MouseTracking::Buttons,
            false,
            false,
        ),
        (
            b"\x1b[?1003;1006h\x1b[?1000l",
            MouseTracking::Disabled,
            true,
            false,
        ),
        (
            b"\x1b[?1003;1006h\x1b[?1006l",
            MouseTracking::AnyMotion,
            false,
            false,
        ),
        (
            b"\x1b[?1003;1006;999h",
            MouseTracking::Disabled,
            false,
            true,
        ),
        (
            b"\x1b[?1003;1006h\x1b[?1000;1006;999l",
            MouseTracking::AnyMotion,
            true,
            true,
        ),
        (b"\x1b[1000h", MouseTracking::Disabled, false, true),
        (b"\x1b[?1003:0h", MouseTracking::Disabled, false, true),
    ];
    for &(wire, tracking, sgr, ignored) in cases {
        for split in 0..=wire.len() {
            let mut screen = Screen::new(8, 4).unwrap();
            screen.write_ascii(b"content", crate::StyleId(0)).unwrap();
            screen.set_cursor(2, 3);
            let mut processor = Processor::default();
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
                (snapshot.mouse_tracking, snapshot.sgr_mouse),
                (tracking, sgr)
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
            assert!(!processor.input_modes().sgr_mouse);
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
