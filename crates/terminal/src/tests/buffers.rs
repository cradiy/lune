use crate::{
    CellText, CellWidth, Color, CursorAppearance, CursorShape, Processor, Screen, ScreenBuffer,
    StorageError, StyleId,
};
use ScreenBuffer::{Alternate, Primary};

fn text(screen: &mut Screen, value: &str, style: StyleId) -> Result<(), StorageError> {
    if value.is_ascii() {
        screen.write_ascii(value.as_bytes(), style)
    } else {
        screen.write(value, CellWidth::Single, style)
    }
}

#[test]
fn buffers_keep_contents_wraps_and_save_slots_while_live_state_is_shared() {
    let mut screen = Screen::new(4, 4).unwrap();
    screen.write_ascii(b"abcd", StyleId(1)).unwrap();
    screen
        .write("水\u{301}", CellWidth::Double, StyleId(2))
        .unwrap();
    screen.save_cursor();
    let primary = super::snapshot(screen.grid());
    let cursor = screen.cursor();
    screen.select_buffer(Alternate);
    assert_eq!(screen.cursor(), cursor);
    assert_eq!(screen.buffer(Primary).row(0).unwrap().wrap_end(), Some(4));
    assert_eq!(super::snapshot(screen.buffer(Primary)), primary);
    screen
        .write("月\u{301}", CellWidth::Double, StyleId(3))
        .unwrap();
    screen.save_cursor();
    let alternate = super::snapshot(screen.grid());
    let edge = screen.cursor();
    assert!(edge.wrap_pending);
    screen.set_tab_stop();
    screen.set_scroll_region(1..4).unwrap();
    screen.set_origin_mode(true);
    screen.set_auto_wrap(false);
    screen.set_insert_mode(true);
    let appearance = CursorAppearance {
        visible: false,
        blinking: false,
        shape: CursorShape::Bar,
    };
    screen.set_cursor_appearance(appearance);
    let live = screen.cursor();
    screen.select_buffer(Primary);
    assert_eq!(screen.cursor(), live);
    assert!(screen.origin_mode() && !screen.auto_wrap() && screen.insert_mode());
    assert_eq!(screen.scroll_region(), 1..4);
    assert_eq!(screen.is_tab_stop(3), Some(true));
    screen.restore_cursor();
    assert_eq!(screen.cursor(), cursor);
    assert!(!screen.origin_mode() && screen.auto_wrap() && screen.insert_mode());
    assert_eq!(screen.cursor_appearance(), appearance);
    assert_eq!(super::snapshot(screen.grid()), primary);
    screen.select_buffer(Alternate);
    screen.restore_cursor();
    assert_eq!(screen.cursor(), edge);
    assert_eq!(super::snapshot(screen.grid()), alternate);
    screen.resize(3, 3, StyleId(7)).unwrap();
    for kind in [Primary, Alternate] {
        let grid = screen.buffer(kind);
        assert_eq!((grid.columns(), grid.rows()), (3, 3));
    }
    assert_eq!(
        screen.grid().row(1).unwrap().text(2),
        Some(CellText::Scalar(' '))
    );
    assert!(screen.grid().row(1).unwrap().cells()[2].is_erased());
    screen.restore_cursor();
    assert_eq!(screen.cursor().column, 2);
    assert!(!screen.cursor().wrap_pending);
    screen.select_buffer(Primary);
    screen.restore_cursor();
    assert_eq!(screen.cursor(), cursor);
    screen.select_buffer(Alternate);
    screen.reset(StyleId(9));
    assert_eq!(screen.active_buffer(), Primary);
    for kind in [Primary, Alternate] {
        for row in screen.buffer(kind).iter_rows() {
            assert_eq!(row.wrap_end(), None);
            assert!(
                row.cells()
                    .iter()
                    .all(|cell| cell.is_erased() && cell.style() == StyleId(9))
            );
        }
    }
    screen.select_buffer(Alternate);
    screen.restore_cursor();
    assert_eq!(screen.cursor(), crate::Cursor::default());
}

