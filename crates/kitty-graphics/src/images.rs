use std::collections::HashMap;

use super::{Command, Controls, MAX_DECODED_PAYLOAD_BYTES, Value};

#[derive(Debug)]
pub struct Image {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl Image {
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reply {
    pub id: u32,
    pub message: &'static [u8],
}

const INVALID: &[u8] = b"EINVAL:Invalid image data";
const UNSUPPORTED: &[u8] = b"ENOTSUP:Unsupported graphics command";
const QUOTA: &[u8] = b"ENOSPC:Image quota exceeded";

struct Transfer {
    id: u32,
    quiet: u32,
    query: bool,
    width: u32,
    height: u32,
    wire_size: usize,
    rgba_size: usize,
    bytes: Vec<u8>,
}

/// Receives direct, uncompressed RGB and RGBA data addressed by image ID.
pub struct DirectImages {
    max_bytes: usize,
    max_images: usize,
    stored_bytes: usize,
    images: HashMap<u32, Image>,
    pending: Option<Transfer>,
}

impl DirectImages {
    pub fn new(max_bytes: usize, max_images: usize) -> Self {
        Self {
            max_bytes,
            max_images,
            stored_bytes: 0,
            images: HashMap::new(),
            pending: None,
        }
    }

    pub fn get(&self, id: u32) -> Option<&Image> {
        self.images.get(&id)
    }

    pub fn abort_transfer(&mut self) {
        self.pending = None;
    }

    pub fn clear(&mut self) {
        self.abort_transfer();
        self.images.clear();
        self.stored_bytes = 0;
    }

    pub fn receive(&mut self, command: Command<'_>) -> Option<Reply> {
        let controls = &command.controls;
        let quiet = match controls.get(b'q') {
            None => Ok(None),
            Some(Value::Unsigned(value @ 0..=2)) => Ok(Some(value)),
            _ => Err(INVALID),
        };
        let mut transfer = if let Some(mut pending) = self.pending.take() {
            if controls.only(b"mq") {
                if let Ok(Some(quiet)) = quiet {
                    pending.quiet = quiet;
                }
                pending
            } else {
                return reply(pending.id, pending.quiet, Err(INVALID));
            }
        } else {
            let id = number(controls, b'i')?;
            let quiet = quiet.ok().flatten().unwrap_or_default();
            match self.begin(controls, id, quiet) {
                Ok(transfer) => transfer,
                Err(message) => return reply(id, quiet, Err(message)),
            }
        };

        let id = transfer.id;
        let quiet_value = transfer.quiet;
        let result = (|| {
            quiet?;
            let mut decoded = [0; MAX_DECODED_PAYLOAD_BYTES];
            let written = command.decode_payload(&mut decoded).map_err(|_| INVALID)?;
            if written > transfer.wire_size - transfer.bytes.len() {
                return Err(INVALID);
            }
            transfer.bytes.try_reserve(written).map_err(|_| QUOTA)?;
            transfer.bytes.extend_from_slice(&decoded[..written]);

            if controls.get(b'm') == Some(Value::Unsigned(1)) {
                self.pending = Some(transfer);
                return Ok(false);
            }
            if transfer.bytes.len() != transfer.wire_size {
                return Err(INVALID);
            }
            if !transfer.query {
                self.store(transfer)?;
            }
            Ok(true)
        })();
        match result {
            Ok(false) => None,
            Ok(true) => reply(id, quiet_value, Ok(())),
            Err(message) => reply(id, quiet_value, Err(message)),
        }
    }

    fn begin(&self, controls: &Controls, id: u32, quiet: u32) -> Result<Transfer, &'static [u8]> {
        if id == 0 || !controls.only(b"afimqstv") {
            return Err(UNSUPPORTED);
        }
        let query = match controls.get(b'a') {
            None | Some(Value::Letter(b't')) => false,
            Some(Value::Letter(b'q')) => true,
            _ => return Err(UNSUPPORTED),
        };
        if !matches!(controls.get(b't'), None | Some(Value::Letter(b'd'))) {
            return Err(UNSUPPORTED);
        }
        let channels = match controls.get(b'f') {
            Some(Value::Unsigned(24)) => 3,
            None | Some(Value::Unsigned(32)) => 4,
            _ => return Err(UNSUPPORTED),
        };
        let width = number(controls, b's')
            .filter(|&value| value != 0)
            .ok_or(INVALID)?;
        let height = number(controls, b'v')
            .filter(|&value| value != 0)
            .ok_or(INVALID)?;
        let pixels = (width as usize).checked_mul(height as usize).ok_or(QUOTA)?;
        let rgba_size = pixels
            .checked_mul(4)
            .filter(|&size| size <= self.max_bytes)
            .ok_or(QUOTA)?;
        Ok(Transfer {
            id,
            quiet,
            query,
            width,
            height,
            wire_size: pixels * channels,
            rgba_size,
            bytes: Vec::new(),
        })
    }

    fn store(&mut self, transfer: Transfer) -> Result<(), &'static [u8]> {
        let previous = self.images.get(&transfer.id);
        let previous_size = previous.map_or(0, |image| image.rgba.len());
        let remaining = self.stored_bytes - previous_size;
        if transfer.rgba_size > self.max_bytes - remaining {
            return Err(QUOTA);
        }
        if previous.is_none() {
            if self.images.len() >= self.max_images {
                return Err(QUOTA);
            }
            self.images.try_reserve(1).map_err(|_| QUOTA)?;
        }
        let rgba = if transfer.wire_size == transfer.rgba_size {
            transfer.bytes
        } else {
            let mut rgba = Vec::new();
            rgba.try_reserve_exact(transfer.rgba_size)
                .map_err(|_| QUOTA)?;
            for rgb in transfer.bytes.as_chunks::<3>().0 {
                rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
            }
            rgba
        };
        self.stored_bytes = remaining + rgba.len();
        self.images.insert(
            transfer.id,
            Image {
                width: transfer.width,
                height: transfer.height,
                rgba,
            },
        );
        Ok(())
    }
}

fn number(controls: &Controls, key: u8) -> Option<u32> {
    match controls.get(key) {
        Some(Value::Unsigned(value)) => Some(value),
        _ => None,
    }
}

fn reply(id: u32, quiet: u32, result: Result<(), &'static [u8]>) -> Option<Reply> {
    let message = match result {
        Ok(()) if quiet == 0 => b"OK",
        Err(message) if quiet != 2 => message,
        _ => return None,
    };
    (id != 0).then_some(Reply { id, message })
}
