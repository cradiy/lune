use lune_vt::{ApcEvent, Event, Parser};

use super::*;

fn feed(input: &[u8], chunk: usize, mut command: impl FnMut(Result<Command<'_>, DecodeError>)) {
    let mut parser = Parser::default();
    let mut decoder = Decoder::default();
    let mut receive = |event: Event<'_>| match event {
        Event::Apc(ApcEvent::Start) => decoder.start(),
        Event::Apc(ApcEvent::Data(bytes)) => decoder.push(bytes),
        Event::Apc(ApcEvent::Cancel) => {
            decoder.cancel();
        }
        Event::Apc(ApcEvent::End) => {
            if let Some(decoded) = decoder.finish() {
                command(decoded);
            }
        }
        _ => {}
    };
    for bytes in input.chunks(chunk) {
        parser.advance(bytes, &mut receive);
    }
    parser.finish(&mut receive);
}

#[test]
fn graphics_commands_survive_fragmentation_and_cancelled_apcs() {
    let wire = b"start\x1b_Xforeign\x1b\\\x1b_Gi=9;partial\x18\x1b_Ga=T,i=4294967295,z=-2147483648,H=2147483647,f=24,s=1,v=1;AQID\x1b\\\x1b_Gi=7;truncated\x1b";
    for chunk in 1..=wire.len() {
        let mut count = 0;
        feed(wire, chunk, |result| {
            let command = result.unwrap();
            count += 1;
            assert_eq!(command.controls.get(b'a'), Some(Value::Letter(b'T')));
            assert_eq!(command.controls.get(b'i'), Some(Value::Unsigned(u32::MAX)));
            assert_eq!(command.controls.get(b'z'), Some(Value::Signed(i32::MIN)));
            assert_eq!(
                command.controls.get(b'H'),
                Some(Value::Unsigned(i32::MAX as u32))
            );
            assert_eq!(command.controls.get(b'I'), None);
            let mut pixel = [0; 3];
            assert_eq!(command.decode_payload(&mut pixel), Ok(3));
            assert_eq!(pixel, [1, 2, 3]);
        });
        assert_eq!(count, 1, "input chunk length {chunk}");
    }
}

#[test]
fn invalid_controls_recover_at_the_next_apc() {
    let input = b"\x1b_Gi=0,i=2\x1b\\\x1b_Gi=4294967296\x1b\\\x1b_Gz=-2147483649\x1b\\\x1b_Gq=1\x07\x1b\\\x1b_Ga=d,d=a\x1b\\";
    let mut errors = Vec::new();
    let mut accepted = 0;
    feed(input, 1, |result| match result {
        Ok(command) => {
            assert_eq!(command.controls.get(b'd'), Some(Value::Letter(b'a')));
            accepted += 1;
        }
        Err(error) => errors.push(error),
    });
    assert_eq!(accepted, 1);
    assert_eq!(
        errors,
        [
            DecodeError::DuplicateKey(b'i'),
            DecodeError::InvalidControl,
            DecodeError::InvalidControl,
            DecodeError::InvalidControl,
        ]
    );
}

#[test]
fn quota_and_query_preserve_stored_pixels() {
    let input = b"\x1b_Gi=1,s=1,v=1,q=1;AQIDBA==\x1b\\\x1b_Gi=2,s=2,v=1;AAAAAA==\x1b\\\x1b_Gi=1,s=1,v=10001;AAAA\x1b\\\x1b_Gi=1,s=1,v=1;AAAA\x1b\\\x1b_Ga=q,i=1,s=1,v=1;AAAAAA==\x1b\\";
    let mut images = ImageStore::new(4, 1);
    let mut replies = Vec::new();
    feed(input, 3, |command| {
        replies.extend(images.receive(command.unwrap(), |_| Ok(())));
    });
    assert_eq!(
        replies
            .iter()
            .map(|reply| (reply.id, reply.message))
            .collect::<Vec<_>>(),
        [
            (2, b"ENOSPC:Image quota exceeded".as_slice()),
            (1, b"EINVAL:Invalid image data".as_slice()),
            (1, b"ENODATA:Insufficient image data".as_slice()),
            (1, b"OK".as_slice()),
        ]
    );
    assert!(images.get(2).is_none());
    assert_eq!(
        images.get(1).unwrap().pixels(&mut Vec::new()).unwrap(),
        &[1, 2, 3, 4]
    );
}

#[test]
fn quota_uses_the_selected_frames_transient_hint_after_composition() {
    let receive = |images: &mut ImageStore, input: &[u8]| {
        feed(input, 7, |command| {
            let command = command.unwrap();
            if let Some(delete) = command.decode_delete() {
                images.delete_frame(delete.unwrap(), |_| Ok(()));
            } else if let Some(reply) = images.receive(command, |_| Ok(())) {
                assert_eq!(reply.message, b"OK");
            }
        });
    };
    for (wire, evicted) in [
        (b"".as_slice(), 1),
        (b"\x1b_Ga=a,i=2,c=2,r=2,z=15\x1b\\", 2),
        (
            concat!(
                "\x1b_Ga=f,i=2,c=2,s=1,v=1,x=1,X=1;AP8A/w==\x1b\\",
                "\x1b_Ga=d,d=f,i=2,r=2\x1b\\\x1b_Ga=a,i=2,c=2\x1b\\",
            )
            .as_bytes(),
            2,
        ),
        (
            concat!(
                "\x1b_Ga=f,i=2,s=1,v=1;AP8A/w==\x1b\\",
                "\x1b_Ga=c,i=2,r=2,c=3,w=1,h=1,C=1\x1b\\",
                "\x1b_Ga=d,d=f,i=2,r=2\x1b\\\x1b_Ga=a,i=2,c=2\x1b\\",
            )
            .as_bytes(),
            2,
        ),
        (
            concat!(
                "\x1b_Ga=f,i=2,s=1,v=1;AP8A/w==\x1b\\",
                "\x1b_Ga=c,i=2,r=3,c=2,C=1\x1b\\",
                "\x1b_Ga=d,d=f,i=2,r=3\x1b\\\x1b_Ga=a,i=2,c=2\x1b\\",
            )
            .as_bytes(),
            2,
        ),
    ] {
        let mut images = ImageStore::new(4096, 2);
        receive(
            &mut images,
            concat!(
                "\x1b_Gi=1,s=1,v=1;AAAA/w==\x1b\\",
                "\x1b_Gi=2,s=2,v=1;AAAA/wAAAP8=\x1b\\",
                "\x1b_Ga=f,i=2,s=1,v=1,N=1;/wAA/w==\x1b\\",
            )
            .as_bytes(),
        );
        receive(&mut images, wire);
        receive(&mut images, b"\x1b_Gi=3,s=1,v=1;AAAA/w==\x1b\\");
        assert!(
            images.get(evicted).is_none(),
            "Expected to evict image {evicted}"
        );
        assert!(images.get(3 - evicted).is_some());
    }
}

#[test]
fn quota_evicts_the_shortest_priority_prefix_after_placement_succeeds() {
    let mut images = ImageStore::new(28, 6);
    let wire = concat!(
        "\x1b_Ga=T,i=1,s=1,v=1;AAAA/w==\x1b\\",
        "\x1b_Ga=T,i=2,N=1,s=1,v=1;AAAA/w==\x1b\\",
        "\x1b_Ga=T,i=3,s=2,v=1;AAAA/wAAAP8=\x1b\\",
        "\x1b_Ga=T,i=4,N=1,s=1,v=1;AAAA/w==\x1b\\",
        "\x1b_Ga=T,i=5,s=1,v=1;AAAA/w==\x1b\\",
        "\x1b_Ga=T,i=6,s=1,v=1;AAAA/w==\x1b\\",
    );
    let mut keys = Vec::new();
    feed(wire.as_bytes(), 7, |command| {
        let reply = images.receive(command.unwrap(), |change| {
            let ImageChange::Place { placement, .. } = change else {
                panic!("Expected an initial placement");
            };
            keys.push(placement.image);
            Ok(())
        });
        assert_eq!(reply.unwrap().message, b"OK");
    });

    let receive = |images: &mut ImageStore, accept: bool| {
        let mut reply = None;
        feed(
            b"\x1b_Ga=T,i=7,P=6,f=24,s=3,v=1;/wAA/wAA/wAA\x1b\\",
            7,
            |command| {
                reply = images.receive(command.unwrap(), |change| match change {
                    ImageChange::RankEvictions { ranking, .. } => {
                        ranking.mark_placed(keys[0])?;
                        ranking.mark_placed(keys[1])?;
                        ranking.protect(keys[5])?;
                        if !accept {
                            ranking.mark_placed(keys[3])?;
                            ranking.protect(keys[2])?;
                        }
                        Ok(())
                    }
                    ImageChange::Place { evicted, .. } => {
                        if accept {
                            assert_eq!(evicted, &[keys[2], keys[3]]);
                            Ok(())
                        } else {
                            assert_eq!(evicted, &[keys[1], keys[3], keys[4]]);
                            Err(PlacementError::InvalidRegion)
                        }
                    }
                    ImageChange::Remove { .. } => panic!("Expected a placement"),
                });
            },
        );
        reply.unwrap().message
    };
    let before = images.storage_revision();
    assert_eq!(
        receive(&mut images, false),
        b"EINVAL:Invalid placement region"
    );
    assert_eq!(images.storage_revision(), before);
    assert!((1..=6).all(|id| images.get(id).is_some()));
    assert!(images.get(7).is_none());

    assert_eq!(receive(&mut images, true), b"OK");
    let retained: Vec<_> = (1..=7).filter(|&id| images.get(id).is_some()).collect();
    assert_eq!(retained, [1, 2, 5, 6, 7]);
}

