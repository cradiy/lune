pub const PLACEHOLDER: char = '\u{10eeee}';

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placeholder {
    pub image_id: u32,
    pub placement_id: u32,
    pub row: u16,
    pub column: u16,
}

impl Placeholder {
    pub fn decode(
        marks: impl Iterator<Item = char>,
        low_image_id: u32,
        placement_id: u32,
        previous: Option<Self>,
    ) -> Option<Self> {
        if low_image_id > 0xff_ffff {
            return None;
        }
        let mut marks = marks.map(diacritic_index);
        let mut next = || match marks.next() {
            Some(Some(value)) => Some(Some(value)),
            Some(None) => None,
            None => Some(None),
        };
        let row = next()?;
        let column = next()?;
        let high = next()?;
        let previous = previous.filter(|cell| {
            cell.image_id & 0xff_ffff == low_image_id && cell.placement_id == placement_id
        });
        let row = row.or_else(|| previous.map(|cell| cell.row)).unwrap_or(0);
        let left = previous.filter(|cell| cell.row == row);
        let column = column
            .or_else(|| left.and_then(|cell| cell.column.checked_add(1)))
            .unwrap_or(0);
        let high = high
            .map(u8::try_from)
            .transpose()
            .ok()?
            .or_else(|| {
                left.filter(|cell| cell.column.checked_add(1) == Some(column))
                    .map(|cell| (cell.image_id >> 24) as u8)
            })
            .unwrap_or(0);
        let image_id = (u32::from(high) << 24) | low_image_id;
        Some(Self {
            image_id,
            placement_id,
            row,
            column,
        })
    }
}

const BLOCK_SIZE: usize = 64;
const BLOCK_MAP_SIZE: usize = DIACRITICS[DIACRITICS.len() - 1] as usize / BLOCK_SIZE + 1;
const BLOCK_COUNT: usize = {
    let mut count = 2;
    let mut index = 1;
    while index < DIACRITICS.len() {
        if DIACRITICS[index] as usize / BLOCK_SIZE != DIACRITICS[index - 1] as usize / BLOCK_SIZE {
            count += 1;
        }
        index += 1;
    }
    count
};

struct DiacriticIndex {
    blocks: [u8; BLOCK_MAP_SIZE],
    values: [[u16; BLOCK_SIZE]; BLOCK_COUNT],
}

// Block zero represents all unassigned blocks. A stored value is the protocol index plus one.
static DIACRITIC_INDEX: DiacriticIndex = {
    let mut table = DiacriticIndex {
        blocks: [0; BLOCK_MAP_SIZE],
        values: [[0; BLOCK_SIZE]; BLOCK_COUNT],
    };
    let mut next_block = 1;
    let mut index = 0;
    while index < DIACRITICS.len() {
        let scalar = DIACRITICS[index] as usize;
        let block = &mut table.blocks[scalar / BLOCK_SIZE];
        if *block == 0 {
            *block = next_block;
            next_block += 1;
        }
        table.values[*block as usize][scalar % BLOCK_SIZE] = index as u16 + 1;
        index += 1;
    }
    table
};

#[inline]
fn diacritic_index(mark: char) -> Option<u16> {
    let scalar = mark as usize;
    let block = usize::from(*DIACRITIC_INDEX.blocks.get(scalar / BLOCK_SIZE)?);
    DIACRITIC_INDEX.values[block][scalar % BLOCK_SIZE].checked_sub(1)
}

