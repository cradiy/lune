use super::support;

use crate::{MAX_TITLE_BYTES, ProcessFailure, Processor, Screen, StyleId};

fn advance(processor: &mut Processor, screen: &mut Screen, input: &[u8]) {
    processor
        .advance(input, screen, &mut |_: &mut Screen, _: &str, _: StyleId| {
            Ok::<_, ()>(())
        })
        .unwrap();
}

#[test]
fn working_directory_reports_commit_only_valid_complete_uris() {
    const OLD: &str = "file:///work";
    const NEW: &str = "FILE://shell.example.test/build%20output/%E2%98%BE";
    let wire = format!("\x1b]7;{NEW}\x1b");
    let wire = wire.as_bytes();
    for split in 0..=wire.len() {
        let mut processor = support::processor();
        let mut screen = Screen::new(2, 2).unwrap();
        advance(
            &mut processor,
            &mut screen,
            b"\x1b]2;title\x07\x1b]7;file:///work\x07",
        );
        assert!(processor.take_working_directory_change());
        advance(&mut processor, &mut screen, &wire[..split]);
        advance(&mut processor, &mut screen, &wire[split..]);
        assert_eq!(processor.working_directory_uri(), Some(OLD));
        assert!(!processor.take_working_directory_change());
        advance(&mut processor, &mut screen, b"\\");
        assert_eq!(processor.working_directory_uri(), Some(NEW));
        assert!(processor.take_working_directory_change());
        assert_eq!(processor.take_window_title_change(), Some("title"));

        advance(&mut processor, &mut screen, wire);
        advance(&mut processor, &mut screen, b"\\");
        advance(
            &mut processor,
            &mut screen,
            b"\x1b]7;file:///cancelled\x18\x1b[!p\x1b]7;https://example.test/path\x07\x1b]7;file://example.test\x07\x1b]7;file://user@example.test/path\x07\x1b]7;file:///bad%2\x07\x1b]7;file:///bad%00\x07\x1b]7;file:///bad#fragment\x07",
        );
        assert_eq!(processor.ignored_sequences(), 6);
        assert_eq!(processor.working_directory_uri(), Some(NEW));
        assert!(!processor.take_working_directory_change());

        advance(
            &mut processor,
            &mut screen,
            b"\x1b]7;file://[2001:db8::1]/work\x07",
        );
        assert_eq!(
            processor.working_directory_uri(),
            Some("file://[2001:db8::1]/work")
        );
        advance(&mut processor, &mut screen, b"\x1b]7;\x07");
        assert_eq!(processor.working_directory_uri(), None);
        assert!(processor.take_working_directory_change());
        advance(&mut processor, &mut screen, b"\x1b]7;\x07");
        assert!(!processor.take_working_directory_change());
        assert_eq!(processor.window_title(), "title");
        advance(&mut processor, &mut screen, b"\x1b]7;file:///work\x07\x1bc");
        assert_eq!(processor.working_directory_uri(), None);
        assert!(processor.take_working_directory_change());
    }
}

#[test]
fn titles_commit_atomically_across_chunks_and_coalesce_until_acknowledged() {
    for terminator in [b"\x07".as_slice(), b"\x1b\\"] {
        let mut wire = "\x1b]2;moon 🌙;水\n\x7f".as_bytes().to_vec();
        wire.extend(terminator);
        for split in 0..=wire.len() {
            let mut processor = support::processor();
            let mut screen = Screen::new(2, 2).unwrap();
            advance(&mut processor, &mut screen, b"\x1b]0;old\x07");
            assert_eq!(processor.take_window_title_change(), Some("old"));
            advance(&mut processor, &mut screen, &wire[..split]);
            assert_eq!(
                processor.window_title(),
                if split == wire.len() {
                    "moon 🌙;水"
                } else {
                    "old"
                }
            );
            advance(&mut processor, &mut screen, &wire[split..]);
            assert_eq!(processor.take_window_title_change(), Some("moon 🌙;水"));
            assert_eq!(processor.take_window_title_change(), None);
            advance(&mut processor, &mut screen, &wire);
            assert_eq!(processor.take_window_title_change(), None);
            advance(
                &mut processor,
                &mut screen,
                b"\x1b]0;first\x07\x1b]2;last\x07",
            );
            assert_eq!(processor.take_window_title_change(), Some("last"));
            advance(
                &mut processor,
                &mut screen,
                b"\x1b7\x1b[?1049h\x1b[?1049l\x1b8",
            );
            assert_eq!(processor.window_title(), "last");
            advance(&mut processor, &mut screen, b"\x1b];\x07");
            assert_eq!(processor.take_window_title_change(), Some(""));
            assert_eq!(
                (processor.parse_errors(), processor.ignored_sequences()),
                (0, 0)
            );
        }
    }
}

