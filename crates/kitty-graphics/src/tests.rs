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
    let input = b"\x1b_Gi=1,s=1,v=1,q=1;AQIDBA==\x1b\\\x1b_Gi=2,s=1,v=1;AAAAAA==\x1b\\\x1b_Gi=1,s=1,v=1;AAAA\x1b\\\x1b_Ga=q,i=1,s=1,v=1;AAAAAA==\x1b\\";
    let mut images = DirectImages::new(4, 1);
    let mut replies = Vec::new();
    feed(input, 3, |command| {
        replies.extend(images.receive(command.unwrap()));
    });
    assert_eq!(
        replies
            .iter()
            .map(|reply| (reply.id, reply.message))
            .collect::<Vec<_>>(),
        [
            (2, b"ENOSPC:Image quota exceeded".as_slice()),
            (1, b"EINVAL:Invalid image data".as_slice()),
            (1, b"OK".as_slice()),
        ]
    );
    assert!(images.get(2).is_none());
    assert_eq!(images.get(1).unwrap().rgba(), &[1, 2, 3, 4]);
}

#[test]
fn payload_bounds_and_nonfinal_block_alignment_are_enforced() {
    let mut decoder = Decoder::default();
    decoder.start();
    decoder.push(b"Gm=1;");
    decoder.push(&[b'A'; MAX_DIRECT_PAYLOAD_BYTES]);
    let command = decoder.finish().unwrap().unwrap();
    let mut output = [0xff; MAX_DECODED_PAYLOAD_BYTES];
    assert_eq!(command.decode_payload(&mut output), Ok(3072));
    assert_eq!(&output[..3072], &[0; 3072]);

    decoder.start();
    decoder.push(b"G;");
    decoder.push(&[b'A'; MAX_DIRECT_PAYLOAD_BYTES + 1]);
    assert_eq!(decoder.finish(), Some(Err(DecodeError::PayloadTooLong)));

    decoder.start();
    decoder.push(b"Gt=f;");
    let path = [b'x'; MAX_PATH_BYTES - 1];
    decoder.push(PAYLOAD_ENGINE.encode(path).as_bytes());
    let command = decoder.finish().unwrap().unwrap();
    assert_eq!(command.decode_payload(&mut output), Ok(path.len()));
    assert_eq!(&output[..path.len()], &path);

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
