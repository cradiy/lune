#[cfg(all(target_os = "linux", feature = "desktop"))]
mod desktop;
#[cfg(target_os = "linux")]
mod geometry;
#[cfg(all(target_os = "linux", feature = "wgpu-renderer"))]
mod gpu;
#[cfg(target_os = "linux")]
mod raster;
#[cfg(target_os = "linux")]
mod session;

#[cfg(all(target_os = "linux", feature = "desktop"))]
#[doc(hidden)]
pub use desktop::bench as desktop_bench;
#[cfg(all(target_os = "linux", feature = "desktop"))]
pub use desktop::{
    cli::{
        CliAction, CliError, HELP_TEXT, LaunchOptions, MsgAction, VERSION_TEXT,
        parse_env as parse_cli,
    },
    ipc::{IpcCommand, list_sockets as list_ipc_sockets, send_command as send_ipc_command},
    run as run_desktop,
};
#[cfg(target_os = "linux")]
pub use geometry::{GridGeometry, PixelRect};
#[cfg(all(target_os = "linux", feature = "wgpu-renderer"))]
pub use gpu::{BackgroundPrepareError, CellBackgroundRenderer, GraphicsLayer, ImageRenderer};
#[cfg(target_os = "linux")]
pub use raster::{CoverageMask, GlyphMask, GlyphSource, PixelRegion, RasterSurface, RasterText};
#[cfg(target_os = "linux")]
pub use session::{
    BackgroundRect, BatchRun, CursorChange, CursorGlyph, CursorPaint, MAX_INPUT_BYTES,
    MAX_PASTE_BYTES, MouseInputOutcome, PaintBatch, PaintCommand, PaintError, PreparedPresentation,
    PresentationDamage, PresentationFrame, PresentationRun, PresentationStyle, PresentationTracker,
    PresentedCursor, PtyInterest, RowDamage, Session, SessionInterests, SessionReady, WaitOutcome,
};
