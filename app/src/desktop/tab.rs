use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lune_terminal::{
    CellPosition, ClipboardQuery, ColorScheme, CursorShape, HistoryLimits, HyperlinkId,
    MouseButton, MouseEvent, MouseTracking, Palette, PixelPosition, PixelSize, Progress,
    ScreenBuffer,
};
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, KeyEvent, MouseScrollDelta};
use winit::event_loop::EventLoopProxy;
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{UserAttentionType, Window};

use crate::{GridGeometry, MAX_INPUT_BYTES, PixelRect, Session, SessionReady, WaitOutcome};

use super::blocks::{self, BlockAction, HoveredBlock};
use super::clipboard::{self, ClipboardType};
use super::config::{ClipboardPolicy, EnvOverride, NotificationPolicy};
use super::confirm;
use super::directory;
use super::hints::{self, HintKind, HintMode, HintResolution};
use super::ime;
use super::input::{self, Input};
use super::notify::Notifier;
use super::readiness::{Readiness, Registration};
use super::scrollbar::{ScrollbarMetrics, ScrollbarPolicy, ScrollbarView};
use super::search;
use super::shell_integration;
use super::split::{PaneRect, SplitLayout, SplitTree};
use super::url::{self, DetectedUrl, HoveredLink};
use super::{Event, PASTE_TIMEOUT, PaneId, Result, selection, vi};

const INPUT_LIMIT: usize = 1024;

#[derive(Clone, Copy, Debug)]
pub(super) struct ContextMenuBlockTarget {
    pub(super) prompt_row: usize,
    pub(super) has_command: bool,
    pub(super) has_output: bool,
    pub(super) bookmarked: bool,
}

pub(super) struct DispatchContext<'a> {
    pub(super) focused: bool,
    pub(super) copy_pending: &'a mut bool,
    pub(super) queued_copy: &'a mut Option<String>,
    pub(super) primary: &'a mut Option<clipboard::Primary>,
    pub(super) clipboard_write: ClipboardPolicy,
    pub(super) clipboard_read: ClipboardPolicy,
    pub(super) notifications_policy: NotificationPolicy,
    pub(super) notifier: Option<&'a Notifier>,
}

pub(super) struct Tab {
    pub(super) id: u64,
    pub(super) tree: SplitTree,
    pub(super) panes: Vec<Pane>,
    pub(super) focused: PaneId,
    pub(super) zoomed: Option<PaneId>,
    pub(super) custom_title: Option<String>,
    pub(super) broadcast_input: bool,
    pub(super) layout: SplitLayout,
}

impl Tab {
    pub(super) fn new(pane: Pane, area: PixelRect) -> Self {
        let id = pane.id;
        let layout = SplitLayout {
            panes: vec![PaneRect { id, rect: area }],
            dividers: Vec::new(),
        };
        Self {
            id: u64::from(id),
            tree: SplitTree::new(id),
            panes: vec![pane],
            focused: id,
            zoomed: None,
            custom_title: None,
            broadcast_input: false,
            layout,
        }
    }

    pub(super) fn focused_pane(&self) -> &Pane {
        self.panes
            .iter()
            .find(|pane| pane.id == self.focused)
            .expect("focused pane must exist in tab")
    }

    pub(super) fn focused_pane_mut(&mut self) -> &mut Pane {
        self.panes
            .iter_mut()
            .find(|pane| pane.id == self.focused)
            .expect("focused pane must exist in tab")
    }

    pub(super) fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.iter().find(|pane| pane.id == id)
    }

    pub(super) fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|pane| pane.id == id)
    }

    pub(super) fn label(&self) -> &str {
        self.custom_title
            .as_deref()
            .unwrap_or_else(|| self.focused_pane().label())
    }

    pub(super) fn progress(&self) -> Progress {
        let focused = self.focused_pane().progress;
        if focused != Progress::None {
            return focused;
        }
        self.panes
            .iter()
            .map(|pane| pane.progress)
            .find(|&progress| progress != Progress::None)
            .unwrap_or(Progress::None)
    }

    pub(super) fn is_zoomed(&self) -> bool {
        self.zoomed.is_some() && self.panes.len() > 1
    }

    pub(super) fn reflow(
        &mut self,
        area: PixelRect,
        divider: u32,
        cell: PixelSize,
        gutter: u32,
        tab_active: bool,
    ) {
        let zoomed = self.zoomed.filter(|_| self.panes.len() > 1);
        self.layout = self.tree.layout(area, divider, zoomed);
        for pane in &mut self.panes {
            if let Some(rect) = self.layout.rect_of(pane.id) {
                let inset = gutter.min(rect.size.width.get().saturating_sub(cell.width.get()));
                pane.origin = PixelPosition {
                    x: rect.origin.x.saturating_add(inset),
                    y: rect.origin.y,
                };
                let text_size = PixelSize {
                    width: NonZeroU32::new(rect.size.width.get().saturating_sub(inset))
                        .unwrap_or(cell.width),
                    height: rect.size.height,
                };
                if let Some(grid) = GridGeometry::fit(text_size, cell) {
                    pane.target_grid = grid;
                }
                pane.visible = tab_active;
                pane.active = tab_active && pane.id == self.focused;
            } else {
                pane.visible = false;
                pane.active = false;
            }
        }
    }
}

pub(super) struct Pane {
    pub(super) id: PaneId,
    pub(super) origin: PixelPosition,
    pub(super) target_grid: GridGeometry,
    pub(super) session: Session,
    pub(super) registration: Registration,
    window: Arc<Window>,
    proxy: EventLoopProxy<Event>,
    clipboard: Option<clipboard::DataDeviceHandle>,
    pub(super) title: String,
    pub(super) title_changed: bool,
    pub(super) progress: Progress,
    pub(super) progress_changed: bool,
    inputs: VecDeque<Input>,
    paste_worker: bool,
    paste_deadline: Option<Instant>,
    pub(super) visible: bool,
    pub(super) active: bool,
    pub(super) dirty: bool,
    pub(super) restart_blink: bool,
    pub(super) modifiers: ModifiersState,
    pub(super) ime_enabled: bool,
    ime_area: ime::ImeAreaCache,
    pub(super) preedit: ime::Preedit,
    pub(super) pointer: Option<PhysicalPosition<f64>>,
    pub(super) pressed: Option<MouseButton>,
    wheel: f64,
    selection_click: Option<selection::Click>,
    pub(super) selection_drag: Option<selection::Drag>,
    pub(super) selection: Option<selection::Selection>,
    pub(super) scrollbar_drag: Option<f64>,
    pub(super) scrollbar_hovered: bool,
    pub(super) hovered_block: Option<HoveredBlock>,
    pub(super) hovered_link: Option<HoveredLink>,
    hyperlink_click: Option<HyperlinkId>,
    pub(super) vi: Option<vi::Mode>,
    pub(super) search: Option<search::SearchBar>,
    pub(super) search_saved: Option<search::SavedSearch>,
    pub(super) hints: Option<HintMode>,
}

pub(super) struct PaneConfig<'a> {
    pub(super) palette: &'a Palette,
    pub(super) scheme: ColorScheme,
    pub(super) history_limits: HistoryLimits,
    pub(super) cursor_style: CursorShape,
    pub(super) cursor_blink: bool,
    pub(super) shell: Option<&'a [OsString]>,
    pub(super) shell_integration: bool,
    pub(super) term: &'a str,
    pub(super) term_program: Option<&'a str>,
    pub(super) env: &'a [EnvOverride],
    pub(super) clipboard: Option<clipboard::DataDeviceHandle>,
    pub(super) ipc_socket: Option<&'a Path>,
}

impl Pane {
    pub(super) fn spawn(
        id: PaneId,
        window: Arc<Window>,
        proxy: EventLoopProxy<Event>,
        grid: GridGeometry,
        config: PaneConfig<'_>,
        cwd: Option<PathBuf>,
        program: Option<&[OsString]>,
    ) -> Result<Self> {
        let mut command = match program {
            Some([executable, args @ ..]) => {
                let mut command = Command::new(executable);
                command.args(args);
                command
            }
            _ => match config.shell {
                Some([executable, args @ ..]) => {
                    shell_integration::build_command(executable, args, config.shell_integration)
                }
                _ => {
                    let shell = std::env::var_os("SHELL")
                        .filter(|shell| !shell.is_empty())
                        .unwrap_or_else(|| "/bin/sh".into());
                    shell_integration::build_command(&shell, &[], config.shell_integration)
                }
            },
        };
        if let Some(dir) = cwd {
            command.current_dir(dir);
        }
        command
            .env("TERM", config.term)
            .env("COLORTERM", "truecolor");
        shell_integration::inject_terminfo(&mut command, config.term);
        match config.term_program {
            Some(program) => {
                command.env("TERM_PROGRAM", program);
            }
            None => {
                command.env_remove("TERM_PROGRAM");
            }
        }
        if let Some(socket) = config.ipc_socket {
            command.env("LUNE_IPC_SOCKET", socket);
        }
        for entry in config.env {
            match entry {
                EnvOverride::Set(name, value) => {
                    command.env(name, value);
                }
                EnvOverride::Unset(name) => {
                    command.env_remove(name);
                }
            }
        }
        let pty_size = grid.pty_size();
        log::debug!(
            "Starting PTY session with {} columns and {} rows",
            pty_size.columns,
            pty_size.rows
        );
        let mut session = Session::spawn(command, pty_size, config.palette.clone())?;
        session.set_theme(config.palette, Some(config.scheme));
        session.set_default_cursor_appearance(config.cursor_style, config.cursor_blink);
        session.set_history_limits(config.history_limits)?;
        Ok(Self {
            id,
            origin: PixelPosition { x: 0, y: 0 },
            target_grid: grid,
            session,
            registration: Registration::default(),
            window,
            proxy,
            clipboard: config.clipboard,
            title: String::new(),
            title_changed: false,
            progress: Progress::None,
            progress_changed: false,
            inputs: VecDeque::new(),
            paste_worker: false,
            paste_deadline: None,
            visible: false,
            active: false,
            dirty: true,
            restart_blink: false,
            modifiers: ModifiersState::empty(),
            ime_enabled: false,
            ime_area: ime::ImeAreaCache::default(),
            preedit: ime::Preedit::default(),
            pointer: None,
            pressed: None,
            wheel: 0.0,
            selection_click: None,
            selection_drag: None,
            selection: None,
            scrollbar_drag: None,
            scrollbar_hovered: false,
            hovered_block: None,
            hovered_link: None,
            hyperlink_click: None,
            vi: None,
            search: None,
            search_saved: None,
            hints: None,
        })
    }

