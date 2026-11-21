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
    let mut session = session(b"\x1b_Gi=7,f=24,s=2,v=1,m=1;/wAA\x1b\\");
    session.pump().unwrap();
    assert!(session.processor().graphics_image(7).is_none());
    assert!(session.transport.written.is_empty());

    session.transport.source = Cursor::new(
        concat!(
            "\x1b_Ga=q,i=31,e=T,U=1,C=1,f=32,N=1,P=2,H=-3,m=1;%%ignored%%\x1b\\",
            "\x1b_Ga=q,i=32,e=a,s=3,c=2\x1b\\",
            "\x1b_Gm=0;AAD/\x1b\\\x1b[c"
        )
        .as_bytes(),
    );
    session.pump().unwrap();
    assert_eq!(session.transport.written, b"\x1b");
    assert!(!session.wants_read());
    assert!(session.processor().graphics_image(7).is_none());

    session.transport.write_budget = usize::MAX;
    session.pump().unwrap();
    let image = session.processor().graphics_image(7).unwrap();
    assert_eq!(image.dimensions(), (2, 1));
    assert_eq!(
        image.pixels(&mut Vec::new()).unwrap(),
        &[255, 0, 0, 255, 0, 0, 255, 255]
    );
    assert_eq!(
        session.transport.written,
        concat!(
            "\x1b_Gi=31,e=T,N=1,C=1,U=1,P=2,H=-3;OK\x1b\\",
            "\x1b_Gi=32,e=a,c=2,s=3;OK\x1b\\",
            "\x1b_Gi=7;OK\x1b\\\x1b[?6c"
        )
        .as_bytes(),
    );
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
    assert_eq!(
        session.processor().graphics_image(7).unwrap().dimensions(),
        (2, 1)
    );

    let now = Instant::now();
    let mut tracker = PresentationTracker::default();
    tracker
        .prepare(&session.presentation_frame().unwrap(), now)
        .unwrap()
        .commit();
    session.transport.source = Cursor::new(b"\x1b_Ga=p,i=7,p=5,C=1,q=1\x1b\\");
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
    assert!(
        !tracker
            .prepare(&frame, now)
            .unwrap()
            .damage()
            .graphics_changed
    );

    session.transport.source = Cursor::new(b"\x1b[?1049h\x1b_Ga=p,i=7,C=1,q=1\x1b\\\x1b[?1049l");
    session.pump().unwrap();
    assert_eq!(session.screen().graphics_placements().len(), 1);
    session.transport.source = Cursor::new(b"\x1b[2J");
    session.pump().unwrap();
    assert!(session.screen().graphics_placements().is_empty());
    assert!(session.processor().graphics_image(7).is_some());
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
    assert_eq!(placement.row_clip().start, -1);
    let key = placement.placement.image;
    assert!(session.processor().graphics_image_revision(key).is_some());

    session.transport.source = Cursor::new(b"\n\x1b[2J");
    session.pump().unwrap();
    let [placement] = session.screen().graphics_placements() else {
        panic!("Expected the historical fragment")
    };
    assert_eq!(placement.row, -2);
    assert_eq!(placement.row_clip().start, -1);
    assert_eq!(session.screen().viewport_offset(), 1);

    session.screen.reflow(16, 3, StyleId::default()).unwrap();
    let [resized] = session.screen().graphics_placements() else {
        panic!("The retained history fragment survives reflow");
    };
    assert_eq!((resized.row, resized.row_clip().start), (-2, -1));

    session.transport.source = Cursor::new(b"\x1b[3J");
    session.pump().unwrap();
    assert!(session.screen().graphics_placements().is_empty());
    assert!(session.processor().graphics_image_revision(key).is_none());
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
    assert!(session.processor().graphics_image(41).is_some());

    session.transport.source = Cursor::new(b"\x1b_Ga=d,d=i,i=41,p=6\x1b\\");
    session.pump().unwrap();
    assert!(session.screen().graphics_placements().is_empty());
    assert!(session.processor().graphics_image(41).is_some());

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
        assert!(
            session.processor().graphics_image(id).is_none(),
            "Image {id}"
        );
    }
    assert!(session.transport.written.is_empty());
}

