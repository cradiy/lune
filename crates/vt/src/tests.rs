use crate::{
    Event, MAX_PARAMETERS, MAX_SEQUENCE_BYTES, OscEvent, OscTerminator, Parameter, ParseError,
    Parser,
};

#[derive(Debug, PartialEq, Eq)]
enum OwnedEvent {
    Print(char),
    Execute(u8),
    Escape(Vec<u8>, u8),
    Csi(Vec<u8>, Vec<Parameter>, Vec<u8>, u8),
    OscStart,
    OscData(Vec<u8>),
    OscEnd(OscTerminator),
    OscCancel,
    Error(ParseError),
}

fn capture(events: &mut Vec<OwnedEvent>, event: Event<'_>) {
    let owned = match event {
        Event::Text(text) => {
            events.extend(text.chars().map(OwnedEvent::Print));
            return;
        }
        Event::Execute(byte) => OwnedEvent::Execute(byte),
        Event::Escape(sequence) => {
            OwnedEvent::Escape(sequence.intermediates.to_vec(), sequence.final_byte)
        }
        Event::Csi(sequence) => OwnedEvent::Csi(
            sequence.private_markers.to_vec(),
            sequence.parameters.to_vec(),
            sequence.intermediates.to_vec(),
            sequence.final_byte,
        ),
        Event::Osc(OscEvent::Start) => OwnedEvent::OscStart,
        Event::Osc(OscEvent::Data(bytes)) => {
            if let Some(OwnedEvent::OscData(data)) = events.last_mut() {
                data.extend_from_slice(bytes);
                return;
            }
            OwnedEvent::OscData(bytes.to_vec())
        }
        Event::Osc(OscEvent::End(terminator)) => OwnedEvent::OscEnd(terminator),
        Event::Osc(OscEvent::Cancel) => OwnedEvent::OscCancel,
        Event::Error(error) => OwnedEvent::Error(error),
    };
    events.push(owned);
}

fn parse<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> Vec<OwnedEvent> {
    let mut parser = Parser::default();
    let mut events = Vec::new();
    for chunk in chunks {
        parser.advance(chunk, |event| capture(&mut events, event));
    }
    parser.finish(|event| capture(&mut events, event));
    events
}

fn csi(value: u16, final_byte: u8) -> OwnedEvent {
    OwnedEvent::Csi(
        Vec::new(),
        vec![Parameter {
            value: Some(value),
            is_subparameter: false,
        }],
        Vec::new(),
        final_byte,
    )
}

#[test]
fn chunk_boundaries_preserve_text_and_parameter_structure() {
    let input = b"A\xe6\xb0\xb4\xf0\x9f\x8c\x99\n\x1b(0B\x1b[?38:2::12;0$px\x1b]0;hidden\x07Y\x1bPignored\x1b\\Z";
    let expected = vec![
        OwnedEvent::Print('A'),
        OwnedEvent::Print('水'),
        OwnedEvent::Print('🌙'),
        OwnedEvent::Execute(b'\n'),
        OwnedEvent::Escape(vec![b'('], b'0'),
        OwnedEvent::Print('B'),
        OwnedEvent::Csi(
            vec![b'?'],
            vec![
                Parameter {
                    value: Some(38),
                    is_subparameter: false,
                },
                Parameter {
                    value: Some(2),
                    is_subparameter: true,
                },
                Parameter {
                    value: None,
                    is_subparameter: true,
                },
                Parameter {
                    value: Some(12),
                    is_subparameter: true,
                },
                Parameter {
                    value: Some(0),
                    is_subparameter: false,
                },
            ],
            vec![b'$'],
            b'p',
        ),
        OwnedEvent::Print('x'),
        OwnedEvent::OscStart,
        OwnedEvent::OscData(b"0;hidden".to_vec()),
        OwnedEvent::OscEnd(OscTerminator::Bell),
        OwnedEvent::Print('Y'),
        OwnedEvent::Print('Z'),
    ];

    assert_eq!(parse([input.as_slice()]), expected);
    for split in 0..=input.len() {
        assert_eq!(parse([&input[..split], &input[split..]]), expected);
    }
    for chunk_size in 1..=input.len() {
        assert_eq!(parse(input.chunks(chunk_size)), expected);
    }
}

