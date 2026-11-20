#![no_main]

use libfuzzer_sys::fuzz_target;
use lune_terminal::{
    CellPosition, InputModes, MAX_DIMENSION, Modifiers, MouseButton, MouseEvent, MouseTracking,
};

fuzz_target!(|data: &[u8]| {
    for record in data.as_chunks::<8>().0 {
        use MouseButton::{Button8, Button9, Button10, Button11, Left, Middle, Right};
        use MouseEvent::{Motion, Press, Release, WheelDown, WheelLeft, WheelRight, WheelUp};
        let cases = [
            (Press(Left), 0, 0, 'M', 0b1110),
            (Press(Middle), 1, 1, 'M', 0b1110),
            (Press(Right), 2, 2, 'M', 0b1110),
            (Release(Left), 3, 0, 'm', 0b1110),
            (Release(Middle), 3, 1, 'm', 0b1110),
            (Release(Right), 3, 2, 'm', 0b1110),
            (Motion(Some(Left)), 32, 32, 'M', 0b1100),
            (Motion(Some(Middle)), 33, 33, 'M', 0b1100),
            (Motion(Some(Right)), 34, 34, 'M', 0b1100),
            (Motion(None), 35, 35, 'M', 0b1000),
            (WheelUp, 64, 64, 'M', 0b1110),
            (WheelDown, 65, 65, 'M', 0b1110),
            (WheelLeft, 66, 66, 'M', 0b1110),
            (WheelRight, 67, 67, 'M', 0b1110),
            (Press(Button8), 128, 128, 'M', 0b1110),
            (Press(Button9), 129, 129, 'M', 0b1110),
            (Press(Button10), 130, 130, 'M', 0b1110),
            (Press(Button11), 131, 131, 'M', 0b1110),
            (Release(Button8), 3, 128, 'm', 0b1110),
            (Release(Button9), 3, 129, 'm', 0b1110),
            (Release(Button10), 3, 130, 'm', 0b1110),
            (Release(Button11), 3, 131, 'm', 0b1110),
            (Motion(Some(Button8)), 160, 160, 'M', 0b1100),
            (Motion(Some(Button9)), 161, 161, 'M', 0b1100),
            (Motion(Some(Button10)), 162, 162, 'M', 0b1100),
            (Motion(Some(Button11)), 163, 163, 'M', 0b1100),
        ];
        let (event, normal, sgr, suffix, allowed) = cases[usize::from(record[5]) % cases.len()];
        let tracking = usize::from(record[6] % 4);
        let modes = InputModes {
            mouse_tracking: [
                MouseTracking::Disabled,
                MouseTracking::Buttons,
                MouseTracking::ButtonMotion,
                MouseTracking::AnyMotion,
            ][tracking],
            sgr_mouse: record[7] & 1 != 0,
            ..InputModes::default()
        };
        let position = CellPosition {
            column: usize::from(u16::from_le_bytes([record[0], record[1]])),
            row: usize::from(u16::from_le_bytes([record[2], record[3]])),
        };
        let modifiers = Modifiers {
            shift: record[4] & 1 != 0,
            alt: record[4] & 2 != 0,
            control: record[4] & 4 != 0,
        };
        let modifier_code = [0, 4, 8, 12, 16, 20, 24, 28][usize::from(record[4] % 8)];
        let encoded = modes.encode_mouse(event, position, modifiers);
        let bound = if modes.sgr_mouse { MAX_DIMENSION } else { 223 };
        if allowed & (1 << tracking) == 0 || position.row >= bound || position.column >= bound {
            assert!(encoded.is_none());
        } else if modes.sgr_mouse {
            let expected = format!(
                "\x1b[<{};{};{}{suffix}",
                sgr + modifier_code,
                position.column + 1,
                position.row + 1
            );
            let encoded = encoded.unwrap();
            assert!(encoded.as_bytes().len() <= 19);
            assert_eq!(encoded.as_bytes(), expected.as_bytes());
        } else {
            assert_eq!(
                encoded.unwrap().as_bytes(),
                &[
                    27,
                    91,
                    77,
                    normal + modifier_code + 32,
                    position.column as u8 + 33,
                    position.row as u8 + 33
                ]
            );
        }
    }
});
