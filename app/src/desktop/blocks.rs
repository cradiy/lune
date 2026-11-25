//! Modern-mode OSC 133 command block discovery, text extraction, and hover toolbar geometry.

use std::num::NonZeroU32;
use std::ops::Range;

use lune_terminal::{CellPosition, PixelPosition, PixelSize, ScreenView};

use crate::PixelRect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BlockAction {
    CopyCommand,
    CopyOutput,
    CopyFull,
    SelectOutput,
    ToggleBookmark,
    SearchInBlock,
    Rerun,
    JumpToPrompt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct HoveredBlock {
    pub(super) prompt_row: usize,
    pub(super) hovered_action: Option<BlockAction>,
    pub(super) on_sticky: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CommandBlock {
    /// Chronological row index of the `OSC 133;A` prompt boundary.
    pub(super) prompt_row: usize,
    /// Chronological row index of `OSC 133;C` command output start, if any.
    pub(super) output_row: Option<usize>,
    /// Exclusive chronological end row (next `prompt_row` or screen end).
    pub(super) end_row: usize,
    /// Column where `OSC 133;B` began the user's command input on `prompt_row`.
    pub(super) input_start_col: Option<usize>,
    /// Recorded command exit code (`None` while running or before execution).
    pub(super) exit_code: Option<u8>,
    /// Recorded command runtime in milliseconds.
    pub(super) duration_ms: Option<u32>,
    /// Whether the user bookmarked this command block.
    pub(super) bookmarked: bool,
}

impl CommandBlock {
    pub(super) fn is_finished(&self) -> bool {
        self.exit_code.is_some()
    }

    pub(super) fn is_running(&self) -> bool {
        self.output_row.is_some() && self.exit_code.is_none()
    }

    pub(super) fn is_error(&self) -> bool {
        self.exit_code.is_some_and(|code| code != 0)
    }

    pub(super) fn output_line_count(&self) -> usize {
        self.output_row
            .map_or(0, |start| self.end_row.saturating_sub(start))
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ToolbarButton {
    pub(super) action: BlockAction,
    pub(super) label: &'static str,
    pub(super) rect: PixelRect,
}

#[derive(Clone, Debug)]
pub(super) struct BlockToolbarLayout {
    pub(super) pill: PixelRect,
    pub(super) radius: u32,
    pub(super) buttons: Vec<ToolbarButton>,
}

impl BlockToolbarLayout {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn compute(
        pane_area: PixelSize,
        anchor_y: u32,
        ui_cell: PixelSize,
        scale: f64,
        scrollbar_reserve: u32,
        has_command: bool,
        has_output: bool,
    ) -> Option<Self> {
        if !has_command && !has_output {
            return None;
        }
        let scale = scale.max(1.0);
        let floor = |logical: f64| (logical * scale).round().max(1.0) as u32;
        let cell_w = ui_cell.width.get().max(1);
        let cell_h = ui_cell.height.get().max(1);

        let icon_size = (cell_h.saturating_mul(7) / 10).max(floor(11.0));
        let btn_pad_x = (cell_w * 3 / 4).max(floor(6.0));
        let icon_gap = (cell_w / 2).max(floor(4.0));
        let btn_h = cell_h.saturating_add(floor(6.0)).max(floor(22.0));
        let pill_pad = floor(3.0);
        let btn_gap = floor(3.0);
        let pill_h = btn_h.saturating_add(pill_pad * 2);

        let mut specs: Vec<(BlockAction, &'static str)> = Vec::with_capacity(3);
        if has_command {
            specs.push((BlockAction::CopyCommand, "Cmd"));
        }
        if has_output {
            specs.push((BlockAction::CopyOutput, "Out"));
        }
        if has_command {
            specs.push((BlockAction::Rerun, "Run"));
        }

        let mut total_btns_w = 0u32;
        let mut widths = Vec::with_capacity(specs.len());
        for (i, (_, label)) in specs.iter().enumerate() {
            let label_w = cell_w.saturating_mul(label.len() as u32);
            let w = btn_pad_x
                .saturating_mul(2)
                .saturating_add(icon_size)
                .saturating_add(icon_gap)
                .saturating_add(label_w);
            widths.push(w);
            total_btns_w = total_btns_w.saturating_add(w);
            if i + 1 < specs.len() {
                total_btns_w = total_btns_w.saturating_add(btn_gap);
            }
        }
        let pill_w = total_btns_w.saturating_add(pill_pad * 2);
        let margin = floor(8.0);
        let right_margin = scrollbar_reserve.saturating_add(margin);
        if pane_area.width.get() < pill_w.saturating_add(right_margin).saturating_add(margin)
            || pane_area.height.get() < pill_h.saturating_add(margin)
        {
            return None;
        }

        let pill_x = pane_area.width.get().saturating_sub(right_margin + pill_w);
        let max_y = pane_area.height.get().saturating_sub(pill_h + floor(2.0));
        let pill_y = anchor_y.clamp(floor(2.0), max_y.max(floor(2.0)));
        let pill = PixelRect {
            origin: PixelPosition {
                x: pill_x,
                y: pill_y,
            },
            size: PixelSize {
                width: NonZeroU32::new(pill_w)?,
                height: NonZeroU32::new(pill_h)?,
            },
        };

        let mut cursor_x = pill_x.saturating_add(pill_pad);
        let btn_y = pill_y.saturating_add(pill_pad);
        let mut buttons = Vec::with_capacity(specs.len());
        for ((action, label), w) in specs.into_iter().zip(widths) {
            if let (Some(w_nz), Some(h_nz)) = (NonZeroU32::new(w), NonZeroU32::new(btn_h)) {
                buttons.push(ToolbarButton {
                    action,
                    label,
                    rect: PixelRect {
                        origin: PixelPosition {
                            x: cursor_x,
                            y: btn_y,
                        },
                        size: PixelSize {
                            width: w_nz,
                            height: h_nz,
                        },
                    },
                });
            }
            cursor_x = cursor_x.saturating_add(w).saturating_add(btn_gap);
        }

        Some(Self {
            pill,
            radius: (pill_h / 3).clamp(floor(6.0), pill_h / 2),
            buttons,
        })
    }

    pub(super) fn hit(&self, local_x: f64, local_y: f64) -> Option<BlockAction> {
        for button in &self.buttons {
            let x = f64::from(button.rect.origin.x);
            let y = f64::from(button.rect.origin.y);
            if local_x >= x
                && local_x < x + f64::from(button.rect.size.width.get())
                && local_y >= y
                && local_y < y + f64::from(button.rect.size.height.get())
            {
                return Some(button.action);
            }
        }
        None
    }
}

#[derive(Clone, Debug)]
pub(super) struct StickyHeaderLayout {
    pub(super) bar: PixelRect,
    pub(super) radius: u32,
    pub(super) accent_bar: PixelRect,
    pub(super) text_x: u32,
    pub(super) text_max_w: u32,
    pub(super) buttons: Vec<ToolbarButton>,
}

impl StickyHeaderLayout {
    pub(super) fn compute(
        pane_area: PixelSize,
        ui_cell: PixelSize,
        scale: f64,
        scrollbar_reserve: u32,
        has_command: bool,
        has_output: bool,
    ) -> Option<Self> {
        if !has_command {
            return None;
        }
        let scale = scale.max(1.0);
        let floor = |logical: f64| (logical * scale).round().max(1.0) as u32;
        let cell_w = ui_cell.width.get().max(1);
        let cell_h = ui_cell.height.get().max(1);

        let margin_x = floor(6.0);
        let margin_y = floor(4.0);
        let right_margin = scrollbar_reserve.saturating_add(margin_x);
        let bar_w = pane_area
            .width
            .get()
            .saturating_sub(margin_x.saturating_add(right_margin));
        let pad_y = floor(4.0);
        let pad_x = floor(8.0);
        let btn_h = cell_h.saturating_add(floor(4.0)).max(floor(20.0));
        let bar_h = btn_h.saturating_add(pad_y * 2);
        if bar_w < cell_w.saturating_mul(16) || pane_area.height.get() < bar_h.saturating_mul(3) {
            return None;
        }

        let bar = PixelRect {
            origin: PixelPosition {
                x: margin_x,
                y: margin_y,
            },
            size: PixelSize {
                width: NonZeroU32::new(bar_w)?,
                height: NonZeroU32::new(bar_h)?,
            },
        };

        let accent_w = floor(3.0).clamp(2, 4);
        let accent_h = bar_h.saturating_sub(pad_y * 2).max(6);
        let accent_bar = PixelRect {
            origin: PixelPosition {
                x: bar.origin.x.saturating_add(pad_x),
                y: bar.origin.y + bar_h.saturating_sub(accent_h) / 2,
            },
            size: PixelSize {
                width: NonZeroU32::new(accent_w)?,
                height: NonZeroU32::new(accent_h)?,
            },
        };

        let icon_size = (cell_h.saturating_mul(7) / 10).max(floor(11.0));
        let btn_pad_x = (cell_w * 2 / 3).max(floor(5.0));
        let icon_gap = (cell_w / 2).max(floor(4.0));
        let btn_gap = floor(3.0);

        let mut specs: Vec<(BlockAction, &'static str)> = Vec::with_capacity(3);
        specs.push((BlockAction::CopyCommand, "Cmd"));
        if has_output {
            specs.push((BlockAction::CopyOutput, "Out"));
        }
        specs.push((BlockAction::Rerun, "Run"));

        let mut widths = Vec::with_capacity(specs.len());
        let mut total_btns_w = 0u32;
        for (i, (_, label)) in specs.iter().enumerate() {
            let label_w = cell_w.saturating_mul(label.len() as u32);
            let w = btn_pad_x
                .saturating_mul(2)
                .saturating_add(icon_size)
                .saturating_add(icon_gap)
                .saturating_add(label_w);
            widths.push(w);
            total_btns_w = total_btns_w.saturating_add(w);
            if i + 1 < specs.len() {
                total_btns_w = total_btns_w.saturating_add(btn_gap);
            }
        }

        let text_x = accent_bar
            .origin
            .x
            .saturating_add(accent_w)
            .saturating_add(pad_x);
        let buttons_gap = floor(10.0);
        let buttons_start_x = bar
            .origin
            .x
            .saturating_add(bar_w)
            .saturating_sub(pad_x.saturating_add(total_btns_w));

        let mut buttons = Vec::with_capacity(specs.len());
        let text_max_w = if buttons_start_x > text_x.saturating_add(cell_w * 8 + buttons_gap) {
            let mut cursor_x = buttons_start_x;
            let btn_y = bar.origin.y + bar_h.saturating_sub(btn_h) / 2;
            for ((action, label), w) in specs.into_iter().zip(widths) {
                if let (Some(w_nz), Some(h_nz)) = (NonZeroU32::new(w), NonZeroU32::new(btn_h)) {
                    buttons.push(ToolbarButton {
                        action,
                        label,
                        rect: PixelRect {
                            origin: PixelPosition {
                                x: cursor_x,
                                y: btn_y,
                            },
                            size: PixelSize {
                                width: w_nz,
                                height: h_nz,
                            },
                        },
                    });
                }
                cursor_x = cursor_x.saturating_add(w).saturating_add(btn_gap);
            }
            buttons_start_x.saturating_sub(text_x.saturating_add(buttons_gap))
        } else {
            bar.origin
                .x
                .saturating_add(bar_w)
                .saturating_sub(text_x.saturating_add(pad_x))
        };

        Some(Self {
            bar,
            radius: (bar_h / 4).clamp(floor(6.0), bar_h / 2),
            accent_bar,
            text_x,
            text_max_w,
            buttons,
        })
    }

    pub(super) fn contains(&self, local_x: f64, local_y: f64) -> bool {
        let x = f64::from(self.bar.origin.x);
        let y = f64::from(self.bar.origin.y);
        local_x >= x
            && local_x < x + f64::from(self.bar.size.width.get())
            && local_y >= y
            && local_y < y + f64::from(self.bar.size.height.get())
    }

    pub(super) fn hit(&self, local_x: f64, local_y: f64) -> Option<BlockAction> {
        if !self.contains(local_x, local_y) {
            return None;
        }
        for button in &self.buttons {
            let x = f64::from(button.rect.origin.x);
            let y = f64::from(button.rect.origin.y);
            if local_x >= x
                && local_x < x + f64::from(button.rect.size.width.get())
                && local_y >= y
                && local_y < y + f64::from(button.rect.size.height.get())
            {
                return Some(button.action);
            }
        }
        Some(BlockAction::JumpToPrompt)
    }
}

/// Returns the block eligible for a pinned sticky command header at the top of the pane:
/// its prompt has scrolled above `first_row` and at least two output rows remain visible.
pub(super) fn sticky_block(blocks: &[CommandBlock], first_row: usize) -> Option<CommandBlock> {
    let first = blocks.first().copied()?;
    (first.prompt_row < first_row && first.end_row >= first_row + 2).then_some(first)
}

/// Finds the nearest earlier `OSC 133;A` prompt row whose recorded exit code is non-zero.
pub(super) fn previous_failed_prompt(view: ScreenView<'_>, before_row: usize) -> Option<usize> {
    let mut cursor = before_row;
    while let Some(prompt_row) = view.previous_prompt(cursor) {
        if view
            .row(prompt_row)
            .is_some_and(|r| r.semantic().exit_code.is_some_and(|code| code != 0))
        {
            return Some(prompt_row);
        }
        cursor = prompt_row;
    }
    None
}

/// Finds the nearest earlier `OSC 133;A` prompt row that is bookmarked.
pub(super) fn previous_bookmarked_prompt(view: ScreenView<'_>, before_row: usize) -> Option<usize> {
    let mut cursor = before_row;
    while let Some(prompt_row) = view.previous_prompt(cursor) {
        if view
            .row(prompt_row)
            .is_some_and(|r| r.semantic().bookmarked)
        {
            return Some(prompt_row);
        }
        cursor = prompt_row;
    }
    None
}

/// Finds the nearest later `OSC 133;A` prompt row that is bookmarked.
pub(super) fn next_bookmarked_prompt(view: ScreenView<'_>, after_row: usize) -> Option<usize> {
    let mut cursor = after_row;
    while let Some(prompt_row) = view.next_prompt(cursor) {
        if view
            .row(prompt_row)
            .is_some_and(|r| r.semantic().bookmarked)
        {
            return Some(prompt_row);
        }
        cursor = prompt_row;
    }
    None
}

/// Resolves the command block targeted by keyboard block actions:
/// when scrolled into history, prefers the block currently anchored at `first_row`;
/// otherwise returns the most recent block with command input or output.
pub(super) fn active_target_block(
    view: ScreenView<'_>,
    first_row: usize,
    offset: usize,
) -> Option<CommandBlock> {
    if offset > 0
        && let Some(block) = block_at_row(view, first_row)
        && (has_command(view, &block) || has_output(view, &block))
    {
        return Some(block);
    }
    let total = view.rows();
    if total == 0 {
        return None;
    }
    let mut cursor = total;
    while let Some(prompt_row) = view.previous_prompt(cursor) {
        if let Some(block) = block_at_row(view, prompt_row)
            && (has_command(view, &block) || has_output(view, &block))
        {
            return Some(block);
        }
        cursor = prompt_row;
    }
    None
}

/// Finds all OSC 133 command blocks intersecting `visible_rows`.
pub(super) fn blocks_in_viewport(
    view: ScreenView<'_>,
    visible_rows: Range<usize>,
) -> Vec<CommandBlock> {
    let total = view.rows();
    if total == 0 || visible_rows.start >= visible_rows.end {
        return Vec::new();
    }
    let vis_start = visible_rows.start.min(total);
    let vis_end = visible_rows.end.min(total);

    let first_prompt = if view
        .row(vis_start)
        .is_some_and(|r| r.semantic().prompt_start)
    {
        Some(vis_start)
    } else {
        view.previous_prompt(vis_start)
    };

    let mut prompt_rows = Vec::new();
    if let Some(p) = first_prompt {
        prompt_rows.push(p);
    }
    for row in vis_start..vis_end {
        if Some(row) == first_prompt {
            continue;
        }
        if view.row(row).is_some_and(|r| r.semantic().prompt_start) {
            prompt_rows.push(row);
        }
    }

    let mut blocks = Vec::with_capacity(prompt_rows.len());
    for (idx, &prompt_row) in prompt_rows.iter().enumerate() {
        let next_prompt = prompt_rows
            .get(idx + 1)
            .copied()
            .or_else(|| view.next_prompt(prompt_row));
        let raw_end = next_prompt.unwrap_or(total);
        let block = build_block(view, prompt_row, raw_end, next_prompt.is_none());
        if block.end_row > vis_start && block.prompt_row < vis_end {
            blocks.push(block);
        }
    }
    blocks
}

/// Finds the OSC 133 command block containing `row`, if any.
pub(super) fn block_at_row(view: ScreenView<'_>, row: usize) -> Option<CommandBlock> {
    if row >= view.rows() {
        return None;
    }
    let prompt_row = if view.row(row)?.semantic().prompt_start {
        row
    } else {
        view.previous_prompt(row)?
    };
    let next_prompt = view.next_prompt(prompt_row);
    let raw_end = next_prompt.unwrap_or(view.rows());
    let block = build_block(view, prompt_row, raw_end, next_prompt.is_none());
    (row >= block.prompt_row && row < block.end_row).then_some(block)
}

/// Finds the most recent block that has an extractable command.
pub(super) fn last_command_text(view: ScreenView<'_>) -> Option<String> {
    let mut cursor = view.rows();
    while let Some(prompt_row) = view.previous_prompt(cursor) {
        let next_prompt = view.next_prompt(prompt_row);
        let block = build_block(
            view,
            prompt_row,
            next_prompt.unwrap_or(view.rows()),
            next_prompt.is_none(),
        );
        if let Some(cmd) = extract_command(view, &block) {
            return Some(cmd);
        }
        cursor = prompt_row;
    }
    None
}

fn build_block(
    view: ScreenView<'_>,
    prompt_row: usize,
    raw_end: usize,
    is_tail: bool,
) -> CommandBlock {
    let semantic = view
        .row(prompt_row)
        .map(|row| row.semantic())
        .unwrap_or_default();
    let mut input_start_col = semantic.input_start_col.map(usize::from);
    let mut output_row = None;

    for r in prompt_row..raw_end {
        let Some(row) = view.row(r) else {
            continue;
        };
        let sem = row.semantic();
        if input_start_col.is_none()
            && r == prompt_row
            && let Some(col) = sem.input_start_col
        {
            input_start_col = Some(usize::from(col));
        }
        if output_row.is_none() && sem.output_start {
            output_row = Some(r);
            break;
        }
    }

    let mut end_row = raw_end;
    if is_tail {
        // Trim trailing completely empty rows below the active/last block so the rail ends cleanly.
        while end_row > prompt_row + 1 && row_is_blank(view, end_row - 1) {
            end_row -= 1;
        }
    }

    CommandBlock {
        prompt_row,
        output_row,
        end_row: end_row.max(prompt_row + 1),
        input_start_col,
        exit_code: semantic.exit_code,
        duration_ms: semantic.duration_ms,
        bookmarked: semantic.bookmarked,
    }
}

fn row_has_non_whitespace(row: &lune_terminal::Row, start_col: usize, end_col: usize) -> bool {
    let cells = row.cells();
    let start = start_col.min(cells.len());
    let end = end_col.min(cells.len());
    for (offset, cell) in cells[start..end].iter().enumerate() {
        if cell.is_erased() || cell.is_continuation() {
            continue;
        }
        if cell.has_extended_text() {
            if row.text(start + offset).is_some_and(|text| match text {
                lune_terminal::CellText::Extended(s) => !s.trim().is_empty(),
                lune_terminal::CellText::Scalar(ch) => !ch.is_whitespace(),
                lune_terminal::CellText::Continuation => false,
            }) {
                return true;
            }
        } else if cell.character().is_some_and(|ch| !ch.is_whitespace()) {
            return true;
        }
    }
    false
}

fn row_is_blank(view: ScreenView<'_>, row_idx: usize) -> bool {
    let Some(row) = view.row(row_idx) else {
        return true;
    };
    let end_col = row.wrap_end().unwrap_or(row.cells().len());
    !row_has_non_whitespace(row, 0, end_col)
}

/// Fast zero-allocation check for whether a command block has non-empty command input.
pub(super) fn has_command(view: ScreenView<'_>, block: &CommandBlock) -> bool {
    let Some(start_col) = block.input_start_col else {
        return false;
    };
    let last_input_row = match block.output_row {
        Some(out) if out > block.prompt_row => out - 1,
        _ => block.prompt_row,
    }
    .min(view.rows().saturating_sub(1));
    for r in block.prompt_row..=last_input_row {
        let Some(row) = view.row(r) else {
            continue;
        };
        let col0 = if r == block.prompt_row { start_col } else { 0 };
        let end_col = row.wrap_end().unwrap_or(row.cells().len());
        if row_has_non_whitespace(row, col0, end_col) {
            return true;
        }
    }
    false
}

/// Fast zero-allocation check for whether a command block has non-empty output rows.
pub(super) fn has_output(view: ScreenView<'_>, block: &CommandBlock) -> bool {
    let Some(start_row) = block.output_row else {
        return false;
    };
    let end_row = block.end_row.min(view.rows());
    for r in start_row..end_row {
        if !row_is_blank(view, r) {
            return true;
        }
    }
    false
}

/// Extracts the exact command input text (`OSC 133;B` .. `OSC 133;C`) for a block.
pub(super) fn extract_command(view: ScreenView<'_>, block: &CommandBlock) -> Option<String> {
    let start_col = block.input_start_col?;
    let prompt_line = view.row(block.prompt_row)?;
    let start_col = start_col.min(prompt_line.cells().len());
    let last_input_row = match block.output_row {
        Some(out) if out > block.prompt_row => out - 1,
        _ => block.prompt_row,
    }
    .min(view.rows().saturating_sub(1));
    let end_col = view.row(last_input_row)?.cells().len();
    let start = CellPosition {
        row: block.prompt_row,
        column: start_col,
    };
    let end = CellPosition {
        row: last_input_row,
        column: end_col,
    };
    if start >= end {
        return None;
    }
    let text = view.selection_text(start..end).ok()?.to_string();
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Returns the tight cell selection range covering a block's non-empty output rows.
pub(super) fn output_selection_range(
    view: ScreenView<'_>,
    block: &CommandBlock,
) -> Option<Range<CellPosition>> {
    let start_row = block.output_row?;
    if block.end_row <= start_row {
        return None;
    }
    let end_row = (block.end_row - 1).min(view.rows().saturating_sub(1));
    let end_col = view.row(end_row)?.cells().len();
    view.selection_bounds(
        CellPosition {
            row: start_row,
            column: 0,
        }..CellPosition {
            row: end_row,
            column: end_col,
        },
    )
}

/// Extracts the command output text (`OSC 133;C` .. next `OSC 133;A`) for a block.
pub(super) fn extract_output(view: ScreenView<'_>, block: &CommandBlock) -> Option<String> {
    let bounds = output_selection_range(view, block)?;
    let text = view.selection_text(bounds).ok()?.to_string();
    (!text.trim().is_empty()).then_some(text)
}

/// Extracts a formatted transcript (`$ <command>\n<output>`) for a command block.
pub(super) fn extract_block_full(view: ScreenView<'_>, block: &CommandBlock) -> Option<String> {
    let cmd = extract_command(view, block);
    let output = extract_output(view, block);
    match (cmd, output) {
        (Some(cmd), Some(out)) => Some(format!("$ {cmd}\n{out}")),
        (Some(cmd), None) => Some(format!("$ {cmd}")),
        (None, Some(out)) => Some(out),
        (None, None) => None,
    }
}

/// Formats command runtime concisely for the block status pill.
pub(super) fn format_duration(ms: u32) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        let secs = f64::from(ms) / 1000.0;
        if secs < 10.0 {
            format!("{secs:.1}s")
        } else {
            format!("{secs:.0}s")
        }
    } else {
        let total_secs = ms / 1_000;
        let mins = total_secs / 60;
        let rem = total_secs % 60;
        if rem == 0 {
            format!("{mins}m")
        } else {
            format!("{mins}m {rem}s")
        }
    }
}

/// Formats a compact status badge (`✓`, `✓ 1.4s · 42L`, `✗ 1`, `● Running`, `★ ...`) for a command block.
pub(super) fn format_status_badge(block: &CommandBlock) -> Option<String> {
    let prefix = if block.bookmarked { "★ " } else { "" };
    if block.is_running() {
        return Some(format!("{prefix}● Running"));
    }
    let Some(code) = block.exit_code else {
        return block.bookmarked.then(|| "★".to_owned());
    };
    let lines = block.output_line_count();
    let line_suffix = if lines >= 10 {
        format!(" · {lines}L")
    } else {
        String::new()
    };
    if code == 0 {
        if let Some(ms) = block.duration_ms
            && ms >= 10
        {
            Some(format!("{prefix}✓ {}{line_suffix}", format_duration(ms)))
        } else if !line_suffix.is_empty() {
            Some(format!("{prefix}✓{line_suffix}"))
        } else {
            Some(format!("{prefix}✓"))
        }
    } else if let Some(ms) = block.duration_ms
        && ms >= 10
    {
        Some(format!(
            "{prefix}✗ {code} · {}{line_suffix}",
            format_duration(ms)
        ))
    } else {
        Some(format!("{prefix}✗ {code}{line_suffix}"))
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use lune_terminal::{HistoryLimits, PixelSize, Processor, Screen, UnicodeWriter};

    use super::{
        BlockAction, BlockToolbarLayout, StickyHeaderLayout, active_target_block, block_at_row,
        blocks_in_viewport, extract_block_full, extract_command, extract_output, format_duration,
        format_status_badge, last_command_text, next_bookmarked_prompt, output_selection_range,
        previous_bookmarked_prompt, previous_failed_prompt, sticky_block,
    };

    fn make_screen() -> Screen {
        let wire = concat!(
            "\x1b]133;A\x07$ \x1b]133;B\x07echo hello\r\n",
            "\x1b]133;C\x07hello\r\n\x1b]133;D;0\x07",
            "\x1b]133;A\x07$ \x1b]133;B\x07false\r\n",
            "\x1b]133;C\x07boom\r\nerr\r\n\x1b]133;D;1\x07",
            "\x1b]133;A\x07$ \x1b]133;B\x07"
        );
        let mut screen = Screen::new(40, 12).unwrap();
        screen
            .set_history_limits(HistoryLimits {
                max_rows: 64,
                max_bytes: 64 * 1024,
            })
            .unwrap();
        let black = lune_terminal::Rgb {
            red: 0,
            green: 0,
            blue: 0,
        };
        let white = lune_terminal::Rgb {
            red: 255,
            green: 255,
            blue: 255,
        };
        let palette = lune_terminal::Palette::new([black; 256], white, black, white);
        let mut processor = Processor::new(1024, palette).unwrap();
        let mut writer = UnicodeWriter::default();
        processor
            .advance(wire.as_bytes(), &mut screen, &mut writer)
            .unwrap();
        screen
    }

    #[test]
    fn discovers_blocks_and_extracts_exact_commands_and_outputs() {
        let screen = make_screen();
        let view = screen.view();
        let blocks = blocks_in_viewport(view, 0..view.rows());
        assert_eq!(blocks.len(), 3);

        assert_eq!(blocks[0].exit_code, Some(0));
        assert_eq!(
            extract_command(view, &blocks[0]).as_deref(),
            Some("echo hello")
        );
        assert_eq!(extract_output(view, &blocks[0]).as_deref(), Some("hello"));

        assert_eq!(blocks[1].exit_code, Some(1));
        assert!(blocks[1].is_error());
        assert_eq!(extract_command(view, &blocks[1]).as_deref(), Some("false"));
        assert_eq!(
            extract_output(view, &blocks[1]).as_deref(),
            Some("boom\nerr")
        );

        // Active empty prompt has neither command nor output.
        assert_eq!(blocks[2].exit_code, None);
        assert_eq!(extract_command(view, &blocks[2]), None);
        assert_eq!(extract_output(view, &blocks[2]), None);

        assert_eq!(
            block_at_row(view, 3).unwrap().prompt_row,
            blocks[1].prompt_row
        );
        assert_eq!(last_command_text(view).as_deref(), Some("false"));
    }

    #[test]
    fn formats_durations_and_hits_toolbar_buttons() {
        assert_eq!(format_duration(42), "42ms");
        assert_eq!(format_duration(1_450), "1.4s");
        assert_eq!(format_duration(12_300), "12s");
        assert_eq!(format_duration(125_000), "2m 5s");

        let area = PixelSize {
            width: NonZeroU32::new(800).unwrap(),
            height: NonZeroU32::new(400).unwrap(),
        };
        let ui_cell = PixelSize {
            width: NonZeroU32::new(8).unwrap(),
            height: NonZeroU32::new(16).unwrap(),
        };
        let toolbar = BlockToolbarLayout::compute(area, 40, ui_cell, 1.0, 12, true, true).unwrap();
        assert_eq!(toolbar.buttons.len(), 3);
        let run_btn = toolbar.buttons[2];
        assert_eq!(
            toolbar.hit(
                f64::from(run_btn.rect.origin.x + 5),
                f64::from(run_btn.rect.origin.y + 5),
            ),
            Some(BlockAction::Rerun)
        );

        let sticky = StickyHeaderLayout::compute(area, ui_cell, 1.0, 12, true, true).unwrap();
        assert_eq!(sticky.buttons.len(), 3);
        assert_eq!(
            sticky.hit(
                f64::from(sticky.text_x + 4),
                f64::from(sticky.bar.origin.y + 6),
            ),
            Some(BlockAction::JumpToPrompt)
        );
        let sticky_out = sticky.buttons[1];
        assert_eq!(
            sticky.hit(
                f64::from(sticky_out.rect.origin.x + 5),
                f64::from(sticky_out.rect.origin.y + 5),
            ),
            Some(BlockAction::CopyOutput)
        );
    }

    #[test]
    fn resolves_sticky_failed_and_active_target_blocks() {
        let screen = make_screen();
        let view = screen.view();
        let blocks = blocks_in_viewport(view, 0..view.rows());

        // Scrolled one row into block[1] (rows 2..5 -> prompt_row=2, end_row=5):
        // at first_row=3, two output rows (3, 4) remain so sticky header activates.
        let scrolled = blocks_in_viewport(view, 3..view.rows());
        assert_eq!(sticky_block(&scrolled, 3).unwrap().prompt_row, 2);
        // At first_row=4 only one row remains, so sticky header stays hidden.
        let near_end = blocks_in_viewport(view, 4..view.rows());
        assert!(sticky_block(&near_end, 4).is_none());

        assert_eq!(previous_failed_prompt(view, view.rows()), Some(2));
        assert_eq!(previous_failed_prompt(view, 2), None);

        // At live bottom (offset=0), target is the most recent finished block (block[1]).
        assert_eq!(
            active_target_block(view, 0, 0).unwrap().prompt_row,
            blocks[1].prompt_row
        );
        // When scrolled to block[0] (offset>0, first_row=0), target follows the viewed block.
        assert_eq!(
            active_target_block(view, 0, 2).unwrap().prompt_row,
            blocks[0].prompt_row
        );

        assert_eq!(
            extract_block_full(view, &blocks[1]).as_deref(),
            Some("$ false\nboom\nerr")
        );
        let sel = output_selection_range(view, &blocks[1]).unwrap();
        assert_eq!(sel.start.row, 3);
        assert_eq!(sel.end.row, 4);

        let mut screen = make_screen();
        assert_eq!(screen.toggle_prompt_bookmark(0), Some(true));
        assert_eq!(screen.toggle_prompt_bookmark(2), Some(true));
        let bookmarked = block_at_row(screen.view(), 2).unwrap();
        assert!(bookmarked.bookmarked);
        assert_eq!(format_status_badge(&bookmarked).as_deref(), Some("★ ✗ 1"));
        assert_eq!(
            previous_bookmarked_prompt(screen.view(), screen.view().rows()),
            Some(2)
        );
        assert_eq!(previous_bookmarked_prompt(screen.view(), 2), Some(0));
        assert_eq!(previous_bookmarked_prompt(screen.view(), 0), None);
        assert_eq!(next_bookmarked_prompt(screen.view(), 0), Some(2));
        assert_eq!(next_bookmarked_prompt(screen.view(), 2), None);
        assert_eq!(screen.toggle_prompt_bookmark(2), Some(false));
    }
}
