use std::collections::{TryReserveError, VecDeque};

use lune_vt::{OscEvent, OscTerminator};

use crate::hyperlinks::{MAX_HYPERLINK_PARAMS_BYTES, MAX_HYPERLINK_URI_BYTES};
use crate::reply::{ColorReport, ColorTarget};
use crate::sizing::{self, MAX_SIZED_TEXT_BYTES, MAX_SIZING_METADATA_BYTES};
use crate::{Palette, PaletteEntry, Rgb};

/// Maximum UTF-8 byte length of an OSC 0/2 window title.
pub const MAX_TITLE_BYTES: usize = 4096;
pub(crate) const MAX_SAVED_TITLES: usize = 10;
/// Maximum encoded byte length of an OSC 7 working directory URI.
pub const MAX_WORKING_DIRECTORY_URI_BYTES: usize = MAX_TITLE_BYTES;
/// Maximum decoded UTF-8 byte length of an OSC 52 clipboard payload.
pub const MAX_CLIPBOARD_BYTES: usize = 128 * 1024;
/// Maximum UTF-8 byte length of an OSC 99 notification identifier.
pub const MAX_NOTIFICATION_ID_BYTES: usize = 128;
/// Maximum UTF-8 byte length of a desktop notification title.
pub const MAX_NOTIFICATION_TITLE_BYTES: usize = 256;
/// Maximum UTF-8 byte length of a desktop notification body.
pub const MAX_NOTIFICATION_BODY_BYTES: usize = 1024;
const MAX_OSC99_META_BYTES: usize = 256;
const MAX_OSC99_PAYLOAD_BYTES: usize = MAX_NOTIFICATION_BODY_BYTES.div_ceil(3) * 4;
const MAX_OSC133_BYTES: usize = 128;
const MAX_COLOR_BYTES: usize = 18;
/// Maximum number of indexed operations in one OSC 4/104 command.
pub const MAX_OSC_PALETTE_ENTRIES: usize = 256;

/// Semantic prompt boundary markers reported via `OSC 133`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticPrompt {
    PromptStart,
    InputStart,
    OutputStart,
    CommandEnd(Option<u8>),
}

/// Task progress reported via `OSC 9;4`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Progress {
    #[default]
    None,
    Normal(u8),
    Error(u8),
    Indeterminate,
    Paused(u8),
}

/// Urgency level for desktop notifications (`OSC 9` / `OSC 99` / `OSC 777`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NotificationUrgency {
    Low,
    #[default]
    Normal,
    Critical,
}

/// A completed desktop notification emitted by the terminal stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DesktopNotification<'a> {
    pub id: Option<&'a str>,
    pub title: &'a str,
    pub body: &'a str,
    pub urgency: NotificationUrgency,
}

/// Target selections requested by an OSC 52 clipboard command.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClipboardTargets {
    pub clipboard: bool,
    pub primary: bool,
}

/// An OSC 52 clipboard write payload. An empty payload clears the target selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipboardWrite<'a> {
    pub targets: ClipboardTargets,
    pub text: &'a str,
}

/// An OSC 52 clipboard read request preserving the request's wire terminator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipboardQuery {
    pub targets: ClipboardTargets,
    terminator: OscTerminator,
}

impl ClipboardQuery {
    /// Encodes an OSC 52 reply carrying `text` as standard base64 into `out`.
    pub fn encode_reply(self, text: &str, out: &mut Vec<u8>) -> Result<(), TryReserveError> {
        let encoded_len = text.len().div_ceil(3) * 4;
        let terminator: &[u8] = match self.terminator {
            OscTerminator::Bell => b"\x07",
            OscTerminator::St => b"\x1b\\",
        };
        let total = 7 + encoded_len + terminator.len();
        out.clear();
        out.try_reserve_exact(total)?;
        out.extend_from_slice(b"\x1b]52;");
        out.push(if self.targets.clipboard { b'c' } else { b'p' });
        out.push(b';');
        encode_base64(text.as_bytes(), out);
        out.extend_from_slice(terminator);
        Ok(())
    }
}

const BASE64_PAD: u8 = u8::MAX;

#[derive(Clone, Copy, Default)]
struct Base64Decoder {
    quantum: [u8; 4],
    len: u8,
    pad_seen: bool,
}

impl Base64Decoder {
    fn reset(&mut self) {
        self.len = 0;
        self.pad_seen = false;
    }

    fn push(&mut self, bytes: &[u8], out: &mut Vec<u8>) -> bool {
        for &byte in bytes {
            if matches!(byte, b'\r' | b'\n' | b' ') {
                continue;
            }
            if byte == b'=' {
                if self.len < 2 {
                    return false;
                }
                self.pad_seen = true;
                self.quantum[usize::from(self.len)] = BASE64_PAD;
                self.len += 1;
                if self.len == 4 {
                    if !self.flush_quantum(out) {
                        return false;
                    }
                    self.len = 0;
                }
                continue;
            }
            if self.pad_seen {
                return false;
            }
            let Some(value) = base64_value(byte) else {
                return false;
            };
            self.quantum[usize::from(self.len)] = value;
            self.len += 1;
            if self.len == 4 {
                if !self.flush_quantum(out) {
                    return false;
                }
                self.len = 0;
            }
        }
        true
    }

    fn finish(&mut self, out: &mut Vec<u8>) -> bool {
        match self.len {
            0 => true,
            1 => false,
            2 => {
                if self.pad_seen {
                    return false;
                }
                let b0 = (self.quantum[0] << 2) | (self.quantum[1] >> 4);
                push_clipboard_bytes(out, &[b0])
            }
            3 => {
                if self.pad_seen {
                    return false;
                }
                let b0 = (self.quantum[0] << 2) | (self.quantum[1] >> 4);
                let b1 = (self.quantum[1] << 4) | (self.quantum[2] >> 2);
                push_clipboard_bytes(out, &[b0, b1])
            }
            _ => unreachable!(),
        }
    }

    fn flush_quantum(&self, out: &mut Vec<u8>) -> bool {
        let q = self.quantum;
        if q[2] == BASE64_PAD {
            if q[3] != BASE64_PAD {
                return false;
            }
            let b0 = (q[0] << 2) | (q[1] >> 4);
            push_clipboard_bytes(out, &[b0])
        } else if q[3] == BASE64_PAD {
            let b0 = (q[0] << 2) | (q[1] >> 4);
            let b1 = (q[1] << 4) | (q[2] >> 2);
            push_clipboard_bytes(out, &[b0, b1])
        } else {
            let b0 = (q[0] << 2) | (q[1] >> 4);
            let b1 = (q[1] << 4) | (q[2] >> 2);
            let b2 = (q[2] << 6) | q[3];
            push_clipboard_bytes(out, &[b0, b1, b2])
        }
    }
}

fn push_clipboard_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> bool {
    if bytes.len() > MAX_CLIPBOARD_BYTES - out.len() || out.try_reserve(bytes.len()).is_err() {
        return false;
    }
    out.extend_from_slice(bytes);
    true
}

