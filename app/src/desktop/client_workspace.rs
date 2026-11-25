//! Tab, split-pane, titlebar/sidebar layout, theme, and window chrome actions for `Client`.

use std::num::NonZeroU32;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Instant;

use lune_terminal::{PixelPosition, PixelSize};
use winit::dpi::PhysicalPosition;
use winit::event_loop::ActiveEventLoop;
use winit::window::{ResizeDirection, Theme};

use crate::GridGeometry;

use super::config::{TabPosition, TerminalMode};
use super::paint::NoticeKind;
use super::split::{Axis, Direction as SplitDirection, SplitTree};
use super::tab::{Pane, Tab};
use super::titlebar::{self, TitlebarAction};
use super::wayland_decorations::{WindowButtons, resolve_window_buttons};
use super::{
    Client, MIN_SPLIT_COLUMNS, MIN_SPLIT_ROWS, PaneId, Result, SPLIT_RESIZE_STEP, content_rect,
    tab_bar, theme,
};

impl Client {
    pub(super) fn split_pane(&mut self, axis: Axis) -> Result<()> {
        let cwd = self.tabs[self.active].focused_pane().working_directory();
        self.split_pane_with_cwd(axis, cwd)
    }

    pub(super) fn split_pane_with_cwd(&mut self, axis: Axis, cwd: Option<PathBuf>) -> Result<()> {
        let cell = self.painter.metrics().cell;
        let divider = self.divider_thickness();
        let gutter = self.block_gutter();
        let tab = &mut self.tabs[self.active];
        if tab.zoomed.take().is_some() {
            tab.reflow(self.content_area, divider, cell, gutter, true);
        }
        let target_id = tab.focused;
        let Some(target_rect) = tab.layout.rect_of(target_id) else {
            return Ok(());
        };
        let half_size = match axis {
            Axis::Columns => {
                let avail = target_rect.size.width.get().saturating_sub(divider);
                let Some(width) = NonZeroU32::new(avail / 2) else {
                    return Ok(());
                };
                PixelSize {
                    width,
                    height: target_rect.size.height,
                }
            }
            Axis::Rows => {
                let avail = target_rect.size.height.get().saturating_sub(divider);
                let Some(height) = NonZeroU32::new(avail / 2) else {
                    return Ok(());
                };
                PixelSize {
                    width: target_rect.size.width,
                    height,
                }
            }
        };
        let Some(grid) = GridGeometry::fit(half_size, cell) else {
            return Ok(());
        };
        let pty = grid.pty_size();
        if pty.columns < MIN_SPLIT_COLUMNS || pty.rows < MIN_SPLIT_ROWS {
            return Ok(());
        }
        let new_id = self.allocate_pane_id()?;
        let mut pane = Pane::spawn(
            new_id,
            self.window.clone(),
            self.proxy.clone(),
            grid,
            self.pane_config(),
            cwd,
            None,
        )?;
        self.readiness
            .arm(new_id, &mut pane.registration, pane.session.interests())?;
        let tab = &mut self.tabs[self.active];
        if !tab.tree.split(target_id, axis, new_id) {
            return Ok(());
        }
        tab.panes.push(pane);
        self.focus_pane(new_id);
        self.reflow_active_tab();
        self.painter.invalidate_overlay();
        self.window.request_redraw();
        Ok(())
    }

    pub(super) fn focus_pane(&mut self, target_id: PaneId) {
        let tab = &mut self.tabs[self.active];
        if tab.focused == target_id || tab.pane(target_id).is_none() {
            return;
        }
        let old = tab.focused_pane_mut();
        old.active = false;
        old.focus(false);
        tab.focused = target_id;
        self.activate_focused_pane();
    }

    pub(super) fn focus_pane_direction(&mut self, direction: SplitDirection) {
        if self.tabs[self.active].panes.len() <= 1 {
            return;
        }
        let cell = self.painter.metrics().cell;
        let divider = self.divider_thickness();
        let gutter = self.block_gutter();
        let tab = &mut self.tabs[self.active];
        if tab.zoomed.take().is_some() {
            tab.reflow(self.content_area, divider, cell, gutter, true);
        }
        if let Some(next_id) = SplitTree::focus_neighbor(tab.focused, direction, &tab.layout) {
            self.focus_pane(next_id);
        }
    }

