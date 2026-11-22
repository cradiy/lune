use std::io::Cursor;
use std::num::NonZeroU32;

use lune_terminal::{
    CellText, Color, MouseButton, PaletteEntry, PixelSize, Rgb, WorkingDirectoryChange,
};

use super::*;

#[cfg(feature = "wgpu-renderer")]
static OFFSCREEN_GPU_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    session_with_size(source, PtySize::new(3, 8))
}

fn session_with_size(source: &'static [u8], size: PtySize) -> Session<Transport> {
    let color = Rgb::default();
    let palette = Palette::new([color; 256], color, color, color);
    Session::new(
        Transport {
            source: Cursor::new(source),
            written: Vec::new(),
            write_budget: 1,
            hung_up: false,
        },
        size,
        Screen::new(size.columns.into(), size.rows.into()).unwrap(),
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
fn kitty_images_commit_after_the_last_chunk_and_keep_reply_order() {
    let mut session = session(b"\x1b_Gi=7,p=18,f=24,s=2,v=1,m=1;/wAA\x1b\\");
    session.pump().unwrap();
    assert!(session.graphics_image(7).is_none());
    assert!(session.transport.written.is_empty());

    session.transport.source = Cursor::new(
        concat!(
            "\x1b_Xforeign\x18",
            "\x1b_Ga=q,i=39,e=T;ignored\x18",
            "\x1b_Ga=q,i=31,e=T,t=d,U=1,C=1,f=32,s=10000,v=10001,N=1,P=2,H=-3,m=1;%%ignored%%\x1b\\",
            "\x1b_Ga=q,i=32,e=a,s=3,c=2\x1b\\",
            "\x1b_Ga=q,i=33,e=f,r=1,X=1,C=1,f=16,N=1\x1b\\",
            "\x1b_Ga=q,i=34,e=W,c=2\x1b\\",
            "\x1b_Ga=q,i=35,I=0,e=T\x1b\\",
            "\x1b_Ga=q,i=36,e=d,d=a\x1b\\",
            "\x1b_Ga=q,i=37,e=f,t=d,f=32,N=1\x1b\\",
            "\x1b_Ga=q,i=38,I=2,e=T\x1b\\",
            "\x1b_Gm=0;AAD/\x1b\\",
            "\x1b_Ga=q,i=7,p=19,f=24,s=1,v=1;AAAA\x1b\\",
            "\x1b_Ga=q,i=7,p=20,f=24,s=1,v=0;AAAA\x1b\\\x1b[c",
        )
        .as_bytes(),
    );
    session.pump().unwrap();
    assert_eq!(session.transport.written, b"\x1b");
    assert!(!session.wants_read());
    assert!(session.graphics_image(7).is_none());

    session.transport.write_budget = usize::MAX;
    session.pump().unwrap();
    let image = session.graphics_image(7).unwrap();
    assert_eq!(image.dimensions(), (2, 1));
    assert_eq!(
        image.pixels(&mut Vec::new()).unwrap(),
        &[255, 0, 0, 255, 0, 0, 255, 255]
    );
    assert_eq!(
        session.transport.written,
        concat!(
            "\x1b_Gi=31,e=T,t=d,f=32,s=10000,N=1,C=1,U=1,P=2,H=-3;OK\x1b\\",
            "\x1b_Gi=32,e=a,s=3,c=2;OK\x1b\\",
            "\x1b_Gi=33,e=f,N=1,r=1,X=1;OK\x1b\\",
            "\x1b_Gi=34;OK\x1b\\",
            "\x1b_Gi=35,e=T;OK\x1b\\",
            "\x1b_Gi=36,e=d,d=a;OK\x1b\\",
            "\x1b_Gi=37,e=f,t=d,f=32,N=1;OK\x1b\\",
            "\x1b_Gi=38,I=2;EINVAL:Invalid image data\x1b\\",
            "\x1b_Gi=7,p=18;OK\x1b\\",
            "\x1b_Gi=7;OK\x1b\\",
            "\x1b_Gi=7;EINVAL:Invalid image data\x1b\\\x1b[?62;22c",
        )
        .as_bytes(),
    );
}

#[test]
fn kitty_chunked_transfers_keep_their_quiet_setting_and_cancel_incomplete_uploads() {
    let mut session = session(
        concat!(
            "\x1b_Gi=1,f=24,s=2,v=1,m=1,q=1;/wAA\x1b\\",
            "\x1b_Gm=0,q=0,i=0,I=0;AAD/\x1b\\",
            "\x1b_Gi=2,s=1,v=1,q=1;AQID/w\x1b\\",
            "\x1b_Gi=2,f=24,s=2,v=1,m=1,q=2;/wAA\x1b\\",
            "\x1b_Gm=0,q=0;AAD/%%%%\x1b\\",
            "\x1b_Gi=3,f=24,s=2,v=1,o=z,m=1,q=2;eJz7z8DA\x1b\\",
            "\x1b_Gm=0;wPAfAAb/Af8=\x18",
            "\x1b_Gi=4,f=24,s=1,v=1,o=z,q=0;eJxj+M8AAAIBAQA=\x1b\\",
            "\x1b_Ga=T,i=4,p=7,f=24,s=2,v=1,m=1;/wAA\x1b\\",
            "\x1b_Gm=1,m=0;AP8A\x1b\\",
            "\x1b_Gm=0;AAD/\x1b\\",
            "\x1b_Gi=5,f=24,s=2,v=1,m=1,q=2;/wAA\x1b\\",
            "\x1b_Gm=1,m=0;AP8A\x1b\\",
            "\x1b_Gm=0;AAD/\x1b\\\x1b[c",
        )
        .as_bytes(),
    );
    let first = b"\x1b_Gi=4;OK\x1b\\";
    session.transport.write_budget = first.len() + 1;
    session.pump().unwrap();
    assert_eq!(
        session.transport.written.strip_prefix(first),
        Some(b"\x1b".as_slice())
    );
    assert!(!session.wants_read());

    session.transport.write_budget = usize::MAX;
    session.pump().unwrap();

    assert_eq!(
        session.transport.written,
        b"\x1b_Gi=4;OK\x1b\\\x1b_Gi=4,p=7;EINVAL:Invalid image data\x1b\\\x1b[?62;22c"
    );
    let mut scratch = Vec::new();
    let pixels = session
        .graphics_image(1)
        .unwrap()
        .pixels(&mut scratch)
        .unwrap();
    assert_eq!(pixels, &[255, 0, 0, 255, 0, 0, 255, 255]);
    let preserved = session.graphics_image(2).unwrap();
    assert_eq!(preserved.dimensions(), (1, 1));
    assert_eq!(preserved.pixels(&mut scratch).unwrap(), &[1, 2, 3, 255]);
    assert!(session.graphics_image(3).is_none());
    assert_eq!(
        session
            .graphics_image(4)
            .unwrap()
            .pixels(&mut scratch)
            .unwrap(),
        &[0, 255, 0, 255],
    );
    assert!(session.screen().graphics_placements().is_empty());
    assert!(session.graphics_image(5).is_none());
}

#[test]
fn kitty_oversized_payloads_keep_replies_and_committed_images() {
    let mut wire = b"\x1b_Ga=T,i=1,p=7,f=24,s=1,v=1,C=1,q=2;/wAA\x1b\\".to_vec();
    for (header, bytes) in [
        ("\x1b_Ga=q,i=11,t=f,s=1,v=1;", 8192),
        ("\x1b_Ga=T,i=1,p=7,f=24,s=256,v=129;", 128 * 1024 + 4),
        (
            "\x1b_Gi=2,f=24,s=2,v=1,m=1;/wAA\x1b\\\x1b_Ga=q,i=12,e=t;",
            8192,
        ),
        ("\x1b_Gm=0,q=2;", 128 * 1024 + 4),
    ] {
        wire.extend_from_slice(header.as_bytes());
        wire.extend(std::iter::repeat_n(b'A', bytes));
        wire.extend_from_slice(b"\x1b\\");
    }
    wire.extend_from_slice(b"\x1b_Gi=3,f=24,s=1,v=1;AP8A\x1b\\\x1b[c");

    let mut session = session(b"");
    session.transport.write_budget = usize::MAX;
    for chunk in wire.chunks(257) {
        session.incoming[..chunk.len()].copy_from_slice(chunk);
        session.incoming_bytes = Some(0..chunk.len());
        session.pump().unwrap();
        assert!(session.incoming_bytes.is_none());
    }
    assert_eq!(
        session.transport.written,
        concat!(
            "\x1b_Gi=11;EBADF:Failed to read image file\x1b\\",
            "\x1b_Gi=1,p=7;EINVAL:Invalid image data\x1b\\",
            "\x1b_Gi=12,e=t;OK\x1b\\",
            "\x1b_Gi=3;OK\x1b\\\x1b[?62;22c",
        )
        .as_bytes()
    );
    let [placement] = session.screen().graphics_placements() else {
        panic!("The existing placement must be preserved");
    };
    assert_eq!(placement.placement.id, 7);
    let mut scratch = Vec::new();
    assert_eq!(
        session
            .graphics_image(1)
            .unwrap()
            .pixels(&mut scratch)
            .unwrap(),
        [255, 0, 0, 255]
    );
    assert!(session.graphics_image(2).is_none());
    assert_eq!(
        session
            .graphics_image(3)
            .unwrap()
            .pixels(&mut scratch)
            .unwrap(),
        [0, 255, 0, 255]
    );
}

#[test]
fn kitty_partial_uploads_resume_in_their_own_screen_buffer() {
    let mut session = session(
        concat!(
            "\x1b_Gi=7,f=24,s=2,v=1,o=z,m=1;eJz7z8DA\x1b\\",
            "\x1b[?47h\x1b_Gi=7,f=24,s=2,v=1,o=z,m=1;eJxj+M/w\x1b\\",
            "\x1b_Ga=q,i=30,e=f,r=2\x1b\\",
            "\x1b[?47l\x1b_Gm=0;wPAfAAb/Af8=\x1b\\",
        )
        .as_bytes(),
    );
    session.transport.write_budget = usize::MAX;
    session.pump().unwrap();
    let mut scratch = Vec::new();
    assert_eq!(
        session
            .graphics_image(7)
            .unwrap()
            .pixels(&mut scratch)
            .unwrap(),
        &[255, 0, 0, 255, 0, 0, 255, 255]
    );
    assert!(
        session
            .processor()
            .graphics_images(ScreenBuffer::Alternate)
            .get(7)
            .is_none()
    );
    session.transport.source = Cursor::new(b"\x1b[?47h\x1b_Gm=0;n4EBAAf+Af8=\x1b\\");
    session.pump().unwrap();
    assert_eq!(
        session
            .graphics_image(7)
            .unwrap()
            .pixels(&mut scratch)
            .unwrap(),
        &[0, 255, 0, 255, 255, 0, 0, 255]
    );
    assert_eq!(
        session.transport.written,
        b"\x1b_Gi=30,e=f,r=2;OK\x1b\\\x1b_Gi=7;OK\x1b\\\x1b_Gi=7;OK\x1b\\"
    );

    session.transport.written.clear();
    session.transport.source = Cursor::new(
        concat!(
            "\x1b_Gi=8,f=24,s=2,v=1,m=1;/wAA\x1b\\",
            "\x1b[?47l\x1b_Gi=8,f=24,s=2,v=1,m=1;AAD/\x1b\\",
            "\x1b[?47h\x1b_Ga=d,d=f,i=7\x1b\\\x1b_Gm=0;AAD/\x1b\\",
            "\x1b[?47l\x1b_Gm=0;/wAA\x1b\\",
        )
        .as_bytes(),
    );
    session.pump().unwrap();
    let alternate = session.processor().graphics_images(ScreenBuffer::Alternate);
    assert!(alternate.get(8).is_none());
    assert!(alternate.get(7).is_some());
    assert_eq!(
        session
            .graphics_image(8)
            .unwrap()
            .pixels(&mut scratch)
            .unwrap(),
        &[0, 0, 255, 255, 255, 0, 0, 255]
    );
    assert_eq!(session.transport.written, b"\x1b_Gi=8;OK\x1b\\");
}

#[test]
fn kitty_scaled_placements_hit_only_the_cells_they_cover() {
    let data = "/wAA".repeat(30 * 30);
    for (cell_width, cell_height, placement, axis, span) in [
        (5, 3, "c=3,h=29", 'y', 5),
        (3, 5, "r=3,h=29", 'x', 5),
        (5, 3, "c=3,h=30", 'y', 6),
    ] {
        let mut session = session_with_size(
            b"",
            PtySize {
                pixel_width: 16 * cell_width,
                pixel_height: 12 * cell_height,
                ..PtySize::new(12, 16)
            },
        );
        let wire = format!(
            "\x1b_Ga=T,i=1,f=24,s=30,v=30,x=1,w=29,C=1,q=2,{placement};{data}\x1b\\\x1b_Ga=d,d={axis},{axis}={}\x1b\\",
            span + 1
        );
        session
            .processor
            .advance(wire.as_bytes(), &mut session.screen, &mut session.writer)
            .unwrap();
        assert_eq!(session.screen().graphics_placements().len(), 1);

        let wire = format!("\x1b_Ga=d,d={axis},{axis}={span}\x1b\\");
        session
            .processor
            .advance(wire.as_bytes(), &mut session.screen, &mut session.writer)
            .unwrap();
        assert!(session.screen().graphics_placements().is_empty());
    }
}

#[test]
fn kitty_placements_use_the_final_cursor_and_replace_atomically() {
    let size = PtySize {
        pixel_width: 16,
        pixel_height: 6,
        ..PtySize::new(3, 8)
    };
    let mut session = session_with_size(b"\x1b_Ga=T,i=7,p=5,f=24,s=2,v=1,m=1,C=1;/wAA\x1b\\", size);
    session.transport.write_budget = usize::MAX;
    session.pump().unwrap();
    assert!(session.screen().graphics_placements().is_empty());

    session.transport.source = Cursor::new(b"\x1b[2;3H\x1b_Gm=0;AAD/\x1b\\");
    session.pump().unwrap();
    let [image] = session.screen().graphics_placements() else {
        panic!("Expected one placement")
    };
    assert_eq!((image.row, image.column), (1, 2));
    assert_eq!(session.screen().cursor().column, 2);
    assert_eq!(session.transport.written, b"\x1b_Gi=7,p=5;OK\x1b\\");

    session.transport.source = Cursor::new(b"\x1b[1;1H\x1b_Ga=p,i=7,p=5,x=1,c=1,r=1,q=1\x1b\\\x1b_Ga=T,i=7,p=6,s=1,v=1,x=1;AAAAAA==\x1b\\");
    session.pump().unwrap();
    let [image] = session.screen().graphics_placements() else {
        panic!("Expected a replaced placement")
    };
    assert_eq!((image.row, image.column), (0, 0));
    assert_eq!(image.placement.source_size, [1, 1]);
    assert_eq!(
        (
            session.screen().cursor().row,
            session.screen().cursor().column
        ),
        (1, 1)
    );
    assert!(
        session
            .transport
            .written
            .ends_with(b"\x1b_Gi=7,p=6;EINVAL:Invalid placement region\x1b\\")
    );
    assert_eq!(session.graphics_image(7).unwrap().dimensions(), (2, 1));

    let now = Instant::now();
    let mut tracker = PresentationTracker::default();
    tracker
        .prepare(&session.presentation_frame().unwrap(), now)
        .unwrap()
        .commit();
    session.transport.source = Cursor::new(b"\x1b_Ga=p,i=7,p=5,C=1,N=1,q=1\x1b\\");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    let prepared = tracker.prepare(&frame, now).unwrap();
    assert_eq!(prepared.damage().rows, RowDamage::Rows(&[]));
    assert!(prepared.damage().cursor.is_none());
    assert!(prepared.damage().graphics_changed);
    drop(prepared);
    let retry = tracker.prepare(&frame, now).unwrap();
    assert!(retry.damage().graphics_changed);
    retry.commit();
    let unchanged = tracker.prepare(&frame, now).unwrap();
    assert!(!unchanged.damage().graphics_changed);
    unchanged.commit();

    let stamp = frame.graphics_stamp();
    session.transport.source = Cursor::new(b"\x1b_Ga=p,i=7,p=5\x1b\\");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    assert_eq!(frame.graphics_stamp(), stamp);
    let prepared = tracker.prepare(&frame, now).unwrap();
    assert!(!prepared.damage().graphics_changed);
    assert_eq!(session.screen().cursor().row, 2);
    assert_eq!(session.screen().cursor().column, 2);
    assert!(
        session
            .transport
            .written
            .ends_with(b"\x1b_Gi=7,p=5;OK\x1b\\")
    );
    prepared.commit();

    session.transport.source = Cursor::new(b"\x1b[?1049h\x1b_Ga=p,i=7,C=1,q=1\x1b\\\x1b[?1049l");
    session.pump().unwrap();
    assert_eq!(session.screen().graphics_placements().len(), 1);
    session.transport.source = Cursor::new(b"\x1b[2J");
    session.pump().unwrap();
    assert!(session.screen().graphics_placements().is_empty());
    assert!(session.graphics_image(7).is_none());
}

#[test]
fn kitty_history_preserves_visible_fragments_until_their_rows_are_evicted() {
    let size = PtySize {
        pixel_width: 16,
        pixel_height: 6,
        ..PtySize::new(3, 8)
    };
    let mut session = session_with_size(
        "\x1b_Ga=T,s=1,v=4,C=1;/wAA//8AAP8A/wD/AAD//w==\x1b\\\x1b[3;8HA水".as_bytes(),
        size,
    );
    session
        .set_history_limits(HistoryLimits {
            max_rows: 1,
            max_bytes: 4096,
        })
        .unwrap();
    session.pump().unwrap();
    assert!(session.scroll_history(1));
    let frame = session.presentation_frame().unwrap();
    let [placement] = frame.graphics_placements() else {
        panic!("Expected one placement")
    };
    assert_eq!(placement.row + frame.viewport.offset() as i64, 0);
    let key = placement.placement.image;
    assert!(
        session
            .processor()
            .graphics_images(session.screen().active_buffer())
            .source_revision(key)
            .is_some()
    );

    session.transport.source = Cursor::new(b"\n\x1b[2J");
    session.pump().unwrap();
    let [placement] = session.screen().graphics_placements() else {
        panic!("Expected the historical fragment")
    };
    assert_eq!(placement.row, -1);
    assert_eq!(placement.layout(pixels(2, 2)).source_origin[1], 2.0);
    assert_eq!(session.screen().viewport_offset(), 1);

    session.screen.reflow(16, 3, StyleId::default()).unwrap();
    let [resized] = session.screen().graphics_placements() else {
        panic!("The retained history fragment survives reflow");
    };
    assert_eq!(resized.row, -1);
    assert_eq!(resized.layout(pixels(2, 2)).source_origin[1], 2.0);

    session.transport.source = Cursor::new(b"\x1b[3J");
    session.pump().unwrap();
    assert!(session.screen().graphics_placements().is_empty());
    assert!(
        session
            .processor()
            .graphics_images(session.screen().active_buffer())
            .source_revision(key)
            .is_none()
    );

    session.transport.source = Cursor::new(
        concat!(
            "\x1b[H\x1b_Ga=T,i=63,p=1,s=1,v=1,C=1,q=2;AP8A/w==\x1b\\",
            "\x1b[3;1H\x1bD\x1b_Ga=p,i=63,p=2,C=1,q=2\x1b\\\x1b[2J",
        )
        .as_bytes(),
    );
    session.pump().unwrap();
    let [placement] = session.screen().graphics_placements() else {
        panic!("ED2 keeps the named placement in scrollback");
    };
    assert_eq!((placement.row, placement.placement.id), (-1, 1));
    assert!(session.graphics_image(63).is_some());
}

#[test]
fn kitty_placeholders_follow_top_region_scrollback_past_a_fixed_status_row() {
    let size = PtySize {
        pixel_width: 16,
        pixel_height: 10,
        ..PtySize::new(5, 8)
    };
    let mut session = session_with_size(
        concat!(
            "\x1b[1;4rA\x1b_Ga=T,s=1,v=1,C=1,q=2;/wAA/w==\x1b\\",
            "\x1b_Ga=T,i=42,U=1,c=1,r=1,s=1,v=1,q=2;AP8A/w==\x1b\\",
            "\x1b[1;4H\x1b[38;5;42m\u{10eeee}\x1b[0m",
            "\x1b[4;6H\x1b_Ga=T,i=3,s=1,v=1,r=2,C=1,q=2;AAD//w==\x1b\\",
            "\x1b[5;1HSTATUS",
        )
        .as_bytes(),
        size,
    );
    session
        .set_history_limits(HistoryLimits {
            max_rows: 2,
            max_bytes: 4096,
        })
        .unwrap();
    session.pump().unwrap();
    let key = session
        .screen()
        .graphics_placements()
        .iter()
        .find(|image| image.placement.image.id() == 0)
        .unwrap()
        .placement
        .image;
    session.transport.source = Cursor::new(b"\x1b[4;1H\x1bD");
    session.pump().unwrap();
    assert!(session.scroll_history(1));
    let frame = session.presentation_frame().unwrap();
    assert!(frame.image_store().source_revision(key).is_none());

    session.transport.source = Cursor::new(b"\x1b[S");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    assert_eq!(frame.viewport.offset(), 2);
    let top = frame.viewport.row(0).unwrap();
    assert_eq!(top.text(0), Some(CellText::Scalar('A')));
    assert_eq!(top.text(3), Some(CellText::Scalar('\u{10eeee}')));
    assert!(
        frame
            .graphics_placements()
            .iter()
            .any(|placement| { placement.placement.image.id() == 3 && placement.row == 3 })
    );
    assert_eq!(
        session.screen().grid().row(4).unwrap().text(0),
        Some(CellText::Scalar('S'))
    );

    session.transport.source = Cursor::new(b"\x1b[S");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    assert!(frame.image_store().source_revision(key).is_none());
    assert_eq!(
        frame.viewport.row(0).unwrap().text(3),
        Some(CellText::Scalar(' '))
    );
}

#[test]
fn kitty_unscroll_restores_images_with_their_anchored_rows() {
    let size = PtySize {
        pixel_width: 16,
        pixel_height: 10,
        ..PtySize::new(5, 8)
    };
    let mut session = session_with_size(
        concat!(
            "\x1b[1;5rA\x1b_Ga=T,i=1,p=1,s=1,v=1,C=1,q=2;/wAA/w==\x1b\\",
            "\x1b_Ga=T,i=3,P=1,Q=1,H=1,s=1,v=1,q=2;AAD//w==\x1b\\",
            "\x1b_Ga=T,i=42,U=1,c=1,r=1,s=1,v=1,q=2;AP8A/w==\x1b\\",
            "\x1b[1;4H\x1b[38;5;42m\u{10eeee}\x1b[0m",
            "\x1b[2;1HB\x1b[5;1HSTATUS\x1b[4;1H\x1b[2S",
        )
        .as_bytes(),
        size,
    );
    session
        .set_history_limits(HistoryLimits {
            max_rows: 4,
            max_bytes: 4096,
        })
        .unwrap();
    session.pump().unwrap();
    assert!(session.scroll_history(2));
    for (command, offset, live) in [(b"\x1b[+T".as_slice(), 1, 'B'), (b"\x1b[1+T", 0, 'A')] {
        session.transport.source = Cursor::new(command);
        session.pump().unwrap();
        let frame = session.presentation_frame().unwrap();
        assert_eq!(frame.viewport.offset(), offset);
        let anchor = frame.viewport.row(0).unwrap();
        assert_eq!(anchor.text(0), Some(CellText::Scalar('A')));
        assert_eq!(anchor.text(3), Some(CellText::Scalar('\u{10eeee}')));
        for (id, column) in [(1, 1), (3, 2)] {
            let placement = frame
                .graphics_placements()
                .iter()
                .find(|placement| placement.placement.image.id() == id)
                .unwrap();
            assert_eq!(
                placement.origin_in_view(offset, |_| None),
                Some([column, 0])
            );
        }
        assert_eq!(
            session.screen().grid().row(0).unwrap().text(0),
            Some(CellText::Scalar(live))
        );
        assert_eq!(
            session.screen().grid().row(4 - offset).unwrap().text(0),
            Some(CellText::Scalar('S'))
        );
    }
    assert_eq!(session.screen().history().retained_bytes(), 0);
}

#[test]
fn kitty_history_limits_release_orphaned_images_in_either_buffer() {
    let size = PtySize {
        pixel_width: 16,
        pixel_height: 6,
        ..PtySize::new(3, 8)
    };
    let input = concat!(
        "\x1b_Ga=T,i=1,p=1,s=1,v=1,C=1,q=2;/wAA/w==\x1b\\",
        "\x1b_Ga=T,i=2,p=1,P=1,Q=1,V=1,s=1,v=1,q=2;AP8A/w==\x1b\\",
        "\x1b_Ga=T,i=3,p=1,P=2,Q=1,s=1,v=1,q=2;AAD//w==\x1b\\",
        "\x1b_Ga=T,i=4,p=1,P=1,Q=1,s=1,v=1,q=2;/////w==\x1b\\",
        "\x1b[3;2H\x1b_Ga=p,i=4,p=2,C=1,q=2\x1b\\\x1bD",
    );
    for alternate in [false, true] {
        let mut session = session_with_size(input.as_bytes(), size);
        session
            .set_history_limits(HistoryLimits {
                max_rows: 1,
                max_bytes: 4096,
            })
            .unwrap();
        session.pump().unwrap();
        assert_eq!(session.screen().history().len(), 1);
        assert_eq!(session.screen().graphics_placements().len(), 5);
        if alternate {
            session.transport.source = Cursor::new(b"\x1b[?47h");
            session.pump().unwrap();
            assert_eq!(session.screen().active_buffer(), ScreenBuffer::Alternate);
        }
        session
            .set_history_limits(HistoryLimits::default())
            .unwrap();
        let images = session.processor().graphics_images(ScreenBuffer::Primary);
        assert!(images.get(2).is_none());
        assert!(images.get(3).is_none());
        assert!(images.get(1).is_some());
        assert!(images.get(4).is_some());
    }
}

#[test]
fn kitty_scroll_releases_orphaned_images_when_replies_are_backpressured() {
    let mut session = session_with_size(
        concat!(
            "\x1b_Ga=T,i=1,p=1,s=1,v=1,C=1,q=2;/wAA/w==\x1b\\",
            "\x1b_Ga=T,i=2,p=1,P=1,Q=1,V=1,s=1,v=1,q=2;AP8A/w==\x1b\\",
            "\x1b[6n\x1b[S\x1b[2;3H\x1b[6n\x1b_Ga=p,i=2,p=1\x1b\\",
        )
        .as_bytes(),
        PtySize {
            pixel_width: 16,
            pixel_height: 6,
            ..PtySize::new(3, 8)
        },
    );
    session.pump().unwrap();
    assert_eq!(session.transport.written, b"\x1b");
    assert!(!session.wants_read());
    assert!(session.screen().graphics_placements().is_empty());
    assert!(session.graphics_image(2).is_none());

    session.transport.write_budget = usize::MAX;
    session.pump().unwrap();
    assert_eq!(
        session.transport.written,
        b"\x1b[1;1R\x1b[2;3R\x1b_Gi=2,p=1;ENOENT:Image not found\x1b\\",
    );
}

#[test]
fn kitty_full_page_reverse_scroll_preserves_history_and_offscreen_pixels() {
    let size = PtySize {
        pixel_width: 16,
        pixel_height: 6,
        ..PtySize::new(3, 8)
    };
    let mut session = session_with_size(
        concat!(
            "\x1b_Ga=T,i=1,s=1,v=1,C=1,q=2;/wAA/w==\x1b\\",
            "\x1b[1;2H\x1b_Ga=T,i=2,s=1,v=4,C=1,q=2;/wAA//8AAP8A/wD/AAD//w==\x1b\\",
            "\x1b[3;1H\x1bD\x1b_Ga=T,i=3,s=1,v=1,C=1,q=2;/wAA/w==\x1b\\",
        )
        .as_bytes(),
        size,
    );
    session
        .set_history_limits(HistoryLimits {
            max_rows: 2,
            max_bytes: 4096,
        })
        .unwrap();
    session.pump().unwrap();
    let rows = |session: &Session<Transport>| {
        session
            .screen()
            .graphics_placements()
            .iter()
            .map(|image| (image.placement.image.id(), image.row))
            .collect::<Vec<_>>()
    };
    assert_eq!(rows(&session), [(1, -1), (2, -1), (3, 2)]);
    assert_eq!(session.screen().history().len(), 1);

    session.transport.source = Cursor::new(b"\x1b[H\x1bM");
    session.pump().unwrap();
    assert_eq!(rows(&session), [(1, -1), (2, 0), (3, 3)]);
    assert_eq!(session.screen().history().len(), 1);

    session.transport.source = Cursor::new(b"\x1b[?1049h");
    session.pump().unwrap();
    assert_eq!(session.screen().active_buffer(), ScreenBuffer::Alternate);
    session.screen.reflow(9, 3, StyleId::default()).unwrap();
    session.geometry = GridGeometry::from_pty_size(PtySize {
        columns: 9,
        pixel_width: 18,
        ..size
    });
    session.processor.update_geometry(
        &mut session.screen,
        session.geometry.text_area_pixels(),
        session.geometry.cell_pixels(),
    );
    session.transport.source = Cursor::new(b"\x1b[?1049l");
    session.pump().unwrap();
    assert_eq!(rows(&session), [(1, -1), (2, 0), (3, 3)]);

    session.transport.source = Cursor::new(b"\x1b[3;1H\x1bD");
    session.pump().unwrap();
    assert_eq!(rows(&session), [(1, -2), (2, -1), (3, 2)]);
}

#[test]
fn kitty_cropped_span_can_scroll_at_the_restored_cell_size() {
    let size = PtySize {
        pixel_width: 16,
        pixel_height: 10,
        ..PtySize::new(10, 8)
    };
    let mut session = session_with_size(b"", size);
    let wire = format!(
        "\x1b[2;8r\x1b[2;1H\x1b_Ga=T,i=1,f=24,s=1,v=7,C=1,q=2;{}\x1b\\\x1b[5S",
        "/wAA".repeat(7)
    );
    session
        .processor
        .advance(wire.as_bytes(), &mut session.screen, &mut session.writer)
        .unwrap();
    for height in [2, 1] {
        session.processor.update_geometry(
            &mut session.screen,
            Some(pixels(16, 10 * height)),
            Some(pixels(2, height)),
        );
    }
    session.transport.source = Cursor::new(b"\x1b[2;3r\x1b[S");
    session.pump().unwrap();
    let [placement] = session.screen().graphics_placements() else {
        panic!("Expected the retained image");
    };
    let layout = placement.layout(pixels(2, 1));
    assert_eq!(placement.row, 1);
    assert!((layout.source_origin[1] - 6.0).abs() < 1e-9, "{layout:?}");
}

#[test]
fn kitty_top_aligned_partial_scroll_clips_images_while_retaining_text_history() {
    let mut session = session_with_size(
        concat!(
            "\x1b[1;8H\x1b_Ga=T,i=1,f=24,s=1,v=1,C=1,q=2;////\x1b\\\x1b[S",
            "\x1b[1;3r\x1b[HT\x1b[H",
            "\x1b_Ga=T,i=2,f=24,s=1,v=4,C=1,q=2;/wAAAP8AAAD///8A\x1b\\",
            "\x1b[3;4H\x1b_Ga=T,i=3,f=24,s=1,v=4,C=1,q=2;////////////////\x1b\\",
            "\x1b[3;1H\x1bD",
        )
        .as_bytes(),
        PtySize {
            pixel_width: 16,
            pixel_height: 10,
            ..PtySize::new(5, 8)
        },
    );
    session
        .set_history_limits(HistoryLimits {
            max_rows: 4,
            max_bytes: 4096,
        })
        .unwrap();
    session.pump().unwrap();
    let [history, cropped, crossing] = session.screen().graphics_placements() else {
        panic!("Expected history, cropped, and crossing placements");
    };
    assert_eq!(session.screen().history().len(), 2);
    assert_eq!(
        session.screen().view().iter_rows().nth(1).unwrap().text(0),
        Some(CellText::Scalar('T'))
    );
    assert_eq!(history.row, -2);
    let layout = cropped.layout(pixels(2, 2));
    assert_eq!(
        (cropped.row, layout.source_origin[1], layout.size[1]),
        (0, 2.0, 2.0)
    );
    assert_eq!(
        (crossing.row, crossing.layout(pixels(2, 2)).size[1]),
        (2, 4.0)
    );

    session.transport.source = Cursor::new(b"\x1b[S");
    session.pump().unwrap();
    let [history, crossing] = session.screen().graphics_placements() else {
        panic!("Expected the history and crossing placements");
    };
    assert_eq!((history.row, crossing.row), (-3, 2));
}

#[test]
fn kitty_delete_preserves_surviving_references_and_cancels_pending_uploads() {
    let mut session = session(
        concat!(
            "\x1b_Ga=T,i=41,p=5,s=1,v=1,z=-2,C=1,q=1;AAAA/w==\x1b\\",
            "\x1b[1;3H\x1b_Ga=p,i=41,p=6,z=3,C=1,q=1\x1b\\",
            "\x1b_Ga=d,d=Q,x=1,y=1,z=-2\x1b\\"
        )
        .as_bytes(),
    );
    session.pump().unwrap();
    let [remaining] = session.screen().graphics_placements() else {
        panic!("Expected one placement")
    };
    assert_eq!(remaining.placement.id, 6);
    assert!(session.graphics_image(41).is_some());

    session.transport.source = Cursor::new(
        concat!(
            "\x1b_Gi=40,m=1,f=24,s=2,v=1;/wAA\x1b\\",
            "\x1b_Ga=d,d=I,i=41,I=9,p=6,q=1\x1b\\",
            "\x1b_Gm=0;AAD/\x1b\\",
            "\x1b_Ga=d,d=F,i=41,I=9,q=2\x1b\\",
            "\x1b_Ga=d,d=G,i=41\x1b\\\x1b[c",
        )
        .as_bytes(),
    );
    session.pump().unwrap();
    assert_eq!(session.transport.written, b"\x1b");
    session.transport.write_budget = usize::MAX;
    session.pump().unwrap();
    assert_eq!(
        session.transport.written,
        b"\x1b_Gi=41,I=9,p=6;EINVAL:Invalid image data\x1b\\\x1b[?62;22c"
    );
    session.transport.written.clear();
    assert!(session.graphics_image(40).is_none());
    assert!(session.graphics_image(41).is_some());
    assert_eq!(session.screen().graphics_placements().len(), 1);

    session.transport.source = Cursor::new(b"\x1b_Ga=d,d=i,i=41,p=6\x1b\\");
    session.pump().unwrap();
    assert!(session.screen().graphics_placements().is_empty());
    assert!(session.graphics_image(41).is_some());

    session.transport.source = Cursor::new(
        concat!(
            "\x1b_Ga=p,i=41,p=6,C=1,q=1\x1b\\\x1b_Ga=d,d=C\x1b\\",
            "\x1b_Ga=t,i=42,s=1,v=1,q=1;AAAA/w==\x1b\\\x1b_Ga=d,d=R,x=42,y=42\x1b\\",
            "\x1b_Ga=T,i=50,m=1,f=24,s=2,v=1,q=1,C=1;/wAA\x1b\\",
            "\x1b_Ga=d,d=y,y=1\x1b\\\x1b_Gm=0;AAD/\x1b\\"
        )
        .as_bytes(),
    );
    session.pump().unwrap();
    assert!(session.screen().graphics_placements().is_empty());
    for id in [41, 42, 50] {
        assert!(session.graphics_image(id).is_none(), "Image {id}");
    }
    assert!(session.transport.written.is_empty());

    session.transport.source = Cursor::new(
        concat!(
            "\x1b_Ga=T,i=4294967295,C=1,s=1,v=1,q=1;AAAA/w==\x1b\\",
            "\x1b_Ga=d,d=R,y=4294967295\x1b\\",
        )
        .as_bytes(),
    );
    session.pump().unwrap();
    assert!(session.screen().graphics_placements().is_empty());
    assert!(session.graphics_image(u32::MAX).is_none());

    session
        .set_history_limits(HistoryLimits {
            max_rows: 1,
            max_bytes: 4096,
        })
        .unwrap();
    session.transport.source = Cursor::new(
        concat!(
            "\x1b[H\x1b_Ga=T,i=41,p=5,s=1,v=1,C=1,q=2;AP8A/w==\x1b\\",
            "\x1b[3;1H\x1bD\x1b_Ga=p,i=41,p=6,C=1,q=2\x1b\\",
            "\x1b_Ga=d,d=A\x1b\\",
        )
        .as_bytes(),
    );
    session.pump().unwrap();
    let [retained] = session.screen().graphics_placements() else {
        panic!("Expected the placement in scrollback");
    };
    assert_eq!((retained.placement.id, retained.row), (5, -1));
    assert!(session.graphics_image(41).is_some());

    session.transport.source = Cursor::new(b"\x1b_Ga=d,d=I,i=41\x1b\\");
    session.pump().unwrap();
    assert!(session.screen().graphics_placements().is_empty());
    assert!(session.graphics_image(41).is_none());
}

#[test]
fn kitty_numbered_images_use_the_newest_live_id() {
    let mut session = session(
        concat!(
            "\x1b_Gi=1,I=0,s=1,v=1,q=1;AAAA/w==\x1b\\",
            "\x1b_Gi=0,I=13,s=1,v=1;AQID/w==\x1b\\",
            "\x1b_GI=13,s=1,v=1;BAUG/w==\x1b\\",
            "\x1b_Ga=p,i=0,I=13,p=9,C=1\x1b\\"
        )
        .as_bytes(),
    );
    session.transport.write_budget = usize::MAX;
    session.pump().unwrap();
    assert_eq!(
        session.transport.written,
        b"\x1b_Gi=2,I=13;OK\x1b\\\x1b_Gi=3,I=13;OK\x1b\\\x1b_Gi=3,I=13,p=9;OK\x1b\\"
    );
    assert_eq!(
        session.screen().graphics_placements()[0]
            .placement
            .image
            .id(),
        3
    );

    session.transport.source = Cursor::new(
        concat!(
            "\x1b_Ga=p,I=13,p=9,P=900,Q=1,q=1\x1b\\",
            "\x1b_Ga=p,I=13,p=9,P=900,Q=1,q=2\x1b\\",
            "\x1b_Ga=d,d=N,I=13,q=3\x1b\\",
            "\x1b_Ga=d,d=F,i=1,I=13,q=2\x1b\\",
            "\x1b_Ga=q,i=3,I=0,s=1,v=1;/////w==\x1b\\",
            "\x1b_Ga=f,i=0,I=13,f=24,s=1,v=1,q=1;AAD/\x1b\\",
            "\x1b_Ga=a,i=0,I=13,c=2,q=1\x1b\\",
        )
        .as_bytes(),
    );
    session.transport.written.clear();
    session.pump().unwrap();
    assert_eq!(
        session.transport.written,
        concat!(
            "\x1b_GI=13,p=9;ENOPARENT:Parent placement missing\x1b\\",
            "\x1b_GI=13;EINVAL:Invalid image data\x1b\\",
            "\x1b_Gi=3;OK\x1b\\",
        )
        .as_bytes()
    );
    let [placement] = session.screen().graphics_placements() else {
        panic!("Expected the original placement")
    };
    assert_eq!(placement.placement.image.id(), 3);
    assert_eq!(placement.placement.id, 9);
    assert_eq!(
        session
            .graphics_image(3)
            .unwrap()
            .pixels(&mut Vec::new())
            .unwrap(),
        &[0, 0, 255, 255]
    );

    session.transport.source = Cursor::new(b"\x1b_Ga=a,i=3,I=0,c=1,q=1\x1b\\");
    session.pump().unwrap();
    assert_eq!(
        session
            .graphics_image(3)
            .unwrap()
            .pixels(&mut Vec::new())
            .unwrap(),
        &[4, 5, 6, 255]
    );

    session.transport.source = Cursor::new(
        concat!(
            "\x1b_Ga=d,d=I,i=1,I=0\x1b\\",
            "\x1b_Ga=d,d=N,i=0,I=13\x1b\\\x1b_Ga=p,i=0,I=13,p=10,C=1\x1b\\",
            "\x1b_Ga=p,i=2,I=13,p=1,C=1\x1b\\",
        )
        .as_bytes(),
    );
    session.pump().unwrap();
    let [placement] = session.screen().graphics_placements() else {
        panic!("Expected one placement")
    };
    assert_eq!(placement.placement.image.id(), 2);
    assert_eq!(
        session
            .graphics_image(2)
            .unwrap()
            .pixels(&mut Vec::new())
            .unwrap(),
        &[1, 2, 3, 255]
    );
    assert!(session.graphics_image(3).is_none());
    assert!(session.graphics_image(1).is_none());
    assert!(session.transport.written.ends_with(
        b"\x1b_Gi=2,I=13,p=10;OK\x1b\\\x1b_Gi=2,I=13,p=1;EINVAL:Invalid image data\x1b\\"
    ));

    session.transport.source = Cursor::new(b"\x1b_Ga=T,i=0,I=0,s=1,v=1,C=1;/////w==\x1b\\");
    session.pump().unwrap();
    assert_eq!(session.screen().graphics_placements().len(), 2);
    assert!(
        session.screen().graphics_placements()[0]
            .anonymous_image()
            .is_some()
    );
    session.transport.source = Cursor::new(b"\x1b_Ga=d,d=A,i=0,I=0\x1b\\");
    session.pump().unwrap();
    assert!(session.screen().graphics_placements().is_empty());
}

#[test]
fn kitty_animation_replies_identify_requests_and_transferred_frames() {
    let mut session = session(
        concat!(
            "\x1b_GI=17,f=24,s=1,v=1;/wAA\x1b\\",
            "\x1b_GI=17,f=24,s=1,v=1;AAD/\x1b\\",
            "\x1b_Ga=f,I=17,p=21,f=24,s=1,v=1,m=1;AP8A\x1b\\",
            "\x1b_Ga=q,i=41,e=c,r=1,c=2,w=3,X=1,C=1,U=1,m=1;%ignored\x1b\\",
            "\x1b_Ga=q,i=42,e=T,U=1,q=1;%ignored\x1b\\",
            "\x1b_Ga=f,m=0;\x1b\\",
            "\x1b_Ga=f,I=17,r=999,f=32,s=1,v=1,m=1;AAAA\x1b\\",
            "\x1b_Ga=f,m=0;\x1b\\",
            "\x1b_Ga=f,I=17,r=999,f=24,s=10001,v=1;AA==\x1b\\",
            "\x1b_Ga=f,I=17,r=3,c=99,f=24,s=1,v=1;AAAA\x1b\\",
            "\x1b_Ga=f,i=99,r=123,f=24,s=1,v=1;AAAA\x1b\\",
            "\x1b_Ga=f,i=99,I=17,r=4\x1b\\",
            "\x1b_Ga=c,I=17,r=2,c=1\x1b\\",
            "\x1b_Ga=c,i=99,r=4,c=1\x1b\\",
            "\x1b_Ga=a,i=99,r=4,q=1\x1b\\",
            "\x1b_Ga=a,I=99,r=4,q=1\x1b\\",
            "\x1b_Ga=a,I=99,r=4,q=2\x1b\\",
            "\x1b_Ga=a,i=99,I=17,r=4\x1b\\",
            "\x1b_Ga=a,I=17,r=1\x1b\\",
            "\x1b_Ga=f,i=2,p=22,r=3,f=24,s=1,v=1;////\x1b\\",
        )
        .as_bytes(),
    );
    session.transport.write_budget = usize::MAX;
    session.pump().unwrap();
    assert_eq!(
        session.transport.written,
        concat!(
            "\x1b_Gi=1,I=17;OK\x1b\\",
            "\x1b_Gi=2,I=17;OK\x1b\\",
            "\x1b_Gi=41,e=c,r=1,c=2,w=3,X=1,C=1;OK\x1b\\",
            "\x1b_GI=17,r=2;OK\x1b\\",
            "\x1b_GI=17,r=3;ENODATA:Insufficient image data\x1b\\",
            "\x1b_GI=17,r=3;EINVAL:Invalid image data\x1b\\",
            "\x1b_GI=17,r=3;EINVAL:Animation base frame not found\x1b\\",
            "\x1b_Gi=99,r=123;ENOENT:Image not found\x1b\\",
            "\x1b_Gi=99,I=17,r=4;EINVAL:Invalid image data\x1b\\",
            "\x1b_GI=17;OK\x1b\\",
            "\x1b_Gi=99;ENOENT:Image not found\x1b\\",
            "\x1b_Gi=99,r=4;ENOENT:Image not found\x1b\\",
            "\x1b_GI=99,r=4;ENOENT:Image not found\x1b\\",
            "\x1b_Gi=99,I=17,r=4;EINVAL:Invalid image data\x1b\\",
            "\x1b_Gi=2,p=22,r=3;OK\x1b\\",
        )
        .as_bytes()
    );
    assert_eq!(
        session
            .graphics_image(2)
            .unwrap()
            .pixels(&mut Vec::new())
            .unwrap(),
        &[0, 255, 0, 255]
    );
}

#[test]
fn kitty_relative_placements_follow_parent_replacement_and_release_descendants() {
    let mut session = session(
        concat!(
            "\x1b[2;3H\x1b_Ga=T,i=20,p=1,s=1,v=1,C=1,q=1;AAAA/w==\x1b\\",
            "\x1b_Ga=T,i=21,p=2,P=20,Q=1,H=-1,V=1,s=1,v=1,q=1;AAAA/w==\x1b\\",
            "\x1b_Ga=T,i=22,p=3,P=21,Q=2,H=2,V=-1,s=1,v=1,q=1;AAAA/w==\x1b\\",
            "\x1b_Ga=p,i=20,p=1,P=22,Q=3\x1b\\",
            "\x1b_Ga=p,i=21,p=2,P=900,Q=1\x1b\\"
        )
        .as_bytes(),
    );
    session.transport.write_budget = usize::MAX;
    session.pump().unwrap();
    let cursor = session.screen().cursor();
    assert_eq!((cursor.column, cursor.row), (2, 1));
    assert_eq!(session.transport.written, b"\x1b_Gi=20,p=1;ECYCLE:Placement cycle\x1b\\\x1b_Gi=21,p=2;ENOPARENT:Parent placement missing\x1b\\");
    let points = |session: &Session<Transport>| {
        session
            .screen()
            .graphics_placements()
            .iter()
            .map(|p| (p.column, p.row))
            .collect::<Vec<_>>()
    };
    assert_eq!(points(&session), [(2, 1), (1, 2), (3, 1)]);
    let root_serial = session.screen().graphics_placements()[0].serial();

    session.transport.source = Cursor::new(b"\x1b[3;5H\x1b_Ga=p,i=20,p=1,C=1,q=1\x1b\\\x1b[S");
    session.pump().unwrap();
    assert_eq!(points(&session), [(4, 1), (3, 2), (5, 1)]);

    session.transport.source = Cursor::new(
        concat!(
            "\x1b_Ga=T,i=25,p=5,P=20,Q=1,H=1,s=1,v=1,q=1;AAAA/w==\x1b\\",
            "\x1b_Ga=p,i=21,p=2,P=20,Q=1,V=1,q=1\x1b\\",
        )
        .as_bytes(),
    );
    session.pump().unwrap();
    assert_eq!(points(&session), [(4, 1), (4, 2), (6, 1), (5, 1)]);

    session.transport.source = Cursor::new(b"\x1b[2;6H\x1b_Ga=p,i=20,p=1,C=1,q=1\x1b\\");
    session.pump().unwrap();
    assert_eq!(points(&session), [(5, 1), (5, 2), (7, 1), (6, 1)]);

    session.transport.source = Cursor::new(b"\x1b_Ga=p,i=20,p=1,z=2,C=1,q=1\x1b\\");
    session.pump().unwrap();
    assert_eq!(points(&session), [(5, 2), (7, 1), (6, 1), (5, 1)]);
    assert_eq!(
        session.screen().graphics_placements()[3].serial(),
        root_serial
    );
    assert_eq!(session.screen().graphics_placements()[3].placement.z, 2);

    session.transport.source = Cursor::new(b"\x1b[2;7H\x1b_Ga=p,i=20,p=1,z=2,C=1,q=1\x1b\\");
    session.pump().unwrap();
    assert_eq!(points(&session), [(6, 2), (8, 1), (7, 1), (6, 1)]);

    session.transport.source = Cursor::new(b"\x1b_Ga=p,i=21,p=2,P=20,Q=1,H=-3,V=1,z=3,q=1\x1b\\");
    session.pump().unwrap();
    assert_eq!(points(&session), [(5, 1), (7, 1), (6, 1), (3, 2)]);

    session.transport.source = Cursor::new(
        concat!(
            "\x1b[3;7H\x1b_Ga=T,i=23,s=1,v=1,C=1,q=1;AAAA/w==\x1b\\",
            "\x1b_Ga=T,i=24,P=23,H=-1,s=1,v=1,q=1;AAAA/w==\x1b\\",
            "\x1b_Ga=d,d=i,i=20,p=1\x1b\\\x1b[S",
        )
        .as_bytes(),
    );
    session.pump().unwrap();
    assert_eq!(points(&session), [(6, 1), (5, 1)]);
    assert!(session.graphics_image(20).is_some());
    assert!(session.graphics_image(21).is_none());
    assert!(session.graphics_image(22).is_none());
    assert!(session.graphics_image(25).is_none());

    session.transport.source =
        Cursor::new(b"\x1b[3;2H\x1b_Ga=T,i=19,s=1,v=1,C=1,q=1;AAAA/w==\x1b\\\x1b[S");
    session.pump().unwrap();
    assert_eq!(points(&session), [(1, 1), (6, 0), (5, 0)]);
}

#[test]
fn kitty_relative_depth_limit_keeps_the_existing_chain() {
    use std::fmt::Write as _;

    let mut session = session(b"");
    let mut wire = String::from("\x1b_Ga=T,i=1,p=1,s=1,v=1,q=1,C=1;AAAA/w==\x1b\\");
    for id in 2..=17 {
        write!(
            wire,
            "\x1b_Ga=T,i={id},p=1,P={},Q=1,H=1,s=1,v=1,q=1;AAAA/w==\x1b\\",
            id - 1
        )
        .unwrap();
    }
    wire.push_str("\x1b_Ga=T,i=18,p=1,P=17,Q=1,H=1,s=1,v=1;AAAA/w==\x1b\\");
    session
        .processor
        .advance(wire.as_bytes(), &mut session.screen, &mut session.writer)
        .unwrap();
    assert_eq!(
        session.processor.reply_bytes(),
        b"\x1b_Gi=18,p=1;ETOODEEP:Placement chain too deep\x1b\\"
    );
    let placements = session.screen().graphics_placements();
    assert_eq!(placements.len(), 17);
    assert_eq!(placements[16].column, 16);
    assert!(session.graphics_image(18).is_none());

    let moved = b"\x1b[2;2H\x1b_Ga=p,i=1,p=1,C=1,q=2\x1b\\";
    session
        .processor
        .advance(moved, &mut session.screen, &mut session.writer)
        .unwrap();
    for (index, placement) in session.screen().graphics_placements().iter().enumerate() {
        assert_eq!((placement.column, placement.row), (index as i64 + 1, 1));
    }
}

#[cfg(feature = "wgpu-renderer")]
#[test]
#[ignore = "Requires a Vulkan adapter"]
fn kitty_images_compose_in_an_offscreen_wgpu_pass() {
    use std::sync::mpsc;
    use std::time::Duration;

    let _gpu_guard = OFFSCREEN_GPU_TESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let size = PtySize {
        pixel_width: 16,
        pixel_height: 10,
        ..PtySize::new(5, 8)
    };
    let mut session = session_with_size(
        concat!(
            "\x1b[2;2H\x1b_Ga=T,i=8,s=1,v=1,C=1,q=1;AAD/gA==\x1b\\",
            "\x1b_Ga=T,i=7,s=2,v=2,C=1,q=1;/wAA/wD/AIAAAP9A////AA==\x1b\\",
            "\x1b[1;8H\x1b_Ga=p,i=7,p=2,x=0,y=1,w=2,h=1,X=1,C=1,q=1\x1b\\",
            "\x1b[1;1H\x1b_Ga=T,f=100,z=-1,C=1,m=1;iVBORw0KGgoAAAAN\x1b\\",
            "\x1b_Gm=0;SUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==\x1b\\",
            "\x1b[4;5r\x1b[4;3H\x1b_Ga=T,i=10,s=1,v=4,C=1,q=1;/wAA//8AAP8A/wD/AAD//w==\x1b\\\x1b[S",
            "\x1b]4;3;#ff0000\x07\x1b]11;#0000ff\x07",
            "\x1b[1;5H\x1b_Ga=T,i=11,f=8,s=1,v=1,C=1,q=1;Aw==\x1b\\",
            "\x1b[1;6H\x1b_Ga=T,i=12,f=16,s=2,v=1,C=1,q=1;A4D4/w==\x1b\\",
            "\x1b_Ga=T,i=33554474,p=9,U=1,c=2,r=1,s=2,v=1,q=1;//8A/wD///8=\x1b\\",
            "\x1b_Ga=T,i=13,p=1,P=33554474,Q=9,H=3,s=1,v=1,q=1;/wD//w==\x1b\\",
            "\x1b[5;1H\x1b[38:2:0:0:42;58;5;9m\u{10eeee}\u{305}\u{305}\u{30e}\u{10eeee}\x1b[0m"
        )
        .as_bytes(),
        size,
    );
    session.writer = UnicodeWriter::new(lune_terminal::WidthPolicy::Wide);
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    assert!(
        frame
            .row_runs(4, 0..2)
            .unwrap()
            .all(|run| run.unwrap().units().next().is_none())
    );

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .expect("A Vulkan adapter is required");
    let info = adapter.get_info();
    eprintln!(
        "Offscreen adapter: {:?} {:?}",
        info.backend, info.device_type
    );
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        required_limits: wgpu::Limits {
            max_texture_dimension_2d: 128,
            ..Default::default()
        },
        ..Default::default()
    }))
    .unwrap();
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let mut renderer = crate::ImageRenderer::new(&device, format);
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    let now = Instant::now();
    let mut tracker = PresentationTracker::default();
    tracker.prepare(&frame, now).unwrap().commit();
    let graphic_stamp = frame.graphics_stamp();
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d {
            width: size.pixel_width.into(),
            height: size.pixel_height.into(),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });

    let stride = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(stride) * u64::from(size.pixel_height),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let read_pass = |draw: &mut dyn FnMut(&mut wgpu::RenderPass<'_>)| {
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            draw(&mut pass);
        }
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride),
                    rows_per_image: None,
                },
            },
            texture.size(),
        );
        let submission = queue.submit([encoder.finish()]);
        let slice = buffer.slice(..);
        let (send, receive) = mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            send.send(result).unwrap();
        });
        let timeout = Duration::from_secs(5);
        device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(timeout),
            })
            .unwrap();
        receive.recv_timeout(timeout).unwrap().unwrap();

        let mapped = slice.get_mapped_range().unwrap().to_vec();
        buffer.unmap();
        mapped
    };
    let read_frame = |renderer: &crate::ImageRenderer| {
        read_pass(&mut |pass| {
            renderer.draw(pass, crate::GraphicsLayer::BehindBackground);
            renderer.draw(pass, crate::GraphicsLayer::BehindText);
            renderer.draw(pass, crate::GraphicsLayer::AboveText);
        })
    };
    let read_layers = |images: &crate::ImageRenderer,
                       backgrounds: &crate::CellBackgroundRenderer| {
        read_pass(&mut |pass| images.draw_frame(pass, backgrounds, |_| {}))
    };
    let pixel = |mapped: &[u8], x: usize, y: usize| {
        let start = y * stride as usize + x * 4;
        <[u8; 4]>::try_from(&mapped[start..start + 4]).unwrap()
    };
    let before = read_frame(&renderer);
    for ((x, y), expected) in [
        ((0, 0), [255, 0, 0, 255]),
        ((14, 0), [0, 0, 0, 0]),
        ((15, 0), [0, 0, 137, 64]),
        ((2, 2), [187, 0, 188, 255]),
        ((3, 2), [0, 188, 0, 128]),
        ((2, 3), [0, 0, 137, 64]),
        ((3, 3), [0, 0, 0, 0]),
        ((4, 5), [0, 0, 0, 0]),
        ((4, 6), [0, 255, 0, 255]),
        ((4, 7), [0, 0, 255, 255]),
        ((8, 0), [255, 0, 0, 255]),
        ((10, 0), [188, 0, 0, 128]),
        ((11, 0), [0, 0, 255, 255]),
        ((0, 8), [255, 255, 0, 255]),
        ((3, 8), [0, 255, 255, 255]),
        ((6, 8), [255, 0, 255, 255]),
    ] {
        let actual = pixel(&before, x, y);
        assert!(
            actual
                .into_iter()
                .zip(expected)
                .all(|(actual, expected)| actual.abs_diff(expected) <= 1),
            "({x}, {y}): {actual:?} != {expected:?}"
        );
    }

    session.transport.source = Cursor::new(concat!(
        "\x1b]4;3;#00ff00\x07\x1b]11;#ff0000\x07",
        "\x1b[5;1H\x1b[2X\x1b[5;3H\x1b[38;5;42;58;5;9m\u{10eeee}\u{305}\u{305}\u{30e}\u{10eeee}\x1b[0m"
    ).as_bytes());
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    assert_eq!(frame.graphics_stamp(), graphic_stamp);
    let prepared = tracker.prepare(&frame, now).unwrap();
    assert!(prepared.damage().graphics_changed);
    prepared.commit();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    let after = read_frame(&renderer);
    assert_eq!(pixel(&after, 8, 0), [0, 255, 0, 255]);
    let [r, g, b, a] = pixel(&after, 10, 0);
    assert_eq!([r, b, a], [0, 0, 128]);
    assert!(g.abs_diff(188) <= 1);
    assert_eq!(pixel(&after, 11, 0), [255, 0, 0, 255]);
    assert_eq!(pixel(&after, 2, 2), pixel(&before, 2, 2));
    assert_eq!(pixel(&after, 0, 8), [0, 0, 0, 0]);
    assert_eq!(pixel(&after, 4, 8), [255, 255, 0, 255]);
    assert_eq!(pixel(&after, 7, 8), [0, 255, 255, 255]);
    assert_eq!(pixel(&after, 10, 8), [255, 0, 255, 255]);
    let animation_key = frame
        .graphics_placements()
        .iter()
        .find(|placement| placement.placement.image.id() == 33554474)
        .unwrap()
        .placement
        .image;
    let animation_texture = renderer.cached_texture(animation_key).unwrap().clone();

    session.transport.source =
        Cursor::new(b"\x1b_Ga=d,d=p,x=4,y=5\x1b\\\x1b_Ga=d,d=q,x=6,y=5,z=8\x1b\\");
    session.pump().unwrap();
    assert!(session.graphics_image(13).is_some());

    session.transport.source =
        Cursor::new(b"\x1b_Ga=f,i=33554474,c=1,x=1,s=1,v=1,X=1,q=1;AP8A/w==\x1b\\");
    session.pump().unwrap();
    assert_eq!(
        session.presentation_frame().unwrap().graphics_stamp(),
        graphic_stamp
    );
    session.transport.source = Cursor::new(b"\x1b_Ga=a,i=33554474,s=2,r=1,z=1\x1b\\");
    session.pump_with_clock(|| now).unwrap();
    let next = now + Duration::from_millis(1);
    assert_eq!(session.presentation_deadline(), Some(next));
    session.pump_with_clock(|| next).unwrap();
    let frame = session.presentation_frame().unwrap();
    assert_ne!(frame.graphics_stamp(), graphic_stamp);
    let prepared = tracker.prepare(&frame, now).unwrap();
    assert_eq!(prepared.damage().rows, RowDamage::Rows(&[]));
    assert!(prepared.damage().graphics_changed);
    prepared.commit();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    assert_eq!(
        renderer.cached_texture(animation_key),
        Some(&animation_texture)
    );
    let selected = read_frame(&renderer);
    assert_eq!(pixel(&selected, 4, 8), [255, 255, 0, 255]);
    assert_eq!(pixel(&selected, 7, 8), [0, 255, 0, 255]);
    assert_eq!(pixel(&selected, 10, 8), [255, 0, 255, 255]);

    session.transport.source =
        Cursor::new(b"\x1b_Ga=c,i=33554474,r=2,c=2,X=1,w=1,h=1,C=1,q=1\x1b\\");
    session.pump().unwrap();
    assert_eq!(
        renderer.prepare(&device, &queue, &session.presentation_frame().unwrap()),
        0
    );
    let composed = read_frame(&renderer);
    assert_eq!(pixel(&composed, 4, 8), [0, 255, 0, 255]);
    assert_eq!(pixel(&composed, 6, 8), [0, 255, 0, 255]);

    for (command, color) in [
        (
            b"\x1b_Ga=a,i=33554474,c=1,q=1\x1b\\".as_slice(),
            [255, 255, 0, 255],
        ),
        (b"\x1b_Ga=a,i=33554474,c=2,q=1\x1b\\", [0, 255, 0, 255]),
    ] {
        session.transport.source = Cursor::new(command);
        session.pump_with_clock(|| next).unwrap();
        let frame = session.presentation_frame().unwrap();
        assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
        assert_eq!(
            renderer.cached_texture(animation_key),
            Some(&animation_texture)
        );
        assert_eq!(pixel(&read_frame(&renderer), 4, 8), color);
    }

    session.transport.source = Cursor::new(b"\x1b_Ga=d,d=Q,x=6,y=5,z=0\x1b\\");
    session.pump().unwrap();
    assert!(session.graphics_image(13).is_none());
    let frame = session.presentation_frame().unwrap();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    let deleted_child = read_frame(&renderer);
    assert_eq!(pixel(&deleted_child, 4, 8), [0, 255, 0, 255]);
    assert_eq!(pixel(&deleted_child, 6, 8), [0, 255, 0, 255]);
    assert_eq!(pixel(&deleted_child, 10, 8), [0, 0, 0, 0]);

    session.transport.source =
        Cursor::new(b"\x1b_Ga=T,i=13,p=1,P=33554474,Q=9,H=3,s=1,v=1,q=1;/wD//w==\x1b\\\x1b_Ga=d,d=f,i=33554474,r=1\x1b\\\x1b_Ga=a,i=33554474,c=1\x1b\\");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    assert_eq!(pixel(&read_frame(&renderer), 4, 8), [0, 255, 0, 255]);

    session.transport.source = Cursor::new(b"\x1b_Ga=d,d=F,i=33554474,r=1\x1b\\");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    assert!(!renderer.has_cached_image(animation_key));
    let deleted = read_frame(&renderer);
    assert_eq!(pixel(&deleted, 4, 8), [0, 0, 0, 0]);
    assert_eq!(pixel(&deleted, 10, 8), [0, 0, 0, 0]);

    let cached = frame
        .graphics_placements()
        .iter()
        .find(|placement| placement.placement.image.id() == 11)
        .unwrap()
        .placement
        .image;
    session.transport.source = Cursor::new(b"\x1b_Ga=d,d=i,i=11\x1b\\");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    let stamp = frame.graphics_stamp();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    assert!(renderer.has_cached_image(cached));

    session.transport.source = Cursor::new(b"\x1b_Ga=d,d=I,i=11\x1b\\");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    assert_eq!(frame.graphics_stamp(), stamp);
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    assert!(!renderer.has_cached_image(cached));

    session.transport.source = Cursor::new(
        concat!(
            "\x1bc\x1b[1;5H\x1b_Ga=T,i=31,p=1,s=1,v=1,C=1,q=1;/wAA/w==\x1b\\",
            "\x1b_Ga=T,i=32,p=1,P=31,Q=1,H=1,V=1,s=1,v=1,q=1;/wD//w==\x1b\\",
            "\x1b[1;1H",
        )
        .as_bytes(),
    );
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    let before = read_frame(&renderer);
    assert_eq!(pixel(&before, 8, 0), [255, 0, 0, 255]);
    assert_eq!(pixel(&before, 10, 2), [255, 0, 255, 255]);
    let resized = PtySize { columns: 4, ..size };
    session.screen.reflow(4, 5, StyleId::default()).unwrap();
    session.geometry = GridGeometry::from_pty_size(resized);
    session.processor.update_geometry(
        &mut session.screen,
        session.geometry.text_area_pixels(),
        session.geometry.cell_pixels(),
    );
    let frame = session.presentation_frame().unwrap();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    let reflowed = read_frame(&renderer);
    assert_eq!(pixel(&reflowed, 0, 2), [255, 0, 0, 255]);
    assert_eq!(pixel(&reflowed, 4, 4), [255, 0, 255, 255]);
    assert_eq!(pixel(&reflowed, 8, 0), [0, 0, 0, 0]);

    session.transport.source = Cursor::new(
        concat!(
            "\x1b_Ga=T,i=42,U=1,c=1,r=1,s=1,v=1,q=1;/////w==\x1b\\",
            "\x1b_Ga=T,i=43,P=42,H=1,s=1,v=1,q=1;/wAA/w==\x1b\\",
            "\x1b[5;1H\x1b[38;5;42m\u{10eeee}\x1b[0m",
        )
        .as_bytes(),
    );
    session.pump().unwrap();
    assert_eq!(
        renderer.prepare(&device, &queue, &session.presentation_frame().unwrap()),
        0
    );
    assert_eq!(pixel(&read_frame(&renderer), 4, 8), [255, 0, 0, 255]);
    session.transport.source =
        Cursor::new("\x1b[2J\x1b[5;1H\x1b[38;5;42m\u{10eeee}\x1b[0m".as_bytes());
    session.pump().unwrap();
    assert_eq!(
        renderer.prepare(&device, &queue, &session.presentation_frame().unwrap()),
        0
    );
    let cleared = read_frame(&renderer);
    assert_eq!(pixel(&cleared, 1, 8), [255, 255, 255, 255]);
    assert_eq!(pixel(&cleared, 4, 8), [0, 0, 0, 0]);

    let rgb = |[red, green, blue]: [u8; 3]| Rgb { red, green, blue };
    let red = [255, 0, 0];
    let green = [0, 255, 0];
    let blue = [0, 0, 255];
    let yellow = [255, 255, 0];
    let magenta = [255, 0, 255];
    let cyan = [0, 255, 255];
    let mut theme = Palette::new([Rgb::default(); 256], rgb(green), rgb(blue), rgb(red));
    theme.set_graphics_colors(lune_terminal::GraphicsColors {
        selection_foreground: Some(rgb(magenta)),
        cursor_depends_on_cell: true,
        cursor_text: Some(rgb(yellow)),
        url_hover: Some(rgb(cyan)),
        ..Default::default()
    });
    session.set_theme(&theme, None);
    session.transport.source = Cursor::new(
        b"\x1bc\x1b[38;2;255;255;255;48;2;255;0;0mXX\x1b[0m\x1b[H\x1b_Ga=T,i=44,f=8,s=8,v=1,C=1,q=1;+Pn6+/z9/v8=\x1b\\",
    );
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    let stamp = frame.graphics_stamp();
    tracker.prepare(&frame, now).unwrap().commit();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);

    let assert_roles = |renderer: &crate::ImageRenderer, cursor, cursor_text| {
        let mapped = read_frame(renderer);
        for (x, [r, g, b]) in [
            blue,
            green,
            green,
            magenta,
            cursor,
            cursor_text,
            cyan,
            green,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                pixel(&mapped, x, 0),
                [r, g, b, 255],
                "Palette index {}",
                248 + x
            );
        }
    };
    assert_roles(&renderer, green, blue);
    for (command, cursor, cursor_text) in [
        (b"\x1b]12;#ff0000\x07".as_slice(), red, yellow),
        (b"\x1b]112\x07", green, blue),
    ] {
        session.transport.source = Cursor::new(command);
        session.pump().unwrap();
        let frame = session.presentation_frame().unwrap();
        assert_eq!(frame.graphics_stamp(), stamp);
        let prepared = tracker.prepare(&frame, now).unwrap();
        assert_eq!(prepared.damage().rows, RowDamage::Rows(&[]));
        assert!(prepared.damage().graphics_changed);
        prepared.commit();
        assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
        assert_roles(&renderer, cursor, cursor_text);
    }

    session.transport.source = Cursor::new(
        concat!(
            "\x1bc\x1b[1;2H\x1b[48;2;0;0;255m \x1b[48;2;0;128;64m  \x1b[0m",
            "\x1b[1;1H\x1b_Ga=T,i=45,z=-1073741825,s=1,v=1,C=1,q=1;/wAA/w==\x1b\\",
            "\x1b[1;2H\x1b_Ga=p,i=45,z=-1073741825,C=1,q=1\x1b\\",
            "\x1b[1;3H\x1b_Ga=p,i=45,z=-1073741825,C=1,q=1\x1b\\",
            "\x1b[1;4H\x1b_Ga=p,i=45,z=-1073741824,C=1,q=1\x1b\\",
            "\x1b_Ga=T,i=46,U=1,z=-2147483648,c=1,r=1,s=1,v=1,q=1;/wAA/w==\x1b\\",
            "\x1b_Ga=T,i=48,U=1,z=9,c=1,r=1,s=1,v=1,q=1;/wAA/w==\x1b\\",
            "\x1b[2;1H\x1b[48;2;0;255;0;38;5;46m\u{10eeee}\x1b[0m",
            "\x1b[38;5;48m\u{10eeee}\x1b[0m",
            "\x1b[2;2H\x1b_Ga=T,i=47,z=0,X=1,s=1,v=1,C=1,q=1;AAD/gA==\x1b\\",
        )
        .as_bytes(),
    );
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    let stamp = frame.graphics_stamp();
    let mut backgrounds = crate::CellBackgroundRenderer::new(&device, format);
    backgrounds.prepare(&device, &queue, &frame).unwrap();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    let composed = read_layers(&renderer, &backgrounds);
    assert_eq!(pixel(&composed, 0, 0), [255, 0, 0, 255]);
    assert_eq!(pixel(&composed, 4, 0), [255, 0, 0, 255]);
    assert_eq!(pixel(&composed, 8, 0), [0, 128, 64, 255]);
    assert_eq!(pixel(&composed, 12, 0), [255, 0, 0, 255]);
    assert_eq!(pixel(&composed, 13, 0), [0, 128, 64, 255]);
    assert_eq!(pixel(&composed, 1, 2), [255, 0, 0, 255]);
    assert_eq!(pixel(&composed, 6, 2), [255, 0, 0, 255]);
    assert!(
        pixel(&composed, 5, 2)
            .into_iter()
            .zip([187, 0, 188, 255])
            .all(|(actual, expected)| actual.abs_diff(expected) <= 1)
    );

    let mut foreground_session =
        session_with_size(b"\x1b[48;2;255;255;255m \x1b[1;7H \x1b[2;3H ", size);
    foreground_session.pump().unwrap();
    let mut foreground = crate::CellBackgroundRenderer::new(&device, format);
    foreground
        .prepare(
            &device,
            &queue,
            &foreground_session.presentation_frame().unwrap(),
        )
        .unwrap();
    let with_foreground = read_pass(&mut |pass| {
        renderer.draw_frame(pass, &backgrounds, |pass| foreground.draw(pass));
    });
    assert_eq!(pixel(&with_foreground, 0, 0), [255; 4]);
    assert_eq!(pixel(&with_foreground, 12, 0), [255; 4]);
    assert!(
        pixel(&with_foreground, 5, 2)
            .into_iter()
            .zip([187, 187, 255, 255])
            .all(|(actual, expected)| actual.abs_diff(expected) <= 1)
    );

    session.transport.source = Cursor::new(b"\x1b]11;#008040\x07");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    assert_eq!(frame.graphics_stamp(), stamp);
    backgrounds.prepare(&device, &queue, &frame).unwrap();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    let recolored = read_layers(&renderer, &backgrounds);
    assert_eq!(pixel(&recolored, 4, 0), [0, 0, 255, 255]);
    assert_eq!(pixel(&recolored, 8, 0), [255, 0, 0, 255]);
    assert_eq!(pixel(&recolored, 13, 0), [0, 0, 0, 0]);

    session.transport.source = Cursor::new(b"\x1b[1;2H\x1b[X");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    assert_eq!(frame.graphics_stamp(), stamp);
    backgrounds.prepare(&device, &queue, &frame).unwrap();
    let erased = read_layers(&renderer, &backgrounds);
    assert_eq!(pixel(&erased, 4, 0), [255, 0, 0, 255]);

    session.transport.source = Cursor::new(
        concat!(
            "\x1bc\x1b[2;4r",
            "\x1b[1;1H\x1b_Ga=T,i=1,s=1,v=4,C=1,q=2;/wAA//8AAP8A/wD/AAD//w==\x1b\\",
            "\x1b[3;2H\x1b_Ga=T,i=2,f=24,s=1,v=2,c=1,r=2,C=1,q=2;AP8AAAD/\x1b\\",
            "\x1b[5;3H\x1b_Ga=T,i=3,s=1,v=1,C=1,q=2;/wAA/w==\x1b\\",
            "\x1b[2;1H\x1bM",
        )
        .as_bytes(),
    );
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    let reversed = read_frame(&renderer);
    assert_eq!(pixel(&reversed, 0, 0), [255, 0, 0, 255]);
    assert_eq!(pixel(&reversed, 0, 2), [0, 255, 0, 255]);
    assert_eq!(pixel(&reversed, 5, 6), [0, 255, 0, 255]);
    assert_eq!(pixel(&reversed, 5, 8), [0, 0, 0, 0]);
    assert_eq!(pixel(&reversed, 8, 8), [255, 0, 0, 255]);

    session.transport.source = Cursor::new(b"\x1b[4;1H\x1bD");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    let forwarded = read_frame(&renderer);
    assert_eq!(pixel(&forwarded, 0, 2), pixel(&reversed, 0, 2));
    assert_eq!(pixel(&forwarded, 5, 4), [0, 255, 0, 255]);
    assert_eq!(pixel(&forwarded, 5, 6), [0, 0, 0, 0]);
    assert_eq!(pixel(&forwarded, 8, 8), pixel(&reversed, 8, 8));

    session
        .set_history_limits(HistoryLimits {
            max_rows: 2,
            max_bytes: 4096,
        })
        .unwrap();
    session.transport.source = Cursor::new(
        concat!(
            "\x1bc\x1b_Ga=T,i=42,U=1,c=1,r=1,s=1,v=1,q=2;AP8A/w==\x1b\\",
            "\x1b_Ga=T,i=43,P=42,H=1,V=1,s=1,v=1,q=2;/wD//w==\x1b\\",
            "\x1b[1;1H\x1b[38;5;42m\u{10eeee}\x1b[0m\x1b[5;1H\x1bD",
        )
        .as_bytes(),
    );
    session.pump().unwrap();
    assert_eq!(session.screen().history().len(), 1);
    let frame = session.presentation_frame().unwrap();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    let live = read_frame(&renderer);
    assert_eq!(pixel(&live, 1, 0), [0; 4]);
    assert_eq!(pixel(&live, 4, 0), [255, 0, 255, 255]);

    assert!(session.scroll_history(1));
    let frame = session.presentation_frame().unwrap();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    let scrolled = read_frame(&renderer);
    assert_eq!(pixel(&scrolled, 1, 0), [0, 255, 0, 255]);
    assert_eq!(pixel(&scrolled, 4, 0), [0, 0, 0, 0]);
    assert_eq!(pixel(&scrolled, 4, 2), [255, 0, 255, 255]);

    session.screen.scroll_viewport_to_bottom();
    session.transport.source = Cursor::new(
        b"\x1b_Ga=d,d=P,x=2,y=1\x1b\\\x1b_Ga=T,i=44,P=42,H=1,V=1,s=1,v=1,q=2;/wD//w==\x1b\\",
    );
    session.pump().unwrap();
    assert!(session.graphics_image(43).is_none());
    let frame = session.presentation_frame().unwrap();
    tracker.prepare(&frame, now).unwrap().commit();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    assert_eq!(pixel(&read_frame(&renderer), 4, 0), [255, 0, 255, 255]);

    session.transport.source = Cursor::new(b"\x1b[3J");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    let prepared = tracker.prepare(&frame, now).unwrap();
    assert_eq!(prepared.damage().rows, RowDamage::Rows(&[]));
    assert!(prepared.damage().graphics_changed);
    prepared.commit();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    assert_eq!(pixel(&read_frame(&renderer), 4, 0), [0, 0, 0, 0]);

    let mut recaptured = session_with_historical_graphics_parent(size);
    recaptured.pump_with_clock(|| now).unwrap();
    assert_eq!(
        renderer.prepare(&device, &queue, &recaptured.presentation_frame().unwrap()),
        0
    );
    assert_eq!(pixel(&read_frame(&renderer), 10, 0), [255, 0, 0, 255]);
    recaptured.transport.source = Cursor::new(RECAPTURE_MOVED_PLACEHOLDER);
    recaptured.pump_with_clock(|| now).unwrap();
    assert_eq!(
        renderer.prepare(&device, &queue, &recaptured.presentation_frame().unwrap()),
        0
    );
    assert_eq!(pixel(&read_frame(&renderer), 10, 0), [0, 0, 0, 0]);

    session.transport.source = Cursor::new(
        concat!(
            "\x1b[5;1H\x1bD\x1b_Ga=d,d=I,i=44\x1b\\",
            "\x1b_Ga=T,i=45,P=42,H=1,V=-1,s=2,v=1,q=2;/wD///8A//8=\x1b\\",
            "\x1b[38;5;42m\u{10eeee}\x1b[0m",
        )
        .as_bytes(),
    );
    session.pump().unwrap();
    assert!(session.scroll_history(1));
    let frame = session.presentation_frame().unwrap();
    let stamp = frame.graphics_stamp();
    tracker.prepare(&frame, now).unwrap().commit();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    assert_eq!(pixel(&read_frame(&renderer), 4, 8), [255, 0, 255, 255]);

    session.transport.source =
        Cursor::new("\x1b[5;1H\x1b[X\x1b[5;3H\x1b[38;5;42m\u{10eeee}\x1b[0m".as_bytes());
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    assert_eq!(frame.graphics_stamp(), stamp);
    let prepared = tracker.prepare(&frame, now).unwrap();
    assert_eq!(prepared.damage().rows, RowDamage::Rows(&[]));
    assert!(prepared.damage().graphics_changed);
    prepared.commit();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    let moved = read_frame(&renderer);
    assert_eq!(pixel(&moved, 4, 8), [0, 0, 0, 0]);
    assert_eq!(pixel(&moved, 12, 8), [255, 0, 255, 255]);

    session.transport.source = Cursor::new(
        concat!(
            "\x1b[5;3H\x1b[X\x1b[5;1H\x1b[38;5;42m\u{10eeee}\x1b[0m",
            "\x1b_Ga=f,i=45,r=1,s=2,v=1,q=2;AAD//wAA//8=\x1b\\",
        )
        .as_bytes(),
    );
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    let moved_and_updated = read_frame(&renderer);
    assert_eq!(pixel(&moved_and_updated, 4, 8), [0, 0, 255, 255]);
    assert_eq!(pixel(&moved_and_updated, 12, 8), [0, 0, 0, 0]);

    let image = frame
        .graphics_placements()
        .iter()
        .find(|placement| placement.placement.image.id() == 45)
        .unwrap()
        .placement
        .image;
    let texture = renderer.cached_texture(image).unwrap().clone();
    session.transport.source =
        Cursor::new(b"\x1b_Ga=f,i=45,r=1,s=2,v=1,X=1,q=2;AP8A/wD/AP8=\x1b\\");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    assert_eq!(renderer.cached_texture(image), Some(&texture));
    assert_eq!(pixel(&read_frame(&renderer), 4, 8), [0, 255, 0, 255]);

    session.transport.source = Cursor::new(
        concat!(
            "\x1b_Ga=f,i=45,s=1,v=1,q=2;AAD//w==\x1b\\",
            "\x1b_Ga=c,i=45,r=2,c=1,x=1,w=1,h=1,C=1,q=2\x1b\\",
        )
        .as_bytes(),
    );
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    assert_eq!(renderer.cached_texture(image), Some(&texture));
    let composed = read_frame(&renderer);
    assert_eq!(
        [pixel(&composed, 4, 8), pixel(&composed, 5, 8)],
        [[0, 255, 0, 255], [0, 0, 255, 255]]
    );

    let previous = session.graphics_image(45).unwrap().pixel_revision();
    session.transport.source = Cursor::new(
        concat!(
            "\x1b_Ga=f,i=45,s=2,v=1,X=1,q=2;AAAA/wAAAP8=\x1b\\",
            "\x1b_Ga=c,i=45,r=1,c=1,X=1,w=1,h=1,C=1,q=2\x1b\\",
            "\x1b_Ga=a,i=45,c=3\x1b\\",
            "\x1b_Ga=c,i=45,r=2,c=3,w=1,h=1,C=1,q=2\x1b\\",
            "\x1b_Ga=c,i=45,r=3,c=1,X=1,w=1,h=1,C=1,q=2\x1b\\",
            "\x1b_Ga=a,i=45,c=1\x1b\\",
        )
        .as_bytes(),
    );
    session.pump().unwrap();
    let [changed, rest] = session
        .graphics_image(45)
        .unwrap()
        .pixel_update_plan(Some(&previous))
        .damage()
        .expect("An interleaved edit preserves the cached frame's damage");
    assert_eq!(
        (changed.origin, changed.size, rest.size),
        ([0, 0], [1, 1], [0, 0])
    );
    let frame = session.presentation_frame().unwrap();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    assert_eq!(renderer.cached_texture(image), Some(&texture));
    let composed = read_frame(&renderer);
    assert_eq!(
        [pixel(&composed, 4, 8), pixel(&composed, 5, 8)],
        [[0, 0, 0, 255], [0, 0, 255, 255]]
    );

    let mut letterbox = session_with_size(
        concat!(
            "\x1b_Ga=T,i=42,U=1,c=3,r=1,s=1,v=1,q=2;AP8A/w==\x1b\\",
            "\x1b_Ga=T,i=43,P=42,H=2,s=1,v=1,q=2;AAD//w==\x1b\\",
            "\x1b[1;1H\x1b[38;5;42m\u{10eeee}\x1b[0m",
        )
        .as_bytes(),
        size,
    );
    letterbox.pump().unwrap();
    let frame = letterbox.presentation_frame().unwrap();
    let stamp = frame.graphics_stamp();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    assert_eq!(renderer.image_instance_count(), 0);
    for (width, expected) in [
        (size.pixel_width / 2, [0, 0, 255, 255]),
        (size.pixel_width, [0; 4]),
    ] {
        letterbox.geometry = GridGeometry::from_pty_size(PtySize {
            pixel_width: width,
            ..size
        });
        letterbox.processor.update_geometry(
            &mut letterbox.screen,
            letterbox.geometry.text_area_pixels(),
            letterbox.geometry.cell_pixels(),
        );
        let frame = letterbox.presentation_frame().unwrap();
        assert_eq!(frame.graphics_stamp(), stamp);
        assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
        assert_eq!(pixel(&read_frame(&renderer), 4, 0), expected);
    }

    for scroll in [b"\x1b[4S".as_slice(), b"\x1b[S\x1b[S\x1b[S\x1b[S"] {
        let mut scrolled = session_with_size(
            concat!(
                "\x1b_Ga=T,i=1,f=24,s=1,v=10,C=1,q=2;",
                "/wAA/wAA/wAA/wAA/wAA/wAAAP8AAP8AAAD/AAD/\x1b\\",
                "\x1b_Ga=T,i=2,P=1,H=1,V=1,s=1,v=1,q=2;/wD//w==\x1b\\",
                "\x1b[5;1H\x1bD",
            )
            .as_bytes(),
            size,
        );
        scrolled
            .set_history_limits(HistoryLimits {
                max_rows: 2,
                max_bytes: 4096,
            })
            .unwrap();
        scrolled.pump().unwrap();
        assert!(scrolled.scroll_history(1));
        scrolled.transport.source = Cursor::new(scroll);
        scrolled.pump().unwrap();
        let frame = scrolled.presentation_frame().unwrap();
        assert_eq!(frame.viewport.offset(), 2);
        assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
        let actual = read_frame(&renderer);
        assert_eq!(pixel(&actual, 0, 0), [0, 255, 0, 255]);
        assert_eq!(pixel(&actual, 0, 2), [0, 0, 255, 255]);
        assert_eq!(pixel(&actual, 2, 2), [255, 0, 255, 255]);

        scrolled.transport.source = Cursor::new(b"\x1b_Ga=d,d=P,x=2,y=1\x1b\\");
        scrolled.pump().unwrap();
        assert!(scrolled.graphics_image(2).is_some());
        scrolled.transport.source = Cursor::new(b"\x1b_Ga=d,d=I,i=1\x1b\\");
        scrolled.pump().unwrap();
        assert!(scrolled.graphics_image(2).is_none());
    }

    let mut indexed = session_with_size(
        concat!(
            "\x1b]4;1;#ff0000;2;#0000ff\x07",
            "\x1b_Ga=T,i=1,f=8,s=2,v=1,c=2,C=1,q=2;AQI=\x1b\\",
            "\x1b[1;3H\x1b_Ga=T,i=2,f=16,s=2,v=1,c=2,C=1,q=2;Af8CAA==\x1b\\",
            "\x1b[3;1H\x1b_Ga=T,i=4,f=8,s=2,v=3,c=1,r=2,C=1,q=2;AQECAgEB\x1b\\",
            "\x1b[3;2H\x1b_Ga=T,i=5,f=16,s=2,v=3,c=1,r=2,C=1,q=2;Af8B/wIAAgAB/wH/\x1b\\",
            "\x1b[3;3H\x1b_Ga=T,i=6,s=2,v=3,c=1,r=2,C=1,q=2;/wAA//8AAP8AAP//AAD///8AAP//AAD/\x1b\\",
            "\x1b[3;4H\x1b_Ga=T,i=7,s=2,v=3,c=1,r=2,C=1,q=2;/wAA//8AAP8AAP8AAAD/AP8AAP//AAD/\x1b\\",
        )
        .as_bytes(),
        size,
    );
    indexed.pump().unwrap();
    let prefix = "Af8B/wH/".repeat(84);
    let wire = format!(
        "\x1b[1;5H\x1b_Ga=T,i=3,f=16,s=257,v=1,x=253,w=2,c=2,C=1,q=2;{prefix}Af8B/wIAAgACAA==\x1b\\"
    );
    assert_eq!(
        indexed
            .processor
            .advance(wire.as_bytes(), &mut indexed.screen, &mut indexed.writer)
            .unwrap(),
        wire.len()
    );
    let frame = indexed.presentation_frame().unwrap();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    let filtered = read_frame(&renderer);
    for (x, expected) in [
        (1, [225, 0, 137, 255]),
        (2, [137, 0, 225, 255]),
        (5, [225, 0, 0, 191]),
        (6, [137, 0, 0, 64]),
        (7, [0; 4]),
    ] {
        let actual = pixel(&filtered, x, 0);
        assert!(
            actual
                .into_iter()
                .zip(expected)
                .all(|(a, b)| a.abs_diff(b) <= 1),
            "({x}, 0): {actual:?} != {expected:?}"
        );
    }
    for x in 0..4 {
        assert_eq!(pixel(&filtered, 4 + x, 0), pixel(&filtered, 8 + x, 0));
    }
    for x in 0..4 {
        for y in 5..7 {
            let actual = pixel(&filtered, x, y);
            let expected = pixel(&filtered, x + 4, y);
            assert!(
                actual
                    .into_iter()
                    .zip(expected)
                    .all(|(a, b)| a.abs_diff(b) <= 1),
                "Fractional indexed sample at ({x}, {y}): {actual:?} != {expected:?}"
            );
        }
    }

    let mut png = session_with_size(
        concat!(
            "\x1b_Ga=T,i=1,f=100,C=1,q=2;",
            "iVBORw0KGgoAAAANSUhEUgAAAAIAAAABCAYAAAD0In+KAAAABGdBTUEAAYagMeiWXwAAABFJREFUeJxjYHRo+C/QcKIBAA2cA5nvkQ2eAAAAAElFTkSuQmCC\x1b\\",
            "\x1b[1;3H\x1b_Ga=T,i=2,s=2,v=1,C=1,q=2;FYi6/0i65IA=\x1b\\",
        )
        .as_bytes(),
        size,
    );
    png.pump().unwrap();
    assert_eq!(
        renderer.prepare(&device, &queue, &png.presentation_frame().unwrap()),
        0
    );
    let corrected = read_frame(&renderer);
    assert_eq!(pixel(&corrected, 0, 0), [21, 136, 186, 255]);
    for x in 0..2 {
        assert_eq!(pixel(&corrected, x, 0), pixel(&corrected, x + 4, 0));
    }

    let mut renderer = crate::ImageRenderer::new(&device, format);
    let mut tiled = session_with_size(b"", size);
    let send = |tiled: &mut Session<Transport>, wire: &[u8]| {
        let consumed = tiled
            .processor
            .advance(wire, &mut tiled.screen, &mut tiled.writer)
            .unwrap();
        assert_eq!(consumed, wire.len());
    };
    let prefix = "AAAA".repeat(124);
    let rows = [
        "AAAAAAAAAAAAAAAA",
        "AAAA/wAAAP8AAAAA",
        "AAAAAAD//wD/AAAA",
        "AAAAAAAAAAAAAAAA",
    ];
    for row in 0..129 {
        let header = if row == 0 {
            "i=47,f=24,s=129,v=129,q=2,"
        } else {
            ""
        };
        let more = u8::from(row != 128);
        let samples = rows[row.clamp(124, 127) - 124];
        let wire = format!("\x1b_G{header}m={more};{prefix}{samples}AAAA\x1b\\");
        send(&mut tiled, wire.as_bytes());
    }
    let near = b"\x1b_Ga=p,i=47,p=7,w=1,h=1,C=1,q=2\x1b\\";
    let far = b"\x1b_Ga=p,i=47,p=7,x=128,y=128,w=1,h=1,C=1,q=2\x1b\\";
    for wire in [near.as_slice(), far, near] {
        send(&mut tiled, wire);
        assert_eq!(
            renderer.prepare(&device, &queue, &tiled.presentation_frame().unwrap()),
            0
        );
        assert_eq!(pixel(&read_frame(&renderer), 0, 0), [0, 0, 0, 255]);
    }
    send(
        &mut tiled,
        concat!(
            "\x1b_Ga=f,i=47,c=1,x=128,y=128,s=1,v=1,X=1,q=2;AP8A/w==\x1b\\",
            "\x1b_Ga=a,i=47,c=2,q=2\x1b\\",
            "\x1b_Ga=p,i=47,p=7,x=128,y=128,w=1,h=1,C=1,q=2\x1b\\",
        )
        .as_bytes(),
    );
    let frame = tiled.presentation_frame().unwrap();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    let image = frame.graphics_placements()[0].placement.image;
    assert_eq!(renderer.resident_image_tiles(image), 2);
    assert_eq!(pixel(&read_frame(&renderer), 0, 0), [0, 255, 0, 255]);
    assert_eq!(renderer.pixel_scratch_capacity(), 0);
    for (wire, tiles, expected) in [
        (
            b"\x1b_Ga=a,i=47,c=1,q=2\x1b\\".as_slice(),
            2,
            [0, 0, 0, 255],
        ),
        (
            concat!(
                "\x1b_Ga=f,i=47,x=128,y=128,s=1,v=1,Y=4278190335,X=1,q=2;AAD//w==\x1b\\",
                "\x1b_Ga=a,i=47,c=3,q=2\x1b\\",
            )
            .as_bytes(),
            2,
            [0, 0, 255, 255],
        ),
        (
            b"\x1b_Ga=p,i=47,p=7,w=1,h=1,C=1,q=2\x1b\\",
            2,
            [255, 0, 0, 255],
        ),
    ] {
        send(&mut tiled, wire);
        let frame = tiled.presentation_frame().unwrap();
        assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
        let image = frame.graphics_placements()[0].placement.image;
        assert_eq!(renderer.resident_image_tiles(image), tiles);
        assert!(renderer.pixel_scratch_capacity() <= 128 * 128 * 4);
        assert_eq!(pixel(&read_frame(&renderer), 0, 0), expected);
    }
    send(&mut tiled, b"\x1b_Ga=p,i=47,p=7,c=2,r=2,C=1,q=2\x1b\\");
    let frame = tiled.presentation_frame().unwrap();
    assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
    let image = frame.graphics_placements()[0].placement.image;
    assert_eq!(renderer.resident_image_tiles(image), 4);
    send(
        &mut tiled,
        concat!(
            "\x1b_Ga=p,i=47,p=7,w=1,h=1,C=1,q=2\x1b\\",
            "\x1b_Ga=f,i=47,x=128,y=128,s=1,v=1,Y=16711935,X=1,q=2;AAD//w==\x1b\\",
            "\x1b_Ga=a,i=47,c=4,q=2\x1b\\",
        )
        .as_bytes(),
    );
    assert_eq!(
        renderer.prepare(&device, &queue, &tiled.presentation_frame().unwrap()),
        0
    );
    assert_eq!(pixel(&read_frame(&renderer), 0, 0), [0, 255, 0, 255]);
    assert!(renderer.pixel_scratch_capacity() <= 128 * 128 * 4);
    send(
        &mut tiled,
        b"\x1b_Ga=p,i=47,p=7,x=128,y=128,w=1,h=1,C=1,q=2\x1b\\",
    );
    assert_eq!(
        renderer.prepare(&device, &queue, &tiled.presentation_frame().unwrap()),
        0
    );
    assert_eq!(renderer.resident_image_tiles(image), 4);
    assert_eq!(pixel(&read_frame(&renderer), 0, 0), [0, 0, 255, 255]);
    for (wire, expected) in [
        (b"\x1b_Ga=a,i=47,c=1,q=2\x1b\\".as_slice(), [0, 0, 0, 255]),
        (b"\x1b_Ga=p,i=47,p=7,w=1,h=1,C=1,q=2\x1b\\", [0, 0, 0, 255]),
        (b"\x1b_Ga=a,i=47,c=2,q=2\x1b\\", [0, 0, 0, 255]),
        (
            b"\x1b_Ga=p,i=47,p=7,x=128,y=128,w=1,h=1,C=1,q=2\x1b\\",
            [0, 255, 0, 255],
        ),
    ] {
        send(&mut tiled, wire);
        assert_eq!(
            renderer.prepare(&device, &queue, &tiled.presentation_frame().unwrap()),
            0
        );
        assert_eq!(pixel(&read_frame(&renderer), 0, 0), expected);
    }
    let first_tile = renderer.cached_texture(image).unwrap().clone();
    send(
        &mut tiled,
        concat!(
            "\x1b_Ga=p,i=47,p=7,c=2,r=2,C=1,q=2\x1b\\",
            "\x1b_Ga=a,i=47,c=3,q=2\x1b\\",
        )
        .as_bytes(),
    );
    assert_eq!(
        renderer.prepare(&device, &queue, &tiled.presentation_frame().unwrap()),
        0
    );
    assert_eq!(renderer.cached_texture(image), Some(&first_tile));
    assert!(renderer.pixel_scratch_capacity() <= 128 * 128 * 4);
    assert_eq!(pixel(&read_frame(&renderer), 0, 0), [255, 0, 0, 255]);
    {
        let mut whole = crate::ImageRenderer::new(&device, format);
        for (wire, color) in [
            (b"".as_slice(), [255, 0, 0, 255]),
            (b"\x1b_Ga=a,i=47,c=4,q=2\x1b\\", [0, 255, 0, 255]),
        ] {
            send(&mut tiled, wire);
            let frame = tiled.presentation_frame().unwrap();
            assert_eq!(whole.prepare(&device, &queue, &frame), 0);
            assert_eq!(whole.resident_image_tiles(image), 4);
            assert_eq!(pixel(&read_frame(&whole), 0, 0), color);
            assert!(whole.pixel_scratch_capacity() <= 128 * 128 * 4, "{color:?}");
        }
    }
    send(
        &mut tiled,
        concat!(
            "\x1b_Ga=a,i=47,c=1,q=2\x1b\\",
            "\x1b_Ga=d,d=f,i=47,r=4\x1b\\\x1b_Ga=d,d=f,i=47,r=3\x1b\\",
            "\x1b_Ga=d,d=f,i=47,r=2\x1b\\",
            "\x1b_Ga=d,d=i,i=47,p=7\x1b\\",
        )
        .as_bytes(),
    );
    {
        let mut overlapping = crate::ImageRenderer::new(&device, format);
        for (row, pixels) in [(0, "/wAAgP8AAID/AACA"), (49, "AP8AgAD/AIAA/wCA")] {
            let wire = format!(
                "\x1b_Ga=f,i=47,c=1,y={row},s=129,v=80,q=2;{}\x1b\\",
                pixels.repeat(129 * 80 / 3),
            );
            send(&mut tiled, wire.as_bytes());
        }
        send(
            &mut tiled,
            b"\x1b[H\x1b_Ga=p,i=47,p=7,c=2,r=2,C=1,q=2\x1b\\",
        );
        for (current, top, bottom) in [
            (2, [128, 0, 0, 255], [0, 0, 0, 255]),
            (3, [0, 0, 0, 255], [0, 128, 0, 255]),
            (2, [128, 0, 0, 255], [0, 0, 0, 255]),
        ] {
            let wire = format!("\x1b_Ga=a,i=47,c={current},q=2\x1b\\");
            send(&mut tiled, wire.as_bytes());
            let frame = tiled.presentation_frame().unwrap();
            assert_eq!(overlapping.prepare(&device, &queue, &frame), 0);
            let output = read_frame(&overlapping);
            assert_eq!([pixel(&output, 0, 0), pixel(&output, 0, 3)], [top, bottom]);
            assert!(
                overlapping.pixel_scratch_capacity() <= 128 * 128 * 4,
                "frame {current}"
            );
        }
        send(
            &mut tiled,
            b"\x1b_Ga=a,i=47,c=1,q=2\x1b\\\x1b_Ga=d,d=f,i=47,r=3\x1b\\\x1b_Ga=d,d=f,i=47,r=2\x1b\\\x1b_Ga=d,d=i,i=47,p=7\x1b\\",
        );
    }
    {
        let mut renderer = crate::ImageRenderer::new(&device, format);
        let mut render = |tiled: &Session<Transport>| {
            let frame = tiled.presentation_frame().unwrap();
            assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
            read_frame(&renderer)
        };
        for wire in [near.as_slice(), far] {
            send(&mut tiled, wire);
            assert_eq!(pixel(&render(&tiled), 0, 0), [0, 0, 0, 255]);
        }
        for wire in [
            concat!(
                "\x1b_Ga=f,i=47,c=1,f=24,x=4,y=4,s=1,v=1,X=1,q=2;/wAA\x1b\\",
                "\x1b_Ga=a,i=47,c=2,q=2\x1b\\",
            )
            .as_bytes(),
            b"\x1b_Ga=f,i=47,r=2,f=24,x=120,y=120,s=1,v=1,X=1,q=2;AP8A\x1b\\",
            b"\x1b_Ga=f,i=47,r=2,f=24,x=5,y=4,s=1,v=1,X=1,q=2;AAD/\x1b\\",
        ] {
            send(&mut tiled, wire);
            assert_eq!(pixel(&render(&tiled), 0, 0), [0, 0, 0, 255]);
        }
        send(
            &mut tiled,
            concat!(
                "\x1b_Ga=p,i=47,p=7,x=4,y=4,w=2,h=1,C=1,q=2\x1b\\",
                "\x1b[1;2H\x1b_Ga=p,i=47,p=8,x=120,y=120,w=1,h=1,C=1,q=2\x1b\\",
            )
            .as_bytes(),
        );
        let revealed = render(&tiled);
        assert_eq!(pixel(&revealed, 0, 0), [255, 0, 0, 255]);
        assert_eq!(pixel(&revealed, 1, 0), [0, 0, 255, 255]);
        assert_eq!(pixel(&revealed, 2, 0), [0, 255, 0, 255]);
        assert!(renderer.pixel_scratch_capacity() <= 8);
        send(
            &mut tiled,
            b"\x1b_Ga=d,d=f,i=47,r=2\x1b\\\x1b_Ga=d,d=i,i=47\x1b\\\x1b[1;1H",
        );
    }
    for move_crop in [false, true] {
        let mut renderer = crate::ImageRenderer::new(&device, format);
        let mut render = |wire: &[u8]| {
            send(&mut tiled, wire);
            let frame = tiled.presentation_frame().unwrap();
            assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
            (
                pixel(&read_frame(&renderer), 0, 0),
                renderer.pixel_scratch_capacity(),
            )
        };
        for wire in [near.as_slice(), far] {
            assert_eq!(render(wire), ([0, 0, 0, 255], 0));
        }
        if move_crop {
            assert_eq!(render(near), ([0, 0, 0, 255], 0));
        }
        let wire = format!(
            "\x1b_Ga=f,i=47,c=1,x=4,y=4,s=27,v=27,q=2;{}\x1b\\\x1b_Ga=a,i=47,c=2,q=2\x1b\\{}",
            "/wAAgP8AAID/AACA".repeat(243),
            if move_crop {
                str::from_utf8(far).unwrap()
            } else {
                ""
            },
        );
        assert_eq!(render(wire.as_bytes()), ([0, 0, 0, 255], 0));
        assert_eq!(
            render(b"\x1b_Ga=p,i=47,p=7,x=4,y=4,w=1,h=1,C=1,q=2\x1b\\").0,
            [128, 0, 0, 255],
        );
        send(
            &mut tiled,
            b"\x1b_Ga=d,d=f,i=47,r=2\x1b\\\x1b_Ga=d,d=i,i=47\x1b\\",
        );
    }
    {
        let mut renderer = crate::ImageRenderer::new(&device, format);
        let mut render = |wire: &[u8]| {
            send(&mut tiled, wire);
            let frame = tiled.presentation_frame().unwrap();
            assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
            (
                pixel(&read_frame(&renderer), 0, 0),
                renderer.pixel_scratch_capacity(),
            )
        };
        let left = b"\x1b_Ga=p,i=47,p=7,x=125,y=4,w=1,h=1,C=1,q=2\x1b\\";
        let right = b"\x1b_Ga=p,i=47,p=7,x=128,y=4,w=1,h=1,C=1,q=2\x1b\\";
        for wire in [left, right, left] {
            assert_eq!(render(wire), ([0, 0, 0, 255], 0));
        }
        let green = format!(
            "\x1b_Ga=f,i=47,c=1,x=125,y=4,s=4,v=27,f=24,X=1,q=2;{}\x1b\\\x1b_Ga=a,i=47,c=2,q=2\x1b\\",
            "AP8A".repeat(4 * 27),
        );
        assert_eq!(render(green.as_bytes()), ([0, 255, 0, 255], 0));
        let blue = format!(
            "\x1b_Ga=f,i=47,c=1,x=4,y=90,s=1,v=27,f=24,X=1,q=2;{}\x1b\\\x1b_Ga=a,i=47,c=3,q=2\x1b\\",
            "AAD/".repeat(27),
        );
        assert_eq!(render(blue.as_bytes()), ([0, 0, 0, 255], 0));
        assert_eq!(
            render(b"\x1b_Ga=p,i=47,p=7,x=4,y=90,w=1,h=1,C=1,q=2\x1b\\"),
            ([0, 0, 255, 255], 0)
        );
        assert_eq!(
            render(b"\x1b_Ga=a,i=47,c=1,q=2\x1b\\\x1b_Ga=d,d=f,i=47,r=3\x1b\\\x1b_Ga=d,d=f,i=47,r=2\x1b\\"),
            ([0, 0, 0, 255], 0)
        );
        assert_eq!(render(left), ([0, 0, 0, 255], 0));
        let wire = format!(
            "\x1b_Ga=f,i=47,c=1,x=125,y=4,s=4,v=27,q=2;{}\x1b\\\x1b_Ga=a,i=47,c=2,q=2\x1b\\",
            "/wAAgP8AAID/AACA".repeat(36),
        );
        let (pixel, scratch) = render(wire.as_bytes());
        assert_eq!(pixel, [128, 0, 0, 255]);
        assert!(scratch <= 2 * 27 * 4, "Scratch capacity was {scratch}");
        assert_eq!(render(right).0, [128, 0, 0, 255]);
        send(
            &mut tiled,
            b"\x1b_Ga=d,d=f,i=47,r=2\x1b\\\x1b_Ga=d,d=i,i=47\x1b\\",
        );
    }
    let reference = rows.concat();
    let wire = format!(
        concat!(
            "\x1b_Ga=p,i=47,p=1,x=125,y=125,w=2,h=2,c=2,r=2,C=1,q=2\x1b\\",
            "\x1b_Gi=49,f=24,s=4,v=4,q=2;{}\x1b\\",
            "\x1b[1;3H\x1b_Ga=p,i=49,x=1,y=1,w=2,h=2,c=2,r=2,C=1,q=2\x1b\\",
        ),
        reference
    );
    send(&mut tiled, wire.as_bytes());
    let mut renderer = crate::ImageRenderer::new(&device, format);
    let read_crop = |renderer: &mut crate::ImageRenderer, tiled: &Session<Transport>| {
        assert_eq!(
            renderer.prepare(&device, &queue, &tiled.presentation_frame().unwrap()),
            0
        );
        let actual = read_frame(renderer);
        for y in 0..4 {
            for x in 0..4 {
                assert!(
                    pixel(&actual, x, y)
                        .into_iter()
                        .zip(pixel(&actual, x + 4, y))
                        .all(|(a, b)| a.abs_diff(b) <= 1),
                    "Tiled pixel at ({x}, {y}) differs from the untiled crop"
                );
            }
        }
        assert_eq!(pixel(&actual, 0, 0), [255, 0, 0, 255]);
        assert_eq!(pixel(&actual, 8, 0), [0; 4]);
        assert_eq!(pixel(&actual, 0, 4), [0; 4]);
        pixel(&actual, 1, 1)
    };
    let initial = read_crop(&mut renderer, &tiled);
    assert!(
        initial
            .into_iter()
            .zip([207, 120, 137, 255])
            .all(|(a, b)| a.abs_diff(b) <= 1)
    );
    for (wire, expected) in [
        (
            concat!(
                "\x1b_Ga=f,i=47,c=1,f=24,x=126,y=126,s=1,v=1,X=1,q=2;////\x1b\\",
                "\x1b_Ga=f,i=49,c=1,f=24,x=2,y=2,s=1,v=1,X=1,q=2;////\x1b\\",
                "\x1b_Ga=a,i=47,c=2,q=2\x1b\\\x1b_Ga=a,i=49,c=2,q=2\x1b\\",
            )
            .as_bytes(),
            [207, 137, 137, 255],
        ),
        (
            concat!(
                "\x1b_Ga=f,i=47,r=2,x=126,y=126,s=1,v=1,X=1,q=2;AP8AAA==\x1b\\",
                "\x1b_Ga=f,i=49,r=2,x=2,y=2,s=1,v=1,X=1,q=2;AP8AAA==\x1b\\",
                "\x1b[1;1H\x1b_Ga=p,i=47,p=1,x=125,y=125,w=2,h=2,c=2,r=2,C=1,q=2\x1b\\",
            )
            .as_bytes(),
            [198, 120, 120, 239],
        ),
        (
            b"\x1b_Ga=a,i=47,c=1,q=2\x1b\\\x1b_Ga=a,i=49,c=1,q=2\x1b\\",
            initial,
        ),
    ] {
        assert_eq!(
            tiled
                .processor
                .advance(wire, &mut tiled.screen, &mut tiled.writer)
                .unwrap(),
            wire.len()
        );
        let actual = read_crop(&mut renderer, &tiled);
        assert!(
            actual
                .into_iter()
                .zip(expected)
                .all(|(a, b)| a.abs_diff(b) <= 1)
        );
    }

    tiled.transport.source = Cursor::new(
        concat!(
            "\x1b_Ga=f,i=47,c=1,s=1,v=1,X=1,q=2;/wAA/w==\x1b\\",
            "\x1b_Ga=f,i=47,c=1,x=126,y=126,s=1,v=1,X=1,q=2;/////w==\x1b\\",
            "\x1b[1;5H\x1b_Ga=p,i=47,p=3,w=1,h=1,C=1,q=2\x1b\\",
            "\x1b[1;6H\x1b_Ga=p,i=47,p=4,x=126,y=126,w=1,h=1,C=1,q=2\x1b\\",
            "\x1b_Ga=a,i=47,c=3,q=2\x1b\\",
        )
        .as_bytes(),
    );
    tiled.pump().unwrap();
    assert_eq!(
        renderer.prepare(&device, &queue, &tiled.presentation_frame().unwrap()),
        0
    );
    assert_eq!(pixel(&read_frame(&renderer), 8, 0), [255, 0, 0, 255]);

    tiled.transport.source = Cursor::new(b"\x1b_Ga=a,i=47,c=4,q=2\x1b\\");
    tiled.pump().unwrap();
    assert_eq!(
        renderer.prepare(&device, &queue, &tiled.presentation_frame().unwrap()),
        0
    );
    let sibling = read_frame(&renderer);
    assert_eq!(pixel(&sibling, 8, 0), [0, 0, 0, 255]);
    assert_eq!(pixel(&sibling, 10, 0), [255, 255, 255, 255]);
    assert!(
        pixel(&sibling, 1, 1)
            .into_iter()
            .zip([207, 137, 137, 255])
            .all(|(a, b)| a.abs_diff(b) <= 1)
    );

    let mut renderer = crate::ImageRenderer::new(&device, format);
    let mut clipped = session_with_size(
        concat!(
            "\x1b[2;5r\x1b[2;1H\x1b_Ga=T,i=51,s=1,v=4,C=1,q=2;/wAA//8AAP8A/wD/AAD//w==\x1b\\",
            "\x1b[1;1H\x1b_Ga=T,i=52,s=1,v=1,P=51,H=1,V=1,q=2;/wAA/w==\x1b\\",
            "\x1b[S\x1b_Ga=T,i=53,s=1,v=1,P=52,H=1,V=1,q=2;/wD//w==\x1b\\",
        )
        .as_bytes(),
        size,
    );
    for row in [1, 2] {
        clipped.pump().unwrap();
        let frame = clipped.presentation_frame().unwrap();
        assert_eq!(renderer.prepare(&device, &queue, &frame), 0);
        let rendered = read_frame(&renderer);
        assert_eq!(pixel(&rendered, 0, row * 2), [0, 255, 0, 255]);
        assert_eq!(pixel(&rendered, 2, (row + 1) * 2), [255, 0, 0, 255]);
        assert_eq!(pixel(&rendered, 4, (row + 2) * 2), [255, 0, 255, 255]);
        assert_eq!(pixel(&rendered, 2, row * 2), [0; 4]);
        clipped.transport.source = Cursor::new(b"\x1b[T");
    }
    let resize_cell_height = |session: &mut Session<Transport>, pixel_height| {
        session.geometry = GridGeometry::from_pty_size(PtySize {
            pixel_height,
            ..size
        });
        session.processor.update_geometry(
            &mut session.screen,
            session.geometry.text_area_pixels(),
            session.geometry.cell_pixels(),
        );
    };
    let draw = |renderer: &mut crate::ImageRenderer, session: &Session<Transport>| {
        assert_eq!(
            renderer.prepare(&device, &queue, &session.presentation_frame().unwrap()),
            0
        );
        read_frame(renderer)
    };
    let mut overlap = session_with_size(
        concat!(
            "\x1b]4;11;#0000ff\x07",
            "\x1b_Ga=T,i=4294967295,p=1,f=16,s=1,v=1,C=1,q=2;C4A=\x1b\\",
            "\x1b_Ga=T,i=2,f=24,s=1,v=1,C=1,q=2;/wAA\x1b\\",
        )
        .as_bytes(),
        size,
    );
    overlap.pump().unwrap();
    let blended = pixel(&draw(&mut renderer, &overlap), 0, 0);
    assert!(
        blended
            .into_iter()
            .zip([187, 0, 188, 255])
            .all(|(actual, expected)| actual.abs_diff(expected) <= 1)
    );
    for (wire, expected) in [
        (
            b"\x1b_Ga=p,i=4294967295,p=1,z=-1,C=1,q=2\x1b\\".as_slice(),
            [255, 0, 0, 255],
        ),
        (b"\x1b_Ga=p,i=4294967295,p=1,z=0,C=1,q=2\x1b\\", blended),
    ] {
        overlap.transport.source = Cursor::new(wire);
        overlap.pump().unwrap();
        assert_eq!(pixel(&draw(&mut renderer, &overlap), 0, 0), expected);
    }

    let mut below_screen = session_with_size(
        concat!(
            "\x1b[4;1H\x1b_Ga=T,i=51,p=1,s=1,v=1,C=1,q=2;/wAA/w==\x1b\\",
            "\x1b_Ga=T,i=52,P=51,Q=1,H=1,s=1,v=1,q=2;AAD//w==\x1b\\",
            "\x1b[H\x1bM",
        )
        .as_bytes(),
        PtySize {
            rows: 4,
            pixel_height: 8,
            ..size
        },
    );
    below_screen.pump().unwrap();
    assert_eq!(
        renderer.prepare(&device, &queue, &below_screen.presentation_frame().unwrap()),
        0
    );
    assert_eq!(renderer.image_instance_count(), 0);
    below_screen
        .screen
        .reflow(8, 5, StyleId::default())
        .unwrap();
    below_screen.geometry = GridGeometry::from_pty_size(size);
    below_screen.processor.update_geometry(
        &mut below_screen.screen,
        below_screen.geometry.text_area_pixels(),
        below_screen.geometry.cell_pixels(),
    );
    let revealed = draw(&mut renderer, &below_screen);
    assert_eq!(pixel(&revealed, 0, 8), [255, 0, 0, 255]);
    assert_eq!(pixel(&revealed, 2, 8), [0, 0, 255, 255]);

    resize_cell_height(&mut clipped, 5);
    let shrunk = draw(&mut renderer, &clipped);
    assert_eq!(pixel(&shrunk, 0, 4), [0, 255, 0, 255]);
    assert_eq!(pixel(&shrunk, 0, 7), [0, 0, 255, 255]);
    assert_eq!(pixel(&shrunk, 2, 6), [255, 0, 0, 255]);
    clipped.transport.source = Cursor::new(b"\x1b[2S");
    clipped.pump().unwrap();
    resize_cell_height(&mut clipped, 10);
    let recropped = draw(&mut renderer, &clipped);
    assert_eq!(pixel(&recropped, 0, 2), [0, 0, 255, 255]);
    assert_eq!(pixel(&recropped, 0, 3), [0; 4]);
    assert_eq!(pixel(&recropped, 2, 4), [255, 0, 0, 255]);
    assert_eq!(pixel(&recropped, 4, 6), [255, 0, 255, 255]);

    clipped.transport.source = Cursor::new(b"\x1b[4S");
    clipped.pump().unwrap();
    assert!(clipped.screen().graphics_placements().is_empty());
    assert!(clipped.graphics_image(52).is_none());
    assert!(clipped.graphics_image(53).is_none());

    clipped.transport.source = Cursor::new(
        b"\x1b[2;4r\x1b[3;1H\x1b_Ga=T,i=54,f=24,s=1,v=5,y=1,h=4,C=1,q=2;AAAA/wAAAP8AAAD//wD/\x1b\\\x1b[T",
    );
    clipped.pump().unwrap();
    resize_cell_height(&mut clipped, 5);
    let shrunk = draw(&mut renderer, &clipped);
    assert_eq!(pixel(&shrunk, 0, 6), [255, 0, 0, 255]);
    assert_eq!(pixel(&shrunk, 0, 9), [0, 255, 0, 255]);

    clipped.transport.source = Cursor::new(b"\x1b[2;5r\x1b[T");
    clipped.pump().unwrap();
    resize_cell_height(&mut clipped, 10);
    let recropped = draw(&mut renderer, &clipped);
    assert_eq!(pixel(&recropped, 0, 8), [255, 0, 0, 255]);
    assert_eq!(pixel(&recropped, 0, 9), [0; 4]);

    let mut translucent = session_with_size(
        b"\x1b_Ga=T,i=55,s=2,v=1,c=2,r=1,C=1,q=2;/wAA/wAA/wA=\x1b\\",
        size,
    );
    translucent.pump().unwrap();
    let scaled = draw(&mut renderer, &translucent);
    assert_eq!(pixel(&scaled, 0, 0), [255, 0, 0, 255]);
    for (x, red, alpha) in [(1, 225, 191), (2, 137, 64)] {
        let [actual_red, green, blue, actual_alpha] = pixel(&scaled, x, 0);
        assert_eq!([green, blue], [0; 2]);
        assert!(actual_red.abs_diff(red) <= 1);
        assert!(actual_alpha.abs_diff(alpha) <= 1);
    }
    assert_eq!(pixel(&scaled, 3, 0), [0; 4]);

    let mut cropped = session_with_size(
        concat!(
            "\x1b]4;1;#ff0000;2;#00ff00\x07",
            "\x1b_Ga=T,i=56,f=24,s=3,v=3,x=1,y=1,w=1,h=1,c=2,r=2,C=1,q=2;AP8AAP8AAP8AAP8A/wAAAP8AAP8AAP8AAP8A\x1b\\",
            "\x1b[1;3H\x1b_Ga=T,i=57,f=16,s=3,v=3,x=1,y=1,w=1,h=1,c=2,r=2,C=1,q=2;Av8C/wL/Av8BgAL/Av8C/wL/\x1b\\",
        )
        .as_bytes(),
        size,
    );
    cropped.pump().unwrap();
    let scaled = draw(&mut renderer, &cropped);
    for y in 0..4 {
        for x in 0..4 {
            assert_eq!(pixel(&scaled, x, y), [255, 0, 0, 255]);
            let [red, green, blue, alpha] = pixel(&scaled, x + 4, y);
            assert_eq!([green, blue, alpha], [0, 0, 128]);
            assert!(red.abs_diff(188) <= 1);
        }
    }

    let mut buffers = session_with_size(b"\x1b_Ga=T,i=58,s=1,v=1,C=1,q=2;/wAA/w==\x1b\\", size);
    buffers.pump().unwrap();
    let key = buffers.screen().graphics_placements()[0].placement.image;
    assert_eq!(
        pixel(&draw(&mut renderer, &buffers), 0, 0),
        [255, 0, 0, 255]
    );
    let primary_texture = renderer.cached_texture(key).unwrap().clone();
    let red = [255, 0, 0, 255];
    let green = [0, 255, 0, 255];
    let blue = [0, 0, 255, 255];
    let empty = [0; 4];
    for (wire, expected) in [
        (
            concat!(
                "\x1b[?47h\x1b_Ga=T,i=58,s=1,v=1,C=1,q=2;AP8A/w==\x1b\\",
                "\x1b_Ga=T,i=59,U=1,c=1,r=1,s=1,v=1,q=2;AAD//w==\x1b\\",
                "\x1b[1;2H\x1b[38;5;59m\u{10eeee}\x1b[39m",
            )
            .as_bytes(),
            [green, blue],
        ),
        (b"\x1b[?1049h", [green, blue]),
        (b"\x1b[?1047l", [red, empty]),
        (b"\x1b[?47h", [green, blue]),
        (b"\x1b[?47l", [red, empty]),
        (b"\x1b[?1049h", [empty, empty]),
        (
            "\x1b[1;2H\x1b[38;5;59m\u{10eeee}\x1b[39m".as_bytes(),
            [empty, blue],
        ),
        (b"\x1b[?1049l", [red, empty]),
    ] {
        buffers.transport.source = Cursor::new(wire);
        buffers.pump().unwrap();
        let rendered = draw(&mut renderer, &buffers);
        assert_eq!([pixel(&rendered, 0, 0), pixel(&rendered, 2, 0)], expected);
        if expected[0] == red {
            assert_eq!(renderer.cached_texture(key), Some(&primary_texture));
        }
    }

    let mut temporary_animation = session_with_size(
        concat!(
            "\x1b_Ga=T,i=59,f=24,s=1,v=1,C=1,q=2;/wAA\x1b\\",
            "\x1b_Ga=f,i=59,f=24,s=1,v=1,q=2;AAD/\x1b\\",
            "\x1b_Ga=a,i=59,c=2\x1b\\",
        )
        .as_bytes(),
        size,
    );
    temporary_animation.pump().unwrap();
    let key = temporary_animation.screen().graphics_placements()[0]
        .placement
        .image;
    assert_eq!(
        pixel(&draw(&mut renderer, &temporary_animation), 0, 0),
        blue
    );
    let texture = renderer.cached_texture(key).unwrap().clone();
    temporary_animation.transport.source = Cursor::new(b"\x1b_Ga=d,d=f,i=59,r=2\x1b\\");
    temporary_animation.pump().unwrap();
    assert_eq!(pixel(&draw(&mut renderer, &temporary_animation), 0, 0), red);
    assert_eq!(renderer.cached_texture(key), Some(&texture));

    for inactive in [false, true] {
        let mut anonymous = session_with_size(
            b"\x1b_Ga=T,s=1,v=1,C=1,q=2;/wAA/w==\x1b\\\x1b[5;1H\x1bD",
            size,
        );
        anonymous
            .set_history_limits(HistoryLimits {
                max_rows: 1,
                max_bytes: 4096,
            })
            .unwrap();
        anonymous.pump().unwrap();
        let key = anonymous.screen().graphics_placements()[0].placement.image;
        assert!(anonymous.scroll_history(1));
        assert_eq!(pixel(&draw(&mut renderer, &anonymous), 0, 0), red);
        assert!(renderer.has_cached_image(key));
        if inactive {
            anonymous.transport.source = Cursor::new(b"\x1b[?47h");
            anonymous.pump().unwrap();
            assert_eq!(pixel(&draw(&mut renderer, &anonymous), 0, 0), empty);
            assert!(renderer.has_inactive_cached_image(key));
        }
        anonymous
            .set_history_limits(HistoryLimits::default())
            .unwrap();
        assert_eq!(pixel(&draw(&mut renderer, &anonymous), 0, 0), empty);
        assert!(!renderer.has_cached_image(key));
        assert!(!renderer.has_inactive_cached_image(key));
    }

    let mut inherited = session_with_size(
        concat!(
            "\x1b_Ga=T,i=60,f=24,s=3,v=1,C=1,q=2;/wAA////AAD/\x1b\\",
            "\x1b_Ga=f,i=60,c=1,f=24,x=1,s=1,v=1,X=1,q=2;AP8A\x1b\\",
            "\x1b_Ga=a,i=60,c=2\x1b\\",
        )
        .as_bytes(),
        size,
    );
    inherited.pump().unwrap();
    let key = inherited.screen().graphics_placements()[0].placement.image;
    let initial = draw(&mut renderer, &inherited);
    assert_eq!([pixel(&initial, 0, 0), pixel(&initial, 1, 0)], [red, green]);
    let texture = renderer.cached_texture(key).unwrap().clone();
    inherited.transport.source = Cursor::new(
        concat!(
            "\x1b_Ga=f,i=60,r=1,f=24,s=1,v=1,X=1,q=2;AAD/\x1b\\",
            "\x1b_Ga=f,i=60,r=1,x=2,f=24,s=1,v=1,X=1,q=2;//8A\x1b\\",
        )
        .as_bytes(),
    );
    inherited.pump().unwrap();
    let updated = draw(&mut renderer, &inherited);
    assert_eq!(
        [
            pixel(&updated, 0, 0),
            pixel(&updated, 1, 0),
            pixel(&updated, 2, 0)
        ],
        [blue, green, [255, 255, 0, 255]]
    );
    assert_eq!(renderer.cached_texture(key), Some(&texture));

    let white = [255; 4];
    for (wire, expected) in [
        (
            concat!(
                "\x1b_Ga=f,i=60,c=2,f=24,s=1,v=1,X=1,q=2;/wAA\x1b\\",
                "\x1b_Ga=a,i=60,c=1\x1b\\",
            )
            .as_bytes(),
            [blue, white],
        ),
        (b"\x1b_Ga=a,i=60,c=3\x1b\\", [red, green]),
        (b"\x1b_Ga=a,i=60,c=1\x1b\\", [blue, white]),
    ] {
        inherited.transport.source = Cursor::new(wire);
        inherited.pump().unwrap();
        let image = draw(&mut renderer, &inherited);
        assert_eq!([pixel(&image, 0, 0), pixel(&image, 1, 0)], expected);
        assert_eq!(renderer.cached_texture(key), Some(&texture));
    }

    let mut automatic = session_with_size(
        concat!(
            "\x1b_Ga=T,U=1,i=61,r=2,s=4,v=2,q=2;/wAA//8AAP//AAD//wAA//8AAP//AAD//wAA//8AAP8=\x1b\\",
            "\x1b_Ga=T,U=1,i=62,c=2,s=2,v=4,q=2;AP8A/wD/AP8A/wD/AP8A/wD/AP8A/wD/AP8A/wD/AP8=\x1b\\",
            "\x1b_Ga=T,U=1,i=63,s=2,v=2,q=2;AAD//wAA//8AAP//AAD//w==\x1b\\",
            "\x1b[1;1H\x1b[38;5;61m\u{10eeee}\u{305}\u{305}\u{10eeee}\u{10eeee}",
            "\x1b[1;5H\x1b[38;5;62m\u{10eeee}\u{305}\u{305}",
            "\x1b[2;5H\u{10eeee}\u{30d}\u{305}",
            "\x1b[3;5H\u{10eeee}\u{30e}\u{305}",
            "\x1b[4;1H\x1b[38;5;63m\u{10eeee}\u{305}\u{305}",
        )
        .as_bytes(),
        size,
    );
    automatic.pump().unwrap();
    let fitted = draw(&mut renderer, &automatic);
    for ([x, y], expected) in [
        ([0, 0], empty),
        ([0, 1], red),
        ([3, 1], red),
        ([4, 1], empty),
        ([8, 0], empty),
        ([9, 0], green),
        ([9, 3], green),
        ([9, 4], empty),
        ([0, 6], blue),
        ([2, 6], empty),
    ] {
        assert_eq!(pixel(&fitted, x, y), expected);
    }
    resize_cell_height(&mut automatic, 5);
    let fitted = draw(&mut renderer, &automatic);
    assert_eq!(pixel(&fitted, 0, 0), red);
    assert_eq!(pixel(&fitted, 9, 4), green);

    let mut relative = session_with_size(
        concat!(
            "\x1b_Ga=T,i=64,p=1,s=1,v=1,C=1,q=2;/wAA/w==\x1b\\",
            "\x1b_Ga=T,i=65,p=1,P=64,Q=1,H=1,s=1,v=1,q=2;AP8A/w==\x1b\\",
        )
        .as_bytes(),
        size,
    );
    relative.pump().unwrap();
    let initial = draw(&mut renderer, &relative);
    assert_eq!([pixel(&initial, 0, 0), pixel(&initial, 2, 0)], [red, green]);
    let key = relative.screen().graphics_placements()[1].placement.image;
    let child_texture = renderer.cached_texture(key).unwrap().clone();
    relative.transport.source = Cursor::new(b"\x1b[2;3H\x1b_Ga=p,i=64,p=1,C=1,q=2\x1b\\");
    relative.pump().unwrap();
    let moved = draw(&mut renderer, &relative);
    assert_eq!([pixel(&moved, 4, 2), pixel(&moved, 6, 2)], [red, green]);
    assert_eq!([pixel(&moved, 0, 0), pixel(&moved, 2, 0)], [empty; 2]);
    assert_eq!(renderer.cached_texture(key), Some(&child_texture));

    relative.transport.source = Cursor::new(b"\x1b_Ga=d,d=G,i=65\x1b\\");
    relative.pump().unwrap();
    assert!(relative.transport.written.is_empty());
    assert_eq!(draw(&mut renderer, &relative), moved);
    assert_eq!(renderer.cached_texture(key), Some(&child_texture));
    relative.transport.source = Cursor::new(b"\x1b_Ga=d,d=I,i=65\x1b\\");
    relative.pump().unwrap();
    let deleted = draw(&mut renderer, &relative);
    assert_eq!([pixel(&deleted, 4, 2), pixel(&deleted, 6, 2)], [red, empty]);

    let mut letterboxed = session_with_size(
        concat!(
            "\x1b_Ga=T,i=66,p=1,U=1,c=3,r=1,s=1,v=1,q=2;AP8A/w==\x1b\\",
            "\x1b_Ga=T,i=67,P=66,Q=1,H=1,s=1,v=1,q=2;/wAA/w==\x1b\\",
            "\x1b[38;5;66;58;5;1m\u{10eeee}\u{305}\u{305}\x1b[0m",
        )
        .as_bytes(),
        size,
    );
    letterboxed.pump().unwrap();
    let padding_only = draw(&mut renderer, &letterboxed);
    assert_eq!(pixel(&padding_only, 2, 0), empty);

    letterboxed.transport.source = Cursor::new(
        concat!(
            "\x1b[2;3H\x1b[38;5;66;58;5;1m\u{10eeee}\u{305}\u{305}\u{10eeee}\x1b[0m",
            "\x1b_Ga=d,d=P,x=2,y=1\x1b\\",
        )
        .as_bytes(),
    );
    letterboxed.pump().unwrap();
    let anchored = draw(&mut renderer, &letterboxed);
    assert_eq!(pixel(&anchored, 2, 0), empty);
    assert_eq!(pixel(&anchored, 6, 2), green);
    assert_eq!(pixel(&anchored, 8, 2), red);
    resize_cell_height(&mut letterboxed, 20);
    letterboxed.transport.source = Cursor::new(b"\x1b_Ga=d,d=P,x=2,y=1\x1b\\");
    letterboxed.pump().unwrap();
    assert!(letterboxed.graphics_image(67).is_none());
    assert!(letterboxed.graphics_image(66).is_some());

    let mut rectangle = session_with_size(
        concat!(
            "\x1b_Ga=T,i=68,U=1,c=2,r=3,f=24,s=2,v=3,C=1,q=2;/wAAAP8AAAD///////8A/wD/\x1b\\",
            "\x1b[38;5;68m\u{10eeee}\u{305}\u{10eeee}",
            "\x1b[2;1H\u{10eeee}\u{30d}\u{10eeee}\u{30d}\u{30d}",
            "\x1b[3;1H\u{10eeee}\u{30e}\u{10eeee}",
            "\x1b[1;5H\u{10eeee}\u{305}\u{10eeee}",
            "\x1b[2;5H\u{10eeee}\u{30d}\u{10eeee}",
            "\x1b[3;5H\u{10eeee}\u{30e}\u{10eeee}\x1b[0m",
        )
        .as_bytes(),
        size,
    );
    rectangle.pump().unwrap();
    let complete = draw(&mut renderer, &rectangle);
    assert_eq!(renderer.image_instance_count(), 2);
    assert_eq!(pixel(&complete, 0, 0), red);
    assert_eq!(pixel(&complete, 3, 5), [255, 0, 255, 255]);
    assert_eq!(pixel(&complete, 8, 0), red);
    assert_eq!(pixel(&complete, 11, 5), [255, 0, 255, 255]);
    assert_eq!(pixel(&complete, 4, 2), empty);

    rectangle.transport.source = Cursor::new(b"\x1b[2;1H\x1b[X");
    rectangle.pump().unwrap();
    let erased = draw(&mut renderer, &rectangle);
    assert_eq!(pixel(&erased, 0, 2), empty);
    assert_eq!(pixel(&erased, 3, 2), pixel(&complete, 3, 2));
    assert_eq!(pixel(&erased, 3, 5), pixel(&complete, 3, 5));
    assert_eq!(pixel(&erased, 8, 2), pixel(&complete, 8, 2));

    let mut hidden = session_with_size(
        concat!(
            "\x1b_Ga=T,i=69,U=1,c=1,r=1,s=1,v=1,q=2;/wAA/w==\x1b\\",
            "\x1b_Ga=T,i=70,P=69,H=1,s=1,v=1,q=2;/wAA/w==\x1b\\",
            "\x1b_Ga=f,i=70,s=1,v=1,z=20,q=2;AP8A/w==\x1b\\",
            "\x1b_Ga=a,i=70,s=3,r=1,z=10\x1b\\",
        )
        .as_bytes(),
        size,
    );
    hidden
        .set_history_limits(HistoryLimits {
            max_rows: 2,
            max_bytes: 4096,
        })
        .unwrap();
    let at = |ms| now + Duration::from_millis(ms);
    hidden.pump_with_clock(|| at(0)).unwrap();
    assert_eq!(hidden.presentation_deadline(), None);
    assert_eq!(pixel(&draw(&mut renderer, &hidden), 2, 0), empty);

    hidden.transport.source = Cursor::new("\x1b[?2026h\x1b[38;5;69m\u{10eeee}\x1b[0m".as_bytes());
    hidden.pump_with_clock(|| at(100)).unwrap();
    assert!(hidden.presentation_frame().is_none());
    assert_eq!(pixel(&read_frame(&renderer), 2, 0), empty);

    hidden.transport.source = Cursor::new(b"\x1b[?2026l");
    hidden.pump_with_clock(|| at(140)).unwrap();
    assert_eq!(hidden.presentation_deadline(), Some(at(150)));
    assert_eq!(pixel(&draw(&mut renderer, &hidden), 2, 0), red);
    hidden.pump_with_clock(|| at(150)).unwrap();
    assert_eq!(pixel(&draw(&mut renderer, &hidden), 2, 0), green);

    hidden.transport.source = Cursor::new(b"\x1b[5;1H\x1bD");
    hidden.pump_with_clock(|| at(151)).unwrap();
    assert_eq!(hidden.presentation_deadline(), None);
    assert_eq!(pixel(&draw(&mut renderer, &hidden), 2, 0), empty);
    hidden.screen.scroll_viewport_up(1);
    hidden.pump_with_clock(|| at(152)).unwrap();
    assert_eq!(hidden.screen().viewport_offset(), 1);
    assert_eq!(pixel(&draw(&mut renderer, &hidden), 2, 0), green);

    hidden.transport.source = Cursor::new(b"\x1b[+T");
    hidden.pump_with_clock(|| at(154)).unwrap();
    assert_eq!(hidden.screen().viewport_offset(), 0);
    assert_eq!(pixel(&draw(&mut renderer, &hidden), 2, 0), green);

    hidden.transport.source = Cursor::new(b"\x1b[1;1H\x1b[X");
    hidden.pump_with_clock(|| at(155)).unwrap();
    assert_eq!(hidden.presentation_deadline(), None);
    assert_eq!(pixel(&draw(&mut renderer, &hidden), 2, 0), empty);

    let mut composition = session_with_size(
        concat!(
            "\x1b_Ga=T,i=71,s=2,v=1,C=1,q=2;/wAA//8AAP8=\x1b\\",
            "\x1b_Ga=f,i=71,s=2,v=1,X=1,q=2;AAD//wAA//8=\x1b\\",
            "\x1b_Ga=f,i=71,c=2,x=1,s=1,v=1,X=1,q=2;AP8A/w==\x1b\\",
            "\x1b_Ga=a,i=71,c=3\x1b\\",
        )
        .as_bytes(),
        size,
    );
    composition.pump().unwrap();
    let before = draw(&mut renderer, &composition);
    assert_eq!(
        [pixel(&before, 0, 0), pixel(&before, 1, 0)],
        [[0, 0, 255, 255], green]
    );

    composition.transport.source = Cursor::new(b"\x1b_Ga=c,i=71,r=1,c=2,C=1,q=2\x1b\\");
    composition.pump().unwrap();
    let composed = draw(&mut renderer, &composition);
    assert_eq!(
        [pixel(&composed, 0, 0), pixel(&composed, 1, 0)],
        [red, green]
    );

    composition.transport.source =
        Cursor::new(b"\x1b_Ga=f,i=71,r=1,s=1,v=1,X=1,q=2;AAD//w==\x1b\\");
    composition.pump().unwrap();
    assert_eq!(draw(&mut renderer, &composition), composed);

    composition.transport.source =
        Cursor::new(b"\x1b[1;3H\x1b_Ga=T,i=72,s=2,v=2,C=1,q=2;/wAA//8AAP//AAD//wAA/w==\x1b\\");
    composition.pump().unwrap();
    let base = draw(&mut renderer, &composition);
    assert_eq!([pixel(&base, 5, 0), pixel(&base, 5, 1)], [red; 2]);
    composition.transport.source = Cursor::new(
        concat!(
            "\x1b_Ga=f,i=72,c=1,x=1,s=2,v=2,X=1,q=2;AP8A//////8AAP///////w==\x1b\\",
            "\x1b_Ga=a,i=72,c=2\x1b\\",
            "\x1b_Ga=a,i=71,c=2,q=2\x1b\\",
        )
        .as_bytes(),
    );
    composition.pump().unwrap();
    let clipped = draw(&mut renderer, &composition);
    assert_eq!([pixel(&clipped, 0, 0), pixel(&clipped, 1, 0)], [red; 2]);
    assert_eq!(
        [pixel(&clipped, 5, 0), pixel(&clipped, 5, 1)],
        [green, [0, 0, 255, 255]]
    );
    assert_eq!([pixel(&clipped, 4, 0), pixel(&clipped, 4, 1)], [red; 2]);

    let mut replacement = session_with_size(
        concat!(
            "\x1b_Ga=T,i=81,p=1,f=24,s=1,v=1,C=1,q=2;/wAA\x1b\\",
            "\x1b[1;2H\x1b_Ga=p,i=81,p=2,C=1,q=2\x1b\\",
            "\x1b_Ga=T,i=82,P=81,Q=1,H=2,f=24,s=1,v=1,q=2;AP8A\x1b\\",
            "\x1b[1;4H\x1b_Ga=T,i=83,f=24,s=1,v=1,C=1,q=2;AAD/\x1b\\",
        )
        .as_bytes(),
        size,
    );
    replacement.pump().unwrap();
    let placed = replacement.screen().graphics_placements();
    let [parent, child, other] = [0, 2, 3].map(|index| placed[index].placement.image);
    let original = draw(&mut renderer, &replacement);
    let other_texture = renderer.cached_texture(other).unwrap().clone();
    for wire in [
        b"\x1b[2;1H\x1b_Ga=T,i=81,p=3,f=24,s=2,v=1,o=z,m=1,C=1,q=2;eJxj+M/w\x1b\\".as_slice(),
        b"\x1b_Gm=0;n4EBAAf+Af4=\x1b\\",
    ] {
        replacement.transport.source = Cursor::new(wire);
        replacement.pump().unwrap();
        assert_eq!(draw(&mut renderer, &replacement), original);
    }
    replacement.transport.source =
        Cursor::new(b"\x1b_Ga=T,i=81,p=3,f=24,s=2,v=1,o=z,C=1,q=2;eJxj+M/wn4EBAAf+Af8=\x1b\\");
    replacement.pump().unwrap();
    let replaced = draw(&mut renderer, &replacement);
    assert_eq!(
        [
            pixel(&replaced, 0, 0),
            pixel(&replaced, 2, 0),
            pixel(&replaced, 4, 0)
        ],
        [[0; 4]; 3]
    );
    assert_eq!(
        [pixel(&replaced, 0, 2), pixel(&replaced, 1, 2)],
        [green, red]
    );
    assert!(!renderer.has_cached_image(child));
    let parent_texture = renderer.cached_texture(parent).unwrap().clone();
    assert_eq!(renderer.cached_texture(other), Some(&other_texture));

    replacement.transport.source = Cursor::new(b"\x1b_Gi=83,f=24,s=1,v=1,q=2;/wAA\x1b\\");
    replacement.pump().unwrap();
    let uploaded = draw(&mut renderer, &replacement);
    assert_eq!(pixel(&uploaded, 6, 0), [0; 4]);
    assert!(!renderer.has_cached_image(other));
    assert_eq!(renderer.cached_texture(parent), Some(&parent_texture));

    let mut hdr = session_with_size(
        concat!(
            "\x1b_Ga=T,f=100,C=1,q=2;iVBORw0KGgoAAAANSUhEUgAAAAEAAAABEAYAAABPhRjKAAAABGNJQ1AJEAABTSMj/gAAABFJREFUeJxjKDVoujOh4/9/ABMuBRov1pKzAAAAAElFTkSuQmCC\x1b\\",
            "\x1b[1;2H\x1b_Ga=T,f=100,C=1,q=2;iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAABGNJQ1AJEgABTqf3kAAAAA1JREFUeJxj2L9//38AB7sDPeNTXsQAAAAASUVORK5CYII=\x1b\\",
        )
        .as_bytes(),
        size,
    );
    hdr.pump().unwrap();
    let mapped = draw(&mut renderer, &hdr);
    for (x, expected) in [(0, [101, 193, 244, 255]), (2, [240, 240, 240, 255])] {
        let actual = pixel(&mapped, x, 0);
        assert!(
            actual
                .into_iter()
                .zip(expected)
                .all(|(a, b)| a.abs_diff(b) <= 1)
        );
    }
}

