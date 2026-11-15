//! Incremental VT stream parsing.
//!
//! [`Parser`] accepts UTF-8 text and seven-bit control sequences. It emits text spans,
//! C0 controls, ESC and CSI sequences, and OSC events through a synchronous callback.
//! Sequence storage uses fixed capacities. Omitted numeric values and colon
//! subparameters retain their original structure.
//! Complete text runs are borrowed from input and dispatched in batches. A
//! four-byte buffer joins UTF-8 scalars split across input chunks.
//!
//! OSC emits framing events and borrowed raw payload spans. DCS, SOS, PM, and APC
//! payloads are discarded. Consumers interpret the emitted protocol events.
//!
//! The parsing rules follow the control-sequence structure described in
//! [XTerm Control Sequences](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html).

mod parser;
mod utf8;

#[cfg(test)]
mod tests;

pub use parser::{
    CsiSequence, EscapeSequence, Event, MAX_INTERMEDIATES, MAX_PARAMETERS, MAX_PRIVATE_MARKERS,
    MAX_SEQUENCE_BYTES, OscEvent, OscTerminator, Parameter, ParseError, Parser,
};
