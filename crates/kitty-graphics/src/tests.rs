use lune_vt::{ApcEvent, Event, Parser};

use super::*;

fn feed(input: &[u8], chunk: usize, mut command: impl FnMut(Result<Command<'_>, DecodeError>)) {
    let mut parser = Parser::default();
    let mut decoder = Decoder::default();
    let mut receive = |event: Event<'_>| match event {
        Event::Apc(ApcEvent::Start) => decoder.start(),
        Event::Apc(ApcEvent::Data(bytes)) => decoder.push(bytes),
        Event::Apc(ApcEvent::Cancel) => decoder.cancel(),
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
    let input = b"\x1b_Gi=1,i=2\x1b\\\x1b_Gi=4294967296\x1b\\\x1b_Gz=-2147483649\x1b\\\x1b_Gq=1\x07\x1b\\\x1b_Ga=d,d=a\x1b\\";
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
    let input = b"\x1b_Gi=1,s=1,v=1,q=1;AQIDBA==\x1b\\\x1b_Gi=2,s=2,v=1;AAAAAA==\x1b\\\x1b_Gi=1,s=1,v=1;AAAA\x1b\\\x1b_Ga=q,i=1,s=1,v=1;AAAAAA==\x1b\\";
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
    assert_eq!((reply.id, reply.number, reply.frame_number), (1, 17, 2));
    assert_eq!(images.visual_revision(), before);

    let mut scratch = Vec::new();
    let original_revision = images.get(1).unwrap().revision();
    assert!(receive(&mut images, b"\x1b_Ga=a,i=1,c=2\x1b\\").is_none());
    let selected = images.visual_revision();
    assert_ne!(selected, before);
    let image = images.get(1).unwrap();
    let update = image.pixel_update(original_revision, &mut scratch).unwrap();
    assert_eq!(
        (update.origin, update.size, update.bytes_per_row),
        ([1, 0], [1, 1], 4)
    );
    assert_eq!(update.bytes, &[0, 128, 127, 255]);
    let selected_revision = image.revision();
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
    assert_eq!(images.visual_revision(), selected);
    assert_eq!(
        images.get(1).unwrap().pixels(&mut scratch).unwrap(),
        &[255, 0, 0, 255, 0, 128, 127, 255],
    );
    let reply = receive(
        &mut images,
        b"\x1b_Ga=c,i=1,r=2,c=2,X=1,x=1,w=1,h=1,C=1\x1b\\",
    )
    .unwrap();
    assert_eq!(reply.message, b"EINVAL:Invalid animation frame region");
    assert_eq!(images.visual_revision(), selected);
    let reply = receive(&mut images, b"\x1b_Ga=c,i=1,r=2,c=2,X=1,w=1,h=1,C=1\x1b\\").unwrap();
    assert_eq!(reply.message, b"OK");
    assert_ne!(images.visual_revision(), selected);
    assert_eq!(
        images.get(1).unwrap().pixels(&mut scratch).unwrap(),
        &[0, 128, 127, 255, 0, 128, 127, 255],
    );
    assert!(receive(&mut images, b"\x1b_Ga=a,i=1,c=1\x1b\\").is_none());
    let update = images
        .get(1)
        .unwrap()
        .pixel_update(selected_revision, &mut scratch)
        .unwrap();
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
fn zlib_checks_stream_boundaries_and_png_expands_paletted_alpha() {
    let mut images = ImageStore::new(8, 2);
    let wire = concat!(
        "\x1b_Gi=1,s=1,v=1,o=z,m=1;eJxj\x1b\\\x1b_Gm=0;ZGJmAQAAGAAL\x1b\\",
        "\x1b_Gi=1,s=1,v=1,o=z;eJxjZGJmAQAAGAALWg==\x1b\\",
        "\x1b_Gi=1,s=1,v=1,o=z;eJxjZGJmAQAAGAA=\x1b\\",
        "\x1b_Gi=1,s=1,v=1,o=z;eJxjZGJmYQUAACgAEA==\x1b\\",
        "\x1b_Gi=2,f=100,o=z,S=99;eJzrDPBz5+WS4mJgYOD19HAJYmBgYARhRmYGBgbV22GnGBgY2AJ8QlwZGBgElYwXPAzfx8DAwFQS5Bf8v4GDf3MWAwMDl6eLY0jFnOQMBgaGJobG8rNF2xgYGBg8Xf1c1jklNAEAsUoWww==\x1b\\"
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
    assert_eq!(replies, [b"OK".as_slice(), bad, bad, bad, b"OK".as_slice()]);
    assert_eq!(
        images.get(1).unwrap().pixels(&mut Vec::new()).unwrap(),
        &[1, 2, 3, 4]
    );
    assert_eq!(
        images.get(2).unwrap().pixels(&mut Vec::new()).unwrap(),
        &[17, 34, 51, 128]
    );
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