#[test]
fn frame_quota_reclaims_idle_animations_only_when_append_can_commit() {
    fn send(
        images: &mut ImageStore,
        wire: &str,
        mut apply: impl FnMut(ImageChange<'_>) -> Result<(), PlacementError>,
    ) -> Reply {
        let mut reply = None;
        feed(wire.as_bytes(), 19, |command| {
            reply = images.receive(command.unwrap(), &mut apply);
        });
        reply.unwrap()
    }

    let mut images = ImageStore::new(512, 4);
    let mut keys = Vec::new();
    let base = PAYLOAD_ENGINE.encode([0; 72]);
    for id in 1..=4 {
        let wire = format!("\x1b_Ga=T,i={id},s=18,v=1;{base}\x1b\\");
        let reply = send(&mut images, &wire, |change| {
            let ImageChange::Place { placement, .. } = change else {
                panic!("Expected an initial placement");
            };
            keys.push(placement.image);
            Ok(())
        });
        assert_eq!(reply.message, b"OK");
    }
    for id in [2, 3] {
        let wire = format!("\x1b_Ga=f,i={id},c=1,s=1,v=1;AP8A/w==\x1b\\");
        assert_eq!(send(&mut images, &wire, |_| Ok(())).message, b"OK");
    }
    let patch = "\x1b_Ga=f,i=3,c=2,s=1,v=1,x=1;AAD//w==\x1b\\";
    loop {
        let reply = send(&mut images, patch, |change| {
            let ImageChange::RankEvictions { ranking, .. } = change else {
                panic!("All animated candidates are in use");
            };
            ranking.mark_placed(keys[1])
        });
        if reply.message == b"ENOSPC:Image quota exceeded" {
            break;
        }
        assert_eq!(reply.message, b"OK");
    }

    let red = [[255, 0, 0, 255]; 18];
    let payload = PAYLOAD_ENGINE.encode(red.as_flattened());
    let wire = format!("\x1b_Ga=f,i=1,s=18,v=1,X=1;{payload}\x1b\\");
    let before = images.storage_revision();
    let revision = images.get(1).unwrap().revision();
    for (protect_idle, expected) in [
        (true, b"ENOSPC:Image quota exceeded".as_slice()),
        (false, b"EINVAL:Invalid placement region"),
    ] {
        let reply = send(&mut images, &wire, |change| match change {
            ImageChange::RankEvictions { ranking, .. } => {
                ranking.mark_placed(keys[1])?;
                if protect_idle {
                    ranking.protect(keys[2])?;
                }
                Ok(())
            }
            ImageChange::Remove { evicted, .. } => {
                assert!(!protect_idle);
                assert_eq!(evicted, &[keys[2]]);
                Err(PlacementError::InvalidRegion)
            }
            ImageChange::Place { .. } => panic!("A frame upload preserves placements"),
        });
        assert_eq!(reply.message, expected);
        assert_eq!(images.storage_revision(), before);
        assert_eq!(images.get(1).unwrap().revision(), revision);
        assert!((1..=4).all(|id| images.get(id).is_some()));
    }

    let reply = send(&mut images, &wire, |change| match change {
        ImageChange::RankEvictions { ranking, .. } => ranking.mark_placed(keys[1]),
        ImageChange::Remove { replaced, evicted } => {
            assert_eq!((replaced, evicted), (None, &keys[2..3]));
            Ok(())
        }
        ImageChange::Place { .. } => panic!("A frame upload preserves placements"),
    });
    assert_eq!((reply.frame_number, reply.message), (2, b"OK".as_slice()));
    assert_eq!(images.invalidated_source(before), Some(keys[2]));
    assert!(images.get(3).is_none());
    assert!(images.get(2).is_some());
    assert!(images.get(4).is_some());
    feed(b"\x1b_Ga=a,i=1,c=2\x1b\\", 19, |command| {
        images.receive(command.unwrap(), |_| Ok(()));
    });
    assert_eq!(
        images.get(1).unwrap().pixels(&mut Vec::new()).unwrap(),
        red.as_flattened()
    );
}

#[test]
fn numbered_images_follow_committed_replacement_and_eviction() {
    let mut images = ImageStore::new(8, 2);
    let mut protected = None;
    feed(b"\x1b_Ga=T,I=17,s=1,v=1;AAAA/w==\x1b\\", 7, |command| {
        let reply = images.receive(command.unwrap(), |change| {
            if let ImageChange::Place { placement, .. } = change {
                protected = Some(placement.image);
            }
            Ok(())
        });
        assert_eq!(reply.unwrap().id, 1);
    });
    let receive = |images: &mut ImageStore, wire: &[u8]| {
        let mut reply = None;
        feed(wire, 7, |command| {
            reply = images.receive(command.unwrap(), |change| match change {
                ImageChange::RankEvictions { ranking, .. } => ranking.protect(protected.unwrap()),
                ImageChange::Place { .. } => Err(PlacementError::InvalidRegion),
                ImageChange::Remove { .. } => Ok(()),
            });
        });
        reply.unwrap()
    };
    assert_eq!(
        receive(&mut images, b"\x1b_GI=17,s=1,v=1;AQID/w==\x1b\\").id,
        2
    );
    assert!(
        receive(&mut images, b"\x1b_Ga=T,I=17,s=1,v=1;AAAA/w==\x1b\\")
            .message
            .starts_with(b"EINVAL")
    );
    assert_eq!(images.newest_id(17), Some(2));

    let previous = images.storage_revision();
    assert_eq!(
        receive(&mut images, b"\x1b_Gi=99,s=1,v=1;AAAA/w==\x1b\\").message,
        b"OK"
    );
    assert!(images.get(2).is_none());
    assert_eq!(images.newest_id(17), Some(1));
    assert_eq!(
        images.invalidated_source(previous).map(ImageKey::id),
        Some(2)
    );

    let previous = images.storage_revision();
    assert_eq!(
        receive(&mut images, b"\x1b_Gi=1,s=2,v=1;/wAA/wAA//8=\x1b\\").message,
        b"OK"
    );
    assert!(images.get(1).is_some());
    assert!(images.get(99).is_none());
    assert_eq!(images.newest_id(17), None);
    assert_eq!(images.invalidated_source(previous), None);
}

#[test]
fn animation_frames_commit_complete_patches_and_keep_the_current_surface_on_failure() {
    let receive = |images: &mut ImageStore, input: &[u8]| {
        let mut reply = None;
        feed(input, 3, |command| {
            reply = images.receive(command.unwrap(), |_| Ok(()));
        });
        reply
    };
    let mut images = ImageStore::new(2048, 1);
    assert_eq!(
        receive(&mut images, b"\x1b_GI=17,s=2,v=1;/wAA/wAA//8=\x1b\\")
            .unwrap()
            .id,
        1,
    );
    assert!(
        receive(
            &mut images,
            b"\x1b_Ga=f,I=17,c=1,x=1,s=1,v=1,m=1;AP8A\x1b\\"
        )
        .is_none()
    );
    let before = images.visual_revision();
    let reply = receive(&mut images, b"\x1b_Ga=a,i=1,c=2\x1b\\").unwrap();
    assert_eq!(reply.message, b"EINVAL:Invalid image data");
    assert_eq!(images.visual_revision(), before);

    let reply = receive(
        &mut images,
        b"\x1b_Ga=f,I=17,c=1,x=1,s=1,v=1,m=1;AP8A\x1b\\\x1b_Ga=f,m=0;gA==\x1b\\",
    )
    .unwrap();
    assert_eq!((reply.id, reply.number, reply.frame_number), (0, 17, 2));
    assert_eq!(images.visual_revision(), before);

    let mut scratch = Vec::new();
    let original_revision = images.get(1).unwrap().pixel_revision();
    assert!(receive(&mut images, b"\x1b_Ga=a,i=1,c=2\x1b\\").is_none());
    let selected = images.visual_revision();
    assert_ne!(selected, before);
    let image = images.get(1).unwrap();
    let PixelUpdates::One(update) = image
        .pixel_updates(Some(&original_revision), &mut scratch)
        .unwrap()
    else {
        panic!("Expected one new patch");
    };
    assert_eq!(
        (update.origin, update.size, update.bytes_per_row),
        ([1, 0], [1, 1], 4)
    );
    assert_eq!(update.bytes, &[0, 128, 127, 255]);
    let selected_revision = image.pixel_revision();
    assert_eq!(
        images.get(1).unwrap().pixels(&mut scratch).unwrap(),
        &[255, 0, 0, 255, 0, 128, 127, 255],
    );

    // The raw frame declares its size in the first chunk. The PNG contains only an IHDR.
    for wire in [
        b"\x1b_Ga=f,i=1,r=2,s=3,v=1,m=1;AAAA\x1b\\".as_slice(),
        b"\x1b_Ga=f,i=1,r=2,f=100;iVBORw0KGgoAAAANSUhEUgAAAAMAAAABCAYAAAAb4BS0\x1b\\",
    ] {
        let reply = receive(&mut images, wire).expect("Reject before reading the frame pixels");
        assert_eq!(reply.message, b"EINVAL:Invalid animation frame region");
        assert_eq!(reply.frame_number, 2);
        assert_eq!(images.visual_revision(), selected);
    }
    assert!(
        receive(
            &mut images,
            b"\x1b_Ga=f,i=1,r=1,s=1,v=1,X=1,q=1;AAAAAA==\x1b\\"
        )
        .is_none()
    );
    let rebased = images.visual_revision();
    assert_ne!(rebased, selected);
    let PixelUpdates::One(update) = images
        .get(1)
        .unwrap()
        .pixel_updates(Some(&selected_revision), &mut scratch)
        .unwrap()
    else {
        panic!("A base edit updates only its affected region");
    };
    assert_eq!((update.origin, update.size), ([0, 0], [1, 1]));
    assert_eq!(update.bytes, &[0; 4]);
    assert_eq!(
        images.get(1).unwrap().pixels(&mut scratch).unwrap(),
        &[0, 0, 0, 0, 0, 128, 127, 255],
    );
    let reply = receive(
        &mut images,
        b"\x1b_Ga=c,i=1,r=2,c=2,X=1,x=1,w=1,h=1,C=1\x1b\\",
    )
    .unwrap();
    assert_eq!(reply.message, b"EINVAL:Invalid animation frame region");
    assert_eq!(images.visual_revision(), rebased);
    let reply = receive(&mut images, b"\x1b_Ga=c,i=1,r=2,c=2,X=1,w=1,h=1,C=1\x1b\\").unwrap();
    assert_eq!(reply.message, b"OK");
    assert_ne!(images.visual_revision(), rebased);
    assert_eq!(
        images.get(1).unwrap().pixels(&mut scratch).unwrap(),
        &[0, 128, 127, 255, 0, 128, 127, 255],
    );
    assert!(receive(&mut images, b"\x1b_Ga=a,i=1,c=1\x1b\\").is_none());
    let PixelUpdates::One(update) = images
        .get(1)
        .unwrap()
        .pixel_updates(Some(&selected_revision), &mut scratch)
        .unwrap()
    else {
        panic!("Expected the full image after the base changed");
    };
    assert_eq!((update.origin, update.size), ([0, 0], [2, 1]));
    assert_eq!(update.bytes, &[0, 0, 0, 0, 0, 0, 255, 255]);

    let revision = images.visual_revision();
    assert!(receive(&mut images, b"\x1b_Ga=f,i=1,r=2,s=1,v=1,m=1,q=1;AAAA\x1b\\").is_none());
    let reply = receive(&mut images, b"\x1b_Ga=f,m=0\x1b\\").unwrap();
    assert_eq!(
        (reply.id, reply.message),
        (1, b"ENODATA:Insufficient image data".as_slice())
    );
    assert!(receive(&mut images, b"\x1b_Ga=a,i=1,c=2\x1b\\").is_none());
    assert_ne!(images.visual_revision(), revision);
    assert_eq!(
        images.get(1).unwrap().pixels(&mut scratch).unwrap(),
        &[0, 128, 127, 255, 0, 128, 127, 255]
    );
}

#[test]
fn chunked_png_frames_validate_dimensions_before_accepting_more_data() {
    use std::io::Write as _;

    let receive = |images: &mut ImageStore, wire: &str| {
        let mut reply = None;
        feed(wire.as_bytes(), 3, |command| {
            reply = images.receive(command.unwrap(), |_| Ok(()));
        });
        reply
    };
    for compress in [false, true] {
        let mut images = ImageStore::new(2048, 1);
        let root = "\x1b_Gi=1,s=2,v=1;AAAA/wAAAP8=\x1b\\";
        assert_eq!(receive(&mut images, root).unwrap().message, b"OK");
        let original = images.get(1).unwrap().revision();
        let color = [9, 16, 24, 255];
        for width in [3, 2] {
            let mut png = Vec::new();
            let mut encoder = png::Encoder::new(&mut png, width, 1);
            encoder.set_color(png::ColorType::Rgba);
            let mut writer = encoder.write_header().unwrap();
            writer
                .write_image_data(&color.repeat(width as usize))
                .unwrap();
            writer.finish().unwrap();

            let header_len = 33;
            let (wire, compression, split) = if compress {
                let mut zlib =
                    flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
                zlib.write_all(&png[..header_len]).unwrap();
                zlib.flush().unwrap();
                let split = zlib.get_ref().len().next_multiple_of(3);
                zlib.write_all(&png[header_len..]).unwrap();
                let compression = format!(",o=z,S={}", png.len());
                (zlib.finish().unwrap(), compression, split)
            } else {
                (png, String::new(), header_len)
            };
            let first = PAYLOAD_ENGINE.encode(&wire[..15]);
            let header = PAYLOAD_ENGINE.encode(&wire[15..split]);
            let rest = PAYLOAD_ENGINE.encode(&wire[split..]);
            assert!(
                receive(
                    &mut images,
                    &format!("\x1b_Ga=f,i=1,r=2,f=100,m=1{compression};{first}\x1b\\")
                )
                .is_none()
            );
            let reply = receive(&mut images, &format!("\x1b_Ga=f,m=1;{header}\x1b\\"));
            assert_eq!(images.get(1).unwrap().revision(), original);
            if width == 3 {
                let reply = reply.expect("Reject an oversized frame as soon as its header arrives");
                assert_eq!((reply.id, reply.frame_number), (1, 2));
                assert_eq!(reply.message, b"EINVAL:Invalid animation frame region");
            } else {
                assert!(reply.is_none());
                let reply = receive(&mut images, &format!("\x1b_Ga=f,m=0;{rest}\x1b\\")).unwrap();
                assert_eq!(reply.message, b"OK");
                assert!(receive(&mut images, "\x1b_Ga=a,i=1,c=2\x1b\\").is_none());
                assert_eq!(
                    images.get(1).unwrap().pixels(&mut Vec::new()).unwrap(),
                    color.repeat(2)
                );
            }
        }
    }
}

#[test]
fn clipped_animation_uploads_keep_only_pixels_inside_the_canvas() {
    let mut images = ImageStore::new(2048, 1);
    let send = |images: &mut ImageStore, wire: &[u8]| {
        let mut reply = None;
        feed(wire, 23, |command| {
            reply = images.receive(command.unwrap(), |_| Ok(()));
        });
        reply
    };
    let wire = format!(
        "\x1b_Gi=1,s=32,v=16;{}\x1b\\",
        PAYLOAD_ENGINE.encode([[255, 0, 0, 255]; 512].as_flattened()),
    );
    assert_eq!(send(&mut images, wire.as_bytes()).unwrap().message, b"OK");

    let mut patch = [[255, 255, 255, 255]; 512];
    patch[0] = [0, 255, 0, 255];
    patch[32] = [0, 0, 255, 255];
    let raw = PAYLOAD_ENGINE.encode(patch.as_flattened());
    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, 32, 16);
    encoder.set_color(png::ColorType::Rgba);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(patch.as_flattened()).unwrap();
    writer.finish().unwrap();
    let png = PAYLOAD_ENGINE.encode(png);
    for frame in 2..=13 {
        let column = 31 + frame % 2;
        let (format, payload) = if frame <= 11 {
            ("s=32,v=16", &raw)
        } else {
            ("f=100", &png)
        };
        let wire = format!("\x1b_Ga=f,i=1,c=1,{format},x={column},y=14,X=1;{payload}\x1b\\");
        let reply = send(&mut images, wire.as_bytes()).unwrap();
        assert_eq!(
            (reply.frame_number, reply.message),
            (frame, b"OK".as_slice())
        );
    }

    let mut scratch = Vec::new();
    let mut expected = [[255, 0, 0, 255]; 512];
    for (frame, clipped) in [(11, false), (10, true), (13, false), (12, true)] {
        let wire = format!("\x1b_Ga=a,i=1,c={frame}\x1b\\");
        assert!(send(&mut images, wire.as_bytes()).is_none());
        expected[479] = if clipped { patch[0] } else { expected[0] };
        expected[511] = if clipped { patch[32] } else { expected[0] };
        assert_eq!(
            images.get(1).unwrap().pixels(&mut scratch).unwrap(),
            expected.as_flattened(),
        );
    }

    let green = [0, 255, 0, 255];
    let blue = [0, 0, 255, 255];
    let red = expected[0];
    let mut patch = [blue; 64];
    patch[..32].fill(green);
    let wire = format!(
        "\x1b_Ga=f,i=1,c=1,x=4,y=14,s=32,v=2,X=1;{}\x1b\\",
        PAYLOAD_ENGINE.encode(patch.as_flattened()),
    );
    assert_eq!(send(&mut images, wire.as_bytes()).unwrap().message, b"OK");
    let edit = b"\x1b_Ga=f,i=1,r=14,s=1,v=1,X=1;AAAA/w==\x1b\\";
    assert_eq!(send(&mut images, edit).unwrap().message, b"OK");
    assert!(send(&mut images, b"\x1b_Ga=a,i=1,c=14\x1b\\").is_none());
    let image = images.get(1).unwrap();
    let mut scratch = Vec::new();
    for (origin, top, bottom) in [([5, 14], green, blue), ([0, 14], red, red)] {
        let update = image.pixel_region(origin, [2, 2], &mut scratch).unwrap();
        assert_eq!(
            (update.origin, update.size, update.bytes_per_row),
            (origin, [2, 2], 128)
        );
        assert_eq!(update.bytes.len(), 136);
        assert_eq!(&update.bytes[..8], [top; 2].as_flattened());
        assert_eq!(&update.bytes[128..], [bottom; 2].as_flattened());
        assert_eq!(scratch.capacity(), 0);
    }
    let update = image.pixel_region([3, 14], [2, 2], &mut scratch).unwrap();
    assert_eq!(update.bytes_per_row, 8);
    assert_eq!(update.bytes, [red, green, red, blue].as_flattened());
}

