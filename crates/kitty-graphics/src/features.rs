use crate::animation::{COMPOSE_FIELDS, CONTROL_FIELDS, LOAD_FIELDS};
use crate::{Controls, Placement, Value};

const TRANSMISSION: &[u8] = b"ftsvSOoN";
const DELETE: &[u8] = b"dpxyzr";
type Fields = (&'static [u8], fn(u8, Value) -> bool);

// APC, i, e, the largest control list with separators, and the OK response.
pub const MAX_FEATURE_REPLY_BYTES: usize =
    3 + 12 + 4 + (TRANSMISSION.len() + Placement::CONTROL_NAMES.len()) * 14 + 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeatureReply {
    action: u8,
    controls: Vec<(u8, Value)>,
}

impl FeatureReply {
    pub fn action(&self) -> u8 {
        self.action
    }

    pub fn controls(&self) -> &[(u8, Value)] {
        &self.controls
    }

    pub(crate) fn resolve(controls: &Controls) -> Option<Self> {
        let Value::Letter(action) = controls.get(b'e')? else {
            return None;
        };
        let fields: &[Fields] = match action {
            b't' | b'q' => &[(TRANSMISSION, transmission)],
            b'T' => &[
                (TRANSMISSION, transmission),
                (Placement::CONTROL_NAMES, placement),
            ],
            b'p' => &[(Placement::CONTROL_NAMES, placement)],
            b'd' => &[(DELETE, deletion)],
            b'f' => &[
                (TRANSMISSION, frame_transmission),
                (LOAD_FIELDS, frame_load),
            ],
            b'a' => &[(CONTROL_FIELDS, frame_control)],
            b'c' => &[(COMPOSE_FIELDS, frame_composition)],
            _ => return None,
        };
        let mut supported = Vec::new();
        for &(keys, accepts) in fields {
            supported.extend(keys.iter().copied().filter_map(|key| {
                let value = controls.get(key)?;
                accepts(key, value).then_some((key, value))
            }));
        }
        Some(Self {
            action,
            controls: supported,
        })
    }
}

fn transmission(key: u8, value: Value) -> bool {
    match (key, value) {
        (b'f', Value::Unsigned(8 | 16 | 24 | 100)) | (b'o', Value::Letter(b'z')) => true,
        (b'N', Value::Unsigned(1)) => true,
        (b't', Value::Letter(b'f' | b't' | b's')) => cfg!(target_os = "linux"),
        (b's' | b'v' | b'S' | b'O', Value::Unsigned(value)) => value != 0,
        _ => false,
    }
}

fn frame_transmission(key: u8, value: Value) -> bool {
    !matches!((key, value), (b'N', _) | (b'f', Value::Unsigned(8 | 16))) && transmission(key, value)
}

fn frame_load(key: u8, value: Value) -> bool {
    match key {
        b'X' => value == Value::Unsigned(1),
        _ => frame_control(key, value),
    }
}

fn frame_control(key: u8, value: Value) -> bool {
    match key {
        b'z' => nonzero_signed(value),
        b's' => matches!(value, Value::Unsigned(1..=3)),
        _ => matches!(value, Value::Unsigned(1..)),
    }
}

fn frame_composition(key: u8, value: Value) -> bool {
    match key {
        b'C' => value == Value::Unsigned(1),
        _ => matches!(value, Value::Unsigned(1..)),
    }
}

fn placement(key: u8, value: Value) -> bool {
    match key {
        b'C' | b'U' => value == Value::Unsigned(1),
        b'z' | b'H' | b'V' => nonzero_signed(value),
        _ => matches!(value, Value::Unsigned(1..)),
    }
}

fn deletion(key: u8, value: Value) -> bool {
    match key {
        b'd' => matches!(
            value,
            Value::Letter(
                b'A' | b'i'
                    | b'I'
                    | b'n'
                    | b'N'
                    | b'c'
                    | b'C'
                    | b'f'
                    | b'F'
                    | b'p'
                    | b'P'
                    | b'q'
                    | b'Q'
                    | b'r'
                    | b'R'
                    | b'x'
                    | b'X'
                    | b'y'
                    | b'Y'
                    | b'z'
                    | b'Z'
            )
        ),
        b'z' => nonzero_signed(value),
        _ => matches!(value, Value::Unsigned(1..)),
    }
}

fn nonzero_signed(value: Value) -> bool {
    match value {
        Value::Signed(value) => value != 0,
        Value::Unsigned(value) => value != 0 && i32::try_from(value).is_ok(),
        _ => false,
    }
}
