use std::num::NonZeroU32;

use lune_vt::{ControlSequence, OscTerminator, Parameter};

use crate::dcs::StatusQuery;
use crate::{
    Color, CursorShape, InputModes, MouseFormat, MouseTracking, Rgb, Screen, ScreenBuffer, Style,
    StyleId, StyleTable, Underline,
};

// A full DECRQSS rendition reply fits in 78 bytes.
const CAPACITY: usize = lune_kitty_graphics::MAX_FEATURE_REPLY_BYTES;
const VERSION_REPLY: &[u8] = concat!(
    "\x1bP>|",
    env!("CARGO_PKG_NAME"),
    "(",
    env!("CARGO_PKG_VERSION"),
    ")\x1b\\"
)
.as_bytes();
const _: () = assert!(VERSION_REPLY.len() <= CAPACITY);

#[derive(Clone, Copy)]
pub(crate) struct Reply {
    bytes: [u8; CAPACITY],
    start: u16,
    end: u16,
}

impl Default for Reply {
    fn default() -> Self {
        Self {
            bytes: [0; CAPACITY],
            start: 0,
            end: 0,
        }
    }
}

/// The host color preference reported to terminal applications.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorScheme {
    Dark,
    Light,
}

/// The width and height of a terminal surface or cell in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelSize {
    pub width: NonZeroU32,
    pub height: NonZeroU32,
}

#[derive(Clone, Copy)]
pub(crate) enum Report {
    Status,
    Position {
        row: u16,
        column: u16,
        private: bool,
    },
    PrimaryDeviceAttributes,
    Version,
    ColorScheme(ColorScheme),
}

#[derive(Clone, Copy)]
pub(crate) enum StatusReport {
    Rendition(Style),
    Margins(u16, u16),
    CursorStyle(u8),
    Unsupported,
}

impl StatusReport {
    #[cold]
    pub(crate) fn capture(
        query: StatusQuery,
        screen: &Screen,
        styles: &StyleTable,
        current: StyleId,
    ) -> Self {
        match query {
            StatusQuery::Rendition => {
                Self::Rendition(*styles.get(current).expect("Current rendition is rooted"))
            }
            StatusQuery::Margins => {
                let region = screen.scroll_region();
                Self::Margins((region.start + 1) as u16, region.end as u16)
            }
            StatusQuery::CursorStyle => {
                let appearance = screen.cursor_appearance();
                let shape = match appearance.shape {
                    CursorShape::Block => 1,
                    CursorShape::Underline => 3,
                    CursorShape::Bar => 5,
                };
                Self::CursorStyle(shape + u8::from(!appearance.blinking))
            }
            StatusQuery::Unsupported => Self::Unsupported,
        }
    }

    #[cold]
    pub(crate) fn encode(self, reply: &mut Reply) {
        reply.clear();
        if matches!(self, Self::Unsupported) {
            reply.append(b"\x1bP0$r\x1b\\");
            return;
        }
        reply.append(b"\x1bP1$r");
        match self {
            Self::Rendition(style) => {
                reply.append(b"0");
                let underline: &[u8] = match style.underline {
                    Underline::None => b"",
                    Underline::Single => b"4",
                    Underline::Double => b"21",
                    Underline::Curly => b"4:3",
                    Underline::Dotted => b"4:4",
                    Underline::Dashed => b"4:5",
                };
                for (enabled, code) in [
                    (style.bold, b"1".as_slice()),
                    (style.faint, b"2"),
                    (style.italic, b"3"),
                    (!underline.is_empty(), underline),
                    (style.blink, b"5"),
                    (style.inverse, b"7"),
                    (style.hidden, b"8"),
                    (style.strikethrough, b"9"),
                ] {
                    if enabled {
                        reply.append(b";");
                        reply.append(code);
                    }
                }
                for (selector, color) in [
                    (38, style.foreground),
                    (48, style.background),
                    (58, style.underline_color),
                ] {
                    if color == Color::Default {
                        continue;
                    }
                    reply.append(b";");
                    reply.number(selector);
                    match color {
                        Color::Default => unreachable!(),
                        Color::Indexed(index) => {
                            reply.append(b";5;");
                            reply.number(u16::from(index));
                        }
                        Color::Rgb { red, green, blue } => {
                            reply.append(b";2");
                            for channel in [red, green, blue] {
                                reply.append(b";");
                                reply.number(u16::from(channel));
                            }
                        }
                    }
                }
                reply.append(b"m");
            }
            Self::Margins(top, bottom) => {
                reply.number(top);
                reply.append(b";");
                reply.number(bottom);
                reply.append(b"r");
            }
            Self::CursorStyle(style) => {
                reply.number(u16::from(style));
                reply.append(b" q");
            }
            Self::Unsupported => unreachable!(),
        }
        reply.append(b"\x1b\\");
    }
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
            Self::PrimaryDeviceAttributes => {
                reply.clear();
                reply.append(b"\x1b[?6c");
            }
            Self::Version => reply.version(),
            Self::Position {
                row,
                column,
                private,
            } => reply.position(row, column, private),
            Self::ColorScheme(scheme) => {
                reply.clear();
                reply.append(match scheme {
                    ColorScheme::Dark => b"\x1b[?997;1n",
                    ColorScheme::Light => b"\x1b[?997;2n",
                });
            }
        }
    }
}

