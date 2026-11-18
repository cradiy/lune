use lune_vt::Parameter;

use crate::{Style, StyleError, StyleId, StyleTable};

const CAPACITY: usize = 10;
const ALL: u32 = u32::MAX;

#[derive(Clone, Copy, Default)]
struct SavedRendition {
    style: Style,
    fields: u32,
}

impl SavedRendition {
    fn restore(self, mut current: Style) -> Style {
        if self.fields == ALL {
            return self.style;
        }
        let selected = |code: u16| self.fields & (1_u32 << code) != 0_u32;
        if selected(1) {
            current.bold = self.style.bold;
        }
        if selected(2) {
            current.faint = self.style.faint;
        }
        if selected(3) {
            current.italic = self.style.italic;
        }
        if selected(4) {
            current.underline = self.style.underline;
        }
        if selected(5) {
            current.blink = self.style.blink;
        }
        if selected(7) {
            current.inverse = self.style.inverse;
        }
        if selected(8) {
            current.hidden = self.style.hidden;
        }
        if selected(9) {
            current.strikethrough = self.style.strikethrough;
        }
        if selected(30) {
            current.foreground = self.style.foreground;
        }
        if selected(31) {
            current.background = self.style.background;
        }
        current
    }
}

#[derive(Default)]
pub(crate) struct RenditionStack {
    saved: [SavedRendition; CAPACITY],
    depth: usize,
}

impl RenditionStack {
    pub(crate) fn clear(&mut self) {
        self.depth = 0;
    }

    pub(crate) fn push(&mut self, parameters: &[Parameter], style: Style) -> bool {
        let mut fields = if parameters.is_empty() { ALL } else { 0 };
        for parameter in parameters {
            let selector = match (parameter.is_subparameter, parameter.value) {
                (false, Some(code @ (1..=5 | 7..=9 | 30 | 31))) => code,
                (false, Some(21)) => 4,
                _ => return false,
            };
            fields |= 1 << selector;
        }
        if let Some(slot) = self.saved.get_mut(self.depth) {
            *slot = SavedRendition { style, fields };
        }
        // Count excess scopes so their pops leave the saved frames intact.
        self.depth = self.depth.saturating_add(1);
        true
    }

    #[cold]
    pub(crate) fn pop(
        &mut self,
        styles: &mut StyleTable,
        current: StyleId,
    ) -> Result<StyleId, StyleError> {
        let Some(index) = self.depth.checked_sub(1) else {
            return Ok(current);
        };
        let next = if let Some(frame) = self.saved.get(index) {
            let base = *styles.get(current).expect("Current rendition is rooted");
            styles.intern(frame.restore(base))?
        } else {
            current
        };
        self.depth = index;
        Ok(next)
    }
}
