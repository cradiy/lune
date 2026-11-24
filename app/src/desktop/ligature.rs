use lune_terminal::Cell;

const MAX_CELLS: usize = 8;

pub(super) struct Ligature {
    bytes: [u8; MAX_CELLS],
    len: usize,
}

impl Ligature {
    pub(super) fn text(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.len]).expect("A ligature candidate contains ASCII")
    }
}

pub(super) fn can_start(character: char) -> bool {
    matches!(character, 'f' | 'w') || character.is_ascii() && is_operator(character as u8)
}

pub(super) fn prefix(cells: &[Cell], previous: Option<Cell>) -> Option<Ligature> {
    let first = ascii(*cells.first()?)?;
    let mut bytes = [0; MAX_CELLS];
    let len = if is_operator(first) {
        let mut length = 0;
        for (&cell, byte) in cells.iter().zip(&mut bytes) {
            let Some(source) = ascii(cell).filter(|&byte| is_operator(byte)) else {
                break;
            };
            *byte = source;
            length += 1;
        }
        if bytes[..length].iter().all(|&byte| byte == first)
            && (length > 4 || previous.and_then(ascii) == Some(first))
        {
            return None;
        }
        length
    } else {
        let mut count = 0;
        for (&cell, byte) in cells.iter().zip(&mut bytes[..3]) {
            let Some(source) = ascii(cell) else { break };
            *byte = source;
            count += 1;
        }
        [b"ffi".as_slice(), b"ffl", b"www", b"ff", b"fi", b"fl"]
            .into_iter()
            .find(|text| bytes[..count].starts_with(text))?
            .len()
    };
    (len > 1).then_some(Ligature { bytes, len })
}

fn ascii(cell: Cell) -> Option<u8> {
    if cell.is_erased() || cell.has_extended_text() || cell.width() != 1 {
        return None;
    }
    cell.character()
        .filter(char::is_ascii)
        .map(|character| character as u8)
}

fn is_operator(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'#'
            | b'$'
            | b'%'
            | b'&'
            | b'*'
            | b'+'
            | b'-'
            | b'.'
            | b'/'
            | b':'
            | b'<'
            | b'='
            | b'>'
            | b'?'
            | b'@'
            | b'\\'
            | b'^'
            | b'|'
            | b'~'
    )
}
