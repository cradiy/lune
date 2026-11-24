//! Fuzzy-searchable Command Palette supporting actions, live theme preview,
//! OSC 133 command history jumps, and runtime settings toggles.

use std::num::NonZeroU32;

use lune_terminal::{PixelPosition, PixelSize, ScreenView};
use winit::dpi::PhysicalPosition;

use crate::PixelRect;

use super::config::TerminalMode;
use super::font::FontMetrics;
use super::scrollbar::ScrollbarPolicy;
use super::shortcuts::{Action, Keymap, Scroll, Zoom};
use super::split::{Axis, Direction as SplitDirection};
use super::theme::Preference as ThemePreference;

pub(super) const MAX_VISIBLE_ROWS: usize = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PaletteAction {
    Shortcut(Action),
    SetMode(TerminalMode),
    ToggleCopyOnSelect,
    OpenThemes,
    ApplyTheme(String),
    OpenCommands,
    JumpToRow(usize),
    OpenScrollbar,
    SetScrollbar(ScrollbarPolicy),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PaletteEntry {
    pub(super) action: PaletteAction,
    pub(super) title: String,
    pub(super) detail: String,
    search_key: String,
}

impl PaletteEntry {
    fn new(action: PaletteAction, title: impl Into<String>, detail: impl Into<String>) -> Self {
        let title = title.into();
        let detail = detail.into();
        let search_key = title.to_ascii_lowercase();
        Self {
            action,
            title,
            detail,
            search_key,
        }
    }
}

struct SavedMenu {
    breadcrumb: Option<&'static str>,
    entries: Vec<PaletteEntry>,
    query: String,
    selected: usize,
    scroll: usize,
}

pub(super) struct PaletteContext<'a> {
    pub(super) keymap: &'a Keymap,
    pub(super) mode: TerminalMode,
    pub(super) scrollbar: ScrollbarPolicy,
    pub(super) copy_on_select: bool,
    pub(super) multi_tab: bool,
    pub(super) multi_pane: bool,
    pub(super) primary_screen: bool,
}

pub(super) struct CommandPalette {
    pub(super) query: String,
    pub(super) preedit: String,
    pub(super) breadcrumb: Option<&'static str>,
    entries: Vec<PaletteEntry>,
    filtered: Vec<usize>,
    pub(super) selected: usize,
    pub(super) hovered: Option<usize>,
    pub(super) scroll: usize,
    stack: Vec<SavedMenu>,
    pub(super) theme_restore: Option<ThemePreference>,
}

impl CommandPalette {
    pub(super) fn new(ctx: &PaletteContext<'_>) -> Self {
        let entries = build_root_entries(ctx);
        let mut palette = Self {
            query: String::new(),
            preedit: String::new(),
            breadcrumb: None,
            entries,
            filtered: Vec::new(),
            selected: 0,
            hovered: None,
            scroll: 0,
            stack: Vec::new(),
            theme_restore: None,
        };
        palette.refilter();
        palette
    }

    pub(super) fn push_submenu(
        &mut self,
        breadcrumb: &'static str,
        entries: Vec<PaletteEntry>,
        initial_selected: usize,
    ) {
        let saved = SavedMenu {
            breadcrumb: self.breadcrumb,
            entries: std::mem::replace(&mut self.entries, entries),
            query: std::mem::take(&mut self.query),
            selected: self.selected,
            scroll: self.scroll,
        };
        self.stack.push(saved);
        self.breadcrumb = Some(breadcrumb);
        self.preedit.clear();
        self.hovered = None;
        self.refilter();
        if !self.filtered.is_empty() {
            self.selected = initial_selected.min(self.filtered.len() - 1);
            self.ensure_selected_visible();
        }
    }

    pub(super) fn pop_submenu(&mut self) -> bool {
        let Some(saved) = self.stack.pop() else {
            return false;
        };
        self.breadcrumb = saved.breadcrumb;
        self.entries = saved.entries;
        self.query = saved.query;
        self.preedit.clear();
        self.hovered = None;
        self.refilter();
        if !self.filtered.is_empty() {
            self.selected = saved.selected.min(self.filtered.len() - 1);
            self.scroll = saved.scroll.min(self.max_scroll());
            self.ensure_selected_visible();
        }
        true
    }