#[cfg(feature = "wgpu-renderer")]
#[test]
#[ignore = "Requires a Vulkan adapter"]
fn kitty_large_crop_uploads_only_the_needed_tiles() {
    let _gpu_guard = OFFSCREEN_GPU_TESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .unwrap();
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        required_limits: wgpu::Limits {
            max_texture_dimension_2d: 1024,
            ..Default::default()
        },
        ..Default::default()
    }))
    .unwrap();
    let mut session = session_with_size(
        b"",
        PtySize {
            pixel_width: 16,
            pixel_height: 10,
            ..PtySize::new(5, 8)
        },
    );
    let send = |session: &mut Session<Transport>, wire: &[u8]| {
        assert_eq!(
            session
                .processor
                .advance(wire, &mut session.screen, &mut session.writer)
                .unwrap(),
            wire.len()
        );
    };
    let row = "AP8A".repeat(1024);
    let first = format!("\x1b_Gi=73,f=24,s=1024,v=1024,m=1,q=2;{row}\x1b\\");
    let middle = format!("\x1b_Gm=1;{row}\x1b\\");
    let last = format!("\x1b_Gm=0;{row}\x1b\\");
    send(&mut session, first.as_bytes());
    for _ in 1..1023 {
        send(&mut session, middle.as_bytes());
    }
    send(&mut session, last.as_bytes());

    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let mut cropped = crate::ImageRenderer::new(&device, format);
    send(
        &mut session,
        b"\x1b_Ga=p,i=73,p=1,x=32,y=32,w=2,h=2,C=1,q=2\x1b\\",
    );
    let frame = session.presentation_frame().unwrap();
    let key = frame.graphics_placements()[0].placement.image;
    assert_eq!(cropped.prepare(&device, &queue, &frame), 0);
    assert_eq!(cropped.cached_texture(key).unwrap().width(), 511);
    assert_eq!(cropped.resident_image_tiles(key), 1);
    queue.submit([]);

    send(
        &mut session,
        b"\x1b_Ga=p,i=73,p=1,x=509,y=32,w=2,h=2,C=1,q=2\x1b\\",
    );
    assert_eq!(
        cropped.prepare(&device, &queue, &session.presentation_frame().unwrap()),
        0
    );
    assert_eq!(cropped.resident_image_tiles(key), 2);
    queue.submit([]);

    send(&mut session, b"\x1b_Ga=p,i=73,p=1,c=8,r=5,C=1,q=2\x1b\\");
    let mut whole = crate::ImageRenderer::new(&device, format);
    assert_eq!(
        whole.prepare(&device, &queue, &session.presentation_frame().unwrap()),
        0
    );
    assert_eq!(whole.cached_texture(key).unwrap().width(), 1024);
    queue.submit([]);

    send(
        &mut session,
        concat!(
            "\x1b_Ga=p,i=73,p=1,U=1,c=512,r=512,q=2\x1b\\",
            "\x1b[1;1H\x1b[38;5;73m\u{10eeee}\u{305}\u{305}",
            "\x1b[1;4H\u{10eeee}\u{30e}\u{30e}",
            "\x1b[1;7H\u{10eeee}\u{30d}\u{310}\x1b[0m",
        )
        .as_bytes(),
    );
    let mut virtual_crop = crate::ImageRenderer::new(&device, format);
    assert_eq!(
        virtual_crop.prepare(&device, &queue, &session.presentation_frame().unwrap()),
        0
    );
    assert_eq!(virtual_crop.cached_texture(key).unwrap().width(), 511);
    assert_eq!(virtual_crop.resident_image_tiles(key), 1);
    assert_eq!(virtual_crop.image_instance_count(), 3);
    queue.submit([]);

    send(
        &mut session,
        concat!(
            "\x1b[2J\x1b_Ga=p,i=73,p=1,U=1,c=256,r=256,q=2\x1b\\",
            "\x1b[1;1H\x1b[38;5;73m\u{10eeee}\u{305}\u{305}",
            "\x1b[1;4H\u{10eeee}\u{a8e5}\u{a8e5}\x1b[0m",
        )
        .as_bytes(),
    );
    let mut disjoint = crate::ImageRenderer::new(&device, format);
    assert_eq!(
        disjoint.prepare(&device, &queue, &session.presentation_frame().unwrap()),
        0
    );
    assert_eq!(disjoint.resident_image_tiles(key), 2);
    assert_eq!(disjoint.image_instance_count(), 2);
    queue.submit([]);

    send(
        &mut session,
        concat!(
            "\x1b[2J\x1b_Ga=p,i=73,p=1,U=1,c=5,r=5,q=2\x1b\\",
            "\x1b[38;5;73m",
            "\x1b[1;1H\u{10eeee}\u{305}\u{10eeee}\u{10eeee}\u{10eeee}\u{10eeee}",
            "\x1b[2;1H\u{10eeee}\u{30d}\u{10eeee}\u{10eeee}\u{10eeee}\u{10eeee}",
            "\x1b[3;1H\u{10eeee}\u{30e}\u{10eeee}\u{10eeee}\u{10eeee}\u{10eeee}",
            "\x1b[4;1H\u{10eeee}\u{310}\u{10eeee}\u{10eeee}\u{10eeee}\u{10eeee}",
            "\x1b[5;1H\u{10eeee}\u{312}\u{10eeee}\u{10eeee}\u{10eeee}\u{10eeee}",
            "\x1b[0m",
        )
        .as_bytes(),
    );
    let mut virtual_whole = crate::ImageRenderer::new(&device, format);
    assert_eq!(
        virtual_whole.prepare(&device, &queue, &session.presentation_frame().unwrap()),
        0
    );
    assert_eq!(virtual_whole.cached_texture(key).unwrap().width(), 1024);
    assert_eq!(virtual_whole.image_instance_count(), 1);
    queue.submit([]);
}