#[test]
fn full_frame_overwrite_shares_pixels_at_quota_and_detaches_source_edits() {
    let mut images = ImageStore::new(2048, 1);
    let send = |images: &mut ImageStore, wire: &[u8]| {
        let mut reply = None;
        feed(wire, 17, |command| {
            reply = images.receive(command.unwrap(), |_| Ok(()));
        });
        reply.map(|reply| reply.message)
    };
    let black = [0, 0, 0, 255];
    let red = [255, 0, 0, 255];
    let green = [0, 255, 0, 255];
    let blue = [0, 0, 255, 255];
    for (index, pixel) in [black, red, green, blue, black].into_iter().enumerate() {
        let controls = if index == 0 { "a=t" } else { "a=f,X=1" };
        let payload = PAYLOAD_ENGINE.encode([pixel; 512].as_flattened());
        let wire = format!("\x1b_G{controls},i=1,s=32,v=16;{payload}\x1b\\");
        assert_eq!(send(&mut images, wire.as_bytes()), Some(b"OK".as_slice()));
    }
    let payload = PAYLOAD_ENGINE.encode([green; 512].as_flattened());
    let overflow = format!("\x1b_Ga=f,i=1,s=32,v=16,X=1;{payload}\x1b\\");
    let quota = Some(b"ENOSPC:Image quota exceeded".as_slice());
    assert_eq!(send(&mut images, overflow.as_bytes()), quota);
    assert_eq!(send(&mut images, b"\x1b_Ga=a,i=1,c=4\x1b\\"), None);
    let previous = images.get(1).unwrap().revision();
    let edit = format!("\x1b_Ga=f,i=1,r=4,s=32,v=16,X=1;{payload}\x1b\\");
    assert_eq!(send(&mut images, edit.as_bytes()), Some(b"OK".as_slice()));
    assert_ne!(images.get(1).unwrap().revision(), previous);
    assert_eq!(
        images.get(1).unwrap().pixels(&mut Vec::new()).unwrap(),
        [green; 512].as_flattened()
    );
    assert_eq!(send(&mut images, overflow.as_bytes()), quota);
    assert_eq!(send(&mut images, b"\x1b_Ga=a,i=1,c=3\x1b\\"), None);

    let mut scratch = Vec::new();
    for (source, color) in [(2, red), (1, black)] {
        let previous = images.get(1).unwrap().revision();
        let wire = format!("\x1b_Ga=c,i=1,r={source},c=3,x=1,C=1\x1b\\");
        assert_eq!(
            send(&mut images, wire.as_bytes()),
            Some(b"EINVAL:Invalid animation frame region".as_slice())
        );
        assert_eq!(images.get(1).unwrap().revision(), previous);

        let wire = format!("\x1b_Ga=c,i=1,r={source},c=3,C=1\x1b\\");
        assert_eq!(send(&mut images, wire.as_bytes()), Some(b"OK".as_slice()));
        if source == 2 {
            assert_eq!(
                send(&mut images, overflow.as_bytes()),
                Some(b"OK".as_slice())
            );
            let wire = format!("\x1b_Ga=f,i=1,r=2,s=32,v=16,X=1;{payload}\x1b\\");
            assert_eq!(send(&mut images, wire.as_bytes()), quota);
        }
        let wire = format!("\x1b_Ga=f,i=1,r={source},s=1,v=1,X=1;AP8A/w==\x1b\\");
        assert_eq!(send(&mut images, wire.as_bytes()), Some(b"OK".as_slice()));
        assert_eq!(
            images.get(1).unwrap().pixels(&mut scratch).unwrap(),
            [color; 512].as_flattened(),
        );
    }
}

#[test]
fn partial_composition_at_frame_quota_updates_only_the_destination() {
    let mut images = ImageStore::new(512, 1);
    let send = |images: &mut ImageStore, wire: &[u8]| {
        let mut reply = None;
        feed(wire, 7, |command| {
            reply = images
                .receive(command.unwrap(), |_| Ok(()))
                .map(|reply| reply.message);
        });
        reply
    };
    let blue = [0, 0, 255, 255];
    let mixed = [128, 0, 127, 255];
    for (action, pixel) in [("t", [255, 0, 0, 128]), ("f", blue)] {
        let payload = PAYLOAD_ENGINE.encode([pixel; 128].as_flattened());
        let wire = format!("\x1b_Ga={action},i=1,s=128,v=1;{payload}\x1b\\");
        assert_eq!(send(&mut images, wire.as_bytes()), Some(b"OK".as_slice()));
    }
    let patch = b"\x1b_Ga=f,i=1,s=1,v=1,X=1;AAAA/w==\x1b\\";
    let share = b"\x1b_Ga=c,i=1,r=1,c=3,C=1\x1b\\";
    assert_eq!(send(&mut images, patch), Some(b"OK".as_slice()));
    assert_eq!(send(&mut images, share), Some(b"OK".as_slice()));
    loop {
        let reply = send(&mut images, patch);
        if reply == Some(b"ENOSPC:Image quota exceeded") {
            break;
        }
        assert_eq!(reply, Some(b"OK".as_slice()));
    }
    assert_eq!(send(&mut images, share), Some(b"OK".as_slice()));
    let hidden = b"\x1b_Ga=c,i=1,r=1,c=2,x=32,w=96,h=1\x1b\\";
    assert_eq!(send(&mut images, hidden), Some(b"OK".as_slice()));
    assert_eq!(send(&mut images, b"\x1b_Ga=a,i=1,c=2\x1b\\"), None);
    let before = images.get(1).unwrap().pixel_revision();
    for (wire, origin) in [
        (
            b"\x1b_Ga=c,i=1,r=1,c=2,x=1,w=1,h=1\x1b\\".as_slice(),
            [1, 0],
        ),
        (b"\x1b_Ga=c,i=1,r=2,c=2,X=1,x=2,w=1,h=1\x1b\\", [2, 0]),
    ] {
        let previous = images.get(1).unwrap().pixel_revision();
        assert_eq!(send(&mut images, wire), Some(b"OK".as_slice()));
        let image = images.get(1).unwrap();
        let mut scratch = Vec::new();
        let Some(PixelUpdates::One(update)) = image.pixel_updates(Some(&previous), &mut scratch)
        else {
            panic!("Expected one destination update");
        };
        assert_eq!((update.origin, update.size), (origin, [1, 1]));
        assert_eq!(update.bytes, mixed);
        assert!(scratch.is_empty());
    }
    let image = images.get(1).unwrap();
    let mut scratch = Vec::new();
    let Some(PixelUpdates::One(update)) = image.pixel_updates(Some(&before), &mut scratch) else {
        panic!("Expected one merged destination update");
    };
    assert_eq!((update.origin, update.size), ([1, 0], [2, 1]));
    let mut expected = [blue; 128];
    expected[1..3].fill(mixed);
    expected[32..].fill(mixed);
    assert_eq!(image.pixels(&mut scratch).unwrap(), expected.as_flattened());
}