    pub(super) fn in_theme_menu(&self) -> bool {
        self.breadcrumb == Some("Theme")
    }

    pub(super) fn total_count(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn match_count(&self) -> usize {
        self.filtered.len()
    }

    pub(super) fn visible_slice(&self) -> impl Iterator<Item = (usize, &PaletteEntry)> {
        let end = (self.scroll + MAX_VISIBLE_ROWS).min(self.filtered.len());
        (self.scroll..end)
            .map(|filtered_idx| (filtered_idx, &self.entries[self.filtered[filtered_idx]]))
    }

    pub(super) fn selected_entry(&self) -> Option<&PaletteEntry> {
        let index = *self.filtered.get(self.selected)?;
        self.entries.get(index)
    }

    pub(super) fn entry_at_visible(&self, visible_row: usize) -> Option<&PaletteEntry> {
        let filtered_idx = self.scroll.checked_add(visible_row)?;
        let index = *self.filtered.get(filtered_idx)?;
        self.entries.get(index)
    }

    pub(super) fn push_text(&mut self, text: &str) -> bool {
        let clean: String = text.chars().filter(|ch| !ch.is_control()).collect();
        if clean.is_empty() {
            return false;
        }
        self.query.push_str(&clean);
        self.refilter();
        true
    }

    pub(super) fn backspace(&mut self) -> bool {
        if self.query.pop().is_some() {
            self.refilter();
            true
        } else {
            false
        }
    }

    pub(super) fn delete_word(&mut self) -> bool {
        let trimmed = self.query.trim_end();
        let new_len = trimmed.rfind(char::is_whitespace).map_or(0, |idx| idx + 1);
        if new_len == self.query.len() {
            return false;
        }
        self.query.truncate(new_len);
        self.refilter();
        true
    }

    pub(super) fn clear_query(&mut self) -> bool {
        if self.query.is_empty() {
            return false;
        }
        self.query.clear();
        self.refilter();
        true
    }

    pub(super) fn step(&mut self, delta: isize) -> bool {
        let count = self.filtered.len();
        if count == 0 {
            return false;
        }
        let next = (self.selected as isize + delta).rem_euclid(count as isize) as usize;
        if next == self.selected {
            return false;
        }
        self.selected = next;
        self.ensure_selected_visible();
        true
    }

    pub(super) fn scroll_list(&mut self, delta: isize) -> bool {
        let max = self.max_scroll();
        if max == 0 {
            return false;
        }
        let next = (self.scroll as isize + delta).clamp(0, max as isize) as usize;
        if next == self.scroll {
            return false;
        }
        self.scroll = next;
        true
    }

    pub(super) fn select_visible(&mut self, visible_row: usize) -> bool {
        let idx = self.scroll + visible_row;
        if idx >= self.filtered.len() {
            return false;
        }
        self.selected = idx;
        self.ensure_selected_visible();
        true
    }

    fn max_scroll(&self) -> usize {
        self.filtered.len().saturating_sub(MAX_VISIBLE_ROWS)
    }

    fn ensure_selected_visible(&mut self) {
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + MAX_VISIBLE_ROWS {
            self.scroll = self.selected + 1 - MAX_VISIBLE_ROWS;
        }
        self.scroll = self.scroll.min(self.max_scroll());
    }

    fn refilter(&mut self) {
        let query = self.query.trim().to_ascii_lowercase();
        self.filtered.clear();
        if query.is_empty() {
            self.filtered.extend(0..self.entries.len());
        } else {
            let mut scored: Vec<(i32, usize)> = self
                .entries
                .iter()
                .enumerate()
                .filter_map(|(idx, entry)| fuzzy_score(&entry.search_key, &query).map(|s| (s, idx)))
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            self.filtered.extend(scored.into_iter().map(|(_, idx)| idx));
        }
        self.selected = 0;
        self.scroll = 0;
        self.hovered = None;
    }
}

pub(super) fn build_theme_entries(names: Vec<String>) -> Vec<PaletteEntry> {
    names
        .into_iter()
        .map(|name| {
            let detail = match name.as_str() {
                "system" => "Follow desktop",
                "dark" | "light" => "Built-in",
                _ => "Custom",
            };
            PaletteEntry::new(PaletteAction::ApplyTheme(name.clone()), name, detail)
        })
        .collect()
}

pub(super) fn build_scrollbar_entries(current: ScrollbarPolicy) -> (Vec<PaletteEntry>, usize) {
    let items = [
        (ScrollbarPolicy::Auto, "Auto (scroll or hover)"),
        (ScrollbarPolicy::Always, "Always visible"),
        (ScrollbarPolicy::Never, "Hidden"),
    ];
    let mut selected = 0;
    let entries = items
        .into_iter()
        .enumerate()
        .map(|(idx, (policy, title))| {
            let active = policy == current;
            if active {
                selected = idx;
            }
            PaletteEntry::new(
                PaletteAction::SetScrollbar(policy),
                title,
                if active { "Active" } else { "" },
            )
        })
        .collect();
    (entries, selected)
}

pub(super) fn build_command_history_entries(view: ScreenView<'_>) -> Vec<PaletteEntry> {
    let total = view.rows();
    let mut commands = Vec::new();
    for row_idx in (0..total).rev() {
        let Some(row) = view.row(row_idx) else {
            continue;
        };
        let semantic = row.semantic();
        if !semantic.prompt_start {
            continue;
        }
        let end_col = row.wrap_end().unwrap_or(row.cells().len());
        let Ok(range) = view.selection_text(
            lune_terminal::CellPosition {
                row: row_idx,
                column: 0,
            }..lune_terminal::CellPosition {
                row: row_idx,
                column: end_col,
            },
        ) else {
            continue;
        };
        let line = range.to_string();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let detail = match semantic.exit_code {
            Some(0) => "ok".to_owned(),
            Some(code) => format!("exit {code}"),
            None => format!("line {}", row_idx + 1),
        };
        commands.push(PaletteEntry::new(
            PaletteAction::JumpToRow(row_idx),
            trimmed.to_owned(),
            detail,
        ));
        if commands.len() >= 100 {
            break;
        }
    }
    commands
}

fn build_root_entries(ctx: &PaletteContext<'_>) -> Vec<PaletteEntry> {
    let mut entries = Vec::new();

    let (next_mode, mode_title) = match ctx.mode {
        TerminalMode::Classic => (TerminalMode::Modern, "Switch to Modern mode"),
        TerminalMode::Modern => (TerminalMode::Classic, "Switch to Classic mode"),
    };
    let mode_shortcut = ctx
        .keymap
        .shortcut_label(Action::ToggleMode)
        .unwrap_or_default();
    entries.push(PaletteEntry::new(
        PaletteAction::SetMode(next_mode),
        mode_title,
        mode_shortcut,
    ));

    entries.push(PaletteEntry::new(
        PaletteAction::OpenThemes,
        "Switch theme",
        ">",
    ));
    if ctx.primary_screen {
        entries.push(PaletteEntry::new(
            PaletteAction::OpenCommands,
            "Jump to command in scrollback",
            ">",
        ));
    }
    entries.push(PaletteEntry::new(
        PaletteAction::OpenScrollbar,
        "Scrollbar visibility",
        match ctx.scrollbar {
            ScrollbarPolicy::Auto => "Auto >",
            ScrollbarPolicy::Always => "Always >",
            ScrollbarPolicy::Never => "Hidden >",
        },
    ));
    entries.push(PaletteEntry::new(
        PaletteAction::ToggleCopyOnSelect,
        "Copy on select",
        if ctx.copy_on_select { "On" } else { "Off" },
    ));

    let actions: &[(Action, &str, bool)] = &[
        (Action::Search, "Search terminal", true),
        (Action::Hints, "Quick select (Hints)", true),
        (Action::ToggleVi, "Toggle Vi mode", true),
        (Action::Copy, "Copy selection", true),
        (Action::Paste, "Paste clipboard", true),
        (
            Action::CopyLastOutput,
            "Copy last command output",
            ctx.primary_screen,
        ),
        (
            Action::PreviousPrompt,
            "Jump to previous prompt",
            ctx.primary_screen,
        ),
        (
            Action::NextPrompt,
            "Jump to next prompt",
            ctx.primary_screen,
        ),
        (Action::SplitPane(Axis::Columns), "Split pane right", true),
        (Action::SplitPane(Axis::Rows), "Split pane down", true),
        (Action::TogglePaneZoom, "Toggle pane zoom", ctx.multi_pane),
        (
            Action::FocusPane(SplitDirection::Left),
            "Focus pane left",
            ctx.multi_pane,
        ),
        (
            Action::FocusPane(SplitDirection::Right),
            "Focus pane right",
            ctx.multi_pane,
        ),
        (
            Action::FocusPane(SplitDirection::Up),
            "Focus pane up",
            ctx.multi_pane,
        ),
        (
            Action::FocusPane(SplitDirection::Down),
            "Focus pane down",
            ctx.multi_pane,
        ),
        (Action::NewTab, "New tab", true),
        (Action::CloseTab, "Close pane or tab", true),
        (Action::NextTab, "Next tab", ctx.multi_tab),
        (Action::PreviousTab, "Previous tab", ctx.multi_tab),
        (Action::NewWindow, "New window", true),
        (Action::OpenDirectory, "Open working directory", true),
        (Action::Zoom(Zoom::In), "Increase font size", true),
        (Action::Zoom(Zoom::Out), "Decrease font size", true),
        (Action::Zoom(Zoom::Reset), "Reset font size", true),
        (
            Action::Scroll(Scroll::Oldest),
            "Scroll to top",
            ctx.primary_screen,
        ),
        (
            Action::Scroll(Scroll::Live),
            "Scroll to bottom",
            ctx.primary_screen,
        ),
        (Action::ToggleFullscreen, "Toggle fullscreen", true),
        (Action::ReloadConfig, "Reload configuration", true),
    ];

    for &(action, title, enabled) in actions {
        if !enabled {
            continue;
        }
        let shortcut = ctx.keymap.shortcut_label(action).unwrap_or_default();
        entries.push(PaletteEntry::new(
            PaletteAction::Shortcut(action),
            title,
            shortcut,
        ));
    }

    entries
}

/// Scores a case-insensitive subsequence match of `query` inside `haystack`.
/// Higher scores indicate stronger matches (prefix > word start > contiguous > sparse).
fn fuzzy_score(haystack: &str, query: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let h_chars: Vec<char> = haystack.chars().collect();
    let q_chars: Vec<char> = query.chars().collect();
    let mut score = 0i32;
    let mut h_idx = 0usize;
    let mut prev_match: Option<usize> = None;

    for (q_pos, &qc) in q_chars.iter().enumerate() {
        let mut found = None;
        while h_idx < h_chars.len() {
            if h_chars[h_idx] == qc {
                found = Some(h_idx);
                h_idx += 1;
                break;
            }
            h_idx += 1;
        }
        let pos = found?;
        score += 10;
        if pos == 0 && q_pos == 0 {
            score += 40;
        } else if pos > 0 && matches!(h_chars[pos - 1], ' ' | '-' | '_' | '/') {
            score += 25;
        }
        if prev_match.is_some_and(|prev| prev + 1 == pos) {
            score += 18;
        }
        prev_match = Some(pos);
    }
    // Strong preference for contiguous substring and prefix matches over scattered acronyms.
    if haystack.starts_with(query) {
        score += 80;
    } else if haystack.contains(query) {
        score += 50;
    }
    // Slight preference for shorter titles when match quality ties.
    score -= (h_chars.len() as i32) / 4;
    Some(score)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Layout {
    pub(super) card: PixelRect,
    pub(super) card_radius: u32,
    pub(super) input_origin: PixelPosition,
    pub(super) input_max_w: u32,
    pub(super) divider_y: u32,
    pub(super) list_top: u32,
    pub(super) row_height: u32,
    pub(super) row_pad_x: u32,
    pub(super) row_pad_y: u32,
    pub(super) visible_rows: usize,
}

impl Layout {
    pub(super) fn compute(
        area: PixelSize,
        metrics: FontMetrics,
        scale: f64,
        item_count: usize,
    ) -> Option<Self> {
        let scale = scale.max(1.0);
        let cell_w = metrics.cell.width.get();
        let cell_h = metrics.cell.height.get();
        let margin_x = (cell_w * 2).max(16);
        let top_margin = ((f64::from(area.height.get()) * 0.12).round() as u32)
            .clamp(cell_h, cell_h.saturating_mul(6).max(cell_h));
        let pad_x = ((f64::from(cell_w) * 1.5).round() as u32).max(12);
        let input_pad_y = ((f64::from(cell_h) * 0.55).round() as u32).max(8);
        let row_pad_y = ((f64::from(cell_h) * 0.30).round() as u32).max(4);
        let list_pad_y = ((f64::from(cell_h) * 0.35).round() as u32).max(6);
        let row_height = cell_h.saturating_add(row_pad_y * 2);

        let max_card_w = area.width.get().saturating_sub(margin_x * 2).min(960);
        let desired_w = ((600.0 * scale).round() as u32).max(cell_w.saturating_mul(48));
        let card_w = desired_w.min(max_card_w).min(area.width.get());
        if card_w < pad_x * 2 + cell_w * 4 {
            return None;
        }

        let input_h = cell_h.saturating_add(input_pad_y * 2);
        let avail_h = area
            .height
            .get()
            .saturating_sub(top_margin.saturating_add(margin_x));
        let max_rows_by_h = avail_h
            .saturating_sub(input_h.saturating_add(1).saturating_add(list_pad_y * 2))
            / row_height.max(1);
        let visible_rows = item_count
            .clamp(1, MAX_VISIBLE_ROWS)
            .min(max_rows_by_h.max(1) as usize);

        let card_h = input_h
            .saturating_add(1)
            .saturating_add(list_pad_y * 2)
            .saturating_add(row_height.saturating_mul(visible_rows as u32))
            .min(area.height.get());
        let card_x = area.width.get().saturating_sub(card_w) / 2;
        let card_y = top_margin.min(area.height.get().saturating_sub(card_h));

        let card = PixelRect {
            origin: PixelPosition {
                x: card_x,
                y: card_y,
            },
            size: PixelSize {
                width: NonZeroU32::new(card_w)?,
                height: NonZeroU32::new(card_h)?,
            },
        };
        let card_radius = ((8.0 * scale).round() as u32).clamp(6, 14).min(card_h / 2);
        let divider_y = card_y.saturating_add(input_h);
        let list_top = divider_y.saturating_add(1).saturating_add(list_pad_y);

        Some(Self {
            card,
            card_radius,
            input_origin: PixelPosition {
                x: card_x.saturating_add(pad_x),
                y: card_y.saturating_add(input_pad_y),
            },
            input_max_w: card_w.saturating_sub(pad_x * 2),
            divider_y,
            list_top,
            row_height,
            row_pad_x: pad_x,
            row_pad_y,
            visible_rows,
        })
    }

    pub(super) fn contains_card(&self, pos: PhysicalPosition<f64>) -> bool {
        let x = f64::from(self.card.origin.x);
        let y = f64::from(self.card.origin.y);
        pos.x >= x
            && pos.x < x + f64::from(self.card.size.width.get())
            && pos.y >= y
            && pos.y < y + f64::from(self.card.size.height.get())
    }

    pub(super) fn hit_row(&self, pos: PhysicalPosition<f64>, shown_rows: usize) -> Option<usize> {
        if !self.contains_card(pos) || shown_rows == 0 || self.row_height == 0 {
            return None;
        }
        let top = f64::from(self.list_top);
        if pos.y < top {
            return None;
        }
        let row = ((pos.y - top) / f64::from(self.row_height)).floor() as usize;
        (row < shown_rows.min(self.visible_rows)).then_some(row)
    }

    pub(super) fn row_rect(&self, visible_row: usize) -> Option<PixelRect> {
        let inset = (self.row_pad_x / 2).max(4);
        let width = self.card.size.width.get().saturating_sub(inset * 2);
        Some(PixelRect {
            origin: PixelPosition {
                x: self.card.origin.x.saturating_add(inset),
                y: self
                    .list_top
                    .saturating_add(self.row_height.saturating_mul(visible_row as u32)),
            },
            size: PixelSize {
                width: NonZeroU32::new(width)?,
                height: NonZeroU32::new(self.row_height)?,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use lune_terminal::PixelSize;
    use winit::dpi::PhysicalPosition;

    use super::super::config::TerminalMode;
    use super::super::font::FontMetrics;
    use super::super::scrollbar::ScrollbarPolicy;
    use super::super::shortcuts::Keymap;
    use super::{CommandPalette, Layout, PaletteContext, build_scrollbar_entries, fuzzy_score};

    #[test]
    fn fuzzy_scoring_prefers_prefix_and_word_boundary_matches() {
        let prefix = fuzzy_score("split pane right", "spl").unwrap();
        let middle = fuzzy_score("toggle split view", "spl").unwrap();
        let sparse = fuzzy_score("search previous line", "spl").unwrap();
        assert!(prefix > middle);
        assert!(middle > sparse);
        assert!(fuzzy_score("copy selection", "xyz").is_none());
    }

    #[test]
    fn palette_filters_navigates_and_restores_submenu_stack() {
        let keymap = Keymap::from_overrides(&[]);
        let ctx = PaletteContext {
            keymap: &keymap,
            mode: TerminalMode::Classic,
            scrollbar: ScrollbarPolicy::Auto,
            copy_on_select: false,
            multi_tab: true,
            multi_pane: true,
            primary_screen: true,
        };
        let mut palette = CommandPalette::new(&ctx);
        assert!(palette.match_count() > 10);

        palette.push_text("modern");
        assert_eq!(palette.match_count(), 1);
        assert_eq!(
            palette.selected_entry().unwrap().title,
            "Switch to Modern mode"
        );

        let (entries, selected) = build_scrollbar_entries(ScrollbarPolicy::Always);
        palette.push_submenu("Scrollbar", entries, selected);
        assert_eq!(palette.breadcrumb, Some("Scrollbar"));
        assert_eq!(palette.selected, 1);
        assert!(palette.step(1));
        assert_eq!(palette.selected, 2);
        assert!(palette.step(1));
        assert_eq!(palette.selected, 0);

        assert!(palette.pop_submenu());
        assert_eq!(palette.breadcrumb, None);
        assert_eq!(palette.query, "modern");
    }

    #[test]
    fn layout_computes_card_and_hits_rows_accurately() {
        let area = PixelSize {
            width: NonZeroU32::new(1000).unwrap(),
            height: NonZeroU32::new(700).unwrap(),
        };
        let metrics = FontMetrics {
            size: 16.0,
            cell: PixelSize {
                width: NonZeroU32::new(10).unwrap(),
                height: NonZeroU32::new(20).unwrap(),
            },
            baseline: 15,
        };
        let layout = Layout::compute(area, metrics, 1.0, 5).unwrap();
        let row0 = layout.row_rect(0).unwrap();
        let row2 = layout.row_rect(2).unwrap();
        assert_eq!(
            layout.hit_row(
                PhysicalPosition::new(f64::from(row0.origin.x + 10), f64::from(row0.origin.y + 4)),
                5,
            ),
            Some(0)
        );
        assert_eq!(
            layout.hit_row(
                PhysicalPosition::new(f64::from(row2.origin.x + 10), f64::from(row2.origin.y + 4)),
                5,
            ),
            Some(2)
        );
        assert_eq!(layout.hit_row(PhysicalPosition::new(0.0, 0.0), 5), None);
        assert!(!layout.contains_card(PhysicalPosition::new(0.0, 0.0)));
    }
}