impl Reply {
    #[cold]
    pub(crate) fn graphics(&mut self, reply: lune_kitty_graphics::Reply) {
        self.clear();
        self.append(b"\x1b_G");
        if reply.id != 0 {
            self.append(b"i=");
            self.decimal::<10>(reply.id);
        }
        if reply.number != 0 {
            if reply.id != 0 {
                self.append(b",");
            }
            self.append(b"I=");
            self.decimal::<10>(reply.number);
        }
        if reply.placement_id != 0 {
            self.append(b",p=");
            self.decimal::<10>(reply.placement_id);
        }
        if reply.frame_number != 0 {
            self.append(b",r=");
            self.decimal::<10>(reply.frame_number);
        }
        if let Some(features) = reply.features {
            use lune_kitty_graphics::Value;

            self.append(&[b',', b'e', b'=', features.action()]);
            for &(key, value) in features.controls() {
                self.append(&[b',', key, b'=']);
                match value {
                    Value::Unsigned(value) => self.decimal::<10>(value),
                    Value::Signed(value) => {
                        self.append(b"-");
                        self.decimal::<10>(value.unsigned_abs());
                    }
                    Value::Letter(value) => self.append(&[value]),
                }
            }
        }
        self.append(b";");
        self.append(reply.message);
        self.append(b"\x1b\\");
    }

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
        self.start += count as u16;
        true
    }

    fn clear(&mut self) {
        self.start = 0;
        self.end = 0;
    }

    pub(crate) fn position(&mut self, row: u16, column: u16, private: bool) {
        self.clear();
        self.append(b"\x1b[");
        if private {
            self.append(b"?");
        }
        if row < 10 && column < 10 {
            self.append(&[b'0' + row as u8, b';', b'0' + column as u8, b'R']);
            return;
        }
        self.number(row);
        self.append(b";");
        self.number(column);
        self.append(b"R");
    }

    #[cold]
    fn version(&mut self) {
        self.clear();
        self.append(VERSION_REPLY);
    }

    #[cold]
    fn text_area_size(&mut self, rows: u16, columns: u16) {
        self.clear();
        self.append(b"\x1b[8;");
        self.number(rows);
        self.append(b";");
        self.number(columns);
        self.append(b"t");
    }

    #[cold]
    fn pixel_size(&mut self, prefix: &[u8; 4], size: PixelSize) {
        self.clear();
        self.append(prefix);
        self.decimal::<10>(size.height.get());
        self.append(b";");
        self.decimal::<10>(size.width.get());
        self.append(b"t");
    }

    fn append(&mut self, bytes: &[u8]) {
        let end = usize::from(self.end);
        self.bytes[end..end + bytes.len()].copy_from_slice(bytes);
        self.end += bytes.len() as u16;
    }

    fn number(&mut self, value: u16) {
        const DECIMAL_PAIRS: [[u8; 2]; 100] = {
            let mut table = [[0; 2]; 100];
            let mut index = 0;
            while index < table.len() {
                table[index] = [b'0' + (index / 10) as u8, b'0' + (index % 10) as u8];
                index += 1;
            }
            table
        };

        match value {
            0..=9 => self.append(&[b'0' + value as u8]),
            10..=99 => self.append(&DECIMAL_PAIRS[usize::from(value)]),
            100..=999 => {
                let [tens, ones] = DECIMAL_PAIRS[usize::from(value % 100)];
                self.append(&[b'0' + (value / 100) as u8, tens, ones]);
            }
            _ => self.decimal::<5>(u32::from(value)),
        }
    }

    fn decimal<const DIGITS: usize>(&mut self, mut number: u32) {
        let mut digits = [0; DIGITS];
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

pub(crate) fn query(
    screen: &Screen,
    sequence: ControlSequence<'_>,
    color_scheme: Option<ColorScheme>,
) -> Option<Report> {
    if !sequence.intermediates.is_empty()
        || sequence.parameters.len() != 1
        || sequence.parameters[0].is_subparameter
    {
        return None;
    }
    match (sequence.parameters[0].value?, sequence.private_markers) {
        (5, []) => Some(Report::Status),
        (6, [] | [b'?']) => {
            let cursor = screen.cursor();
            let origin = if screen.origin_mode() {
                screen.scroll_region().start
            } else {
                0
            };
            let row = u16::try_from(cursor.row - origin + 1).expect("Rows fit the protocol bounds");
            let column = u16::try_from(cursor.column + 1).expect("Columns fit the protocol bounds");
            Some(Report::Position {
                row,
                column,
                private: !sequence.private_markers.is_empty(),
            })
        }
        (996, [b'?']) => color_scheme.map(Report::ColorScheme),
        _ => None,
    }
}

#[cold]
pub(crate) fn identity_query(sequence: ControlSequence<'_>) -> Option<Report> {
    if !sequence.intermediates.is_empty()
        || !matches!(
            sequence.parameters,
            [] | [Parameter {
                value: Some(0),
                is_subparameter: false,
            }]
        )
    {
        return None;
    }
    match (sequence.final_byte, sequence.private_markers) {
        (b'c', []) => Some(Report::PrimaryDeviceAttributes),
        (b'q', [b'>']) => Some(Report::Version),
        _ => None,
    }
}

#[derive(Clone, Copy)]
pub(crate) enum SizeReport {
    TextArea { rows: u16, columns: u16 },
    TextAreaPixels(PixelSize),
    CellPixels(PixelSize),
    Resize(ResizeReport),
}

impl SizeReport {
    #[cold]
    pub(crate) fn encode(self, reply: &mut Reply) {
        match self {
            Self::TextArea { rows, columns } => reply.text_area_size(rows, columns),
            Self::TextAreaPixels(size) => reply.pixel_size(b"\x1b[4;", size),
            Self::CellPixels(size) => reply.pixel_size(b"\x1b[6;", size),
            Self::Resize(size) => size.encode(reply),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResizeReport {
    rows: u16,
    columns: u16,
    pixels: Option<PixelSize>,
}

impl ResizeReport {
    pub(crate) fn capture(screen: &Screen, pixels: Option<PixelSize>) -> Self {
        let grid = screen.grid();
        Self {
            rows: u16::try_from(grid.rows()).expect("Rows fit the protocol bounds"),
            columns: u16::try_from(grid.columns()).expect("Columns fit the protocol bounds"),
            pixels,
        }
    }

    #[cold]
    pub(crate) fn encode(self, reply: &mut Reply) {
        let (height, width) = self
            .pixels
            .map_or((0, 0), |size| (size.height.get(), size.width.get()));
        reply.clear();
        reply.append(b"\x1b[48;");
        reply.number(self.rows);
        reply.append(b";");
        reply.number(self.columns);
        reply.append(b";");
        reply.decimal::<10>(height);
        reply.append(b";");
        reply.decimal::<10>(width);
        reply.append(b"t");
    }
}

#[cold]
pub(crate) fn size_query(
    screen: &Screen,
    sequence: ControlSequence<'_>,
    text_area_pixels: Option<PixelSize>,
    cell_pixels: Option<PixelSize>,
) -> Option<SizeReport> {
    if !sequence.private_markers.is_empty() || !sequence.intermediates.is_empty() {
        return None;
    }
    let [
        Parameter {
            value: Some(selector),
            is_subparameter: false,
        },
    ] = sequence.parameters
    else {
        return None;
    };
    match selector {
        14 => text_area_pixels.map(SizeReport::TextAreaPixels),
        16 => cell_pixels.map(SizeReport::CellPixels),
        18 => {
            let grid = screen.grid();
            Some(SizeReport::TextArea {
                rows: u16::try_from(grid.rows()).expect("Rows fit the protocol bounds"),
                columns: u16::try_from(grid.columns()).expect("Columns fit the protocol bounds"),
            })
        }
        _ => None,
    }
}

#[cold]
pub(crate) fn mode_query(
    screen: &Screen,
    input: InputModes,
    auto_repeat: bool,
    synchronized_output: bool,
    color_scheme_notifications: bool,
    resize_reporting: bool,
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
        (true, 5) => Some(screen.reverse_video()),
        (true, 6) => Some(screen.origin_mode()),
        (true, 7) => Some(screen.auto_wrap()),
        (true, 8) => Some(auto_repeat),
        (true, 12) => Some(screen.cursor_appearance().blinking),
        (true, 25) => Some(screen.cursor_appearance().visible),
        (true, 47 | 1047 | 1049) => Some(screen.active_buffer() == ScreenBuffer::Alternate),
        (true, 66) => Some(input.application_keypad),
        (true, 67) => Some(input.backarrow),
        (true, 1000) => Some(input.mouse_tracking == MouseTracking::Buttons),
        (true, 1002) => Some(input.mouse_tracking == MouseTracking::ButtonMotion),
        (true, 1003) => Some(input.mouse_tracking == MouseTracking::AnyMotion),
        (true, 1004) => Some(input.focus_reporting),
        (true, 1006) => Some(input.mouse_format == MouseFormat::Sgr),
        (true, 1007) => Some(input.alternate_scroll),
        (true, 1016) => Some(input.mouse_format == MouseFormat::SgrPixels),
        (true, 2004) => Some(input.bracketed_paste),
        (true, 2026) => Some(synchronized_output),
        (true, 2031) => Some(color_scheme_notifications),
        (true, 2048) => Some(resize_reporting),
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
