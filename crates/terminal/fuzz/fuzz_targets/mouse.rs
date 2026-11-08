#![no_main]

use libfuzzer_sys::fuzz_target;
use lune_terminal::{
    CellPosition, InputModes, MAX_DIMENSION, Modifiers, MouseButton, MouseEvent, MouseTracking,
};

fuzz_target!(|data: &[u8]| {
    for record in data.as_chunks::<8>().0 {
        use MouseButton::{Left, Middle, Right};
        use MouseEvent::{Motion, Press, Release, WheelDown, WheelUp};
        let (event, normal, sgr, suffix, allowed) = [
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
        ][usize::from(record[5] % 12)];
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
            assert!(encoded.as_bytes().len() <= 18);
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