#[test]
fn kitty_placeholder_damage_follows_placeholder_rows() {
    let mut session = session(
        concat!(
            "\x1b_Ga=T,i=42,U=1,c=1,r=1,s=1,v=1,q=2;/wAA/w==\x1b\\",
            "\x1b[38;5;42m\u{10eeee}\x1b[0m",
        )
        .as_bytes(),
    );
    let now = Instant::now();
    let mut tracker = PresentationTracker::default();
    session.pump_with_clock(|| now).unwrap();
    tracker
        .prepare(&session.presentation_frame().unwrap(), now)
        .unwrap()
        .commit();

    session.transport.source = Cursor::new(b"\x1b[2;1Hx");
    session.pump_with_clock(|| now).unwrap();
    let frame = session.presentation_frame().unwrap();
    let update = tracker.prepare(&frame, now).unwrap();
    assert_eq!(update.damage().rows, RowDamage::Rows(&[1]));
    assert!(!update.damage().graphics_changed);
    update.commit();

    session.transport.source = Cursor::new(b"\x1b[1;1H\x1b[X");
    session.pump_with_clock(|| now).unwrap();
    let frame = session.presentation_frame().unwrap();
    assert!(
        tracker
            .prepare(&frame, now)
            .unwrap()
            .damage()
            .graphics_changed
    );
}

