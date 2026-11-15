use lune_vt::{Event, OscEvent, OscTerminator, Parameter, ParseError, Parser};

#[derive(Debug, PartialEq, Eq)]
pub enum Token {
    Text(char),
    Control(u8),
    Escape(Vec<u8>, u8),
    Csi(Vec<u8>, Vec<Parameter>, Vec<u8>, u8),
    OscStart,
    OscByte(u8),
    OscEnd(OscTerminator),
    OscCancel,
    Error(ParseError),
}

pub fn capture(tokens: &mut Vec<Token>, budget: usize, event: Event<'_>) {
    match event {
        Event::Text(text) => tokens.extend(text.chars().map(Token::Text)),
        Event::Execute(byte) => tokens.push(Token::Control(byte)),
        Event::Escape(sequence) => tokens.push(Token::Escape(
            sequence.intermediates.to_vec(),
            sequence.final_byte,
        )),
        Event::Csi(sequence) => tokens.push(Token::Csi(
            sequence.private_markers.to_vec(),
            sequence.parameters.to_vec(),
            sequence.intermediates.to_vec(),
            sequence.final_byte,
        )),
        Event::Osc(OscEvent::Start) => tokens.push(Token::OscStart),
        Event::Osc(OscEvent::Data(bytes)) => {
            tokens.extend(bytes.iter().copied().map(Token::OscByte));
        }
        Event::Osc(OscEvent::End(terminator)) => tokens.push(Token::OscEnd(terminator)),
        Event::Osc(OscEvent::Cancel) => tokens.push(Token::OscCancel),
        Event::Error(error) => tokens.push(Token::Error(error)),
    }
    assert!(tokens.len() <= budget, "Parser exceeded the output budget");
}

pub fn parse(bytes: &[u8], finish: bool) -> Vec<Token> {
    let mut parser = Parser::default();
    let mut tokens = Vec::new();
    let budget = bytes.len().saturating_mul(2).saturating_add(2);
    parser.advance(bytes, |event| capture(&mut tokens, budget, event));
    if finish {
        parser.finish(|event| capture(&mut tokens, budget, event));
    }
    tokens
}

pub fn random(seed: &mut u64) -> usize {
    *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    (*seed >> 32) as usize
}
