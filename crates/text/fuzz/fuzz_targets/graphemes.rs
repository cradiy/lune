#![no_main]

use libfuzzer_sys::fuzz_target;
use lune_text::{GraphemeBuffer, TextFailure, TextUpdate, WidthPolicy};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

fn apply(output: &mut Vec<(String, usize)>, update: TextUpdate<'_>) {
    match update {
        TextUpdate::Ascii(text) => output.extend(text.chars().map(|scalar| (scalar.into(), 1))),
        TextUpdate::Start(unit) => output.push((unit.text.into(), unit.columns)),
        TextUpdate::Extend(unit) => {
            *output.last_mut().unwrap() = (unit.text.into(), unit.columns);
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let mut text = String::new();
    let special = [
        'a', ' ', '·', 'é', '\u{301}', '\u{308}', '\u{600}', '\u{605}', '\u{200d}', '\u{fe0e}',
        '\u{fe0f}', '\u{20e3}', '1', '❤', '⌚', '👩', '🏽', '💻', '🇺', '🇸', '🇨', '水', 'क', '्',
        'ष', '\u{1100}', '\u{1161}', '\u{11a8}', '\u{17d8}', '\u{338}', '\u{1ab0}', '\u{fe01}',
    ];
    for bytes in data.chunks(4).take(256) {
        let mut encoded = [0; 4];
        encoded[..bytes.len()].copy_from_slice(bytes);
        let value = u32::from_le_bytes(encoded);
        let scalar = if value & 1 == 0 {
            special[(value as usize >> 1) % special.len()]
        } else {
            char::from_u32((value >> 1) % 0x110000).unwrap_or('�')
        };
        if !scalar.is_control() {
            text.push(scalar);
        }
    }
    for policy in [WidthPolicy::Narrow, WidthPolicy::Wide] {
        let expected: Vec<_> = text
            .graphemes(true)
            .map(|unit| {
                let width = if policy == WidthPolicy::Narrow {
                    unit.width()
                } else {
                    unit.width_cjk()
                };
                (unit.to_owned(), width)
            })
            .collect();
        for fragmented in [false, true] {
            let mut buffer = GraphemeBuffer::new(policy);
            let mut output = Vec::new();
            let mut offset = 0;
            let mut callbacks = 0;
            let mut rejected = false;
            let reject_at = usize::from(data.first().copied().unwrap_or(0)) % 17;
            while offset < text.len() {
                let count = if fragmented {
                    1 + usize::from(data.get(offset % data.len()).copied().unwrap_or(0)) % 7
                } else {
                    usize::MAX
                };
                let end = text[offset..]
                    .char_indices()
                    .nth(count)
                    .map_or(text.len(), |(i, _)| offset + i);
                let mut consumed = offset;
                while consumed < end {
                    let result = buffer.advance(&text[consumed..end], |update| {
                        let reject = !rejected && callbacks == reject_at;
                        callbacks += 1;
                        if reject {
                            rejected = true;
                            return Err(());
                        }
                        apply(&mut output, update);
                        Ok(())
                    });
                    match result {
                        Ok(count) => {
                            consumed += count;
                        }
                        Err(error) => {
                            assert_eq!(error.error, TextFailure::Rejected(()));
                            consumed += error.consumed;
                            let before = buffer
                                .current()
                                .map(|unit| (unit.text.to_owned(), unit.columns));
                            let again = buffer
                                .advance(&text[consumed..end], |_| Err(()))
                                .unwrap_err();
                            assert_eq!(again.consumed, 0);
                            assert_eq!(
                                buffer
                                    .current()
                                    .map(|unit| (unit.text.to_owned(), unit.columns)),
                                before
                            );
                        }
                    }
                }
                offset = end;
            }
            assert_eq!(output, expected);
            assert_eq!(
                buffer
                    .current()
                    .map(|unit| (unit.text.to_owned(), unit.columns)),
                expected.last().cloned()
            );
            buffer.clear();
            assert_eq!(buffer.current(), None);
        }
    }
});