#[test]
fn kitty_text_line_edits_move_only_unicode_placeholders_and_their_children() {
    let mut session = session_with_size(
        concat!(
            "\x1b[3;2H\x1b_Ga=T,i=1,p=1,s=1,v=1,C=1,q=2;AAAA/w==\x1b\\",
            "\x1b_Ga=T,i=2,P=1,Q=1,H=1,s=1,v=1,q=2;AAAA/w==\x1b\\",
            "\x1b_Ga=T,i=3,p=1,U=1,c=1,r=1,s=1,v=1,q=2;AAAA/w==\x1b\\",
            "\x1b_Ga=T,i=4,P=3,Q=1,H=1,s=1,v=1,q=2;AAAA/w==\x1b\\",
            "\x1b[3;5H\x1b[38;5;3;58;5;1m\u{10eeee}\x1b[0m",
        )
        .as_bytes(),
        PtySize {
            pixel_width: 16,
            pixel_height: 12,
            ..PtySize::new(6, 8)
        },
    );
    let mut cache = lune_terminal::GraphicsPlaceholderCache::default();
    let mut check = |session: &Session<Transport>, anchored_row, placeholder_row| {
        let frame = session.presentation_frame().unwrap();
        let placements = frame.graphics_placements();
        let mut cells = Vec::new();
        cache
            .visit(
                frame.graphics_revision.unwrap(),
                placements,
                frame.viewport,
                frame.styles,
                frame.geometry.cell_pixels().unwrap(),
                Some(|run: lune_terminal::GraphicsPlaceholderRun| cells.push(run.position)),
            )
            .unwrap();
        assert_eq!(cells, [[4, placeholder_row]]);
        let origins: Vec<_> = placements
            .iter()
            .filter(|placement| !placement.is_virtual())
            .map(|placement| {
                placement
                    .origin_in_view(frame.viewport.offset(), |parent| cache.origin(parent))
                    .unwrap()
            })
            .collect();
        assert_eq!(
            origins,
            [[1, anchored_row], [2, anchored_row], [5, placeholder_row]]
        );
    };
    session.pump().unwrap();
    check(&session, 2, 2);
    for (command, anchored_row, placeholder_row) in [
        (b"\x1b[2;1H\x1b[2L".as_slice(), 2, 4),
        (b"\x1b[M", 2, 3),
        (b"\x1b[S", 1, 2),
    ] {
        session.transport.source = Cursor::new(command);
        session.pump().unwrap();
        check(&session, anchored_row, placeholder_row);
    }
}

