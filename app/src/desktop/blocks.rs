//! Modern-mode OSC 133 command block discovery, text extraction, and hover toolbar geometry.

use std::num::NonZeroU32;
use std::ops::Range;

use lune_terminal::{CellPosition, PixelPosition, PixelSize, ScreenView};

use crate::PixelRect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BlockAction {
    CopyCommand,
    CopyOutput,
    Rerun,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct HoveredBlock {
    pub(super) prompt_row: usize,
    pub(super) hovered_action: Option<BlockAction>,
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
    }
}

fn row_is_blank(view: ScreenView<'_>, row_idx: usize) -> bool {
    let Some(row) = view.row(row_idx) else {
        return true;
    };
    let end_col = row.wrap_end().unwrap_or(row.cells().len());
    let Ok(range) = view.selection_text(
        CellPosition {
            row: row_idx,
            column: 0,
        }..CellPosition {
            row: row_idx,
            column: end_col,
        },
    ) else {
        return true;
    };
    range.to_string().trim().is_empty()
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

/// Extracts the command output text (`OSC 133;C` .. next `OSC 133;A`) for a block.
pub(super) fn extract_output(view: ScreenView<'_>, block: &CommandBlock) -> Option<String> {
    let start_row = block.output_row?;
    if block.end_row <= start_row {
        return None;
    }
    let end_row = (block.end_row - 1).min(view.rows().saturating_sub(1));
    let end_col = view.row(end_row)?.cells().len();
    let bounds = view.selection_bounds(
        CellPosition {
            row: start_row,
            column: 0,
        }..CellPosition {
            row: end_row,
            column: end_col,
        },
    )?;
    let text = view.selection_text(bounds).ok()?.to_string();
    (!text.trim().is_empty()).then_some(text)
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

/// Formats a compact status badge (`✓`, `✓ 1.4s`, `✗ 1`, `● Running`) for a command block.
pub(super) fn format_status_badge(block: &CommandBlock) -> Option<String> {
    if block.is_running() {
        return Some("● Running".to_owned());
    }
    let code = block.exit_code?;
    if code == 0 {
        if let Some(ms) = block.duration_ms
            && ms >= 10
        {
            Some(format!("✓ {}", format_duration(ms)))
        } else {
            Some("✓".to_owned())
        }
    } else if let Some(ms) = block.duration_ms
        && ms >= 10
    {
        Some(format!("✗ {code} · {}", format_duration(ms)))
    } else {
        Some(format!("✗ {code}"))
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use lune_terminal::{HistoryLimits, PixelSize, Processor, Screen, UnicodeWriter};

    use super::{
        BlockAction, BlockToolbarLayout, block_at_row, blocks_in_viewport, extract_command,
        extract_output, format_duration, last_command_text,
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
    }
}