    pub(super) fn resize_pane_direction(&mut self, direction: SplitDirection) {
        let tab = &mut self.tabs[self.active];
        if tab.panes.len() <= 1 || tab.is_zoomed() {
            return;
        }
        if tab.tree.resize(tab.focused, direction, SPLIT_RESIZE_STEP) {
            self.reflow_active_tab();
            self.painter.invalidate_overlay();
            self.window.request_redraw();
        }
    }

    pub(super) fn toggle_pane_zoom(&mut self) {
        let tab = &mut self.tabs[self.active];
        if tab.panes.len() <= 1 {
            return;
        }
        tab.zoomed = if tab.zoomed.is_some() {
            None
        } else {
            Some(tab.focused)
        };
        self.dragging_divider = None;
        self.reflow_active_tab();
        self.tabs[self.active].focused_pane_mut().update_ime_area();
        self.painter.reset_cursor_motion();
        self.painter.invalidate_overlay();
        self.update_cursor_icon();
        self.window.request_redraw();
    }

    pub(super) fn toggle_broadcast_input(&mut self) {
        let tab = &mut self.tabs[self.active];
        if tab.panes.len() <= 1 {
            tab.broadcast_input = false;
            self.show_notice(
                NoticeKind::Warning,
                "Broadcast input requires split panes".into(),
            );
            return;
        }
        tab.broadcast_input = !tab.broadcast_input;
        let count = tab.panes.len();
        let active = tab.broadcast_input;
        let notice = if active {
            format!("Broadcast input: ON ({count} panes)")
        } else {
            "Broadcast input: OFF".into()
        };
        self.show_notice(NoticeKind::Success, notice);
    }

    pub(super) fn swap_pane_next(&mut self) {
        if self.tabs[self.active].panes.len() <= 1 {
            return;
        }
        let cell = self.painter.metrics().cell;
        let divider = self.divider_thickness();
        let gutter = self.block_gutter();
        let tab = &mut self.tabs[self.active];
        if tab.zoomed.take().is_some() {
            tab.reflow(self.content_area, divider, cell, gutter, true);
        }
        let order: Vec<PaneId> = tab.layout.panes.iter().map(|p| p.id).collect();
        let Some(pos) = order.iter().position(|&id| id == tab.focused) else {
            return;
        };
        let next_id = order[(pos + 1) % order.len()];
        if tab.tree.swap_panes(tab.focused, next_id) {
            self.dragging_divider = None;
            self.reflow_active_tab();
            self.painter.reset_cursor_motion();
            self.painter.invalidate_overlay();
            self.window.request_redraw();
        }
    }

    pub(super) fn detach_pane_to_tab(&mut self) {
        if self.tabs[self.active].panes.len() <= 1 {
            self.show_notice(
                NoticeKind::Warning,
                "Cannot detach the only pane in a tab".into(),
            );
            return;
        }
        let source_index = self.active;
        let target_id = self.tabs[source_index].focused;
        let (mut detached_pane, surviving_focus) = {
            let tab = &mut self.tabs[source_index];
            let Some(pos) = tab.panes.iter().position(|pane| pane.id == target_id) else {
                return;
            };
            let pane = tab.panes.remove(pos);
            let next_focus = tab.tree.remove(target_id);
            if tab.zoomed == Some(target_id) || tab.panes.len() <= 1 {
                tab.zoomed = None;
            }
            if tab.panes.len() <= 1 {
                tab.broadcast_input = false;
            }
            (pane, next_focus)
        };
        if let Some(next_id) =
            surviving_focus.or_else(|| self.tabs[source_index].panes.first().map(|pane| pane.id))
        {
            self.tabs[source_index].focused = next_id;
        }
        for pane in &mut self.tabs[source_index].panes {
            pane.active = false;
            pane.visible = false;
            pane.pointer = None;
            pane.focus(false);
        }
        detached_pane.active = true;
        detached_pane.visible = true;
        let new_tab = Tab::new(detached_pane, self.content_area);
        let insert_at = source_index + 1;
        self.tabs.insert(insert_at, new_tab);
        self.active = insert_at;
        self.dragging_divider = None;
        self.resize_pending = true;
        self.apply_resize();
        self.activate_tab();
        self.show_notice(NoticeKind::Success, "Moved pane to new tab".into());
    }