#[test]
fn title_stack_restores_nested_titles_and_validates_xtwinops() {
    let wire = "\x1b[22t\x1b]2;outer 水\x07\x1b[22;2t\x1b]0;inner 🌙\x1b\\\x1b[23;1t\x1b[23;2;1t\x1b[?23t\x1b[23:2t";
    for split in 0..=wire.len() {
        let mut processor = support::processor();
        let mut screen = Screen::new(2, 2).unwrap();
        advance(&mut processor, &mut screen, &wire.as_bytes()[..split]);
        advance(&mut processor, &mut screen, &wire.as_bytes()[split..]);
        assert_eq!(processor.take_window_title_change(), Some("inner 🌙"));
        assert_eq!(processor.ignored_sequences(), 4);
        advance(&mut processor, &mut screen, b"\x1b[23;0;0t");
        assert_eq!(processor.take_window_title_change(), Some("outer 水"));
        advance(&mut processor, &mut screen, b"\x1b[23;2t");
        assert_eq!(processor.take_window_title_change(), Some(""));
        advance(&mut processor, &mut screen, b"\x1b[23t");
        assert_eq!(processor.take_window_title_change(), None);
    }
}

#[test]
fn title_stack_retains_the_newest_entries_and_clears_on_resets() {
    use crate::osc::MAX_SAVED_TITLES;

    let mut screen = Screen::new(2, 2).unwrap();
    let mut processor = support::processor();
    for index in 0..MAX_SAVED_TITLES + 3 {
        let wire = format!("\x1b]2;saved-{index}\x07\x1b[22t");
        advance(&mut processor, &mut screen, wire.as_bytes());
    }
    assert_eq!(processor.take_window_title_change(), Some("saved-12"));
    advance(&mut processor, &mut screen, b"\x1b[23t");
    assert!(processor.take_window_title_change().is_none());
    for index in (3..MAX_SAVED_TITLES + 2).rev() {
        advance(&mut processor, &mut screen, b"\x1b[23t");
        assert_eq!(processor.window_title(), format!("saved-{index}"));
    }
    advance(&mut processor, &mut screen, b"\x1b[23t");
    assert_eq!(processor.window_title(), "saved-3");
    advance(&mut processor, &mut screen, b"\x1b[22t\x1bc\x1b[23t");
    assert_eq!(processor.window_title(), "");

    advance(&mut processor, &mut screen, b"\x1b]2;saved\x07\x1b[22t");
    processor.reset(&mut screen, &mut |_: &mut Screen, _: &str, _: StyleId| {
        Ok::<_, ()>(())
    });
    advance(&mut processor, &mut screen, b"\x1b[23t");
    assert_eq!(processor.window_title(), "");
}

