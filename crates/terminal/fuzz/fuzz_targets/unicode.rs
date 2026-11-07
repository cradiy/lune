#![no_main]

#[allow(
    dead_code,
    reason = "Shared screen oracle operations serve several targets."
)]
mod model;
#[allow(
    dead_code,
    reason = "Shared screen oracle operations serve several targets."
)]
mod screen_model;

use libfuzzer_sys::fuzz_target;
use lune_terminal::{
    CellPosition, CellWidth, EraseMode, ProcessFailure, Processor, Screen, StorageError, StyleId,
    TextSink, UnicodeWriter, WidthPolicy,
};
use screen_model::{Command, ScreenModel};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

struct Oracle {
    screen: ScreenModel,
    current: String,
    position: Option<CellPosition>,
    policy: WidthPolicy,
}

impl Oracle {
    fn boundary(&mut self) {
        self.current.clear();
        self.position = None;
    }

    fn control(&mut self, command: Command<'_>) {
        self.boundary();
        self.screen.apply(&command, StyleId(0)).unwrap();
    }

    fn scalar(&mut self, scalar: char) {
        let candidate = format!("{}{scalar}", self.current);
        let extending = !self.current.is_empty() && candidate.graphemes(true).count() == 1;
        let unit = if extending {
            candidate
        } else {
            scalar.to_string()
        };
        let raw = if self.policy == WidthPolicy::Narrow {
            unit.width()
        } else {
            unit.width_cjk()
        };
        let (cursor, columns, auto_wrap) = self.screen.placement();
        let column = if extending {
            self.position.unwrap().column
        } else {
            cursor.column
        };
        let width = raw
            .clamp(1, 2)
            .min(if auto_wrap { columns } else { columns - column });
        let target = if (extending && column + width > columns)
            || (!extending && (cursor.wrap_pending || column + width > columns))
        {
            0
        } else {
            column
        };
        let width = if width == 1 {
            CellWidth::Single
        } else {
            CellWidth::Double
        };
        let command = if extending {
            Command::Revise(self.position.unwrap(), &unit, width)
        } else {
            Command::Write(&unit, width)
        };
        self.screen.apply(&command, StyleId(0)).unwrap();
        self.position = Some(CellPosition {
            row: self.screen.placement().0.row,
            column: target,
        });
        self.current = unit;
    }
}

struct FaultSink {
    writer: UnicodeWriter,
    calls: usize,
    fail_at: usize,
}
impl TextSink for FaultSink {
    type Error = Option<StorageError>;
    fn boundary(&mut self) {
        self.writer.boundary();
    }
    fn write(
        &mut self,
        screen: &mut Screen,
        text: &str,
        style: StyleId,
    ) -> Result<(), Self::Error> {
        let fail = self.calls == self.fail_at;
        self.calls += 1;
        if fail {
            return Err(None);
        }
        self.writer.write(screen, text, style).map_err(Some)
    }
}

fn run(wire: &[u8], chunk: usize, policy: WidthPolicy, fail_at: usize, oracle: &ScreenModel) {
    let mut processor = Processor::default();
    let mut screen = Screen::new(8, 4).unwrap();
    let mut sink = FaultSink {
        writer: UnicodeWriter::new(policy),
        calls: 0,
        fail_at,
    };
    for mut part in wire.chunks(chunk) {
        loop {
            match processor.advance(part, &mut screen, &mut sink) {
                Ok(count) => {
                    assert_eq!(count, part.len());
                    break;
                }
                Err(error) => {
                    assert!(matches!(error.error, ProcessFailure::Text(None)));
                    part = &part[error.consumed..];
                }
            }
        }
        processor.collect_styles(&screen, []).unwrap();
    }
    processor.finish(&mut screen, &mut sink).unwrap();
    assert_eq!(processor.parse_errors(), 0);
    oracle.check(&screen);
}

