//! PTY process lifecycle and byte transport.

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::{Pty, PtySize};
