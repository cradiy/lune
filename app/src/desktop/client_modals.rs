//! Modal overlays and popups for `Client`: Command Palette, right-click Context Menu,
//! close/paste confirmation dialogs, and status toast notices.

use std::num::NonZeroU32;
use std::time::Instant;

use lune_terminal::{PixelPosition, PixelSize};
use winit::dpi::PhysicalPosition;
use winit::event::KeyEvent;
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, NamedKey};

use super::blocks::BlockAction;
use super::clipboard;
use super::config::WindowControlsPolicy;
use super::confirm;
use super::context_menu::{self, ContextMenu, MenuAction, MenuEntry};
use super::paint::NoticeKind;
use super::palette::{self, CommandPalette, PaletteAction, PaletteContext};
use super::scrollbar::ScrollbarPolicy;
use super::shortcuts::Action;
use super::split::Axis;
use super::tab::Pane;
use super::{
    Client, Notice, PaneId, TOAST_ERROR_DURATION, TOAST_SUCCESS_DURATION, TOAST_WARNING_DURATION,
    TabPosition, theme,
};

impl Client {
    pub(super) fn palette_layout(&self) -> Option<(palette::Layout, usize)> {
        let cmd_palette = self.command_palette.as_ref()?;
        let size = self.painter.size();
        let area = PixelSize {
            width: NonZeroU32::new(size.width)?,
            height: NonZeroU32::new(size.height)?,
        };
        let shown = cmd_palette.match_count().min(palette::MAX_VISIBLE_ROWS);
        let layout = palette::Layout::compute(
            area,
            self.painter.metrics(),
            self.window.scale_factor(),
            cmd_palette.match_count(),
        )?;
        Some((layout, shown))
    }

    pub(super) fn open_palette(&mut self) {
        if self.command_palette.is_some() {
            self.close_palette(true);
            return;
        }
        self.dragging_divider = None;
        let tab = &self.tabs[self.active];
        let pane = tab.focused_pane();
        let ctx = PaletteContext {
            keymap: &self.keymap,
            mode: self.mode,
            tab_position: self.tab_position,
            window_controls: self.window_controls,
            command_blocks: self.command_blocks,
            scrollbar: self.scrollbar,
            copy_on_select: self.copy_on_select,
            multi_tab: self.tabs.len() > 1,
            multi_pane: tab.panes.len() > 1,
            primary_screen: pane.session.screen().active_buffer()
                == lune_terminal::ScreenBuffer::Primary,
        };
        self.command_palette = Some(CommandPalette::new(&ctx));
        self.window.set_ime_allowed(self.focused);
        self.painter.reset_cursor_motion();
        self.painter.invalidate_overlay();
        self.update_cursor_icon();
        self.window.request_redraw();
    }

    pub(super) fn close_palette(&mut self, restore_theme: bool) {
        if let Some(cmd_palette) = self.command_palette.take() {
            if restore_theme && let Some(saved) = cmd_palette.theme_restore {
                self.apply_theme_preference(saved);
            }
            let pane = self.tabs[self.active].focused_pane_mut();
            pane.invalidate_ime_area();
            self.window
                .set_ime_allowed(self.focused && pane.allows_ime());
            pane.update_ime_area();
            self.painter.invalidate_overlay();
            self.update_cursor_icon();
            self.window.request_redraw();
        }
    }

    pub(super) fn sync_palette_theme_preview(&mut self) {
        let preview_name = self
            .command_palette
            .as_ref()
            .filter(|palette| palette.in_theme_menu())
            .and_then(|palette| palette.selected_entry())
            .and_then(|entry| match &entry.action {
                PaletteAction::ApplyTheme(name) => Some(name.clone()),
                _ => None,
            });
        let Some(name) = preview_name else {
            return;
        };
        let dir = self
            .config_dir
            .as_deref()
            .unwrap_or(std::path::Path::new(""));
        let mut errors = Vec::new();
        if let Ok(pref) = theme::Preference::load(dir, &name, &mut errors) {
            self.apply_theme_preference(pref);
        }
    }