#[test]
fn bulk_dispatch_matches_bytewise_dispatch_for_arbitrary_streams() {
    let mut seed = 1_u64;
    for _ in 0..512 {
        let mut input = [0; 256];
        for byte in &mut input {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            *byte = (seed >> 32) as u8;
        }
        let expected = parse(input.chunks(1));
        assert_eq!(parse([input.as_slice()]), expected);
        for chunk_size in [2, 3, 7, 17, 31] {
            assert_eq!(parse(input.chunks(chunk_size)), expected);
        }
    }
}

#[test]
fn rejected_sequences_emit_one_error_and_resume_after_the_final_byte() {
    let mut too_many_parameters = b"\x1b[".to_vec();
    too_many_parameters.extend(std::iter::repeat_n(b';', MAX_PARAMETERS));
    too_many_parameters.extend_from_slice(b"mZ");

    let mut too_long = b"\x1b[".to_vec();
    too_long.extend(std::iter::repeat_n(b'0', MAX_SEQUENCE_BYTES));
    too_long.extend_from_slice(b"mZ");

    let cases = [
        (b"\x1b[65536mZ".to_vec(), ParseError::ParameterOverflow),
        (too_many_parameters, ParseError::TooManyParameters),
        (too_long, ParseError::SequenceTooLong),
        (b"\x1b[?????hZ".to_vec(), ParseError::TooManyPrivateMarkers),
        (b"\x1b[     mZ".to_vec(), ParseError::TooManyIntermediates),
        (b"\x1b     1Z".to_vec(), ParseError::TooManyIntermediates),
        (b"\x1b[1?mZ".to_vec(), ParseError::InvalidSequence),
        (b"\x1b[1 2mZ".to_vec(), ParseError::InvalidSequence),
    ];

    for (input, error) in cases {
        let expected = vec![OwnedEvent::Error(error), OwnedEvent::Print('Z')];
        assert_eq!(parse([input.as_slice()]), expected, "{input:?}");
        assert_eq!(parse(input.chunks(1)), expected, "{input:?}");
    }

    assert_eq!(
        parse([b"\x1b[65535m".as_slice()]),
        vec![csi(u16::MAX, b'm')]
    );
    let mut full_parameters = b"\x1b[".to_vec();
    full_parameters.extend(std::iter::repeat_n(b';', MAX_PARAMETERS - 1));
    full_parameters.push(b'm');
    assert_eq!(
        parse([full_parameters.as_slice()]),
        vec![OwnedEvent::Csi(
            Vec::new(),
            vec![Parameter::default(); MAX_PARAMETERS],
            Vec::new(),
            b'm',
        )]
    );
}

#[test]
fn ignored_headers_preserve_controls_final_bytes_and_escape_restarts() {
    for (header, error, first_final) in [
        (
            b"\x1b[65536".as_slice(),
            ParseError::ParameterOverflow,
            0x40,
        ),
        (
            b"\x1b     ".as_slice(),
            ParseError::TooManyIntermediates,
            0x30,
        ),
    ] {
        let payload = (0x20..first_final).collect::<Vec<u8>>().repeat(4);
        let mut wire = Vec::new();
        for ending in [b"~Z".as_slice(), b"\x18A", b"\x1aB", b"\x1b[31mC"] {
            wire.extend_from_slice(header);
            wire.extend_from_slice(&payload);
            wire.extend_from_slice(b"\x08\x7f\x80\xff");
            wire.extend_from_slice(&payload);
            wire.extend_from_slice(ending);
        }
        let expected = vec![
            OwnedEvent::Error(error),
            OwnedEvent::Execute(0x08),
            OwnedEvent::Print('Z'),
            OwnedEvent::Error(error),
            OwnedEvent::Execute(0x08),
            OwnedEvent::Execute(0x18),
            OwnedEvent::Print('A'),
            OwnedEvent::Error(error),
            OwnedEvent::Execute(0x08),
            OwnedEvent::Execute(0x1a),
            OwnedEvent::Print('B'),
            OwnedEvent::Error(error),
            OwnedEvent::Execute(0x08),
            csi(31, b'm'),
            OwnedEvent::Print('C'),
        ];
        for chunk in [1, 2, 17, wire.len()] {
            assert_eq!(parse(wire.chunks(chunk)), expected);
        }
    }
}