#[test]
fn composition_damage_can_meet_an_immutable_patch_at_a_shared_snapshot() {
    let mut images = ImageStore::new(4096, 1);
    let send = |images: &mut ImageStore, wire: &[u8]| {
        let mut reply = None;
        feed(wire, 7, |command| {
            reply = images
                .receive(command.unwrap(), |_| Ok(()))
                .map(|reply| reply.message);
        });
        reply
    };
    for wire in [
        b"\x1b_Gi=1,s=4,v=1;AAAA/wAAAP8AAAD/AAAA/w==\x1b\\".as_slice(),
        b"\x1b_Ga=f,i=1,s=4,v=1,X=1;AAD//wAA//8AAP//AAD//w==\x1b\\",
        b"\x1b_Ga=f,i=1,s=1,v=1,X=1;AAAA/w==\x1b\\",
        b"\x1b_Ga=c,i=1,r=2,c=3,C=1\x1b\\",
        b"\x1b_Ga=f,i=1,r=2,s=1,v=1,X=1;AP8A/w==\x1b\\",
    ] {
        assert_eq!(send(&mut images, wire), Some(b"OK".as_slice()));
    }
    assert_eq!(send(&mut images, b"\x1b_Ga=a,i=1,c=2\x1b\\"), None);
    let cached = images.get(1).unwrap().pixel_revision();
    assert_eq!(
        send(
            &mut images,
            b"\x1b_Ga=f,i=1,r=2,s=4,v=1,X=1;/wAA//8AAP//AAD//wAA/w==\x1b\\"
        ),
        Some(b"OK".as_slice())
    );
    let unrelated = images.get(1).unwrap().pixel_revision();
    assert_eq!(
        send(&mut images, b"\x1b_Ga=c,i=1,r=1,c=3,x=1,w=1,h=1,C=1\x1b\\"),
        Some(b"OK".as_slice())
    );
    assert_eq!(send(&mut images, b"\x1b_Ga=a,i=1,c=3\x1b\\"), None);
    let image = images.get(1).unwrap();
    assert!(image.pixel_update_plan(Some(&unrelated)).damage().is_none());
    let mut scratch = Vec::new();
    let Some(PixelUpdates::One(update)) = image.pixel_updates(Some(&cached), &mut scratch) else {
        panic!("Expected one update from the shared snapshot");
    };
    assert_eq!((update.origin, update.size), ([0, 0], [2, 1]));
    assert_eq!(update.bytes, [0, 0, 255, 255, 0, 0, 0, 255]);
    for _ in 0..8 {
        assert_eq!(
            send(&mut images, b"\x1b_Ga=c,i=1,r=1,c=3,x=1,w=1,h=1,C=1\x1b\\"),
            Some(b"OK".as_slice())
        );
    }
    assert!(
        images
            .get(1)
            .unwrap()
            .pixel_update_plan(Some(&cached))
            .damage()
            .is_none()
    );
}

#[test]
fn frame_deletion_cancels_partial_edits_before_frame_numbers_change() {
    for typed_delete in [true, false] {
        let mut images = ImageStore::new(4096, 1);
        let receive = |images: &mut ImageStore, wire: &[u8]| {
            let mut reply = None;
            feed(wire, 5, |command| {
                let command = command.unwrap();
                if typed_delete && let Some(delete) = command.decode_delete() {
                    images.delete_frame(delete.unwrap(), |_| Ok(()));
                } else {
                    reply = images.receive(command, |_| Ok(()));
                }
            });
            reply
        };
        for wire in [
            b"\x1b_Gi=1,s=1,v=1;/wAA/w==\x1b\\".as_slice(),
            b"\x1b_Ga=f,i=1,s=1,v=1;AP8A/w==\x1b\\",
        ] {
            assert_eq!(receive(&mut images, wire).unwrap().message, b"OK");
        }
        assert!(
            receive(
                &mut images,
                b"\x1b_Ga=f,i=1,r=2,s=1,v=1,o=z,m=1;eJxjYPj/HwAD\x1b\\"
            )
            .is_none()
        );
        assert!(receive(&mut images, b"\x1b_Ga=d,d=f,i=1,r=2\x1b\\").is_none());
        assert!(receive(&mut images, b"\x1b_Ga=f,m=0;AQH/\x1b\\").is_none());
        receive(&mut images, b"\x1b_Ga=a,i=1,c=2\x1b\\");
        assert_eq!(
            images.get(1).unwrap().pixels(&mut Vec::new()).unwrap(),
            [255, 0, 0, 255]
        );

        let reply = receive(
            &mut images,
            b"\x1b_Ga=f,i=1,s=1,v=1,o=z;eJxjYPj/HwADAQH/\x1b\\",
        )
        .unwrap();
        assert_eq!((reply.frame_number, reply.message), (2, b"OK".as_slice()));
    }
}

#[test]
fn animation_base_edits_follow_live_links_and_deletion_freezes_dependents() {
    let mut images = ImageStore::new(4096, 1);
    let mut receive = |wire: &[u8]| {
        feed(wire, 5, |command| {
            let command = command.unwrap();
            if let Some(delete) = command.decode_delete() {
                images.delete_frame(delete.unwrap(), |_| Ok(()));
            } else if let Some(reply) = images.receive(command, |_| Ok(())) {
                assert_eq!(reply.message, b"OK");
            }
        });
        images
            .get(1)
            .unwrap()
            .pixels(&mut Vec::new())
            .unwrap()
            .to_vec()
    };

    assert_eq!(
        receive(
            concat!(
                "\x1b_Gi=1,f=24,s=3,v=1;/wAA/////wD/\x1b\\",
                "\x1b_Ga=f,i=1,c=1,f=24,x=1,s=1,v=1,X=1;AP8A\x1b\\",
                "\x1b_Ga=f,i=1,c=2,f=24,x=2,s=1,v=1,X=1;AAD/\x1b\\",
                "\x1b_Ga=f,i=1,c=1,f=24,x=2,s=1,v=1,X=1;////\x1b\\",
                "\x1b_Ga=a,i=1,c=3\x1b\\",
                "\x1b_Ga=f,i=1,r=1,f=24,s=1,v=1,X=1;//8A\x1b\\",
            )
            .as_bytes(),
        ),
        [255, 255, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255]
    );
    let detached = [255, 255, 0, 255, 0, 255, 255, 255, 0, 0, 255, 255];
    assert_eq!(
        receive(b"\x1b_Ga=f,i=1,r=2,f=24,x=1,s=1,v=1,X=1;AP//\x1b\\"),
        detached,
    );
    assert_eq!(
        receive(b"\x1b_Ga=f,i=1,r=1,f=24,s=1,v=1,X=1;/wAA\x1b\\"),
        detached,
    );
    assert_eq!(receive(b"\x1b_Ga=d,d=f,i=1,r=2\x1b\\"), detached);
    assert_eq!(
        receive(b"\x1b_Ga=a,i=1,c=3\x1b\\"),
        [255, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255],
    );
    assert_eq!(
        receive(b"\x1b_Ga=d,d=F,i=1,r=1\x1b\\\x1b_Ga=f,i=1,r=1,f=24,s=1,v=1,X=1;/wD/\x1b\\"),
        [255, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255],
    );
}

#[test]
fn temporary_animation_releases_its_state_when_the_original_frame_remains() {
    let mut images = ImageStore::new(4096, 1);
    let apply = |images: &mut ImageStore, wire: &[u8]| {
        feed(wire, 5, |command| {
            let command = command.unwrap();
            if let Some(delete) = command.decode_delete() {
                images.delete_frame(delete.unwrap(), |_| Ok(()));
            } else if let Some(reply) = images.receive(command, |_| Ok(())) {
                assert_eq!(reply.message, b"OK");
            }
        });
    };
    apply(
        &mut images,
        b"\x1b_Gi=1,f=24,s=1,v=1;/wAA\x1b\\\x1b_Ga=a,i=1,c=1\x1b\\",
    );
    assert_eq!(images.animation_membership(), None);
    let upload = b"\x1b_Ga=f,i=1,f=24,s=1,v=1;AAD/\x1b\\\x1b_Ga=a,i=1,c=2\x1b\\";
    let delete = b"\x1b_Ga=d,d=f,i=1,r=2\x1b\\";
    apply(&mut images, upload);
    assert!(images.animation_membership().is_some());
    apply(&mut images, delete);
    assert_eq!(images.animation_membership(), None);
    assert_eq!(
        images.get(1).unwrap().pixels(&mut Vec::new()).unwrap(),
        [255, 0, 0, 255]
    );

    apply(&mut images, b"\x1b_Ga=a,i=1,v=2\x1b\\");
    apply(&mut images, upload);
    apply(&mut images, delete);
    assert!(images.animation_membership().is_some());
}

#[test]
fn playback_follows_gap_changes_and_frame_removal() {
    let mut images = ImageStore::new(4096, 1);
    let start = std::time::Instant::now();
    let at = |millis| start + std::time::Duration::from_millis(millis);
    let mut step = |millis, wire: &[u8], expected: [u8; 4], deadline: Option<u64>| {
        feed(wire, 13, |command| {
            let command = command.unwrap();
            if let Some(delete) = command.decode_delete() {
                images.delete_frame(delete.unwrap(), |_| Ok(()));
            } else if let Some(reply) = images.receive(command, |_| Ok(())) {
                assert_eq!(reply.message, b"OK");
            }
        });
        images.poll_animations(|| at(millis));
        assert_eq!(
            images.get(1).unwrap().pixels(&mut Vec::new()).unwrap(),
            expected
        );
        assert_eq!(images.animation_deadline(), deadline.map(at));
    };
    let red = [255, 0, 0, 255];
    let green = [0, 255, 0, 255];
    let blue = [0, 0, 255, 255];
    let mut animation = concat!(
        "\x1b_Gi=1,s=1,v=1;/wAA/w==\x1b\\",
        "\x1b_Ga=f,i=1,s=1,v=1,z=-1;AAAA/w==\x1b\\",
        "\x1b_Ga=f,i=1,s=1,v=1,z=-1;AP8A/w==\x1b\\",
    )
    .to_owned();
    animation.push_str(&"\x1b_Ga=f,i=1,s=1,v=1,z=-1;AAAA/w==\x1b\\".repeat(63));
    animation.push_str(concat!(
        "\x1b_Ga=f,i=1,s=1,v=1,z=20;AAD//w==\x1b\\",
        "\x1b_Ga=f,i=1,s=1,v=1,z=30;/////w==\x1b\\",
        "\x1b_Ga=a,i=1,r=1,z=10,s=3\x1b\\",
    ));
    step(0, animation.as_bytes(), red, Some(10));
    step(10, b"", blue, Some(30));
    step(
        11,
        b"\x1b_Ga=a,i=1,r=3,z=8\x1b\\\x1b_Ga=d,d=f,i=1,r=2\x1b\\",
        blue,
        Some(30),
    );
    step(12, b"\x1b_Ga=a,i=1,r=67,z=-1\x1b\\", blue, Some(30));
    step(30, b"", red, Some(40));
    step(40, b"", green, Some(48));
    step(48, b"", blue, Some(68));
    step(50, b"\x1b_Ga=d,d=f,i=1,r=1\x1b\\", blue, Some(68));
    step(51, b"\x1b_Ga=a,i=1,s=3,v=3\x1b\\", blue, Some(68));
    step(200, b"", blue, None);
}

#[test]
fn removing_an_animation_releases_shared_frame_storage() {
    let receive = |images: &mut ImageStore, input: &[u8]| {
        let mut reply = None;
        feed(input, input.len(), |command| {
            reply = images.receive(command.unwrap(), |_| Ok(()));
        });
        reply.unwrap()
    };
    let mut images = ImageStore::new(256, 1);
    let base = b"\x1b_Gi=1,s=2,v=1;AAAAAAAAAAA=\x1b\\";
    let patch = b"\x1b_Ga=f,i=1,c=1,s=1,v=1,X=1;/wAA/w==\x1b\\";
    assert_eq!(receive(&mut images, base).message, b"OK");
    assert_eq!(receive(&mut images, patch).message, b"OK");
    let edit = b"\x1b_Ga=f,i=1,r=2,s=1,v=1,X=1;AP8A/w==\x1b\\";
    for _ in 0..40 {
        assert_eq!(receive(&mut images, edit).message, b"OK");
    }
    let mut stored = 1;
    for _ in 0..16 {
        if receive(&mut images, patch).message == b"ENOSPC:Image quota exceeded" {
            break;
        }
        stored += 1;
    }
    assert!((1..16).contains(&stored));
    assert_eq!(
        receive(&mut images, patch).message,
        b"ENOSPC:Image quota exceeded"
    );
    images.retain_ids(|_| false);
    assert_eq!(receive(&mut images, base).message, b"OK");
    for _ in 0..stored {
        assert_eq!(receive(&mut images, patch).message, b"OK");
    }
}

