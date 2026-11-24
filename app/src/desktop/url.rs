use std::ops::Range;

use lune_terminal::{CellPosition, CellText, HyperlinkId, ScreenView};

use super::directory;

const MAX_WRAP_SCAN_ROWS: usize = 16;
const SCHEMES: &[&str] = &["https://", "http://", "file://", "mailto:"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DetectedUrl {
    pub(super) range: Range<CellPosition>,
    pub(super) uri: String,
}

impl DetectedUrl {
    pub(super) fn columns_on_row(&self, abs_row: usize, row_end: usize) -> Option<Range<usize>> {
        if abs_row < self.range.start.row || abs_row > self.range.end.row {
            return None;
        }
        let start = if abs_row == self.range.start.row {
            self.range.start.column
        } else {
            0
        };
        let end = if abs_row == self.range.end.row {
            self.range.end.column
        } else {
            row_end
        };
        (start < end).then_some(start..end)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum HoveredLink {
    Explicit(HyperlinkId),
    Detected(DetectedUrl),
}

struct CellSpan {
    byte_start: usize,
    byte_end: usize,
    row: usize,
    start_col: usize,
    end_col: usize,
}

pub(super) fn detect_at(view: ScreenView<'_>, target: CellPosition) -> Option<DetectedUrl> {
    let target_row = view.row(target.row)?;
    let target_limit = target_row.wrap_end().unwrap_or(target_row.cells().len());
    if target.column >= target_limit {
        return None;
    }

    let mut first = target.row;
    let min_row = target.row.saturating_sub(MAX_WRAP_SCAN_ROWS);
    while first > min_row {
        let previous = first - 1;
        if view.row(previous)?.wrap_end().is_some() {
            first = previous;
        } else {
            break;
        }
    }

    let mut last = target.row;
    let max_row = (target.row + MAX_WRAP_SCAN_ROWS).min(view.rows().saturating_sub(1));
    while last < max_row && view.row(last)?.wrap_end().is_some() {
        last += 1;
    }

    let mut text = String::new();
    let mut spans = Vec::new();
    let mut target_byte = None;

    for row_index in first..=last {
        let row = view.row(row_index)?;
        let limit = row.wrap_end().unwrap_or(row.cells().len());
        let mut column = 0;
        for unit in row.display_units(0..limit) {
            if unit.column > column {
                // Gap between units (e.g. skipped placeholder) breaks a URL.
                text.push(' ');
            }
            let width = unit.cell.width();
            let end_col = (unit.column + width).min(limit);
            let byte_start = text.len();
            if unit.cell.is_erased() {
                text.push(' ');
            } else {
                match unit.text {
                    CellText::Scalar(ch) => text.push(ch),
                    CellText::Extended(cluster) => text.push_str(cluster),
                    CellText::Continuation => continue,
                }
            }
            let byte_end = text.len();
            if row_index == target.row && target.column >= unit.column && target.column < end_col {
                target_byte = Some(byte_start);
            }
            spans.push(CellSpan {
                byte_start,
                byte_end,
                row: row_index,
                start_col: unit.column,
                end_col,
            });
            column = end_col;
        }
        if column < limit && row.wrap_end().is_some() {
            text.push(' ');
        }
    }

    let target_byte = target_byte?;
    let (start_byte, end_byte) = find_url_span(&text, target_byte)?;
    let uri = text[start_byte..end_byte].to_owned();
    if !directory::is_valid_hyperlink(&uri) {
        return None;
    }

    let start_idx = spans
        .binary_search_by_key(&start_byte, |span| span.byte_start)
        .ok()?;
    let end_idx = spans
        .binary_search_by_key(&end_byte, |span| span.byte_end)
        .ok()?;
    let start_span = &spans[start_idx];
    let end_span = &spans[end_idx];

    Some(DetectedUrl {
        range: CellPosition {
            row: start_span.row,
            column: start_span.start_col,
        }..CellPosition {
            row: end_span.row,
            column: end_span.end_col,
        },
        uri,
    })
}

fn find_url_span(text: &str, target_byte: usize) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index <= target_byte && index < bytes.len() {
        let Some(scheme_len) = match_scheme(&bytes[index..]) else {
            index += 1;
            continue;
        };
        if index > 0 && bytes[index - 1].is_ascii_alphanumeric() {
            index += 1;
            continue;
        }
        let body_start = index + scheme_len;
        let mut end = body_start;
        for (offset, ch) in text[body_start..].char_indices() {
            if !is_url_char(ch) {
                break;
            }
            end = body_start + offset + ch.len_utf8();
        }
        let trimmed_end = trim_trailing_punctuation(&text[index..end]);
        let candidate_end = index + trimmed_end;
        if target_byte >= index && target_byte < candidate_end {
            return Some((index, candidate_end));
        }
        index = candidate_end.max(index + 1);
    }
    None
}

fn match_scheme(slice: &[u8]) -> Option<usize> {
    SCHEMES.iter().find_map(|scheme| {
        let len = scheme.len();
        slice
            .get(..len)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(scheme.as_bytes()))
            .then_some(len)
    })
}

fn is_url_char(ch: char) -> bool {
    !ch.is_whitespace()
        && !ch.is_control()
        && !matches!(ch, '<' | '>' | '"' | '`' | '{' | '}' | '|' | '\\' | '^')
}

fn trim_trailing_punctuation(candidate: &str) -> usize {
    let mut end = candidate.len();
    loop {
        let slice = &candidate[..end];
        let Some(last) = slice.chars().next_back() else {
            break;
        };
        let strip = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' => true,
            ')' => count_char(slice, ')') > count_char(slice, '('),
            ']' => count_char(slice, ']') > count_char(slice, '['),
            _ => false,
        };
        if strip {
            end -= last.len_utf8();
        } else {
            break;
        }
    }
    end
}

