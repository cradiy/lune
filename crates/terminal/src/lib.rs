//! Terminal screen storage and incremental VT execution.

#[cfg(test)]
extern crate self as lune_terminal;

mod cell;
mod charsets;
mod dcs;
mod error;
mod graphics;
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
mod rendition_stack;
mod reply;
mod row;
mod screen;
mod sizing;
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
pub use graphics::{
    GraphicsLayout, GraphicsPlaceholderCache, GraphicsPlaceholderRun, GraphicsPlacement,
};
pub use grid::{Grid, MAX_CELLS, MAX_DIMENSION};
pub use history::{HistoryLimits, MAX_HISTORY_BYTES, MAX_HISTORY_ROWS, RowHistory};
pub use input::{InputModes, Key, KeySequence, Modifiers};
pub use lines::{LogicalLine, LogicalLines};
pub use lune_kitty_graphics::{
    Image as GraphicsImage, ImageFormat as GraphicsImageFormat, ImageKey as GraphicsImageKey,
    ImageStore as GraphicsImageStore, PixelBounds as GraphicsPixelBounds,
    PixelRevision as GraphicsPixelRevision, PixelUpdate as GraphicsPixelUpdate,
    PixelUpdates as GraphicsPixelUpdates,
};
pub use lune_kitty_graphics::{
    PLACEHOLDER as GRAPHICS_PLACEHOLDER, Placeholder as GraphicsPlaceholder,
};
pub use lune_text::WidthPolicy;
pub use mouse::{
    MouseButton, MouseEvent, MouseFormat, MouseSequence, MouseTracking, PixelPosition,
};
pub use osc::{MAX_OSC_PALETTE_ENTRIES, MAX_TITLE_BYTES, MAX_WORKING_DIRECTORY_URI_BYTES};
pub use palette::{GraphicsColors, Palette, PaletteEntry, Rgb};
pub use paste::Paste;
pub use processor::{
    ProcessError, ProcessFailure, Processor, TerminalNotifications, WorkingDirectoryChange,
};
pub use reflow::{ReflowAffinity, ReflowAnchor, ReflowRow, ReflowRows};
pub use reply::{ColorScheme, PixelSize};
pub use row::{DisplayUnit, DisplayUnits, Row, RowRevision, StyleRun};
pub use screen::{Cursor, CursorAppearance, CursorShape, EraseMode, Screen, ScreenBuffer};
pub use sizing::{SizedTextRow, TextSize};
pub use style::{Color, Style, Underline};
pub use styles::{MAX_STYLES, StyleError, StyleTable};
pub use text::{CellPosition, ScreenView, TextRange};
pub use viewport::Viewport;
pub use writer::{TextSink, UnicodeWriter};
