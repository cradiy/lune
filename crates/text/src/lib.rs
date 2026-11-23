//! Bounded Unicode text units for cell-based displays and text inspection.
//!
//! [`GraphemeBuffer`] emits each scalar immediately, either starting a grapheme
//! or extending the previous one. Extensions include the complete revised text
//! and column width, which can increase or decrease. Printable ASCII is batched
//! into independent single-column graphemes. The final grapheme remains available
//! for extension across calls; input chunk boundaries have no semantic effect.
//!
//! [`WidthPolicy`] selects narrow or wide ambiguous characters. Widths follow
//! `unicode-width`'s sequence rules, including emoji presentation and ligatures
//! within each grapheme. Widths can be zero or greater than two; consumers decide
//! how to place those units. Widths are calculated per grapheme, so shaping
//! effects spanning separate graphemes require a separate presentation policy.
//!
//! Call [`GraphemeBuffer::clear`] at a semantic text boundary, such as a cursor
//! control or a consumer-selected rendition boundary. It ends the opportunity
//! to extend the previous unit. All accepted text has already been emitted.
//! Controls in the C0/C1 ranges, including DEL, are rejected before consumption.
//! Callers own byte decoding, protocol interpretation, and placement.
//!
//! Each callback is a transaction: on rejection it must preserve its destination.
//! The buffer commits only after acceptance. [`TextError::consumed`] identifies
//! the accepted UTF-8 prefix; retry the remaining suffix. One grapheme retains
//! at most [`MAX_GRAPHEME_BYTES`] UTF-8 bytes. An excessive extension fails before
//! consumption; a new grapheme can still replace a full buffer. Storage is fixed,
//! and processing makes no allocation requests. Unicode data is pinned to 17.0.0.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]
#![deny(missing_docs)]

use core::{error::Error, fmt};
use unicode_segmentation::GraphemeCursor;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Unicode data version used for both segmentation and width calculation.
pub const UNICODE_VERSION: (u8, u8, u8) = unicode_width::UNICODE_VERSION;

const _: () = {
    assert!(UNICODE_VERSION.0 == 17 && UNICODE_VERSION.1 == 0 && UNICODE_VERSION.2 == 0);
    let (major, minor, patch) = unicode_segmentation::UNICODE_VERSION;
    assert!(major == UNICODE_VERSION.0 as u64);
    assert!(minor == UNICODE_VERSION.1 as u64);
    assert!(patch == UNICODE_VERSION.2 as u64);
};

/// Maximum UTF-8 bytes retained in one extended grapheme cluster.
pub const MAX_GRAPHEME_BYTES: usize = 1024;

/// Ambiguous-character column width and sequence-aware Unicode width rules.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WidthPolicy {
    /// Ambiguous characters use one column. This is the default policy.
    #[default]
    Narrow,
    /// Ambiguous characters use the East Asian width rules.
    Wide,
}

impl WidthPolicy {
    fn width_neutral(self, scalar: char) -> bool {
        if matches!(scalar, '\u{338}' | '\u{fe00}'..='\u{fe02}' | '\u{fe0e}'..='\u{fe0f}') {
            return false;
        }
        match self {
            Self::Narrow => scalar.width() == Some(0),
            Self::Wide => scalar.width_cjk() == Some(0),
        }
    }

    /// Returns the number of columns for a complete grapheme.
    pub fn columns(self, text: &str) -> usize {
        match self {
            Self::Narrow => text.width(),
            Self::Wide => text.width_cjk(),
        }
    }
}

/// A complete current grapheme and its sequence-aware column width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grapheme<'a> {
    /// UTF-8 text, including all accepted combining and joining scalars.
    pub text: &'a str,
    /// Column width under the buffer's policy, including zero-width units.
    pub columns: usize,
}

/// One atomic update to the consumer's text stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextUpdate<'a> {
    /// Independent, printable, single-column ASCII graphemes. The final byte
    /// becomes the current grapheme and can receive later extensions.
    Ascii(&'a str),
    /// Starts a new grapheme after the preceding unit, if any.
    Start(Grapheme<'a>),
    /// Replaces the previous grapheme with its complete extended value.
    Extend(Grapheme<'a>),
}

/// Failure before accepting the next update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextFailure<E> {
    /// A C0/C1 control or DEL requires protocol handling by the caller.
    Control(char),
    /// Extending the current grapheme would exceed [`MAX_GRAPHEME_BYTES`].
    TooLong,
    /// The consumer rejected the proposed update.
    Rejected(E),
}

impl<E: fmt::Display> fmt::Display for TextFailure<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Control(value) => write!(
                f,
                "Control character U+{:04X} requires handling",
                *value as u32
            ),
            Self::TooLong => f.write_str("Grapheme text exceeds the storage limit"),
            Self::Rejected(error) => write!(f, "Text update rejected: {error}"),
        }
    }
}

impl<E: Error + 'static> Error for TextFailure<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Rejected(error) => Some(error),
            _ => None,
        }
    }
}

/// A failure together with the accepted prefix of the current input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextError<E> {
    /// Accepted byte count, always a UTF-8 boundary in the supplied input.
    pub consumed: usize,
    /// Reason the following update could not be accepted.
    pub error: TextFailure<E>,
}

impl<E: fmt::Display> fmt::Display for TextError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Text processing stopped after {} bytes: {}",
            self.consumed, self.error
        )
    }
}