fn count_char(text: &str, needle: char) -> usize {
    text.chars().filter(|&ch| ch == needle).count()
}

#[cfg(test)]
mod tests {
    use super::detect_at;
    use lune_terminal::{CellPosition, MAX_STYLES, Processor, Screen, UnicodeWriter};

    fn feed_screen(columns: usize, rows: usize, text: &str) -> Screen {
        let (palette, _) = super::super::theme::palette(winit::window::Theme::Dark);
        let mut processor = Processor::new(MAX_STYLES, palette).unwrap();
        let mut screen = Screen::new(columns, rows).unwrap();
        let mut writer = UnicodeWriter::default();
        processor
            .advance(text.as_bytes(), &mut screen, &mut writer)
            .unwrap();
        screen
    }

    #[test]
    fn detects_url_and_trims_unbalanced_punctuation() {
        let screen = feed_screen(
            80,
            4,
            "See (https://example.com/a_(b)/c). and [http://example.org/x?y=1], ok",
        );
        let view = screen.view();
        let first = detect_at(view, CellPosition { row: 0, column: 10 }).unwrap();
        assert_eq!(first.uri, "https://example.com/a_(b)/c");
        assert_eq!(
            first.range,
            CellPosition { row: 0, column: 5 }..CellPosition { row: 0, column: 32 }
        );
        assert!(detect_at(view, CellPosition { row: 0, column: 32 }).is_none());

        let second = detect_at(view, CellPosition { row: 0, column: 45 }).unwrap();
        assert_eq!(second.uri, "http://example.org/x?y=1");
    }

    #[test]
    fn detects_soft_wrapped_url_across_rows() {
        let screen = feed_screen(20, 4, "url: https://example.com/path/to/resource ok");
        let view = screen.view();
        let on_first = detect_at(view, CellPosition { row: 0, column: 12 }).unwrap();
        let on_second = detect_at(view, CellPosition { row: 1, column: 5 }).unwrap();
        let on_third = detect_at(view, CellPosition { row: 2, column: 0 }).unwrap();
        assert_eq!(on_first, on_second);
        assert_eq!(on_second, on_third);
        assert_eq!(on_first.uri, "https://example.com/path/to/resource");
        assert_eq!(
            on_first.range,
            CellPosition { row: 0, column: 5 }..CellPosition { row: 2, column: 1 }
        );
        assert_eq!(on_first.columns_on_row(0, 20), Some(5..20));
        assert_eq!(on_first.columns_on_row(1, 20), Some(0..20));
        assert_eq!(on_first.columns_on_row(2, 20), Some(0..1));
        assert_eq!(on_first.columns_on_row(3, 20), None);
    }

    #[test]
    fn rejects_invalid_or_embedded_urls() {
        let screen = feed_screen(80, 4, "foohttps://example.com https:// javascript:alert(1)");
        let view = screen.view();
        assert!(detect_at(view, CellPosition { row: 0, column: 10 }).is_none());
        assert!(detect_at(view, CellPosition { row: 0, column: 25 }).is_none());
        assert!(detect_at(view, CellPosition { row: 0, column: 38 }).is_none());
    }
}
