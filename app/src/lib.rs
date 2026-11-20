#[cfg(target_os = "linux")]
mod geometry;
#[cfg(target_os = "linux")]
mod session;

#[cfg(target_os = "linux")]
pub use geometry::GridGeometry;
#[cfg(target_os = "linux")]
pub use session::{
    CursorChange, MAX_INPUT_BYTES, MAX_PASTE_BYTES, MouseInputOutcome, PresentationDamage,
    PresentationFrame, PresentationRun, PresentationStyle, PresentationTracker, PresentedCursor,
    PtyInterest, RowDamage, Session, SessionInterests, SessionReady, WaitOutcome,
};