#[inline]
fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn encode_base64(input: &[u8], out: &mut Vec<u8>) {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let (chunks, remainder) = input.as_chunks::<3>();
    for chunk in chunks {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        out.extend_from_slice(&[
            TABLE[((n >> 18) & 63) as usize],
            TABLE[((n >> 12) & 63) as usize],
            TABLE[((n >> 6) & 63) as usize],
            TABLE[(n & 63) as usize],
        ]);
    }
    match remainder {
        [b0] => {
            let n = u32::from(*b0) << 16;
            out.extend_from_slice(&[
                TABLE[((n >> 18) & 63) as usize],
                TABLE[((n >> 12) & 63) as usize],
                b'=',
                b'=',
            ]);
        }
        [b0, b1] => {
            let n = (u32::from(*b0) << 16) | (u32::from(*b1) << 8);
            out.extend_from_slice(&[
                TABLE[((n >> 18) & 63) as usize],
                TABLE[((n >> 12) & 63) as usize],
                TABLE[((n >> 6) & 63) as usize],
                b'=',
            ]);
        }
        _ => {}
    }
}

#[derive(Clone, Copy, Default)]
enum Phase {
    #[default]
    Idle,
    Selector {
        value: u16,
        digits: u8,
    },
    Title,
    WorkingDirectory,
    HyperlinkParams,
    HyperlinkUri,
    Osc9,
    Osc99Meta,
    Osc99Payload,
    Osc777Verb,
    Osc777Title,
    Osc777Body,
    Osc133,
    ClipboardTargets,
    ClipboardData,
    SizedText,
    Color(u8),
    Index,
    IndexedColor(u8),
    ResetIndex,
    Discard,
}

pub(crate) enum Outcome {
    Done,
    Ignored,
    Reply(ColorReport),
    Replies,
    SizedText,
    Hyperlink,
    SemanticPrompt(SemanticPrompt),
}

#[derive(Clone, Copy)]
enum Operation {
    Set(u8, Rgb),
    Query(u8),
    Reset(u8),
    Reply(u8, Rgb),
    SetDynamic(u8, Rgb),
    QueryDynamic(u8),
    ReplyDynamic(u8, Rgb),
}

pub(crate) struct Osc {
    buffers: [[u8; MAX_TITLE_BYTES]; 2],
    lengths: [usize; 2],
    active: usize,
    phase: Phase,
    changed: bool,
    saved_titles: VecDeque<Vec<u8>>,
    palette: Palette,
    palette_changed: bool,
    operations: [Operation; MAX_OSC_PALETTE_ENTRIES],
    operation_count: usize,
    reply_head: usize,
    reply_end: usize,
    terminator: OscTerminator,
    index: u16,
    index_digits: u8,
    directory: [u8; MAX_WORKING_DIRECTORY_URI_BYTES],
    directory_length: usize,
    directory_changed: bool,
    sized_text: Vec<u8>,
    clipboard_targets: ClipboardTargets,
    clipboard_target_seen: bool,
    clipboard_is_query: Option<bool>,
    clipboard_decoder: Base64Decoder,
    clipboard_candidate: Vec<u8>,
    clipboard_text: Vec<u8>,
    clipboard_write: Option<ClipboardTargets>,
    clipboard_query: Option<ClipboardQuery>,
    hyperlink_params: [u8; MAX_HYPERLINK_PARAMS_BYTES],
    hyperlink_params_len: usize,
    hyperlink_uri: [u8; MAX_HYPERLINK_URI_BYTES],
    hyperlink_uri_len: usize,
    progress: Progress,
    progress_changed: bool,
    notify_stage: [u8; MAX_OSC99_PAYLOAD_BYTES],
    notify_stage_len: usize,
    osc99_meta: [u8; MAX_OSC99_META_BYTES],
    osc99_meta_len: usize,
    notify_title_stage: [u8; MAX_NOTIFICATION_TITLE_BYTES],
    notify_title_stage_len: usize,
    notify_body_stage: [u8; MAX_NOTIFICATION_BODY_BYTES],
    notify_body_stage_len: usize,
    osc99_id: [u8; MAX_NOTIFICATION_ID_BYTES],
    osc99_id_len: usize,
    osc99_urgency: NotificationUrgency,
    osc99_in_progress: bool,
    notify_id: [u8; MAX_NOTIFICATION_ID_BYTES],
    notify_id_len: usize,
    notify_title: [u8; MAX_NOTIFICATION_TITLE_BYTES],
    notify_title_len: usize,
    notify_body: [u8; MAX_NOTIFICATION_BODY_BYTES],
    notify_body_len: usize,
    notify_urgency: NotificationUrgency,
    notify_ready: bool,
    osc133: [u8; MAX_OSC133_BYTES],
    osc133_len: usize,
}

impl Osc {
    pub(crate) fn new(palette: Palette) -> Self {
        Self {
            buffers: [[0; MAX_TITLE_BYTES]; 2],
            lengths: [0; 2],
            active: 0,
            phase: Phase::Idle,
            changed: false,
            saved_titles: VecDeque::new(),
            palette,
            palette_changed: false,
            operations: [Operation::Query(0); MAX_OSC_PALETTE_ENTRIES],
            operation_count: 0,
            reply_head: 0,
            reply_end: 0,
            terminator: OscTerminator::St,
            index: 0,
            index_digits: 0,
            directory: [0; MAX_WORKING_DIRECTORY_URI_BYTES],
            directory_length: 0,
            directory_changed: false,
            sized_text: Vec::new(),
            clipboard_targets: ClipboardTargets::default(),
            clipboard_target_seen: false,
            clipboard_is_query: None,
            clipboard_decoder: Base64Decoder::default(),
            clipboard_candidate: Vec::new(),
            clipboard_text: Vec::new(),
            clipboard_write: None,
            clipboard_query: None,
            hyperlink_params: [0; MAX_HYPERLINK_PARAMS_BYTES],
            hyperlink_params_len: 0,
            hyperlink_uri: [0; MAX_HYPERLINK_URI_BYTES],
            hyperlink_uri_len: 0,
            progress: Progress::None,
            progress_changed: false,
            notify_stage: [0; MAX_OSC99_PAYLOAD_BYTES],
            notify_stage_len: 0,
            osc99_meta: [0; MAX_OSC99_META_BYTES],
            osc99_meta_len: 0,
            notify_title_stage: [0; MAX_NOTIFICATION_TITLE_BYTES],
            notify_title_stage_len: 0,
            notify_body_stage: [0; MAX_NOTIFICATION_BODY_BYTES],
            notify_body_stage_len: 0,
            osc99_id: [0; MAX_NOTIFICATION_ID_BYTES],
            osc99_id_len: 0,
            osc99_urgency: NotificationUrgency::Normal,
            osc99_in_progress: false,
            notify_id: [0; MAX_NOTIFICATION_ID_BYTES],
            notify_id_len: 0,
            notify_title: [0; MAX_NOTIFICATION_TITLE_BYTES],
            notify_title_len: 0,
            notify_body: [0; MAX_NOTIFICATION_BODY_BYTES],
            notify_body_len: 0,
            notify_urgency: NotificationUrgency::Normal,
            notify_ready: false,
            osc133: [0; MAX_OSC133_BYTES],
            osc133_len: 0,
        }
    }

    pub(crate) fn palette(&self) -> &Palette {
        &self.palette
    }

    pub(crate) fn take_palette_change(&mut self) -> bool {
        std::mem::take(&mut self.palette_changed)
    }

    pub(crate) fn reset_indexed(&mut self) {
        self.palette_changed |= self.palette.reset_indexed();
    }