    pub(super) fn working_directory(&self) -> Option<PathBuf> {
        directory::working_directory(
            self.session.processor().working_directory_uri(),
            self.session.foreground_process_group(),
            self.session.child_id(),
        )
    }

    pub(super) fn foreground_process_name(&self) -> Option<String> {
        let child = self.session.child_id();
        let pgid = self
            .session
            .foreground_process_group()
            .filter(|&pgid| pgid != child)?;
        let name = std::fs::read_to_string(format!("/proc/{pgid}/comm"))
            .ok()
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| "a running process".into());
        Some(name)
    }

    pub(super) fn drop_path(&mut self, path: &Path) {
        if self.vi.is_some() || self.inputs.len() >= INPUT_LIMIT {
            return;
        }
        let mut quoted = shell_quote_path(path);
        quoted.push(' ');
        self.follow_output();
        self.push_input(Input::Paste(quoted));
    }

    fn scrollbar_metrics(&self, scale: f64) -> Option<ScrollbarMetrics> {
        let screen = self.session.screen();
        if screen.active_buffer() != ScreenBuffer::Primary {
            return None;
        }
        let history_rows = screen.history().len();
        if history_rows == 0 {
            return None;
        }
        let offset = screen.viewport_offset();
        let area = self.session.geometry().text_area_pixels()?;
        ScrollbarMetrics::compute(area, history_rows, screen.viewport().rows(), offset, scale)
    }

    pub(super) fn scrollbar_view(
        &self,
        scale: f64,
        policy: ScrollbarPolicy,
    ) -> Option<ScrollbarView> {
        if policy == ScrollbarPolicy::Never {
            return None;
        }
        let metrics = self.scrollbar_metrics(scale)?;
        let highlighted = self.scrollbar_hovered || self.scrollbar_drag.is_some();
        let offset = self.session.screen().viewport_offset();
        let visible = match policy {
            ScrollbarPolicy::Always => true,
            ScrollbarPolicy::Auto => offset > 0 || highlighted,
            ScrollbarPolicy::Never => false,
        };
        visible.then(|| metrics.view(highlighted))
    }

    pub(super) fn update_scrollbar_hover(&mut self, policy: ScrollbarPolicy) {
        let hovered = (policy != ScrollbarPolicy::Never && self.visible)
            .then(|| {
                let pos = self.pointer?;
                let metrics = self.scrollbar_metrics(self.window.scale_factor())?;
                let local_x = pos.x - f64::from(self.origin.x);
                let local_y = pos.y - f64::from(self.origin.y);
                Some(metrics.contains(local_x, local_y))
            })
            .flatten()
            .unwrap_or(false);
        if self.scrollbar_hovered != hovered {
            self.scrollbar_hovered = hovered;
            self.invalidate_overlay();
        }
    }

    pub(super) fn invalidate_overlay(&mut self) {
        self.dirty = true;
        if self.visible {
            self.window.request_redraw();
        }
    }

    pub(super) fn allows_ime(&self) -> bool {
        self.search.is_some() || (self.vi.is_none() && self.hints.is_none())
    }

    pub(super) fn invalidate_ime_area(&mut self) {
        self.ime_area.invalidate();
    }

    pub(super) fn set_ime_allowed(&mut self) {
        if self.active {
            self.ime_area.invalidate();
            self.window.set_ime_allowed(self.allows_ime());
        }
    }

    pub(super) fn focus(&mut self, focused: bool) {
        self.push_input(Input::Focus(focused));
        if !focused {
            if let Some(button) = self.pressed.take() {
                self.mouse_event(MouseEvent::Release(button));
            }
            self.selection_click = None;
            self.selection_drag = None;
            self.scrollbar_drag = None;
            self.scrollbar_hovered = false;
            self.hyperlink_click = None;
            if let Some(vi) = &mut self.vi {
                vi.cancel_pending();
            }
            self.close_hints();
            self.clear_preedit();
        }
    }

    pub(super) fn pointer_moved(
        &mut self,
        position: PhysicalPosition<f64>,
        scrollbar_policy: ScrollbarPolicy,
    ) {
        let moved = self.pointer.replace(position) != Some(position);
        if let Some(grab_offset_y) = self.scrollbar_drag {
            if let Some(metrics) = self.scrollbar_metrics(self.window.scale_factor()) {
                let local_y = position.y - f64::from(self.origin.y);
                let target = metrics.offset_for_y(local_y, grab_offset_y);
                let current = self.session.screen().viewport_offset();
                if target != current {
                    self.scroll_history(target as isize - current as isize);
                }
            }
            return;
        }
        if self.selection_drag.is_some() {
            self.extend_selection();
            if moved {
                self.update_selection_scroll(position.y);
            }
        } else {
            self.mouse_event(MouseEvent::Motion(self.pressed));
        }
        self.update_scrollbar_hover(scrollbar_policy);
        self.update_hovered_hyperlink();
    }

    pub(super) fn update_hovered_block(
        &mut self,
        enabled: bool,
        ui_cell: PixelSize,
        scrollbar_policy: ScrollbarPolicy,
    ) {
        let active_hover = enabled
            && self.visible
            && self.vi.is_none()
            && self.search.is_none()
            && self.hints.is_none()
            && self.selection_drag.is_none()
            && self.scrollbar_drag.is_none()
            && !self.scrollbar_hovered
            && !self.mouse_reporting()
            && self.session.screen().active_buffer() == ScreenBuffer::Primary;

        let next = if active_hover {
            self.block_hover_at_pointer(ui_cell, scrollbar_policy)
        } else {
            None
        };
        if self.hovered_block != next {
            self.hovered_block = next;
            self.invalidate_overlay();
        }
    }

    fn block_hover_at_pointer(
        &self,
        ui_cell: PixelSize,
        scrollbar_policy: ScrollbarPolicy,
    ) -> Option<HoveredBlock> {
        let pos = self.pointer?;
        let geometry = self.session.geometry();
        let pane_area = geometry.text_area_pixels()?;
        let scale = self.window.scale_factor();
        let gutter_allow = (16.0 * scale.max(1.0)).round();
        let local_x = pos.x - f64::from(self.origin.x);
        let local_y = pos.y - f64::from(self.origin.y);
        if local_x < -gutter_allow
            || local_y < 0.0
            || local_x >= f64::from(pane_area.width.get())
            || local_y >= f64::from(pane_area.height.get())
        {
            return None;
        }
        let pixel = PixelPosition {
            x: local_x.max(0.0) as u32,
            y: local_y as u32,
        };
        let screen = self.session.screen();
        let first_row = screen.viewport().range().start;
        let rows = usize::from(geometry.pty_size().rows);
        let view = screen.view();
        let scrollbar_reserve = self
            .scrollbar_view(scale, scrollbar_policy)
            .map_or(0, |sb| sb.track.size.width.get() + 6);

        // Sticky command header takes hit-test precedence over underlying pane rows.
        if local_x >= 0.0 && self.vi.is_none() && self.search.is_none() && self.hints.is_none() {
            let visible_blocks = blocks::blocks_in_viewport(view, first_row..first_row + rows);
            if let Some(sticky) = blocks::sticky_block(&visible_blocks, first_row)
                && blocks::has_command(view, &sticky)
            {
                let has_output = blocks::has_output(view, &sticky);
                if let Some(layout) = blocks::StickyHeaderLayout::compute(
                    pane_area,
                    ui_cell,
                    scale,
                    scrollbar_reserve,
                    true,
                    has_output,
                ) && let Some(action) = layout.hit(local_x, local_y)
                {
                    return Some(HoveredBlock {
                        prompt_row: sticky.prompt_row,
                        hovered_action: Some(action),
                        on_sticky: true,
                    });
                }
            }
        }

        let cell = geometry.cell_at(pixel)?;
        let block = blocks::block_at_row(view, first_row + cell.row)?;
        let has_command = blocks::has_command(view, &block);
        let has_output = blocks::has_output(view, &block);
        if !has_command && !has_output {
            return None;
        }
        let vis_start = block
            .prompt_row
            .saturating_sub(first_row)
            .min(rows.saturating_sub(1));
        let anchor_y = geometry.row_rect(vis_start, 0..1).map_or(0, |r| r.origin.y);
        let hovered_action = if local_x < 0.0 && has_output {
            Some(BlockAction::SelectOutput)
        } else {
            blocks::BlockToolbarLayout::compute(
                pane_area,
                anchor_y,
                ui_cell,
                scale,
                scrollbar_reserve,
                has_command,
                has_output,
            )
            .and_then(|toolbar| toolbar.hit(local_x, local_y))
        };
        Some(HoveredBlock {
            prompt_row: block.prompt_row,
            hovered_action,
            on_sticky: false,
        })
    }

    pub(super) fn trigger_hovered_block_action(
        &mut self,
        copy_pending: &mut bool,
    ) -> Option<String> {
        let HoveredBlock {
            prompt_row,
            hovered_action: Some(action),
            ..
        } = self.hovered_block?
        else {
            return None;
        };
        self.run_block_action(prompt_row, action, copy_pending)
    }

    pub(super) fn run_block_action(
        &mut self,
        prompt_row: usize,
        action: BlockAction,
        copy_pending: &mut bool,
    ) -> Option<String> {
        if action == BlockAction::JumpToPrompt {
            self.jump_to_row(prompt_row);
            self.hovered_block = None;
            self.invalidate_overlay();
            return None;
        }
        if action == BlockAction::ToggleBookmark {
            let bookmarked = self.session.toggle_prompt_bookmark(prompt_row)?;
            self.invalidate_overlay();
            return Some(if bookmarked {
                "Bookmarked command block".to_owned()
            } else {
                "Removed bookmark".to_owned()
            });
        }
        let view = self.session.screen().view();
        let block = blocks::block_at_row(view, prompt_row)?;
        match action {
            BlockAction::JumpToPrompt | BlockAction::ToggleBookmark => unreachable!(),
            BlockAction::SearchInBlock => {
                let scope = block.prompt_row..block.end_row;
                self.open_block_search(scope);
                None
            }
            BlockAction::SelectOutput => {
                let bounds = blocks::output_selection_range(view, &block)?;
                let lines = bounds.end.row.saturating_sub(bounds.start.row) + 1;
                self.selection = Some(selection::Selection::new(
                    view,
                    bounds,
                    selection::SelectionKind::Stream,
                    None,
                ));
                self.selection_click = None;
                self.selection_drag = None;
                self.invalidate_overlay();
                Some(if lines == 1 {
                    "Selected output (1 line)".to_owned()
                } else {
                    format!("Selected output ({lines} lines)")
                })
            }
            BlockAction::CopyFull => {
                if *copy_pending {
                    return None;
                }
                let text = blocks::extract_block_full(view, &block)?;
                let notice = format_copied_summary(&text);
                match clipboard::write(text, self.clipboard.clone(), self.proxy.clone()) {
                    Ok(()) => {
                        *copy_pending = true;
                        Some(notice)
                    }
                    Err(error) => {
                        log::warn!("Failed to start the clipboard copy worker: {error}");
                        None
                    }
                }
            }
            BlockAction::CopyCommand => {
                if *copy_pending {
                    return None;
                }
                let cmd = blocks::extract_command(view, &block)?;
                let notice = format_copied_preview(&cmd);
                match clipboard::write(cmd, self.clipboard.clone(), self.proxy.clone()) {
                    Ok(()) => {
                        *copy_pending = true;
                        Some(notice)
                    }
                    Err(error) => {
                        log::warn!("Failed to start the clipboard copy worker: {error}");
                        None
                    }
                }
            }
            BlockAction::CopyOutput => {
                if *copy_pending {
                    return None;
                }
                let output = blocks::extract_output(view, &block)?;
                let notice = format_copied_summary(&output);
                match clipboard::write(output, self.clipboard.clone(), self.proxy.clone()) {
                    Ok(()) => {
                        *copy_pending = true;
                        Some(notice)
                    }
                    Err(error) => {
                        log::warn!("Failed to start the clipboard copy worker: {error}");
                        None
                    }
                }
            }
            BlockAction::Rerun => {
                let cmd = blocks::extract_command(view, &block)?;
                let preview = if cmd.chars().count() > 36 {
                    let short: String = cmd.chars().take(36).collect();
                    format!("Re-running: {short}...")
                } else {
                    format!("Re-running: {cmd}")
                };
                self.follow_output();
                self.push_input(Input::Text(format!("{cmd}\r")));
                let _ = self.flush_input(Instant::now());
                Some(preview)
            }
        }
    }

    pub(super) fn context_menu_target_at(
        &self,
        point: PhysicalPosition<f64>,
        command_blocks_enabled: bool,
    ) -> (Option<String>, Option<ContextMenuBlockTarget>) {
        let mut link_url = None;
        let mut block_info = None;
        let geometry = self.session.geometry();
        let Some(pane_area) = geometry.text_area_pixels() else {
            return (None, None);
        };
        let local_x = point.x - f64::from(self.origin.x);
        let local_y = point.y - f64::from(self.origin.y);
        if local_y < 0.0 || local_y >= f64::from(pane_area.height.get()) {
            return (None, None);
        }
        let scale = self.window.scale_factor();
        let gutter_allow = (16.0 * scale.max(1.0)).round();
        if local_x < -gutter_allow || local_x >= f64::from(pane_area.width.get()) {
            return (None, None);
        }
        let pixel = PixelPosition {
            x: local_x.max(0.0) as u32,
            y: local_y as u32,
        };
        let Some(cell) = geometry.cell_at(pixel) else {
            return (None, None);
        };
        let screen = self.session.screen();
        let view = screen.view();
        let first_row = screen.viewport().range().start;
        if local_x >= 0.0 {
            if let Some(row) = screen.viewport().row(cell.row)
                && let Some(style_id) = row.cells().get(cell.column).map(|c| c.style())
                && let Some(style) = self.session.processor().styles().get(style_id)
                && style.hyperlink != HyperlinkId(0)
                && let Some(link) = self.session.processor().hyperlinks().get(style.hyperlink)
            {
                link_url = Some(link.uri().to_owned());
            } else {
                let abs_cell = CellPosition {
                    row: first_row + cell.row,
                    column: cell.column,
                };
                if let Some(detected) = url::detect_at(view, abs_cell) {
                    link_url = Some(detected.uri);
                }
            }
        }
        if command_blocks_enabled
            && screen.active_buffer() == ScreenBuffer::Primary
            && let Some(block) = blocks::block_at_row(view, first_row + cell.row)
        {
            let has_cmd = blocks::has_command(view, &block);
            let has_out = blocks::has_output(view, &block);
            if has_cmd || has_out {
                block_info = Some(ContextMenuBlockTarget {
                    prompt_row: block.prompt_row,
                    has_command: has_cmd,
                    has_output: has_out,
                    bookmarked: block.bookmarked,
                });
            }
        }
        (link_url, block_info)
    }

    pub(super) fn allows_context_menu(&self, shift: bool) -> bool {
        !self.mouse_reporting() || shift
    }

    pub(super) fn has_selection(&self) -> bool {
        self.selection.is_some()
    }

    pub(super) fn open_uri_external(&self, uri: String, hyperlink_open_pending: &mut bool) {
        self.open_uri(uri, hyperlink_open_pending);
    }

    pub(super) fn copy_last_command(&self, copy_pending: &mut bool) -> Option<String> {
        if *copy_pending {
            return None;
        }
        let screen = self.session.screen();
        let view = screen.view();
        let first_row = screen.viewport().range().start;
        let offset = screen.viewport_offset();
        let cmd = blocks::active_target_block(view, first_row, offset)
            .and_then(|block| blocks::extract_command(view, &block))
            .or_else(|| blocks::last_command_text(view))?;
        let notice = format_copied_preview(&cmd);
        match clipboard::write(cmd, self.clipboard.clone(), self.proxy.clone()) {
            Ok(()) => {
                *copy_pending = true;
                Some(notice)
            }
            Err(error) => {
                log::warn!("Failed to start the clipboard copy worker: {error}");
                None
            }
        }
    }

    pub(super) fn rerun_last_command(&mut self) -> Option<String> {
        let screen = self.session.screen();
        if screen.active_buffer() != ScreenBuffer::Primary {
            return None;
        }
        let view = screen.view();
        let first_row = screen.viewport().range().start;
        let offset = screen.viewport_offset();
        let cmd = blocks::active_target_block(view, first_row, offset)
            .and_then(|block| blocks::extract_command(view, &block))
            .or_else(|| blocks::last_command_text(view))?;
        let preview = if cmd.chars().count() > 36 {
            let short: String = cmd.chars().take(36).collect();
            format!("Re-running: {short}...")
        } else {
            format!("Re-running: {cmd}")
        };
        self.follow_output();
        self.push_input(Input::Text(format!("{cmd}\r")));
        let _ = self.flush_input(Instant::now());
        Some(preview)
    }

    pub(super) fn select_last_output(&mut self) -> Option<String> {
        let screen = self.session.screen();
        if screen.active_buffer() != ScreenBuffer::Primary {
            return None;
        }
        let view = screen.view();
        let first_row = screen.viewport().range().start;
        let offset = screen.viewport_offset();
        let prompt_row = blocks::active_target_block(view, first_row, offset)?.prompt_row;
        let mut dummy = false;
        self.run_block_action(prompt_row, BlockAction::SelectOutput, &mut dummy)
    }

    pub(super) fn toggle_active_block_bookmark(&mut self) -> Option<String> {
        let screen = self.session.screen();
        if screen.active_buffer() != ScreenBuffer::Primary {
            return None;
        }
        let view = screen.view();
        let first_row = screen.viewport().range().start;
        let offset = screen.viewport_offset();
        let prompt_row = blocks::active_target_block(view, first_row, offset)?.prompt_row;
        let mut dummy = false;
        self.run_block_action(prompt_row, BlockAction::ToggleBookmark, &mut dummy)
    }

    pub(super) fn open_block_search(&mut self, scope: std::ops::Range<usize>) {
        if self.vi.is_some() {
            self.toggle_vi();
        }
        let columns = usize::from(self.session.geometry().pty_size().columns);
        let screen = self.session.screen();
        let buffer = screen.active_buffer();
        let view = screen.view();
        let viewport = screen.viewport().range();
        let bar = search::SearchBar::open_scoped(
            self.search_saved.take(),
            buffer,
            view,
            columns,
            viewport,
            scope,
        );
        let target = bar.current_match();
        self.search = Some(bar);
        self.hovered_block = None;
        if let Some(pos) = target {
            self.reveal_search_row(pos.row);
        }
        self.set_ime_allowed();
        self.update_ime_area();
        self.invalidate_overlay();
    }

    pub(super) fn jump_bookmark(&mut self, forward: bool) -> bool {
        let screen = self.session.screen();
        if screen.active_buffer() != ScreenBuffer::Primary {
            return false;
        }
        let view = screen.view();
        let history = view.history_rows();
        let viewport = screen.viewport();
        let current_offset = screen.viewport_offset();
        let target = if forward {
            if current_offset == 0 {
                return false;
            }
            blocks::next_bookmarked_prompt(view, viewport.range().start)
        } else {
            let anchor = if current_offset == 0 {
                view.rows()
            } else {
                viewport.range().start
            };
            blocks::previous_bookmarked_prompt(view, anchor)
        };
        let Some(row) = target else {
            return false;
        };
        let desired_offset = history.saturating_sub(row);
        let delta = desired_offset as isize - current_offset as isize;
        if delta != 0 {
            self.scroll_history(delta);
        }
        true
    }

    pub(super) fn jump_previous_failed_prompt(&mut self) -> bool {
        let screen = self.session.screen();
        if screen.active_buffer() != ScreenBuffer::Primary {
            return false;
        }
        let view = screen.view();
        let history = view.history_rows();
        let viewport = screen.viewport();
        let current_offset = screen.viewport_offset();
        let anchor = if current_offset == 0 {
            view.rows()
        } else {
            viewport.range().start
        };
        let Some(row) = blocks::previous_failed_prompt(view, anchor) else {
            return false;
        };
        let desired_offset = history.saturating_sub(row);
        let delta = desired_offset as isize - current_offset as isize;
        if delta != 0 {
            self.scroll_history(delta);
        }
        true
    }

    pub(super) fn update_hovered_hyperlink(&mut self) {
        let ctrl_only = self.modifiers.control_key()
            && !self.modifiers.alt_key()
            && !self.modifiers.super_key();
        let active_hover = self.visible
            && self.vi.is_none()
            && self.selection_drag.is_none()
            && self.scrollbar_drag.is_none()
            && !self.scrollbar_hovered
            && (!self.mouse_reporting() || ctrl_only);
        let next = if active_hover {
            self.hyperlink_at_pointer()
                .map(HoveredLink::Explicit)
                .or_else(|| {
                    ctrl_only
                        .then(|| self.detected_url_at_pointer())
                        .flatten()
                        .map(HoveredLink::Detected)
                })
        } else {
            None
        };
        if self.hovered_link != next {
            self.hovered_link = next;
            self.invalidate_overlay();
        }
    }

    fn hyperlink_at_pointer(&self) -> Option<HyperlinkId> {
        let pixel = self.pointer_pixel(false)?;
        let cell = self.session.geometry().cell_at(pixel)?;
        let row = self.session.screen().viewport().row(cell.row)?;
        let style_id = row.cells().get(cell.column)?.style();
        let link = self.session.processor().styles().get(style_id)?.hyperlink;
        (link != HyperlinkId(0)).then_some(link)
    }

    fn detected_url_at_pointer(&self) -> Option<DetectedUrl> {
        let pixel = self.pointer_pixel(false)?;
        let mut cell = self.session.geometry().cell_at(pixel)?;
        let screen = self.session.screen();
        cell.row += screen.viewport().range().start;
        url::detect_at(screen.view(), cell)
    }

    pub(super) fn label(&self) -> &str {
        if self.title.is_empty() {
            "Terminal"
        } else {
            &self.title
        }
    }

    pub(super) fn ime(&mut self, ime: Ime) {
        match ime {
            Ime::Preedit(text, cursor) => {
                if self.vi.is_none()
                    && self.search.is_none()
                    && self.preedit.is_empty()
                    && !text.is_empty()
                {
                    self.follow_output();
                }
                if self.allows_ime() {
                    self.set_preedit(text, cursor);
                    self.update_ime_area();
                }
            }
            Ime::Commit(mut text) => {
                self.clear_preedit();
                if self.search.is_some() {
                    self.search_push(&text);
                    self.update_ime_area();
                    return;
                }
                if self.vi.is_some() {
                    return;
                }
                self.follow_output();
                while text.len() > MAX_INPUT_BYTES {
                    let rest = text.split_off(text.floor_char_boundary(MAX_INPUT_BYTES));
                    self.push_input(Input::Text(text));
                    text = rest;
                }
                if !text.is_empty() {
                    self.push_input(Input::Text(text));
                }
                self.update_ime_area();
            }
            _ => {}
        }
    }

    pub(super) fn dispatch(
        &mut self,
        ready: SessionReady,
        ctx: DispatchContext<'_>,
    ) -> Result<bool> {
        let selection_source = (self.selection.is_some()
            || self.selection_click.is_some()
            || self.selection_drag.is_some())
        .then(|| {
            let screen = self.session.screen();
            (
                screen.active_buffer(),
                screen.view().discarded_history_rows(),
            )
        });
        let outcome = self.session.dispatch_ready(ready)?;
        let Some(mut outcome) = outcome else {
            return Ok(true);
        };
        let start = Instant::now();
        for _ in 0..if self.visible { 16 } else { 0 } {
            if outcome != WaitOutcome::Pumped || start.elapsed() >= Duration::from_millis(2) {
                break;
            }
            outcome = self.session.wait_and_pump(Some(Duration::ZERO))?;
        }
        if matches!(outcome, WaitOutcome::ChildExited(_) | WaitOutcome::Finished) {
            if let WaitOutcome::ChildExited(status) = outcome {
                log::info!("PTY child exited: {status}");
            } else {
                log::info!("PTY session finished");
            }
            return Ok(false);
        }
        let notifications = self.session.take_notifications();
        if let Some(title) = notifications.window_title
            && self.title != title
        {
            self.title.clear();
            self.title
                .extend(title.chars().filter(|ch| !ch.is_control()).take(128));
            self.title_changed = true;
        }
        if notifications.bell && !ctx.focused {
            self.window
                .request_user_attention(Some(UserAttentionType::Informational));
        }
        if let Some(progress) = notifications.progress
            && self.progress != progress
        {
            self.progress = progress;
            self.progress_changed = true;
            self.dirty = true;
        }
        if let Some(desktop_note) = notifications.desktop_notification {
            let allowed = match ctx.notifications_policy {
                NotificationPolicy::Always => true,
                NotificationPolicy::Unfocused => !ctx.focused,
                NotificationPolicy::Deny => false,
            };
            if allowed && let Some(notifier) = ctx.notifier {
                notifier.send(desktop_note);
            }
        }
        if let Some(write) = notifications.clipboard_write
            && ctx.clipboard_write == ClipboardPolicy::Allow
        {
            let text = write.text.to_owned();
            if write.targets.clipboard {
                if *ctx.copy_pending {
                    *ctx.queued_copy = Some(text.clone());
                } else {
                    match clipboard::write(text.clone(), self.clipboard.clone(), self.proxy.clone())
                    {
                        Ok(()) => *ctx.copy_pending = true,
                        Err(error) => {
                            log::warn!("Failed to start the OSC 52 copy worker: {error}");
                        }
                    }
                }
            }
            if write.targets.primary {
                publish_primary_text(text, self.clipboard.as_ref(), ctx.primary);
            }
        }
        if let Some(query) = notifications.clipboard_query
            && ctx.clipboard_read == ClipboardPolicy::Allow
        {
            let kind = if query.targets.clipboard {
                ClipboardType::Regular
            } else {
                ClipboardType::Primary
            };
            if let Err(error) = clipboard::read_osc52(
                kind,
                self.clipboard.clone(),
                ctx.primary.as_ref(),
                self.id,
                query,
                self.proxy.clone(),
            ) {
                log::warn!("Failed to start the OSC 52 read worker: {error}");
            }
        }
        if let Some((buffer, discarded_rows)) = selection_source {
            let screen = self.session.screen();
            let view = screen.view();
            if screen.active_buffer() != buffer
                || self
                    .selection
                    .as_mut()
                    .is_some_and(|selection| !selection.refresh(view))
            {
                self.clear_selection();
                if let Some(vi) = &mut self.vi {
                    vi.clear_visual();
                }
            } else if view.discarded_history_rows() != discarded_rows {
                self.selection_click = None;
                let removed = view.discarded_history_rows().wrapping_sub(discarded_rows);
                let removed = usize::try_from(removed).unwrap_or(usize::MAX);
                if self
                    .selection_drag
                    .as_mut()
                    .is_some_and(|drag| drag.rebase(removed).is_none())
                {
                    self.selection_drag = None;
                }
            }
        }
        self.refresh_vi();
        self.refresh_search();
        self.update_ime_area();
        self.update_hovered_hyperlink();
        if self.visible && self.session.presentation_frame().is_some() {
            self.window.request_redraw();
        }
        Ok(true)
    }

    pub(super) fn service(
        &mut self,
        readiness: &Readiness,
        now: Instant,
    ) -> Result<(bool, Option<Instant>)> {
        self.resize(self.target_grid)?;
        self.flush_input(now)?;
        let interests = self.session.interests();
        readiness.arm(self.id, &mut self.registration, interests)?;
        let deadline = interests
            .presentation_deadline
            .into_iter()
            .chain(self.paste_deadline)
            .min();
        Ok((interests.immediate, deadline))
    }

    fn resize(&mut self, grid: GridGeometry) -> Result<()> {
        let previous = self.session.geometry().pty_size();
        let next = grid.pty_size();
        if next == previous {
            return Ok(());
        }
        let vi_row = self.vi.as_ref().map(|vi| {
            vi.cursor
                .row
                .saturating_sub(self.session.screen().viewport().range().start)
        });
        match self.session.resize(next) {
            Ok(()) => {
                log::debug!(
                    "PTY resized to {} columns and {} rows",
                    next.columns,
                    next.rows
                );
                self.clear_selection();
                if let Some(vi) = &mut self.vi {
                    vi.clear_visual();
                }
                if let Some(row) = vi_row
                    && (previous.columns != next.columns || previous.rows != next.rows)
                {
                    self.reset_vi_cursor(row);
                }
                self.update_ime_area();
                self.update_hovered_hyperlink();
                self.invalidate_overlay();
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    pub(super) fn update_ime_area(&mut self) {
        if !self.active || !self.ime_enabled {
            return;
        }
        let geometry = self.session.geometry();
        let Some(cell) = geometry.cell_pixels() else {
            return;
        };
        let columns = usize::from(geometry.pty_size().columns);
        let rows = usize::from(geometry.pty_size().rows);
        let viewport = self.session.screen().viewport();

        let rect = if let Some(search) = &self.search {
            let Some(area) = geometry.text_area_pixels() else {
                return;
            };
            let cell_w = cell.width.get();
            let cell_h = cell.height.get();
            let pad_x = (cell_w / 2).max(4);
            let pad_y = (cell_h / 5).max(3);
            let margin = (cell_w / 2).max(6);
            let box_h = cell_h.saturating_add(pad_y * 2);
            let max_w = area.width.get().saturating_sub(margin * 2).min(1024);
            let box_w = (cell_w * 36).min(max_w);
            let box_x = area.width.get().saturating_sub(box_w + margin);
            let text_y = margin.min(area.height.get().saturating_sub(box_h)) + pad_y;
            let text_cols =
                ime::display_width(&search.query) + ime::display_width(self.preedit.text());
            let prefix_px = if search.vi_mode {
                cell_w + (cell_w / 2).max(4)
            } else {
                0
            };
            let max_caret_x = box_x
                .saturating_add(box_w)
                .saturating_sub(pad_x.saturating_add(cell_w));
            let caret_x = box_x
                .saturating_add(pad_x)
                .saturating_add(prefix_px)
                .saturating_add((text_cols as u32).saturating_mul(cell_w))
                .min(max_caret_x);
            PixelRect {
                origin: PixelPosition {
                    x: self.origin.x.saturating_add(caret_x),
                    y: self.origin.y.saturating_add(text_y),
                },
                size: cell,
            }
        } else if let Some(cursor) = viewport.cursor() {
            let base = CellPosition {
                row: cursor.row,
                column: cursor.column,
            };
            let anchor = self
                .preedit
                .cursor_cell(base, columns, rows)
                .unwrap_or(base);
            let Some(cell_rect) = geometry.row_rect(anchor.row, anchor.column..anchor.column + 1)
            else {
                return;
            };
            PixelRect {
                origin: PixelPosition {
                    x: self.origin.x.saturating_add(cell_rect.origin.x),
                    y: self.origin.y.saturating_add(cell_rect.origin.y),
                },
                size: cell_rect.size,
            }
        } else {
            return;
        };

        let surface = self.window.inner_size();
        let Some(clamped) =
            ime::clamp_to_surface(rect.origin, rect.size, surface.width, surface.height)
        else {
            return;
        };
        let scale = self.window.scale_factor();
        if self
            .ime_area
            .record_if_changed(clamped.origin, clamped.size, scale)
        {
            self.window.set_ime_cursor_area(
                PhysicalPosition::new(clamped.origin.x as i32, clamped.origin.y as i32),
                PhysicalSize::new(clamped.size.width.get(), clamped.size.height.get()),
            );
        }
    }

    pub(super) fn set_preedit(&mut self, text: String, cursor: Option<(usize, usize)>) {
        if self.preedit.set(text, cursor) {
            self.invalidate_overlay();
        }
    }

    pub(super) fn clear_preedit(&mut self) {
        if self.preedit.clear() {
            self.invalidate_overlay();
        }
    }

    fn push_input(&mut self, input: Input) {
        if self.inputs.len() >= INPUT_LIMIT {
            return;
        }
        if matches!(input, Input::Mouse(MouseEvent::Motion(_), ..))
            && matches!(
                self.inputs.back(),
                Some(Input::Mouse(MouseEvent::Motion(_), ..))
            )
        {
            self.inputs.pop_back();
        }
        self.inputs.push_back(input);
    }

    fn flush_input(&mut self, now: Instant) -> Result<()> {
        if self.paste_deadline.is_some_and(|deadline| deadline <= now) {
            self.paste_deadline = None;
            self.inputs
                .retain(|input| !matches!(input, Input::PendingPaste));
            log::warn!("Clipboard paste timed out");
        }
        while let Some(input) = self.inputs.front() {
            match input.queue(&mut self.session) {
                Ok(()) => {
                    self.inputs.pop_front();
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                    self.inputs.clear();
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub(super) fn finish_paste(
        &mut self,
        result: std::result::Result<String, String>,
        confirm_paste: bool,
    ) -> Option<String> {
        self.paste_worker = false;
        if self.paste_deadline.take().is_some()
            && let Some(index) = self
                .inputs
                .iter()
                .position(|input| matches!(input, Input::PendingPaste))
        {
            if self.search.is_some() {
                self.inputs.remove(index);
                match result {
                    Ok(text) if !text.is_empty() => self.search_push(&text),
                    Ok(_) => {}
                    Err(error) => log::warn!("Clipboard paste failed: {error}"),
                }
                return None;
            }
            match result {
                Ok(text) if !text.is_empty() => {
                    let bracketed = self.session.processor().input_modes().bracketed_paste;
                    if confirm_paste && !bracketed && confirm::needs_paste_confirm(&text) {
                        self.inputs.remove(index);
                        return Some(text);
                    }
                    self.inputs[index] = Input::Paste(text);
                }
                Ok(_) => {
                    self.inputs.remove(index);
                }
                Err(error) => {
                    log::warn!("Clipboard paste failed: {error}");
                    self.inputs.remove(index);
                }
            }
        }
        None
    }

    pub(super) fn commit_paste(&mut self, text: String) {
        if text.is_empty() || self.inputs.len() >= INPUT_LIMIT {
            return;
        }
        self.follow_output();
        self.push_input(Input::Paste(text));
    }

    pub(super) fn finish_osc52_read(
        &mut self,
        query: ClipboardQuery,
        result: std::result::Result<String, String>,
    ) {
        let text = match result {
            Ok(text) => text,
            Err(error) => {
                log::warn!("OSC 52 clipboard read failed: {error}");
                return;
            }
        };
        let mut reply = Vec::new();
        if let Err(error) = query.encode_reply(&text, &mut reply) {
            log::warn!("OSC 52 reply allocation failed: {error}");
            return;
        }
        for chunk in reply.chunks(MAX_INPUT_BYTES) {
            self.push_input(Input::Raw(chunk.to_vec()));
        }
    }

    pub(super) fn key(&mut self, event: &KeyEvent, copy_pending: &mut bool) -> Option<String> {
        if self.vi.is_some() {
            self.vi_key(event, copy_pending)
        } else {
            let kitty = !self
                .session
                .processor()
                .input_modes()
                .kitty_keyboard
                .is_empty();
            if let Some(input) = input::key(event, self.modifiers, kitty) {
                self.follow_output();
                self.push_input(input);
            }
            None
        }
    }

    pub(super) fn key_release(&mut self, event: &KeyEvent) {
        if self.vi.is_none()
            && self.search.is_none()
            && self.hints.is_none()
            && self
                .session
                .processor()
                .input_modes()
                .kitty_keyboard
                .report_events()
            && let Some(input) = input::key(event, self.modifiers, true)
        {
            self.push_input(input);
        }
    }

    pub(super) fn toggle_hints(&mut self) -> bool {
        if self.hints.is_some() {
            self.close_hints();
            return true;
        }
        if self.search.is_some() {
            self.close_search();
        }
        let columns = usize::from(self.session.geometry().pty_size().columns);
        let viewport = self.session.screen().viewport();
        let processor = self.session.processor();
        let Some(mode) = hints::HintMode::scan(
            viewport,
            processor.styles(),
            processor.hyperlinks(),
            columns,
        ) else {
            return false;
        };
        self.hints = Some(mode);
        self.clear_preedit();
        self.set_ime_allowed();
        self.invalidate_overlay();
        true
    }

    pub(super) fn close_hints(&mut self) {
        if self.hints.take().is_some() {
            self.set_ime_allowed();
            self.update_ime_area();
            self.invalidate_overlay();
        }
    }

    pub(super) fn hints_key(
        &mut self,
        event: &KeyEvent,
        copy_pending: &mut bool,
        primary: &mut Option<clipboard::Primary>,
        hyperlink_open_pending: &mut bool,
    ) -> Option<String> {
        let ctrl = self.modifiers.control_key();
        let alt = self.modifiers.alt_key();
        let super_key = self.modifiers.super_key();
        let shift = self.modifiers.shift_key();

        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                self.close_hints();
                return None;
            }
            Key::Named(NamedKey::Backspace) if !ctrl && !alt && !super_key => {
                if let Some(hints) = &mut self.hints
                    && hints.backspace()
                {
                    self.invalidate_overlay();
                }
                return None;
            }
            Key::Character(text) if !ctrl && !alt && !super_key => {
                let mut chars = text.chars();
                let ch = chars.next()?;
                if chars.next().is_some() || !ch.is_ascii_alphabetic() {
                    return None;
                }
                let resolution = self.hints.as_mut()?.push(ch, shift);
                match resolution {
                    HintResolution::Pending => {
                        self.invalidate_overlay();
                    }
                    HintResolution::NoMatch => {
                        self.close_hints();
                    }
                    HintResolution::Matched {
                        kind,
                        text,
                        open_or_insert,
                    } => {
                        self.close_hints();
                        if open_or_insert {
                            if kind == HintKind::Url {
                                self.open_uri(text, hyperlink_open_pending);
                                return None;
                            }
                            self.follow_output();
                            self.push_input(Input::Paste(text));
                            return None;
                        }
                        publish_primary_text(text.clone(), self.clipboard.as_ref(), primary);
                        if !*copy_pending {
                            match clipboard::write(
                                text.clone(),
                                self.clipboard.clone(),
                                self.proxy.clone(),
                            ) {
                                Ok(()) => *copy_pending = true,
                                Err(error) => {
                                    log::warn!(
                                        "Failed to start the hint clipboard copy worker: {error}"
                                    );
                                }
                            }
                        }
                        return Some(format_copied_preview(&text));
                    }
                }
            }
            _ => {}
        }
        None
    }

    pub(super) fn toggle_search(&mut self) {
        if self.search.is_some() {
            self.close_search();
            return;
        }
        if self.vi.is_some() {
            self.open_vi_search(vi::Direction::Forward);
            return;
        }
        let columns = usize::from(self.session.geometry().pty_size().columns);
        let screen = self.session.screen();
        let buffer = screen.active_buffer();
        let view = screen.view();
        let viewport = screen.viewport().range();
        let bar =
            search::SearchBar::open(self.search_saved.take(), buffer, view, columns, viewport);
        let target = bar.current_match();
        self.search = Some(bar);
        if let Some(pos) = target {
            self.reveal_search_row(pos.row);
        }
        self.set_ime_allowed();
        self.update_ime_area();
        self.invalidate_overlay();
    }

    pub(super) fn open_vi_search(&mut self, direction: vi::Direction) {
        let Some(vi) = &self.vi else {
            return;
        };
        let bar =
            search::SearchBar::open_vi(self.search_saved.as_ref(), direction, vi.cursor, vi.buffer);
        self.search = Some(bar);
        self.set_ime_allowed();
        self.update_ime_area();
        self.invalidate_overlay();
    }

    pub(super) fn close_search(&mut self) {
        if let Some(bar) = self.search.take() {
            if bar.vi_mode {
                if let Some(origin) = bar.vi_origin
                    && let Some(vi) = &mut self.vi
                {
                    vi.set_cursor(origin);
                    self.reveal_vi_cursor();
                    self.update_vi_selection();
                }
            } else {
                let fallback = self.search_saved.take();
                self.search_saved = Some(bar.into_saved(fallback));
            }
            self.clear_preedit();
            self.set_ime_allowed();
            self.update_ime_area();
            self.invalidate_overlay();
        }
    }

    fn confirm_vi_search(&mut self) {
        let Some(bar) = self.search.take() else {
            return;
        };
        let empty = bar.query.is_empty();
        let invalid = bar.invalid_regex;
        let matched = bar.current_match();
        let origin = bar.vi_origin;
        let fallback = self.search_saved.take();
        if !invalid {
            let saved = bar.into_saved(fallback);
            let target = if empty {
                let columns = usize::from(self.session.geometry().pty_size().columns);
                let view = self.session.screen().view();
                origin.and_then(|pos| saved.find_from(view, columns, pos, false))
            } else {
                matched
            };
            if let Some(pos) = target
                && let Some(vi) = &mut self.vi
            {
                vi.set_cursor(pos);
            }
            self.search_saved = Some(saved);
        } else {
            self.search_saved = fallback;
            if let Some(origin) = origin
                && let Some(vi) = &mut self.vi
            {
                vi.set_cursor(origin);
            }
        }
        self.clear_preedit();
        self.set_ime_allowed();
        if self.vi.is_some() {
            self.reveal_vi_cursor();
            self.update_vi_selection();
        }
        self.update_ime_area();
        self.invalidate_overlay();
    }

    pub(super) fn search_key(&mut self, event: &KeyEvent) {
        let ctrl = self.modifiers.control_key();
        let alt = self.modifiers.alt_key();
        let shift = self.modifiers.shift_key();
        let super_key = self.modifiers.super_key();
        let vi_mode = self.search.as_ref().is_some_and(|bar| bar.vi_mode);

        let mut recompute = false;
        let mut step: Option<bool> = None;

        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                self.close_search();
                return;
            }
            Key::Named(NamedKey::Enter) if !ctrl && !alt && !super_key => {
                if vi_mode && !shift {
                    self.confirm_vi_search();
                    return;
                }
                step = Some(!shift);
            }
            Key::Named(NamedKey::F3) if !ctrl && !alt && !super_key => {
                step = Some(!shift);
            }
            Key::Named(NamedKey::ArrowDown) if !ctrl && !alt && !super_key => {
                step = Some(true);
            }
            Key::Named(NamedKey::ArrowUp) if !ctrl && !alt && !super_key => {
                step = Some(false);
            }
            Key::Named(NamedKey::Backspace) if !alt && !super_key => {
                if let Some(bar) = &mut self.search {
                    recompute = if ctrl {
                        bar.delete_word()
                    } else {
                        bar.backspace()
                    };
                }
            }
            Key::Character(text) if alt && !ctrl && !super_key => {
                if text.eq_ignore_ascii_case("c") {
                    if let Some(bar) = &mut self.search {
                        bar.toggle_case();
                        recompute = true;
                    }
                } else if text.eq_ignore_ascii_case("r")
                    && let Some(bar) = &mut self.search
                {
                    bar.toggle_regex();
                    recompute = true;
                }
            }
            Key::Character(text) if ctrl && !alt && !super_key => {
                if text.eq_ignore_ascii_case("w") {
                    if let Some(bar) = &mut self.search {
                        recompute = bar.delete_word();
                    }
                } else if text.eq_ignore_ascii_case("u") {
                    if let Some(bar) = &mut self.search {
                        recompute = bar.clear();
                    }
                } else if text.eq_ignore_ascii_case("g") {
                    step = Some(!shift);
                }
            }
            Key::Named(NamedKey::Space) if !ctrl && !alt && !super_key => {
                if let Some(bar) = &mut self.search {
                    recompute = bar.push(" ");
                }
            }
            Key::Character(text) if !ctrl && !alt && !super_key => {
                if let Some(bar) = &mut self.search {
                    recompute = bar.push(text);
                }
            }
            _ => return,
        }

        if let Some(forward) = step {
            let target = self.search.as_mut().and_then(|bar| bar.step(forward));
            if let Some(pos) = target {
                self.sync_search_target(Some(pos));
            }
            self.invalidate_overlay();
        } else if recompute {
            self.recompute_search(true);
        }
    }

    fn search_push(&mut self, text: &str) {
        let changed = self.search.as_mut().is_some_and(|bar| bar.push(text));
        if changed {
            self.recompute_search(true);
        }
    }

    fn recompute_search(&mut self, reveal: bool) {
        let columns = usize::from(self.session.geometry().pty_size().columns);
        let (target, origin) = {
            let screen = self.session.screen();
            let view = screen.view();
            let viewport = screen.viewport().range();
            let Some(bar) = &mut self.search else {
                return;
            };
            bar.recompute(view, columns, viewport);
            (bar.current_match(), bar.vi_origin)
        };
        if reveal {
            self.sync_search_target(target.or(origin));
        }
        self.update_ime_area();
        self.invalidate_overlay();
    }

    fn sync_search_target(&mut self, target: Option<CellPosition>) {
        if let Some(pos) = target {
            if let Some(vi) = &mut self.vi {
                vi.set_cursor(pos);
                self.reveal_vi_cursor();
                self.update_vi_selection();
            } else {
                self.reveal_search_row(pos.row);
            }
        }
    }

    fn refresh_search(&mut self) {
        if self.search.is_none() {
            return;
        }
        let columns = usize::from(self.session.geometry().pty_size().columns);
        let screen = self.session.screen();
        let buffer = screen.active_buffer();
        let view = screen.view();
        let viewport = screen.viewport().range();
        if let Some(bar) = &mut self.search {
            bar.refresh(buffer, view, columns, viewport);
            self.dirty = true;
        }
    }

    fn reveal_search_row(&mut self, row: usize) {
        let screen = self.session.screen();
        if screen.active_buffer() != ScreenBuffer::Primary {
            return;
        }
        let viewport = screen.viewport();
        let range = viewport.range();
        if range.contains(&row) {
            return;
        }
        let history = screen.view().history_rows();
        let rows = viewport.rows();
        let target_top = row.saturating_sub(rows / 2);
        let desired_offset = history.saturating_sub(target_top);
        let current_offset = screen.viewport_offset();
        let delta = desired_offset as isize - current_offset as isize;
        if delta != 0 {
            self.scroll_history(delta);
        }
    }

    pub(super) fn follow_output(&mut self) {
        let was_scrolled = self.session.screen().viewport_offset() != 0;
        self.session.follow_output();
        self.clear_selection();
        self.restart_blink = true;
        if was_scrolled {
            self.update_ime_area();
            if self.visible {
                self.window.request_redraw();
            }
        }
    }

    fn pointer_pixel(&self, clamp: bool) -> Option<PixelPosition> {
        let pointer = self.pointer?;
        let area = self.session.geometry().text_area_pixels()?;
        let x = pointer.x - f64::from(self.origin.x);
        let y = pointer.y - f64::from(self.origin.y);
        if !clamp
            && (x < 0.0
                || y < 0.0
                || x >= f64::from(area.width.get())
                || y >= f64::from(area.height.get()))
        {
            return None;
        }
        Some(PixelPosition {
            x: (x.max(0.0) as u32).min(area.width.get() - 1),
            y: (y.max(0.0) as u32).min(area.height.get() - 1),
        })
    }

    pub(super) fn mouse_event(&mut self, event: MouseEvent) {
        if matches!(event, MouseEvent::Motion(_)) && !self.mouse_reporting() {
            return;
        }
        let clamp = self.pressed.is_some() || matches!(event, MouseEvent::Release(_));
        if let Some(pixel) = self.pointer_pixel(clamp) {
            self.push_input(Input::Mouse(
                event,
                pixel,
                input::terminal_modifiers(self.modifiers),
            ));
        }
    }

    fn mouse_reporting(&self) -> bool {
        self.vi.is_none()
            && self.session.screen().viewport_offset() == 0
            && self.session.processor().input_modes().mouse_tracking != MouseTracking::Disabled
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn button(
        &mut self,
        state: ElementState,
        button: winit::event::MouseButton,
        primary: &mut Option<clipboard::Primary>,
        hyperlink_open_pending: &mut bool,
        copy_pending: &mut bool,
        copy_on_select: bool,
        scrollbar_policy: ScrollbarPolicy,
    ) -> Option<String> {
        if !state.is_pressed()
            && button == winit::event::MouseButton::Left
            && self.scrollbar_drag.take().is_some()
        {
            self.update_scrollbar_hover(scrollbar_policy);
            self.invalidate_overlay();
            return None;
        }
        if state.is_pressed()
            && button == winit::event::MouseButton::Left
            && scrollbar_policy != ScrollbarPolicy::Never
            && let Some(pos) = self.pointer
            && let Some(metrics) = self.scrollbar_metrics(self.window.scale_factor())
        {
            let local_x = pos.x - f64::from(self.origin.x);
            let local_y = pos.y - f64::from(self.origin.y);
            if metrics.contains(local_x, local_y) {
                let grab = if metrics.thumb_contains_y(local_y) {
                    local_y - f64::from(metrics.thumb.origin.y)
                } else {
                    let center = f64::from(metrics.thumb.size.height.get()) * 0.5;
                    let target = metrics.offset_for_y(local_y, center);
                    let current = self.session.screen().viewport_offset();
                    if target != current {
                        self.scroll_history(target as isize - current as isize);
                    }
                    center
                };
                self.scrollbar_drag = Some(grab);
                self.scrollbar_hovered = true;
                self.invalidate_overlay();
                return None;
            }
        }
        if self.vi.is_some() {
            return None;
        }
        let button = match button {
            winit::event::MouseButton::Left => MouseButton::Left,
            winit::event::MouseButton::Right => MouseButton::Right,
            winit::event::MouseButton::Middle => MouseButton::Middle,
            winit::event::MouseButton::Back => MouseButton::Button8,
            winit::event::MouseButton::Forward => MouseButton::Button9,
            _ => return None,
        };
        if state.is_pressed() {
            let ctrl_only = self.modifiers.control_key()
                && !self.modifiers.alt_key()
                && !self.modifiers.super_key();
            if button == MouseButton::Left && ctrl_only {
                if let Some(link_id) = self.hyperlink_at_pointer() {
                    self.open_hyperlink(link_id, hyperlink_open_pending);
                    return None;
                }
                if let Some(detected) = self.detected_url_at_pointer() {
                    self.open_uri(detected.uri, hyperlink_open_pending);
                    return None;
                }
            }
            let mouse_reporting = self.mouse_reporting();
            let local = !mouse_reporting || self.modifiers.shift_key();
            if button != MouseButton::Left || !local {
                self.selection_click = None;
                self.hyperlink_click = None;
            }
            if button == MouseButton::Left && local {
                let candidate = (!self.modifiers.shift_key()
                    && !self.modifiers.alt_key()
                    && !self.modifiers.super_key()
                    && !mouse_reporting)
                    .then(|| self.hyperlink_at_pointer())
                    .flatten();
                self.start_selection();
                self.hyperlink_click = candidate.filter(|_| self.selection.is_none());
            } else if button == MouseButton::Middle && local {
                if self.pointer_pixel(false).is_some() {
                    self.paste(ClipboardType::Primary, primary.as_ref());
                }
            } else if mouse_reporting && self.pointer_pixel(false).is_some() {
                self.pressed = Some(button);
                self.mouse_event(MouseEvent::Press(button));
            }
            None
        } else if button == MouseButton::Left && self.selection_drag.is_some() {
            self.extend_selection();
            let moved = self
                .selection_drag
                .as_ref()
                .is_some_and(selection::Drag::moved);
            self.selection_drag = None;
            let clicked_link = self.hyperlink_click.take();
            let mut copied_notice = None;
            if !moved
                && self.selection.is_none()
                && let Some(link_id) = clicked_link
                && self.hyperlink_at_pointer() == Some(link_id)
            {
                self.open_hyperlink(link_id, hyperlink_open_pending);
            } else {
                self.publish_primary_selection(primary);
                if copy_on_select && self.selection.is_some() {
                    copied_notice = self.copy(copy_pending);
                }
            }
            self.update_hovered_hyperlink();
            copied_notice
        } else if self.pressed == Some(button) {
            self.mouse_event(MouseEvent::Release(button));
            self.pressed = None;
            None
        } else {
            None
        }
    }

    fn open_hyperlink(&self, link_id: HyperlinkId, hyperlink_open_pending: &mut bool) {
        if let Some(link) = self.session.processor().hyperlinks().get(link_id) {
            self.open_uri(link.uri().to_owned(), hyperlink_open_pending);
        }
    }

    fn open_uri(&self, uri: String, hyperlink_open_pending: &mut bool) {
        if *hyperlink_open_pending {
            return;
        }
        match directory::open_hyperlink(uri, self.proxy.clone()) {
            Ok(()) => *hyperlink_open_pending = true,
            Err(error) => {
                log::warn!("Failed to start the hyperlink worker: {error}");
            }
        }
    }

    pub(super) fn scroll(&mut self, delta: MouseScrollDelta) {
        let horizontal = match delta {
            MouseScrollDelta::LineDelta(x, y) => {
                self.wheel += f64::from(y);
                x
            }
            MouseScrollDelta::PixelDelta(position) => {
                self.wheel += position.y
                    / f64::from(self.session.geometry().cell_pixels().unwrap().height.get());
                0.0
            }
        };
        let steps = (self.wheel as isize).clamp(-32, 32);
        self.wheel = self.wheel.fract();
        if self.vi.is_some()
            || self.session.screen().active_buffer() == ScreenBuffer::Primary
                && (!self.mouse_reporting() || self.modifiers.shift_key())
        {
            self.scroll_history(steps * 3);
        } else {
            let event = if steps > 0 {
                MouseEvent::WheelUp
            } else {
                MouseEvent::WheelDown
            };
            for _ in 0..steps.unsigned_abs() {
                self.mouse_event(event);
            }
            if horizontal != 0.0 && self.mouse_reporting() {
                self.mouse_event(if horizontal > 0.0 {
                    MouseEvent::WheelLeft
                } else {
                    MouseEvent::WheelRight
                });
            }
        }
    }

    pub(super) fn scroll_history(&mut self, steps: isize) {
        if self.session.scroll_history(steps) {
            self.close_hints();
            self.selection_click = None;
            self.selection_drag = None;
            self.hyperlink_click = None;
            self.hovered_block = None;
            self.scroll_vi_cursor();
            self.update_hovered_hyperlink();
            self.invalidate_overlay();
        }
    }

    pub(super) fn jump_to_row(&mut self, row: usize) {
        let screen = self.session.screen();
        if screen.active_buffer() != ScreenBuffer::Primary {
            return;
        }
        let history = screen.view().history_rows();
        let desired_offset = history.saturating_sub(row);
        let current_offset = screen.viewport_offset();
        let delta = desired_offset as isize - current_offset as isize;
        if delta != 0 {
            self.scroll_history(delta);
        }
    }

    pub(super) fn jump_prompt(&mut self, forward: bool) {
        let screen = self.session.screen();
        if screen.active_buffer() != ScreenBuffer::Primary {
            return;
        }
        let view = screen.view();
        let history = view.history_rows();
        let viewport = screen.viewport();
        let current_offset = screen.viewport_offset();
        let target = if forward {
            if current_offset == 0 {
                return;
            }
            view.next_prompt(viewport.range().start)
        } else {
            let anchor = if current_offset == 0 {
                history + screen.cursor().row
            } else {
                viewport.range().start
            };
            view.previous_prompt(anchor)
        };
        match target {
            Some(row) => {
                let desired_offset = history.saturating_sub(row);
                let delta = desired_offset as isize - current_offset as isize;
                if delta != 0 {
                    self.scroll_history(delta);
                }
            }
            None if forward => self.follow_output(),
            None => {}
        }
    }

    pub(super) fn copy_last_output(&self, copy_pending: &mut bool) -> Option<String> {
        if *copy_pending {
            return None;
        }
        let screen = self.session.screen();
        let view = screen.view();
        let first_row = screen.viewport().range().start;
        let offset = screen.viewport_offset();
        let text = blocks::active_target_block(view, first_row, offset)
            .and_then(|block| blocks::extract_output(view, &block))
            .or_else(|| {
                let range = view.last_command_output()?;
                let s = view.selection_text(range).ok()?.to_string();
                (!s.is_empty()).then_some(s)
            })?;
        let notice = format_copied_summary(&text);
        match clipboard::write(text, self.clipboard.clone(), self.proxy.clone()) {
            Ok(()) => {
                *copy_pending = true;
                Some(notice)
            }
            Err(error) => {
                log::warn!("Failed to start the clipboard copy worker: {error}");
                None
            }
        }
    }

    fn update_selection_scroll(&mut self, y: f64) {
        let Some(drag) = &mut self.selection_drag else {
            return;
        };
        let y = y - f64::from(self.origin.y);
        let screen = self.session.screen();
        let band = f64::from(self.session.geometry().cell_pixels().unwrap().height.get()) * 0.4;
        let rows = if let Some(area) = self.session.geometry().text_area_pixels()
            && screen.active_buffer() == ScreenBuffer::Primary
        {
            let offset = screen.viewport_offset();
            if y < band && offset < screen.history().len() {
                2
            } else if y >= f64::from(area.height.get()) - band && offset > 0 {
                -2
            } else {
                0
            }
        } else {
            0
        };
        drag.set_edge_scroll(rows, Instant::now());
    }

    pub(super) fn tick_selection_scroll(&mut self, now: Instant) {
        let Some(drag) = &mut self.selection_drag else {
            return;
        };
        let Some(rows) = drag.edge_scroll.as_mut().and_then(|scroll| scroll.due(now)) else {
            return;
        };
        if self.session.scroll_history(rows) {
            self.extend_selection();
            self.window.request_redraw();
        } else {
            drag.edge_scroll = None;
        }
    }

    fn selection_position(&self, clamp: bool) -> Option<CellPosition> {
        let pixel = self.pointer_pixel(clamp)?;
        let mut position = self.session.geometry().cell_at(pixel)?;
        position.row += self.session.screen().viewport().range().start;
        Some(position)
    }

    fn start_selection(&mut self) {
        let previous = self.selection_click.take();
        let shift = self.modifiers.shift_key();
        let block = self.modifiers.alt_key();
        let Some(position) = self.selection_position(false) else {
            if !shift {
                self.clear_selection();
            }
            return;
        };
        let now = Instant::now();
        let next = previous.and_then(|click| click.next(position, now, shift, block));
        let repeated = next.is_some();
        let click = next.unwrap_or_else(|| selection::Click::new(position, now, shift, block));
        if shift
            && !block
            && !repeated
            && let Some(anchor) = self
                .selection
                .as_ref()
                .and_then(|selection| selection.anchor.as_ref())
        {
            self.selection_drag = Some(anchor.clone().drag(true));
        } else {
            self.clear_selection();
            self.selection_drag = Some(click.drag(self.session.screen().view()));
        }
        self.extend_selection();
        self.selection_click = Some(click);
    }

    fn extend_selection(&mut self) {
        let Some(focus) = self.selection_position(true) else {
            return;
        };
        let Some(drag) = &mut self.selection_drag else {
            return;
        };
        if !drag.move_to(focus) {
            return;
        }
        if drag.moved() {
            self.selection_click = None;
            self.hyperlink_click = None;
        }
        let view = self.session.screen().view();
        let kind = drag.anchor.kind;
        let range = drag.range(view);
        if self
            .selection
            .as_ref()
            .map(|selection| (&selection.range, selection.kind))
            == range.as_ref().map(|range| (range, kind))
        {
            return;
        }
        match (&mut self.selection, range) {
            (Some(selection), Some(range)) => selection.set_range(view, range, kind),
            (selection, Some(range)) => {
                *selection = Some(selection::Selection::new(
                    view,
                    range,
                    kind,
                    Some(drag.anchor.clone()),
                ));
            }
            (selection, None) => *selection = None,
        }
        self.invalidate_overlay();
    }

    pub(super) fn clear_selection(&mut self) {
        self.selection_click = None;
        self.selection_drag = None;
        if self.selection.take().is_some() {
            self.invalidate_overlay();
        }
    }

    pub(super) fn copy(&self, copy_pending: &mut bool) -> Option<String> {
        if *copy_pending {
            return None;
        }
        let text = self.selected_text()?;
        let notice = format_copied_summary(&text);
        match clipboard::write(text, self.clipboard.clone(), self.proxy.clone()) {
            Ok(()) => {
                *copy_pending = true;
                Some(notice)
            }
            Err(error) => {
                log::warn!("Failed to start the clipboard copy worker: {error}");
                None
            }
        }
    }

    fn selected_text(&self) -> Option<String> {
        let selection = self.selection.as_ref()?;
        let linewise = self.vi.as_ref().is_some_and(vi::Mode::linewise);
        selection.extract_text(self.session.screen().view(), linewise)
    }

    fn publish_primary_selection(&self, primary: &mut Option<clipboard::Primary>) {
        if let Some(text) = self.selected_text() {
            publish_primary_text(text, self.clipboard.as_ref(), primary);
        }
    }

    pub(super) fn paste(&mut self, clipboard: ClipboardType, primary: Option<&clipboard::Primary>) {
        if self.paste_worker || self.inputs.len() >= INPUT_LIMIT {
            return;
        }
        match clipboard::read(
            clipboard,
            self.clipboard.clone(),
            primary,
            self.id,
            self.proxy.clone(),
        ) {
            Ok(()) => {
                self.follow_output();
                self.push_input(Input::PendingPaste);
                self.paste_worker = true;
                self.paste_deadline = Some(Instant::now() + PASTE_TIMEOUT);
            }
            Err(error) => log::warn!("Failed to start the clipboard paste worker: {error}"),
        }
    }
}

fn format_copied_summary(text: &str) -> String {
    let lines = text.lines().count();
    if lines > 1 {
        format!("Copied {lines} lines")
    } else {
        format_copied_preview(text)
    }
}

fn format_copied_preview(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "Copied".to_owned();
    }
    const LIMIT: usize = 36;
    let mut chars = trimmed.chars();
    let preview: String = chars.by_ref().take(LIMIT).collect();
    if chars.next().is_some() {
        format!("Copied: {preview}...")
    } else {
        format!("Copied: {preview}")
    }
}

fn ensure_primary(primary: &mut Option<clipboard::Primary>) -> Option<clipboard::Primary> {
    if primary.is_none() {
        *primary = match clipboard::Primary::new() {
            Ok(primary) => Some(primary),
            Err(error) => {
                log::warn!("Failed to start the primary selection worker: {error}");
                return None;
            }
        };
    }
    primary.clone()
}

fn publish_primary_text(
    text: String,
    data_device: Option<&clipboard::DataDeviceHandle>,
    primary: &mut Option<clipboard::Primary>,
) {
    if let Some(dd) = data_device
        && dd.has_primary_selection()
    {
        let fallback = if dd.has_data_control() {
            ensure_primary(primary)
        } else {
            None
        };
        if dd.publish_primary(text.clone(), fallback) {
            return;
        }
    }
    if let Some(primary) = ensure_primary(primary) {
        primary.publish(text);
    }
}

fn shell_quote_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if !text.is_empty()
        && text.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(ch, '/' | '.' | '_' | '-' | '+' | ':' | '@' | ',' | '=')
        })
    {
        return text.into_owned();
    }
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('\'');
    for ch in text.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::shell_quote_path;

    #[test]
    fn shell_quote_path_escapes_spaces_and_quotes_safely() {
        assert_eq!(shell_quote_path(Path::new("/tmp/foo.txt")), "/tmp/foo.txt");
        assert_eq!(
            shell_quote_path(Path::new("/tmp/my file.txt")),
            "'/tmp/my file.txt'"
        );
        assert_eq!(
            shell_quote_path(Path::new("/tmp/it's $HOME.txt")),
            "'/tmp/it'\\''s $HOME.txt'"
        );
        assert_eq!(shell_quote_path(Path::new("")), "''");
    }
}