#[test]
fn kitty_virtual_anchor_combines_retained_and_live_placeholders() {
    let mut session = session_with_size(
        concat!(
            "\x1b_Ga=T,i=42,U=1,c=2,r=2,s=1,v=1,q=2;AAAA/w==\x1b\\",
            "\x1b_Ga=T,i=43,P=42,H=1,V=1,s=1,v=1,q=2;/wAA/w==\x1b\\",
            "\x1b[1;5H\x1b[38;5;42m\u{10eeee}\u{305}\u{305}",
            "\x1b[2;2H\u{10eeee}\u{30d}\u{305}\x1b[0m\x1b[S",
        )
        .as_bytes(),
        PtySize {
            pixel_width: 16,
            pixel_height: 6,
            ..PtySize::new(3, 8)
        },
    );
    session
        .set_history_limits(HistoryLimits {
            max_rows: 4,
            max_bytes: 4096,
        })
        .unwrap();
    let mut cache = lune_terminal::GraphicsPlaceholderCache::default();
    let mut check = |session: &Session<Transport>, positions: &[[i64; 2]], origin| {
        let frame = session.presentation_frame().unwrap();
        let [parent, child] = frame.graphics_placements() else {
            panic!("Expected the virtual prototype and its child");
        };
        let mut seen = Vec::new();
        cache
            .visit(
                frame.graphics_revision.unwrap(),
                frame.graphics_placements(),
                frame.viewport,
                frame.styles,
                frame.geometry.cell_pixels().unwrap(),
                Some(|run: lune_terminal::GraphicsPlaceholderRun| seen.push(run.position)),
            )
            .unwrap();
        assert_eq!(seen, positions);
        assert_eq!(cache.origin(parent.serial()), Some(origin));
        assert_eq!(
            child.origin_in_view(frame.viewport.offset(), |serial| cache.origin(serial)),
            Some([
                origin[0] + 1,
                origin[1] + 1 + frame.viewport.offset() as i64
            ])
        );
    };

    session.pump().unwrap();
    check(&session, &[[1, 0]], [1, -1]);
    assert!(session.scroll_history(1));
    session.transport.source = Cursor::new(
        "\x1b[1;2H\x1b[X\x1b[2;7H\x1b[38;5;42m\u{10eeee}\u{30d}\u{305}\x1b[0m".as_bytes(),
    );
    session.pump().unwrap();
    check(&session, &[[4, 0], [6, 2]], [4, -1]);
    session
        .set_history_limits(HistoryLimits::default())
        .unwrap();
    check(&session, &[[6, 1]], [6, 1]);
}

