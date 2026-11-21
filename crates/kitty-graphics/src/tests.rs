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

    assert_eq!(
        receive(&mut images, b"\x1b_Gi=99,s=1,v=1;AAAA/w==\x1b\\").message,
        b"OK"
    );
    assert!(images.get(2).is_none());
    assert_eq!(images.newest_id(17), Some(1));

    assert_eq!(
        receive(&mut images, b"\x1b_Gi=1,s=1,v=1;AAAA/w==\x1b\\").message,
        b"OK"
    );
    assert!(images.get(1).is_some());
    assert_eq!(images.newest_id(17), None);
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

    let reply = receive(
        &mut images,
        b"\x1b_Ga=f,i=1,r=2,s=3,v=1;AAAAAAAAAAAAAAAA\x1b\\",
    )
    .unwrap();
    assert_eq!(reply.message, b"EINVAL:Invalid animation frame region");
    assert_eq!(images.visual_revision(), selected);
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
    assert_eq!((restored.origin, restored.size), ([1, 0], [1, 2]));
    assert_eq!(restored.bytes.as_chunks::<4>().0, &[red, red]);
    assert_eq!((blended.origin, blended.size), ([63, 63], [1, 1]));
    assert_eq!(blended.bytes, &[127, 128, 0, 255]);

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

    apply(
        &mut images,
        b"\x1b_Ga=d,d=f,i=1,r=5\x1b\\\x1b_Ga=a,i=1,c=1,q=2\x1b\\",
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

    let rgb: Vec<_> = (0..257 * 64)
        .map(|index| [index as u8, (index >> 8) as u8, (index % 17) as u8])
        .collect();
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(rgb.as_flattened()).unwrap();
    let compressed = encoder.finish().unwrap();
    let expected: Vec<_> = rgb.iter().map(|&[r, g, b]| [r, g, b, 255]).collect();
    for (bytes, compression) in [(rgb.as_flattened(), ""), (&compressed, "o=z,")] {
        let encoded = PAYLOAD_ENGINE.encode(bytes);
        let mut wire = String::new();
        for (index, chunk) in encoded.as_bytes().chunks(52).enumerate() {
            let more = u8::from((index + 1) * 52 < encoded.len());
            wire.push_str("\x1b_G");
            if index == 0 {
                wire.push_str("i=1,f=24,s=257,v=64,");
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
    let mut images = ImageStore::new(8, 2);
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
    assert_eq!(replies, [ok, bad, bad, bad, bad, ok, bad, ok]);
    assert_eq!(
        images.get(1).unwrap().pixels(&mut Vec::new()).unwrap(),
        &[1, 2, 3, 4]
    );
    assert_eq!(
        images.get(2).unwrap().pixels(&mut Vec::new()).unwrap(),
        &[17, 34, 51, 128]
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
        local_reply(&mut images, "i=1,t=f,s=1,v=1,S=23,O=2", &b(&link)).message,
        b"OK"
    );
    assert_eq!(
        local_reply(&mut images, "a=q,i=1,t=f,s=1,v=1,S=3,O=2", &b(&link)).message,
        b"EBADF:Failed to read image file"
    );
    for path in [&fifo, &directory.0.join("missing"), &file] {
        let reply = local_reply(&mut images, "i=1,t=f,s=1,v=1,S=4,O=5", &b(path));
        assert_eq!(reply.message, b"EBADF:Failed to read image file");
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
    let controls = format!("i=2,t=t,f=100,o=z,O=2,S={}", encoded.len());
    assert_eq!(
        local_reply(&mut images, &controls, &b(&temporary)).message,
        b"OK"
    );
    assert_eq!(
        images.get(2).unwrap().pixels(&mut Vec::new()).unwrap(),
        &[255, 0, 0, 255]
    );
    assert!(!temporary.exists());
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
    let file = shm::open(
        name.as_str(),
        OFlags::CREATE | OFlags::EXCL | OFlags::RDWR,
        Mode::RUSR | Mode::WUSR,
    )
    .unwrap();
    struct Cleanup<'a>(&'a str);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = shm::unlink(self.0);
        }
    }
    let _cleanup = Cleanup(&name);
    File::from(file).write_all(&[0, 3, 17, 0]).unwrap();
    let mut images = ImageStore::new(4, 1);
    assert_eq!(
        local_reply(
            &mut images,
            "i=1,t=s,f=16,s=1,v=1,O=1,S=19",
            name.as_bytes()
        )
        .message,
        b"OK"
    );
    let image = images.get(1).unwrap();
    assert_eq!(image.format(), ImageFormat::IndexedAlpha);
    assert_eq!(image.pixels(&mut Vec::new()).unwrap(), &[3, 17]);
    assert_eq!(
        shm::open(name.as_str(), OFlags::RDONLY, Mode::empty()).unwrap_err(),
        rustix::io::Errno::NOENT
    );
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

    decoder.start();
    decoder.push(b"G;");
    decoder.push(&[b'A'; MAX_DIRECT_PAYLOAD_BYTES + 1]);
    assert_eq!(decoder.finish(), Some(Err(DecodeError::PayloadTooLong)));

    decoder.start();
    decoder.push(b"Ga=q,e=T,i=19;");
    decoder.push(&[b'%'; INLINE_ENCODED_BYTES + 1]);
    decoder.push(b"ignored");
    assert!(
        decoder
            .finish()
            .unwrap()
            .unwrap()
            .encoded_payload
            .is_empty()
    );

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
    assert_eq!(decoder.finish(), Some(Err(DecodeError::PayloadTooLong)));

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
