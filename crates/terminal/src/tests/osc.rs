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

#[test]
fn sized_text_tracks_blocks_across_chunk_boundaries_and_edits() {
    use crate::{CellPosition, UnicodeWriter};

    let mut processor = support::processor();
    let mut screen = Screen::new(12, 6).unwrap();
    let mut writer = UnicodeWriter::default();
    let input = "\x1b]66;s=2;A中\x1b\\";
    for chunk in input.as_bytes().chunks(2) {
        processor.advance(chunk, &mut screen, &mut writer).unwrap();
    }
    assert_eq!((screen.cursor().row, screen.cursor().column), (0, 6));
    for row_offset in 0..2 {
        let row = screen.grid().row(row_offset).unwrap();
        let mut blocks = row.sized_text_rows();
        let a = blocks.next().unwrap();
        let han = blocks.next().unwrap();
        assert_eq!(
            (a.text(), a.columns(), a.row_offset),
            ("A", 0..2, row_offset)
        );
        assert_eq!(
            (han.text(), han.columns(), han.row_offset),
            ("中", 2..6, row_offset)
        );
        assert!(blocks.next().is_none());
    }
    assert_eq!(
        screen
            .grid()
            .text(CellPosition { row: 0, column: 0 }..CellPosition { row: 0, column: 6 })
            .unwrap()
            .to_string(),
        "A中",
    );

    processor
        .advance(b"\x1b[2;1Hxy", &mut screen, &mut writer)
        .unwrap();
    assert_eq!(
        screen.grid().row(1).unwrap().cells()[6].character(),
        Some('x')
    );
    assert_eq!(
        screen.grid().row(1).unwrap().cells()[7].character(),
        Some('y')
    );
    assert_eq!(screen.grid().row(0).unwrap().sized_text_rows().count(), 2);

    let original = screen.grid().row(0).unwrap().revision();
    processor
        .advance(b"\x1b[2;4H\x1b[X", &mut screen, &mut writer)
        .unwrap();
    assert_eq!(screen.grid().row(0).unwrap().sized_text_rows().count(), 1);
    assert_eq!(screen.grid().row(1).unwrap().sized_text_rows().count(), 1);
    assert_ne!(screen.grid().row(0).unwrap().revision(), original);
}

#[test]
fn sized_text_validates_metadata_and_honors_the_explicit_footprint() {
    let mut processor = support::processor();
    let mut screen = Screen::new(8, 5).unwrap();
    advance(
        &mut processor,
        &mut screen,
        b"\x1b]66;s=8;bad\x07\x1b]66;n=2:d=1;bad\x07\x1b]66;s=3;cancelled\x18",
    );
    assert_eq!(screen.cursor().column, 0);
    advance(
        &mut processor,
        &mut screen,
        b"\x1b]66;s=3:w=1:n=1:d=2:v=1:h=2;ab\x07",
    );
    assert_eq!(screen.cursor().column, 3);
    let row = screen.grid().row(2).unwrap();
    let part = row.sized_text_rows().next().unwrap();
    assert_eq!(
        (part.text(), part.columns(), part.row_offset),
        ("ab", 0..3, 2)
    );
    assert_eq!(
        (
            part.size().fraction(),
            part.size().horizontal,
            part.size().vertical
        ),
        (0.5, 2, 1)
    );

    advance(
        &mut processor,
        &mut screen,
        b"\x1b[4;6H\x1b]66;s=2:w=2;next\x07",
    );
    assert_eq!((screen.cursor().row, screen.cursor().column), (3, 4));
    assert_eq!(screen.grid().row(2).unwrap().wrap_end(), Some(5));
    processor
        .advance(
            b"\x1b[3;7Hx",
            &mut screen,
            &mut crate::UnicodeWriter::default(),
        )
        .unwrap();
    assert_eq!(screen.grid().row(2).unwrap().wrap_end(), None);
}