    pub(super) fn reflow_active_tab(&mut self) {
        let cell = self.painter.metrics().cell;
        let divider = self.divider_thickness();
        let gutter = self.block_gutter();
        self.tabs[self.active].reflow(self.content_area, divider, cell, gutter, true);
        self.tabs[self.active].focused_pane_mut().update_ime_area();
    }

    pub(super) fn move_tab(&mut self, delta: isize) {
        let Some(target) = self.active.checked_add_signed(delta) else {
            return;
        };
        if target >= self.tabs.len() {
            return;
        }
        self.tabs.swap(self.active, target);
        self.active = target;
        self.reflow_tab_bar();
        self.painter.invalidate_overlay();
        self.window.request_redraw();
    }

    pub(super) fn new_tab(&mut self) -> Result<()> {
        let cwd = self.tabs[self.active].focused_pane().working_directory();
        self.new_tab_with_cwd(cwd)
    }

    pub(super) fn new_tab_with_cwd(&mut self, cwd: Option<PathBuf>) -> Result<()> {
        self.new_tab_with_options(cwd, None, None)
    }

    pub(super) fn new_tab_with_options(
        &mut self,
        cwd: Option<PathBuf>,
        custom_title: Option<String>,
        program: Option<&[std::ffi::OsString]>,
    ) -> Result<()> {
        let id = self.allocate_pane_id()?;
        let size = self.window.inner_size();
        let scale = self.window.scale_factor();
        let metrics = self.painter.metrics();
        let chrome_cell = self.painter.chrome_metrics().cell;
        let top_bar_height =
            titlebar::Layout::height(size, chrome_cell, scale, self.mode == TerminalMode::Modern);
        let left_bar_width = titlebar::Layout::compute_sidebar_width(
            size,
            chrome_cell,
            scale,
            self.mode == TerminalMode::Modern,
            self.tab_position,
            self.sidebar_collapsed,
            self.sidebar_width_cells,
        );
        let bar_height = if self.mode == TerminalMode::Classic {
            tab_bar::Layout::height(size, metrics.cell, self.tabs.len() + 1)
        } else {
            0
        };
        let padding = self.physical_padding();
        let area = content_rect(
            size,
            left_bar_width,
            top_bar_height,
            bar_height,
            padding,
            metrics.cell,
        )
        .unwrap_or(self.content_area);
        let gutter = self.block_gutter();
        let inset = gutter.min(
            area.size
                .width
                .get()
                .saturating_sub(metrics.cell.width.get()),
        );
        let text_size = PixelSize {
            width: NonZeroU32::new(area.size.width.get().saturating_sub(inset))
                .unwrap_or(area.size.width),
            height: area.size.height,
        };
        let grid = GridGeometry::fit(text_size, metrics.cell)
            .unwrap_or(self.tabs[self.active].focused_pane().target_grid);
        let mut pane = Pane::spawn(
            id,
            self.window.clone(),
            self.proxy.clone(),
            grid,
            self.pane_config(),
            cwd,
            program,
        )?;
        pane.origin = PixelPosition {
            x: area.origin.x + inset,
            y: area.origin.y,
        };
        self.readiness
            .arm(id, &mut pane.registration, pane.session.interests())?;
        let mut tab = Tab::new(pane, area);
        tab.custom_title = custom_title.filter(|title| !title.trim().is_empty());
        self.tabs.push(tab);
        self.resize_pending = true;
        self.apply_resize();
        self.select_tab(self.tabs.len() - 1);
        Ok(())
    }

    pub(super) fn select_tab(&mut self, index: usize) {
        if index == self.active {
            return;
        }
        self.dragging_divider = None;
        let old = &mut self.tabs[self.active];
        old.focused_pane_mut().focus(false);
        for pane in &mut old.panes {
            pane.active = false;
            pane.visible = false;
            pane.pointer = None;
            pane.update_hovered_hyperlink();
        }
        self.active = index;
        self.activate_tab();
    }

    pub(super) fn activate_tab(&mut self) {
        let cell = self.painter.metrics().cell;
        let divider = self.divider_thickness();
        let gutter = self.block_gutter();
        self.tabs[self.active].reflow(self.content_area, divider, cell, gutter, true);
        self.activate_focused_pane();
        self.painter.switch_session();
        self.window.request_redraw();
    }

