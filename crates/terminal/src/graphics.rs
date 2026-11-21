use lune_kitty_graphics::{Decoder, DirectImages, Image, Reply};
use lune_vt::ApcEvent;

pub(crate) struct Graphics {
    decoder: Decoder,
    images: DirectImages,
    active: bool,
}

impl Default for Graphics {
    fn default() -> Self {
        Self {
            decoder: Decoder::default(),
            images: DirectImages::new(64 * 1024 * 1024, 1024),
            active: false,
        }
    }
}

impl Graphics {
    pub(crate) fn get(&self, id: u32) -> Option<&Image> {
        self.images.get(id)
    }

    pub(crate) fn reset(&mut self) {
        self.decoder.cancel();
        self.images.clear();
        self.active = false;
    }

    pub(crate) fn finish(&mut self) {
        self.decoder.cancel();
        self.images.abort_transfer();
        self.active = false;
    }

    pub(crate) fn needs_data(&self) -> bool {
        self.active
    }

    pub(crate) fn data(&mut self, bytes: &[u8]) {
        self.decoder.push(bytes);
    }

    #[cold]
    pub(crate) fn event(&mut self, event: ApcEvent<'_>) -> Option<Reply> {
        match event {
            ApcEvent::Start => {
                self.decoder.start();
                self.active = true;
            }
            ApcEvent::Data(bytes) => self.data(bytes),
            ApcEvent::End => {
                self.active = false;
                match self.decoder.finish() {
                    Some(Ok(command)) => return self.images.receive(command),
                    Some(Err(_)) => self.images.abort_transfer(),
                    None => {}
                }
            }
            ApcEvent::Cancel => self.finish(),
        }
        None
    }
}
