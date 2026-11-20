use std::io::Cursor;

use lune_terminal::{CellText, MouseButton, Rgb};

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
fn pixel_mouse_uses_content_bounds_and_tracking_takes_precedence_over_alternate_scroll() {
    let mut session = session(b"\x1b[?1;1007;1016h");
    session.size.pixel_width = 64;
    session.size.pixel_height = 48;
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
    assert_eq!(
        session
            .mouse_input(
                MouseEvent::WheelDown,
                cell,
                Some(PixelPosition { x: 63, y: 47 }),
                modifiers
            )
            .unwrap(),
        MouseInputOutcome::Queued
    );
    session.pump().unwrap();
    assert_eq!(session.transport.written, b"\x1bOA\x1b[<65;64;48M");
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
        printf '\033[4;1HY'
        read_report '\033[48;4;12;600;1200t' || exit 75
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
    let pump_until = |session: &mut Session, stage: &str, ready: fn(&Session) -> bool| {
        while !ready(session) {
            assert!(Instant::now() < deadline, "timed out waiting for {stage}");
            assert!(!session.is_finished(), "child exited before {stage}");
            session.pump().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    pump_until(&mut session, "initial text", |session| {
        session.screen.grid().row(1).unwrap().text(3) == Some(CellText::Scalar('l'))
    });

    let cursor = session.screen.cursor();
    assert!(session.resize(PtySize::new(0, 12)).is_err());
    assert_eq!(session.transport.size().unwrap(), initial);
    assert_eq!(session.screen.cursor(), cursor);
    assert!(!session.wants_write());

    let resized = PtySize {
        pixel_width: 960,
        pixel_height: 480,
        ..PtySize::new(4, 12)
    };
    session.resize(resized).unwrap();
    assert_eq!(session.transport.size().unwrap(), resized);
    assert_eq!(session.screen.grid().rows(), 4);
    assert_eq!(
        session.screen.grid().row(0).unwrap().text(11),
        Some(CellText::Scalar('l'))
    );
    pump_until(&mut session, "resize acknowledgement", |session| {
        session.screen.grid().row(3).unwrap().text(0) == Some(CellText::Scalar('Y'))
    });

    session.resize(resized).unwrap();
    assert!(!session.wants_write());
    let pixels_only = PtySize {
        pixel_width: 1200,
        pixel_height: 600,
        ..resized
    };
    session.resize(pixels_only).unwrap();
    assert_eq!(session.transport.size().unwrap(), pixels_only);
    pump_until(&mut session, "EOF", Session::is_finished);
    loop {
        if let Some(status) = session.try_wait().unwrap() {
            assert!(status.success(), "resize handshake exited with {status}");
            break;
        }
        assert!(Instant::now() < deadline, "child did not exit after EOF");
        std::thread::sleep(Duration::from_millis(1));
    }
}