    pub(super) fn activate_focused_pane(&mut self) {
        self.window.set_ime_allowed(false);
        let focused_id = self.tabs[self.active].focused;
        for pane in &mut self.tabs[self.active].panes {
            pane.active = pane.id == focused_id;
            pane.modifiers = self.modifiers;
        }
        let pane = self.tabs[self.active].focused_pane_mut();
        pane.ime_enabled = self.ime_enabled;
        pane.invalidate_ime_area();
        pane.pointer = self.pointer;
        pane.set_ime_allowed();
        pane.update_ime_area();
        pane.update_hovered_hyperlink();
        if self.focused {
            pane.focus(true);
        }
        self.update_window_title();
        self.reflow_tab_bar();
        self.painter.reset_cursor_motion();
        self.painter.invalidate_overlay();
        self.update_cursor_icon();
        self.window.request_redraw();
    }

    pub(super) fn close_pane(
        &mut self,
        event_loop: &ActiveEventLoop,
        tab_index: usize,
        pane_id: PaneId,
    ) {
        if self.tabs[tab_index].panes.len() <= 1 {
            self.close_tab(event_loop, tab_index);
            return;
        }
        let tab = &mut self.tabs[tab_index];
        let Some(pos) = tab.panes.iter().position(|pane| pane.id == pane_id) else {
            return;
        };
        tab.panes.remove(pos);
        self.painter.remove_pane(pane_id);
        let next_focus = tab.tree.remove(pane_id);
        if tab.zoomed == Some(pane_id) || tab.panes.len() <= 1 {
            tab.zoomed = None;
        }
        if tab.panes.len() <= 1 {
            tab.broadcast_input = false;
        }
        if tab.focused == pane_id
            && let Some(next_id) = next_focus.or_else(|| tab.panes.first().map(|pane| pane.id))
        {
            tab.focused = next_id;
        }
        let cell = self.painter.metrics().cell;
        let divider = self.divider_thickness();
        let gutter = self.block_gutter();
        let is_active = tab_index == self.active;
        self.tabs[tab_index].reflow(self.content_area, divider, cell, gutter, is_active);
        if is_active {
            self.dragging_divider = None;
            self.activate_focused_pane();
        }
    }

    pub(super) fn close_tab(&mut self, event_loop: &ActiveEventLoop, index: usize) {
        if self.tabs.len() == 1 {
            event_loop.exit();
            return;
        }
        let removed = self.tabs.remove(index);
        for pane in removed.panes {
            self.painter.remove_pane(pane.id);
        }
        self.dragging_divider = None;
        if index == self.active {
            self.active = index.min(self.tabs.len() - 1);
            self.activate_tab();
        } else if index < self.active {
            self.active -= 1;
        }
        self.resize_pending = true;
        self.apply_resize();
        self.painter.invalidate_overlay();
        self.window.request_redraw();
    }