const RECAPTURE_MOVED_PLACEHOLDER: &[u8] =
    "\x1b[2+T\x1b[1;2H\x1b[X\x1b[1;5H\x1b[38;5;42m\u{10eeee}\x1b[0m\x1b[2S".as_bytes();

fn session_with_historical_graphics_parent(size: PtySize) -> Session<Transport> {
    let mut session = session_with_size(
        concat!(
            "\x1b_Ga=T,i=42,U=1,c=1,r=1,s=1,v=1,q=2;AAAA/w==\x1b\\",
            "\x1b_Ga=T,i=43,P=42,H=4,V=2,s=1,v=1,q=2;/wAA/w==\x1b\\",
            "\x1b_Ga=f,i=43,s=1,v=1,z=20,q=2;AP8A/w==\x1b\\",
            "\x1b_Ga=a,i=43,r=1,z=10,s=3,q=2\x1b\\",
            "A\x1b[2;2H\x1b[38;5;42m\u{10eeee}\x1b[0m\x1b[3;1HB\x1b[3S",
        )
        .as_bytes(),
        size,
    );
    session
        .set_history_limits(HistoryLimits {
            max_rows: 4,
            max_bytes: 4096,
        })
        .unwrap();
    session
}

#[test]
fn kitty_virtual_anchor_rechecks_history_after_unscroll_and_recapture() {
    let mut session = session_with_historical_graphics_parent(PtySize {
        pixel_width: 16,
        pixel_height: 6,
        ..PtySize::new(3, 8)
    });
    let mut cache = lune_terminal::GraphicsPlaceholderCache::default();
    let origin = |session: &Session<Transport>,
                  cache: &mut lune_terminal::GraphicsPlaceholderCache| {
        let frame = session.presentation_frame().unwrap();
        let [_, child] = frame.graphics_placements() else {
            panic!("Expected the virtual prototype and its child");
        };
        cache
            .visit(
                frame.graphics_revision.unwrap(),
                frame.graphics_placements(),
                frame.viewport,
                frame.styles,
                frame.geometry.cell_pixels().unwrap(),
                None::<fn(lune_terminal::GraphicsPlaceholderRun)>,
            )
            .unwrap();
        child.origin_in_view(0, |serial| cache.origin(serial))
    };
    let mut tracker = PresentationTracker::default();
    let now = Instant::now();
    session.pump_with_clock(|| now).unwrap();
    assert_eq!(origin(&session, &mut cache), Some([5, 0]));
    assert_eq!(
        session.presentation_deadline(),
        Some(now + Duration::from_millis(10))
    );
    tracker
        .prepare(&session.presentation_frame().unwrap(), now)
        .unwrap()
        .commit();

    session.transport.source = Cursor::new(RECAPTURE_MOVED_PLACEHOLDER);
    session.pump_with_clock(|| now).unwrap();
    assert_eq!(session.screen().history().len(), 3);
    assert_eq!(origin(&session, &mut cache), Some([8, 0]));
    assert_eq!(session.presentation_deadline(), None);
    assert!(
        tracker
            .prepare(&session.presentation_frame().unwrap(), now)
            .unwrap()
            .damage()
            .graphics_changed
    );
}

#[test]
fn kitty_virtual_children_use_live_placeholder_bounds_for_coordinate_deletion() {
    for delete in [
        b"\x1b[2;3H\x1b_Ga=d,d=C\x1b\\".as_slice(),
        b"\x1b_Ga=d,d=X,x=3\x1b\\",
        b"\x1b_Ga=d,d=Y,y=2\x1b\\",
        b"\x1b_Ga=d,d=A\x1b\\",
    ] {
        let mut session = session(
            concat!(
                "\x1b_Ga=T,i=42,p=1,U=1,c=2,r=2,s=1,v=1,q=1;AAAA/w==\x1b\\",
                "\x1b_Ga=T,i=43,p=1,P=42,Q=1,H=1,V=1,s=1,v=1,q=1;/wAA/w==\x1b\\",
                "\x1b[38;5;42;58;5;1m\x1b[1;5H\u{10eeee}\u{305}\u{305}",
                "\x1b[2;2H\u{10eeee}\u{30d}\u{305}\x1b[0m",
            )
            .as_bytes(),
        );
        session.pump().unwrap();
        assert_eq!(session.screen().graphics_placements().len(), 2);
        session.transport.source = Cursor::new(delete);
        session.pump().unwrap();
        let [prototype] = session.screen().graphics_placements() else {
            panic!("The virtual prototype remains");
        };
        assert!(prototype.is_virtual());
        assert!(session.graphics_image(43).is_none());
    }
}

