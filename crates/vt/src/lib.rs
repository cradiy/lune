//! Incremental VT stream parsing.
//!
//! [`Parser`] accepts UTF-8 text and seven-bit control sequences. It emits text spans,
//! C0 controls, ESC sequences, and CSI sequences through a synchronous callback.
//! Sequence storage uses fixed capacities. Omitted numeric values and colon
//! subparameters retain their original structure.
//! Complete text runs are borrowed from input and dispatched in batches. A
//! four-byte buffer joins UTF-8 scalars split across input chunks.
//!
//! OSC, DCS, SOS, PM, and APC payloads are discarded until their terminator or
//! cancellation. OSC also accepts BEL as a terminator. Consumers interpret the
//! emitted protocol events.
//!
//! The parsing rules follow the control-sequence structure described in
//! [XTerm Control Sequences](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html).

mod parser;
mod utf8;

#[cfg(test)]
mod tests;

pub use parser::{
    CsiSequence, EscapeSequence, Event, MAX_INTERMEDIATES, MAX_PARAMETERS, MAX_PRIVATE_MARKERS,
    MAX_SEQUENCE_BYTES, Parameter, ParseError, Parser,
};
