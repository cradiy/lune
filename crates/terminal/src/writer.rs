use lune_text::{GraphemeBuffer, TextFailure, TextUpdate, WidthPolicy};

use crate::{CellPosition, CellWidth, GRAPHICS_PLACEHOLDER, Screen, StorageError, StyleId};

const _: () = assert!(lune_text::MAX_GRAPHEME_BYTES == crate::MAX_CELL_TEXT_BYTES);

/// Atomic text application and semantic boundaries for [`crate::Processor`].
/// Pair the sink with one processor and screen across calls. Each write receives
/// printable ASCII or one decoded scalar. Rejection preserves the screen and
/// accepted text state. Boundaries end the previous unit's extension opportunity;
/// they are infallible and idempotent. Closures implement stateless unit writing.
pub trait TextSink {
    /// Whether writes retain context that requires protocol boundary notifications.
    const STATEFUL: bool = true;

    /// Application error reported when a text update fails.
    type Error;

    /// Applies a complete callback atomically.
    fn write(&mut self, screen: &mut Screen, text: &str, style: StyleId)
    -> Result<(), Self::Error>;
    /// Ends the opportunity to extend the previous text unit.
    fn boundary(&mut self);
}

impl<E, F: FnMut(&mut Screen, &str, StyleId) -> Result<(), E>> TextSink for F {
    const STATEFUL: bool = false;
    type Error = E;

    #[inline]
    fn write(&mut self, screen: &mut Screen, text: &str, style: StyleId) -> Result<(), E> {
        self(screen, text, style)
    }

    #[inline]
    fn boundary(&mut self) {}
}

/// Incremental Unicode placement for a paired screen and processor.
///
/// Grapheme text retains at most 1,024 UTF-8 bytes. Updates use fixed scratch space;
/// rows own fallible text storage. Widths are clamped to one or two columns, then
/// reduced to fit a single-column screen or the remaining columns with wrapping
/// disabled. Orphan zero-width clusters occupy one column and retain their text.
/// Decoded C1 controls end the cluster and are ignored. Every failed text write
/// preserves both screen and accepted grapheme state for processor retry.
///
/// Processor controls, finish, and reset establish boundaries automatically.
/// Call [`TextSink::boundary`] before externally changing live text or placement,
/// or replacing the processor. Width policy remains fixed for the writer's lifetime.
pub struct UnicodeWriter {
    grapheme: GraphemeBuffer,
    position: Option<CellPosition>,
}

impl Default for UnicodeWriter {
    fn default() -> Self {
        Self::new(WidthPolicy::default())
    }
}

impl UnicodeWriter {
    /// Starts an empty text stream with a fixed ambiguous-width policy.
    pub const fn new(policy: WidthPolicy) -> Self {
        Self {
            grapheme: GraphemeBuffer::new(policy),
            position: None,
        }
    }

    /// The ambiguous-width policy selected at construction.
    pub const fn width_policy(&self) -> WidthPolicy {
        self.grapheme.policy()
    }
}

impl TextSink for UnicodeWriter {
    type Error = StorageError;

    fn boundary(&mut self) {
        self.grapheme.clear();
        self.position = None;
    }

    #[inline]
    fn write(
        &mut self,
        screen: &mut Screen,
        text: &str,
        style: StyleId,
    ) -> Result<(), StorageError> {
        if text.is_empty() {
            return Ok(());
        }
        // Processor callbacks contain one scalar or an entire printable ASCII run.
        // After a possible first-scalar extension, an ASCII suffix is infallible.
        // This keeps the entire callback atomic even when it emits multiple updates.
        let ascii = text.as_bytes().iter().all(|b| (b' '..=b'~').contains(b));
        if ascii
            && self
                .grapheme
                .current()
                .is_none_or(|unit| unit.text.len() == 1 && unit.text.is_ascii())
        {
            let position = place_ascii(screen, text, style)?;
            // Every ASCII byte starts a new cluster in this context. Only its
            // final byte must remain available for a later Unicode extension.
            self.grapheme
                .advance(&text[text.len() - 1..], |_| {
                    Ok::<_, core::convert::Infallible>(())
                })
                .expect("A printable ASCII tail starts a bounded cluster");
            self.position = Some(position);
            return Ok(());
        }
        if ascii && screen.history_capture_enabled() {
            self.write_ascii_clusters(screen, text, style)
        } else {
            self.write_clusters(screen, text, style, ascii)
        }
    }
}

impl UnicodeWriter {
    #[inline(never)]
    fn write_ascii_clusters(
        &mut self,
        screen: &mut Screen,
        text: &str,
        style: StyleId,
    ) -> Result<(), StorageError> {
        // A Prepend extension can consume one extra column before the ASCII suffix.
        let prepared = screen.prepare_ascii_text(text.len().saturating_add(1), style)?;
        let result = self.write_clusters(screen, text, style, true);
        if prepared {
            screen.finish_text_reservation();
        }
        result
    }

    #[inline(never)]
    fn write_clusters(
        &mut self,
        screen: &mut Screen,
        text: &str,
        style: StyleId,
        ascii: bool,
    ) -> Result<(), StorageError> {
        if !ascii {
            let mut scalars = text.chars();
            let scalar = scalars.next().expect("Text is nonempty");
            if scalars.next().is_some() {
                return Err(StorageError::InvalidText);
            }
            if scalar.is_control() {
                self.boundary();
                return Ok(());
            }
        }
        let position = &mut self.position;
        self.grapheme
            .advance(text, |update| {
                match update {
                    TextUpdate::Ascii(run) => {
                        *position = Some(place_ascii(screen, run, style)?);
                    }
                    TextUpdate::Start(unit) | TextUpdate::Extend(unit) => {
                        let extending = matches!(update, TextUpdate::Extend(_));
                        let columns = screen.grid().columns();
                        let column = if extending {
                            position.expect("An extension has a screen position").column
                        } else {
                            screen.cursor().column
                        };
                        let available = if screen.auto_wrap() {
                            columns
                        } else {
                            columns - column
                        };
                        let width = if unit.columns.min(available) >= 2
                            && !unit.text.starts_with(GRAPHICS_PLACEHOLDER)
                        {
                            CellWidth::Double
                        } else {
                            CellWidth::Single
                        };
                        *position = Some(if extending {
                            screen.revise_unit(
                                position.expect("An extension has a screen position"),
                                unit.text,
                                width,
                                style,
                            )?
                        } else {
                            let cursor = screen.cursor();
                            let target_column =
                                if cursor.wrap_pending || column + width.columns() > columns {
                                    0
                                } else {
                                    column
                                };
                            screen.write(unit.text, width, style)?;
                            CellPosition {
                                row: screen.cursor().row,
                                column: target_column,
                            }
                        });
                    }
                }
                Ok(())
            })
            .map(|_| ())
            .map_err(|failure| match failure.error {
                TextFailure::Rejected(error) => error,
                TextFailure::TooLong => StorageError::TextTooLong,
                TextFailure::Control(_) => StorageError::InvalidText,
            })
    }
}

#[inline]
fn place_ascii(
    screen: &mut Screen,
    text: &str,
    style: StyleId,
) -> Result<CellPosition, StorageError> {
    let initial_column = screen.cursor().column;
    screen.write_ascii(text.as_bytes(), style)?;
    let cursor = screen.cursor();
    Ok(CellPosition {
        row: cursor.row,
        column: if !screen.auto_wrap() {
            initial_column
                .saturating_add(text.len() - 1)
                .min(screen.grid().columns() - 1)
        } else if cursor.wrap_pending {
            cursor.column
        } else {
            cursor.column - 1
        },
    })
}
