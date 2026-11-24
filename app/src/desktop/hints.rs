//! Keyboard-driven quick selection (Hints) over visible terminal content.
//!
//! Scans the active viewport for URLs, explicit OSC 8 hyperlinks, file paths,
//! UUIDs, IPv4 addresses, and hex/git hashes, then assigns prefix-free home-row
//! labels for mouse-free copying, opening, or insertion.

use std::ops::Range;
use std::sync::OnceLock;

use lune_terminal::{
    CellPosition, CellText, HyperlinkId, HyperlinkTable, ScreenView, StyleTable, Viewport,
};
use regex::Regex;

use super::directory;

const ALPHABET: &[u8] = b"asdfghjklqwertyuiopzxcvbnm";
const MAX_HINTS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HintKind {
    Url,
    Path,
    Uuid,
    Ip,
    Hash,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Hint {
    pub(super) label: String,
    pub(super) kind: HintKind,
    pub(super) range: Range<CellPosition>,
    pub(super) text: String,
}

impl Hint {
    /// Returns the column span covered by this hint on chronological `row`.
    pub(super) fn columns_on_row(&self, row: usize, row_end: usize) -> Option<Range<usize>> {
        if row < self.range.start.row || row > self.range.end.row || row_end == 0 {
            return None;
        }
        let start = if row == self.range.start.row {
            self.range.start.column.min(row_end)
        } else {
            0
        };
        let end = if row == self.range.end.row {
            self.range.end.column.min(row_end)
        } else {
            row_end
        };
        (start < end).then_some(start..end)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum HintResolution {
    Pending,
    NoMatch,
    Matched {
        kind: HintKind,
        text: String,
        open_or_insert: bool,
    },
}

pub(super) struct HintMode {
    pub(super) hints: Vec<Hint>,
    pub(super) typed: String,
}

impl HintMode {
    /// Scans the visible viewport and builds a hint overlay. Returns `None` if
    /// no selectable targets exist in the viewport.
    pub(super) fn scan(
        viewport: Viewport<'_>,
        styles: &StyleTable,
        hyperlinks: &HyperlinkTable,
        columns: usize,
    ) -> Option<Self> {
        let view = viewport.source();
        let visible = viewport.range();
        let mut raw = Vec::new();
        collect_osc8_hyperlinks(view, visible.clone(), styles, hyperlinks, columns, &mut raw);
        collect_pattern_candidates(view, visible, columns, &mut raw);
        if raw.is_empty() {
            return None;
        }

        // Resolve overlaps: higher priority (lower rank), then longer span, then earlier start.
        raw.sort_by(|a, b| {
            a.priority
                .cmp(&b.priority)
                .then_with(|| b.text.len().cmp(&a.text.len()))
                .then_with(|| a.range.start.cmp(&b.range.start))
        });

        let mut accepted: Vec<Candidate> = Vec::new();
        for candidate in raw {
            if accepted
                .iter()
                .any(|existing| ranges_overlap(&existing.range, &candidate.range))
            {
                continue;
            }
            accepted.push(candidate);
            if accepted.len() >= MAX_HINTS {
                break;
            }
        }

        // Order bottom-to-top so the newest terminal output gets the easiest home-row keys.
        accepted.sort_by_key(|a| std::cmp::Reverse(a.range.start));

        let labels = generate_labels(accepted.len());
        let hints = accepted
            .into_iter()
            .zip(labels)
            .map(|(candidate, label)| Hint {
                label,
                kind: candidate.kind,
                range: candidate.range,
                text: candidate.text,
            })
            .collect();

        Some(Self {
            hints,
            typed: String::new(),
        })
    }

    /// Processes a typed label character. `shift` selects the secondary action
    /// (open URL or insert into terminal input) instead of copying.
    pub(super) fn push(&mut self, ch: char, shift: bool) -> HintResolution {
        let lower = ch.to_ascii_lowercase();
        if !lower.is_ascii_lowercase() {
            return HintResolution::Pending;
        }
        self.typed.push(lower);
        if let Some(exact) = self.hints.iter().find(|hint| hint.label == self.typed) {
            return HintResolution::Matched {
                kind: exact.kind,
                text: exact.text.clone(),
                open_or_insert: shift,
            };
        }
        if self
            .hints
            .iter()
            .any(|hint| hint.label.starts_with(&self.typed))
        {
            HintResolution::Pending
        } else {
            self.typed.pop();
            HintResolution::NoMatch
        }
    }

    pub(super) fn backspace(&mut self) -> bool {
        self.typed.pop().is_some()
    }

    pub(super) fn active_hints(&self) -> impl Iterator<Item = &Hint> {
        self.hints
            .iter()
            .filter(|hint| hint.label.starts_with(&self.typed))
    }
}

struct Candidate {
    priority: u8,
    kind: HintKind,
    range: Range<CellPosition>,
    text: String,
}

fn ranges_overlap(a: &Range<CellPosition>, b: &Range<CellPosition>) -> bool {
    a.start < b.end && b.start < a.end
}

fn collect_osc8_hyperlinks(
    view: ScreenView<'_>,
    visible: Range<usize>,
    styles: &StyleTable,
    hyperlinks: &HyperlinkTable,
    columns: usize,
    out: &mut Vec<Candidate>,
) {
    let mut active: Option<(HyperlinkId, CellPosition, CellPosition)> = None;
    let flush = |active: &mut Option<(HyperlinkId, CellPosition, CellPosition)>,
                 out: &mut Vec<Candidate>| {
        if let Some((id, start, end)) = active.take()
            && let Some(link) = hyperlinks.get(id)
        {
            let uri = link.uri();
            if directory::is_valid_hyperlink(uri) {
                out.push(Candidate {
                    priority: 0,
                    kind: HintKind::Url,
                    range: start..end,
                    text: uri.to_owned(),
                });
            }
        }
    };

    for row_idx in visible {
        let Some(row) = view.row(row_idx) else {
            flush(&mut active, out);
            continue;
        };
        let limit = row.wrap_end().unwrap_or(row.cells().len()).min(columns);
        for unit in row.display_units(0..limit) {
            let id = styles
                .get(unit.cell.style())
                .map(|style| style.hyperlink)
                .filter(|id| *id != HyperlinkId(0));
            let unit_start = CellPosition {
                row: row_idx,
                column: unit.column,
            };
            let unit_end = CellPosition {
                row: row_idx,
                column: (unit.column + unit.cell.width()).min(limit),
            };
            match (&mut active, id) {
                (Some((current_id, _, end)), Some(next_id)) if *current_id == next_id => {
                    *end = unit_end;
                }
                (_, Some(next_id)) => {
                    flush(&mut active, out);
                    active = Some((next_id, unit_start, unit_end));
                }
                (Some(_), None) => flush(&mut active, out),
                (None, None) => {}
            }
        }
        if row.wrap_end().is_none() {
            flush(&mut active, out);
        }
    }
    flush(&mut active, out);
}

struct CellSpan {
    byte_start: usize,
    byte_end: usize,
    row: usize,
    start_col: usize,
    end_col: usize,
}

struct Patterns {
    url: Regex,
    uuid: Regex,
    ipv4: Regex,
    path: Regex,
    hash: Regex,
}

fn patterns() -> &'static Patterns {
    static PATTERNS: OnceLock<Patterns> = OnceLock::new();
    PATTERNS.get_or_init(|| Patterns {
        url: Regex::new(r#"(?i)\b(?:https?://|file://|mailto:)[^\s<>"`{}|\\^]+"#).unwrap(),
        uuid: Regex::new(r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b")
            .unwrap(),
        ipv4: Regex::new(r"\b(?:\d{1,3}\.){3}\d{1,3}(?::\d{1,5})?\b").unwrap(),
        path: Regex::new(
            r#"(?:~|\.{1,2}|/[a-zA-Z0-9._@+-]+|[a-zA-Z0-9._-]+)/(?:[a-zA-Z0-9._@+-]+/)*[a-zA-Z0-9._@+-]+(?::\d+(?::\d+)?)?"#,
        )
        .unwrap(),
        hash: Regex::new(r"\b[0-9a-fA-F]{7,40}\b").unwrap(),
    })
}

fn collect_pattern_candidates(
    view: ScreenView<'_>,
    visible: Range<usize>,
    columns: usize,
    out: &mut Vec<Candidate>,
) {
    let pats = patterns();
    for logical in view.logical_lines() {
        let rows = logical.range();
        if rows.end <= visible.start || rows.start >= visible.end {
            continue;
        }
        let clipped_start = rows.start.max(visible.start);
        let clipped_end = rows.end.min(visible.end);

        let mut text = String::new();
        let mut spans: Vec<CellSpan> = Vec::new();
        for row_idx in clipped_start..clipped_end {
            let Some(row) = view.row(row_idx) else {
                continue;
            };
            let limit = row.wrap_end().unwrap_or(row.cells().len()).min(columns);
            let mut col = 0;
            for unit in row.display_units(0..limit) {
                if unit.column > col {
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
                spans.push(CellSpan {
                    byte_start,
                    byte_end,
                    row: row_idx,
                    start_col: unit.column,
                    end_col,
                });
                col = end_col;
            }
            if col < limit && row.wrap_end().is_some() {
                text.push(' ');
            }
        }
        if text.is_empty() || spans.is_empty() {
            continue;
        }

        // 1. URLs
        for m in pats.url.find_iter(&text) {
            let trimmed_len = trim_url_trailing(&text[m.start()..m.end()]);
            let end_byte = m.start() + trimmed_len;
            if end_byte <= m.start() {
                continue;
            }
            let uri = &text[m.start()..end_byte];
            if !directory::is_valid_hyperlink(uri) {
                continue;
            }
            if let Some(range) = span_range(&spans, m.start(), end_byte) {
                out.push(Candidate {
                    priority: 1,
                    kind: HintKind::Url,
                    range,
                    text: uri.to_owned(),
                });
            }
        }

        // 2. UUIDs
        for m in pats.uuid.find_iter(&text) {
            if let Some(range) = span_range(&spans, m.start(), m.end()) {
                out.push(Candidate {
                    priority: 2,
                    kind: HintKind::Uuid,
                    range,
                    text: m.as_str().to_owned(),
                });
            }
        }

        // 3. IPv4 (+ optional port)
        for m in pats.ipv4.find_iter(&text) {
            if is_valid_ipv4(m.as_str())
                && let Some(range) = span_range(&spans, m.start(), m.end())
            {
                out.push(Candidate {
                    priority: 3,
                    kind: HintKind::Ip,
                    range,
                    text: m.as_str().to_owned(),
                });
            }
        }

        // 4. File paths (with optional :line[:col])
        for m in pats.path.find_iter(&text) {
            // Avoid matching inside the middle of a word or URL scheme (`https://...`).
            if m.start() > 0 {
                let prev = text.as_bytes()[m.start() - 1];
                if prev.is_ascii_alphanumeric() || prev == b':' || prev == b'/' {
                    continue;
                }
            }
            let raw = m.as_str();
            let trimmed_len = trim_path_trailing(raw);
            let end_byte = m.start() + trimmed_len;
            if end_byte <= m.start() {
                continue;
            }
            let path = &text[m.start()..end_byte];
            if !path.contains('/') || path.len() < 3 {
                continue;
            }
            if let Some(range) = span_range(&spans, m.start(), end_byte) {
                out.push(Candidate {
                    priority: 4,
                    kind: HintKind::Path,
                    range,
                    text: path.to_owned(),
                });
            }
        }

        // 5. Git / hex hashes
        for m in pats.hash.find_iter(&text) {
            let s = m.as_str();
            if is_plausible_hash(s)
                && let Some(range) = span_range(&spans, m.start(), m.end())
            {
                out.push(Candidate {
                    priority: 5,
                    kind: HintKind::Hash,
                    range,
                    text: s.to_owned(),
                });
            }
        }
    }
}

fn span_range(
    spans: &[CellSpan],
    start_byte: usize,
    end_byte: usize,
) -> Option<Range<CellPosition>> {
    let start_idx = spans
        .binary_search_by_key(&start_byte, |span| span.byte_start)
        .ok()?;
    let end_idx = spans
        .binary_search_by_key(&end_byte, |span| span.byte_end)
        .ok()?;
    let start = &spans[start_idx];
    let end = &spans[end_idx];
    Some(
        CellPosition {
            row: start.row,
            column: start.start_col,
        }..CellPosition {
            row: end.row,
            column: end.end_col,
        },
    )
}

fn trim_url_trailing(candidate: &str) -> usize {
    let mut end = candidate.len();
    loop {
        let slice = &candidate[..end];
        let Some(last) = slice.chars().next_back() else {
            break;
        };
        let strip = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' => true,
            ')' => {
                slice.chars().filter(|&c| c == ')').count()
                    > slice.chars().filter(|&c| c == '(').count()
            }
            ']' => {
                slice.chars().filter(|&c| c == ']').count()
                    > slice.chars().filter(|&c| c == '[').count()
            }
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

fn trim_path_trailing(candidate: &str) -> usize {
    let mut end = candidate.len();
    while let Some(last) = candidate[..end].chars().next_back() {
        if matches!(last, '.' | ',' | ';' | ':' | ')' | ']') {
            end -= last.len_utf8();
        } else {
            break;
        }
    }
    end
}

fn is_valid_ipv4(text: &str) -> bool {
    let (host, port) = match text.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (text, None),
    };
    if let Some(port) = port
        && port.parse::<u16>().is_err()
    {
        return false;
    }
    let mut count = 0;
    for part in host.split('.') {
        if part.len() > 1 && part.starts_with('0') {
            return false;
        }
        if part.parse::<u8>().is_err() {
            return false;
        }
        count += 1;
    }
    count == 4
}

fn is_plausible_hash(s: &str) -> bool {
    let has_digit = s.bytes().any(|b| b.is_ascii_digit());
    let has_hex_letter = s.bytes().any(|b| matches!(b, b'a'..=b'f' | b'A'..=b'F'));
    has_digit && has_hex_letter
}

/// Generates `count` prefix-free labels using `ALPHABET` (home row first).
fn generate_labels(count: usize) -> Vec<String> {
    let n = ALPHABET.len();
    if count == 0 {
        return Vec::new();
    }
    if count <= n {
        return ALPHABET[..count]
            .iter()
            .map(|&b| (b as char).to_string())
            .collect();
    }
    // Reserve `two_prefixes` letters as prefixes of 2-letter codes so no 1-letter
    // label is ever a prefix of a 2-letter label.
    let overflow = count - n;
    let two_prefixes = overflow.div_ceil(n - 1).min(n);
    let single_count = n.saturating_sub(two_prefixes);

    let mut labels = Vec::with_capacity(count);
    for &b in &ALPHABET[..single_count] {
        labels.push((b as char).to_string());
    }
    for &first in &ALPHABET[single_count..] {
        for &second in ALPHABET {
            if labels.len() == count {
                return labels;
            }
            let mut label = String::with_capacity(2);
            label.push(first as char);
            label.push(second as char);
            labels.push(label);
        }
    }
    labels
}

#[cfg(test)]
mod tests {
    use lune_terminal::{MAX_STYLES, Processor, Screen, UnicodeWriter};

    use super::*;

    fn scan_text(columns: usize, rows: usize, text: &str) -> Option<HintMode> {
        let (palette, _) = super::super::theme::palette(winit::window::Theme::Dark);
        let mut processor = Processor::new(MAX_STYLES, palette).unwrap();
        let mut screen = Screen::new(columns, rows).unwrap();
        processor
            .advance(text.as_bytes(), &mut screen, &mut UnicodeWriter::default())
            .unwrap();
        HintMode::scan(
            screen.viewport(),
            processor.styles(),
            processor.hyperlinks(),
            columns,
        )
    }

    #[test]
    fn detects_urls_paths_ips_uuids_and_hashes_without_overlapping() {
        let content = "\
visit https://example.com/repo/src/main.rs today\r\n\
error at app/src/desktop/tab.rs:42:10\r\n\
host 192.168.1.42:8080 id 550e8400-e29b-41d4-a716-446655440000\r\n\
commit 91ff376 deadbeef01\r\n";
        let mode = scan_text(80, 6, content).unwrap();
        let texts: Vec<&str> = mode.hints.iter().map(|h| h.text.as_str()).collect();
        assert!(texts.contains(&"https://example.com/repo/src/main.rs"));
        assert!(texts.contains(&"app/src/desktop/tab.rs:42:10"));
        assert!(texts.contains(&"192.168.1.42:8080"));
        assert!(texts.contains(&"550e8400-e29b-41d4-a716-446655440000"));
        assert!(texts.contains(&"91ff376"));
        assert!(texts.contains(&"deadbeef01"));
        // URL sub-path must not produce a duplicate overlapping path hint.
        assert!(!texts.contains(&"example.com/repo/src/main.rs"));
    }

    #[test]
    fn labels_are_prefix_free_for_small_and_large_counts() {
        for count in [1, 10, 26, 27, 60, 200] {
            let labels = generate_labels(count);
            assert_eq!(labels.len(), count);
            for (i, a) in labels.iter().enumerate() {
                for (j, b) in labels.iter().enumerate() {
                    if i != j {
                        assert!(
                            !b.starts_with(a.as_str()),
                            "label {a} is a prefix of {b} at count={count}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn incremental_typing_resolves_and_supports_shift_action() {
        let mut mode = scan_text(80, 4, "open https://example.com now").unwrap();
        assert_eq!(mode.hints.len(), 1);
        let label = mode.hints[0].label.chars().next().unwrap();
        assert_eq!(
            mode.push(label, true),
            HintResolution::Matched {
                kind: HintKind::Url,
                text: "https://example.com".into(),
                open_or_insert: true,
            }
        );
    }
}
