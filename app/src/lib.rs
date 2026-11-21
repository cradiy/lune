#[cfg(target_os = "linux")]
mod geometry;
#[cfg(target_os = "linux")]
mod raster;
#[cfg(target_os = "linux")]
mod session;

#[cfg(target_os = "linux")]
pub use geometry::{GridGeometry, PixelRect};
#[cfg(target_os = "linux")]
pub use raster::{CoverageMask, PixelRegion, RasterSurface, RasterText};
#[cfg(target_os = "linux")]
pub use session::{
    BackgroundRect, BatchRun, CursorChange, CursorGlyph, CursorPaint, MAX_INPUT_BYTES,
    MAX_PASTE_BYTES, MouseInputOutcome, PaintBatch, PaintCommand, PaintError, PreparedPresentation,
    PresentationDamage, PresentationFrame, PresentationRun, PresentationStyle, PresentationTracker,
    PresentedCursor, PtyInterest, RowDamage, Session, SessionInterests, SessionReady, WaitOutcome,
};