#[test]
fn deleting_the_last_dependent_frame_releases_original_pixels() {
    let receive = |images: &mut ImageStore, wire: &[u8]| {
        let mut reply = None;
        feed(wire, wire.len(), |command| {
            let command = command.unwrap();
            if let Some(delete) = command.decode_delete() {
                images.delete_frame(delete.unwrap(), |_| Ok(()));
            } else {
                reply = images.receive(command, |_| Ok(()));
            }
        });
        reply
    };
    let mut images = ImageStore::new(1024, 4);
    let base = format!("\x1b_Gi=1,f=24,s=16,v=16;{}\x1b\\", "/wAA".repeat(256));
    let next = format!("\x1b_Ga=f,i=1,f=24,s=16,v=16;{}\x1b\\", "AP8A".repeat(256));
    for wire in [
        base.as_bytes(),
        next.as_bytes(),
        b"\x1b_Ga=f,i=1,c=1,f=24,s=1,v=1;AAD/\x1b\\",
    ] {
        assert_eq!(receive(&mut images, wire).unwrap().message, b"OK");
    }
    receive(&mut images, b"\x1b_Ga=d,d=f,i=1,r=1\x1b\\");
    receive(&mut images, b"\x1b_Ga=a,i=1,c=2\x1b\\");
    let mut scratch = Vec::new();
    let dependent = images.get(1).unwrap().pixels(&mut scratch).unwrap();
    assert_eq!(&dependent[..8], &[0, 0, 255, 255, 255, 0, 0, 255]);

    receive(&mut images, b"\x1b_Ga=d,d=F,i=1,r=999\x1b\\");
    let second = format!("\x1b_Gi=2,f=24,s=16,v=16;{}\x1b\\", "AAAA".repeat(256));
    assert_eq!(
        receive(&mut images, second.as_bytes()).unwrap().message,
        b"OK"
    );
    let root = images.get(1).unwrap();
    assert_eq!(&root.pixels(&mut scratch).unwrap()[..4], &[0, 255, 0, 255]);
    let root_revision = root.pixel_revision();

    let patch = b"\x1b_Ga=f,i=1,c=1,f=24,s=1,v=1;AAD/\x1b\\";
    assert_eq!(receive(&mut images, patch).unwrap().message, b"OK");
    receive(&mut images, b"\x1b_Ga=a,i=1,c=2\x1b\\");
    let image = images.get(1).unwrap();
    let PixelUpdates::One(patch) = image
        .pixel_updates(Some(&root_revision), &mut scratch)
        .unwrap()
    else {
        panic!("Expected one new patch");
    };
    assert_eq!((patch.origin, patch.size), ([0, 0], [1, 1]));
    assert_eq!(patch.bytes, &[0, 0, 255, 255]);
    assert!(matches!(
        image.pixel_updates(None, &mut scratch),
        Some(PixelUpdates::Rasterize)
    ));
    let surface = image.pixels(&mut scratch).unwrap();
    assert_eq!(surface.len(), 1024);
    assert_eq!(&surface[..8], &[0, 0, 255, 255, 0, 255, 0, 255]);
}

#[test]
fn patch_transitions_restore_pixels_from_a_shared_base() {
    let mut images = ImageStore::new(16384, 1);
    let base = format!("\x1b_Gi=1,f=24,s=64,v=64,q=2;{}\x1b\\", "/wAA".repeat(4096));
    let apply = |images: &mut ImageStore, wire| {
        feed(wire, 5, |command| {
            let command = command.unwrap();
            if let Some(delete) = command.decode_delete() {
                images.delete_frame(delete.unwrap(), |_| Ok(()));
            } else {
                assert!(images.receive(command, |_| Ok(())).is_none());
            }
        });
    };
    apply(&mut images, base.as_bytes());
    apply(
        &mut images,
        b"\x1b_Ga=f,i=1,c=1,x=1,f=24,s=1,v=2,X=1,q=2;AAD/AAD/\x1b\\",
    );
    apply(
        &mut images,
        b"\x1b_Ga=f,i=1,c=1,x=2,y=1,f=24,s=1,v=1,X=1,q=2;AP8A\x1b\\",
    );
    apply(
        &mut images,
        b"\x1b_Ga=f,i=1,c=1,x=63,y=63,s=1,v=1,q=2;AP8AgA==\x1b\\",
    );
    let root = images.get(1).unwrap().pixel_revision();
    apply(&mut images, b"\x1b_Ga=a,i=1,c=2,q=2\x1b\\");
    let first = images.get(1).unwrap().pixel_revision();

    apply(&mut images, b"\x1b_Ga=a,i=1,c=3,q=2\x1b\\");
    let mut scratch = Vec::new();
    let image = images.get(1).unwrap();
    let PixelUpdates::One(update) = image.pixel_updates(Some(&first), &mut scratch).unwrap() else {
        panic!("Expected nearby patches to share one update");
    };
    assert_eq!(
        (update.origin, update.size, update.bytes_per_row),
        ([1, 0], [2, 2], 8)
    );
    let red = [255, 0, 0, 255];
    let green = [0, 255, 0, 255];
    assert_eq!(update.bytes.as_chunks::<4>().0, &[red, red, red, green]);

    apply(&mut images, b"\x1b_Ga=a,i=1,c=4,q=2\x1b\\");
    let image = images.get(1).unwrap();
    let PixelUpdates::Two([restored, blended]) =
        image.pixel_updates(Some(&first), &mut scratch).unwrap()
    else {
        panic!("Expected two updates for distant patches");
    };
    assert_eq!(
        (restored.origin, restored.size, restored.bytes_per_row),
        ([1, 0], [1, 2], 256)
    );
    assert_eq!(
        [&restored.bytes[..4], &restored.bytes[256..]],
        [&red[..]; 2]
    );
    assert_eq!(
        (blended.origin, blended.size, blended.bytes_per_row),
        ([63, 63], [1, 1], 4)
    );
    assert_eq!(blended.bytes, &[127, 128, 0, 255]);
    assert_eq!(scratch, [127, 128, 0, 255]);

    apply(
        &mut images,
        concat!(
            "\x1b_Ga=f,i=1,c=2,x=63,y=63,s=1,v=1,q=2;AP8AgA==\x1b\\",
            "\x1b_Ga=a,i=1,c=5,q=2\x1b\\",
        )
        .as_bytes(),
    );
    let edited = images.get(1).unwrap();
    let chained = edited.pixel_revision();
    let PixelUpdates::Two([far, near]) = edited.pixel_updates(Some(&root), &mut scratch).unwrap()
    else {
        panic!("Expected two updates after jumping over the intermediate frame");
    };
    assert_eq!(
        (far.origin, far.size, far.bytes),
        ([63, 63], [1, 1], &[127, 128, 0, 255][..])
    );
    assert_eq!((near.origin, near.size), ([1, 0], [1, 2]));
    assert_eq!(near.bytes.as_chunks::<4>().0, &[[0, 0, 255, 255]; 2]);
    assert_eq!(scratch, [127, 128, 0, 255]);

    apply(
        &mut images,
        b"\x1b_Ga=f,i=1,c=4,x=1,s=1,v=1,q=2;AAD/gA==\x1b\\\x1b_Ga=a,i=1,c=6,q=2\x1b\\",
    );
    let image = images.get(1).unwrap();
    let PixelUpdates::Two([near, far]) = image.pixel_updates(Some(&root), &mut scratch).unwrap()
    else {
        panic!("Expected separate updates for two blends");
    };
    assert_eq!((near.origin, near.size), ([1, 0], [1, 1]));
    assert_eq!(near.bytes, &[127, 0, 128, 255]);
    assert_eq!((far.origin, far.size), ([63, 63], [1, 1]));
    assert_eq!(far.bytes, &[127, 128, 0, 255]);
    assert_eq!(scratch.len(), 8);

    apply(
        &mut images,
        concat!(
            "\x1b_Ga=a,i=1,c=5,q=2\x1b\\",
            "\x1b_Ga=d,d=f,i=1,r=5\x1b\\",
            "\x1b_Ga=a,i=1,c=1,q=2\x1b\\",
        )
        .as_bytes(),
    );
    scratch.clear();
    let restored = images.get(1).unwrap();
    let PixelUpdates::Two([far, near]) = restored
        .pixel_updates(Some(&chained), &mut scratch)
        .unwrap()
    else {
        panic!("Expected both regions to be restored after deleting the selected frame");
    };
    assert_eq!(
        (far.origin, far.size, far.bytes),
        ([63, 63], [1, 1], &red[..])
    );
    assert_eq!((near.origin, near.size), ([1, 0], [1, 2]));
    assert_eq!([&near.bytes[..4], &near.bytes[256..]], [&red[..]; 2]);
    assert!(scratch.is_empty());

    apply(
        &mut images,
        b"\x1b_Ga=d,d=f,i=1,r=2\x1b\\\x1b_Ga=a,i=1,c=1,q=2\x1b\\",
    );
    let mut scratch = Vec::new();
    let image = images.get(1).unwrap();
    let PixelUpdates::One(update) = image.pixel_updates(Some(&first), &mut scratch).unwrap() else {
        panic!("Expected only the restored region");
    };
    assert_eq!(
        (update.origin, update.size, update.bytes_per_row),
        ([1, 0], [1, 2], 256)
    );
    assert_eq!(update.bytes.len(), 260);
    assert_eq!(update.bytes[..4], red);
    assert_eq!(update.bytes[256..], red);
    assert!(scratch.is_empty());
}

#[test]
fn batched_base_edits_upload_only_changed_parts_of_the_displayed_frame() {
    let mut images = ImageStore::new(16384, 1);
    let send = |images: &mut ImageStore, wire: &[u8]| {
        feed(wire, 17, |command| {
            if let Some(reply) = images.receive(command.unwrap(), |_| Ok(())) {
                assert_eq!(reply.message, b"OK");
            }
        });
    };
    let base = format!("\x1b_Gi=1,f=24,s=64,v=64;{}\x1b\\", "/wAA".repeat(4096));
    send(&mut images, base.as_bytes());
    send(
        &mut images,
        b"\x1b_Ga=f,i=1,c=1,x=32,y=32,f=24,s=1,v=1,X=1;AP8A\x1b\\\x1b_Ga=a,i=1,c=2\x1b\\",
    );
    let before = images.get(1).unwrap().pixel_revision();
    send(
        &mut images,
        b"\x1b_Ga=f,i=1,r=1,f=24,s=1,v=1,X=1;AAD/\x1b\\",
    );
    let intermediate = images.get(1).unwrap().pixel_revision();
    send(
        &mut images,
        b"\x1b_Ga=f,i=1,r=1,x=63,y=63,f=24,s=1,v=1,X=1;////\x1b\\",
    );

    let image = images.get(1).unwrap();
    let mut scratch = Vec::new();
    let PixelUpdates::Two([far, near]) = image.pixel_updates(Some(&before), &mut scratch).unwrap()
    else {
        panic!("Expected separate updates for the two edited corners");
    };
    assert_eq!((far.origin, far.size), ([63, 63], [1, 1]));
    assert_eq!(far.bytes, &[255; 4]);
    assert_eq!((near.origin, near.size), ([0, 0], [1, 1]));
    assert_eq!(near.bytes, &[0, 0, 255, 255]);
    assert_eq!(scratch.capacity(), 0);
    let PixelUpdates::One(update) = image
        .pixel_updates(Some(&intermediate), &mut scratch)
        .unwrap()
    else {
        panic!("Expected only the second edit after the intermediate revision");
    };
    assert_eq!((update.origin, update.size), ([63, 63], [1, 1]));
    assert_eq!(update.bytes, &[255; 4]);

    for (payload, color) in [("AP//", [0, 255, 255, 255]), ("/wD/", [255, 0, 255, 255])]
        .into_iter()
        .cycle()
        .take(32)
    {
        let wire = format!(
            "\x1b_Ga=f,i=1,r=1,f=24,s=1,v=1,X=1;{payload}\x1b\\\x1b_Ga=f,i=1,r=1,x=63,y=63,f=24,s=1,v=1,X=1;////\x1b\\"
        );
        send(&mut images, wire.as_bytes());
        let image = images.get(1).unwrap();
        assert_eq!(
            image
                .pixel_region([0, 0], [1, 1], &mut scratch)
                .unwrap()
                .bytes,
            color
        );
        assert_eq!(
            image
                .pixel_region([32, 32], [1, 1], &mut scratch)
                .unwrap()
                .bytes,
            &[0, 255, 0, 255]
        );
    }
}

