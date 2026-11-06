extern crate std;

use std::{string::String, vec::Vec};
use unicode_segmentation::UnicodeSegmentation;

use super::*;

fn apply(output: &mut Vec<(String, usize)>, update: TextUpdate<'_>) {
    match update {
        TextUpdate::Ascii(text) => output.extend(text.chars().map(|value| (value.into(), 1))),
        TextUpdate::Start(unit) => output.push((unit.text.into(), unit.columns)),
        TextUpdate::Extend(unit) => {
            *output.last_mut().expect("An extension follows a start") =
                (unit.text.into(), unit.columns);
        }
    }
}

#[test]
fn clusters_and_widths_survive_every_input_split() {
    let cases: &[(&str, usize, usize)] = &[
        ("e\u{301}", 1, 1),
        ("水", 2, 2),
        ("·", 1, 2),
        ("👩🏽\u{200d}💻", 2, 2),
        ("🇺🇸", 2, 2),
        ("1\u{fe0f}\u{20e3}", 2, 2),
        ("\u{1100}\u{1161}\u{11a8}", 2, 2),
        ("\u{600}a", 2, 2),
        ("क्\u{200d}ष", 2, 2),
        ("\u{301}", 0, 0),
        ("\u{17d8}", 3, 3),
    ];
    let text: String = cases
        .iter()
        .map(|(text, _, _)| *text)
        .collect::<Vec<_>>()
        .join("|");
    for policy in [WidthPolicy::Narrow, WidthPolicy::Wide] {
        let mut expected: Vec<(String, usize)> = Vec::new();
        for (index, &(text, narrow, wide)) in cases.iter().enumerate() {
            if index != 0 {
                expected.push(("|".into(), 1));
            }
            expected.push((
                text.into(),
                if policy == WidthPolicy::Narrow {
                    narrow
                } else {
                    wide
                },
            ));
        }
        // A leading combining mark attaches to the delimiter in a continuous stream.
        let zero = expected
            .iter()
            .position(|(text, _)| text == "\u{301}")
            .unwrap();
        expected[zero - 1].0.push('\u{301}');
        expected.remove(zero);
        for first in text
            .char_indices()
            .map(|(offset, _)| offset)
            .chain([text.len()])
        {
            for second in text[first..]
                .char_indices()
                .map(|(offset, _)| first + offset)
                .chain([text.len()])
            {
                let mut buffer = GraphemeBuffer::new(policy);
                let mut output = Vec::new();
                for part in [&text[..first], &text[first..second], &text[second..]] {
                    assert_eq!(
                        buffer.advance(part, |update| {
                            apply(&mut output, update);
                            Ok::<_, ()>(())
                        }),
                        Ok(part.len())
                    );
                }
                assert_eq!(output, expected, "{policy:?}, splits {first}/{second}");
                let last = expected.last().unwrap();
                assert_eq!(
                    buffer.current(),
                    Some(Grapheme {
                        text: &last.0,
                        columns: last.1
                    })
                );
            }
        }
    }
}

#[test]
fn updates_expose_late_width_changes_and_explicit_boundaries() {
    for (text, selector, before, after) in [("❤", '\u{fe0f}', 1, 2), ("⌚", '\u{fe0e}', 2, 1)] {
        let mut buffer = GraphemeBuffer::default();
        buffer
            .advance(text, |update| {
                assert_eq!(
                    update,
                    TextUpdate::Start(Grapheme {
                        text,
                        columns: before
                    })
                );
                Ok::<_, ()>(())
            })
            .unwrap();
        let mut bytes = [0; 4];
        let suffix = selector.encode_utf8(&mut bytes);
        buffer
            .advance(suffix, |update| {
                let TextUpdate::Extend(unit) = update else {
                    panic!("Expected extension")
                };
                assert_eq!(unit.text, String::from(text) + suffix);
                assert_eq!(unit.columns, after);
                Ok::<_, ()>(())
            })
            .unwrap();
        buffer.clear();
        assert_eq!(buffer.current(), None);
        buffer
            .advance("\u{301}", |update| {
                assert_eq!(
                    update,
                    TextUpdate::Start(Grapheme {
                        text: "\u{301}",
                        columns: 0
                    })
                );
                Ok::<_, ()>(())
            })
            .unwrap();
        buffer
            .advance("", |_| -> Result<(), ()> { panic!("Empty input") })
            .unwrap();
        assert_eq!(buffer.current().unwrap().text, "\u{301}");
    }
}