    pub(super) fn spawn_new_window(&self) {
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(error) => {
                log::warn!("Could not resolve Lune executable for new window: {error}");
                return;
            }
        };
        let cwd = self.tabs[self.active].focused_pane().working_directory();
        let config = self.custom_config.clone();
        let app_id = self.custom_app_id.clone();
        if let Err(error) = thread::Builder::new()
            .name("lune-new-window".into())
            .spawn(move || {
                let mut command = Command::new(exe);
                if let Some(dir) = cwd {
                    command.arg("--working-directory").arg(dir);
                }
                if let Some(path) = config {
                    command.arg("--config").arg(path);
                }
                if let Some(id) = app_id {
                    command.arg("--app-id").arg(id);
                }
                command
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .process_group(0);
                match command.spawn() {
                    Ok(mut child) => {
                        let _ = child.wait();
                    }
                    Err(error) => {
                        log::warn!("Could not launch new Lune window: {error}");
                    }
                }
            })
        {
            log::warn!("Could not start new-window worker thread: {error}");
        }
    }

    pub(super) fn update_window_title(&self) {
        let tab = &self.tabs[self.active];
        let title = tab
            .custom_title
            .as_deref()
            .filter(|title| !title.is_empty())
            .or_else(|| {
                let pane_title = tab.focused_pane().title.as_str();
                (!pane_title.is_empty()).then_some(pane_title)
            })
            .unwrap_or(&self.default_title);
        self.window.set_title(title);
    }

    pub(super) fn apply_system_theme(&mut self, theme: Theme) {
        self.system_theme = theme;
        self.apply_theme_preference(self.theme_preference.clone());
    }

    pub(super) fn apply_theme_preference(&mut self, preference: theme::Preference) {
        let selected = preference.resolve(self.system_theme);
        self.window.set_theme(Some(selected));
        (self.palette, self.scheme) = preference.palette(selected);
        self.theme_preference = preference;
        for tab in &mut self.tabs {
            for pane in &mut tab.panes {
                pane.session.set_theme(&self.palette, Some(self.scheme));
            }
        }
        self.painter.invalidate_overlay();
        self.window.request_redraw();
    }

    pub(super) fn command_blocks_enabled(&self) -> bool {
        self.mode == TerminalMode::Modern && self.command_blocks
    }

    pub(super) fn block_gutter(&self) -> u32 {
        if self.command_blocks_enabled() {
            (14.0 * self.window.scale_factor().max(1.0)).round() as u32
        } else {
            0
        }
    }

    pub(super) fn native_decorations_enabled(&self) -> bool {
        (self.mode == TerminalMode::Classic && self.window_decorations)
            || self
                .compositor_decorations
                .as_ref()
                .is_some_and(super::wayland_decorations::CompositorDecorations::uses_frameless_ssd)
    }

    pub(super) fn toggle_window_decorations(&mut self) {
        self.window_decorations = !self.window_decorations;
        self.window
            .set_decorations(self.native_decorations_enabled());
        self.update_cursor_icon();
        let label = if self.window_decorations {
            "Window decorations: On"
        } else {
            "Window decorations: Off"
        };
        self.show_notice(NoticeKind::Success, label.into());
    }

    pub(super) fn sync_background_blur(&mut self) {
        let Some(blur) = &mut self.blur else {
            return;
        };
        let logical = self
            .window
            .inner_size()
            .to_logical::<u32>(self.window.scale_factor().max(1.0));
        let active = self.background_blur && self.painter.background_opacity() < 1.0;
        if blur.sync(active, logical) {
            self.window.request_redraw();
        }
    }

    pub(super) fn toggle_background_blur(&mut self) {
        self.background_blur = !self.background_blur;
        self.sync_background_blur();
        let label = if !self.background_blur {
            "Background blur: Off"
        } else if self.blur.is_none() {
            "Background blur: On (unsupported by compositor)"
        } else if self.painter.background_opacity() >= 1.0 {
            "Background blur: On (requires background-opacity < 1.0)"
        } else {
            "Background blur: On"
        };
        self.show_notice(NoticeKind::Success, label.into());
    }

    pub(super) fn set_mode(&mut self, mode: TerminalMode) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.window
            .set_decorations(self.native_decorations_enabled());
        self.titlebar_hovered = None;
        self.titlebar_pressed = None;
        self.tab_drag = None;
        self.sidebar_resize_drag = None;
        self.tab_animator.clear();
        self.resize_pending = true;
        self.apply_resize();
        let label = match mode {
            TerminalMode::Classic => "Mode: Classic",
            TerminalMode::Modern => "Mode: Modern",
        };
        self.show_notice(NoticeKind::Success, label.into());
    }

    pub(super) fn set_tab_position(&mut self, position: TabPosition) {
        if self.mode != TerminalMode::Modern {
            self.mode = TerminalMode::Modern;
            self.window
                .set_decorations(self.native_decorations_enabled());
        }
        self.tab_position = position;
        self.sidebar_collapsed = false;
        self.titlebar_hovered = None;
        self.titlebar_pressed = None;
        self.tab_drag = None;
        self.sidebar_resize_drag = None;
        self.tab_animator.clear();
        self.resize_pending = true;
        self.apply_resize();
        let label = match position {
            TabPosition::Top => "Tab layout: Top tabs",
            TabPosition::Left => "Tab layout: Left sidebar",
        };
        self.show_notice(NoticeKind::Success, label.into());
    }

    pub(super) fn toggle_tab_sidebar(&mut self) {
        if self.mode != TerminalMode::Modern || self.tab_position != TabPosition::Left {
            return;
        }
        self.sidebar_collapsed = !self.sidebar_collapsed;
        self.titlebar_hovered = None;
        self.titlebar_pressed = None;
        self.tab_drag = None;
        self.sidebar_resize_drag = None;
        self.tab_animator.clear();
        self.resize_pending = true;
        self.apply_resize();
    }

    pub(super) fn execute_titlebar_action(
        &mut self,
        event_loop: &ActiveEventLoop,
        action: TitlebarAction,
    ) {
        match action {
            TitlebarAction::CommandPalette | TitlebarAction::CommandBar => self.open_palette(),
            TitlebarAction::ToggleSidebar => self.toggle_tab_sidebar(),
            TitlebarAction::NewTab | TitlebarAction::SidebarNewTab => {
                if let Err(error) = self.new_tab() {
                    log::warn!("Could not open a terminal tab: {error}");
                }
            }
            TitlebarAction::SelectTab(index) => {
                if index < self.tabs.len() {
                    self.select_tab(index);
                }
            }
            TitlebarAction::CloseTab(index) => {
                if index < self.tabs.len() {
                    self.request_close_tab(event_loop, index);
                }
            }
            TitlebarAction::Minimize => {
                self.window.set_minimized(true);
            }
            TitlebarAction::Maximize => {
                self.window.set_maximized(!self.window.is_maximized());
            }
            TitlebarAction::Close => {
                if self.confirm_window_close() {
                    log::debug!("Terminal window closed");
                    event_loop.exit();
                }
            }
        }
    }

    pub(super) fn borderless_resize_direction(
        &self,
        pos: PhysicalPosition<f64>,
    ) -> Option<ResizeDirection> {
        if self.native_decorations_enabled()
            || self.window.fullscreen().is_some()
            || self.window.is_maximized()
        {
            return None;
        }
        titlebar::edge_resize_direction(pos, self.painter.size(), self.window.scale_factor())
    }

    pub(super) fn resolved_window_buttons(&self) -> WindowButtons {
        if self.window.fullscreen().is_some() {
            return WindowButtons::NONE;
        }
        resolve_window_buttons(
            self.window_controls,
            self.compositor_decorations.as_ref(),
            self.portal_buttons,
        )
    }

    pub(super) fn apply_portal_buttons(&mut self, buttons: Option<WindowButtons>) {
        let before = self.resolved_window_buttons();
        self.portal_buttons = buttons;
        if self.resolved_window_buttons() != before {
            self.resize_pending = true;
            self.apply_resize();
            self.painter.invalidate_overlay();
            self.window.request_redraw();
        }
    }

    pub(super) fn reflow_tab_bar_only(&mut self) {
        let size = self.painter.size();
        let cell = self.painter.metrics().cell;
        let chrome_cell = self.painter.chrome_metrics().cell;
        let scale = self.window.scale_factor();
        let modern = self.mode == TerminalMode::Modern;
        let window_buttons = self.resolved_window_buttons();
        self.titlebar.reflow(
            titlebar::ReflowParams {
                size,
                cell: chrome_cell,
                scale,
                enabled: modern,
                tab_position: self.tab_position,
                sidebar_collapsed: self.sidebar_collapsed,
                sidebar_width_cells: self.sidebar_width_cells,
                window_buttons,
                tabs: self.tabs.len(),
                active: self.active,
            },
            |index| self.tabs[index].id,
        );
        self.bar.reflow(
            size,
            cell,
            self.active,
            if modern { 0 } else { self.tabs.len() },
            |index| self.tabs[index].label(),
        );
    }

    pub(super) fn sync_tab_animator(&mut self) {
        if self.mode != TerminalMode::Modern {
            self.tab_animator.clear();
            return;
        }
        let targets = self.titlebar.tab_targets();
        let dragged = self
            .tab_drag
            .filter(|drag| drag.moved)
            .map(|drag| (drag.tab_id, drag.current_coord));
        self.tab_animator
            .sync(self.tab_position, &targets, dragged, Instant::now());
    }

    pub(super) fn reflow_tab_bar(&mut self) {
        self.reflow_tab_bar_only();
        self.sync_tab_animator();
    }
}
