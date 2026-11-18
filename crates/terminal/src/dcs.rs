use lune_vt::DcsEvent;

#[derive(Clone, Copy)]
pub(crate) enum StatusQuery {
    Rendition,
    Margins,
    CursorStyle,
    Unsupported,
}

#[derive(Default)]
pub(crate) enum Dcs {
    #[default]
    Inactive,
    Empty,
    Rendition,
    Margins,
    Space,
    CursorStyle,
    Unsupported,
}

impl Dcs {
    #[inline]
    pub(crate) fn needs_data(&self) -> bool {
        !matches!(self, Self::Inactive | Self::Unsupported)
    }

    #[inline]
    pub(crate) fn data(&mut self, bytes: &[u8]) {
        *self = match (&*self, bytes) {
            (_, []) | (Self::Inactive | Self::Unsupported, _) => return,
            (Self::Empty, b"m") => Self::Rendition,
            (Self::Empty, b"r") => Self::Margins,
            (Self::Empty, b" ") => Self::Space,
            (Self::Empty, b" q") | (Self::Space, b"q") => Self::CursorStyle,
            _ => Self::Unsupported,
        };
    }

    #[inline(always)]
    pub(crate) fn event(&mut self, event: DcsEvent<'_>) -> Option<StatusQuery> {
        match event {
            DcsEvent::Start(header) => {
                *self = if header.final_byte == b'q'
                    && header.intermediates == b"$"
                    && header.private_markers.is_empty()
                    && header.parameters.is_empty()
                {
                    Self::Empty
                } else {
                    Self::Inactive
                };
            }
            DcsEvent::Data(bytes) => self.data(bytes),
            DcsEvent::Cancel => *self = Self::default(),
            DcsEvent::End => {
                return match std::mem::take(self) {
                    Self::Inactive => None,
                    Self::Rendition => Some(StatusQuery::Rendition),
                    Self::Margins => Some(StatusQuery::Margins),
                    Self::CursorStyle => Some(StatusQuery::CursorStyle),
                    _ => Some(StatusQuery::Unsupported),
                };
            }
        }
        None
    }
}