    pub(crate) fn set_base(
        &mut self,
        indexed: [Rgb; 256],
        foreground: Rgb,
        background: Rgb,
        cursor: Rgb,
    ) -> bool {
        let changed = self
            .palette
            .set_base(indexed, foreground, background, cursor);
        self.palette_changed |= changed;
        changed
    }

    pub(crate) fn set_base_from(&mut self, theme: &Palette) -> bool {
        let changed = self.palette.set_base_from(theme);
        self.palette_changed |= changed;
        changed
    }

    pub(crate) fn title(&self) -> &str {
        std::str::from_utf8(&self.buffers[self.active][..self.lengths[self.active]])
            .expect("Committed window titles are valid UTF-8")
    }

    pub(crate) fn take_title_change(&mut self) -> Option<&str> {
        self.take_title_changed().then(|| self.title())
    }

    pub(crate) fn take_title_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    #[cold]
    pub(crate) fn push_title(&mut self) -> Result<(), TryReserveError> {
        let title = &self.buffers[self.active][..self.lengths[self.active]];
        if self.saved_titles.len() == MAX_SAVED_TITLES {
            let oldest = self
                .saved_titles
                .front_mut()
                .expect("The title stack is full");
            oldest.try_reserve_exact(title.len().saturating_sub(oldest.len()))?;
            oldest.clear();
            oldest.extend_from_slice(title);
            self.saved_titles.rotate_left(1);
        } else {
            self.saved_titles
                .try_reserve_exact(MAX_SAVED_TITLES - self.saved_titles.len())?;
            let mut saved = Vec::new();
            saved.try_reserve_exact(title.len())?;
            saved.extend_from_slice(title);
            self.saved_titles.push_back(saved);
        }
        Ok(())
    }

    #[cold]
    pub(crate) fn pop_title(&mut self) {
        let Some(title) = self.saved_titles.pop_back() else {
            return;
        };
        let active = &mut self.buffers[self.active];
        if active[..self.lengths[self.active]] != title {
            active[..title.len()].copy_from_slice(&title);
            self.lengths[self.active] = title.len();
            self.changed = true;
        }
    }

    pub(crate) fn working_directory_uri(&self) -> Option<&str> {
        (self.directory_length != 0).then(|| {
            std::str::from_utf8(&self.directory[..self.directory_length])
                .expect("Committed directory URIs are valid ASCII")
        })
    }

    pub(crate) fn take_working_directory_change(&mut self) -> bool {
        std::mem::take(&mut self.directory_changed)
    }

    pub(crate) fn take_clipboard_write(&mut self) -> Option<ClipboardTargets> {
        self.clipboard_write.take()
    }

    pub(crate) fn clipboard_text(&self) -> &str {
        std::str::from_utf8(&self.clipboard_text)
            .expect("Committed clipboard payloads are valid UTF-8")
    }

    pub(crate) fn take_clipboard_query(&mut self) -> Option<ClipboardQuery> {
        self.clipboard_query.take()
    }

    pub(crate) fn progress(&self) -> Progress {
        self.progress
    }

    pub(crate) fn take_progress_change(&mut self) -> Option<Progress> {
        std::mem::take(&mut self.progress_changed).then_some(self.progress)
    }

    pub(crate) fn take_notification_ready(&mut self) -> bool {
        std::mem::take(&mut self.notify_ready)
    }

    pub(crate) fn desktop_notification(&self) -> DesktopNotification<'_> {
        let id = (self.notify_id_len != 0).then(|| {
            std::str::from_utf8(&self.notify_id[..self.notify_id_len])
                .expect("Committed notification IDs are valid UTF-8")
        });
        let title = std::str::from_utf8(&self.notify_title[..self.notify_title_len])
            .expect("Committed notification titles are valid UTF-8");
        let body = std::str::from_utf8(&self.notify_body[..self.notify_body_len])
            .expect("Committed notification bodies are valid UTF-8");
        DesktopNotification {
            id,
            title,
            body,
            urgency: self.notify_urgency,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.directory_changed |= self.directory_length != 0;
        self.directory_length = 0;
        self.changed |= self.lengths[self.active] != 0;
        self.lengths = [0; 2];
        self.saved_titles.clear();
        self.phase = Phase::Idle;
        self.palette_changed |= self.palette.reset_all();
        self.operation_count = 0;
        self.reply_head = 0;
        self.reply_end = 0;
        self.clipboard_candidate.clear();
        self.clipboard_text.clear();
        self.clipboard_write = None;
        self.clipboard_query = None;
        self.hyperlink_params_len = 0;
        self.hyperlink_uri_len = 0;
        self.progress_changed |= self.progress != Progress::None;
        self.progress = Progress::None;
        self.notify_stage_len = 0;
        self.osc99_meta_len = 0;
        self.notify_title_stage_len = 0;
        self.notify_body_stage_len = 0;
        self.osc99_id_len = 0;
        self.osc99_urgency = NotificationUrgency::Normal;
        self.osc99_in_progress = false;
        self.notify_ready = false;
        self.osc133_len = 0;
    }

    #[inline]
    pub(crate) fn needs_data(&self) -> bool {
        matches!(
            self.phase,
            Phase::Selector { .. }
                | Phase::Title
                | Phase::WorkingDirectory
                | Phase::HyperlinkParams
                | Phase::HyperlinkUri
                | Phase::Osc9
                | Phase::Osc99Meta
                | Phase::Osc99Payload
                | Phase::Osc777Verb
                | Phase::Osc777Title
                | Phase::Osc777Body
                | Phase::Osc133
                | Phase::ClipboardTargets
                | Phase::ClipboardData
                | Phase::SizedText
                | Phase::Color(_)
                | Phase::Index
                | Phase::IndexedColor(_)
                | Phase::ResetIndex
        )
    }

    #[inline]
    pub(crate) fn data(&mut self, bytes: &[u8]) {
        if matches!(self.phase, Phase::Title) {
            self.append_text(bytes);
        } else if matches!(self.phase, Phase::SizedText) {
            self.append_sized_text(bytes);
        } else if matches!(self.phase, Phase::ClipboardData) {
            self.append_clipboard_data(bytes);
        } else if matches!(self.phase, Phase::HyperlinkUri) {
            self.append_hyperlink_uri(bytes);
        } else if matches!(self.phase, Phase::Osc9) {
            self.append_osc9(bytes);
        } else if matches!(self.phase, Phase::Osc99Payload) {
            self.append_osc99_payload(bytes);
        } else if matches!(self.phase, Phase::Osc777Body) {
            self.append_osc777_body(bytes);
        } else if matches!(self.phase, Phase::Osc133) {
            self.append_osc133(bytes);
        } else if matches!(self.phase, Phase::Selector { .. }) {
            self.command_data(bytes);
        } else {
            self.batch_data(bytes);
        }
    }

