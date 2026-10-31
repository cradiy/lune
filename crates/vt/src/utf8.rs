#[derive(Default)]
pub(crate) struct Decoder {
    bytes: [u8; 4],
    len: usize,
}

pub(crate) enum Decoded<'a> {
    Text(&'a str),
    Pending,
    Invalid { retry: bool },
}

impl Decoder {
    pub(crate) fn is_pending(&self) -> bool {
        self.len != 0
    }

    #[inline]
    pub(crate) fn push(&mut self, byte: u8) -> Decoded<'_> {
        self.bytes[self.len] = byte;
        self.len += 1;

        match std::str::from_utf8(&self.bytes[..self.len]) {
            Ok(text) => {
                self.len = 0;
                Decoded::Text(text)
            }
            Err(error) => match error.error_len() {
                None => Decoded::Pending,
                Some(invalid_len) => {
                    let retry = invalid_len < self.len;
                    self.len = 0;
                    Decoded::Invalid { retry }
                }
            },
        }
    }

    pub(crate) fn finish(&mut self) -> bool {
        let incomplete = self.is_pending();
        self.len = 0;
        incomplete
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8]) -> String {
        let mut decoder = Decoder::default();
        let mut output = String::new();
        for &byte in bytes {
            loop {
                match decoder.push(byte) {
                    Decoded::Text(text) => output.push_str(text),
                    Decoded::Pending => {}
                    Decoded::Invalid { retry } => {
                        output.push(char::REPLACEMENT_CHARACTER);
                        if retry {
                            continue;
                        }
                    }
                }
                break;
            }
        }
        if decoder.finish() {
            output.push(char::REPLACEMENT_CHARACTER);
        }
        output
    }

    #[test]
    fn malformed_utf8_matches_standard_lossy_decoding() {
        for bytes in [
            b"\xe0\xa0\x80".as_slice(),
            b"\xe0\x9f\xbf",
            b"\xed\x9f\xbf",
            b"\xed\xa0\x80",
            b"\xf0\x90\x80\x80",
            b"\xf0\x8f\xbf\xbf",
            b"\xf4\x8f\xbf\xbf",
            b"\xf4\x90\x80\x80",
        ] {
            assert_eq!(decode(bytes), String::from_utf8_lossy(bytes));
        }

        for pair in 0..=u16::MAX {
            let bytes = pair.to_be_bytes();
            assert_eq!(decode(&bytes), String::from_utf8_lossy(&bytes));
        }

        let mut seed = 1_u64;
        for _ in 0..4096 {
            let mut bytes = [0; 32];
            for byte in &mut bytes {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                *byte = (seed >> 32) as u8;
            }
            assert_eq!(decode(&bytes), String::from_utf8_lossy(&bytes));
        }
    }
}
