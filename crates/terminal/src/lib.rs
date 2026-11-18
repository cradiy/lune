//! Terminal screen storage and incremental VT execution.

#[cfg(test)]
extern crate self as lune_terminal;

mod cell;
mod charsets;
mod dcs;
mod error;
mod grid;
mod history;
mod input;
mod lines;
mod mouse;
mod osc;
mod palette;
mod paste;
mod processor;
mod reflow;
mod reply;
mod row;
mod screen;
mod style;
mod styles;
mod tabs;
mod text;
mod viewport;
mod writer;

#[cfg(test)]
mod tests;

pub use cell::{Cell, CellText, CellWidth, MAX_CELL_TEXT_BYTES, StyleId};
pub use error::StorageError;
pub use grid::{Grid, MAX_CELLS, MAX_DIMENSION};
pub use history::{HistoryLimits, MAX_HISTORY_BYTES, MAX_HISTORY_ROWS, RowHistory};
pub use input::{InputModes, Key, KeySequence, Modifiers};
pub use lines::{LogicalLine, LogicalLines};
pub use lune_text::WidthPolicy;
pub use mouse::{MouseButton, MouseEvent, MouseSequence, MouseTracking};
pub use osc::{MAX_OSC_PALETTE_ENTRIES, MAX_TITLE_BYTES, MAX_WORKING_DIRECTORY_URI_BYTES};
pub use palette::{Palette, PaletteEntry, Rgb};
pub use paste::Paste;
pub use processor::{ProcessError, ProcessFailure, Processor};
pub use reflow::{ReflowAffinity, ReflowAnchor, ReflowRow, ReflowRows};
pub use row::Row;
pub use screen::{Cursor, CursorAppearance, CursorShape, EraseMode, Screen, ScreenBuffer};
pub use style::{Color, Style, Underline};
pub use styles::{MAX_STYLES, StyleError, StyleTable};
pub use text::{CellPosition, ScreenView, TextRange};
pub use viewport::Viewport;
pub use writer::{TextSink, UnicodeWriter};
