#![no_main]

mod support;

use libfuzzer_sys::fuzz_target;
use lune_vt::Parser;
use support::{capture, parse, random};

fuzz_target!(|data: &[u8]| {
    let budget = data.len().saturating_mul(2).saturating_add(2);
    let mut parser = Parser::default();
    let mut actual = Vec::new();
    let mut pending = Vec::new();
    let mut offset = 0;
    let mut seed = data.len() as u64;

    while offset < data.len() {
        let length = (random(&mut seed) % 64 + 1).min(data.len() - offset);
        let chunk = &data[offset..offset + length];
        parser.advance(chunk, |event| capture(&mut actual, budget, event));
        pending.extend_from_slice(chunk);
        offset += length;

        match random(&mut seed) % 4 {
            0 => {
                parser.finish(|event| capture(&mut actual, budget, event));
                assert_eq!(actual, parse(&pending, true));
                parser.finish(|_| panic!("Repeated finish emitted an event"));
                pending.clear();
                actual.clear();
            }
            1 => {
                assert_eq!(actual, parse(&pending, false));
                parser.reset();
                parser.reset();
                pending.clear();
                actual.clear();
            }
            _ => {}
        }
    }

    parser.finish(|event| capture(&mut actual, budget, event));
    assert_eq!(actual, parse(&pending, true));

    actual.clear();
    let probe = b"Z\x1b[?38:2::12;0$px";
    let probe_budget = probe.len() * 2 + 2;
    parser.advance(probe, |event| capture(&mut actual, probe_budget, event));
    parser.finish(|event| capture(&mut actual, probe_budget, event));
    assert_eq!(actual, parse(probe, true));
});