    #[inline(never)]
    fn command_data(&mut self, mut bytes: &[u8]) {
        while let Phase::Selector { value, digits } = self.phase {
            let Some((&byte, rest)) = bytes.split_first() else {
                return;
            };
            bytes = rest;
            match byte {
                b';' if matches!(value, 0 | 2) => self.phase = Phase::Title,
                b';' if value == 7 => self.phase = Phase::WorkingDirectory,
                b';' if value == 8 => {
                    self.hyperlink_params_len = 0;
                    self.hyperlink_uri_len = 0;
                    self.phase = Phase::HyperlinkParams;
                }
                b';' if value == 9 => {
                    self.notify_stage_len = 0;
                    self.phase = Phase::Osc9;
                }
                b';' if value == 99 => {
                    self.osc99_meta_len = 0;
                    self.notify_stage_len = 0;
                    self.phase = Phase::Osc99Meta;
                }
                b';' if value == 777 => {
                    self.notify_stage_len = 0;
                    self.notify_title_stage_len = 0;
                    self.notify_body_stage_len = 0;
                    self.phase = Phase::Osc777Verb;
                }
                b';' if value == 133 => {
                    self.osc133_len = 0;
                    self.phase = Phase::Osc133;
                }
                b';' if value == 52 => {
                    self.clipboard_targets = ClipboardTargets::default();
                    self.clipboard_target_seen = false;
                    self.clipboard_is_query = None;
                    self.clipboard_decoder.reset();
                    self.clipboard_candidate.clear();
                    self.phase = Phase::ClipboardTargets;
                }
                b';' if value == 66 => {
                    self.sized_text.clear();
                    self.phase = Phase::SizedText;
                }
                b';' if matches!(value, 10..=12) => self.phase = Phase::Color(value as u8),
                b';' if value == 4 => self.phase = Phase::Index,
                b';' if value == 104 => self.phase = Phase::ResetIndex,
                b'0'..=b'9' if digits < 5 => {
                    let Some(value) = value
                        .checked_mul(10)
                        .and_then(|value| value.checked_add(u16::from(byte - b'0')))
                    else {
                        self.phase = Phase::Discard;
                        return;
                    };
                    self.phase = Phase::Selector {
                        value,
                        digits: digits + 1,
                    };
                }
                _ => {
                    self.phase = Phase::Discard;
                    return;
                }
            }
        }
        if matches!(self.phase, Phase::Title) {
            self.append_text(bytes);
        } else if matches!(self.phase, Phase::SizedText) {
            self.append_sized_text(bytes);
        } else if matches!(self.phase, Phase::ClipboardData) {
            self.append_clipboard_data(bytes);
        } else if matches!(self.phase, Phase::HyperlinkUri) {
            self.append_hyperlink_uri(bytes);
        } else if matches!(self.phase, Phase::Osc9) {
            self.append_osc9(bytes);
        } else if matches!(self.phase, Phase::Osc99Payload) {
            self.append_osc99_payload(bytes);
        } else if matches!(self.phase, Phase::Osc777Body) {
            self.append_osc777_body(bytes);
        } else if matches!(self.phase, Phase::Osc133) {
            self.append_osc133(bytes);
        } else {
            self.batch_data(bytes);
        }
    }

    #[cold]
    fn append_osc133(&mut self, bytes: &[u8]) {
        if bytes.len() > MAX_OSC133_BYTES - self.osc133_len {
            self.phase = Phase::Discard;
            return;
        }
        let end = self.osc133_len + bytes.len();
        self.osc133[self.osc133_len..end].copy_from_slice(bytes);
        self.osc133_len = end;
    }

    #[cold]
    fn append_osc9(&mut self, bytes: &[u8]) {
        if bytes.len() > MAX_NOTIFICATION_BODY_BYTES - self.notify_stage_len {
            self.phase = Phase::Discard;
            return;
        }
        let end = self.notify_stage_len + bytes.len();
        self.notify_stage[self.notify_stage_len..end].copy_from_slice(bytes);
        self.notify_stage_len = end;
    }

    #[cold]
    fn append_osc99_payload(&mut self, bytes: &[u8]) {
        if bytes.len() > MAX_OSC99_PAYLOAD_BYTES - self.notify_stage_len {
            self.phase = Phase::Discard;
            return;
        }
        let end = self.notify_stage_len + bytes.len();
        self.notify_stage[self.notify_stage_len..end].copy_from_slice(bytes);
        self.notify_stage_len = end;
    }

    #[cold]
    fn append_osc777_body(&mut self, bytes: &[u8]) {
        if bytes.len() > MAX_NOTIFICATION_BODY_BYTES - self.notify_body_stage_len {
            self.phase = Phase::Discard;
            return;
        }
        let end = self.notify_body_stage_len + bytes.len();
        self.notify_body_stage[self.notify_body_stage_len..end].copy_from_slice(bytes);
        self.notify_body_stage_len = end;
    }

    #[cold]
    fn append_hyperlink_uri(&mut self, bytes: &[u8]) {
        if bytes.iter().any(|&byte| byte < 0x20 || byte == 0x7f)
            || bytes.len() > MAX_HYPERLINK_URI_BYTES - self.hyperlink_uri_len
        {
            self.phase = Phase::Discard;
            return;
        }
        let end = self.hyperlink_uri_len + bytes.len();
        self.hyperlink_uri[self.hyperlink_uri_len..end].copy_from_slice(bytes);
        self.hyperlink_uri_len = end;
    }

    pub(crate) fn hyperlink(&self) -> Option<(&str, Option<&str>)> {
        let uri = std::str::from_utf8(&self.hyperlink_uri[..self.hyperlink_uri_len]).ok()?;
        let params =
            std::str::from_utf8(&self.hyperlink_params[..self.hyperlink_params_len]).ok()?;
        let id = params
            .split(':')
            .filter_map(|pair| pair.split_once('='))
            .rev()
            .find_map(|(key, value)| (key == "id" && !value.is_empty()).then_some(value));
        Some((uri, id))
    }

    #[cold]
    fn append_clipboard_data(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        match self.clipboard_is_query {
            None if bytes == b"?" => {
                self.clipboard_is_query = Some(true);
                return;
            }
            Some(true) => {
                self.phase = Phase::Discard;
                return;
            }
            None => {
                self.clipboard_is_query = Some(false);
            }
            Some(false) => {}
        }
        if !self
            .clipboard_decoder
            .push(bytes, &mut self.clipboard_candidate)
        {
            self.phase = Phase::Discard;
        }
    }

    #[inline]
    fn append_text(&mut self, bytes: &[u8]) {
        let candidate = self.active ^ 1;
        let length = self.lengths[candidate];
        if bytes.len() > MAX_TITLE_BYTES - length {
            self.phase = Phase::Discard;
            return;
        }
        match bytes {
            [byte] => self.buffers[candidate][length] = *byte,
            [] => return,
            _ => self.buffers[candidate][length..length + bytes.len()].copy_from_slice(bytes),
        }
        self.lengths[candidate] += bytes.len();
    }

    fn append_sized_text(&mut self, bytes: &[u8]) {
        const LIMIT: usize = MAX_SIZED_TEXT_BYTES + MAX_SIZING_METADATA_BYTES + 1;
        if bytes.len() > LIMIT - self.sized_text.len()
            || self.sized_text.try_reserve(bytes.len()).is_err()
        {
            self.phase = Phase::Discard;
            return;
        }
        self.sized_text.extend_from_slice(bytes);
    }

    pub(crate) fn sized_text(&self) -> Option<(crate::TextSize, usize, &str)> {
        sizing::parse(&self.sized_text)
    }

