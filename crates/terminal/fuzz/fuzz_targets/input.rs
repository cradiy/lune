#![no_main]

use libfuzzer_sys::fuzz_target;
use lune_terminal::{KeySequence, Modifiers};

// ASCII Control results; 255 marks an unmapped combination.
const CONTROL: [u8; 128] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26, 27, 28, 29, 30, 31, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
    255, 31, 255, 255, 0, 27, 28, 29, 30, 31, 127, 255, 255, 255, 255, 255, 255, 127, 0, 1, 2, 3,
    4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28,
    29, 30, 31, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22,
    23, 24, 25, 26, 27, 28, 29, 30, 127,
];

fuzz_target!(|data: &[u8]| {
    for record in data.as_chunks::<5>().0 {
        let value = u32::from_le_bytes(record[..4].try_into().unwrap()) % 0x110000;
        let Some(character) = char::from_u32(value) else {
            continue;
        };
        let modifiers = Modifiers {
            shift: record[4] & 1 != 0,
            alt: record[4] & 2 != 0,
            control: record[4] & 4 != 0,
        };
        let encoded = KeySequence::from_character(character, modifiers);
        let control = if character.is_ascii() {
            CONTROL[character as usize]
        } else {
            255
        };
        if modifiers.control && control == 255 {
            assert!(encoded.is_none());
            continue;
        }
        let encoded = encoded.unwrap();
        let bytes = encoded.as_bytes();
        let prefix = usize::from(modifiers.alt);
        assert!((1..=5).contains(&bytes.len()));
        if modifiers.alt {
            assert_eq!(bytes[0], 0x1b);
        }
        if modifiers.control {
            assert_eq!(&bytes[prefix..], &[control]);
        } else {
            let mut decoded = std::str::from_utf8(&bytes[prefix..]).unwrap().chars();
            assert_eq!(decoded.next(), Some(character));
            assert_eq!(decoded.next(), None);
        }
        assert_eq!(
            KeySequence::from_character(
                character,
                Modifiers {
                    shift: !modifiers.shift,
                    ..modifiers
                }
            ),
            Some(encoded)
        );
    }
});