#[test]
fn cancellation_and_escape_restart_preserve_control_order() {
    let input = b"\x1b[12\x08;\x18A\x1b[3\x1b[2mB\x1b]hidden\x1aC\x1b_Pignored\x1b[1mD\x7f\x1b_X\x07Y\x1b\\E";
    let expected = vec![
        OwnedEvent::Execute(0x08),
        OwnedEvent::Execute(0x18),
        OwnedEvent::Print('A'),
        csi(2, b'm'),
        OwnedEvent::Print('B'),
        OwnedEvent::OscStart,
        OwnedEvent::OscData(b"hidden".to_vec()),
        OwnedEvent::OscCancel,
        OwnedEvent::Execute(0x1a),
        OwnedEvent::Print('C'),
        csi(1, b'm'),
        OwnedEvent::Print('D'),
        OwnedEvent::Print('E'),
    ];
    assert_eq!(parse(input.chunks(1)), expected);

    let mut string = b"\x1bP".to_vec();
    string.extend(std::iter::repeat_n(b'x', 65536));
    string.extend_from_slice(b"\x1b\\Z");
    assert_eq!(parse(string.chunks(17)), vec![OwnedEvent::Print('Z')]);
}

#[test]
fn broken_utf8_preserves_following_controls() {
    let input = b"\xe2\x82\x1b[31mZ\xc2\n\xc2\x9b31m";
    let expected = vec![
        OwnedEvent::Print(char::REPLACEMENT_CHARACTER),
        csi(31, b'm'),
        OwnedEvent::Print('Z'),
        OwnedEvent::Print(char::REPLACEMENT_CHARACTER),
        OwnedEvent::Execute(b'\n'),
        OwnedEvent::Print('\u{9b}'),
        OwnedEvent::Print('3'),
        OwnedEvent::Print('1'),
        OwnedEvent::Print('m'),
    ];
    for split in 0..=input.len() {
        assert_eq!(parse([&input[..split], &input[split..]]), expected);
    }
}

#[test]
fn stream_boundaries_finalize_or_discard_pending_input_explicitly() {
    let mut parser = Parser::default();
    let mut events = Vec::new();
    let mut emit = |event: Event<'_>| capture(&mut events, event);

    parser.advance(b"\xe2\x82", &mut emit);
    assert!(!parser.is_ground());
    parser.finish(&mut emit);
    assert!(parser.is_ground());
    parser.finish(&mut emit);
    parser.advance(b"\xf0\x9f", &mut emit);
    assert!(!parser.is_ground());
    parser.advance(b"\x8c", &mut emit);
    assert!(!parser.is_ground());
    parser.advance(b"\x99", &mut emit);
    assert!(parser.is_ground());
    parser.advance(b"\x1b[12", &mut emit);
    parser.finish(&mut emit);
    parser.advance(b"\x1b[65536", &mut emit);
    parser.finish(&mut emit);
    parser.advance(b"\x1b[7", &mut emit);
    parser.reset();
    parser.advance(b"\xf0\x9f", &mut emit);
    parser.reset();
    parser.advance(b"Z", &mut emit);
    parser.finish(&mut emit);

    assert_eq!(
        events,
        vec![
            OwnedEvent::Print(char::REPLACEMENT_CHARACTER),
            OwnedEvent::Print('🌙'),
            OwnedEvent::Error(ParseError::IncompleteSequence),
            OwnedEvent::Error(ParseError::ParameterOverflow),
            OwnedEvent::Print('Z'),
        ]
    );

    for prefix in [
        b"\x1b".as_slice(),
        b"\x1b(",
        b"\x1b[",
        b"\x1b[1$",
        b"\x1b]title",
        b"\x1bPpayload",
        b"\x1bXpayload",
        b"\x1b^payload",
        b"\x1b_payload",
        b"\x1b]title\x1b",
    ] {
        let mut parser = Parser::default();
        let mut events = Vec::new();
        parser.advance(prefix, |_| {});
        parser.finish(|event| capture(&mut events, event));
        let mut expected = Vec::new();
        if prefix.starts_with(b"\x1b]") {
            expected.push(OwnedEvent::OscCancel);
        }
        expected.push(OwnedEvent::Error(ParseError::IncompleteSequence));
        assert_eq!(events, expected, "{prefix:?}");
        parser.finish(|_| panic!("Repeated finish emitted an event"));
        events.clear();
        parser.advance(b"Z", |event| capture(&mut events, event));
        assert_eq!(events, [OwnedEvent::Print('Z')]);

        parser.advance(prefix, |_| {});
        parser.reset();
        parser.reset();
        events.clear();
        parser.advance(b"\x1b[2mZ", |event| capture(&mut events, event));
        assert_eq!(events, [csi(2, b'm'), OwnedEvent::Print('Z')]);
    }
}