    #[inline(never)]
    pub(crate) fn event(&mut self, event: OscEvent<'_>) -> Outcome {
        match event {
            OscEvent::Start => {
                self.operation_count = 0;
                self.reply_head = 0;
                self.reply_end = 0;
                self.index = 0;
                self.index_digits = 0;
                self.lengths[self.active ^ 1] = 0;
                self.phase = Phase::Selector {
                    value: 0,
                    digits: 0,
                };
            }
            OscEvent::Data(bytes) => self.data(bytes),
            OscEvent::End(terminator) => {
                let candidate = self.active ^ 1;
                let bytes = &self.buffers[candidate][..self.lengths[candidate]];
                let phase = std::mem::take(&mut self.phase);
                return match phase {
                    Phase::Title if std::str::from_utf8(bytes).is_ok() => {
                        if bytes != &self.buffers[self.active][..self.lengths[self.active]] {
                            self.active = candidate;
                            self.changed = true;
                        }
                        Outcome::Done
                    }
                    Phase::WorkingDirectory => self.commit_directory(candidate),
                    Phase::HyperlinkUri if self.hyperlink().is_some() => Outcome::Hyperlink,
                    Phase::Osc9 => self.commit_osc9(),
                    Phase::Osc99Payload => self.commit_osc99(),
                    Phase::Osc777Body => self.commit_osc777(),
                    Phase::Osc133 => self.commit_osc133(),
                    Phase::ClipboardData => self.commit_clipboard(terminator),
                    Phase::SizedText if self.sized_text().is_some() => Outcome::SizedText,
                    Phase::Color(selector) if self.operation_count == 0 => {
                        let entry = role(u16::from(selector));
                        if bytes == b"?" {
                            Outcome::Reply(ColorReport {
                                target: ColorTarget::Dynamic(selector),
                                color: self.palette.color(entry),
                                terminator,
                            })
                        } else if let Some(color) = parse_color(bytes) {
                            self.palette_changed |= self.palette.set_override(entry, color);
                            Outcome::Done
                        } else {
                            Outcome::Ignored
                        }
                    }
                    Phase::Color(selector) => {
                        if !self.finish_color(ColorTarget::Dynamic(selector)) {
                            return Outcome::Ignored;
                        }
                        self.execute_operations(terminator)
                    }
                    Phase::Selector {
                        value: selector @ 110..=112,
                        digits: 1..=5,
                    } => {
                        self.palette_changed |= self.palette.reset(role(selector - 100));
                        Outcome::Done
                    }
                    Phase::IndexedColor(index) => {
                        if !self.finish_color(ColorTarget::Indexed(index)) {
                            return Outcome::Ignored;
                        }
                        self.execute_operations(terminator)
                    }
                    Phase::ResetIndex => {
                        let Some(index) = self.finished_index() else {
                            return Outcome::Ignored;
                        };
                        if !self.push(Operation::Reset(index)) {
                            return Outcome::Ignored;
                        }
                        self.execute_operations(terminator)
                    }
                    Phase::Selector {
                        value: 104,
                        digits: 1..=5,
                    } => {
                        self.palette_changed |= self.palette.reset_indexed();
                        Outcome::Done
                    }
                    _ => Outcome::Ignored,
                };
            }
            OscEvent::Cancel => {
                self.phase = Phase::Idle;
                self.operation_count = 0;
                self.clipboard_candidate.clear();
                self.hyperlink_params_len = 0;
                self.hyperlink_uri_len = 0;
            }
        }
        Outcome::Done
    }

    #[cold]
    fn commit_osc133(&mut self) -> Outcome {
        let bytes = &self.osc133[..self.osc133_len];
        let (kind, rest) = match bytes.iter().position(|&byte| byte == b';') {
            Some(separator) => (&bytes[..separator], Some(&bytes[separator + 1..])),
            None => (bytes, None),
        };
        let event = match kind {
            b"A" => SemanticPrompt::PromptStart,
            b"B" => SemanticPrompt::InputStart,
            b"C" => SemanticPrompt::OutputStart,
            b"D" => {
                let exit_code = rest.and_then(|rest| {
                    let first = rest.split(|&b| b == b';').next().unwrap_or_default();
                    if let Some(code) = parse_decimal_u16(first) {
                        return u8::try_from(code).ok();
                    }
                    rest.split(|&b| b == b';').find_map(|field| {
                        let digits = field.strip_prefix(b"err=")?;
                        u8::try_from(parse_decimal_u16(digits)?).ok()
                    })
                });
                SemanticPrompt::CommandEnd(exit_code)
            }
            _ => return Outcome::Ignored,
        };
        Outcome::SemanticPrompt(event)
    }

    #[cold]
    fn commit_osc9(&mut self) -> Outcome {
        let bytes = &self.notify_stage[..self.notify_stage_len];
        if bytes == b"4" || bytes.starts_with(b"4;") {
            let rest = bytes.strip_prefix(b"4;").unwrap_or_default();
            let Some(progress) = parse_progress(rest) else {
                return Outcome::Ignored;
            };
            if self.progress != progress {
                self.progress = progress;
                self.progress_changed = true;
            }
            return Outcome::Done;
        }
        if bytes.is_empty() || std::str::from_utf8(bytes).is_err() {
            return Outcome::Ignored;
        }
        self.notify_id_len = 0;
        self.notify_title_len = 0;
        self.notify_body[..bytes.len()].copy_from_slice(bytes);
        self.notify_body_len = bytes.len();
        self.notify_urgency = NotificationUrgency::Normal;
        self.notify_ready = true;
        Outcome::Done
    }

    #[cold]
    fn commit_osc777(&mut self) -> Outcome {
        let title = &self.notify_title_stage[..self.notify_title_stage_len];
        let body = &self.notify_body_stage[..self.notify_body_stage_len];
        if (title.is_empty() && body.is_empty())
            || std::str::from_utf8(title).is_err()
            || std::str::from_utf8(body).is_err()
        {
            return Outcome::Ignored;
        }
        self.notify_id_len = 0;
        self.notify_title[..title.len()].copy_from_slice(title);
        self.notify_title_len = title.len();
        self.notify_body[..body.len()].copy_from_slice(body);
        self.notify_body_len = body.len();
        self.notify_urgency = NotificationUrgency::Normal;
        self.notify_ready = true;
        Outcome::Done
    }

    #[cold]
    fn commit_osc99(&mut self) -> Outcome {
        let Ok(meta) = std::str::from_utf8(&self.osc99_meta[..self.osc99_meta_len]) else {
            return Outcome::Ignored;
        };
        let mut id: Option<&str> = None;
        let mut done = true;
        let mut part = "title";
        let mut encoded = false;
        let mut urgency: Option<NotificationUrgency> = None;
        for entry in meta.split(':').filter(|entry| !entry.is_empty()) {
            let Some((key, value)) = entry.split_once('=') else {
                return Outcome::Ignored;
            };
            match key {
                "i" => {
                    if value.len() > MAX_NOTIFICATION_ID_BYTES
                        || !value.bytes().all(|b| {
                            b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'+')
                        })
                    {
                        return Outcome::Ignored;
                    }
                    id = Some(value);
                }
                "d" => match value {
                    "0" => done = false,
                    "1" => done = true,
                    _ => return Outcome::Ignored,
                },
                "p" => part = value,
                "e" => match value {
                    "0" => encoded = false,
                    "1" => encoded = true,
                    _ => return Outcome::Ignored,
                },
                "u" => {
                    urgency = Some(match value {
                        "0" => NotificationUrgency::Low,
                        "1" => NotificationUrgency::Normal,
                        "2" => NotificationUrgency::Critical,
                        _ => return Outcome::Ignored,
                    });
                }
                _ => {}
            }
        }
        if part == "close" {
            self.osc99_in_progress = false;
            return Outcome::Done;
        }
        if !matches!(part, "title" | "body") {
            return Outcome::Done;
        }