    pub(super) fn handle_palette_key(&mut self, event_loop: &ActiveEventLoop, event: &KeyEvent) {
        let ctrl = self.modifiers.control_key();
        let alt = self.modifiers.alt_key();
        let shift = self.modifiers.shift_key();
        let super_key = self.modifiers.super_key();

        if let Some(binding) =
            self.keymap
                .binding(event, self.modifiers, lune_terminal::ScreenBuffer::Primary)
            && let Some(Action::CommandPalette) = binding.action(event.repeat)
        {
            self.close_palette(true);
            return;
        }

        let mut changed = false;
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                let popped = self
                    .command_palette
                    .as_mut()
                    .is_some_and(CommandPalette::pop_submenu);
                if popped {
                    let restore = self
                        .command_palette
                        .as_mut()
                        .and_then(|palette| palette.theme_restore.take());
                    if let Some(saved) = restore {
                        self.apply_theme_preference(saved);
                    }
                    self.painter.invalidate_overlay();
                    self.window.request_redraw();
                } else {
                    self.close_palette(true);
                }
                return;
            }
            Key::Named(NamedKey::Enter) if !ctrl && !alt && !super_key => {
                let action = self
                    .command_palette
                    .as_ref()
                    .and_then(|palette| palette.selected_entry().map(|e| e.action.clone()));
                if let Some(action) = action {
                    self.execute_palette_action(event_loop, action);
                } else {
                    self.close_palette(true);
                }
                return;
            }
            Key::Named(NamedKey::ArrowDown) if !ctrl && !alt && !super_key => {
                if let Some(palette) = &mut self.command_palette {
                    changed = palette.step(1);
                }
            }
            Key::Named(NamedKey::ArrowUp) if !ctrl && !alt && !super_key => {
                if let Some(palette) = &mut self.command_palette {
                    changed = palette.step(-1);
                }
            }
            Key::Named(NamedKey::Tab) if !ctrl && !alt && !super_key => {
                if let Some(palette) = &mut self.command_palette {
                    changed = palette.step(if shift { -1 } else { 1 });
                }
            }
            Key::Named(NamedKey::Backspace) if !alt && !super_key => {
                if let Some(palette) = &mut self.command_palette {
                    if palette.query.is_empty() && palette.pop_submenu() {
                        let restore = palette.theme_restore.take();
                        if let Some(saved) = restore {
                            self.apply_theme_preference(saved);
                        }
                        self.painter.invalidate_overlay();
                        self.window.request_redraw();
                        return;
                    }
                    changed = if ctrl {
                        palette.delete_word()
                    } else {
                        palette.backspace()
                    };
                }
            }
            Key::Character(text) if ctrl && !alt && !super_key => {
                if let Some(palette) = &mut self.command_palette {
                    if text.eq_ignore_ascii_case("n") || text.eq_ignore_ascii_case("j") {
                        changed = palette.step(1);
                    } else if text.eq_ignore_ascii_case("p") || text.eq_ignore_ascii_case("k") {
                        changed = palette.step(-1);
                    } else if text.eq_ignore_ascii_case("w") {
                        changed = palette.delete_word();
                    } else if text.eq_ignore_ascii_case("u") {
                        changed = palette.clear_query();
                    }
                }
            }
            Key::Named(NamedKey::Space) if !ctrl && !alt && !super_key => {
                if let Some(palette) = &mut self.command_palette {
                    changed = palette.push_text(" ");
                }
            }
            Key::Character(text) if !ctrl && !alt && !super_key => {
                if let Some(palette) = &mut self.command_palette {
                    changed = palette.push_text(text);
                }
            }
            _ => return,
        }

        if changed {
            self.sync_palette_theme_preview();
            self.painter.invalidate_overlay();
            self.window.request_redraw();
        }
    }

    pub(super) fn execute_palette_action(
        &mut self,
        event_loop: &ActiveEventLoop,
        action: PaletteAction,
    ) {
        match action {
            PaletteAction::OpenThemes => {
                let themes = theme::Preference::list_available(self.config_dir.as_deref());
                let entries = palette::build_theme_entries(themes);
                let restore = self.theme_preference.clone();
                if let Some(cmd_palette) = &mut self.command_palette {
                    cmd_palette.theme_restore = Some(restore);
                    cmd_palette.push_submenu("Theme", entries, 0);
                }
                self.sync_palette_theme_preview();
                self.painter.invalidate_overlay();
                self.window.request_redraw();
            }
            PaletteAction::ApplyTheme(name) => {
                let dir = self
                    .config_dir
                    .as_deref()
                    .unwrap_or(std::path::Path::new(""));
                let mut errors = Vec::new();
                match theme::Preference::load(dir, &name, &mut errors) {
                    Ok(pref) => {
                        self.close_palette(false);
                        self.apply_theme_preference(pref);
                        self.show_notice(NoticeKind::Success, format!("Theme: {name}"));
                    }
                    Err(error) => {
                        self.close_palette(true);
                        self.show_notice(NoticeKind::Warning, error.into());
                    }
                }
            }
            PaletteAction::OpenCommands => {
                let entries = {
                    let screen = self.tabs[self.active].focused_pane().session.screen();
                    palette::build_command_history_entries(screen.view())
                };
                if entries.is_empty() {
                    self.close_palette(true);
                    self.show_notice(
                        NoticeKind::Warning,
                        "No command history recorded in pane".into(),
                    );
                    return;
                }
                if let Some(cmd_palette) = &mut self.command_palette {
                    cmd_palette.push_submenu("Commands", entries, 0);
                }
                self.painter.invalidate_overlay();
                self.window.request_redraw();
            }
            PaletteAction::OpenBookmarks => {
                let entries = {
                    let screen = self.tabs[self.active].focused_pane().session.screen();
                    palette::build_bookmarked_command_entries(screen.view())
                };
                if entries.is_empty() {
                    self.close_palette(true);
                    self.show_notice(NoticeKind::Warning, "No bookmarked commands in pane".into());
                    return;
                }
                if let Some(cmd_palette) = &mut self.command_palette {
                    cmd_palette.push_submenu("Bookmarks", entries, 0);
                }
                self.painter.invalidate_overlay();
                self.window.request_redraw();
            }
            PaletteAction::JumpToRow(row) => {
                self.close_palette(true);
                self.tabs[self.active].focused_pane_mut().jump_to_row(row);
            }
            PaletteAction::OpenScrollbar => {
                let (entries, selected) = palette::build_scrollbar_entries(self.scrollbar);
                if let Some(cmd_palette) = &mut self.command_palette {
                    cmd_palette.push_submenu("Scrollbar", entries, selected);
                }
                self.painter.invalidate_overlay();
                self.window.request_redraw();
            }
            PaletteAction::SetScrollbar(policy) => {
                self.close_palette(true);
                self.scrollbar = policy;
                for tab in &mut self.tabs {
                    for pane in &mut tab.panes {
                        pane.update_scrollbar_hover(policy);
                    }
                }
                let label = match policy {
                    ScrollbarPolicy::Auto => "Scrollbar: Auto",
                    ScrollbarPolicy::Always => "Scrollbar: Always",
                    ScrollbarPolicy::Never => "Scrollbar: Hidden",
                };
                self.show_notice(NoticeKind::Success, label.into());
            }
            PaletteAction::SetMode(mode) => {
                self.close_palette(true);
                self.set_mode(mode);
            }
            PaletteAction::OpenTabPosition => {
                let (entries, selected) = palette::build_tab_position_entries(self.tab_position);
                if let Some(cmd_palette) = &mut self.command_palette {
                    cmd_palette.push_submenu("Tab layout", entries, selected);
                }
                self.painter.invalidate_overlay();
                self.window.request_redraw();
            }
            PaletteAction::SetTabPosition(position) => {
                self.close_palette(true);
                self.set_tab_position(position);
            }
            PaletteAction::ToggleWindowControls => {
                self.close_palette(true);
                self.window_controls = self.window_controls.cycle();
                self.resize_pending = true;
                self.apply_resize();
                let label = match self.window_controls {
                    WindowControlsPolicy::Auto => "Window controls: Auto (Wayland)",
                    WindowControlsPolicy::Show => "Window controls: Show",
                    WindowControlsPolicy::Hide => "Window controls: Hide",
                };
                self.show_notice(NoticeKind::Success, label.into());
            }
            PaletteAction::ToggleCommandBlocks => {
                self.close_palette(true);
                self.command_blocks = !self.command_blocks;
                if !self.command_blocks {
                    for tab in &mut self.tabs {
                        for pane in &mut tab.panes {
                            pane.hovered_block = None;
                        }
                    }
                }
                self.resize_pending = true;
                self.apply_resize();
                self.painter.invalidate_overlay();
                self.window.request_redraw();
                let label = if self.command_blocks {
                    "Command blocks: On"
                } else {
                    "Command blocks: Off"
                };
                self.show_notice(NoticeKind::Success, label.into());
            }
            PaletteAction::ToggleCopyOnSelect => {
                self.close_palette(true);
                self.copy_on_select = !self.copy_on_select;
                let label = if self.copy_on_select {
                    "Copy on select: On"
                } else {
                    "Copy on select: Off"
                };
                self.show_notice(NoticeKind::Success, label.into());
            }
            PaletteAction::Shortcut(shortcut) => {
                self.close_palette(true);
                self.run_shortcut(event_loop, shortcut);
            }
        }
    }

    pub(super) fn context_menu_layout(&self) -> Option<context_menu::Layout> {
        let menu = self.context_menu.as_ref()?;
        let size = self.painter.size();
        let area = PixelSize {
            width: NonZeroU32::new(size.width)?,
            height: NonZeroU32::new(size.height)?,
        };
        context_menu::Layout::compute(
            area,
            self.painter.chrome_metrics(),
            self.window.scale_factor(),
            menu,
        )
    }

    pub(super) fn close_context_menu(&mut self) {
        if self.context_menu.take().is_some() {
            self.painter.invalidate_overlay();
            self.window.request_redraw();
        }
    }

    pub(super) fn shortcut_hint(&self, action: Action) -> String {
        self.keymap.shortcut_label(action).unwrap_or_default()
    }

    pub(super) fn open_pane_context_menu(&mut self, point: PhysicalPosition<f64>, pane_id: PaneId) {
        let blocks_enabled = self.command_blocks_enabled();
        let Some(pane) = self.tabs[self.active].pane(pane_id) else {
            return;
        };
        let has_selection = pane.has_selection();
        let (link_url, block_info) = pane.context_menu_target_at(point, blocks_enabled);

        let mut entries = Vec::new();
        if let Some(uri) = link_url {
            entries.push(MenuEntry::item(
                MenuAction::OpenLink(uri.clone()),
                "Open Link",
                "",
            ));
            entries.push(MenuEntry::item(MenuAction::CopyLink(uri), "Copy Link", ""));
            entries.push(MenuEntry::Separator);
        }
        if let Some(target) = block_info {
            let prompt_row = target.prompt_row;
            let has_cmd = target.has_command;
            let has_out = target.has_output;
            let bookmarked = target.bookmarked;
            if has_cmd {
                entries.push(MenuEntry::item(
                    MenuAction::BlockAction {
                        pane_id,
                        prompt_row,
                        action: BlockAction::CopyCommand,
                    },
                    "Copy Command",
                    "",
                ));
            }
            if has_out {
                entries.push(MenuEntry::item(
                    MenuAction::BlockAction {
                        pane_id,
                        prompt_row,
                        action: BlockAction::CopyOutput,
                    },
                    "Copy Output",
                    "",
                ));
                entries.push(MenuEntry::item(
                    MenuAction::BlockAction {
                        pane_id,
                        prompt_row,
                        action: BlockAction::SelectOutput,
                    },
                    "Select Output",
                    "",
                ));
            }
            if has_cmd && has_out {
                entries.push(MenuEntry::item(
                    MenuAction::BlockAction {
                        pane_id,
                        prompt_row,
                        action: BlockAction::CopyFull,
                    },
                    "Copy Command & Output",
                    "",
                ));
            }
            entries.push(MenuEntry::item(
                MenuAction::BlockAction {
                    pane_id,
                    prompt_row,
                    action: BlockAction::SearchInBlock,
                },
                "Search in Block",
                "",
            ));
            entries.push(MenuEntry::item(
                MenuAction::BlockAction {
                    pane_id,
                    prompt_row,
                    action: BlockAction::ToggleBookmark,
                },
                if bookmarked {
                    "Remove Bookmark"
                } else {
                    "Bookmark Block"
                },
                "",
            ));
            if has_cmd {
                entries.push(MenuEntry::item(
                    MenuAction::BlockAction {
                        pane_id,
                        prompt_row,
                        action: BlockAction::Rerun,
                    },
                    "Re-run Command",
                    "",
                ));
            }
            entries.push(MenuEntry::Separator);
        }
        if has_selection {
            entries.push(MenuEntry::item(
                MenuAction::Shortcut(Action::Copy),
                "Copy",
                self.shortcut_hint(Action::Copy),
            ));
        }
        entries.push(MenuEntry::item(
            MenuAction::Shortcut(Action::Paste),
            "Paste",
            self.shortcut_hint(Action::Paste),
        ));
        entries.push(MenuEntry::Separator);
        entries.push(MenuEntry::item(
            MenuAction::Shortcut(Action::SplitPane(Axis::Columns)),
            "Split Right",
            self.shortcut_hint(Action::SplitPane(Axis::Columns)),
        ));
        entries.push(MenuEntry::item(
            MenuAction::Shortcut(Action::SplitPane(Axis::Rows)),
            "Split Down",
            self.shortcut_hint(Action::SplitPane(Axis::Rows)),
        ));
        entries.push(MenuEntry::Separator);
        entries.push(MenuEntry::item(
            MenuAction::Shortcut(Action::Search),
            "Search",
            self.shortcut_hint(Action::Search),
        ));
        entries.push(MenuEntry::item(
            MenuAction::Shortcut(Action::CommandPalette),
            "Command Palette",
            self.shortcut_hint(Action::CommandPalette),
        ));
        entries.push(MenuEntry::item(
            MenuAction::Shortcut(Action::OpenDirectory),
            "Open Directory",
            self.shortcut_hint(Action::OpenDirectory),
        ));

        let anchor = PixelPosition {
            x: point.x.max(0.0) as u32,
            y: point.y.max(0.0) as u32,
        };
        self.context_menu = ContextMenu::new(anchor, entries);
        self.painter.invalidate_overlay();
        self.window.request_redraw();
    }

    pub(super) fn open_tab_context_menu(&mut self, point: PhysicalPosition<f64>, index: usize) {
        let mut entries = vec![
            MenuEntry::item(
                MenuAction::Shortcut(Action::NewTab),
                "New Tab",
                self.shortcut_hint(Action::NewTab),
            ),
            MenuEntry::item(MenuAction::DuplicateTab(index), "Duplicate Tab", ""),
            MenuEntry::Separator,
            MenuEntry::danger(
                MenuAction::CloseTabAt(index),
                "Close Tab",
                self.shortcut_hint(Action::CloseTab),
            ),
        ];
        if self.tabs.len() > 1 {
            entries.push(MenuEntry::danger(
                MenuAction::CloseOtherTabs(index),
                "Close Other Tabs",
                "",
            ));
        }
        if index + 1 < self.tabs.len() {
            entries.push(MenuEntry::danger(
                MenuAction::CloseTabsToRight(index),
                "Close Tabs to Right",
                "",
            ));
        }
        let anchor = PixelPosition {
            x: point.x.max(0.0) as u32,
            y: point.y.max(0.0) as u32,
        };
        self.context_menu = ContextMenu::new(anchor, entries);
        self.painter.invalidate_overlay();
        self.window.request_redraw();
    }

    pub(super) fn open_chrome_context_menu(&mut self, point: PhysicalPosition<f64>) {
        let (next_pos, pos_label) = match self.tab_position {
            TabPosition::Top => (TabPosition::Left, "Move Tabs to Sidebar"),
            TabPosition::Left => (TabPosition::Top, "Move Tabs to Top"),
        };
        let entries = vec![
            MenuEntry::item(
                MenuAction::Shortcut(Action::NewTab),
                "New Tab",
                self.shortcut_hint(Action::NewTab),
            ),
            MenuEntry::item(MenuAction::SetTabPosition(next_pos), pos_label, ""),
            MenuEntry::Separator,
            MenuEntry::item(
                MenuAction::Shortcut(Action::CommandPalette),
                "Command Palette",
                self.shortcut_hint(Action::CommandPalette),
            ),
        ];
        let anchor = PixelPosition {
            x: point.x.max(0.0) as u32,
            y: point.y.max(0.0) as u32,
        };
        self.context_menu = ContextMenu::new(anchor, entries);
        self.painter.invalidate_overlay();
        self.window.request_redraw();
    }

    pub(super) fn handle_context_menu_key(
        &mut self,
        event_loop: &ActiveEventLoop,
        event: &KeyEvent,
    ) {
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                self.close_context_menu();
                self.update_cursor_icon();
            }
            Key::Named(NamedKey::ArrowDown) => {
                if self.context_menu.as_mut().is_some_and(|m| m.step(1)) {
                    self.painter.invalidate_overlay();
                    self.window.request_redraw();
                }
            }
            Key::Named(NamedKey::ArrowUp) => {
                if self.context_menu.as_mut().is_some_and(|m| m.step(-1)) {
                    self.painter.invalidate_overlay();
                    self.window.request_redraw();
                }
            }
            Key::Named(NamedKey::Enter) => {
                let action = self
                    .context_menu
                    .as_ref()
                    .and_then(|menu| menu.hovered.and_then(|idx| menu.action_at(idx)));
                self.close_context_menu();
                if let Some(action) = action {
                    self.execute_context_menu_action(event_loop, action);
                }
                self.update_cursor_icon();
            }
            _ => {
                self.close_context_menu();
                self.update_cursor_icon();
            }
        }
    }

    pub(super) fn execute_context_menu_action(
        &mut self,
        event_loop: &ActiveEventLoop,
        action: MenuAction,
    ) {
        match action {
            MenuAction::Shortcut(shortcut) => self.run_shortcut(event_loop, shortcut),
            MenuAction::OpenLink(uri) => {
                self.tabs[self.active]
                    .focused_pane()
                    .open_uri_external(uri, &mut self.hyperlink_open_pending);
            }
            MenuAction::CopyLink(uri) => {
                if !self.copy_pending {
                    let preview = if uri.chars().count() > 40 {
                        let short: String = uri.chars().take(40).collect();
                        format!("Copied link: {short}...")
                    } else {
                        format!("Copied link: {uri}")
                    };
                    match clipboard::write(uri, self.proxy.clone()) {
                        Ok(()) => {
                            self.copy_pending = true;
                            self.show_notice(NoticeKind::Success, preview);
                        }
                        Err(error) => {
                            log::warn!("Failed to start the clipboard copy worker: {error}");
                        }
                    }
                }
            }
            MenuAction::BlockAction {
                pane_id,
                prompt_row,
                action,
            } => {
                if let Some(notice) = self.tabs[self.active].pane_mut(pane_id).and_then(|pane| {
                    pane.run_block_action(prompt_row, action, &mut self.copy_pending)
                }) {
                    self.show_notice(NoticeKind::Success, notice);
                }
            }
            MenuAction::DuplicateTab(index) => {
                let cwd = self
                    .tabs
                    .get(index)
                    .and_then(|tab| tab.focused_pane().working_directory());
                if let Err(error) = self.new_tab_with_cwd(cwd) {
                    log::warn!("Could not duplicate tab: {error}");
                    self.show_notice(NoticeKind::Error, "Could not duplicate tab".into());
                }
            }
            MenuAction::CloseTabAt(index) => {
                if index < self.tabs.len() {
                    self.request_close_tab(event_loop, index);
                }
            }
            MenuAction::CloseOtherTabs(keep) => {
                if keep < self.tabs.len() && self.tabs.len() > 1 {
                    for idx in (0..self.tabs.len()).rev() {
                        if idx != keep {
                            self.close_tab(event_loop, idx);
                        }
                    }
                }
            }
            MenuAction::CloseTabsToRight(index) => {
                if index + 1 < self.tabs.len() {
                    for idx in (index + 1..self.tabs.len()).rev() {
                        self.close_tab(event_loop, idx);
                    }
                }
            }
            MenuAction::SetTabPosition(position) => {
                self.set_tab_position(position);
            }
        }
    }

    pub(super) fn confirm_layout(&self) -> Option<confirm::Layout> {
        let dialog = self.confirm.as_ref()?;
        let size = self.painter.size();
        let area = PixelSize {
            width: NonZeroU32::new(size.width)?,
            height: NonZeroU32::new(size.height)?,
        };
        confirm::Layout::compute(
            area,
            self.painter.metrics(),
            self.window.scale_factor(),
            dialog,
        )
    }

    pub(super) fn open_confirm(&mut self, target: confirm::ConfirmTarget, message: String) {
        self.context_menu = None;
        self.dragging_divider = None;
        let dialog = confirm::Dialog::new(target, message);
        self.confirm = Some(dialog);
        if let Some(point) = self.pointer {
            let hovered = self.confirm_layout().and_then(|layout| layout.hit(point));
            if let Some(dialog) = &mut self.confirm {
                dialog.hovered = hovered;
            }
        }
        self.window.set_ime_allowed(false);
        self.painter.reset_cursor_motion();
        self.painter.invalidate_overlay();
        self.update_cursor_icon();
        self.window.request_redraw();
    }

    pub(super) fn dismiss_confirm(&mut self) {
        if self.confirm.take().is_some() {
            let pane = self.tabs[self.active].focused_pane_mut();
            pane.invalidate_ime_area();
            self.window
                .set_ime_allowed(self.focused && pane.allows_ime());
            pane.update_ime_area();
            self.painter.invalidate_overlay();
            self.update_cursor_icon();
            self.window.request_redraw();
        }
    }

    pub(super) fn execute_confirm(
        &mut self,
        event_loop: &ActiveEventLoop,
        target: confirm::ConfirmTarget,
    ) {
        self.painter.invalidate_overlay();
        self.update_cursor_icon();
        match target {
            confirm::ConfirmTarget::CloseWindow => {
                log::debug!("Terminal window closed");
                event_loop.exit();
            }
            confirm::ConfirmTarget::CloseTab(index) => {
                if index < self.tabs.len() {
                    self.close_tab(event_loop, index);
                }
            }
            confirm::ConfirmTarget::ClosePane(pane_id) => {
                if let Some(tab_index) =
                    self.tabs.iter().position(|tab| tab.pane(pane_id).is_some())
                {
                    self.close_pane(event_loop, tab_index, pane_id);
                }
            }
            confirm::ConfirmTarget::Paste { pane, text } => {
                if let Some(pane) = self.pane_mut(pane) {
                    pane.commit_paste(text);
                }
                let focused_pane = self.tabs[self.active].focused_pane_mut();
                focused_pane.invalidate_ime_area();
                self.window
                    .set_ime_allowed(self.focused && focused_pane.allows_ime());
                focused_pane.update_ime_area();
                self.window.request_redraw();
            }
        }
    }

    pub(super) fn handle_confirm_key(&mut self, event_loop: &ActiveEventLoop, event: &KeyEvent) {
        match &event.logical_key {
            Key::Character(text) if text.eq_ignore_ascii_case("y") => {
                if let Some(dialog) = self.confirm.take() {
                    self.execute_confirm(event_loop, dialog.target);
                }
            }
            Key::Character(text) if text.eq_ignore_ascii_case("n") => {
                self.dismiss_confirm();
            }
            Key::Named(NamedKey::Escape) => {
                self.dismiss_confirm();
            }
            Key::Named(NamedKey::Enter | NamedKey::Space) => {
                if let Some(selected) = self.confirm.as_ref().map(|dialog| dialog.selected) {
                    match selected {
                        confirm::Button::Yes => {
                            if let Some(dialog) = self.confirm.take() {
                                self.execute_confirm(event_loop, dialog.target);
                            }
                        }
                        confirm::Button::No => self.dismiss_confirm(),
                    }
                }
            }
            Key::Named(NamedKey::ArrowLeft | NamedKey::ArrowRight | NamedKey::Tab) => {
                if let Some(dialog) = &mut self.confirm {
                    dialog.selected = dialog.selected.toggle();
                    self.painter.invalidate_overlay();
                    self.window.request_redraw();
                }
            }
            Key::Character(text)
                if text.eq_ignore_ascii_case("h") || text.eq_ignore_ascii_case("l") =>
            {
                if let Some(dialog) = &mut self.confirm {
                    dialog.selected = dialog.selected.toggle();
                    self.painter.invalidate_overlay();
                    self.window.request_redraw();
                }
            }
            _ => {}
        }
    }

    pub(super) fn confirm_window_close(&mut self) -> bool {
        if !self.confirm_close {
            return true;
        }
        let total_panes: usize = self.tabs.iter().map(|tab| tab.panes.len()).sum();
        let processes: Vec<String> = self
            .tabs
            .iter()
            .flat_map(|tab| tab.panes.iter())
            .filter_map(Pane::foreground_process_name)
            .collect();
        if total_panes <= 1 && processes.is_empty() {
            return true;
        }
        let message = confirm::format_running("window", &processes, total_panes);
        self.open_confirm(confirm::ConfirmTarget::CloseWindow, message);
        false
    }

    pub(super) fn request_close_pane(
        &mut self,
        event_loop: &ActiveEventLoop,
        tab_index: usize,
        pane_id: PaneId,
    ) {
        if !self.confirm_close {
            self.close_pane(event_loop, tab_index, pane_id);
            return;
        }
        let Some(process) = self.tabs[tab_index]
            .pane(pane_id)
            .and_then(Pane::foreground_process_name)
        else {
            self.close_pane(event_loop, tab_index, pane_id);
            return;
        };
        let message = confirm::format_running("pane", &[process], 1);
        self.open_confirm(confirm::ConfirmTarget::ClosePane(pane_id), message);
    }

    pub(super) fn request_close_tab(&mut self, event_loop: &ActiveEventLoop, index: usize) {
        if !self.confirm_close {
            self.close_tab(event_loop, index);
            return;
        }
        let tab = &self.tabs[index];
        let processes: Vec<String> = tab
            .panes
            .iter()
            .filter_map(Pane::foreground_process_name)
            .collect();
        if tab.panes.len() <= 1 && processes.is_empty() {
            self.close_tab(event_loop, index);
            return;
        }
        let message = confirm::format_running("tab", &processes, tab.panes.len());
        self.open_confirm(confirm::ConfirmTarget::CloseTab(index), message);
    }

    pub(super) fn show_notice(&mut self, kind: NoticeKind, text: String) {
        let duration = match kind {
            NoticeKind::Success => TOAST_SUCCESS_DURATION,
            NoticeKind::Warning => TOAST_WARNING_DURATION,
            NoticeKind::Error => TOAST_ERROR_DURATION,
        };
        self.notice = Some(Notice {
            kind,
            text,
            expires: Instant::now() + duration,
        });
        self.painter.invalidate_overlay();
        self.window.request_redraw();
    }

    pub(super) fn set_notice(&mut self, text: Option<String>) {
        match text {
            Some(text) => self.show_notice(NoticeKind::Error, text),
            None => {
                if self.notice.take().is_some() {
                    self.painter.invalidate_overlay();
                    self.window.request_redraw();
                }
            }
        }
    }
}
