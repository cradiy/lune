use crate::{Command, Controls, DecodeError, Value, control_keys};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Delete {
    pub target: DeleteTarget,
    pub free_data: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteTarget {
    Visible,
    Image {
        id: u32,
        placement_id: u32,
    },
    Number {
        number: u32,
        placement_id: u32,
    },
    Frame {
        id: u32,
        image_number: u32,
        number: u32,
    },
    IdRange {
        first: u32,
        last: u32,
    },
    Cursor,
    Point {
        column: u32,
        row: u32,
        z: Option<i32>,
    },
    Column(u32),
    Row(u32),
    Layer(i32),
}

impl DeleteTarget {
    pub fn selects_image(self, id: u32) -> bool {
        if id == 0 {
            return false;
        }
        match self {
            Self::Image {
                id: target,
                placement_id: 0,
            } => id == target,
            Self::IdRange { first, last } => (first..=last).contains(&id),
            _ => false,
        }
    }
}

impl Command<'_> {
    pub fn decode_delete(&self) -> Option<Result<Delete, DecodeError>> {
        if self.controls.get(b'a') != Some(Value::Letter(b'd')) {
            return None;
        }
        let result = self
            .encoded_payload
            .is_empty()
            .then(|| parse(&self.controls))
            .flatten();
        Some(result.ok_or(DecodeError::InvalidControl))
    }
}

fn parse(controls: &Controls) -> Option<Delete> {
    let mode = match controls.get(b'd') {
        None => b'a',
        Some(Value::Letter(mode)) => mode,
        _ => return None,
    };
    let unsigned = |key| match controls.get(key) {
        Some(Value::Unsigned(value)) => Some(value),
        _ => None,
    };
    let coordinate = |key| unsigned(key)?.checked_sub(1);
    let placement_id = || match controls.get(b'p') {
        None => Some(0),
        _ => unsigned(b'p'),
    };
    let z = || match controls.get(b'z') {
        None => Some(0),
        Some(Value::Signed(z)) => Some(z),
        Some(Value::Unsigned(z)) => i32::try_from(z).ok(),
        _ => None,
    };
    let (target, keys): (DeleteTarget, &[u8]) = match mode.to_ascii_lowercase() {
        b'a' => (DeleteTarget::Visible, b""),
        b'i' => (
            DeleteTarget::Image {
                id: unsigned(b'i').filter(|&id| id != 0)?,
                placement_id: placement_id()?,
            },
            b"ip",
        ),
        b'n' => (
            DeleteTarget::Number {
                number: unsigned(b'I').filter(|&number| number != 0)?,
                placement_id: placement_id()?,
            },
            b"Ip",
        ),
        b'r' => {
            let first = unsigned(b'x')?;
            let last = unsigned(b'y')?;
            if first > last {
                return None;
            }
            (DeleteTarget::IdRange { first, last }, b"xy")
        }
        b'c' => (DeleteTarget::Cursor, b""),
        b'f' => {
            let optional = |key| match controls.get(key) {
                None => Some(0),
                _ => unsigned(key),
            };
            let id = optional(b'i')?;
            let image_number = optional(b'I')?;
            if (id == 0) == (image_number == 0) {
                return None;
            }
            (
                DeleteTarget::Frame {
                    id,
                    image_number,
                    number: optional(b'r')?,
                },
                b"iIr",
            )
        }
        b'p' => (
            DeleteTarget::Point {
                column: coordinate(b'x')?,
                row: coordinate(b'y')?,
                z: None,
            },
            b"xy",
        ),
        b'q' => (
            DeleteTarget::Point {
                column: coordinate(b'x')?,
                row: coordinate(b'y')?,
                z: Some(z()?),
            },
            b"xyz",
        ),
        b'x' => (DeleteTarget::Column(coordinate(b'x')?), b"x"),
        b'y' => (DeleteTarget::Row(coordinate(b'y')?), b"y"),
        b'z' => (DeleteTarget::Layer(z()?), b"z"),
        _ => return None,
    };
    controls
        .only(const { control_keys(b"adq") } | control_keys(keys))
        .then_some(Delete {
            target,
            free_data: mode.is_ascii_uppercase(),
        })
}