        let raw = &self.notify_stage[..self.notify_stage_len];
        let mut decoded = [0u8; MAX_NOTIFICATION_BODY_BYTES];
        let payload = if encoded {
            let Some(len) = decode_base64_slice(raw, &mut decoded) else {
                return Outcome::Ignored;
            };
            &decoded[..len]
        } else {
            if raw.len() > MAX_NOTIFICATION_BODY_BYTES {
                return Outcome::Ignored;
            }
            raw
        };
        if std::str::from_utf8(payload).is_err() {
            return Outcome::Ignored;
        }

        let incoming_id = id.unwrap_or_default().as_bytes();
        let same_stream =
            self.osc99_in_progress && incoming_id == &self.osc99_id[..self.osc99_id_len];
        if !same_stream {
            self.osc99_id[..incoming_id.len()].copy_from_slice(incoming_id);
            self.osc99_id_len = incoming_id.len();
            self.notify_title_stage_len = 0;
            self.notify_body_stage_len = 0;
            self.osc99_urgency = NotificationUrgency::Normal;
            self.osc99_in_progress = true;
        }
        if let Some(urgency) = urgency {
            self.osc99_urgency = urgency;
        }
        if part == "title" {
            if payload.len() > MAX_NOTIFICATION_TITLE_BYTES - self.notify_title_stage_len {
                self.osc99_in_progress = false;
                return Outcome::Ignored;
            }
            let end = self.notify_title_stage_len + payload.len();
            self.notify_title_stage[self.notify_title_stage_len..end].copy_from_slice(payload);
            self.notify_title_stage_len = end;
        } else {
            if payload.len() > MAX_NOTIFICATION_BODY_BYTES - self.notify_body_stage_len {
                self.osc99_in_progress = false;
                return Outcome::Ignored;
            }
            let end = self.notify_body_stage_len + payload.len();
            self.notify_body_stage[self.notify_body_stage_len..end].copy_from_slice(payload);
            self.notify_body_stage_len = end;
        }