#[test]
fn sized_text_extends_from_later_utf8_and_osc_input() {
    use crate::UnicodeWriter;

    let mut processor = support::processor();
    let mut screen = Screen::new(12, 4).unwrap();
    let mut writer = UnicodeWriter::default();
    processor
        .advance(b"\x1b]66;s=2;A\x07", &mut screen, &mut writer)
        .unwrap();
    let cursor = screen.cursor();
    let revisions: [_; 2] = std::array::from_fn(|row| screen.grid().row(row).unwrap().revision());

    let suffix = "\u{301}\x1b]66;;\u{323}\x1b\\";
    for bytes in suffix.as_bytes().chunks(1) {
        processor.advance(bytes, &mut screen, &mut writer).unwrap();
    }
    assert_eq!(screen.cursor(), cursor);
    for (row, revision) in revisions.into_iter().enumerate() {
        let row = screen.grid().row(row).unwrap();
        assert_eq!(row.sized_text_at(0).unwrap().text(), "A\u{301}\u{323}");
        assert_ne!(row.revision(), revision);
    }

    let input = "\x1b]66;s=2:w=1;🧑\x07\u{200d}💻β";
    for bytes in input.as_bytes().chunks(2) {
        processor.advance(bytes, &mut screen, &mut writer).unwrap();
    }
    let row = screen.grid().row(0).unwrap();
    let emoji = row.sized_text_at(2).unwrap();
    assert_eq!(emoji.text(), "🧑\u{200d}💻");
    assert_eq!(emoji.columns(), 2..4);
    assert_eq!(row.cells()[4].character(), Some('β'));
    assert_eq!(screen.cursor().column, 5);
}

#[test]
fn sized_text_variation_selectors_adjust_natural_widths_and_wrap() {
    use crate::UnicodeWriter;

    let mut processor = support::processor();
    let mut screen = Screen::new(8, 6).unwrap();
    let mut writer = UnicodeWriter::default();
    let input =
        "\x1b]66;;❤\x07\u{fe0f}\x1b]66;s=2;☕\x07\x1b]66;;\u{fe0e}\x07\x1b]66;w=2;☕\x07\u{fe0e}";
    for bytes in input.as_bytes().chunks(2) {
        processor.advance(bytes, &mut screen, &mut writer).unwrap();
    }
    let head = screen.grid().row(0).unwrap();
    let tail = screen.grid().row(1).unwrap();
    let emoji = head.sized_text_at(0).unwrap();
    assert_eq!((emoji.text(), emoji.columns()), ("❤\u{fe0f}", 0..2));
    let natural = head.sized_text_at(2).unwrap();
    assert_eq!((natural.text(), natural.columns()), ("☕\u{fe0e}", 2..4));
    assert!(natural.belongs_to(tail.sized_text_at(2).unwrap()));
    assert!(tail.sized_text_at(4).is_none());
    let explicit = head.sized_text_at(4).unwrap();
    assert_eq!((explicit.text(), explicit.columns()), ("☕\u{fe0e}", 4..6));
    assert_eq!(screen.cursor().column, 6);

    processor
        .advance(
            "\x1b[4;8H\x1b]66;;❤\x07\u{fe0f}X".as_bytes(),
            &mut screen,
            &mut writer,
        )
        .unwrap();
    assert_eq!((screen.cursor().row, screen.cursor().column), (4, 3));
    assert_eq!(screen.grid().row(3).unwrap().wrap_end(), Some(7));
    assert!(screen.grid().row(3).unwrap().sized_text_at(7).is_none());
    let row = screen.grid().row(4).unwrap();
    assert_eq!(row.sized_text_at(0).unwrap().columns(), 0..2);
    assert_eq!(row.cells()[2].character(), Some('X'));

    screen
        .set_history_limits(crate::HistoryLimits {
            max_rows: 4,
            max_bytes: 4096,
        })
        .unwrap();
    processor
        .advance(
            "\x1b[6;8H\x1b]66;;❤\x07\u{fe0f}\x1b[6;8H\x1b]66;;☕\x07\u{fe0e}Y".as_bytes(),
            &mut screen,
            &mut writer,
        )
        .unwrap();
    assert_eq!(screen.history().len(), 2);
    assert_eq!((screen.cursor().row, screen.cursor().column), (5, 2));
    assert_eq!(screen.grid().row(4).unwrap().wrap_end(), Some(7));
    let row = screen.grid().row(5).unwrap();
    assert_eq!(row.sized_text_at(0).unwrap().columns(), 0..1);
    assert_eq!(row.cells()[1].character(), Some('Y'));

    processor
        .advance(
            "\x1b[5;8H\x1b]66;;☕\u{fe0e}\x07Z".as_bytes(),
            &mut screen,
            &mut writer,
        )
        .unwrap();
    assert_eq!((screen.cursor().row, screen.cursor().column), (5, 2));
    assert_eq!(screen.grid().row(4).unwrap().wrap_end(), Some(7));
    let row = screen.grid().row(5).unwrap();
    assert_eq!(row.sized_text_at(0).unwrap().columns(), 0..1);
    assert_eq!(row.cells()[1].character(), Some('Z'));
}