#[test]
fn anonymous_pixels_live_with_their_placement_and_release_quota() {
    let mut images = ImageStore::new(4, 1);
    let mut pixels = None;
    let mut key = None;
    feed(b"\x1b_Ga=T,p=14,s=1,v=1;AQIDBA==\x1b\\", 5, |command| {
        assert!(
            images
                .receive(command.unwrap(), |change| {
                    let ImageChange::Place {
                        placement,
                        anonymous,
                        ..
                    } = change
                    else {
                        panic!("Expected a placement")
                    };
                    assert_eq!(placement.id, 0);
                    assert_eq!(placement.image.id(), 0);
                    key = Some(placement.image);
                    pixels = anonymous;
                    Ok(())
                })
                .is_none()
        );
    });
    let key = key.unwrap();
    assert_eq!(
        pixels.as_ref().unwrap().pixels(&mut Vec::new()).unwrap(),
        &[1, 2, 3, 4]
    );
    assert!(images.source_revision(key).is_some());
    let named = b"\x1b_Gi=1,s=1,v=1;AQIDBA==\x1b\\";
    feed(named, named.len(), |command| {
        assert_eq!(
            images
                .receive(command.unwrap(), |change| {
                    if let ImageChange::RankEvictions { ranking, .. } = change {
                        ranking.protect(key)?;
                    }
                    Ok(())
                })
                .unwrap()
                .message,
            b"ENOSPC:Image quota exceeded"
        );
    });
    drop(pixels.take());
    assert!(images.source_revision(key).is_none());
    feed(named, named.len(), |command| {
        assert_eq!(
            images
                .receive(command.unwrap(), |_| Ok(()))
                .unwrap()
                .message,
            b"OK"
        );
    });
    assert!(images.get(1).is_some());
}

#[test]
fn direct_rgb_chunks_preserve_pixel_order_at_the_storage_limit() {
    use std::io::Write as _;

    let rgb: Vec<_> = (0..257 * 96)
        .map(|index| {
            let red = (index % 251) as u8;
            [red, red >> 3, red % 17]
        })
        .collect();
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(rgb.as_flattened()).unwrap();
    let compressed = encoder.finish().unwrap();
    assert!(compressed.len().div_ceil(3) * 4 <= 4096);
    let expected: Vec<_> = rgb.iter().map(|&[r, g, b]| [r, g, b, 255]).collect();
    for (bytes, compression, block) in [
        (rgb.as_flattened(), "", 52),
        (&compressed, "o=z,", 52),
        (&compressed, "o=z,", 4096),
    ] {
        let encoded = PAYLOAD_ENGINE.encode(bytes);
        let mut wire = String::new();
        for (index, chunk) in encoded.as_bytes().chunks(block).enumerate() {
            let more = u8::from((index + 1) * block < encoded.len());
            wire.push_str("\x1b_G");
            if index == 0 {
                wire.push_str("i=1,f=24,s=257,v=96,");
                wire.push_str(compression);
            }
            wire.push_str(&format!(
                "m={more};{}\x1b\\",
                str::from_utf8(chunk).unwrap()
            ));
        }
        wire.push_str("\x1b_Gi=1,f=24,s=1,v=1;AQI=\x1b\\");

        let mut images = ImageStore::new(rgb.len() * 4, 1);
        let mut replies = Vec::new();
        feed(wire.as_bytes(), 11, |command| {
            let command = command.unwrap();
            let more = command.controls.get(b'm') == Some(Value::Unsigned(1));
            let reply = images.receive(command, |_| Ok(()));
            if more {
                assert!(reply.is_none() && images.get(1).is_none());
            }
            replies.extend(reply.map(|reply| reply.message));
        });
        assert_eq!(
            replies,
            [b"OK".as_slice(), b"ENODATA:Insufficient image data"]
        );
        let mut scratch = Vec::new();
        let rgba = images.get(1).unwrap().pixels(&mut scratch).unwrap();
        assert_eq!(rgba, expected.as_flattened());
    }
}

#[test]
fn zlib_checks_stream_boundaries_and_png_expands_paletted_alpha() {
    let mut images = ImageStore::new(24, 3);
    let wire = concat!(
        "\x1b_Gi=1,s=1,v=1,o=z,m=1;eJxj\x1b\\\x1b_Gm=0;ZGJmAQAAGAAL\x1b\\",
        "\x1b_Gi=1,s=1,v=1,o=z;eJxjZGJmAQAAGAALWg==\x1b\\",
        "\x1b_Gi=1,s=1,v=1,o=z,m=1;eJxj\x1b\\\x1b_Gm=0;ZGJmAQAAGAA=\x1b\\",
        "\x1b_Gi=1,s=1,v=1,o=z,m=1;eJxj\x1b\\\x1b_Gm=0;ZGJmAQAAGAAM\x1b\\",
        "\x1b_Gi=1,s=1,v=1,o=z,m=1;eJxj\x1b\\\x1b_Gm=0;ZGJmYQUAACgAEA==\x1b\\",
        "\x1b_Gi=1,s=1,v=1,o=z,m=1;eJxjZGJmAQAAGAAL\x1b\\\x1b_Gm=0\x1b\\",
        "\x1b_Gi=1,s=1,v=1,o=z,m=1;eJxjZGJmAQAAGAAL\x1b\\\x1b_Gm=0;Wg==\x1b\\",
        "\x1b_Gi=2,f=100,o=z,S=99,m=1;eJzrDPBz5+WS4mJg\x1b\\",
        "\x1b_Gm=0;YOD19HAJYmBgYARhRmYGBgbV22GnGBgY2AJ8QlwZGBgElYwXPAzfx8DAwFQS5Bf8v4GDf3MWAwMDl6eLY0jFnOQMBgaGJobG8rNF2xgYGBg8Xf1c1jklNAEAsUoWww==\x1b\\",
        "\x1b_Gi=3,f=24,s=2,v=2,o=z;eJxjZGJmYWVj5+Dk4uYBAAF4AE8=\x1b\\",
        "\x1b_Gi=3,f=24,s=1,v=3,o=z;eJxjZGJmYWVj5+Dk4uYBAAF4AE8=\x1b\\",
    );
    let mut replies = Vec::new();
    feed(wire.as_bytes(), 7, |command| {
        replies.extend(
            images
                .receive(command.unwrap(), |_| Ok(()))
                .map(|reply| reply.message),
        );
    });
    let bad = b"EINVAL:Invalid image data".as_slice();
    let ok = b"OK".as_slice();
    assert_eq!(replies, [ok, bad, bad, bad, bad, ok, bad, ok, ok, bad]);
    assert_eq!(
        images.get(1).unwrap().pixels(&mut Vec::new()).unwrap(),
        &[1, 2, 3, 4]
    );
    assert_eq!(
        images.get(2).unwrap().pixels(&mut Vec::new()).unwrap(),
        &[17, 34, 51, 128]
    );
    assert_eq!(
        images.get(3).unwrap().pixels(&mut Vec::new()).unwrap(),
        &[1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]
    );
}

#[test]
fn rejected_png_uploads_preserve_the_existing_image() {
    use std::io::Write as _;

    let mut images = ImageStore::new(4, 1);
    let mut receive = |wire: &str| {
        let mut reply = None;
        feed(wire.as_bytes(), 7, |command| {
            reply = images.receive(command.unwrap(), |_| Ok(()));
        });
        let reply = reply.unwrap();
        assert_eq!(reply.id, 1);
        (reply.message, images.get(1).unwrap().source_revision())
    };
    let (message, original) = receive("\x1b_Gi=1,s=1,v=1;AQIDBA==\x1b\\");
    assert_eq!(message, b"OK");

    // The IDAT CRC is valid. Only the zlib stream's Adler-32 is wrong.
    let (message, retained) = receive(concat!(
        "\x1b_Gi=1,f=100;",
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGM44NBwAAAFhQJAPU4x7AAAAABJRU5ErkJggg==",
        "\x1b\\",
    ));
    assert_eq!(message, b"EBADPNG:Invalid PNG image data");
    assert_eq!(retained, original);

    let (message, retained) = receive(concat!(
        "\x1b_Gi=1,f=100;",
        "iVBORw0KGgoAAAANSUhEUgAAJxEAAAABCAYAAACXHN81AAAADUlEQVR4nGNgZGL+DwABFAEG1rmmRQAAAABJRU5ErkJggg==",
        "\x1b\\",
    ));
    assert_eq!(message, b"ENOMEM:PNG image is too large");
    assert_eq!(retained, original);

    let png = PAYLOAD_ENGINE
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAACklEQVR4nGM44NBwAAAFqPLXkQAAAANJREFUhQJBLdmETQAAAABJRU5ErkJggg==")
        .unwrap();
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&png).unwrap();
    let compressed = encoder.finish().unwrap();
    let size = png.len();
    let payload = PAYLOAD_ENGINE.encode(&compressed);
    let truncated = PAYLOAD_ENGINE.encode(&compressed[..compressed.len() - 1]);
    for (size, payload) in [
        (0, &payload),
        (size - 1, &payload),
        (size + 1, &payload),
        (size, &truncated),
    ] {
        let (message, retained) = receive(&format!("\x1b_Gi=1,f=100,o=z,S={size};{payload}\x1b\\"));
        assert_eq!(message, b"EINVAL:Invalid image data");
        assert_eq!(retained, original);
    }
    let (message, replaced) = receive(&format!("\x1b_Gi=1,f=100,o=z,S={size};{payload}\x1b\\"));
    assert_eq!(message, b"OK");
    assert_ne!(replaced, original);
    assert_eq!(
        images.get(1).unwrap().pixels(&mut Vec::new()).unwrap(),
        &[192, 64, 128, 192]
    );
}

#[test]
fn png_expansion_preserves_pixels_across_rows() {
    let cases = [
        (
            png::ColorType::Rgb,
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12].as_slice(),
            [
                [1, 2, 3, 255],
                [4, 5, 6, 255],
                [7, 8, 9, 255],
                [10, 11, 12, 255],
            ],
        ),
        (
            png::ColorType::Grayscale,
            &[1, 2, 3, 4],
            [
                [1, 1, 1, 255],
                [2, 2, 2, 255],
                [3, 3, 3, 255],
                [4, 4, 4, 255],
            ],
        ),
        (
            png::ColorType::GrayscaleAlpha,
            &[1, 0, 2, 128, 3, 192, 4, 255],
            [[1, 1, 1, 0], [2, 2, 2, 128], [3, 3, 3, 192], [4, 4, 4, 255]],
        ),
    ];
    for (color, input, expected) in cases {
        let mut png = Vec::new();
        let mut encoder = png::Encoder::new(&mut png, 2, 2);
        encoder.set_color(color);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(input)
            .unwrap();
        let wire = format!("\x1b_Gi=1,f=100;{}\x1b\\", PAYLOAD_ENGINE.encode(&png));
        let mut images = ImageStore::new(16, 1);
        feed(wire.as_bytes(), 7, |command| {
            let reply = images.receive(command.unwrap(), |_| Ok(())).unwrap();
            assert_eq!(reply.message, b"OK");
        });
        assert_eq!(
            images.get(1).unwrap().pixels(&mut Vec::new()).unwrap(),
            expected.as_flattened()
        );
    }
}