#[test]
fn kitty_display_erase_and_alternate_entry_clear_their_placements() {
    let mut session = session(
        concat!(
            "\x1b_Ga=T,i=1,p=1,U=1,c=1,r=1,s=1,v=1,q=1;AAAA/w==\x1b\\",
            "\x1b_Ga=T,i=2,P=1,Q=1,H=1,s=1,v=1,q=1;/wAA/w==\x1b\\",
            "\x1b_Ga=T,i=3,P=1,Q=1,H=100,s=1,v=1,q=1;AAD//w==\x1b\\",
            "\x1b_Gi=4,s=1,v=1,q=1;AAAA/w==\x1b\\",
            "\x1b_Gi=5,f=24,s=2,v=1,q=2,m=1;/wAA\x1b\\",
            "\x1b[38;5;1m\u{10eeee}\x1b[2K",
        )
        .as_bytes(),
    );
    let placements = |session: &Session<Transport>| {
        session
            .screen()
            .graphics_placements()
            .iter()
            .map(|image| image.placement.image.id())
            .collect::<Vec<_>>()
    };
    session.transport.write_budget = usize::MAX;
    session.pump().unwrap();
    assert_eq!(placements(&session), [1, 2, 3]);
    session.transport.source = Cursor::new(
        "\x1b[1;1H\u{10eeee}\x1b[2J\x1b_Gm=0,q=2;AAD/\x1b\\\x1b[1;1H\u{10eeee}".as_bytes(),
    );
    session.pump().unwrap();
    assert_eq!(placements(&session), [1, 3]);
    assert!(
        [2, 4, 5]
            .into_iter()
            .all(|id| session.graphics_image(id).is_none())
    );

    session.transport.source = Cursor::new(
        concat!(
            "\x1b[?47h",
            "\x1b_Ga=p,i=1,p=2,U=1,c=1,r=1,q=1\x1b\\",
            "\x1b_Ga=T,i=1,p=2,U=1,c=1,r=1,s=1,v=1,q=1;AAAA/w==\x1b\\",
            "\x1b_Ga=T,i=2,P=1,Q=2,H=1,s=1,v=1,q=1;AP8A/w==\x1b\\",
            "\x1b_Ga=T,i=3,P=1,Q=2,H=2,s=1,v=1,q=1;AAD//w==\x1b\\",
            "\x1b[?1047l\x1b[?47h",
        )
        .as_bytes(),
    );
    session.pump().unwrap();
    assert_eq!(placements(&session), [1, 2, 3]);
    assert_eq!(
        session.transport.written,
        b"\x1b_Gi=1,p=2;ENOENT:Image not found\x1b\\"
    );
    assert_eq!(
        session
            .graphics_image(2)
            .unwrap()
            .pixels(&mut Vec::new())
            .unwrap(),
        &[0, 255, 0, 255]
    );
    session.transport.source = Cursor::new(b"\x1b[?47l\x1b[?1049h");
    session.pump().unwrap();
    assert_eq!(placements(&session), [1]);
    assert!(session.graphics_image(1).is_some());
    assert!((2..=3).all(|id| session.graphics_image(id).is_none()));

    session.transport.source = Cursor::new(b"\x1b[?1049l");
    session.pump().unwrap();
    assert_eq!(placements(&session), [1, 3]);
    assert!(session.graphics_image(3).is_some());
    session.transport.source = Cursor::new(b"\x1bc");
    session.pump().unwrap();
    assert!(placements(&session).is_empty());
    assert!((1..=3).all(|id| session.graphics_image(id).is_none()));
}

#[test]
fn kitty_animation_deadlines_follow_loading_and_finite_loops() {
    let mut session = session(
        concat!(
            "\x1b_Ga=T,i=1,s=1,v=1,C=1,q=1;/wAA/w==\x1b\\",
            "\x1b_Ga=f,i=1,s=1,v=1,z=-1,q=1;AP8A/w==\x1b\\",
            "\x1b_Ga=f,i=1,s=1,v=1,z=20,q=1;AAD//w==\x1b\\",
            "\x1b_Ga=a,i=1,s=2,r=1,z=10\x1b\\"
        )
        .as_bytes(),
    );
    let start = Instant::now();
    let at = |millis| start + Duration::from_millis(millis);
    let current = |session: &Session<Transport>, id| -> [u8; 4] {
        session
            .graphics_image(id)
            .unwrap()
            .pixels(&mut Vec::new())
            .unwrap()
            .try_into()
            .unwrap()
    };
    session.pump_with_clock(|| at(0)).unwrap();
    assert_eq!(session.presentation_deadline(), Some(at(10)));
    session.pump_with_clock(|| at(9)).unwrap();
    assert_eq!(current(&session, 1), [255, 0, 0, 255]);
    session.pump_with_clock(|| at(10)).unwrap();
    assert_eq!(current(&session, 1), [0, 0, 255, 255]);
    assert_eq!(session.presentation_deadline(), Some(at(30)));
    session.pump_with_clock(|| at(30)).unwrap();
    assert!(session.presentation_deadline().is_none());
    session
        .pump_with_clock(|| panic!("Waiting animations have no clock work"))
        .unwrap();

    session.transport.source = Cursor::new(b"\x1b_Ga=f,i=1,s=1,v=1,z=40,q=1;/////w==\x1b\\");
    session.pump_with_clock(|| at(100)).unwrap();
    assert_eq!(current(&session, 1), [255, 255, 255, 255]);
    assert_eq!(session.presentation_deadline(), Some(at(140)));

    session.transport.source = Cursor::new(b"\x1b_Ga=a,i=1,s=3,c=1,v=3\x1b\\");
    session.pump_with_clock(|| at(200)).unwrap();
    assert_eq!(session.presentation_deadline(), Some(at(210)));
    session.pump_with_clock(|| at(275)).unwrap();
    assert_eq!(current(&session, 1), [255, 0, 0, 255]);
    assert_eq!(session.presentation_deadline(), Some(at(280)));
    session.pump_with_clock(|| at(500)).unwrap();
    assert_eq!(current(&session, 1), [255, 255, 255, 255]);
    assert!(session.presentation_deadline().is_none());

    session.transport.source = Cursor::new(b"\x1b_Ga=a,i=1,s=1,c=1\x1b\\");
    session.pump_with_clock(|| at(510)).unwrap();
    assert!(session.presentation_deadline().is_none());
    session.transport.source = Cursor::new(b"\x1b_Ga=a,i=1,s=3,v=1\x1b\\");
    session.pump_with_clock(|| at(600)).unwrap();
    session.pump_with_clock(|| at(70_615)).unwrap();
    assert_eq!(current(&session, 1), [0, 0, 255, 255]);
    assert_eq!(session.presentation_deadline(), Some(at(70_630)));

    session.transport.source = Cursor::new(
        concat!(
            "\x1b_Ga=T,i=2,s=1,v=1,C=1,q=1;AAAA/w==\x1b\\",
            "\x1b_Ga=f,i=2,s=1,v=1,z=15,q=1;//8A/w==\x1b\\",
            "\x1b_Ga=a,i=2,s=2,r=1,z=15\x1b\\"
        )
        .as_bytes(),
    );
    session.pump_with_clock(|| at(70_615)).unwrap();
    assert_eq!(session.presentation_deadline(), Some(at(70_630)));
    session.pump_with_clock(|| at(70_630)).unwrap();
    assert_eq!(current(&session, 1), [255, 255, 255, 255]);
    assert_eq!(current(&session, 2), [255, 255, 0, 255]);
    assert_eq!(session.presentation_deadline(), Some(at(70_645)));

    session.transport.source = Cursor::new(b"\x1b_Gi=2,s=1,v=1,q=1;AAAA/w==\x1b\\");
    session.pump_with_clock(|| at(70_630)).unwrap();
    assert_eq!(session.presentation_deadline(), Some(at(70_670)));
}

#[test]
fn kitty_frame_overwrite_clears_pixels_through_a_gapless_base() {
    let mut session = session(
        concat!(
            "\x1b_Ga=T,i=1,s=2,v=1,C=1,q=2;/wAA//8AAP8=\x1b\\",
            "\x1b_Ga=f,i=1,c=1,x=1,s=1,v=1,z=20,q=2;AP8A/w==\x1b\\",
            "\x1b_Ga=f,i=1,r=2,x=1,s=1,v=1,X=0,C=1,q=2;AAAAAA==\x1b\\",
            "\x1b_Ga=f,i=1,c=2,x=1,s=1,v=1,C=1,z=-1,q=2;AAAAAA==\x1b\\",
            "\x1b_Ga=f,i=1,c=3,s=1,v=1,z=30,q=2;AAD//w==\x1b\\",
            "\x1b_Ga=a,i=1,s=3,r=1,z=10,v=2\x1b\\",
        )
        .as_bytes(),
    );
    let start = Instant::now();
    let at = |ms| start + Duration::from_millis(ms);
    for (ms, next, pixels) in [
        (0, Some(10), [255, 0, 0, 255, 255, 0, 0, 255]),
        (10, Some(30), [255, 0, 0, 255, 0, 255, 0, 255]),
        (30, Some(60), [0, 0, 255, 255, 0, 0, 0, 0]),
        (60, None, [0, 0, 255, 255, 0, 0, 0, 0]),
    ] {
        session.pump_with_clock(|| at(ms)).unwrap();
        assert_eq!(session.presentation_deadline(), next.map(at));
        let mut scratch = Vec::new();
        assert_eq!(
            session
                .graphics_image(1)
                .unwrap()
                .pixels(&mut scratch)
                .unwrap(),
            pixels,
            "Frame at {ms} ms"
        );
    }
}

#[test]
fn kitty_animation_shared_frames_advance_without_presentation_damage() {
    let mut session = session(
        concat!(
            "\x1b_Ga=T,i=1,s=1,v=1,C=1,q=1;/wAA/w==\x1b\\",
            "\x1b_Ga=f,i=1,s=1,v=1,z=20,q=1;AAD//w==\x1b\\",
            "\x1b_Ga=f,i=1,s=1,v=1,z=30,q=1;AP8A/w==\x1b\\",
            "\x1b_Ga=f,i=1,s=1,v=1,z=40,q=1;AAD//w==\x1b\\",
            "\x1b_Ga=c,i=1,r=1,c=2,C=1,q=1\x1b\\",
            "\x1b_Ga=c,i=1,r=3,c=4,C=1,q=1\x1b\\",
            "\x1b_Ga=a,i=1,s=3,r=1,z=10,v=2,q=1\x1b\\",
        )
        .as_bytes(),
    );
    let start = Instant::now();
    let at = |ms| start + Duration::from_millis(ms);
    let mut tracker = PresentationTracker::default();
    let red = [255, 0, 0, 255];
    let green = [0, 255, 0, 255];
    for (ms, next, color, changed) in [
        (0, Some(10), red, true),
        (10, Some(30), red, false),
        (30, Some(60), green, true),
        (60, Some(100), green, false),
        (100, None, green, false),
    ] {
        session.pump_with_clock(|| at(ms)).unwrap();
        assert!(session.transport.written.is_empty());
        assert_eq!(session.presentation_deadline(), next.map(at));
        let frame = session.presentation_frame().unwrap();
        let damage = submit_frame(&mut tracker, &frame, at(ms), |damage| {
            damage.graphics_changed
        });
        assert_eq!(damage, changed, "Graphics damage at {ms} ms");
        assert_eq!(
            session
                .graphics_image(1)
                .unwrap()
                .pixels(&mut Vec::new())
                .unwrap(),
            color
        );
    }
}

#[test]
fn kitty_animation_waits_for_placement_and_only_damages_visible_images() {
    let mut session = session(
        concat!(
            "\x1b_Ga=T,i=2,s=1,v=1,C=1,q=2;/////w==\x1b\\",
            "\x1b_Gi=1,s=1,v=1,q=2;/wAA/w==\x1b\\",
            "\x1b_Ga=f,i=1,s=1,v=1,z=20,q=2;AP8A/w==\x1b\\",
            "\x1b_Ga=a,i=1,s=3,r=1,z=10\x1b\\",
        )
        .as_bytes(),
    );
    session
        .set_history_limits(HistoryLimits {
            max_rows: 4,
            max_bytes: 4096,
        })
        .unwrap();
    let start = Instant::now();
    let at = |ms| start + Duration::from_millis(ms);
    let current = |session: &Session<Transport>| -> [u8; 4] {
        session
            .graphics_image(1)
            .unwrap()
            .pixels(&mut Vec::new())
            .unwrap()
            .try_into()
            .unwrap()
    };
    session
        .pump_with_clock(|| panic!("An unplaced animation must not start a timer"))
        .unwrap();
    assert_eq!(session.presentation_deadline(), None);
    let static_stamp = session.presentation_frame().unwrap().graphics_stamp();

    session.transport.source = Cursor::new(b"\x1b_Ga=a,i=1,c=2\x1b\\");
    session.pump_with_clock(|| at(25)).unwrap();
    assert_eq!(current(&session), [0, 255, 0, 255]);
    assert_eq!(
        session.presentation_frame().unwrap().graphics_stamp(),
        static_stamp
    );

    session.transport.source = Cursor::new(b"\x1b_Ga=p,i=1,C=1,q=2\x1b\\");
    session.pump_with_clock(|| at(100)).unwrap();
    assert_eq!(current(&session), [0, 255, 0, 255]);
    assert_eq!(session.presentation_deadline(), Some(at(120)));

    session.transport.source = Cursor::new(b"\x1b[?1049h");
    session.pump_with_clock(|| at(104)).unwrap();
    assert_eq!(session.presentation_deadline(), None);
    assert!(session.graphics_image(1).is_none());
    session.transport.source = Cursor::new(b"\x1b[?1049l");
    session.pump_with_clock(|| at(112)).unwrap();
    assert_eq!(session.presentation_deadline(), Some(at(120)));
    session.pump_with_clock(|| at(137)).unwrap();
    assert_eq!(current(&session), [0, 255, 0, 255]);
    assert_eq!(session.presentation_deadline(), Some(at(150)));

    session.transport.source = Cursor::new(b"\x1b_Ga=d,d=i,i=1\x1b\\");
    session.pump_with_clock(|| at(145)).unwrap();
    assert_eq!(session.presentation_deadline(), None);
    session.pump_with_clock(|| at(800)).unwrap();
    assert_eq!(current(&session), [0, 255, 0, 255]);

    session.transport.source = Cursor::new(b"\x1b_Ga=p,i=1,C=1,q=2\x1b\\");
    session.pump_with_clock(|| at(1000)).unwrap();
    assert_eq!(current(&session), [255, 0, 0, 255]);
    assert_eq!(session.presentation_deadline(), Some(at(1010)));

    session.transport.source = Cursor::new(b"\x1b[3;1H\x1bD");
    session.pump_with_clock(|| at(1005)).unwrap();
    assert_eq!(session.presentation_deadline(), None);
    assert!(session.scroll_history(1));
    assert!(session.presentation_deadline().is_some());
    session.follow_output();
    assert_eq!(session.presentation_deadline(), None);
}

#[test]
fn kitty_relative_animation_follows_viewport_and_hidden_parent_edits() {
    let mut session = session(
        concat!(
            "\x1b_Ga=T,i=1,U=1,c=1,r=1,s=1,v=1,q=2;AAAA/w==\x1b\\",
            "\x1b_Ga=T,i=2,P=1,H=6,V=-3,s=1,v=1,q=2;/wAA/w==\x1b\\",
            "\x1b_Ga=f,i=2,s=1,v=1,z=10,q=2;AP8A/w==\x1b\\",
            "\x1b_Ga=a,i=2,s=3,r=1,z=10,q=2\x1b\\",
            "\x1b[3;1H\x1bD\x1b[38;5;1m\u{10eeee}\x1b[0m",
        )
        .as_bytes(),
    );
    session
        .set_history_limits(HistoryLimits {
            max_rows: 2,
            max_bytes: 4096,
        })
        .unwrap();
    let start = Instant::now();
    let at = |ms| start + Duration::from_millis(ms);
    let current = |session: &Session<Transport>| -> [u8; 4] {
        session
            .graphics_image(2)
            .unwrap()
            .pixels(&mut Vec::new())
            .unwrap()
            .try_into()
            .unwrap()
    };

    session.pump_with_clock(|| at(0)).unwrap();
    assert_eq!(session.presentation_deadline(), None);
    session.screen.scroll_viewport_up(1);
    session.pump_with_clock(|| at(20)).unwrap();
    assert_eq!(session.presentation_deadline(), Some(at(30)));
    session.pump_with_clock(|| at(30)).unwrap();
    assert_eq!(current(&session), [0, 255, 0, 255]);

    session.transport.source =
        Cursor::new("\x1b[3;1H\x1b[X\x1b[3;4H\x1b[38;5;1m\u{10eeee}\x1b[0m".as_bytes());
    session.pump_with_clock(|| at(31)).unwrap();
    assert_eq!(session.presentation_deadline(), None);
    session.pump_with_clock(|| at(99)).unwrap();
    assert_eq!(current(&session), [0, 255, 0, 255]);

    session.transport.source =
        Cursor::new("\x1b[3;4H\x1b[X\x1b[3;1H\x1b[38;5;1m\u{10eeee}\x1b[0m".as_bytes());
    session.pump_with_clock(|| at(100)).unwrap();
    assert_eq!(session.presentation_deadline(), Some(at(110)));
    session.screen.scroll_viewport_to_bottom();
    session.pump_with_clock(|| at(101)).unwrap();
    assert_eq!(session.presentation_deadline(), None);
}

#[test]
fn kitty_animation_controls_keep_frame_selection_and_gap_independent() {
    let mut session = session(
        concat!(
            "\x1b_Ga=T,i=1,s=1,v=1,C=1,q=1;/wAA/w==\x1b\\",
            "\x1b_Ga=f,i=1,s=1,v=1,z=10,q=1;AP8A/w==\x1b\\",
            "\x1b_Ga=f,i=1,s=1,v=1,z=40,q=1;AAD//w==\x1b\\",
            "\x1b_Ga=a,i=1,s=3,r=1,z=20\x1b\\",
        )
        .as_bytes(),
    );
    session.transport.write_budget = usize::MAX;
    let start = Instant::now();
    let at = |millis| start + Duration::from_millis(millis);
    session.pump_with_clock(|| at(0)).unwrap();

    let mut pump = |millis, command| {
        session.transport.source = Cursor::new(command);
        session.pump_with_clock(|| at(millis)).unwrap();
        let image = session.graphics_image(1).unwrap();
        let pixel: [u8; 4] = image.pixels(&mut Vec::new()).unwrap().try_into().unwrap();
        assert!(session.transport.written.is_empty());
        (pixel, session.presentation_deadline())
    };
    assert_eq!(
        pump(5, b"\x1b_Ga=a,i=1,c=1\x1b\\"),
        ([255, 0, 0, 255], Some(at(20)))
    );
    assert_eq!(pump(20, b""), ([0, 255, 0, 255], Some(at(30))));
    assert_eq!(
        pump(25, b"\x1b_Ga=a,i=1,c=999,r=2,z=30\x1b\\"),
        ([0, 255, 0, 255], Some(at(50)))
    );
    assert_eq!(
        pump(27, b"\x1b_Ga=a,i=1,c=3,r=999,z=5\x1b\\"),
        ([0, 0, 255, 255], Some(at(67)))
    );
}

#[test]
fn kitty_frame_edits_restart_only_the_displayed_frames_deadline() {
    let mut session = session(
        concat!(
            "\x1b_Ga=T,i=1,s=2,v=1,C=1,q=1;/wAA/wD/AP8=\x1b\\",
            "\x1b_Ga=f,i=1,s=2,v=1,z=10,q=1;AAD//wAA//8=\x1b\\",
            "\x1b_Ga=a,i=1,s=3,r=1,z=20\x1b\\",
        )
        .as_bytes(),
    );
    let start = Instant::now();
    let at = |ms| start + Duration::from_millis(ms);
    session.transport.write_budget = usize::MAX;
    session.pump_with_clock(|| at(0)).unwrap();
    let mut scratch = Vec::new();
    let mut pump = |ms, command, pixels: [u8; 8], due: Option<u64>, reply: &[u8]| {
        session.transport.source = Cursor::new(command);
        session.pump_with_clock(|| at(ms)).unwrap();
        let image = session.graphics_image(1).unwrap();
        assert_eq!(image.pixels(&mut scratch).unwrap(), pixels);
        assert_eq!(session.presentation_deadline(), due.map(at));
        assert_eq!(session.transport.written, reply);
        session.transport.written.clear();
    };
    let red_green = [255, 0, 0, 255, 0, 255, 0, 255];
    let green_green = [0, 255, 0, 255, 0, 255, 0, 255];
    let white_green = [255, 255, 255, 255, 0, 255, 0, 255];
    let blue_red = [0, 0, 255, 255, 255, 0, 0, 255];
    let invalid = b"\x1b_Gi=1;EINVAL:Invalid animation frame region\x1b\\";
    pump(0, b"", red_green, Some(20), b"");
    pump(
        3,
        b"\x1b_Ga=c,i=1,r=1,c=2,x=1,w=1,h=1,C=1,q=1\x1b\\",
        red_green,
        Some(20),
        b"",
    );
    pump(
        5,
        b"\x1b_Ga=c,i=1,r=1,c=1\x1b\\",
        red_green,
        Some(20),
        invalid,
    );
    pump(
        7,
        b"\x1b_Ga=c,i=1,r=2,c=1,X=4294967295,w=1,h=1\x1b\\",
        red_green,
        Some(20),
        invalid,
    );
    pump(
        18,
        b"\x1b_Ga=c,i=1,r=1,c=1,X=1,w=1,h=1,C=1,q=1\x1b\\",
        green_green,
        Some(38),
        b"",
    );
    pump(
        32,
        b"\x1b_Ga=f,i=1,r=1,s=1,v=1,X=1,q=1;/////w==\x1b\\",
        white_green,
        Some(52),
        b"",
    );
    pump(52, b"", blue_red, Some(62), b"");
    pump(55, b"\x1b_Ga=a,i=1,v=2\x1b\\", blue_red, Some(62), b"");
    pump(62, b"", blue_red, None, b"");
    let white_red = [255, 255, 255, 255, 255, 0, 0, 255];
    pump(
        70,
        b"\x1b_Ga=c,i=1,r=1,c=2,w=1,h=1,C=1,q=1\x1b\\",
        white_red,
        None,
        b"",
    );
    pump(72, b"\x1b_Ga=a,i=1,v=1\x1b\\", white_red, Some(80), b"");
}