#[test]
fn sized_text_combines_at_the_right_edge_without_auto_wrap() {
    use crate::UnicodeWriter;

    let mut processor = support::processor();
    let mut screen = Screen::new(6, 4).unwrap();
    let mut writer = UnicodeWriter::default();
    let input =
        "\x1b[?7l\x1b[1;5H\x1b]66;w=1;A\x07\x1b]66;w=1;e\x07\x1b[31m\u{301}\x1b]66;;\u{323}\x07";
    for byte in input.as_bytes().chunks(1) {
        processor.advance(byte, &mut screen, &mut writer).unwrap();
    }
    assert!(!screen.cursor().wrap_pending);
    assert_eq!((screen.cursor().row, screen.cursor().column), (0, 5));
    let row = screen.grid().row(0).unwrap();
    assert_eq!(row.sized_text_at(4).unwrap().text(), "A");
    assert_eq!(row.sized_text_at(5).unwrap().text(), "e\u{301}\u{323}");

    processor
        .advance("\x1b[1;6H\u{302}".as_bytes(), &mut screen, &mut writer)
        .unwrap();
    let row = screen.grid().row(0).unwrap();
    assert_eq!(row.sized_text_at(4).unwrap().text(), "A\u{302}");
    assert_eq!(row.sized_text_at(5).unwrap().text(), "e\u{301}\u{323}");

    processor
        .advance(
            "\x1b[2;6H\x1b]66;;❤\x07\u{fe0f}\x1b[3;6H\x1b]66;;☕\x07\u{fe0e}\u{301}".as_bytes(),
            &mut screen,
            &mut writer,
        )
        .unwrap();
    let grown = screen.grid().row(1).unwrap().sized_text_at(4).unwrap();
    assert_eq!((grown.text(), grown.columns()), ("❤\u{fe0f}", 4..6));
    let row = screen.grid().row(2).unwrap();
    let shrunk = row.sized_text_at(4).unwrap();
    assert_eq!(
        (shrunk.text(), shrunk.columns()),
        ("☕\u{fe0e}\u{301}", 4..5)
    );
    assert!(row.sized_text_at(5).is_none());
    assert_eq!((screen.cursor().row, screen.cursor().column), (2, 5));
    assert!(!screen.cursor().wrap_pending);
}

#[test]
fn sized_text_widens_at_the_history_join() {
    use crate::{HistoryLimits, UnicodeWriter};

    for (start, in_place) in [(4, true), (5, false)] {
        let mut processor = support::processor();
        let mut screen = Screen::new(6, 3).unwrap();
        let mut writer = UnicodeWriter::default();
        let mut limits = HistoryLimits {
            max_rows: 4,
            max_bytes: 4096,
        };
        screen.set_history_limits(limits).unwrap();
        let input = format!("\x1b[1;{}H\x1b]66;;❤\x07\x1b]66;w=2;ab\x07", start + 1);
        processor
            .advance(input.as_bytes(), &mut screen, &mut writer)
            .unwrap();
        screen.scroll_up(1, StyleId::default()).unwrap();
        let bytes = screen.history().retained_bytes();
        limits.max_bytes = bytes;
        screen.set_history_limits(limits).unwrap();

        processor
            .advance("\x1b[1;1H\u{fe0f}".as_bytes(), &mut screen, &mut writer)
            .unwrap();
        if in_place {
            let history = screen.history().row(0).unwrap();
            assert_eq!(history.wrap_end(), Some(start + 1));
            assert_eq!(history.sized_text_at(start).unwrap().text(), "❤");
            assert_eq!(screen.history().retained_bytes(), bytes);

            limits.max_bytes += 16;
            screen.set_history_limits(limits).unwrap();
            processor
                .advance("\u{fe0f}".as_bytes(), &mut screen, &mut writer)
                .unwrap();
            let history = screen.history().row(0).unwrap();
            let part = history.sized_text_at(start).unwrap();
            assert_eq!(
                (part.text(), part.columns()),
                ("❤\u{fe0f}", start..start + 2)
            );
            assert_eq!(history.wrap_end(), Some(start + 2));
            assert!(screen.history().retained_bytes() > bytes);
            assert!(screen.history().retained_bytes() <= limits.max_bytes);
            assert_eq!(screen.cursor().column, 0);
            assert_eq!(
                screen
                    .grid()
                    .row(0)
                    .unwrap()
                    .sized_text_at(0)
                    .unwrap()
                    .text(),
                "ab"
            );
        } else {
            let history = screen.history().row(0).unwrap();
            assert_eq!(history.wrap_end(), Some(start));
            assert!(history.sized_text_at(start).is_none());
            assert!(screen.history().retained_bytes() < bytes);
            let part = screen.grid().row(0).unwrap().sized_text_at(0).unwrap();
            assert_eq!((part.text(), part.columns()), ("❤\u{fe0f}", 0..2));
            assert_eq!(screen.cursor().column, 2);
        }
        assert_eq!(screen.cursor().row, 0);
        assert_eq!(screen.history().len(), 1);
    }
}