#[test]
fn rejection_retries_the_exact_suffix_without_changing_the_current_unit() {
    let text = "abc\u{301}水\u{600}dEF👩🏽\u{200d}💻xyz";
    let mut baseline = Vec::new();
    let mut event_count = 0;
    GraphemeBuffer::default()
        .advance(text, |update| {
            event_count += 1;
            apply(&mut baseline, update);
            Ok::<_, ()>(())
        })
        .unwrap();
    for rejected in 0..event_count {
        let mut buffer = GraphemeBuffer::default();
        let mut output = Vec::new();
        let mut index = 0;
        let error = buffer
            .advance(text, |update| {
                if index == rejected {
                    return Err(7);
                }
                index += 1;
                apply(&mut output, update);
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.error, TextFailure::Rejected(7));
        let before = buffer
            .current()
            .map(|unit| (String::from(unit.text), unit.columns));
        let suffix = &text[error.consumed..];
        for _ in 0..2 {
            assert_eq!(
                buffer.advance(suffix, |_| Err(7)),
                Err(TextError {
                    consumed: 0,
                    error: TextFailure::Rejected(7)
                })
            );
            assert_eq!(
                buffer
                    .current()
                    .map(|unit| (String::from(unit.text), unit.columns)),
                before
            );
        }
        buffer
            .advance(suffix, |update| {
                apply(&mut output, update);
                Ok::<_, ()>(())
            })
            .unwrap();
        assert_eq!(output, baseline);
    }
}

#[test]
fn capacity_failure_preserves_a_full_cluster_and_allows_a_new_start() {
    let text = String::from("é") + &"\u{301}".repeat((MAX_GRAPHEME_BYTES - 2) / 2);
    assert_eq!(text.len(), MAX_GRAPHEME_BYTES);
    let mut buffer = GraphemeBuffer::default();
    buffer.advance(&text, |_| Ok::<_, ()>(())).unwrap();
    for _ in 0..2 {
        assert_eq!(
            buffer.advance("\u{301}z", |_| -> Result<(), ()> {
                panic!("Oversized extension")
            }),
            Err(TextError {
                consumed: 0,
                error: TextFailure::TooLong
            })
        );
        assert_eq!(buffer.current().unwrap().text, text);
    }
    assert_eq!(
        buffer.advance("🌙", |_| Err(9)),
        Err(TextError {
            consumed: 0,
            error: TextFailure::Rejected(9)
        })
    );
    assert_eq!(buffer.current().unwrap().text, text);
    buffer
        .advance("🌙", |update| {
            assert_eq!(
                update,
                TextUpdate::Start(Grapheme {
                    text: "🌙",
                    columns: 2
                })
            );
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(buffer.current().unwrap().text, "🌙");
    buffer.clear();
    buffer.advance("ok", |_| Ok::<_, ()>(())).unwrap();
    assert_eq!(buffer.current().unwrap().text, "k");
}

#[test]
fn controls_stop_at_the_accepted_prefix() {
    for value in (0..=0x9f)
        .filter_map(char::from_u32)
        .filter(|value| value.is_control())
    {
        let text = String::from("ok") + value.encode_utf8(&mut [0; 4]) + "after";
        let mut buffer = GraphemeBuffer::new(WidthPolicy::Wide);
        let mut output = Vec::new();
        assert_eq!(
            buffer.advance(&text, |update| {
                apply(&mut output, update);
                Ok::<_, ()>(())
            }),
            Err(TextError {
                consumed: 2,
                error: TextFailure::Control(value)
            })
        );
        assert_eq!(
            buffer.current(),
            Some(Grapheme {
                text: "k",
                columns: 1
            })
        );
        assert_eq!(output, [(String::from("o"), 1), (String::from("k"), 1)]);
        buffer.clear();
        assert_eq!(buffer.policy(), WidthPolicy::Wide);
    }
}

#[test]
fn zero_width_suffix_matches_full_sequence_evaluation() {
    let marks: Vec<_> = (0..=0x10ffff)
        .filter_map(char::from_u32)
        .filter(|scalar| scalar.width() == Some(0) || scalar.width_cjk() == Some(0))
        .collect();
    for policy in [WidthPolicy::Narrow, WidthPolicy::Wide] {
        for prefix in [
            "e",
            "·",
            "<",
            "“\u{fe01}",
            "❤\u{fe0f}",
            "⌚\u{fe0e}",
            "👩\u{200d}💻",
            "क्\u{200d}ष",
            "\u{600}a",
            "\u{301}",
        ] {
            for &mark in &marks {
                let mut buffer = GraphemeBuffer::new(policy);
                buffer.advance(prefix, |_| Ok::<_, ()>(())).unwrap();
                let mut text = String::from(prefix);
                for _ in 0..2 {
                    text.push(mark);
                    buffer
                        .advance(mark.encode_utf8(&mut [0; 4]), |_| Ok::<_, ()>(()))
                        .unwrap();
                    let last = text.graphemes(true).next_back().unwrap();
                    assert_eq!(
                        buffer.current(),
                        Some(Grapheme {
                            text: last,
                            columns: policy.columns(last)
                        }),
                        "{policy:?} {text:?}"
                    );
                }
                // A later sequence-sensitive suffix evaluates its complete cluster.
                text.push('\u{338}');
                buffer.advance("\u{338}", |_| Ok::<_, ()>(())).unwrap();
                let last = text.graphemes(true).next_back().unwrap();
                assert_eq!(buffer.current().unwrap().columns, policy.columns(last));
            }
        }
    }
}
