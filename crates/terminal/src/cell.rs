/// Maximum UTF-8 bytes stored in one display unit.
pub const MAX_CELL_TEXT_BYTES: usize = 1024;

const SCALAR_MASK: u32 = (1 << 21) - 1;
const DOUBLE: u32 = 1 << 21;
const CONTINUATION: u32 = 1 << 22;
const EXTENDED: u32 = 1 << 23;
// The last cell carries the outgoing connection. An early wrap also marks the
// preceding cell, so replacing either edge cell invalidates the connection.
const ROW_WRAP: u32 = 1 << 24;
const ROW_GAP: u32 = 1 << 25;
const ROW_MASK: u32 = ROW_WRAP | ROW_GAP;
const ERASED: u32 = 1 << 26;

/// An opaque style key supplied and interpreted by the caller. Zero is the default
/// key. Keys issued by [`crate::StyleTable`] belong to that table's lifetime.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct StyleId(pub u32);

/// Explicit display width supplied by the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellWidth {
    Single,
    Double,
}

impl CellWidth {
    pub(crate) fn columns(self) -> usize {
        match self {
            Self::Single => 1,
            Self::Double => 2,
        }
    }
}

/// An eight-byte cell header. Its row owns any extended text.
#[derive(Clone, Copy, Debug)]
pub struct Cell {
    text: u32,
    style: StyleId,
}

const _: () = assert!(size_of::<Cell>() == 8);

// Row metadata shares spare header bits without changing cell value semantics.
impl PartialEq for Cell {
    fn eq(&self, other: &Self) -> bool {
        self.text & !ROW_MASK == other.text & !ROW_MASK && self.style == other.style
    }
}

impl Eq for Cell {}

impl Cell {
    pub(crate) fn without_row_metadata(self) -> Self {
        Self {
            text: self.text & !ROW_MASK,
            ..self
        }
    }

    pub(crate) fn row_wrap(self) -> Option<bool> {
        (self.text & ROW_WRAP != 0).then_some(self.text & ROW_GAP != 0)
    }

    pub(crate) fn is_row_gap(self) -> bool {
        self.text & ROW_GAP != 0
    }

    pub(crate) fn set_row_gap(&mut self, gap: bool) {
        self.text = (self.text & !ROW_GAP) | if gap { ROW_GAP } else { 0 };
    }

    pub(crate) fn set_row_wrap(&mut self, gap: Option<bool>) {
        self.text = (self.text & !ROW_MASK)
            | match gap {
                None => 0,
                Some(false) => ROW_WRAP,
                Some(true) => ROW_WRAP | ROW_GAP,
            };
    }

    pub(crate) fn scalar(character: char, width: CellWidth, style: StyleId) -> Self {
        Self {
            text: character as u32
                | match width {
                    CellWidth::Single => 0,
                    CellWidth::Double => DOUBLE,
                },
            style,
        }
    }

    pub(crate) fn blank(style: StyleId) -> Self {
        Self {
            text: ' ' as u32 | ERASED,
            style,
        }
    }

    pub(crate) fn continuation(style: StyleId) -> Self {
        Self {
            text: CONTINUATION,
            style,
        }
    }

    pub(crate) fn set_extended(&mut self) {
        self.text = (self.text & !ERASED) | EXTENDED;
    }

    pub(crate) fn is_inline_with_width(self, width: CellWidth) -> bool {
        let occupancy = match width {
            CellWidth::Single => 0,
            CellWidth::Double => DOUBLE,
        };
        self.text & (DOUBLE | CONTINUATION | EXTENDED) == occupancy
    }

    /// Returns the leading scalar of a display unit, or `None` for a continuation.
    pub fn character(self) -> Option<char> {
        if self.is_continuation() {
            None
        } else {
            char::from_u32(self.text & SCALAR_MASK)
        }
    }

    /// Returns one or two for a leading cell, and zero for a continuation.
    pub fn width(self) -> usize {
        if self.is_continuation() {
            0
        } else if self.text & DOUBLE != 0 {
            2
        } else {
            1
        }
    }

    pub fn is_continuation(self) -> bool {
        self.text & CONTINUATION != 0
    }

    pub fn has_extended_text(self) -> bool {
        self.text & EXTENDED != 0
    }

    /// Whether initialization, erasure, or blank filling created this cell.
    /// Printed spaces are text and return false. Erased cells have single width
    /// and expose a space through [`Self::character`] and text extraction.
    pub fn is_erased(self) -> bool {
        self.text & ERASED != 0
    }

    pub fn style(self) -> StyleId {
        self.style
    }
}

/// Text borrowed from a row, or a scalar stored in its cell header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellText<'a> {
    Scalar(char),
    Extended(&'a str),
    Continuation,
}