impl<E: Error + 'static> Error for TextError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.error)
    }
}

/// Fixed-capacity incremental extended-grapheme state.
///
/// The extra four scratch bytes allow boundary detection when a full cluster is
/// followed by a new scalar. Segmentation and sequence-sensitive widths inspect
/// the current cluster, with work bounded by [`MAX_GRAPHEME_BYTES`] per scalar.
pub struct GraphemeBuffer {
    bytes: [u8; MAX_GRAPHEME_BYTES + 4],
    length: usize,
    columns: usize,
    policy: WidthPolicy,
}

impl Default for GraphemeBuffer {
    fn default() -> Self {
        Self::new(WidthPolicy::default())
    }
}

impl GraphemeBuffer {
    /// Starts an empty stream with a fixed column-width policy.
    pub const fn new(policy: WidthPolicy) -> Self {
        Self {
            bytes: [0; MAX_GRAPHEME_BYTES + 4],
            length: 0,
            columns: 0,
            policy,
        }
    }

    /// The column-width policy selected at construction.
    pub const fn policy(&self) -> WidthPolicy {
        self.policy
    }

    /// Current grapheme, including accepted extensions across earlier calls.
    pub fn current(&self) -> Option<Grapheme<'_>> {
        (self.length != 0).then(|| Grapheme {
            text: self.text(self.length),
            columns: self.columns,
        })
    }

    /// Ends the current grapheme while retaining the width policy.
    pub fn clear(&mut self) {
        self.length = 0;
        self.columns = 0;
    }

    /// Applies printable UTF-8 text in callback-sized transactions.
    ///
    /// Success returns `input.len()`. On failure, earlier accepted updates remain
    /// committed, and the failing update preserves the current grapheme. Retry
    /// `&input[error.consumed..]`; the consumer must preserve its state on rejection.
    /// Empty input preserves state and invokes no callback. ASCII batching can
    /// vary with input chunking; the resulting sequence of graphemes is invariant.
    pub fn advance<E>(
        &mut self,
        input: &str,
        mut accept: impl FnMut(TextUpdate<'_>) -> Result<(), E>,
    ) -> Result<usize, TextError<E>> {
        let mut consumed = 0;
        while consumed < input.len() {
            let remaining = &input[consumed..];
            if (self.length == 0 || (self.length == 1 && (b' '..=b'~').contains(&self.bytes[0])))
                && let Some(&first) = remaining.as_bytes().first()
                && (b' '..=b'~').contains(&first)
            {
                let length = remaining
                    .as_bytes()
                    .iter()
                    .position(|byte| !(b' '..=b'~').contains(byte))
                    .unwrap_or(remaining.len());
                accept(TextUpdate::Ascii(&remaining[..length])).map_err(|error| TextError {
                    consumed,
                    error: TextFailure::Rejected(error),
                })?;
                self.bytes[0] = remaining.as_bytes()[length - 1];
                self.length = 1;
                self.columns = 1;
                consumed += length;
                continue;
            }
            let scalar = remaining
                .chars()
                .next()
                .expect("Remaining text is nonempty");
            self.scalar(scalar, &mut accept)
                .map_err(|error| TextError { consumed, error })?;
            consumed += scalar.len_utf8();
        }
        Ok(consumed)
    }

    fn scalar<E>(
        &mut self,
        scalar: char,
        accept: &mut impl FnMut(TextUpdate<'_>) -> Result<(), E>,
    ) -> Result<(), TextFailure<E>> {
        if scalar.is_control() {
            return Err(TextFailure::Control(scalar));
        }
        let end = self.length + scalar.len_utf8();
        scalar.encode_utf8(&mut self.bytes[self.length..end]);
        let text = self.text(end);
        let boundary = self.length == 0
            || GraphemeCursor::new(self.length, end, true)
                .is_boundary(text, 0)
                .expect("The complete preceding cluster and following scalar are available");
        let start = if boundary { self.length } else { 0 };
        let text = &text[start..];
        if text.len() > MAX_GRAPHEME_BYTES {
            return Err(TextFailure::TooLong);
        }
        // In the pinned width implementation, neutral zero-width suffixes leave
        // the reverse width machine in its initial state. Appending one preserves
        // the preceding text's width. Overlay and variation selectors carry state
        // and require evaluating the complete sequence.
        let columns = if !boundary && self.policy.width_neutral(scalar) {
            self.columns
        } else {
            self.policy.columns(text)
        };
        let grapheme = Grapheme { text, columns };
        accept(if boundary {
            TextUpdate::Start(grapheme)
        } else {
            TextUpdate::Extend(grapheme)
        })
        .map_err(TextFailure::Rejected)?;
        if boundary {
            self.bytes.copy_within(start..end, 0);
        }
        self.length = end - start;
        self.columns = columns;
        Ok(())
    }

    fn text(&self, end: usize) -> &str {
        // SAFETY: the accepted prefix contains complete UTF-8 scalars. Scratch
        // bytes are written by char::encode_utf8; callers use either the accepted
        // length or the complete appended scalar's end. Rejections leave every
        // byte of the accepted prefix intact, and ASCII commits store one byte.
        unsafe { core::str::from_utf8_unchecked(&self.bytes[..end]) }
    }
}

#[cfg(test)]
mod tests;
