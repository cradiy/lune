//! Incremental VT stream parsing.
//!
//! [`Parser`] accepts UTF-8 text and seven-bit control sequences. It emits text spans,
//! C0 controls, ESC and CSI sequences, and OSC/DCS events through a synchronous callback.
//! Sequence storage uses fixed capacities. Omitted numeric values and colon
//! subparameters retain their original structure.
//! Complete text runs are borrowed from input and dispatched in batches. A
//! four-byte buffer joins UTF-8 scalars split across input chunks.
//!
//! OSC and DCS emit framing events and borrowed raw payload spans. DCS headers
//! share CSI's bounded parameter structure. APC streams raw payload spans. SOS
//! and PM payloads are discarded.
//!
//! The parsing rules follow the control-sequence structure described in
//! [XTerm Control Sequences](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html).

mod parser;
mod utf8;

#[cfg(test)]
mod tests;

pub use parser::{
    ApcEvent, ControlSequence, DcsEvent, EscapeSequence, Event, MAX_INTERMEDIATES, MAX_PARAMETERS,
    MAX_PRIVATE_MARKERS, MAX_SEQUENCE_BYTES, OscEvent, OscTerminator, Parameter, ParseError,
    Parser,
};
