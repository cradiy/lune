#[cfg(target_os = "linux")]
mod session;

#[cfg(target_os = "linux")]
pub use session::{MAX_INPUT_BYTES, MAX_PASTE_BYTES, MouseInputOutcome, Session, WaitOutcome};