#[test]
fn titles_reject_invalid_and_oversized_commands_without_publishing_prefixes() {
    let mut cases = vec![
        b"2".to_vec(),
        b"1;icon".to_vec(),
        b"8;;link".to_vec(),
        b"-2;bad".to_vec(),
        b"65536;bad".to_vec(),
        b"000002;bad".to_vec(),
        b"2;\xff".to_vec(),
        b"2;\xe2\x82".to_vec(),
    ];
    let mut oversized = b"2;".to_vec();
    oversized.extend(std::iter::repeat_n(b'x', MAX_TITLE_BYTES + 1));
    cases.push(oversized);
    for chunk in [1, 17, usize::MAX] {
        let mut processor = support::processor();
        let mut screen = Screen::new(2, 2).unwrap();
        let title = "🌙".repeat(MAX_TITLE_BYTES / 4);
        let mut full = b"\x1b]2;".to_vec();
        full.extend(title.as_bytes());
        full.push(7);
        for bytes in full.chunks(chunk) {
            advance(&mut processor, &mut screen, bytes);
        }
        assert_eq!(processor.take_window_title_change(), Some(title.as_str()));
        for (index, payload) in cases.iter().enumerate() {
            advance(&mut processor, &mut screen, b"\x1b]");
            for bytes in payload.chunks(chunk) {
                advance(&mut processor, &mut screen, bytes);
            }
            advance(&mut processor, &mut screen, b"\x07");
            assert_eq!(processor.window_title(), title);
            assert_eq!(processor.take_window_title_change(), None);
            assert_eq!(processor.ignored_sequences(), index as u64 + 1);
        }
        for cancel in [b"\x18".as_slice(), b"\x1a", b"\x1b[0m"] {
            advance(&mut processor, &mut screen, b"\x1b]2;cancelled");
            advance(&mut processor, &mut screen, cancel);
            assert_eq!(processor.window_title(), title);
            assert_eq!(processor.take_window_title_change(), None);
        }
        advance(&mut processor, &mut screen, b"\x1b]2;unfinished\x1b");
        processor
            .finish(&mut screen, &mut |_: &mut Screen, _: &str, _: StyleId| {
                Ok::<_, ()>(())
            })
            .unwrap();
        assert_eq!(processor.window_title(), title);
        assert_eq!(processor.parse_errors(), 1);
        advance(&mut processor, &mut screen, b"\x1b]2;recovered\x07");
        assert_eq!(processor.take_window_title_change(), Some("recovered"));
    }
}

#[test]
fn title_updates_follow_text_failures_reply_backpressure_and_resets() {
    let mut processor = support::processor();
    let mut screen = Screen::new(2, 2).unwrap();
    let wire = b"A\x1b]2;accepted\x07B";
    let error = processor
        .advance(
            wire,
            &mut screen,
            &mut |_: &mut Screen, _: &str, _: StyleId| Err(()),
        )
        .unwrap_err();
    assert_eq!(error.consumed, 0);
    assert_eq!(processor.take_window_title_change(), None);
    let error = processor
        .advance(
            wire,
            &mut screen,
            &mut |_: &mut Screen, text: &str, _: StyleId| {
                if text == "B" { Err(()) } else { Ok(()) }
            },
        )
        .unwrap_err();
    assert_eq!(error.consumed, wire.len() - 1);
    assert_eq!(processor.take_window_title_change(), Some("accepted"));
    advance(&mut processor, &mut screen, &wire[error.consumed..]);
    assert_eq!(processor.take_window_title_change(), None);

    let wire = b"\x1b[5n\x1b[5n\x1b]0;after reply\x07";
    let error = processor
        .advance(
            wire,
            &mut screen,
            &mut |_: &mut Screen, _: &str, _: StyleId| Ok::<_, ()>(()),
        )
        .unwrap_err();
    assert!(matches!(error.error, ProcessFailure::ReplyPending));
    assert_eq!(processor.window_title(), "accepted");
    assert!(processor.consume_reply(processor.reply_bytes().len()));
    advance(&mut processor, &mut screen, &wire[error.consumed..]);
    assert_eq!(processor.take_window_title_change(), Some("after reply"));
    advance(&mut processor, &mut screen, b"\x1b]2;aborted\x1bc");
    assert_eq!(processor.take_window_title_change(), Some(""));
    advance(
        &mut processor,
        &mut screen,
        b"\x1b]0;reset\x07\x1b]2;partial",
    );
    let mut text = |_: &mut Screen, _: &str, _: StyleId| Ok::<_, ()>(());
    processor.reset(&mut screen, &mut text);
    processor.reset(&mut screen, &mut text);
    assert_eq!(processor.take_window_title_change(), Some(""));
    processor.reset(&mut screen, &mut text);
    assert_eq!(processor.take_window_title_change(), None);
    advance(&mut processor, &mut screen, b"\x1b]2;new\x07");
    assert_eq!(processor.take_window_title_change(), Some("new"));
}
