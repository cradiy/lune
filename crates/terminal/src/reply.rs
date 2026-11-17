use lune_vt::{ControlSequence, OscTerminator};

use crate::{InputModes, MouseTracking, Rgb, Screen, ScreenBuffer};

// OSC 4;255;rgb:hhhh/hhhh/hhhh followed by ST.
const CAPACITY: usize = 28;

#[derive(Clone, Copy, Default)]
pub(crate) struct Reply {
    bytes: [u8; CAPACITY],
    start: u8,
    end: u8,
}

#[derive(Clone, Copy)]
pub(crate) enum Report {
    Status,
    Position(u16, u16),
}

#[derive(Clone, Copy)]
pub(crate) struct ModeReport {
    private: bool,
    selector: u16,
    enabled: Option<bool>,
}

#[derive(Clone, Copy)]
pub(crate) enum ColorTarget {
    Dynamic(u8),
    Indexed(u8),
}

#[derive(Clone, Copy)]
pub(crate) struct ColorReport {
    pub(crate) target: ColorTarget,
    pub(crate) color: Rgb,
    pub(crate) terminator: OscTerminator,
}

impl ColorReport {
    #[cold]
    pub(crate) fn encode(self, reply: &mut Reply) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        reply.clear();
        reply.append(b"\x1b]");
        match self.target {
            ColorTarget::Dynamic(selector) => reply.number(u16::from(selector)),
            ColorTarget::Indexed(index) => {
                reply.append(b"4;");
                reply.number(u16::from(index));
            }
        }
        reply.append(b";rgb:");
        for (index, channel) in [self.color.red, self.color.green, self.color.blue]
            .into_iter()
            .enumerate()
        {
            if index != 0 {
                reply.append(b"/");
            }
            let high = HEX[usize::from(channel >> 4)];
            let low = HEX[usize::from(channel & 15)];
            reply.append(&[high, low, high, low]);
        }
        reply.append(match self.terminator {
            OscTerminator::Bell => b"\x07",
            OscTerminator::St => b"\x1b\\",
        });
    }
}

impl Report {
    pub(crate) fn encode(self, reply: &mut Reply) {
        match self {
            Self::Status => {
                reply.clear();
                reply.append(b"\x1b[0n");
            }
            Self::Position(row, column) => reply.position(row, column),
        }
    }
}

impl Reply {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes[usize::from(self.start)..usize::from(self.end)]
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.start == self.end
    }

    pub(crate) fn consume(&mut self, count: usize) -> bool {
        if count > usize::from(self.end - self.start) {
            return false;
        }
        self.start += count as u8;
        true
    }

    fn clear(&mut self) {
        self.start = 0;
        self.end = 0;
    }

    pub(crate) fn position(&mut self, row: u16, column: u16) {
        self.clear();
        self.append(b"\x1b[");
        self.number(row);
        self.append(b";");
        self.number(column);
        self.append(b"R");
    }

    fn append(&mut self, bytes: &[u8]) {
        let end = usize::from(self.end);
        self.bytes[end..end + bytes.len()].copy_from_slice(bytes);
        self.end += bytes.len() as u8;
    }

    fn number(&mut self, mut number: u16) {
        let mut digits = [0; 5];
        let mut start = digits.len();
        loop {
            start -= 1;
            digits[start] = b'0' + (number % 10) as u8;
            number /= 10;
            if number == 0 {
                break;
            }
        }
        self.append(&digits[start..]);
    }
}

pub(crate) fn query(screen: &Screen, sequence: ControlSequence<'_>) -> Option<Report> {
    if !sequence.private_markers.is_empty()
        || !sequence.intermediates.is_empty()
        || sequence.parameters.len() != 1
        || sequence.parameters[0].is_subparameter
    {
        return None;
    }
    match sequence.parameters[0].value? {
        5 => Some(Report::Status),
        6 => {
            let cursor = screen.cursor();
            let origin = if screen.origin_mode() {
                screen.scroll_region().start
            } else {
                0
            };
            let row = u16::try_from(cursor.row - origin + 1).expect("Rows fit the protocol bounds");
            let column = u16::try_from(cursor.column + 1).expect("Columns fit the protocol bounds");
            Some(Report::Position(row, column))
        }
        _ => None,
    }
}

#[cold]
pub(crate) fn mode_query(
    screen: &Screen,
    input: InputModes,
    sequence: ControlSequence<'_>,
) -> Option<ModeReport> {
    let private = match sequence.private_markers {
        [] => false,
        [b'?'] => true,
        _ => return None,
    };
    if sequence.intermediates != b"$"
        || sequence.parameters.len() > 1
        || sequence
            .parameters
            .iter()
            .any(|parameter| parameter.is_subparameter)
    {
        return None;
    }
    let selector = sequence
        .parameters
        .first()
        .and_then(|parameter| parameter.value)
        .unwrap_or(0);
    let enabled = match (private, selector) {
        (false, 4) => Some(screen.insert_mode()),
        (false, 20) => Some(input.new_line),
        (true, 1) => Some(input.application_cursor),
        (true, 6) => Some(screen.origin_mode()),
        (true, 7) => Some(screen.auto_wrap()),
        (true, 12) => Some(screen.cursor_appearance().blinking),
        (true, 25) => Some(screen.cursor_appearance().visible),
        (true, 47 | 1047 | 1049) => Some(screen.active_buffer() == ScreenBuffer::Alternate),
        (true, 66) => Some(input.application_keypad),
        (true, 67) => Some(input.backarrow),
        (true, 1000) => Some(input.mouse_tracking == MouseTracking::Buttons),
        (true, 1002) => Some(input.mouse_tracking == MouseTracking::ButtonMotion),
        (true, 1003) => Some(input.mouse_tracking == MouseTracking::AnyMotion),
        (true, 1004) => Some(input.focus_reporting),
        (true, 1006) => Some(input.sgr_mouse),
        (true, 2004) => Some(input.bracketed_paste),
        _ => None,
    };
    Some(ModeReport {
        private,
        selector,
        enabled,
    })
}

impl ModeReport {
    #[cold]
    pub(crate) fn encode(self, reply: &mut Reply) {
        let Self {
            private,
            selector,
            enabled,
        } = self;
        reply.clear();
        reply.append(if private { b"\x1b[?" } else { b"\x1b[" });
        reply.number(selector);
        let status = match enabled {
            None => b'0',
            Some(true) => b'1',
            Some(false) => b'2',
        };
        reply.append(&[b';', status, b'$', b'y']);
    }
}