#[test]
fn kitty_streamed_animation_preserves_frame_time_across_loading_waits() {
    let mut session = session(
        concat!(
            "\x1b_Ga=T,i=1,s=1,v=1,C=1,q=1;/wAA/w==\x1b\\",
            "\x1b_Ga=a,i=1,s=2,r=1,z=10\x1b\\",
        )
        .as_bytes(),
    );
    let start = Instant::now();
    let at = |millis| start + Duration::from_millis(millis);
    session.pump_with_clock(|| at(0)).unwrap();
    assert_eq!(session.presentation_deadline(), None);

    let mut pump = |millis, command| {
        session.transport.source = Cursor::new(command);
        session.pump_with_clock(|| at(millis)).unwrap();
        let image = session.graphics_image(1).unwrap();
        let pixel: [u8; 4] = image.pixels(&mut Vec::new()).unwrap().try_into().unwrap();
        (pixel, session.presentation_deadline())
    };
    assert_eq!(
        pump(
            100,
            concat!(
                "\x1b_Ga=f,i=1,s=1,v=1,z=20,q=1;AP8A/w==\x1b\\",
                "\x1b_Ga=f,i=1,s=1,v=1,z=30,q=1;AAD//w==\x1b\\",
            )
            .as_bytes(),
        ),
        ([0, 255, 0, 255], Some(at(120)))
    );
    assert_eq!(pump(120, b""), ([0, 0, 255, 255], Some(at(150))));
    assert_eq!(
        pump(155, b"\x1b_Ga=f,i=1,s=1,v=1,z=40,q=1;/////w==\x1b\\"),
        ([255, 255, 255, 255], Some(at(195)))
    );
    assert_eq!(pump(195, b""), ([255, 255, 255, 255], None));
    assert_eq!(
        pump(205, b"\x1b_Ga=a,i=1,r=4,z=80\x1b\\"),
        ([255, 255, 255, 255], Some(at(235)))
    );
    assert_eq!(
        pump(215, b"\x1b_Ga=f,i=1,s=1,v=1,z=25,q=1;/wD//w==\x1b\\"),
        ([255, 255, 255, 255], Some(at(235)))
    );
    assert_eq!(pump(240, b""), ([255, 0, 255, 255], Some(at(260))));
    assert_eq!(
        pump(300, b"\x1b_Ga=a,i=1,c=1,s=3,v=3\x1b\\"),
        ([255, 0, 0, 255], Some(at(310)))
    );
    assert_eq!(pump(700, b""), ([255, 0, 255, 255], None));
    assert_eq!(
        pump(700, b"\x1b_Ga=a,i=1,v=1,r=5,z=200\x1b\\"),
        ([255, 0, 255, 255], Some(at(805)))
    );
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
fn kitty_synchronized_output_defers_animation_until_presentation() {
    for release_by_timeout in [false, true] {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut session = session(
            concat!(
                "\x1b_Ga=T,i=1,s=1,v=1,C=1,q=2;/wAA/w==\x1b\\",
                "\x1b_Ga=f,i=1,s=1,v=1,z=20,q=2;AP8A/w==\x1b\\",
                "\x1b_Ga=a,i=1,s=3,r=1,z=20\x1b\\",
            )
            .as_bytes(),
        );
        let pixel = |session: &Session<Transport>, id| -> [u8; 4] {
            session
                .graphics_image(id)
                .unwrap()
                .pixels(&mut Vec::new())
                .unwrap()
                .try_into()
                .unwrap()
        };
        session.pump_with_clock(|| at(0)).unwrap();
        assert_eq!(session.presentation_deadline(), Some(at(20)));

        session.transport.source = Cursor::new(
            concat!(
                "\x1b[?2026h\x1b[1;2H\x1b_Ga=T,i=2,s=1,v=1,C=1;AAD//w==\x1b\\",
                "\x1b_Ga=f,i=2,s=1,v=1,z=8,q=2;/////w==\x1b\\",
                "\x1b_Ga=a,i=2,s=3,r=1,z=8\x1b\\",
            )
            .as_bytes(),
        );
        session.transport.write_budget = usize::MAX;
        session.pump_with_clock(|| at(5)).unwrap();
        assert_eq!(session.transport.written, b"\x1b_Gi=2;OK\x1b\\");
        assert_eq!(
            session.presentation_deadline(),
            Some(at(5) + SYNCHRONIZED_OUTPUT_TIMEOUT)
        );
        session.pump_with_clock(|| at(60)).unwrap();
        assert!(session.presentation_frame().is_none());
        assert_eq!(pixel(&session, 1), [255, 0, 0, 255]);
        assert_eq!(pixel(&session, 2), [0, 0, 255, 255]);

        let released = if release_by_timeout {
            at(5) + SYNCHRONIZED_OUTPUT_TIMEOUT
        } else {
            session.transport.source = Cursor::new(b"\x1b[?2026l");
            at(70)
        };
        session.pump_with_clock(|| released).unwrap();
        assert!(session.presentation_frame().is_some());
        assert_eq!(pixel(&session, 1), [0, 255, 0, 255]);
        session
            .pump_with_clock(|| released + Duration::from_millis(7))
            .unwrap();
        assert_eq!(pixel(&session, 2), [0, 0, 255, 255]);
        session
            .pump_with_clock(|| released + Duration::from_millis(8))
            .unwrap();
        assert_eq!(pixel(&session, 2), [255, 255, 255, 255]);
    }
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

fn submit_frame<T>(
    tracker: &mut PresentationTracker,
    frame: &PresentationFrame<'_>,
    now: Instant,
    submit: impl FnOnce(PresentationDamage<'_>) -> T,
) -> T {
    let prepared = tracker.prepare(frame, now).unwrap();
    let submitted = submit(prepared.damage());
    prepared.commit();
    submitted
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
    submit_frame(&mut tracker, &frame, now, |damage| {
        assert_eq!(damage.rows, RowDamage::All)
    });
    submit_frame(&mut tracker, &frame, now, |damage| {
        assert_eq!(damage.rows, RowDamage::Rows(&[]))
    });

    session.transport.source = Cursor::new(b"\x1b[2;1HB");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    submit_frame(&mut tracker, &frame, now, |damage| {
        assert_eq!(damage.rows, RowDamage::Rows(&[1]))
    });
    let retained_row = frame.viewport.row(1).unwrap().revision();

    session.transport.source = Cursor::new(b"\x1b[3;1H\r\nd");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    assert_eq!(frame.viewport.row(0).unwrap().revision(), retained_row);
    submit_frame(&mut tracker, &frame, now, |damage| {
        assert_eq!(damage.rows, RowDamage::Rows(&[0, 1, 2]))
    });

    assert!(session.scroll_history(1));
    let frame = session.presentation_frame().unwrap();
    submit_frame(&mut tracker, &frame, now, |damage| {
        assert_eq!(damage.rows, RowDamage::Rows(&[0, 1, 2]));
        assert_eq!(damage.cursor.unwrap().after, None);
    });

    session.screen.resize(10, 4, StyleId::default()).unwrap();
    session.geometry = GridGeometry::from_pty_size(PtySize::new(4, 10));
    let frame = session.presentation_frame().unwrap();
    submit_frame(&mut tracker, &frame, now, |damage| {
        assert_eq!(damage.rows, RowDamage::All)
    });
    submit_frame(&mut tracker, &frame, now, |damage| {
        assert_eq!(damage.rows, RowDamage::Rows(&[]))
    });
    tracker.invalidate();
    submit_frame(&mut tracker, &frame, now, |damage| {
        assert_eq!(damage.rows, RowDamage::All)
    });
}

#[test]
fn presentation_damage_separates_cursor_and_resolved_text_colors() {
    let mut session = session(b"");
    let mut tracker = PresentationTracker::default();
    let now = Instant::now();
    let frame = session.presentation_frame().unwrap();
    let initial = submit_frame(&mut tracker, &frame, now, |damage| {
        damage.cursor.unwrap().after.unwrap()
    });

    session.transport.source = Cursor::new(b"\x1b[2;3H\x1b[6 q\x1b]12;#010203\x07");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    let cursor = submit_frame(&mut tracker, &frame, now, |damage| {
        assert_eq!(damage.rows, RowDamage::Rows(&[]));
        damage.cursor.unwrap()
    });
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
    let frame = session.presentation_frame().unwrap();
    submit_frame(&mut tracker, &frame, now, |damage| {
        assert_eq!(damage.rows, RowDamage::All);
        assert_eq!(
            damage.cursor.unwrap(),
            CursorChange {
                before: Some(moved),
                after: None
            }
        );
    });

    let base = Rgb::default();
    session.set_theme(&Palette::new([base; 256], base, color, base), None);
    let frame = session.presentation_frame().unwrap();
    submit_frame(&mut tracker, &frame, now, |damage| {
        assert_eq!(damage.rows, RowDamage::Rows(&[]));
        assert_eq!(damage.cursor, None);
    });

    session.transport.source = Cursor::new(b"\x1b[?5h");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    submit_frame(&mut tracker, &frame, now, |damage| {
        assert_eq!(damage.rows, RowDamage::All);
        assert_eq!(damage.cursor, None);
    });
}

#[test]
fn paint_commands_replay_damage_and_keep_the_cursor_on_its_own_layer() {
    #[derive(Debug, PartialEq, Eq)]
    enum Paint {
        Background(u32),
        Text(PixelPosition),
        Cursor(Option<(PixelPosition, Option<char>)>),
    }

    fn trace(frame: &PresentationFrame<'_>, damage: PresentationDamage<'_>) -> Vec<Paint> {
        let mut commands = Vec::new();
        frame
            .visit_paint_commands(damage, NonZeroU32::new(2).unwrap(), |command| {
                commands.push(match command {
                    PaintCommand::Background(background) => {
                        Paint::Background(background.rect.origin.y)
                    }
                    PaintCommand::Text { clip, run } => {
                        assert!(!run.style.style.hidden);
                        Paint::Text(clip.origin)
                    }
                    PaintCommand::Cursor(cursor) => Paint::Cursor(cursor.map(|cursor| {
                        let character = cursor.glyph.and_then(|glyph| glyph.unit.cell.character());
                        (cursor.rect.origin, character)
                    })),
                });
                Ok::<_, std::convert::Infallible>(())
            })
            .unwrap();
        commands
    }
    let thickness = NonZeroU32::new(2).unwrap();
    let mut session = session(b"A\x1b[2;1H\x1b[8mB\x1b[0m\x1b[1;1H");
    session.geometry = GridGeometry::fit(pixels(64, 48), pixels(8, 16)).unwrap();
    session.pump().unwrap();
    let mut tracker = PresentationTracker::default();
    let now = Instant::now();
    let frame = session.presentation_frame().unwrap();
    let prepared = tracker.prepare(&frame, now).unwrap();
    let damage = prepared.damage();

    let mut received = 0;
    let interrupted = frame.visit_paint_commands(damage, thickness, |_| {
        received += 1;
        Err("submission interrupted")
    });
    assert!(matches!(
        interrupted,
        Err(PaintError::Sink("submission interrupted"))
    ));
    assert_eq!(received, 1);
    let top_left = PixelPosition { x: 0, y: 0 };
    assert_eq!(
        trace(&frame, damage),
        [
            Paint::Background(0),
            Paint::Text(top_left),
            Paint::Background(16),
            Paint::Text(PixelPosition { x: 8, y: 16 }),
            Paint::Background(32),
            Paint::Text(PixelPosition { x: 0, y: 32 }),
            Paint::Cursor(Some((top_left, Some('A')))),
        ]
    );
    prepared.commit();

    session.transport.source = Cursor::new(b"Z\x1b[1;1H");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    assert_eq!(
        submit_frame(&mut tracker, &frame, now, |damage| {
            assert_eq!(damage.cursor, None);
            trace(&frame, damage)
        }),
        [
            Paint::Background(0),
            Paint::Text(top_left),
            Paint::Cursor(Some((top_left, Some('Z'))))
        ]
    );

    session.transport.source = Cursor::new(b"\x1b[3;1H");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    assert_eq!(
        submit_frame(&mut tracker, &frame, now, |damage| trace(&frame, damage)),
        [Paint::Cursor(Some((PixelPosition { x: 0, y: 32 }, None)))]
    );

    session.transport.source = Cursor::new(b"\x1b[?25l");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    assert_eq!(
        submit_frame(&mut tracker, &frame, now, |damage| trace(&frame, damage)),
        [Paint::Cursor(None)]
    );
    assert!(submit_frame(&mut tracker, &frame, now, |damage| trace(&frame, damage)).is_empty());
}

#[test]
fn raster_surface_restores_the_cursor_and_clips_text_and_backgrounds() {
    use crate::{CoverageMask, GlyphMask, GlyphSource, RasterSurface};
    use lune_terminal::DisplayUnit;

    struct Glyphs;

    impl GlyphSource for Glyphs {
        fn glyph(
            &mut self,
            unit: DisplayUnit<'_>,
            style: &Style,
            cell: PixelSize,
        ) -> Option<GlyphMask<'_>> {
            assert_eq!(cell, pixels(2, 2));
            match unit.text {
                CellText::Extended("水\u{301}") => {
                    assert!(style.bold);
                    assert_eq!((unit.column, unit.cell.width()), (1, 2));
                    Some(GlyphMask {
                        coverage: CoverageMask::new(pixels(2, 1), 2, &[255, 255]).unwrap(),
                        offset: (-1, 0),
                    })
                }
                CellText::Scalar('Z') => Some(GlyphMask {
                    coverage: CoverageMask::new(pixels(1, 1), 1, &[255]).unwrap(),
                    offset: (0, 0),
                }),
                _ => panic!("Unexpected display unit"),
            }
        }
    }

    fn present(
        session: &Session<Transport>,
        tracker: &mut PresentationTracker,
        batch: &mut PaintBatch,
        surface: &mut RasterSurface<'_>,
    ) {
        let frame = session.presentation_frame().unwrap();
        let prepared = tracker.prepare(&frame, Instant::now()).unwrap();
        batch
            .capture(&frame, prepared.damage(), NonZeroU32::new(1).unwrap())
            .unwrap();
        surface.apply_glyphs(batch, &mut Glyphs);
        prepared.commit();
    }

    let mut session = session("\x1b[1;2H\x1b[1m水\u{301}\x1b[0m\x1b[1;3H".as_bytes());
    session.geometry = GridGeometry::fit(pixels(16, 6), pixels(2, 2)).unwrap();
    let background = Rgb {
        red: 5,
        green: 6,
        blue: 7,
    };
    let foreground = Rgb {
        red: 200,
        green: 201,
        blue: 202,
    };
    let cursor = Rgb {
        red: 10,
        green: 20,
        blue: 30,
    };
    let mut indexed = [background; 256];
    indexed[1] = cursor;
    session.set_theme(&Palette::new(indexed, foreground, background, cursor), None);
    session.pump().unwrap();
    let mut tracker = PresentationTracker::default();
    let mut batch = PaintBatch::default();
    let padding = Rgb::default();
    let mut content = [padding; 90];
    let mut output = [padding; 90];
    {
        let size = pixels(15, 5);
        assert!(RasterSurface::new(size, 14, &mut content, &mut output).is_none());
        let mut surface = RasterSurface::new(size, 18, &mut content, &mut output).unwrap();
        present(&session, &mut tracker, &mut batch, &mut surface);
        assert_eq!(
            &surface.pixels()[..7],
            &[
                background, background, background, cursor, cursor, cursor, background
            ]
        );

        session.transport.source = Cursor::new(b"\x1b[2;1H");
        session.pump().unwrap();
        present(&session, &mut tracker, &mut batch, &mut surface);
        assert_eq!(
            &surface.pixels()[..7],
            &[
                background, background, foreground, background, background, background, background
            ]
        );
        assert_eq!(&surface.pixels()[36..39], &[cursor, cursor, background]);

        session.transport.source = Cursor::new(b"\x1b[3;8H\x1b[41mZ\x1b[2;1H");
        session.pump().unwrap();
        present(&session, &mut tracker, &mut batch, &mut surface);
        assert_eq!(&surface.pixels()[36..39], &[cursor, cursor, background]);
        assert_eq!(&surface.pixels()[85..], &[background, foreground]);

        session.transport.source = Cursor::new(b"\x1b[?25l");
        session.pump().unwrap();
        present(&session, &mut tracker, &mut batch, &mut surface);
        assert_eq!(&surface.pixels()[36..39], &[background; 3]);
        assert_eq!(surface.pixels()[86], foreground);
    }
    for row in output
        .as_chunks::<18>()
        .0
        .iter()
        .chain(content.as_chunks::<18>().0)
    {
        assert_eq!(&row[15..], &[padding; 3]);
    }
}

#[test]
fn paint_batch_retains_text_rendition_and_cursor_after_the_source_changes() {
    let mut session = session("A\x1b[2;1H\x1b[1;31;44m水\u{301}\x1b[2;2H".as_bytes());
    session.geometry = GridGeometry::fit(pixels(64, 48), pixels(8, 16)).unwrap();
    let red = Rgb {
        red: 180,
        green: 5,
        blue: 10,
    };
    let blue = Rgb {
        red: 3,
        green: 20,
        blue: 160,
    };
    let base = Rgb::default();
    let mut indexed = [base; 256];
    indexed[1] = red;
    indexed[4] = blue;
    session.set_theme(&Palette::new(indexed, base, base, base), None);
    session.pump().unwrap();

    let mut tracker = PresentationTracker::default();
    let mut batch = PaintBatch::default();
    let now = Instant::now();
    let thickness = NonZeroU32::new(2).unwrap();
    let frame = session.presentation_frame().unwrap();
    let prepared = tracker.prepare(&frame, now).unwrap();
    batch.capture(&frame, prepared.damage(), thickness).unwrap();
    prepared.commit();

    session.transport.source = Cursor::new(b"\x1b[2;1H\x1b[0mZ\x1b[2;1H");
    session.pump().unwrap();
    session.set_theme(&Palette::new([base; 256], base, base, base), None);
    let mut commands = batch.commands();
    assert!(matches!(commands.next(), Some(PaintCommand::Background(_))));
    let PaintCommand::Text { run, .. } = commands.next().unwrap() else {
        panic!("Expected the first text run");
    };
    assert_eq!(run.units().next().unwrap().text, CellText::Scalar('A'));
    let PaintCommand::Background(background) = commands.next().unwrap() else {
        panic!("Expected the stored row background");
    };
    assert_eq!(background.color, blue);
    assert_eq!(background.rect.size, pixels(16, 16));
    let PaintCommand::Text { run, .. } = commands
        .find(|command| matches!(command, PaintCommand::Text { .. }))
        .unwrap()
    else {
        panic!("Expected the extended text run");
    };
    assert_eq!(run.columns(), 0..2);
    let unit = run.units().next().unwrap();
    assert_eq!(unit.text, CellText::Extended("水\u{301}"));
    assert_eq!(unit.cell.width(), 2);
    assert!(run.style.style.bold);
    assert_eq!(run.style.foreground, red);
    let PaintCommand::Cursor(Some(cursor)) = commands.last().unwrap() else {
        panic!("Expected the stored cursor");
    };
    let glyph = cursor.glyph.unwrap();
    assert_eq!(glyph.unit, unit);
    assert_eq!(glyph.color, blue);
    assert!(glyph.style.bold);

    let frame = session.presentation_frame().unwrap();
    let prepared = tracker.prepare(&frame, now).unwrap();
    batch.capture(&frame, prepared.damage(), thickness).unwrap();
    let PaintCommand::Cursor(Some(cursor)) = batch.commands().last().unwrap() else {
        panic!("Expected the new cursor");
    };
    assert_eq!(cursor.glyph.unwrap().unit.text, CellText::Scalar('Z'));
    prepared.commit();
}

#[test]
fn failed_paint_capture_leaves_the_reusable_batch_empty() {
    let mut session = session(b"A\x1b[2;1H\x1b[1mB");
    session.geometry = GridGeometry::fit(pixels(64, 48), pixels(8, 16)).unwrap();
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    let mut tracker = PresentationTracker::default();
    let mut batch = PaintBatch::default();
    let thickness = NonZeroU32::new(2).unwrap();
    let prepared = tracker.prepare(&frame, Instant::now()).unwrap();
    let damage = prepared.damage();
    batch.capture(&frame, damage, thickness).unwrap();
    assert!(batch.commands().len() > 0);

    let detached_styles = StyleTable::new(1).unwrap();
    let detached = PresentationFrame {
        styles: &detached_styles,
        ..frame
    };
    assert!(matches!(
        batch.capture(&detached, damage, thickness),
        Err(PaintError::Style(StyleError::InvalidId(_)))
    ));
    assert_eq!(batch.commands().len(), 0);
    batch.capture(&frame, damage, thickness).unwrap();
    assert!(matches!(
        batch.commands().last(),
        Some(PaintCommand::Cursor(Some(_)))
    ));
    prepared.commit();
}

#[test]
fn failed_submission_repaints_and_retains_the_last_committed_cursor() {
    let mut session = session(b"A\x1b[1;1H");
    session.geometry = GridGeometry::fit(pixels(64, 48), pixels(8, 16)).unwrap();
    session.pump().unwrap();
    let mut tracker = PresentationTracker::default();
    let start = Instant::now();
    let phase = Duration::from_millis(40);
    let thickness = NonZeroU32::new(2).unwrap();
    tracker.set_cursor_blink_interval(phase, start);
    let frame = session.presentation_frame().unwrap();
    tracker.prepare(&frame, start).unwrap().commit();
    let original = tracker.displayed_cursor().unwrap();

    session.transport.source = Cursor::new(b"\x1b[2;1HB\x1b[2;1H");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    let now = start + Duration::from_millis(10);
    {
        let prepared = tracker.prepare(&frame, now).unwrap();
        let damage = prepared.damage();
        assert_eq!(damage.rows, RowDamage::Rows(&[1]));
        assert_eq!(damage.displayed_cursor.unwrap().position.row, 1);
        let result = frame.visit_paint_commands(damage, thickness, |command| match command {
            PaintCommand::Background(_) => Ok(()),
            _ => Err("surface unavailable"),
        });
        assert!(matches!(
            result,
            Err(PaintError::Sink("surface unavailable"))
        ));
    }
    assert_eq!(tracker.displayed_cursor(), Some(original));
    assert_eq!(tracker.cursor_deadline(), Some(start + phase));
    let blink = tracker.tick_cursor(start + phase).unwrap();
    assert_eq!((blink.before, blink.after), (Some(original), None));

    let retry_at = start + phase;
    let retry = tracker.prepare(&frame, retry_at).unwrap();
    assert_eq!(retry.damage().rows, RowDamage::All);
    let mut painted_rows = Vec::new();
    frame
        .visit_paint_commands(retry.damage(), thickness, |command| {
            if let PaintCommand::Background(background) = command {
                painted_rows.push(background.rect.origin.y);
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();
    assert_eq!(painted_rows, [0, 16, 32]);
    retry.commit();
    assert_eq!(tracker.displayed_cursor().unwrap().position.row, 1);
    assert_eq!(tracker.cursor_deadline(), Some(retry_at + phase));
    submit_frame(&mut tracker, &frame, retry_at, |damage| {
        assert_eq!(damage.rows, RowDamage::Rows(&[]));
        assert_eq!(damage.cursor, None);
    });
}

#[test]
fn cursor_paint_covers_the_whole_unit_at_a_continuation_cell() {
    let mut session = session("\x1b[2;2H\x1b[1;31;44m水\u{301}\x1b[2;3H".as_bytes());
    session.geometry = GridGeometry::fit(pixels(64, 48), pixels(8, 16)).unwrap();
    let background = Rgb {
        red: 10,
        green: 20,
        blue: 60,
    };
    let cursor = Rgb {
        red: 240,
        green: 220,
        blue: 200,
    };
    let base = Rgb::default();
    let mut indexed = [base; 256];
    indexed[4] = background;
    session.set_theme(&Palette::new(indexed, base, base, cursor), None);
    session.pump().unwrap();

    let mut tracker = PresentationTracker::default();
    let now = Instant::now();
    let thickness = NonZeroU32::new(2).unwrap();
    let frame = session.presentation_frame().unwrap();
    tracker.prepare(&frame, now).unwrap().commit();
    let displayed = tracker.displayed_cursor().unwrap();
    assert_eq!(displayed.position, CellPosition { row: 1, column: 2 });
    let block = frame.cursor_paint(displayed, thickness).unwrap().unwrap();
    assert_eq!(
        block.rect,
        PixelRect {
            origin: PixelPosition { x: 8, y: 16 },
            size: pixels(16, 16)
        }
    );
    assert_eq!(block.color, cursor);
    let glyph = block.glyph.unwrap();
    assert_eq!(glyph.unit.column, 1);
    assert_eq!(glyph.unit.text, CellText::Extended("水\u{301}"));
    assert_eq!(glyph.color, background);
    assert!(glyph.style.bold);

    for (input, origin, size) in [
        (
            &b"\x1b[4 q"[..],
            PixelPosition { x: 8, y: 30 },
            pixels(16, 2),
        ),
        (
            &b"\x1b[6 q"[..],
            PixelPosition { x: 8, y: 16 },
            pixels(2, 16),
        ),
    ] {
        session.transport.source = Cursor::new(input);
        session.pump().unwrap();
        let frame = session.presentation_frame().unwrap();
        tracker.prepare(&frame, now).unwrap().commit();
        let paint = frame
            .cursor_paint(tracker.displayed_cursor().unwrap(), thickness)
            .unwrap()
            .unwrap();
        assert_eq!(paint.rect, PixelRect { origin, size });
        assert_eq!(paint.glyph, None);
    }
}

#[test]
fn block_cursor_preserves_concealment_and_handles_matching_colors() {
    let mut session = session(b"X\x1b[8mY\x1b[1;1H");
    session.geometry = GridGeometry::fit(pixels(64, 48), pixels(8, 16)).unwrap();
    session.pump().unwrap();
    let mut tracker = PresentationTracker::default();
    let now = Instant::now();
    let thickness = NonZeroU32::new(2).unwrap();
    let frame = session.presentation_frame().unwrap();
    tracker.prepare(&frame, now).unwrap().commit();
    let paint = frame
        .cursor_paint(tracker.displayed_cursor().unwrap(), thickness)
        .unwrap()
        .unwrap();
    assert_eq!(
        paint.glyph.unwrap().color,
        Rgb {
            red: 255,
            green: 255,
            blue: 255
        }
    );

    session.transport.source = Cursor::new(b"\x1b[1;2H");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    tracker.prepare(&frame, now).unwrap().commit();
    let paint = frame
        .cursor_paint(tracker.displayed_cursor().unwrap(), thickness)
        .unwrap()
        .unwrap();
    assert_eq!(paint.glyph, None);
}

#[test]
fn cursor_blink_keeps_its_cadence_and_only_changes_the_overlay() {
    let session = session(b"");
    let frame = session.presentation_frame().unwrap();
    let mut tracker = PresentationTracker::default();
    let start = Instant::now();
    let phase = Duration::from_millis(40);
    tracker.set_cursor_blink_interval(phase, start);
    let cursor = submit_frame(&mut tracker, &frame, start, |damage| {
        damage.cursor.unwrap().after
    });
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

    submit_frame(
        &mut tracker,
        &frame,
        start + Duration::from_millis(161),
        |damage| {
            assert_eq!(damage.rows, RowDamage::Rows(&[]));
            assert_eq!(damage.cursor.unwrap().after, cursor);
        },
    );
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
    tracker.prepare(&initial, start).unwrap().commit();
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
    let change = submit_frame(&mut tracker, &frame, release, |damage| {
        damage.cursor.unwrap()
    });
    assert_eq!(change.before, Some(original));
    assert_eq!(
        change.after.unwrap().position,
        CellPosition { row: 1, column: 2 }
    );
    assert_eq!(tracker.cursor_deadline(), Some(release + phase));

    session.transport.source = Cursor::new(b"\x1b[?12l");
    session.pump().unwrap();
    let frame = session.presentation_frame().unwrap();
    tracker.prepare(&frame, release).unwrap().commit();
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
        IFS= read -r value && test "$value" = animate || exit 74
        printf '\033_Ga=T,i=1,s=1,v=1,C=1,q=1;/wAA/w==\033\\'
        printf '\033_Ga=f,i=1,s=1,v=1,z=25,q=1;AAD//w==\033\\'
        printf '\033_Ga=a,i=1,s=3,v=2,r=1,z=25\033\\'
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

    session.queue_input(b"animate\n").unwrap();
    wait_until(&mut session, limit, "animation deadline", |session| {
        session.presentation_deadline().is_some()
    });
    let deadline = session.presentation_deadline().unwrap();
    assert_eq!(
        session.wait_and_pump(Some(Duration::from_secs(5))).unwrap(),
        WaitOutcome::Pumped
    );
    assert!(Instant::now() >= deadline);
    assert_eq!(
        session.graphics_image(1).unwrap().pixels(&mut Vec::new()),
        Some(&[0, 0, 255, 255][..]),
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
    submit_frame(&mut tracker, &initial, now, |damage| {
        assert_eq!(damage.rows, RowDamage::All)
    });
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
            submit_frame(&mut tracker, &frame, now, |damage| {
                assert_eq!(damage.rows, RowDamage::Rows(&[0]));
            });
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
        .prepare(&session.presentation_frame().unwrap(), now)
        .unwrap()
        .commit();
    let pixels_only = GridGeometry::fit(pixels(1200, 600), pixels(100, 150)).unwrap();
    session.resize(pixels_only.pty_size()).unwrap();
    assert_eq!(session.transport.size().unwrap(), pixels_only.pty_size());
    assert_eq!(session.geometry(), pixels_only);
    let frame = session.presentation_frame().unwrap();
    submit_frame(&mut tracker, &frame, now, |damage| {
        assert_eq!(damage.rows, RowDamage::All)
    });
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
