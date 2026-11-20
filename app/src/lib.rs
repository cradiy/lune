#[cfg(target_os = "linux")]
mod session;

#[cfg(target_os = "linux")]
pub use session::{
    CursorChange, MAX_INPUT_BYTES, MAX_PASTE_BYTES, MouseInputOutcome, PresentationDamage,
    PresentationFrame, PresentationStyle, PresentationTracker, PresentedCursor, PtyInterest,
    RowDamage, Session, SessionInterests, SessionReady, WaitOutcome,
};