#[test]
fn osc_framing_preserves_raw_payload_and_cancellation_across_chunks() {
    let input = b"\x1b]\x07\x1b]2;\xf0\x9f\x8c\x99\xff\0\n\x7f\x1b\\\x1b]8;;link\x18\x1b]cut\x1b[2mZ\x1b]tail\x1b";
    let expected = vec![
        OwnedEvent::OscStart,
        OwnedEvent::OscEnd(OscTerminator::Bell),
        OwnedEvent::OscStart,
        OwnedEvent::OscData(b"2;\xf0\x9f\x8c\x99\xff".to_vec()),
        OwnedEvent::OscEnd(OscTerminator::St),
        OwnedEvent::OscStart,
        OwnedEvent::OscData(b"8;;link".to_vec()),
        OwnedEvent::OscCancel,
        OwnedEvent::Execute(0x18),
        OwnedEvent::OscStart,
        OwnedEvent::OscData(b"cut".to_vec()),
        OwnedEvent::OscCancel,
        csi(2, b'm'),
        OwnedEvent::Print('Z'),
        OwnedEvent::OscStart,
        OwnedEvent::OscData(b"tail".to_vec()),
        OwnedEvent::OscCancel,
        OwnedEvent::Error(ParseError::IncompleteSequence),
    ];
    for split in 0..=input.len() {
        assert_eq!(parse([&input[..split], &input[split..]]), expected);
    }
    for chunk_size in 1..=input.len() {
        assert_eq!(parse(input.chunks(chunk_size)), expected);
    }
}

#[test]
fn osc_payload_is_borrowed_without_a_length_limit_and_can_be_discarded() {
    let mut parser = Parser::default();
    parser.advance(b"\x1b]", |event| {
        assert_eq!(event, Event::Osc(OscEvent::Start))
    });
    let payload = [b'x'; 65536];
    let mut callbacks = 0;
    parser.advance(&payload, |event| {
        let Event::Osc(OscEvent::Data(bytes)) = event else {
            panic!("Unexpected payload event: {event:?}");
        };
        assert_eq!(bytes.as_ptr(), payload.as_ptr());
        assert_eq!(bytes.len(), payload.len());
        callbacks += 1;
    });
    assert_eq!(callbacks, 1);
    assert!(!parser.is_ground());
    assert_eq!(parser.discard_string_payload(b"discard\x07Z"), 7);
    let mut events = Vec::new();
    parser.advance(b"\x07Z", |event| capture(&mut events, event));
    assert_eq!(
        events,
        [
            OwnedEvent::OscEnd(OscTerminator::Bell),
            OwnedEvent::Print('Z')
        ]
    );
    assert!(parser.is_ground());
    assert_eq!(parser.discard_string_payload(b"text"), 0);
}