#[test]
fn sized_text_selection_copies_the_first_selected_fragment_once() {
    use crate::{CellPosition, HistoryLimits};

    let mut processor = support::processor();
    let mut screen = Screen::new(8, 4).unwrap();
    screen
        .set_history_limits(HistoryLimits {
            max_rows: 4,
            max_bytes: 4096,
        })
        .unwrap();
    advance(
        &mut processor,
        &mut screen,
        b"\x1b]66;s=2:w=2;AB\x07\x1b]66;s=2;Q\x07",
    );
    let cell = |row, column| CellPosition { row, column };
    let viewport = screen.viewport();
    let hit = cell(1, 2)..cell(1, 3);
    assert_eq!(viewport.selection_bounds(hit.clone()), Some(hit.clone()));
    let tail = viewport.selection_text(hit).unwrap();
    let head = viewport.row(0).unwrap();
    assert!(tail.includes_sized_text(0, head.sized_text_at(2).unwrap()));
    assert!(!tail.includes_sized_text(0, head.sized_text_at(4).unwrap()));
    assert_eq!(tail.to_string(), "AB");
    let multiple = viewport.text(cell(0, 2)..cell(1, 3)).unwrap();
    assert_eq!(multiple.to_string().matches("AB").count(), 1);
    assert_eq!(
        viewport.text(cell(0, 1)..cell(0, 1)).unwrap().to_string(),
        ""
    );

    screen.scroll_up(1, StyleId::default()).unwrap();
    let viewport = screen.viewport();
    let tail = viewport.text(cell(0, 2)..cell(0, 3)).unwrap();
    assert!(tail.includes_sized_text(1, viewport.row(0).unwrap().sized_text_at(2).unwrap()));
    assert_eq!(tail.to_string(), "AB");
}

#[test]
fn sized_text_survives_reflow_until_a_row_is_evicted() {
    use crate::{HistoryLimits, UnicodeWriter};

    let mut processor = support::processor();
    let mut screen = Screen::new(8, 4).unwrap();
    screen
        .set_history_limits(HistoryLimits {
            max_rows: 4,
            max_bytes: 4096,
        })
        .unwrap();
    advance(&mut processor, &mut screen, b"\x1b]66;s=2;A\x07");
    screen.scroll_up(1, StyleId::default()).unwrap();
    let bytes = screen.history().retained_bytes();
    screen
        .set_history_limits(HistoryLimits {
            max_rows: 4,
            max_bytes: bytes,
        })
        .unwrap();
    let mut writer = UnicodeWriter::default();
    processor
        .advance("\u{301}".as_bytes(), &mut screen, &mut writer)
        .unwrap();
    assert_eq!(
        screen
            .history()
            .row(0)
            .unwrap()
            .sized_text_at(0)
            .unwrap()
            .text(),
        "A"
    );
    assert_eq!(screen.history().retained_bytes(), bytes);

    screen
        .set_history_limits(HistoryLimits {
            max_rows: 4,
            max_bytes: 4096,
        })
        .unwrap();
    processor
        .advance("\u{301}".as_bytes(), &mut screen, &mut writer)
        .unwrap();
    assert!(screen.history().retained_bytes() > bytes);
    screen.reflow(10, 4, StyleId::default()).unwrap();

    let view = screen.view();
    let head = view.row(0).unwrap().sized_text_at(0).unwrap();
    let tail = view.row(1).unwrap().sized_text_at(0).unwrap();
    assert!(head.belongs_to(tail));
    assert_eq!(head.text(), "A\u{301}");
    assert_eq!((head.row_offset, tail.row_offset), (0, 1));

    screen.set_history_limits(HistoryLimits::default()).unwrap();
    assert!(screen.grid().row(0).unwrap().sized_text_at(0).is_none());
}