        if done {
            self.osc99_in_progress = false;
            let title = &self.notify_title_stage[..self.notify_title_stage_len];
            let body = &self.notify_body_stage[..self.notify_body_stage_len];
            if title.is_empty() && body.is_empty() {
                return Outcome::Done;
            }
            self.notify_id[..self.osc99_id_len]
                .copy_from_slice(&self.osc99_id[..self.osc99_id_len]);
            self.notify_id_len = self.osc99_id_len;
            self.notify_title[..title.len()].copy_from_slice(title);
            self.notify_title_len = title.len();
            self.notify_body[..body.len()].copy_from_slice(body);
            self.notify_body_len = body.len();
            self.notify_urgency = self.osc99_urgency;
            self.notify_ready = true;
        }
        Outcome::Done
    }

    #[cold]
    fn commit_clipboard(&mut self, terminator: OscTerminator) -> Outcome {
        let targets = self.clipboard_targets;
        if self.clipboard_is_query == Some(true) {
            if targets.clipboard || targets.primary {
                self.clipboard_query = Some(ClipboardQuery {
                    targets,
                    terminator,
                });
            }
            return Outcome::Done;
        }
        if !self.clipboard_decoder.finish(&mut self.clipboard_candidate)
            || std::str::from_utf8(&self.clipboard_candidate).is_err()
        {
            self.clipboard_candidate.clear();
            return Outcome::Ignored;
        }
        if targets.clipboard || targets.primary {
            let merged = if let Some(existing) = self.clipboard_write
                && self.clipboard_candidate == self.clipboard_text
            {
                ClipboardTargets {
                    clipboard: existing.clipboard || targets.clipboard,
                    primary: existing.primary || targets.primary,
                }
            } else {
                std::mem::swap(&mut self.clipboard_text, &mut self.clipboard_candidate);
                targets
            };
            self.clipboard_write = Some(merged);
        }
        self.clipboard_candidate.clear();
        Outcome::Done
    }

    #[cold]
    fn commit_directory(&mut self, candidate: usize) -> Outcome {
        let length = self.lengths[candidate];
        let bytes = &self.buffers[candidate][..length];
        if !bytes.is_empty() && !valid_directory_uri(bytes) {
            return Outcome::Ignored;
        }
        if bytes != &self.directory[..self.directory_length] {
            self.directory[..length].copy_from_slice(bytes);
            self.directory_length = length;
            self.directory_changed = true;
        }
        Outcome::Done
    }

    #[inline(never)]
    fn batch_data(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            match self.phase {
                Phase::WorkingDirectory => {
                    self.append_text(bytes);
                    return;
                }
                Phase::HyperlinkParams => {
                    if let Some(separator) = bytes.iter().position(|&byte| byte == b';') {
                        let prefix = &bytes[..separator];
                        if prefix.iter().any(|&byte| byte < 0x20 || byte == 0x7f)
                            || prefix.len() > MAX_HYPERLINK_PARAMS_BYTES - self.hyperlink_params_len
                        {
                            self.phase = Phase::Discard;
                            return;
                        }
                        let end = self.hyperlink_params_len + prefix.len();
                        self.hyperlink_params[self.hyperlink_params_len..end]
                            .copy_from_slice(prefix);
                        self.hyperlink_params_len = end;
                        self.phase = Phase::HyperlinkUri;
                        self.append_hyperlink_uri(&bytes[separator + 1..]);
                        return;
                    }
                    if bytes.iter().any(|&byte| byte < 0x20 || byte == 0x7f)
                        || bytes.len() > MAX_HYPERLINK_PARAMS_BYTES - self.hyperlink_params_len
                    {
                        self.phase = Phase::Discard;
                        return;
                    }
                    let end = self.hyperlink_params_len + bytes.len();
                    self.hyperlink_params[self.hyperlink_params_len..end].copy_from_slice(bytes);
                    self.hyperlink_params_len = end;
                    return;
                }
                Phase::Osc99Meta => {
                    if let Some(separator) = bytes.iter().position(|&byte| byte == b';') {
                        let prefix = &bytes[..separator];
                        if prefix.len() > MAX_OSC99_META_BYTES - self.osc99_meta_len {
                            self.phase = Phase::Discard;
                            return;
                        }
                        let end = self.osc99_meta_len + prefix.len();
                        self.osc99_meta[self.osc99_meta_len..end].copy_from_slice(prefix);
                        self.osc99_meta_len = end;
                        self.phase = Phase::Osc99Payload;
                        self.append_osc99_payload(&bytes[separator + 1..]);
                        return;
                    }
                    if bytes.len() > MAX_OSC99_META_BYTES - self.osc99_meta_len {
                        self.phase = Phase::Discard;
                        return;
                    }
                    let end = self.osc99_meta_len + bytes.len();
                    self.osc99_meta[self.osc99_meta_len..end].copy_from_slice(bytes);
                    self.osc99_meta_len = end;
                    return;
                }
                Phase::Osc777Verb => {
                    if let Some(separator) = bytes.iter().position(|&byte| byte == b';') {
                        let prefix = &bytes[..separator];
                        if prefix.len() > 6 - self.notify_stage_len {
                            self.phase = Phase::Discard;
                            return;
                        }
                        let end = self.notify_stage_len + prefix.len();
                        self.notify_stage[self.notify_stage_len..end].copy_from_slice(prefix);
                        self.notify_stage_len = end;
                        if &self.notify_stage[..self.notify_stage_len] != b"notify" {
                            self.phase = Phase::Discard;
                            return;
                        }
                        self.phase = Phase::Osc777Title;
                        bytes = &bytes[separator + 1..];
                        continue;
                    }
                    if bytes.len() > 6 - self.notify_stage_len {
                        self.phase = Phase::Discard;
                        return;
                    }
                    let end = self.notify_stage_len + bytes.len();
                    self.notify_stage[self.notify_stage_len..end].copy_from_slice(bytes);
                    self.notify_stage_len = end;
                    return;
                }
                Phase::Osc777Title => {
                    if let Some(separator) = bytes.iter().position(|&byte| byte == b';') {
                        let prefix = &bytes[..separator];
                        if prefix.len() > MAX_NOTIFICATION_TITLE_BYTES - self.notify_title_stage_len
                        {
                            self.phase = Phase::Discard;
                            return;
                        }
                        let end = self.notify_title_stage_len + prefix.len();
                        self.notify_title_stage[self.notify_title_stage_len..end]
                            .copy_from_slice(prefix);
                        self.notify_title_stage_len = end;
                        self.phase = Phase::Osc777Body;
                        self.append_osc777_body(&bytes[separator + 1..]);
                        return;
                    }
                    if bytes.len() > MAX_NOTIFICATION_TITLE_BYTES - self.notify_title_stage_len {
                        self.phase = Phase::Discard;
                        return;
                    }
                    let end = self.notify_title_stage_len + bytes.len();
                    self.notify_title_stage[self.notify_title_stage_len..end]
                        .copy_from_slice(bytes);
                    self.notify_title_stage_len = end;
                    return;
                }
                Phase::ClipboardTargets => {
                    let byte = bytes[0];
                    bytes = &bytes[1..];
                    match byte {
                        b';' => {
                            if !self.clipboard_target_seen {
                                self.clipboard_targets.clipboard = true;
                            }
                            self.phase = Phase::ClipboardData;
                            self.append_clipboard_data(bytes);
                            return;
                        }
                        b'c' => {
                            self.clipboard_targets.clipboard = true;
                            self.clipboard_target_seen = true;
                        }
                        b'p' => {
                            self.clipboard_targets.primary = true;
                            self.clipboard_target_seen = true;
                        }
                        b's' | b'q' | b'0'..=b'7' => {
                            self.clipboard_target_seen = true;
                        }
                        _ => {
                            self.phase = Phase::Discard;
                            return;
                        }
                    }
                }
                Phase::Index | Phase::ResetIndex => {
                    let byte = bytes[0];
                    bytes = &bytes[1..];
                    match byte {
                        b'0'..=b'9' if self.index_digits < 3 => {
                            self.index = self.index * 10 + u16::from(byte - b'0');
                            self.index_digits += 1;
                            if self.index > 255 {
                                self.phase = Phase::Discard;
                                return;
                            }
                        }
                        b';' => {
                            let Some(index) = self.finished_index() else {
                                self.phase = Phase::Discard;
                                return;
                            };
                            self.index = 0;
                            self.index_digits = 0;
                            if matches!(self.phase, Phase::ResetIndex) {
                                if !self.push(Operation::Reset(index)) {
                                    self.phase = Phase::Discard;
                                    return;
                                }
                            } else {
                                self.lengths[self.active ^ 1] = 0;
                                self.phase = Phase::IndexedColor(index);
                            }
                        }
                        _ => {
                            self.phase = Phase::Discard;
                            return;
                        }
                    }
                }
                phase @ (Phase::IndexedColor(_) | Phase::Color(_)) => {
                    let candidate = self.active ^ 1;
                    let offset = self.lengths[candidate];
                    if let [byte] = bytes
                        && *byte != b';'
                    {
                        if offset >= MAX_COLOR_BYTES {
                            self.phase = Phase::Discard;
                            return;
                        }
                        self.buffers[candidate][offset] = *byte;
                        self.lengths[candidate] += 1;
                        return;
                    }
                    let Some(capacity) = MAX_COLOR_BYTES.checked_sub(offset) else {
                        self.phase = Phase::Discard;
                        return;
                    };
                    let field = &bytes[..bytes.len().min(capacity + 1)];
                    let length = field
                        .iter()
                        .position(|&byte| byte == b';')
                        .unwrap_or(field.len());
                    if length > capacity {
                        self.phase = Phase::Discard;
                        return;
                    }
                    self.buffers[candidate][offset..offset + length]
                        .copy_from_slice(&bytes[..length]);
                    self.lengths[candidate] += length;
                    if length == bytes.len() {
                        return;
                    }
                    let target = match phase {
                        Phase::IndexedColor(index) => ColorTarget::Indexed(index),
                        Phase::Color(selector) => ColorTarget::Dynamic(selector),
                        _ => unreachable!("A color field is present"),
                    };
                    if !self.finish_color(target) {
                        self.phase = Phase::Discard;
                        return;
                    }
                    self.phase = match target {
                        ColorTarget::Indexed(_) => Phase::Index,
                        ColorTarget::Dynamic(selector) if selector < 12 => {
                            self.lengths[candidate] = 0;
                            Phase::Color(selector + 1)
                        }
                        ColorTarget::Dynamic(_) => Phase::Discard,
                    };
                    bytes = &bytes[length + 1..];
                }
                _ => return,
            }
        }
    }

    fn finished_index(&self) -> Option<u8> {
        (self.index_digits != 0).then_some(self.index as u8)
    }

    fn push(&mut self, operation: Operation) -> bool {
        if self.operation_count == self.operations.len() {
            return false;
        }
        self.operations[self.operation_count] = operation;
        self.operation_count += 1;
        true
    }

    fn finish_color(&mut self, target: ColorTarget) -> bool {
        let candidate = self.active ^ 1;
        let bytes = &self.buffers[candidate][..self.lengths[candidate]];
        let operation = if bytes == b"?" {
            match target {
                ColorTarget::Indexed(index) => Operation::Query(index),
                ColorTarget::Dynamic(selector) => Operation::QueryDynamic(selector),
            }
        } else if let Some(color) = parse_color(bytes) {
            match target {
                ColorTarget::Indexed(index) => Operation::Set(index, color),
                ColorTarget::Dynamic(selector) => Operation::SetDynamic(selector, color),
            }
        } else {
            return false;
        };
        self.push(operation)
    }

    #[inline(never)]
    fn execute_operations(&mut self, terminator: OscTerminator) -> Outcome {
        let mut replies = 0;
        for index in 0..self.operation_count {
            match self.operations[index] {
                Operation::Set(index, color) => {
                    self.palette_changed |= self
                        .palette
                        .set_override(PaletteEntry::Indexed(index), color)
                }
                Operation::SetDynamic(selector, color) => {
                    self.palette_changed |=
                        self.palette.set_override(role(u16::from(selector)), color)
                }
                Operation::Reset(index) => {
                    self.palette_changed |= self.palette.reset(PaletteEntry::Indexed(index))
                }
                Operation::Query(index) => {
                    let color = self.palette.color(PaletteEntry::Indexed(index));
                    // Captured replies compact into consumed operation slots.
                    self.operations[replies] = Operation::Reply(index, color);
                    replies += 1;
                }
                Operation::QueryDynamic(selector) => {
                    let color = self.palette.color(role(u16::from(selector)));
                    self.operations[replies] = Operation::ReplyDynamic(selector, color);
                    replies += 1;
                }
                Operation::Reply(..) | Operation::ReplyDynamic(..) => {
                    unreachable!("A command contains unexecuted operations")
                }
            }
        }
        self.operation_count = 0;
        self.reply_head = 0;
        self.reply_end = replies;
        self.terminator = terminator;
        if replies != 0 {
            Outcome::Replies
        } else {
            Outcome::Done
        }
    }

    #[inline]
    pub(crate) fn has_replies(&self) -> bool {
        self.reply_head < self.reply_end
    }

    #[inline]
    pub(crate) fn next_reply(&mut self) -> Option<ColorReport> {
        if !self.has_replies() {
            return None;
        }
        let (target, color) = match self.operations[self.reply_head] {
            Operation::Reply(index, color) => (ColorTarget::Indexed(index), color),
            Operation::ReplyDynamic(selector, color) => (ColorTarget::Dynamic(selector), color),
            _ => unreachable!("A captured color reply is present"),
        };
        self.reply_head += 1;
        Some(ColorReport {
            target,
            color,
            terminator: self.terminator,
        })
    }
}