#[test]
fn alternate_modes_preserve_primary_saves_and_validate_whole_lists_across_splits() {
    let input = b"\x1b[31m\x1b(0q\x1b[2;3H\x1b[?1049h\x1b(B\x1b[34mA\x1b[?1048h\x1b[32m\x1b(0\x1b[?1049hqx\x1b[?1049lq\x1b[?47h\x1b[?1048lq\x1b[?47l\x1b[?1047h\x1b[?1047l\x1b[?47;1048;999h\x1b[?1049lq\x1b[6n";
    for split in 0..=input.len() {
        let mut processor = Processor::default();
        let mut screen = Screen::new(8, 4).unwrap();
        processor
            .advance(&input[..split], &mut screen, &mut text)
            .unwrap();
        processor
            .advance(&input[split..], &mut screen, &mut text)
            .unwrap();
        assert_eq!(screen.active_buffer(), Primary);
        assert_eq!((screen.cursor().row, screen.cursor().column), (1, 3));
        assert_eq!(
            screen.grid().row(0).unwrap().text(0),
            Some(CellText::Scalar('─'))
        );
        assert_eq!(
            screen.grid().row(1).unwrap().text(2),
            Some(CellText::Scalar('─'))
        );
        let red = processor.styles().get(processor.style_id()).unwrap();
        assert_eq!(red.foreground, Color::Indexed(1));
        assert_eq!(processor.reply_bytes(), b"\x1b[2;4R");
        assert_eq!(
            (processor.parse_errors(), processor.ignored_sequences()),
            (0, 1)
        );
        for row in screen.buffer(Alternate).iter_rows() {
            for cell in row.cells() {
                assert!(cell.is_erased());
                assert_eq!(
                    processor.styles().get(cell.style()).unwrap().foreground,
                    Color::Indexed(2)
                );
            }
        }
        processor.collect_styles(&screen, []).unwrap();
        assert_eq!(processor.styles().style_count(), 3);
        let primary = super::snapshot(screen.buffer(Primary));
        let alternate = super::snapshot(screen.buffer(Alternate));
        let cursor = screen.cursor();
        let style = processor.style_id();
        processor
            .advance(b"\x1b[?47;1048;999h", &mut screen, &mut text)
            .unwrap();
        assert_eq!(screen.active_buffer(), Primary);
        assert_eq!(screen.cursor(), cursor);
        assert_eq!(processor.style_id(), style);
        assert_eq!(super::snapshot(screen.buffer(Primary)), primary);
        assert_eq!(super::snapshot(screen.buffer(Alternate)), alternate);
        processor.advance(b"\x1b8", &mut screen, &mut text).unwrap();
        assert_eq!((screen.cursor().row, screen.cursor().column), (1, 2));
        assert!(processor.consume_reply(6));
        processor
            .advance(b"\x1b[?47h\x1bc", &mut screen, &mut text)
            .unwrap();
        assert_eq!(screen.active_buffer(), Primary);
        processor.collect_styles(&screen, []).unwrap();
        assert_eq!(processor.styles().style_count(), 1);
    }
}

#[test]
fn collection_traces_both_buffer_save_slots_even_without_styled_cells() {
    let mut processor = Processor::new(4).unwrap();
    let mut screen = Screen::new(8, 4).unwrap();
    processor
        .advance(
            b"\x1b[31m\x1b7\x1b[0m\x1b[?47h\x1b[34m\x1b7\x1b[0m\x1b[32mG\x1b[0m\x1b[2J",
            &mut screen,
            &mut text,
        )
        .unwrap();
    assert_eq!(processor.collect_styles(&screen, []).unwrap(), 1);
    assert_eq!(processor.styles().style_count(), 3);
    processor
        .advance(b"\x1b[?47l\x1b8p", &mut screen, &mut text)
        .unwrap();
    let primary = screen.grid().row(0).unwrap().cells()[0].style();
    assert_eq!(
        processor.styles().get(primary).unwrap().foreground,
        Color::Indexed(1)
    );
    processor
        .advance(b"\x1b[?47h\x1b8a", &mut screen, &mut text)
        .unwrap();
    let alternate = screen.grid().row(0).unwrap().cells()[0].style();
    assert_eq!(
        processor.styles().get(alternate).unwrap().foreground,
        Color::Indexed(4)
    );
    processor.reset(&mut screen, &mut text);
    processor.collect_styles(&screen, []).unwrap();
    assert_eq!(processor.styles().style_count(), 1);
}