fuzz_target!(|data: &[u8]| {
    let policy = if data.first().copied().unwrap_or(0) & 1 == 0 {
        WidthPolicy::Narrow
    } else {
        WidthPolicy::Wide
    };
    let fixtures = [
        "abc",
        "é",
        "e\u{301}",
        "❤\u{fe0f}",
        "⌚\u{fe0e}",
        "👩🏽\u{200d}💻",
        "🇺🇸",
        "🇨",
        "\u{301}",
        "水",
        "·",
        "1\u{fe0f}\u{20e3}",
        "\u{600}ab",
        "\u{17d8}",
        "\u{200d}",
        "\u{fe0f}",
        "\u{605}a",
        "क्\u{200d}ष",
    ];
    let mut wire = Vec::new();
    let mut oracle = Oracle {
        screen: ScreenModel::new(8, 4),
        current: String::new(),
        position: None,
        policy,
    };
    for bytes in data.as_chunks::<4>().0.iter().take(512) {
        let enabled = bytes[1] & 1 != 0;
        match bytes[0] % 20 {
            0..=4 => {
                let fixture = fixtures[bytes[1] as usize % fixtures.len()];
                if oracle.current.len() + fixture.len() > 1024 {
                    wire.push(b'\r');
                    oracle.control(Command::CarriageReturn);
                }
                wire.extend(fixture.as_bytes());
                for scalar in fixture.chars() {
                    oracle.scalar(scalar);
                }
            }
            5 => {
                wire.push(b'\r');
                oracle.control(Command::CarriageReturn);
            }
            6 => {
                wire.push(b'\n');
                oracle.control(Command::LineFeed);
            }
            7 => {
                let row = bytes[1] as usize % 4;
                let column = bytes[2] as usize % 8;
                wire.extend(format!("\x1b[{};{}H", row + 1, column + 1).as_bytes());
                oracle.control(Command::Cursor(row, column));
            }
            8 => {
                wire.extend(if enabled { b"\x1b[4h" } else { b"\x1b[4l" });
                oracle.control(Command::InsertMode(enabled));
            }
            9 => {
                wire.extend(if enabled { b"\x1b[?7h" } else { b"\x1b[?7l" });
                oracle.control(Command::AutoWrap(enabled));
            }
            10 => {
                wire.extend(if enabled { b"\x1b[?47h" } else { b"\x1b[?47l" });
                oracle.control(Command::SelectBuffer(enabled));
            }
            11 => {
                let region = if enabled { 1..3 } else { 0..4 };
                wire.extend(format!("\x1b[{};{}r", region.start + 1, region.end).as_bytes());
                oracle.control(Command::Region(region));
            }
            12 => {
                wire.extend(if enabled { b"\x1b[?6h" } else { b"\x1b[?6l" });
                oracle.control(Command::OriginMode(enabled));
            }
            13 => {
                wire.extend(b"\x1b[2J");
                oracle.control(Command::EraseDisplay(EraseMode::All));
            }
            14 => {
                wire.extend(if enabled { b"\x1b7" } else { b"\x1b8" });
                oracle.control(if enabled {
                    Command::SaveCursor
                } else {
                    Command::RestoreCursor
                });
            }
            15 => {
                wire.extend(b"\x1bc");
                oracle.control(Command::Reset);
            }
            16 => {
                wire.extend(b"\x1b[0m");
                oracle.boundary();
            }
            17 => {
                wire.extend(b"\x1b]title\x07");
                oracle.boundary();
            }
            18 => {
                wire.extend(if enabled {
                    &b"\x7f"[..]
                } else {
                    &b"\xc2\x80"[..]
                });
                oracle.boundary();
            }
            _ => {
                wire.extend(b"\xe2\n");
                oracle.scalar('�');
                oracle.control(Command::LineFeed);
            }
        }
    }
    run(&wire, wire.len().max(1), policy, usize::MAX, &oracle.screen);
    run(
        &wire,
        1 + usize::from(data.get(1).copied().unwrap_or(0)) % 17,
        policy,
        usize::from(data.get(2).copied().unwrap_or(0)) % 31,
        &oracle.screen,
    );
});
