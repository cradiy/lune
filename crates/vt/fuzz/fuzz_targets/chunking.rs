#![no_main]

mod support;

use libfuzzer_sys::fuzz_target;
use lune_vt::{Event, Parser};
use support::{capture, parse, random};

fuzz_target!(|data: &[u8]| {
    let expected = parse(data, true);
    let budget = data.len().saturating_mul(2).saturating_add(2);
    let mut parser = Parser::default();
    let mut actual = Vec::new();
    for chunk in data.chunks(1) {
        parser.advance(chunk, |event| capture(&mut actual, budget, event));
    }
    parser.finish(|event| capture(&mut actual, budget, event));
    assert_eq!(actual, expected);

    for initial_seed in [1, data.len() as u64] {
        let mut seed = initial_seed;
        let mut offset = 0;
        let mut parser = Parser::default();
        actual.clear();
        while offset < data.len() {
            let length = (random(&mut seed) % 64 + 1).min(data.len() - offset);
            parser.advance(&[], |event| capture(&mut actual, budget, event));
            parser.advance(&data[offset..offset + length], |event| {
                capture(&mut actual, budget, event)
            });
            offset += length;
        }
        parser.finish(|event| capture(&mut actual, budget, event));
        assert_eq!(actual, expected);
    }

    let text_input: Vec<u8> = data
        .iter()
        .map(|&byte| {
            if byte < 0x20 || byte == 0x7f {
                b' '
            } else {
                byte
            }
        })
        .collect();
    let expected = String::from_utf8_lossy(&text_input);
    let mut actual = String::new();
    let mut parser = Parser::default();
    let mut emit = |event: Event<'_>| match event {
        Event::Text(text) => actual.push_str(text),
        _ => panic!("Text-only input emitted a control event"),
    };
    for chunk in text_input.chunks(3) {
        parser.advance(chunk, &mut emit);
    }
    parser.finish(&mut emit);
    assert_eq!(actual, expected);
});