#[test]
fn kitty_numbered_images_use_the_newest_live_id() {
    let mut session = session(
        concat!(
            "\x1b_Gi=1,s=1,v=1,q=1;AAAA/w==\x1b\\",
            "\x1b_GI=13,s=1,v=1;AQID/w==\x1b\\",
            "\x1b_GI=13,s=1,v=1;BAUG/w==\x1b\\",
            "\x1b_Ga=p,I=13,p=9,C=1\x1b\\"
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
            "\x1b_Ga=d,d=N,I=13\x1b\\\x1b_Ga=p,I=13,p=10,C=1\x1b\\",
            "\x1b_Ga=p,i=1,I=13,p=1,C=1\x1b\\"
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
            .processor()
            .graphics_image(2)
            .unwrap()
            .pixels(&mut Vec::new())
            .unwrap(),
        &[1, 2, 3, 255]
    );
    assert!(session.processor().graphics_image(3).is_none());
    assert!(session.transport.written.ends_with(
        b"\x1b_Gi=2,I=13,p=10;OK\x1b\\\x1b_Gi=1,I=13,p=1;EINVAL:Invalid image data\x1b\\"
    ));
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

    session.transport.source = Cursor::new(b"\x1b[3;5H\x1b_Ga=p,i=20,p=1,C=1,q=1\x1b\\\x1b[S");
    session.pump().unwrap();
    assert_eq!(points(&session), [(4, 1), (3, 2), (5, 1)]);

    session.transport.source = Cursor::new(b"\x1b_Ga=d,d=i,i=20,p=1\x1b\\");
    session.pump().unwrap();
    assert!(session.screen().graphics_placements().is_empty());
    assert!(session.processor().graphics_image(20).is_some());
    assert!(session.processor().graphics_image(21).is_none());
    assert!(session.processor().graphics_image(22).is_none());
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
    assert!(session.processor.graphics_image(18).is_none());
}

#[cfg(feature = "wgpu-renderer")]
#[test]
#[ignore = "Requires a Vulkan adapter"]
fn kitty_images_compose_in_an_offscreen_wgpu_pass() {
    use std::sync::mpsc;
    use std::time::Duration;

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
            "\x1b[5;1H\x1b[38;5;42;58;5;9m\u{10eeee}\u{305}\u{305}\u{30e}\u{10eeee}\x1b[0m"
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
    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).unwrap();
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
    let read_frame = |renderer: &crate::ImageRenderer| {
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
            renderer.draw(&mut pass, crate::GraphicsLayer::BehindText);
            renderer.draw(&mut pass, crate::GraphicsLayer::AboveText);
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
        ((2, 8), [0, 255, 255, 255]),
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
    assert_eq!(pixel(&after, 6, 8), [0, 255, 255, 255]);
    assert_eq!(pixel(&after, 10, 8), [255, 0, 255, 255]);

    session.transport.source =
        Cursor::new(b"\x1b_Ga=d,d=p,x=4,y=5\x1b\\\x1b_Ga=d,d=q,x=6,y=5,z=8\x1b\\");
    session.pump().unwrap();
    assert!(session.processor().graphics_image(13).is_some());

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
    let selected = read_frame(&renderer);
    assert_eq!(pixel(&selected, 4, 8), [255, 255, 0, 255]);
    assert_eq!(pixel(&selected, 6, 8), [0, 255, 0, 255]);
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

    session.transport.source = Cursor::new(b"\x1b_Ga=d,d=Q,x=6,y=5,z=0\x1b\\");
    session.pump().unwrap();
    assert!(session.processor().graphics_image(13).is_none());
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
        assert!(session.processor().graphics_image(43).is_none());
    }
}

#[test]
fn kitty_display_erase_and_alternate_entry_clear_their_placements() {
    let mut session = session(
        concat!(
            "\x1b_Ga=T,i=1,p=1,U=1,c=1,r=1,s=1,v=1,q=1;AAAA/w==\x1b\\",
            "\x1b_Ga=T,i=2,P=1,Q=1,H=1,s=1,v=1,q=1;/wAA/w==\x1b\\",
            "\x1b_Ga=T,i=3,P=1,Q=1,H=100,s=1,v=1,q=1;AAD//w==\x1b\\",
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
    session.pump().unwrap();
    assert_eq!(placements(&session), [1, 2, 3]);
    session.transport.source =
        Cursor::new("\x1b[1;1H\u{10eeee}\x1b[2J\x1b[1;1H\u{10eeee}".as_bytes());
    session.pump().unwrap();
    assert_eq!(placements(&session), [1, 3]);
    assert!(session.processor().graphics_image(2).is_some());

    session.transport.source = Cursor::new(
        concat!(
            "\x1b[?47h",
            "\x1b_Ga=p,i=1,p=2,U=1,c=1,r=1,q=1\x1b\\",
            "\x1b_Ga=p,i=2,P=1,Q=2,H=1,q=1\x1b\\",
            "\x1b[?47l\x1b[?47h",
        )
        .as_bytes(),
    );
    session.pump().unwrap();
    assert_eq!(placements(&session), [1, 2]);
    session.transport.source = Cursor::new(b"\x1b[?47l\x1b[?1049h");
    session.pump().unwrap();
    assert!(placements(&session).is_empty());
    assert!(session.processor().graphics_image(1).is_some());

    session.transport.source = Cursor::new(b"\x1b[?1049l");
    session.pump().unwrap();
    assert_eq!(placements(&session), [1, 3]);
    session.transport.source = Cursor::new(b"\x1bc");
    session.pump().unwrap();
    assert!(placements(&session).is_empty());
    assert!((1..=3).all(|id| session.processor().graphics_image(id).is_none()));
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
            .processor()
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
            "\x1b_Gi=2,s=1,v=1,q=1;AAAA/w==\x1b\\",
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
        session
            .processor()
            .graphics_image(1)
            .unwrap()
            .pixels(&mut Vec::new()),
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