// Index order is defined by Kitty's row/column diacritics table.
const DIACRITICS: [u32; 297] = [
    0x305, 0x30d, 0x30e, 0x310, 0x312, 0x33d, 0x33e, 0x33f, 0x346, 0x34a, 0x34b, 0x34c, 0x350,
    0x351, 0x352, 0x357, 0x35b, 0x363, 0x364, 0x365, 0x366, 0x367, 0x368, 0x369, 0x36a, 0x36b,
    0x36c, 0x36d, 0x36e, 0x36f, 0x483, 0x484, 0x485, 0x486, 0x487, 0x592, 0x593, 0x594, 0x595,
    0x597, 0x598, 0x599, 0x59c, 0x59d, 0x59e, 0x59f, 0x5a0, 0x5a1, 0x5a8, 0x5a9, 0x5ab, 0x5ac,
    0x5af, 0x5c4, 0x610, 0x611, 0x612, 0x613, 0x614, 0x615, 0x616, 0x617, 0x657, 0x658, 0x659,
    0x65a, 0x65b, 0x65d, 0x65e, 0x6d6, 0x6d7, 0x6d8, 0x6d9, 0x6da, 0x6db, 0x6dc, 0x6df, 0x6e0,
    0x6e1, 0x6e2, 0x6e4, 0x6e7, 0x6e8, 0x6eb, 0x6ec, 0x730, 0x732, 0x733, 0x735, 0x736, 0x73a,
    0x73d, 0x73f, 0x740, 0x741, 0x743, 0x745, 0x747, 0x749, 0x74a, 0x7eb, 0x7ec, 0x7ed, 0x7ee,
    0x7ef, 0x7f0, 0x7f1, 0x7f3, 0x816, 0x817, 0x818, 0x819, 0x81b, 0x81c, 0x81d, 0x81e, 0x81f,
    0x820, 0x821, 0x822, 0x823, 0x825, 0x826, 0x827, 0x829, 0x82a, 0x82b, 0x82c, 0x82d, 0x951,
    0x953, 0x954, 0xf82, 0xf83, 0xf86, 0xf87, 0x135d, 0x135e, 0x135f, 0x17dd, 0x193a, 0x1a17,
    0x1a75, 0x1a76, 0x1a77, 0x1a78, 0x1a79, 0x1a7a, 0x1a7b, 0x1a7c, 0x1b6b, 0x1b6d, 0x1b6e, 0x1b6f,
    0x1b70, 0x1b71, 0x1b72, 0x1b73, 0x1cd0, 0x1cd1, 0x1cd2, 0x1cda, 0x1cdb, 0x1ce0, 0x1dc0, 0x1dc1,
    0x1dc3, 0x1dc4, 0x1dc5, 0x1dc6, 0x1dc7, 0x1dc8, 0x1dc9, 0x1dcb, 0x1dcc, 0x1dd1, 0x1dd2, 0x1dd3,
    0x1dd4, 0x1dd5, 0x1dd6, 0x1dd7, 0x1dd8, 0x1dd9, 0x1dda, 0x1ddb, 0x1ddc, 0x1ddd, 0x1dde, 0x1ddf,
    0x1de0, 0x1de1, 0x1de2, 0x1de3, 0x1de4, 0x1de5, 0x1de6, 0x1dfe, 0x20d0, 0x20d1, 0x20d4, 0x20d5,
    0x20d6, 0x20d7, 0x20db, 0x20dc, 0x20e1, 0x20e7, 0x20e9, 0x20f0, 0x2cef, 0x2cf0, 0x2cf1, 0x2de0,
    0x2de1, 0x2de2, 0x2de3, 0x2de4, 0x2de5, 0x2de6, 0x2de7, 0x2de8, 0x2de9, 0x2dea, 0x2deb, 0x2dec,
    0x2ded, 0x2dee, 0x2def, 0x2df0, 0x2df1, 0x2df2, 0x2df3, 0x2df4, 0x2df5, 0x2df6, 0x2df7, 0x2df8,
    0x2df9, 0x2dfa, 0x2dfb, 0x2dfc, 0x2dfd, 0x2dfe, 0x2dff, 0xa66f, 0xa67c, 0xa67d, 0xa6f0, 0xa6f1,
    0xa8e0, 0xa8e1, 0xa8e2, 0xa8e3, 0xa8e4, 0xa8e5, 0xa8e6, 0xa8e7, 0xa8e8, 0xa8e9, 0xa8ea, 0xa8eb,
    0xa8ec, 0xa8ed, 0xa8ee, 0xa8ef, 0xa8f0, 0xa8f1, 0xaab0, 0xaab2, 0xaab3, 0xaab7, 0xaab8, 0xaabe,
    0xaabf, 0xaac1, 0xfe20, 0xfe21, 0xfe22, 0xfe23, 0xfe24, 0xfe25, 0xfe26, 0x10a0f, 0x10a38,
    0x1d185, 0x1d186, 0x1d187, 0x1d188, 0x1d189, 0x1d1aa, 0x1d1ab, 0x1d1ac, 0x1d1ad, 0x1d242,
    0x1d243, 0x1d244,
];

#[cfg(test)]
mod tests {
    use super::{BLOCK_MAP_SIZE, BLOCK_SIZE, DIACRITICS, diacritic_index};

    #[test]
    fn diacritic_index_preserves_protocol_order_and_rejects_unassigned_scalars() {
        for scalar in (0..=(BLOCK_MAP_SIZE * BLOCK_SIZE) as u32).chain([char::MAX as u32]) {
            if let Some(mark) = char::from_u32(scalar) {
                let expected = DIACRITICS
                    .binary_search(&scalar)
                    .ok()
                    .map(|index| index as u16);
                assert_eq!(diacritic_index(mark), expected, "U+{scalar:04X}");
            }
        }
    }
}
