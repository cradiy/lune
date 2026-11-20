use std::io::Cursor;
use std::time::{Duration, Instant};

use lune_terminal::{CellText, Rgb};

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