fn role(selector: u16) -> PaletteEntry {
    match selector {
        10 => PaletteEntry::Foreground,
        11 => PaletteEntry::Background,
        12 => PaletteEntry::Cursor,
        _ => unreachable!("A supported dynamic color selector is present"),
    }
}

fn valid_directory_uri(bytes: &[u8]) -> bool {
    let Some((prefix, rest)) = bytes.split_at_checked(b"file://".len()) else {
        return false;
    };
    if !prefix.eq_ignore_ascii_case(b"file://") {
        return false;
    }
    let Some(path_start) = rest.iter().position(|&byte| byte == b'/') else {
        return false;
    };
    let (host, path) = rest.split_at(path_start);
    let valid_host = if let Some(address) = host.strip_prefix(b"[") {
        address
            .strip_suffix(b"]")
            .and_then(|address| std::str::from_utf8(address).ok())
            .is_some_and(|address| address.parse::<std::net::Ipv6Addr>().is_ok())
    } else {
        valid_uri_component(host, false)
    };
    valid_host && valid_uri_component(path, true)
}

fn valid_uri_component(mut bytes: &[u8], path: bool) -> bool {
    while let Some((&byte, rest)) = bytes.split_first() {
        bytes = match byte {
            b'%' => match rest {
                [high, low, remaining @ ..]
                    if high.is_ascii_hexdigit()
                        && low.is_ascii_hexdigit()
                        && (*high, *low) != (b'0', b'0') =>
                {
                    remaining
                }
                _ => return false,
            },
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'=' => rest,
            b'/' | b':' | b'@' if path => rest,
            _ => return false,
        };
    }
    true
}

fn parse_color(bytes: &[u8]) -> Option<Rgb> {
    // Sharp notation supplies high bits; rgb: scales each field to 16 bits.
    let channels = if let Some(hex) = bytes.strip_prefix(b"#") {
        let width = match hex.len() {
            3 => 1,
            6 => 2,
            9 => 3,
            12 => 4,
            _ => return None,
        };
        let mut channels = [0; 3];
        for (channel, digits) in channels.iter_mut().zip(hex.chunks_exact(width)) {
            *channel = (hex_value(digits)? << ((4 - width) * 4) >> 8) as u8;
        }
        channels
    } else if bytes.get(..4)?.eq_ignore_ascii_case(b"rgb:") {
        let mut parts = bytes[4..].split(|&byte| byte == b'/');
        let mut channels = [0; 3];
        for channel in &mut channels {
            let digits = parts.next()?;
            let value = u32::from(hex_value(digits)?);
            let maximum = (1_u32 << (4 * digits.len())) - 1;
            *channel = ((value * 65535 / maximum) >> 8) as u8;
        }
        if parts.next().is_some() {
            return None;
        }
        channels
    } else {
        return None;
    };
    Some(Rgb {
        red: channels[0],
        green: channels[1],
        blue: channels[2],
    })
}

fn hex_value(bytes: &[u8]) -> Option<u16> {
    if !(1..=4).contains(&bytes.len()) {
        return None;
    }
    let mut value = 0_u16;
    for &byte in bytes {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return None,
        };
        value = (value << 4) | u16::from(digit);
    }
    Some(value)
}

fn parse_progress(bytes: &[u8]) -> Option<Progress> {
    let (state_bytes, value_bytes) = match bytes.iter().position(|&b| b == b';') {
        Some(separator) => (&bytes[..separator], Some(&bytes[separator + 1..])),
        None => (bytes, None),
    };
    let state = parse_decimal_u16(state_bytes)?;
    let value = match value_bytes {
        None | Some([]) => 0u8,
        Some(digits) => parse_decimal_u16(digits)?.min(100) as u8,
    };
    match state {
        0 => Some(Progress::None),
        1 => Some(Progress::Normal(value)),
        2 => Some(Progress::Error(value)),
        3 => Some(Progress::Indeterminate),
        4 => Some(Progress::Paused(value)),
        _ => None,
    }
}

fn parse_decimal_u16(bytes: &[u8]) -> Option<u16> {
    if bytes.is_empty() || bytes.len() > 5 {
        return None;
    }
    let mut value = 0u16;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u16::from(byte - b'0'))?;
    }
    Some(value)
}

fn decode_base64_slice(input: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut quantum = [0u8; 4];
    let mut qlen = 0usize;
    let mut pad_seen = false;
    let mut out_len = 0usize;

    let mut push_slice = |bytes: &[u8]| -> Option<()> {
        if bytes.len() > out.len() - out_len {
            return None;
        }
        out[out_len..out_len + bytes.len()].copy_from_slice(bytes);
        out_len += bytes.len();
        Some(())
    };

    for &byte in input {
        if matches!(byte, b'\r' | b'\n' | b' ') {
            continue;
        }
        if byte == b'=' {
            if qlen < 2 {
                return None;
            }
            pad_seen = true;
            quantum[qlen] = BASE64_PAD;
            qlen += 1;
            if qlen == 4 {
                if quantum[2] == BASE64_PAD {
                    if quantum[3] != BASE64_PAD {
                        return None;
                    }
                    push_slice(&[(quantum[0] << 2) | (quantum[1] >> 4)])?;
                } else {
                    push_slice(&[
                        (quantum[0] << 2) | (quantum[1] >> 4),
                        (quantum[1] << 4) | (quantum[2] >> 2),
                    ])?;
                }
                qlen = 0;
            }
            continue;
        }
        if pad_seen {
            return None;
        }
        quantum[qlen] = base64_value(byte)?;
        qlen += 1;
        if qlen == 4 {
            push_slice(&[
                (quantum[0] << 2) | (quantum[1] >> 4),
                (quantum[1] << 4) | (quantum[2] >> 2),
                (quantum[2] << 6) | quantum[3],
            ])?;
            qlen = 0;
        }
    }
    match qlen {
        0 => Some(out_len),
        1 => None,
        2 if !pad_seen => {
            push_slice(&[(quantum[0] << 2) | (quantum[1] >> 4)])?;
            Some(out_len)
        }
        3 if !pad_seen => {
            push_slice(&[
                (quantum[0] << 2) | (quantum[1] >> 4),
                (quantum[1] << 4) | (quantum[2] >> 2),
            ])?;
            Some(out_len)
        }
        _ => None,
    }
}
