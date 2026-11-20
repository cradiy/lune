use std::io::Cursor;
use std::num::NonZeroU32;

use lune_terminal::{
    CellText, Color, MouseButton, PaletteEntry, PixelSize, Rgb, WorkingDirectoryChange,
};

use super::*;

struct Transport {
    source: Cursor<&'static [u8]>,
    written: Vec<u8>,
    write_budget: usize,
    hung_up: bool,
}

impl Read for Transport {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self.source.read(bytes)? {
            0 if !self.hung_up => Err(io::ErrorKind::WouldBlock.into()),
            count => Ok(count),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.hung_up {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let count = bytes.len().min(self.write_budget).min(2);
        if count == 0 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.written.extend_from_slice(&bytes[..count]);
        self.write_budget -= count;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn session(source: &'static [u8]) -> Session<Transport> {
    let color = Rgb::default();
    let palette = Palette::new([color; 256], color, color, color);
    Session::new(
        Transport {
            source: Cursor::new(source),
            written: Vec::new(),
            write_budget: 1,
            hung_up: false,
        },
        PtySize::new(3, 8),
        Screen::new(8, 3).unwrap(),
        Processor::new(MAX_STYLES, palette).unwrap(),
    )
}

fn pixels(width: u32, height: u32) -> PixelSize {
    PixelSize {
        width: NonZeroU32::new(width).unwrap(),
        height: NonZeroU32::new(height).unwrap(),
    }
}

#[test]
fn short_writes_preserve_frames_and_resume_a_reply_at_end_of_input() {
    let mut session = session(b"abc\x1b[6n\x1b[2;3H\x1b[6n");
    session.queue_input(b"key").unwrap();
    session.pump().unwrap();
    assert_eq!(session.transport.written, b"k");
    assert!(!session.wants_read());
    assert!(session.wants_write());
    assert_eq!(
        session.queue_input(b"next").unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );

    session.transport.write_budget = 3;
    session.pump().unwrap();
    assert_eq!(session.transport.written, b"key\x1b");
    assert!(!session.wants_read());

    session.transport.write_budget = usize::MAX;
    session.pump().unwrap();
    assert!(session.wants_read());
    assert!(!session.wants_write());
    session.queue_input(b"next").unwrap();
    session.pump().unwrap();
    assert_eq!(session.transport.written, b"key\x1b[1;4R\x1b[2;3Rnext");
}

#[test]
fn hangup_drains_child_output_and_finalizes_an_incomplete_scalar() {
    let mut session = session(b"\x1b[6n\x1b[6nZ\xc2");
    session.queue_input(b"key").unwrap();
    session.pump().unwrap();
    assert!(!session.wants_read());
    session.transport.hung_up = true;

    session.pump().unwrap();
    assert!(session.is_finished());
    session.pump().unwrap();
    assert_eq!(session.transport.written, b"k");
    let row = session.screen().grid().row(0).unwrap();
    assert_eq!(row.text(0), Some(CellText::Scalar('Z')));
    assert_eq!(row.text(1), Some(CellText::Scalar('\u{fffd}')));
    assert_eq!(session.screen().cursor().column, 2);
    assert!(!session.wants_read());
    assert!(!session.wants_write());
    assert_eq!(
        session.queue_input(b"next").unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
}

#[test]
fn session_notifications_coalesce_and_distinguish_clearing_metadata() {
    let mut session = session(
        b"\x1b]2;first\x07\x1b]2;latest\x07\x1b]7;file:///sample\x07\x1b]11;#123456\x07\x07\x07",
    );
    session.pump().unwrap();
    let changes = session.take_notifications();
    assert_eq!(changes.window_title, Some("latest"));
    assert_eq!(
        changes.working_directory,
        WorkingDirectoryChange::Set("file:///sample")
    );
    let background = Rgb {
        red: 0x12,
        green: 0x34,
        blue: 0x56,
    };
    assert_eq!(
        changes.palette.unwrap().color(PaletteEntry::Background),
        background
    );
    assert!(changes.bell);

    let changes = session.take_notifications();
    assert!(changes.window_title.is_none() && changes.palette.is_none() && !changes.bell);
    assert_eq!(changes.working_directory, WorkingDirectoryChange::Unchanged);

    session.transport.source = Cursor::new(b"\x1bc");
    session.pump().unwrap();
    let changes = session.take_notifications();
    assert_eq!(changes.window_title, Some(""));
    assert_eq!(changes.working_directory, WorkingDirectoryChange::Cleared);
    assert_eq!(
        changes.palette.unwrap().color(PaletteEntry::Background),
        Rgb::default()
    );
    assert!(!changes.bell);
}

#[test]
fn combined_theme_switch_preserves_overrides_and_reports_one_final_scheme() {
    let black = Rgb::default();
    let white = Rgb {
        red: 255,
        green: 255,
        blue: 255,
    };
    let dark = Palette::new([black; 256], white, black, white);
    let light = Palette::new([white; 256], black, white, black);
    let mut session = session(b"\x1b[?2031h\x1b]11;#010203\x07");
    session.set_theme(&dark, Some(ColorScheme::Dark));
    session.pump().unwrap();
    assert!(!session.wants_write());
    session.take_notifications();

    session.set_theme(&light, Some(ColorScheme::Light));
    let palette = session.take_notifications().palette.unwrap();
    assert_eq!(palette.color(PaletteEntry::Foreground), black);
    assert_eq!(
        palette.color(PaletteEntry::Background),
        Rgb {
            red: 1,
            green: 2,
            blue: 3
        }
    );
    assert_eq!(palette.base_color(PaletteEntry::Background), white);
    session.pump().unwrap();
    assert_eq!(session.transport.written, b"\x1b");
    session.set_theme(&light, Some(ColorScheme::Light));
    session.transport.write_budget = usize::MAX;
    session.pump().unwrap();
    assert_eq!(session.transport.written, b"\x1b[?997;2n");

    session.transport.source = Cursor::new(b"\x1b[?996n");
    session.transport.write_budget = 1;
    session.pump().unwrap();
    session.set_theme(&dark, Some(ColorScheme::Dark));
    session.transport.write_budget = usize::MAX;
    session.pump().unwrap();
    assert_eq!(
        session.transport.written,
        b"\x1b[?997;2n\x1b[?997;2n\x1b[?997;1n"
    );
}

#[test]
fn synchronized_output_replies_while_held_and_releases_at_the_original_deadline() {
    let start = Instant::now();
    let mut session = session(b"\x1b[?20");
    session
        .pump_with_clock(|| panic!("inactive presentation read the clock"))
        .unwrap();
    assert!(session.presentation_screen().is_some());

    session.transport.source = Cursor::new(b"26hframe\x1b[?2026$p");
    session.transport.write_budget = usize::MAX;
    session.pump_with_clock(|| start).unwrap();
    let deadline = start + SYNCHRONIZED_OUTPUT_TIMEOUT;
    assert_eq!(session.presentation_deadline(), Some(deadline));
    assert!(session.presentation_screen().is_none());
    assert_eq!(session.transport.written, b"\x1b[?2026;1$y");

    session.transport.source = Cursor::new(b"\x1b[?2026h\x1b[?2026l\x1b[?2026h");
    session
        .pump_with_clock(|| deadline - Duration::from_millis(1))
        .unwrap();
    assert_eq!(session.presentation_deadline(), Some(deadline));

    session.transport.source = Cursor::new(b"\x1b[?2026$p");
    session.pump_with_clock(|| deadline).unwrap();
    assert!(session.presentation_deadline().is_none());
    let screen = session.presentation_screen().unwrap();
    assert_eq!(
        screen.grid().row(0).unwrap().text(4),
        Some(CellText::Scalar('e'))
    );
    assert_eq!(session.transport.source.position(), 0);
    session.pump_with_clock(|| deadline).unwrap();
    assert_eq!(session.transport.written, b"\x1b[?2026;1$y\x1b[?2026;2$y");
}

#[test]
fn synchronized_output_releases_on_explicit_end_and_pty_eof() {
    let now = Instant::now();
    let mut session = session(b"\x1b[?2026ha");
    session.pump_with_clock(|| now).unwrap();
    assert!(session.presentation_screen().is_none());
    session.transport.source = Cursor::new(b"b\x1b[?2026l");
    session.pump_with_clock(|| now).unwrap();
    assert!(session.presentation_deadline().is_none());
    let screen = session.presentation_screen().unwrap();
    assert_eq!(
        screen.grid().row(0).unwrap().text(1),
        Some(CellText::Scalar('b'))
    );

    session.transport.source = Cursor::new(b"\x1b[?2026h\xc2");
    session.transport.hung_up = true;
    session.pump_with_clock(|| now).unwrap();
    assert!(session.presentation_screen().is_none());
    session.pump_with_clock(|| now).unwrap();
    assert!(session.is_finished());
    assert!(!session.processor().synchronized_output());
    let screen = session.presentation_screen().unwrap();
    assert_eq!(
        screen.grid().row(0).unwrap().text(2),
        Some(CellText::Scalar('\u{fffd}'))
    );
}

#[test]
fn presentation_frame_keeps_whole_units_and_matches_their_rendition() {
    let now = Instant::now();
    let mut session = session(
        "\x1b[?2026h\x1b[31mA\u{301}水\x1b[32me\u{301} \x1b[?5h\x1b[?25l\x1b]11;#010203\x07"
            .as_bytes(),
    );
    session.pump_with_clock(|| now).unwrap();
    assert!(session.presentation_frame().is_none());
    session.transport.source = Cursor::new(b"\x1b[?2026l");
    session.pump_with_clock(|| now).unwrap();

    let frame = session.presentation_frame().unwrap();
    assert!(frame.reverse_video && !frame.cursor_appearance.visible);
    assert_eq!(frame.viewport.cursor().unwrap().column, 5);
    assert_eq!(
        frame.palette.color(PaletteEntry::Background),
        Rgb {
            red: 1,
            green: 2,
            blue: 3
        }
    );
    let row = frame.viewport.row(0).unwrap();
    let mut clipped = row.display_units(2..4);
    let first = clipped.next().unwrap();
    assert_eq!(
        (first.column, first.cell.width(), first.text),
        (1, 2, CellText::Scalar('水'))
    );
    assert_eq!(
        frame.styles.get(first.cell.style()).unwrap().foreground,
        Color::Indexed(1)
    );
    let second = clipped.next().unwrap();
    assert_eq!(
        (second.column, second.text),
        (3, CellText::Extended("e\u{301}"))
    );
    assert_eq!(
        frame.styles.get(second.cell.style()).unwrap().foreground,
        Color::Indexed(2)
    );
    assert!(clipped.next().is_none());
    assert_eq!(row.display_units(0..2).last().unwrap(), first);

    let mut trailing = row.display_units(4..frame.geometry.pty_size().columns.into());
    assert!(!trailing.next().unwrap().cell.is_erased());
    assert!(trailing.all(|unit| unit.cell.is_erased()));
}

#[test]
fn presentation_runs_resolve_colors_after_screen_and_cell_inversion() {
    let mut session = session(
        "\x1b]4;1;#123456\x07\x1b[31;4mA水\x1b[7;48;2;20;21;22;58;2;4;5;6mB\x1b[0;7mC\x1b[0mD\x1b[?5h"
            .as_bytes(),
    );
    let foreground = Rgb {
        red: 110,
        green: 111,
        blue: 112,
    };
    let background = Rgb {
        red: 10,
        green: 11,
        blue: 12,
    };
    let cursor = Rgb::default();
    session.set_theme(
        &Palette::new([cursor; 256], foreground, background, cursor),
        None,
    );
    session.pump().unwrap();

    let frame = session.presentation_frame().unwrap();
    let indexed = Rgb {
        red: 0x12,
        green: 0x34,
        blue: 0x56,
    };
    let explicit_background = Rgb {
        red: 20,
        green: 21,
        blue: 22,
    };
    let explicit_underline = Rgb {
        red: 4,
        green: 5,
        blue: 6,
    };
    let mut runs = frame.row_runs(0, 2..usize::MAX).unwrap();
    for (columns, fg, bg, underline) in [
        (2..3, indexed, foreground, indexed),
        (3..4, explicit_background, indexed, explicit_underline),
        (4..5, foreground, background, foreground),
        (5..8, background, foreground, background),
    ] {
        let run = runs.next().unwrap().unwrap();
        let style = run.style;
        assert_eq!(run.columns(), columns);
        assert_eq!(
            (style.foreground, style.background, style.underline_color),
            (fg, bg, underline)
        );
    }
    assert!(runs.next().is_none());
    let clip = frame.row_runs(0, 2..3).unwrap().next().unwrap().unwrap();
    let wide = clip.units().next().unwrap();
    assert_eq!((wide.column, wide.text), (1, CellText::Scalar('水')));
    assert_eq!(clip.style, frame.resolve_style(wide.cell.style()).unwrap());

    let unrelated_styles = StyleTable::new(1).unwrap();
    let detached = PresentationFrame {
        styles: &unrelated_styles,
        ..frame
    };
    assert!(matches!(
        detached.row_runs(0, 2..3).unwrap().next(),
        Some(Err(StyleError::InvalidId(id))) if id == wide.cell.style()
    ));
}

#[test]
fn presentation_backgrounds_merge_by_resolved_color_in_pixel_coordinates() {
    let mut session =
        session("\x1b[2;1H\x1b[31mA\x1b[1mB\x1b[4m水\x1b[0;7mC\x1b[0mD\x1b[?5h".as_bytes());
    let foreground = Rgb {
        red: 240,
        green: 220,
        blue: 200,
    };
    let background = Rgb {
        red: 10,
        green: 20,
        blue: 30,
    };
    let black = Rgb::default();
    session.set_theme(
        &Palette::new([black; 256], foreground, background, black),
        None,
    );
    session.pump().unwrap();
    assert!(
        session
            .presentation_frame()
            .unwrap()
            .row_backgrounds(1, 1..8)
            .is_none()
    );
    session.geometry = GridGeometry::fit(pixels(67, 51), pixels(8, 16)).unwrap();

    let frame = session.presentation_frame().unwrap();
    let mut rects = frame.row_backgrounds(1, 1..usize::MAX).unwrap();
    let rect = |x, width, color| BackgroundRect {
        rect: PixelRect {
            origin: PixelPosition { x, y: 16 },
            size: pixels(width, 16),
        },
        color,
    };
    assert_eq!(rects.next().unwrap().unwrap(), rect(8, 24, foreground));
    assert_eq!(rects.next().unwrap().unwrap(), rect(32, 8, background));
    assert_eq!(rects.next().unwrap().unwrap(), rect(40, 24, foreground));
    assert!(rects.next().is_none());
    assert_eq!(
        frame
            .row_backgrounds(1, 3..4)
            .unwrap()
            .next()
            .unwrap()
            .unwrap(),
        rect(24, 8, foreground)
    );
    assert!(
        frame
            .row_backgrounds(1, 8..usize::MAX)
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn presentation_runs_clip_and_pad_to_the_current_width() {
    let mut session = session("\x1b]4;1;#010203\x07\x1b[41mabcdef水\r\nb\r\nc\r\nd".as_bytes());
    session
        .set_history_limits(HistoryLimits {
            max_rows: 1,
            max_bytes: 1024,
        })
        .unwrap();
    session.pump().unwrap();
    assert!(session.scroll_history(1));

    session.screen.resize(7, 3, StyleId::default()).unwrap();
    session.geometry = GridGeometry::from_pty_size(PtySize::new(3, 7));
    let frame = session.presentation_frame().unwrap();
    let mut runs = frame.row_runs(0, 6..usize::MAX).unwrap();
    let clipped = runs.next().unwrap().unwrap();
    assert_eq!(clipped.columns(), 6..7);
    let wide = clipped.units().next().unwrap();
    assert_eq!(
        (wide.column, wide.cell.width(), wide.text),
        (6, 2, CellText::Scalar('水'))
    );
    assert!(runs.next().is_none());
    drop(runs);

    session.screen.resize(10, 3, StyleId::default()).unwrap();
    session.geometry = GridGeometry::fit(pixels(80, 48), pixels(8, 16)).unwrap();
    let frame = session.presentation_frame().unwrap();
    let mut runs = frame.row_runs(0, 7..usize::MAX).unwrap();
    let tail = runs.next().unwrap().unwrap();
    assert_eq!(tail.columns(), 7..8);
    assert_eq!(tail.style.style.background, Color::Indexed(1));
    assert_eq!(tail.units().next().unwrap().column, 6);
    let padding = runs.next().unwrap().unwrap();
    assert_eq!(padding.columns(), 8..10);
    assert_eq!(
        padding.style,
        frame.resolve_style(StyleId::default()).unwrap()
    );
    assert!(padding.units().next().is_none());
    assert!(runs.next().is_none());
    assert_eq!(frame.row_runs(0, 9..10).unwrap().count(), 1);
    assert_eq!(frame.row_runs(0, 10..10).unwrap().count(), 0);
    assert!(frame.row_runs(3, 0..10).is_none());

    let mut rects = frame.row_backgrounds(0, 7..usize::MAX).unwrap();
    let retained = rects.next().unwrap().unwrap();
    assert_eq!(
        retained.rect,
        PixelRect {
            origin: PixelPosition { x: 56, y: 0 },
            size: pixels(8, 16)
        }
    );
    assert_eq!(
        retained.color,
        Rgb {
            red: 1,
            green: 2,
            blue: 3
        }
    );
    let padding = rects.next().unwrap().unwrap();
    assert_eq!(
        padding.rect,
        PixelRect {
            origin: PixelPosition { x: 64, y: 0 },
            size: pixels(16, 16)
        }
    );
    assert_eq!(padding.color, Rgb::default());
    assert!(rects.next().is_none());
}

#[test]
fn presentation_damage_follows_visible_row_positions() {
    let mut session = session(b"a\r\nb\r\nc");
    session
        .set_history_limits(HistoryLimits {
            max_rows: 2,
            max_bytes: 1024,
        })
        .unwrap();
    session.pump().unwrap();
    let mut tracker = PresentationTracker::default();
    let now = Instant::now();
    let frame = session.presentation_frame().unwrap();
    assert_eq!(tracker.update(&frame, now).unwrap().rows, RowDamage::All);
    assert_eq!(
        tracker.update(&frame, now).unwrap().rows,
        RowDamage::Rows(&[])
    );

    session.transport.source = Cursor::new(b"\x1b[2;1HB");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    assert_eq!(
        tracker.update(&frame, now).unwrap().rows,
        RowDamage::Rows(&[1])
    );
    let retained_row = frame.viewport.row(1).unwrap().revision();

    session.transport.source = Cursor::new(b"\x1b[3;1H\r\nd");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    assert_eq!(frame.viewport.row(0).unwrap().revision(), retained_row);
    assert_eq!(
        tracker.update(&frame, now).unwrap().rows,
        RowDamage::Rows(&[0, 1, 2])
    );

    assert!(session.scroll_history(1));
    let frame = session.presentation_frame().unwrap();
    let damage = tracker.update(&frame, now).unwrap();
    assert_eq!(damage.rows, RowDamage::Rows(&[0, 1, 2]));
    assert_eq!(damage.cursor.unwrap().after, None);

    session.screen.resize(10, 4, StyleId::default()).unwrap();
    session.geometry = GridGeometry::from_pty_size(PtySize::new(4, 10));
    let frame = session.presentation_frame().unwrap();
    assert_eq!(tracker.update(&frame, now).unwrap().rows, RowDamage::All);
    assert_eq!(
        tracker.update(&frame, now).unwrap().rows,
        RowDamage::Rows(&[])
    );
    tracker.invalidate();
    assert_eq!(tracker.update(&frame, now).unwrap().rows, RowDamage::All);
}

#[test]
fn presentation_damage_separates_cursor_and_resolved_text_colors() {
    let mut session = session(b"");
    let mut tracker = PresentationTracker::default();
    let now = Instant::now();
    let damage = tracker
        .update(&session.presentation_frame().unwrap(), now)
        .unwrap();
    let initial = damage.cursor.unwrap().after.unwrap();

    session.transport.source = Cursor::new(b"\x1b[2;3H\x1b[6 q\x1b]12;#010203\x07");
    session.pump().unwrap();
    let damage = tracker
        .update(&session.presentation_frame().unwrap(), now)
        .unwrap();
    assert_eq!(damage.rows, RowDamage::Rows(&[]));
    let cursor = damage.cursor.unwrap();
    assert_eq!(cursor.before, Some(initial));
    let moved = cursor.after.unwrap();
    assert_eq!(moved.position, CellPosition { row: 1, column: 2 });
    assert_eq!(moved.appearance.shape, lune_terminal::CursorShape::Bar);
    let color = Rgb {
        red: 1,
        green: 2,
        blue: 3,
    };
    assert_eq!(moved.color, color);

    session.transport.source = Cursor::new(b"\x1b[?25l\x1b]11;#010203\x07");
    session.pump().unwrap();
    let damage = tracker
        .update(&session.presentation_frame().unwrap(), now)
        .unwrap();
    assert_eq!(damage.rows, RowDamage::All);
    assert_eq!(
        damage.cursor.unwrap(),
        CursorChange {
            before: Some(moved),
            after: None
        }
    );

    let base = Rgb::default();
    session.set_theme(&Palette::new([base; 256], base, color, base), None);
    let damage = tracker
        .update(&session.presentation_frame().unwrap(), now)
        .unwrap();
    assert_eq!(damage.rows, RowDamage::Rows(&[]));
    assert_eq!(damage.cursor, None);

    session.transport.source = Cursor::new(b"\x1b[?5h");
    session.pump().unwrap();
    let damage = tracker
        .update(&session.presentation_frame().unwrap(), now)
        .unwrap();
    assert_eq!(damage.rows, RowDamage::All);
    assert_eq!(damage.cursor, None);
}

#[test]
fn cursor_blink_keeps_its_cadence_and_only_changes_the_overlay() {
    let session = session(b"");
    let frame = session.presentation_frame().unwrap();
    let mut tracker = PresentationTracker::default();
    let start = Instant::now();
    let phase = Duration::from_millis(40);
    tracker.set_cursor_blink_interval(phase, start);
    let cursor = tracker.update(&frame, start).unwrap().cursor.unwrap().after;
    assert!(cursor.is_some());
    assert_eq!(tracker.cursor_deadline(), Some(start + phase));
    assert_eq!(tracker.tick_cursor(start), None);

    let hidden = tracker.tick_cursor(start + phase).unwrap();
    assert_eq!((hidden.before, hidden.after), (cursor, None));
    assert_eq!(
        tracker.tick_cursor(start + Duration::from_millis(125)),
        None
    );
    assert_eq!(tracker.cursor_deadline(), Some(start + phase * 4));

    let update = tracker
        .update(&frame, start + Duration::from_millis(161))
        .unwrap();
    assert_eq!(update.rows, RowDamage::Rows(&[]));
    assert_eq!(update.cursor.unwrap().after, cursor);
    assert_eq!(tracker.cursor_deadline(), Some(start + phase * 5));

    tracker.tick_cursor(start + phase * 5).unwrap();
    let activity = start + Duration::from_millis(220);
    let shown = tracker.restart_cursor_blink(activity).unwrap();
    assert_eq!((shown.before, shown.after), (None, cursor));
    assert_eq!(tracker.cursor_deadline(), Some(activity + phase));

    tracker.tick_cursor(activity + phase).unwrap();
    let shown = tracker
        .set_cursor_blink_interval(Duration::ZERO, activity + phase)
        .unwrap();
    assert_eq!(shown.after, cursor);
    assert_eq!(tracker.cursor_deadline(), None);
    assert_eq!(tracker.displayed_cursor(), cursor);
}

#[test]
fn synchronized_output_blinks_only_the_last_presented_cursor() {
    let mut session = session(b"\x1b[?2026h\x1b[2;3H");
    let mut tracker = PresentationTracker::default();
    let start = Instant::now();
    let phase = Duration::from_millis(30);
    tracker.set_cursor_blink_interval(phase, start);
    let initial = session.presentation_frame().unwrap();
    tracker.update(&initial, start).unwrap();
    let original = tracker.displayed_cursor().unwrap();

    session.pump_with_clock(|| start).unwrap();
    assert!(session.presentation_frame().is_none());
    let hidden = tracker.tick_cursor(start + phase).unwrap();
    assert_eq!((hidden.before, hidden.after), (Some(original), None));
    assert_eq!(
        tracker.tick_cursor(start + phase * 2).unwrap().after,
        Some(original)
    );

    let release = start + Duration::from_millis(70);
    session.transport.source = Cursor::new(b"\x1b[?2026l");
    session.pump_with_clock(|| release).unwrap();
    let frame = session.presentation_frame().unwrap();
    let change = tracker.update(&frame, release).unwrap().cursor.unwrap();
    assert_eq!(change.before, Some(original));
    assert_eq!(
        change.after.unwrap().position,
        CellPosition { row: 1, column: 2 }
    );
    assert_eq!(tracker.cursor_deadline(), Some(release + phase));

    session.transport.source = Cursor::new(b"\x1b[?12l");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    tracker.update(&frame, release).unwrap();
    assert!(tracker.displayed_cursor().is_some());
    assert_eq!(tracker.cursor_deadline(), None);
}

#[test]
fn waiting_on_a_real_pty_respects_idle_deadlines_and_drains_hangup() {
    let mut child = Command::new("sh");
    child.args([
        "-c",
        r#"
        stty -echo || exit 71
        printf 'ready'
        IFS= read -r value && test "$value" = begin || exit 72
        printf '\033[?2026hA'
        IFS= read -r value && test "$value" = finish || exit 73
        printf '\033[3;1Htail'
        "#,
    ]);
    let color = Rgb::default();
    let mut session = Session::spawn(
        child,
        PtySize::new(4, 20),
        Palette::new([color; 256], color, color, color),
    )
    .unwrap();
    let limit = Instant::now() + Duration::from_secs(5);
    wait_until(&mut session, limit, "initial text", |session| {
        session.screen().grid().row(0).unwrap().text(4) == Some(CellText::Scalar('y'))
    });
    assert_eq!(
        session.wait_and_pump(Some(Duration::ZERO)).unwrap(),
        WaitOutcome::TimedOut
    );
    let timeout = Duration::from_millis(10);
    let start = Instant::now();
    assert_eq!(
        session.wait_and_pump(Some(timeout)).unwrap(),
        WaitOutcome::TimedOut
    );
    assert!(start.elapsed() >= timeout);

    session.queue_input(b"begin\n").unwrap();
    wait_until(&mut session, limit, "synchronized output", |session| {
        session.presentation_deadline().is_some()
    });
    assert!(session.presentation_screen().is_none());
    let deadline = session.presentation_deadline().unwrap();
    assert_eq!(
        session.wait_and_pump(Some(Duration::from_secs(5))).unwrap(),
        WaitOutcome::Pumped
    );
    assert!(Instant::now() >= deadline);
    let screen = session.presentation_screen().unwrap();
    assert_eq!(
        screen.grid().row(0).unwrap().text(5),
        Some(CellText::Scalar('A'))
    );

    session.queue_input(b"finish\n").unwrap();
    let mut exit = None;
    loop {
        let remaining = limit.saturating_duration_since(Instant::now());
        match session.wait_and_pump(Some(remaining)).unwrap() {
            WaitOutcome::Pumped => {}
            WaitOutcome::ChildExited(status) => assert!(exit.replace(status).is_none()),
            WaitOutcome::Finished => break,
            WaitOutcome::TimedOut => panic!("timed out waiting for session completion"),
        }
    }
    assert!(exit.unwrap().success());
    assert_eq!(session.wait_and_pump(None).unwrap(), WaitOutcome::Finished);
    assert_eq!(
        session.screen().grid().row(2).unwrap().text(3),
        Some(CellText::Scalar('l'))
    );
}

#[test]
fn child_exit_is_reported_once_while_a_descendant_keeps_the_pty_open() {
    let mut child = Command::new("sh");
    child.args([
        "-c",
        r#"
        stty -echo || exit 71
        trap '' HUP
        exec 3<&0
        (IFS= read -r value && test "$value" = drain && printf tail) <&3 &
        exit 23
        "#,
    ]);
    let color = Rgb::default();
    let mut session = Session::spawn(
        child,
        PtySize::new(4, 20),
        Palette::new([color; 256], color, color, color),
    )
    .unwrap();
    let timeout = Some(Duration::from_secs(5));
    let WaitOutcome::ChildExited(status) = session.wait_and_pump(timeout).unwrap() else {
        panic!("direct child exit was not reported while the PTY remained open");
    };
    assert_eq!(status.code(), Some(23));
    assert!(!session.is_finished());
    assert_eq!(session.try_wait().unwrap(), Some(status));
    assert_eq!(
        session.wait_and_pump(Some(Duration::ZERO)).unwrap(),
        WaitOutcome::TimedOut
    );

    session.queue_input(b"drain\n").unwrap();
    wait_until(
        &mut session,
        Instant::now() + Duration::from_secs(5),
        "descendant output",
        Session::is_finished,
    );
    assert_eq!(session.wait_and_pump(None).unwrap(), WaitOutcome::Finished);
    assert_eq!(
        session.screen().grid().row(0).unwrap().text(3),
        Some(CellText::Scalar('l'))
    );
}

#[test]
fn external_poll_dispatches_host_io_pty_exit_and_presentation_deadline() {
    use std::os::fd::AsRawFd;

    let mut child = Command::new("sh");
    child.args([
        "-c",
        r#"
        stty -echo || exit 71
        printf '\033[?2026h\033[31mA'
        IFS= read -r value && test "$value" = finish || exit 72
        printf '\033[2;1Htail'
        exit 23
        "#,
    ]);
    let color = Rgb::default();
    let accent = Rgb {
        red: 42,
        green: 21,
        blue: 7,
    };
    let mut indexed = [color; 256];
    indexed[1] = accent;
    let mut session = Session::spawn(
        child,
        PtySize::new(3, 8),
        Palette::new(indexed, color, color, color),
    )
    .unwrap();
    let (mut host, mut peer) = io::pipe().unwrap();
    peer.write_all(b"x").unwrap();
    let mut tracker = PresentationTracker::default();
    let now = Instant::now();
    let initial = session.presentation_frame().unwrap();
    assert_eq!(tracker.update(&initial, now).unwrap().rows, RowDamage::All);
    let limit = Instant::now() + Duration::from_secs(5);
    let mut host_ready = false;
    let mut held = false;
    let mut sent = false;
    let mut exit = None;
    loop {
        let interests = session.interests();
        let mut descriptors = [
            libc::pollfd {
                fd: interests.pty.map_or(-1, |pty| pty.fd.as_raw_fd()),
                events: interests.pty.map_or(0, |pty| {
                    (if pty.read { libc::POLLIN } else { 0 })
                        | (if pty.write { libc::POLLOUT } else { 0 })
                }),
                revents: 0,
            },
            libc::pollfd {
                fd: interests.child_exit.map_or(-1, |fd| fd.as_raw_fd()),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: host.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let now = Instant::now();
        assert!(now < limit, "external polling did not complete");
        if !interests.immediate {
            let deadline = interests
                .presentation_deadline
                .map_or(limit, |deadline| deadline.min(limit));
            let timeout = deadline
                .saturating_duration_since(now)
                .as_nanos()
                .div_ceil(1_000_000);
            // SAFETY: The three entries are initialized and their live descriptors remain owned.
            if unsafe {
                libc::poll(
                    descriptors.as_mut_ptr(),
                    descriptors.len() as libc::nfds_t,
                    timeout as i32,
                )
            } < 0
            {
                assert_eq!(
                    io::Error::last_os_error().kind(),
                    io::ErrorKind::Interrupted
                );
                continue;
            }
        }
        if descriptors[2].revents & libc::POLLIN != 0 {
            let mut byte = [0];
            host.read_exact(&mut byte).unwrap();
            assert_eq!(&byte, b"x");
            host_ready = true;
        }
        let ready = SessionReady {
            pty: descriptors[0].revents != 0,
            child_exit: descriptors[1].revents != 0,
        };
        match session.dispatch_ready(ready).unwrap() {
            Some(WaitOutcome::ChildExited(status)) => {
                assert!(exit.replace(status).is_none());
                assert!(session.interests().child_exit.is_none());
            }
            Some(WaitOutcome::Finished) => break,
            Some(WaitOutcome::TimedOut) => panic!("dispatch waited"),
            Some(WaitOutcome::Pumped) | None => {}
        }
        held |= session.presentation_deadline().is_some();
        if held
            && !sent
            && let Some(frame) = session.presentation_frame()
        {
            let row = frame.viewport.row(0).unwrap();
            assert_eq!(
                tracker.update(&frame, now).unwrap().rows,
                RowDamage::Rows(&[0])
            );
            let first = row
                .display_units(0..frame.geometry.pty_size().columns.into())
                .next()
                .unwrap();
            assert_eq!((first.column, first.text), (0, CellText::Scalar('A')));
            assert_eq!(
                frame.resolve_style(first.cell.style()).unwrap().foreground,
                accent
            );
            session.queue_input(b"finish\n").unwrap();
            assert!(session.interests().pty.unwrap().write);
            sent = true;
        }
    }
    assert!(host_ready && held && sent);
    assert_eq!(exit.unwrap().code(), Some(23));
    assert!(session.interests().pty.is_none());
    assert_eq!(
        session.screen().grid().row(1).unwrap().text(3),
        Some(CellText::Scalar('l'))
    );
}

#[test]
fn queued_key_keeps_its_encoding_across_a_mode_change() {
    let mut session = session(b"\x1b[?1h");
    session.pump().unwrap();
    let modifiers = Modifiers::default();
    assert!(session.queue_key(Key::Up, modifiers, false).unwrap());

    session.transport.source = Cursor::new(b"\x1b[?1;8l");
    session.pump().unwrap();
    assert_eq!(session.transport.written, b"\x1b");
    assert!(!session.queue_key(Key::Up, modifiers, true).unwrap());
    assert_eq!(
        session
            .queue_key(Key::Up, modifiers, false)
            .unwrap_err()
            .kind(),
        io::ErrorKind::WouldBlock
    );

    session.transport.write_budget = usize::MAX;
    session.pump().unwrap();
    assert!(session.queue_key(Key::Up, modifiers, false).unwrap());
    session.pump().unwrap();
    assert!(
        session
            .queue_character(
                'c',
                Modifiers {
                    alt: true,
                    control: true,
                    ..modifiers
                },
                false
            )
            .unwrap()
    );
    session.pump().unwrap();
    assert_eq!(session.transport.written, b"\x1bOA\x1b[A\x1b\x03");
}

#[test]
fn queued_mouse_report_finishes_before_a_reply_from_new_modes() {
    let mut session = session(b"\x1b[?1000;1004;1006h");
    assert!(!session.queue_focus(true).unwrap());
    session.pump().unwrap();
    session.transport.write_budget = usize::MAX;
    assert!(session.queue_focus(true).unwrap());
    session.pump().unwrap();

    let cell = CellPosition { row: 1, column: 2 };
    let shift = Modifiers {
        shift: true,
        ..Modifiers::default()
    };
    assert_eq!(
        session
            .mouse_input(MouseEvent::Press(MouseButton::Left), cell, None, shift)
            .unwrap(),
        MouseInputOutcome::Queued
    );
    session.transport.source = Cursor::new(b"\x1b[?1004;1006l\x1b[6n");
    session.transport.write_budget = 1;
    session.pump().unwrap();
    assert_eq!(session.transport.written, b"\x1b[I\x1b");
    assert_eq!(
        session.queue_focus(false).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );

    session.transport.write_budget = usize::MAX;
    session.pump().unwrap();
    assert!(!session.queue_focus(false).unwrap());
    assert_eq!(
        session
            .mouse_input(
                MouseEvent::Release(MouseButton::Left),
                cell,
                None,
                Modifiers::default()
            )
            .unwrap(),
        MouseInputOutcome::Queued
    );
    session.pump().unwrap();
    assert_eq!(
        session.transport.written,
        b"\x1b[I\x1b[<4;3;2M\x1b[1;1R\x1b[M##\""
    );
}

#[test]
fn grid_geometry_checks_pty_and_grid_capacity() {
    let cell = pixels(8, 16);
    assert!(GridGeometry::fit(pixels(7, 32), cell).is_none());
    assert!(GridGeometry::fit(pixels(65_536, 16), pixels(32_768, 16)).is_none());
    assert!(GridGeometry::fit(pixels(1025, 1024), pixels(1, 1)).is_none());
    assert!(GridGeometry::fit(pixels(1024, 1024), pixels(1, 1)).is_some());

    let uneven = GridGeometry::from_pty_size(PtySize {
        pixel_width: 65,
        pixel_height: 48,
        ..PtySize::new(3, 8)
    });
    assert_eq!(uneven.text_area_pixels(), Some(pixels(65, 48)));
    assert_eq!(uneven.cell_pixels(), None);
}

#[test]
fn pixel_mouse_uses_content_bounds_and_tracking_takes_precedence_over_alternate_scroll() {
    let mut session = session(b"\x1b[?1;1007;1016h");
    session.geometry = GridGeometry::fit(pixels(67, 61), pixels(8, 16)).unwrap();
    session.transport.write_budget = usize::MAX;
    session.pump().unwrap();
    let cell = CellPosition { row: 2, column: 7 };
    let modifiers = Modifiers::default();
    assert_eq!(
        session
            .mouse_input(MouseEvent::WheelUp, cell, None, modifiers)
            .unwrap(),
        MouseInputOutcome::Ignored
    );

    session.transport.source = Cursor::new(b"\x1b[?1049h");
    session.pump().unwrap();
    assert_eq!(
        session
            .mouse_input(MouseEvent::WheelUp, cell, None, modifiers)
            .unwrap(),
        MouseInputOutcome::Queued
    );
    session.pump().unwrap();

    session.transport.source = Cursor::new(b"\x1b[?1000h");
    session.pump().unwrap();
    for (cell, pixel) in [
        (cell, None),
        (cell, Some(PixelPosition { x: 64, y: 47 })),
        (
            CellPosition { row: 2, column: 8 },
            Some(PixelPosition { x: 63, y: 47 }),
        ),
    ] {
        assert_eq!(
            session
                .mouse_input(MouseEvent::WheelDown, cell, pixel, modifiers)
                .unwrap(),
            MouseInputOutcome::Ignored
        );
    }
    for pixel in [
        PixelPosition { x: 64, y: 47 },
        PixelPosition { x: 63, y: 48 },
    ] {
        assert_eq!(
            session
                .mouse_input_at(MouseEvent::WheelDown, pixel, modifiers)
                .unwrap(),
            MouseInputOutcome::Ignored
        );
    }
    let last_pixel = PixelPosition { x: 63, y: 47 };
    assert_eq!(
        session
            .mouse_input_at(MouseEvent::WheelDown, last_pixel, modifiers)
            .unwrap(),
        MouseInputOutcome::Queued
    );
    session.pump().unwrap();
    session.transport.source = Cursor::new(b"\x1b[?1016l\x1b[?1006h");
    session.pump().unwrap();
    assert_eq!(
        session
            .mouse_input_at(MouseEvent::WheelDown, last_pixel, modifiers)
            .unwrap(),
        MouseInputOutcome::Queued
    );
    session.pump().unwrap();
    assert_eq!(
        session.transport.written,
        b"\x1bOA\x1b[<65;64;48M\x1b[<65;8;3M"
    );
}

#[test]
fn browsing_scrollback_keeps_the_anchor_and_routes_wheel_steps_locally() {
    let mut session = session(b"a\r\nb\r\nc\r\nd\r\ne");
    let limits = HistoryLimits {
        max_rows: 2,
        max_bytes: 1024,
    };
    session.set_history_limits(limits).unwrap();
    session.pump().unwrap();
    let cell = CellPosition { row: 0, column: 0 };
    let mouse = |session: &mut Session<Transport>, event| {
        session
            .mouse_input(event, cell, None, Modifiers::default())
            .unwrap()
    };
    let top = |session: &Session<Transport>| match session
        .presentation_screen()
        .unwrap()
        .viewport()
        .row(0)
        .unwrap()
        .text(0)
    {
        Some(CellText::Scalar(value)) => value,
        _ => panic!("expected a viewport marker"),
    };
    session.queue_input(b"x").unwrap();
    assert_eq!(
        mouse(&mut session, MouseEvent::WheelUp),
        MouseInputOutcome::HistoryScrolled
    );
    assert_eq!(top(&session), 'b');

    session.transport.source = Cursor::new(b"\r\nf\x1b[?1000;1006h");
    session.transport.write_budget = usize::MAX;
    session.pump().unwrap();
    assert_eq!(top(&session), 'b');
    assert_eq!(session.screen().history().len(), 2);
    assert_eq!(
        mouse(&mut session, MouseEvent::Press(MouseButton::Left)),
        MouseInputOutcome::Ignored
    );
    assert_eq!(
        mouse(&mut session, MouseEvent::WheelDown),
        MouseInputOutcome::HistoryScrolled
    );
    assert_eq!(top(&session), 'c');

    session.follow_output();
    assert_eq!(top(&session), 'd');
    assert_eq!(
        mouse(&mut session, MouseEvent::WheelDown),
        MouseInputOutcome::Queued
    );
    session.pump().unwrap();
    assert_eq!(session.transport.written, b"x\x1b[<65;1;1M");

    session.transport.hung_up = true;
    session.pump().unwrap();
    assert_eq!(
        mouse(&mut session, MouseEvent::WheelUp),
        MouseInputOutcome::HistoryScrolled
    );
    assert!(session.scroll_history(isize::MAX));
    let max_bytes = session.screen().history().retained_bytes() / 2;
    session
        .set_history_limits(HistoryLimits {
            max_bytes,
            ..limits
        })
        .unwrap();
    assert_eq!(session.screen().viewport_offset(), 1);
    assert_eq!(top(&session), 'c');
    assert!(session.scroll_history(isize::MIN));
    assert_eq!(top(&session), 'd');
}

#[test]
fn large_bracketed_paste_spans_pumps_without_interleaving_replies() {
    let mut session = session(b"\x1b[?2004h");
    session.pump().unwrap();
    let payload = "水\r\n".repeat(PASTE_WRITE_BYTES / 5 + 4);
    session.queue_paste(&payload).unwrap();
    session.transport.source = Cursor::new(b"\x1b[?2004l\x1b[6n\x1b[6n");
    session.pump().unwrap();
    assert_eq!(session.transport.written, b"\x1b");
    assert!(!session.wants_read());
    assert_eq!(
        session.queue_paste("next").unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );

    session.transport.write_budget = usize::MAX;
    session.pump().unwrap();
    assert_eq!(session.transport.written.len(), 1 + PASTE_WRITE_BYTES);
    assert!(session.wants_write());
    session.pump().unwrap();
    assert!(session.wants_read());
    assert!(!session.wants_write());

    session.queue_paste("水").unwrap();
    session.pump().unwrap();
    let mut expected = b"\x1b[200~".to_vec();
    expected.extend_from_slice(payload.as_bytes());
    expected.extend_from_slice(b"\x1b[201~\x1b[1;1R\x1b[1;1R");
    expected.extend_from_slice("水".as_bytes());
    assert_eq!(session.transport.written, expected);
}

#[test]
fn child_observes_committed_grid_and_pixel_resizes() {
    let mut child = Command::new("sh");
    child.args([
        "-c",
        r#"
        read_report() {
            expected=$(printf "$1")
            actual=$(dd bs=1 count="${#expected}" 2>/dev/null)
            test "$actual" = "$expected"
        }
        stty -echo -icanon min 1 time 0 || exit 71
        printf '\033[?2048h'
        read_report '\033[48;3;8;0;0t' || exit 72
        printf 'abcdefghijkl'
        read_report '\033[48;4;12;480;960t' || exit 73
        test "$(stty size)" = '4 12' || exit 74
        printf '\033[16t'
        read_report '\033[6;120;80t' || exit 75
        printf '\033[4;1HY'
        read_report '\033[48;4;12;600;1200t' || exit 76
        printf '\033[16t'
        read_report '\033[6;150;100t' || exit 77
        "#,
    ]);
    let color = Rgb::default();
    let initial = PtySize::new(3, 8);
    let mut session = Session::spawn(
        child,
        initial,
        Palette::new([color; 256], color, color, color),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    wait_until(&mut session, deadline, "initial text", |session| {
        session.screen.grid().row(1).unwrap().text(3) == Some(CellText::Scalar('l'))
    });

    let cursor = session.screen.cursor();
    assert!(session.resize(PtySize::new(0, 12)).is_err());
    assert_eq!(session.transport.size().unwrap(), initial);
    assert_eq!(session.geometry().pty_size(), initial);
    assert_eq!(session.screen.cursor(), cursor);
    assert!(!session.wants_write());

    let resized = GridGeometry::fit(pixels(979, 501), pixels(80, 120)).unwrap();
    session.resize(resized.pty_size()).unwrap();
    assert_eq!(session.transport.size().unwrap(), resized.pty_size());
    assert_eq!(session.geometry(), resized);
    assert_eq!(session.screen.grid().rows(), 4);
    assert_eq!(
        session.screen.grid().row(0).unwrap().text(11),
        Some(CellText::Scalar('l'))
    );
    wait_until(
        &mut session,
        deadline,
        "resize acknowledgement",
        |session| session.screen.grid().row(3).unwrap().text(0) == Some(CellText::Scalar('Y')),
    );

    session.resize(resized.pty_size()).unwrap();
    assert!(!session.wants_write());
    let mut tracker = PresentationTracker::default();
    let now = Instant::now();
    tracker
        .update(&session.presentation_frame().unwrap(), now)
        .unwrap();
    let pixels_only = GridGeometry::fit(pixels(1200, 600), pixels(100, 150)).unwrap();
    session.resize(pixels_only.pty_size()).unwrap();
    assert_eq!(session.transport.size().unwrap(), pixels_only.pty_size());
    assert_eq!(session.geometry(), pixels_only);
    assert_eq!(
        tracker
            .update(&session.presentation_frame().unwrap(), now)
            .unwrap()
            .rows,
        RowDamage::All
    );
    wait_until(&mut session, deadline, "EOF", Session::is_finished);
    loop {
        if let Some(status) = session.try_wait().unwrap() {
            assert!(status.success(), "resize handshake exited with {status}");
            break;
        }
        assert!(Instant::now() < deadline, "child did not exit after EOF");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn wait_until(
    session: &mut Session,
    deadline: Instant,
    stage: &str,
    ready: impl Fn(&Session) -> bool,
) {
    while !ready(session) {
        assert!(!session.is_finished(), "child exited before {stage}");
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for {stage}");
        assert_ne!(
            session.wait_and_pump(Some(remaining)).unwrap(),
            WaitOutcome::TimedOut,
            "timed out waiting for {stage}"
        );
    }
}