#[test]
fn png_gamma_corrects_color_at_source_precision_and_keeps_alpha() {
    let rgba8 = [1, 64, 128, 255, 16, 128, 200, 128];
    let rgba16 = [129u16, 255, 384, 385, 0x7f80, 0x8080, 0xfeff, 0xff00]
        .into_iter()
        .flat_map(u16::to_be_bytes)
        .collect::<Vec<_>>();
    let cases = [
        (
            png::BitDepth::Eight,
            rgba8.as_slice(),
            [21, 136, 186, 255, 72, 186, 228, 128],
        ),
        (
            png::BitDepth::Sixteen,
            rgba16.as_slice(),
            [14, 19, 24, 1, 186, 186, 255, 255],
        ),
    ];
    for (depth, pixels, expected) in cases {
        let mut png = Vec::new();
        let mut encoder = png::Encoder::new(&mut png, 2, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(depth);
        encoder.set_source_gamma(png::ScaledFloat::from_scaled(100_000));
        encoder
            .write_header()
            .unwrap()
            .write_image_data(pixels)
            .unwrap();

        let wire = format!("\x1b_Gi=1,f=100;{}\x1b\\", PAYLOAD_ENGINE.encode(&png));
        let mut images = ImageStore::new(8, 1);
        feed(wire.as_bytes(), 11, |command| {
            let reply = images.receive(command.unwrap(), |_| Ok(())).unwrap();
            assert_eq!(reply.message, b"OK");
        });
        assert_eq!(
            images.get(1).unwrap().pixels(&mut Vec::new()).unwrap(),
            expected
        );
    }
}

#[test]
fn png_chromaticities_and_gamma_convert_to_srgb() {
    use png::{BitDepth, ScaledFloat, SourceChromaticities};

    let pair = |x, y| (ScaledFloat::from_scaled(x), ScaledFloat::from_scaled(y));
    let p3 = SourceChromaticities {
        white: pair(31270, 32900),
        red: pair(68000, 32000),
        green: pair(26500, 69000),
        blue: pair(15000, 6000),
    };
    let mut images = ImageStore::new(4, 1);
    for (depth, primaries, gamma, input, expected) in [
        (
            BitDepth::Eight,
            p3,
            100_000,
            [128, 64, 32, 127].as_slice(),
            [197, 135, 92, 127],
        ),
        (
            BitDepth::Eight,
            p3,
            45_455,
            &[128, 64, 32, 127],
            [139, 57, 11, 127],
        ),
        (
            BitDepth::Sixteen,
            p3,
            100_000,
            &[0, 127, 0, 255, 1, 128, 127, 128],
            [5, 13, 18, 127],
        ),
        (
            BitDepth::Eight,
            SourceChromaticities {
                green: p3.red,
                ..p3
            },
            100_000,
            &[128, 64, 32, 127],
            [186, 136, 99, 127],
        ),
    ] {
        let mut png = Vec::new();
        let mut encoder = png::Encoder::new(&mut png, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(depth);
        encoder.set_source_chromaticities(primaries);
        encoder.set_source_gamma(ScaledFloat::from_scaled(gamma));
        encoder
            .write_header()
            .unwrap()
            .write_image_data(input)
            .unwrap();

        let wire = format!("\x1b_Gi=1,f=100;{}\x1b\\", PAYLOAD_ENGINE.encode(&png));
        feed(wire.as_bytes(), 13, |command| {
            let reply = images.receive(command.unwrap(), |_| Ok(())).unwrap();
            assert_eq!(reply.message, b"OK");
        });
        let mut scratch = Vec::new();
        let pixels = images.get(1).unwrap().pixels(&mut scratch).unwrap();
        assert_eq!(pixels[3], expected[3]);
        assert!(
            pixels[..3]
                .iter()
                .zip(expected)
                .all(|(&actual, target)| actual.abs_diff(target) <= 1),
            "{pixels:?} vs {expected:?}"
        );
    }
}

#[test]
fn png_cicp_uses_the_highest_supported_color_metadata() {
    for (codes, depth, input, embedded, expected) in [
        (
            [12, 13, 0, 1],
            png::BitDepth::Eight,
            [185, 96, 40, 127].as_slice(),
            moxcms::ColorProfile::new_srgb(),
            [199, 90, 14, 127],
        ),
        (
            [1, 13, 0, 1],
            png::BitDepth::Eight,
            &[185, 96, 40, 127],
            moxcms::ColorProfile::new_display_p3(),
            [185, 96, 40, 127],
        ),
        (
            [12, 255, 0, 1],
            png::BitDepth::Eight,
            &[185, 96, 40, 127],
            moxcms::ColorProfile::new_display_p3(),
            [199, 90, 14, 127],
        ),
        (
            [12, 8, 0, 1],
            png::BitDepth::Sixteen,
            &[0, 127, 0, 255, 1, 128, 127, 128],
            moxcms::ColorProfile::new_srgb(),
            [5, 13, 18, 127],
        ),
        (
            [1, 13, 0, 0],
            png::BitDepth::Eight,
            &[16, 235, 126, 129],
            moxcms::ColorProfile::new_srgb(),
            [0, 255, 128, 129],
        ),
        (
            [12, 8, 0, 0],
            png::BitDepth::Eight,
            &[64, 0, 64, 127],
            moxcms::ColorProfile::new_srgb(),
            [145, 0, 135, 127],
        ),
        (
            [1, 1, 0, 0],
            png::BitDepth::Eight,
            &[18, 248, 168, 94],
            moxcms::ColorProfile::new_srgb(),
            [7, 255, 185, 94],
        ),
        (
            [1, 1, 0, 0],
            png::BitDepth::Sixteen,
            &[0x10, 0, 0xeb, 0, 0x7d, 0x80, 127, 128],
            moxcms::ColorProfile::new_srgb(),
            [0, 255, 139, 127],
        ),
        (
            [9, 16, 0, 0],
            png::BitDepth::Eight,
            &[120, 110, 96, 63],
            moxcms::ColorProfile::new_srgb(),
            [179, 125, 86, 63],
        ),
        (
            [9, 16, 0, 1],
            png::BitDepth::Sixteen,
            &[0x75, 0x30, 0x82, 0xdc, 0x90, 0x88, 0x7f, 0xb2],
            moxcms::ColorProfile::new_srgb(),
            [101, 193, 244, 127],
        ),
        (
            [9, 18, 0, 1],
            png::BitDepth::Eight,
            &[191, 191, 191, 65],
            moxcms::ColorProfile::new_srgb(),
            [240, 240, 240, 65],
        ),
        (
            [9, 18, 0, 0],
            png::BitDepth::Sixteen,
            &[0x80, 0xe8, 0x7d, 0x00, 0x7b, 0x0c, 0x80, 0xbf],
            moxcms::ColorProfile::new_srgb(),
            [144, 136, 134, 128],
        ),
    ] {
        let mut png = Vec::new();
        let mut info = png::Info::with_size(1, 1);
        info.color_type = png::ColorType::Rgba;
        info.bit_depth = depth;
        info.icc_profile = Some(embedded.encode().unwrap().into());
        let mut writer = png::Encoder::with_info(&mut png, info)
            .unwrap()
            .write_header()
            .unwrap();
        writer.write_chunk(png::chunk::cICP, &codes).unwrap();
        writer.write_chunk(png::chunk::sRGB, &[0]).unwrap();
        writer.write_image_data(input).unwrap();
        writer.finish().unwrap();

        let wire = format!("\x1b_Gi=1,f=100;{}\x1b\\", PAYLOAD_ENGINE.encode(&png));
        let mut images = ImageStore::new(4, 1);
        feed(wire.as_bytes(), 13, |command| {
            let reply = images.receive(command.unwrap(), |_| Ok(())).unwrap();
            assert_eq!(reply.message, b"OK");
        });
        let mut scratch = Vec::new();
        let pixels = images.get(1).unwrap().pixels(&mut scratch).unwrap();
        assert_eq!(pixels[3], expected[3]);
        assert!(
            pixels[..3]
                .iter()
                .zip(expected)
                .all(|(&actual, target)| actual.abs_diff(target) <= 1),
            "{pixels:?} vs {expected:?}"
        );
    }
}

#[test]
fn png_embedded_profiles_convert_to_srgb_and_keep_alpha() {
    use moxcms::ColorProfile;

    let linear_gray = ColorProfile::new_gray_with_gamma(1.0);
    let mut linear_rgb = ColorProfile::new_srgb();
    linear_rgb.cicp = None;
    linear_rgb.red_trc = linear_gray.gray_trc.clone();
    linear_rgb.green_trc = linear_gray.gray_trc.clone();
    linear_rgb.blue_trc = linear_gray.gray_trc.clone();
    let mut images = ImageStore::new(8, 1);
    for (depth, color, profile, input, expected) in [
        (
            png::BitDepth::Eight,
            png::ColorType::Rgba,
            ColorProfile::new_display_p3(),
            [185, 96, 40, 127].as_slice(),
            [[199, 90, 14, 127]].as_slice(),
        ),
        (
            png::BitDepth::Eight,
            png::ColorType::GrayscaleAlpha,
            ColorProfile::new_gray_with_gamma(1.0),
            &[1, 31, 128, 63],
            &[[13, 13, 13, 31], [188, 188, 188, 63]],
        ),
        (
            png::BitDepth::Sixteen,
            png::ColorType::Rgba,
            linear_rgb,
            &[0, 127, 0, 255, 1, 128, 127, 128],
            &[[6, 13, 18, 127]],
        ),
        (
            png::BitDepth::Sixteen,
            png::ColorType::GrayscaleAlpha,
            linear_gray,
            &[0, 127, 31, 128, 0, 255, 63, 128],
            &[[6, 6, 6, 31], [13, 13, 13, 63]],
        ),
        (
            png::BitDepth::Sixteen,
            png::ColorType::GrayscaleAlpha,
            ColorProfile::new_gray_with_gamma(2.2),
            &[0, 127, 31, 128, 0, 255, 63, 128],
            &[[0, 0, 0, 31], [0, 0, 0, 63]],
        ),
    ] {
        let profile = profile.encode().unwrap();
        let mut png = Vec::new();
        let mut info = png::Info::with_size(expected.len() as u32, 1);
        info.bit_depth = depth;
        info.color_type = color;
        info.icc_profile = Some(profile.into());
        info.source_gamma = Some(png::ScaledFloat::from_scaled(45_455));
        png::Encoder::with_info(&mut png, info)
            .unwrap()
            .write_header()
            .unwrap()
            .write_image_data(input)
            .unwrap();

        let wire = format!("\x1b_Gi=1,f=100;{}\x1b\\", PAYLOAD_ENGINE.encode(&png));
        feed(wire.as_bytes(), 17, |command| {
            let reply = images.receive(command.unwrap(), |_| Ok(())).unwrap();
            assert_eq!(reply.message, b"OK");
        });
        let mut scratch = Vec::new();
        let pixels = images.get(1).unwrap().pixels(&mut scratch).unwrap();
        assert_eq!(pixels.len(), expected.len() * 4);
        for (pixel, reference) in pixels.as_chunks::<4>().0.iter().zip(expected) {
            assert_eq!(pixel[3], reference[3]);
            assert!(
                pixel[..3]
                    .iter()
                    .zip(reference)
                    .all(|(&a, &b)| a.abs_diff(b) <= 1),
                "{pixel:?}"
            );
        }
    }
}

#[cfg(target_os = "linux")]
fn local_reply(images: &mut ImageStore, controls: &str, name: &[u8]) -> Reply {
    let wire = format!("\x1b_G{controls};{}\x1b\\", PAYLOAD_ENGINE.encode(name));
    let mut reply = None;
    feed(wire.as_bytes(), 11, |command| {
        reply = images.receive(command.unwrap(), |_| Ok(()));
    });
    reply.unwrap()
}

#[cfg(target_os = "linux")]
#[test]
fn local_transfers_use_checked_ranges_and_unlink_only_tagged_temp_files() {
    use rustix::fs::{CWD, Mode, mkfifoat};
    use std::{
        fs,
        os::unix::fs::symlink,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct Directory(PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("lune-graphics-{nonce}"));
    fs::create_dir(&path).unwrap();
    let directory = Directory(path);
    let file = directory.0.join("pixels");
    let link = directory.0.join("link");
    fs::write(&file, [9, 9, 1, 2, 3, 4, 8]).unwrap();
    symlink(&file, &link).unwrap();
    let fifo = directory.0.join("fifo");
    mkfifoat(CWD, &fifo, Mode::RUSR).unwrap();

    let mut images = ImageStore::new(16, 4);
    let b = |path: &std::path::Path| path.as_os_str().as_encoded_bytes().to_vec();
    assert_eq!(
        local_reply(&mut images, "i=1,t=f,s=1,v=1,S=5,O=2", &b(&link)).message,
        b"OK"
    );
    let revision = images.get(1).unwrap().source_revision();
    assert_eq!(
        local_reply(&mut images, "a=q,i=1,t=f,s=1,v=1,S=80,O=2", &b(&link)).message,
        b"OK"
    );
    assert_eq!(images.get(1).unwrap().source_revision(), revision);
    for controls in ["a=q,i=1,t=f,s=1,v=1,S=3,O=2", "i=1,t=f,s=1,v=1,S=6,O=4"] {
        assert_eq!(
            local_reply(&mut images, controls, &b(&link)).message,
            b"EBADF:Failed to read image file"
        );
        assert_eq!(images.get(1).unwrap().source_revision(), revision);
    }
    for path in [&fifo, &directory.0.join("missing"), &file] {
        let reply = local_reply(&mut images, "i=1,t=f,s=1,v=1,S=4,O=5", &b(path));
        assert_eq!(reply.message, b"EBADF:Failed to read image file");
    }
    for (controls, expected) in [
        (
            "a=q,i=1,t=f,f=100",
            b"EBADPNG:Invalid PNG image data".as_slice(),
        ),
        ("a=q,i=1,t=f,f=100,O=7", b"EBADPNG:Invalid PNG image data"),
        ("a=q,i=1,t=f,f=100,S=8", b"EBADF:Failed to read image file"),
        (
            "a=q,i=1,t=f,f=100,o=z",
            b"EINVAL:Invalid image data".as_slice(),
        ),
    ] {
        assert_eq!(
            local_reply(&mut images, controls, &b(&link)).message,
            expected
        );
    }
    assert_eq!(
        images.get(1).unwrap().pixels(&mut Vec::new()).unwrap(),
        &[1, 2, 3, 4]
    );
    assert_eq!(
        local_reply(&mut images, "i=3,t=f,f=24,s=2,v=1,S=6,O=1", &b(&link)).message,
        b"OK"
    );
    assert_eq!(
        images.get(3).unwrap().pixels(&mut Vec::new()).unwrap(),
        &[9, 1, 2, 255, 3, 4, 8, 255]
    );
    assert_eq!(
        local_reply(&mut images, "i=1,t=t,s=1,v=1,S=4,O=2", &b(&file)).message,
        b"OK"
    );
    assert!(file.exists());

    let temporary = directory.0.join("tty-graphics-protocol-image");
    let encoded = PAYLOAD_ENGINE.decode("eJzrDPBz5+WS4mJgYOD19HAJYmBgYARhDjYGBgZ50SOdYAkXx5CKOck/zh/4IM/AysD4v3OmrSwDAwODp6ufyzqnhCYAzMQQHg==").unwrap();
    let mut stored = vec![0; 2];
    stored.extend_from_slice(&encoded);
    stored.push(0);
    fs::write(&temporary, stored).unwrap();
    let controls = format!("i=2,t=t,f=100,o=z,O=2,S={}", encoded.len() + 1);
    assert_eq!(
        local_reply(&mut images, &controls, &b(&temporary)).message,
        b"OK"
    );
    assert_eq!(
        images.get(2).unwrap().pixels(&mut Vec::new()).unwrap(),
        &[255, 0, 0, 255]
    );
    assert!(!temporary.exists());

    let pixel_count = 64 * 32;
    let mut ranged = vec![0; 7];
    ranged.extend((0..pixel_count).flat_map(|_| [9, 32, 81]));
    ranged.push(0);
    fs::write(&file, ranged).unwrap();
    let mut images = ImageStore::new(pixel_count * 4, 1);
    let controls = format!("i=1,t=f,f=24,s=64,v=32,O=7,S={}", pixel_count * 3);
    assert_eq!(
        local_reply(&mut images, &controls, &b(&file)).message,
        b"OK"
    );
    let mut scratch = Vec::new();
    let pixels = images.get(1).unwrap().pixels(&mut scratch).unwrap();
    assert_eq!(pixels.len(), pixel_count * 4);
    assert!(
        pixels
            .as_chunks::<4>()
            .0
            .iter()
            .all(|&pixel| pixel == [9, 32, 81, 255])
    );
}

#[cfg(target_os = "linux")]
#[test]
fn posix_shared_transfer_reads_and_unlinks_the_object() {
    use rustix::shm::{self, Mode, OFlags};
    use std::{
        fs::File,
        io::Write,
        time::{SystemTime, UNIX_EPOCH},
    };

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!("lune-graphics-{nonce}");
    struct Cleanup<'a>(&'a str);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = shm::unlink(self.0);
        }
    }
    let _cleanup = Cleanup(&name);
    let mut images = ImageStore::new(4, 1);
    let transfer = |images: &mut ImageStore, controls: &str, data: &[u8]| {
        let file = shm::open(
            name.as_str(),
            OFlags::CREATE | OFlags::EXCL | OFlags::RDWR,
            Mode::RUSR | Mode::WUSR,
        )
        .unwrap();
        File::from(file).write_all(data).unwrap();
        let reply = local_reply(images, controls, name.as_bytes());
        assert_eq!(
            shm::open(name.as_str(), OFlags::RDONLY, Mode::empty()).unwrap_err(),
            rustix::io::Errno::NOENT
        );
        reply.message
    };
    for (controls, expected) in [
        ("i=3,t=s,f=16,s=1,v=1,O=1,S=3", b"OK".as_slice()),
        (
            "i=4,t=s,f=16,s=1,v=1,O=3,S=4",
            b"EBADF:Failed to read image file",
        ),
        ("a=q,i=4,t=s,f=24,s=1,v=1,O=1,S=80", b"OK"),
    ] {
        assert_eq!(transfer(&mut images, controls, &[0, 3, 17, 0]), expected);
    }
    let image = images.get(3).unwrap();
    assert_eq!(image.format(), ImageFormat::IndexedAlpha);
    assert_eq!(image.pixels(&mut Vec::new()).unwrap(), &[3, 17]);
    assert!(images.get(4).is_none());

    let compressed = PAYLOAD_ENGINE.decode("eJxjZGJmAQAAGAAL").unwrap();
    let mut padded = vec![0; 4096];
    padded[1..][..compressed.len()].copy_from_slice(&compressed);
    let controls = "i=3,t=s,s=1,v=1,o=z,O=1";
    assert_eq!(transfer(&mut images, controls, &padded), b"OK");
    let revision = images.get(3).unwrap().source_revision();
    let short = format!("{controls},S={}", compressed.len() - 1);
    for controls in [short.as_str(), "i=3,t=s,f=24,s=1,v=1,o=z,O=1"] {
        assert_eq!(
            transfer(&mut images, controls, &padded),
            b"EINVAL:Invalid image data"
        );
    }
    let image = images.get(3).unwrap();
    assert_eq!(image.source_revision(), revision);
    assert_eq!(image.pixels(&mut Vec::new()).unwrap(), &[1, 2, 3, 4]);
}

