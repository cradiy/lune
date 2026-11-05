#[derive(Clone, Copy, Default)]
enum Charset {
    #[default]
    Ascii,
    SpecialGraphics,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct Charsets {
    g0: Charset,
    g1: Charset,
    use_g1: bool,
    graphics: bool,
}

impl Charsets {
    pub(crate) fn shift(&mut self, use_g1: bool) {
        self.use_g1 = use_g1;
        self.refresh();
    }

    pub(crate) fn designate(&mut self, g1: bool, final_byte: u8) -> bool {
        let charset = match final_byte {
            b'B' => Charset::Ascii,
            b'0' => Charset::SpecialGraphics,
            _ => return false,
        };
        if g1 {
            self.g1 = charset;
        } else {
            self.g0 = charset;
        }
        self.refresh();
        true
    }

    fn refresh(&mut self) {
        self.graphics = matches!(
            if self.use_g1 { self.g1 } else { self.g0 },
            Charset::SpecialGraphics
        );
    }

    #[inline]
    pub(crate) fn is_graphics(self) -> bool {
        self.graphics
    }

    #[inline]
    pub(crate) fn map(self, character: char) -> char {
        if self.graphics && ('_'..='~').contains(&character) {
            const GRAPHICS: [char; 32] = [
                '\u{a0}', '◆', '▒', '␉', '␌', '␍', '␊', '°', '±', '␤', '␋', '┘', '┐', '┌', '└',
                '┼', '⎺', '⎻', '─', '⎼', '⎽', '├', '┤', '┴', '┬', '│', '≤', '≥', 'π', '≠', '£',
                '·',
            ];
            GRAPHICS[character as usize - '_' as usize]
        } else {
            character
        }
    }
}
