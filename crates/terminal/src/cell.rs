/// Maximum UTF-8 bytes stored in one display unit.
pub const MAX_CELL_TEXT_BYTES: usize = 1024;

const SCALAR_MASK: u32 = (1 << 21) - 1;
const DOUBLE: u32 = 1 << 21;
const CONTINUATION: u32 = 1 << 22;
const EXTENDED: u32 = 1 << 23;

/// An opaque style key supplied and interpreted by the caller. Zero is the default key.
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    text: u32,
    style: StyleId,
}

impl Cell {
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
        Self::scalar(' ', CellWidth::Single, style)
    }

    pub(crate) fn continuation(style: StyleId) -> Self {
        Self {
            text: CONTINUATION,
            style,
        }
    }

    pub(crate) fn set_extended(&mut self) {
        self.text |= EXTENDED;
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