#[test]
fn payload_allocation_failures_keep_reply_identity_and_quiet() {
    for (pending, header, report) in [
        (
            b"".as_slice(),
            b"Ga=T,i=41,p=9,f=24,s=2000,v=1,C=1;".as_slice(),
            true,
        ),
        (
            b"\x1b_Ga=T,i=41,p=9,f=24,s=2001,v=1,C=1,q=2,m=1;/wAA\x1b\\",
            b"Gm=0,q=1;",
            true,
        ),
        (
            b"\x1b_Ga=T,i=41,p=9,f=24,s=2001,v=1,C=1,m=1;/wAA\x1b\\",
            b"Gm=0,q=2;",
            false,
        ),
    ] {
        let mut images = ImageStore::new(8192, 1);
        feed(b"\x1b_Gi=41,f=24,s=1,v=1,q=2;/wAA\x1b\\", 9, |command| {
            assert!(images.receive(command.unwrap(), |_| Ok(())).is_none());
        });
        feed(pending, 5, |command| {
            assert!(images.receive(command.unwrap(), |_| Ok(())).is_none());
        });

        let mut decoder = Decoder::default();
        decoder.start();
        decoder.push(header);
        decoder.state = State::DiscardedPayload(PayloadDiscard::Allocation);
        let command = decoder.finish().unwrap().unwrap();
        assert_eq!(command.encoded_payload(), Err(PayloadError::Allocation));
        let reply = images.receive(command, |_| {
            panic!("Failed uploads cannot change placements")
        });
        assert_eq!(
            reply.map(|reply| (reply.id, reply.placement_id, reply.message)),
            report.then_some((41, 9, b"ENOSPC:Image quota exceeded".as_slice())),
        );
        assert_eq!(images.get(41).unwrap().dimensions(), (1, 1));
    }
}

#[test]
fn payload_bounds_and_nonfinal_block_alignment_are_enforced() {
    let mut decoder = Decoder::default();
    decoder.start();
    decoder.push(b"Gm=1;");
    let first = vec![b'A'; MAX_DIRECT_PAYLOAD_BYTES];
    for piece in first.chunks(2047) {
        decoder.push(piece);
    }
    let command = decoder.finish().unwrap().unwrap();
    let mut output = vec![0xff; MAX_DECODED_PAYLOAD_BYTES];
    assert_eq!(
        command.decode_payload(&mut output),
        Ok(MAX_DECODED_PAYLOAD_BYTES)
    );
    assert!(output.iter().all(|&byte| byte == 0));

    let oversized = vec![b'A'; MAX_DIRECT_PAYLOAD_BYTES + 1];
    decoder.start();
    decoder.push(b"G;");
    decoder.push(&oversized);
    assert_eq!(
        decoder
            .finish()
            .unwrap()
            .unwrap()
            .decode_payload(&mut output),
        Err(PayloadError::TooLong)
    );

    for payload in [b"%ignored", &first[..INLINE_ENCODED_BYTES + 1], &oversized] {
        decoder.start();
        decoder.push(b"Ga=q,e=T,i=19,m=2;");
        decoder.push(payload);
        let command = decoder.finish().unwrap().unwrap();
        assert_eq!(command.encoded_payload(), Ok(&[][..]));
        assert_eq!(command.decode_payload(&mut []), Ok(0));
    }

    decoder.start();
    decoder.push(b"Gt=f;");
    let path = [b'x'; MAX_PATH_BYTES - 1];
    decoder.push(PAYLOAD_ENGINE.encode(path).as_bytes());
    let command = decoder.finish().unwrap().unwrap();
    assert_eq!(command.decode_payload(&mut output), Ok(path.len()));
    assert_eq!(&output[..path.len()], &path);
    decoder.start();
    decoder.push(b"Gt=f;");
    decoder.push(&[b'A'; INLINE_ENCODED_BYTES + 1]);
    assert_eq!(
        decoder
            .finish()
            .unwrap()
            .unwrap()
            .decode_payload(&mut output),
        Err(PayloadError::TooLong)
    );

    decoder.start();
    decoder.push(b"Gm=1;AQ==");
    assert_eq!(
        decoder
            .finish()
            .unwrap()
            .unwrap()
            .decode_payload(&mut output),
        Err(PayloadError::InvalidChunk)
    );
    decoder.start();
    decoder.push(b"Gm=0;AQ");
    assert_eq!(
        decoder
            .finish()
            .unwrap()
            .unwrap()
            .decode_payload(&mut output),
        Ok(1)
    );
    assert_eq!(output[0], 1);

    decoder.start();
    decoder.push(b"G");
    decoder.push(&[b'x'; MAX_CONTROL_BYTES + 1]);
    assert_eq!(decoder.finish(), Some(Err(DecodeError::ControlTooLong)));
}

#[test]
fn large_direct_transfer_commits_on_matching_action_in_the_final_chunk() {
    let mut wire = b"\x1b_Ga=T,i=1,f=24,s=128,v=257,m=1;".to_vec();
    wire.extend(std::iter::repeat_n(b'A', MAX_DIRECT_PAYLOAD_BYTES));
    wire.extend(b"\x1b\\\x1b_Ga=T;");
    wire.extend(std::iter::repeat_n(b'/', 512));
    wire.extend(b"\x1b\\");

    let mut images = ImageStore::new(256 * 1024, 1);
    let mut placed = 0;
    let mut replies = 0;
    feed(&wire, 2047, |command| {
        if let Some(reply) = images.receive(command.unwrap(), |change| {
            placed += usize::from(matches!(change, ImageChange::Place { .. }));
            Ok(())
        }) {
            assert_eq!(reply.message, b"OK");
            replies += 1;
        }
    });
    assert_eq!((replies, placed), (1, 1));
    let mut scratch = Vec::new();
    let image = images.get(1).unwrap();
    let pixels = image.pixels(&mut scratch).unwrap();
    assert_eq!(image.dimensions(), (128, 257));
    assert_eq!(&pixels[..4], &[0, 0, 0, 255]);
    assert!(
        pixels[pixels.len() - 128 * 4..]
            .iter()
            .all(|&byte| byte == 255)
    );
}
