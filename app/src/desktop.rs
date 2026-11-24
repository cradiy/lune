mod clipboard;
mod config;
mod cursor;
mod directory;
mod font;
mod input;
mod ligature;
mod notify;
mod paint;
mod readiness;
mod selection;
mod shortcuts;
mod split;
mod tab;
mod tab_bar;
mod text;
mod theme;
mod url;
mod vi;

use std::io;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lune_terminal::{ClipboardQuery, ColorScheme, Palette, PixelPosition, PixelSize};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{Ime, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::platform::wayland::{EventLoopBuilderExtWayland, WindowAttributesExtWayland};
use winit::window::{CursorIcon, Fullscreen, Theme, Window, WindowId};

use crate::{GridGeometry, PixelRect, SessionReady};

use clipboard::ClipboardType;
use config::{ClipboardPolicy, Config, MAX_FONT_SIZE, MIN_FONT_SIZE, NotificationPolicy};
use font::{Font, physical_font_size};
use notify::Notifier;
use paint::Painter;
use readiness::Readiness;
use shortcuts::{Action, Scroll, Zoom};
use split::{Axis, Direction as SplitDirection, Divider, SplitTree};
use tab::{Pane, Tab};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub(super) type PaneId = u32;

const PASTE_TIMEOUT: Duration = Duration::from_secs(2);
const NOTICE_DURATION: Duration = Duration::from_secs(6);
const SPLIT_RESIZE_STEP: f32 = 0.05;
const MIN_SPLIT_COLUMNS: u16 = 4;
const MIN_SPLIT_ROWS: u16 = 2;

struct Notice {
    text: String,
    expires: Instant,
}

enum Event {
    PtyReady(u64),
    PtyError(io::Error),
    CopyReady(std::result::Result<(), String>),
    PasteReady(PaneId, std::result::Result<String, String>),
    Osc52ReadReady(PaneId, ClipboardQuery, std::result::Result<String, String>),
    DirectoryOpened(io::Result<()>),
    HyperlinkOpened(io::Result<()>),
    SystemTheme(Theme),
    ConfigReload(bool),
}

pub fn run() -> Result<()> {
    let loaded = Config::load();
    let startup_notice = config::format_errors(&loaded.errors);
    let event_loop = EventLoop::<Event>::with_user_event()
        .with_wayland()
        .build()?;
    log::debug!("Wayland event loop ready");
    let proxy = event_loop.create_proxy();
    let config_watch =
        Config::directory().and_then(|directory| {
            match config::watch::Watch::start(directory, proxy.clone()) {
                Ok(watch) => Some(watch),
                Err(error) => {
                    log::warn!("Could not start the configuration watcher: {error}");
                    None
                }
            }
        });
    let mut app = App {
        proxy,
        config: loaded.config,
        startup_notice,
        client: None,
        error: None,
        system_theme: Theme::Dark,
        _theme_watch: None,
        _config_watch: config_watch,
    };
    app.sync_theme_watch();
    event_loop.run_app(&mut app)?;
    app.error.map_or(Ok(()), Err)
}

struct App {
    proxy: EventLoopProxy<Event>,
    config: Config,
    startup_notice: Option<String>,
    client: Option<Client>,
    error: Option<Box<dyn std::error::Error>>,
    system_theme: Theme,
    _theme_watch: Option<theme::system::Watch>,
    _config_watch: Option<config::watch::Watch>,
}

impl App {
    fn sync_theme_watch(&mut self) {
        if matches!(self.config.theme, theme::Preference::System) {
            if self._theme_watch.is_none() {
                match theme::system::Watch::start(self.proxy.clone()) {
                    Ok(watch) => self._theme_watch = Some(watch),
                    Err(error) => {
                        log::warn!("Could not start the system theme watcher: {error}");
                    }
                }
            }
        } else {
            self._theme_watch = None;
        }
    }

    fn reload_config(&mut self, force: bool) {
        let loaded = match Config::load_checked() {
            Ok(loaded) => loaded,
            Err(error) => {
                let notice = config::format_errors(&[error]);
                if let Some(client) = &mut self.client {
                    client.set_notice(notice);
                } else {
                    self.startup_notice = notice;
                }
                return;
            }
        };
        let notice = config::format_errors(&loaded.errors);
        if let Some(client) = &mut self.client {
            client.set_notice(notice);
        } else {
            self.startup_notice = notice;
        }
        if !force && loaded.config == self.config {
            return;
        }
        if let Some(client) = &mut self.client {
            client.apply_config(&self.config, &loaded.config, self.system_theme);
        }
        self.config = loaded.config;
        self.sync_theme_watch();
        log::info!("Reloaded configuration");
    }

    fn report(&mut self, event_loop: &ActiveEventLoop, result: Result<()>) {
        if let Err(error) = result {
            self.error = Some(error);
            event_loop.exit();
        }
    }
}

impl ApplicationHandler<Event> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.client.is_none() {
            match Client::new(
                event_loop,
                self.proxy.clone(),
                &self.config,
                self.system_theme,
                self.startup_notice.take(),
            ) {
                Ok(client) => self.client = Some(client),
                Err(error) => self.report(event_loop, Err(error)),
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if let Some(client) = &mut self.client
            && id == client.window.id()
        {
            let result = client.window_event(event_loop, event);
            self.report(event_loop, result);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Event) {
        if let Event::ConfigReload(force) = event {
            self.reload_config(force);
            return;
        }
        if let Event::SystemTheme(theme) = event {
            if self.system_theme == theme {
                return;
            }
            self.system_theme = theme;
            if !matches!(self.config.theme, theme::Preference::System) {
                return;
            }
        }
        let Some(client) = &mut self.client else {
            return;
        };
        let result = match event {
            Event::PtyReady(token) => client.pty_ready(event_loop, token),
            Event::PtyError(error) => Err(error.into()),
            Event::PasteReady(id, result) => {
                if let Some(pane) = client.pane_mut(id) {
                    pane.finish_paste(result);
                }
                Ok(())
            }
            Event::Osc52ReadReady(id, query, result) => {
                if let Some(pane) = client.pane_mut(id) {
                    pane.finish_osc52_read(query, result);
                }
                Ok(())
            }
            Event::CopyReady(result) => {
                client.copy_pending = false;
                if let Err(error) = result {
                    log::warn!("Clipboard copy failed: {error}");
                }
                Ok(())
            }
            Event::DirectoryOpened(result) => {
                client.directory_open_pending = false;
                if let Err(error) = result {
                    log::warn!("Working directory open failed: {error}");
                }
                Ok(())
            }
            Event::HyperlinkOpened(result) => {
                client.hyperlink_open_pending = false;
                if let Err(error) = result {
                    log::warn!("Hyperlink open failed: {error}");
                }
                Ok(())
            }
            Event::SystemTheme(theme) => {
                client.apply_system_theme(theme);
                Ok(())
            }
            Event::ConfigReload(_) => unreachable!(),
        };
        self.report(event_loop, result);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(client) = &mut self.client {
            let result = client.wait(event_loop);
            self.report(event_loop, result);
        }
    }
}

struct Client {
    window: Arc<Window>,
    painter: Painter,
    readiness: Readiness,
    proxy: EventLoopProxy<Event>,
    tabs: Vec<Tab>,
    active: usize,
    next_pane: PaneId,
    palette: Palette,
    scheme: ColorScheme,
    bar: tab_bar::Layout,
    content_area: PixelRect,
    copy_pending: bool,
    directory_open_pending: bool,
    hyperlink_open_pending: bool,
    primary_selection: Option<clipboard::Primary>,
    clipboard_write: ClipboardPolicy,
    clipboard_read: ClipboardPolicy,
    notifications_policy: NotificationPolicy,
    notifier: Option<Notifier>,
    notice: Option<Notice>,
    font_size_points: f32,
    base_font_size_points: f32,
    resize_pending: bool,
    modifiers: ModifiersState,
    focused: bool,
    ime_enabled: bool,
    pointer: Option<PhysicalPosition<f64>>,
    dragging_divider: Option<Divider>,
    cursor_icon: CursorIcon,
}

impl Client {
    fn new(
        event_loop: &ActiveEventLoop,
        proxy: EventLoopProxy<Event>,
        config: &Config,
        system_theme: Theme,
        startup_notice: Option<String>,
    ) -> Result<Self> {
        let selected_theme = config.theme.resolve(system_theme);
        let window = Arc::new(
            event_loop.create_window(
                Window::default_attributes()
                    .with_title("Lune")
                    .with_name("lune", "")
                    .with_theme(Some(selected_theme))
                    .with_transparent(true)
                    .with_inner_size(LogicalSize::new(960, 600))
                    .with_min_inner_size(LogicalSize::new(180, 100)),
            )?,
        );
        let scale = window.scale_factor();
        let painter = pollster::block_on(Painter::new(
            window.clone(),
            Font::new(config.font_family.as_deref(), config.font_ligatures),
            physical_font_size(config.font_size_points, scale),
            config.background_opacity,
        ))?;
        let bar = tab_bar::Layout::default();
        let content_area = content_rect(window.inner_size(), 0)
            .ok_or_else(|| io::Error::other("The terminal grid does not fit the Wayland window"))?;
        let grid = GridGeometry::fit(content_area.size, painter.metrics().cell)
            .ok_or_else(|| io::Error::other("The terminal grid does not fit the Wayland window"))?;
        let (palette, scheme) = config.theme.palette(selected_theme);
        let mut pane = Pane::spawn(
            1,
            window.clone(),
            proxy.clone(),
            grid,
            &palette,
            scheme,
            None,
        )?;
        let readiness = Readiness::new(proxy.clone())?;
        readiness.arm(pane.id, &mut pane.registration, pane.session.interests())?;
        pane.visible = true;
        pane.active = true;
        let tab = Tab::new(pane, content_area);
        window.set_ime_allowed(true);
        window.request_redraw();
        log::info!("Wayland terminal ready");
        let notifier = match config.notifications {
            NotificationPolicy::Deny => None,
            _ => match Notifier::start() {
                Ok(notifier) => Some(notifier),
                Err(error) => {
                    log::warn!("Could not start desktop notifications: {error}");
                    None
                }
            },
        };
        Ok(Self {
            window,
            painter,
            readiness,
            proxy,
            tabs: vec![tab],
            active: 0,
            next_pane: 2,
            palette,
            scheme,
            bar,
            content_area,
            copy_pending: false,
            directory_open_pending: false,
            hyperlink_open_pending: false,
            primary_selection: None,
            clipboard_write: config.clipboard_write,
            clipboard_read: config.clipboard_read,
            notifications_policy: config.notifications,
            notifier,
            notice: startup_notice.map(|text| Notice {
                text,
                expires: Instant::now() + NOTICE_DURATION,
            }),
            font_size_points: config.font_size_points,
            base_font_size_points: config.font_size_points,
            resize_pending: false,
            modifiers: ModifiersState::empty(),
            focused: false,
            ime_enabled: false,
            pointer: None,
            dragging_divider: None,
            cursor_icon: CursorIcon::Default,
        })
    }

    fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.tabs.iter_mut().find_map(|tab| tab.pane_mut(id))
    }

    fn divider_thickness(&self) -> u32 {
        (self.window.scale_factor().round() as u32).max(1)
    }

    fn divider_grab_margin(&self) -> f64 {
        (4.0 * self.window.scale_factor()).max(4.0)
    }

    fn allocate_pane_id(&mut self) -> Result<PaneId> {
        let id = self.next_pane;
        self.next_pane = id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Pane IDs exhausted"))?;
        Ok(id)
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, event: WindowEvent) -> Result<()> {
        match event {
            WindowEvent::CloseRequested | WindowEvent::Destroyed => {
                log::debug!("Terminal window closed");
                event_loop.exit();
            }
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                self.resize_pending = true
            }
            WindowEvent::RedrawRequested => {
                self.apply_resize();
                self.draw()?;
            }
            WindowEvent::Occluded(true) => {
                self.painter.reset_cursor_motion();
            }
            WindowEvent::Occluded(false) => {
                self.painter.invalidate_overlay();
                self.window.request_redraw();
            }
            WindowEvent::Focused(focused) => {
                self.focused = focused;
                self.tabs[self.active].focused_pane_mut().focus(focused);
                if !focused {
                    self.dragging_divider = None;
                    self.painter.reset_cursor_motion();
                    self.modifiers = ModifiersState::empty();
                    for pane in &mut self.tabs[self.active].panes {
                        pane.modifiers = self.modifiers;
                        pane.update_hovered_hyperlink();
                    }
                    self.update_cursor_icon();
                }
                self.painter.invalidate_overlay();
                self.window.request_redraw();
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
                for pane in &mut self.tabs[self.active].panes {
                    pane.modifiers = self.modifiers;
                    pane.update_hovered_hyperlink();
                }
                self.update_cursor_icon();
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } => {
                if event.state.is_pressed() {
                    self.key(event_loop, &event);
                } else {
                    self.tabs[self.active]
                        .focused_pane_mut()
                        .key_release(&event);
                }
            }
            WindowEvent::Ime(Ime::Enabled) => {
                self.ime_enabled = true;
                let pane = self.tabs[self.active].focused_pane_mut();
                pane.ime_enabled = true;
                pane.ime_area = None;
                pane.update_ime_area();
            }
            WindowEvent::Ime(Ime::Disabled) => {
                self.ime_enabled = false;
                let pane = self.tabs[self.active].focused_pane_mut();
                pane.ime_enabled = false;
                pane.set_preedit(String::new());
            }
            WindowEvent::Ime(ime) => self.tabs[self.active].focused_pane_mut().ime(ime),
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer = Some(position);
                if let Some(divider) = self.dragging_divider {
                    let ratio = divider.ratio_for(position);
                    if self.tabs[self.active].tree.set_ratio(divider.path, ratio) {
                        self.reflow_active_tab();
                        self.painter.invalidate_overlay();
                        self.window.request_redraw();
                    }
                    self.update_cursor_icon();
                    return Ok(());
                }
                let tab = &mut self.tabs[self.active];
                let captured = tab
                    .panes
                    .iter()
                    .find(|pane| pane.selection_drag.is_some() || pane.pressed.is_some())
                    .map(|pane| pane.id);
                if let Some(id) = captured {
                    if let Some(pane) = tab.pane_mut(id) {
                        pane.pointer_moved(position);
                    }
                } else {
                    let hovered_id = (!self.bar.contains(position))
                        .then(|| tab.layout.pane_at(position))
                        .flatten();
                    for pane in &mut tab.panes {
                        if Some(pane.id) == hovered_id {
                            pane.modifiers = self.modifiers;
                            pane.pointer_moved(position);
                        } else if pane.pointer.is_some() {
                            pane.pointer = None;
                            pane.update_hovered_hyperlink();
                        }
                    }
                }
                self.update_cursor_icon();
            }
            WindowEvent::CursorLeft { .. } => {
                self.pointer = None;
                for pane in &mut self.tabs[self.active].panes {
                    if pane.pressed.is_none() && pane.selection_drag.is_none() {
                        pane.pointer = None;
                    }
                    pane.update_hovered_hyperlink();
                }
                self.update_cursor_icon();
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if !state.is_pressed()
                    && button == winit::event::MouseButton::Left
                    && self.dragging_divider.take().is_some()
                {
                    self.update_cursor_icon();
                    return Ok(());
                }
                if state.is_pressed() && self.pointer.is_some_and(|point| self.bar.contains(point))
                {
                    if let Some(index) = self.bar.hit(self.pointer.unwrap())
                        && index < self.tabs.len()
                    {
                        match button {
                            winit::event::MouseButton::Left => self.select_tab(index),
                            winit::event::MouseButton::Middle => self.close_tab(event_loop, index),
                            _ => {}
                        }
                    }
                } else if state.is_pressed()
                    && button == winit::event::MouseButton::Left
                    && let Some(point) = self.pointer
                    && let Some(divider) = self.tabs[self.active]
                        .layout
                        .divider_at(point, self.divider_grab_margin())
                {
                    self.dragging_divider = Some(divider);
                    self.update_cursor_icon();
                } else if state.is_pressed() {
                    if let Some(point) = self.pointer
                        && let Some(target_id) = self.tabs[self.active].layout.pane_at(point)
                    {
                        if target_id != self.tabs[self.active].focused {
                            self.focus_pane(target_id);
                        }
                        if let Some(pane) = self.tabs[self.active].pane_mut(target_id) {
                            pane.pointer = Some(point);
                            pane.modifiers = self.modifiers;
                            pane.button(
                                state,
                                button,
                                &mut self.primary_selection,
                                &mut self.hyperlink_open_pending,
                            );
                        }
                    } else {
                        self.tabs[self.active].focused_pane_mut().button(
                            state,
                            button,
                            &mut self.primary_selection,
                            &mut self.hyperlink_open_pending,
                        );
                    }
                    self.update_cursor_icon();
                } else {
                    let target_id = self.tabs[self.active]
                        .panes
                        .iter()
                        .find(|pane| pane.selection_drag.is_some() || pane.pressed.is_some())
                        .map(|pane| pane.id)
                        .unwrap_or(self.tabs[self.active].focused);
                    if let Some(pane) = self.tabs[self.active].pane_mut(target_id) {
                        pane.button(
                            state,
                            button,
                            &mut self.primary_selection,
                            &mut self.hyperlink_open_pending,
                        );
                    }
                    self.update_cursor_icon();
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if self.pointer.is_some_and(|point| self.bar.contains(point)) {
                    if self.tabs.len() > 1 {
                        let (x, y) = match delta {
                            winit::event::MouseScrollDelta::LineDelta(x, y) => {
                                (f64::from(x), f64::from(y))
                            }
                            winit::event::MouseScrollDelta::PixelDelta(position) => {
                                (position.x, position.y)
                            }
                        };
                        let direction = if y != 0.0 { y } else { x };
                        if direction > 0.0 {
                            self.select_tab(
                                self.active.checked_sub(1).unwrap_or(self.tabs.len() - 1),
                            );
                        } else if direction < 0.0 {
                            self.select_tab((self.active + 1) % self.tabs.len());
                        }
                    }
                } else {
                    let target_id = self
                        .pointer
                        .and_then(|point| self.tabs[self.active].layout.pane_at(point))
                        .unwrap_or(self.tabs[self.active].focused);
                    if let Some(pane) = self.tabs[self.active].pane_mut(target_id) {
                        pane.scroll(delta);
                    }
                    self.update_cursor_icon();
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn update_cursor_icon(&mut self) {
        let next = if let Some(divider) = self.dragging_divider {
            divider_cursor(divider.axis)
        } else if let Some(point) = self.pointer {
            if self.bar.contains(point) {
                CursorIcon::Default
            } else {
                let tab = &self.tabs[self.active];
                let busy = tab
                    .panes
                    .iter()
                    .any(|pane| pane.selection_drag.is_some() || pane.pressed.is_some());
                if !busy
                    && let Some(divider) = tab.layout.divider_at(point, self.divider_grab_margin())
                {
                    divider_cursor(divider.axis)
                } else if tab
                    .panes
                    .iter()
                    .any(|pane| pane.visible && pane.hovered_link.is_some())
                {
                    CursorIcon::Pointer
                } else {
                    CursorIcon::Default
                }
            }
        } else {
            CursorIcon::Default
        };
        if self.cursor_icon != next {
            self.cursor_icon = next;
            self.window.set_cursor(next);
        }
    }

    fn key(&mut self, event_loop: &ActiveEventLoop, event: &KeyEvent) {
        let buffer = self.tabs[self.active]
            .focused_pane()
            .session
            .screen()
            .active_buffer();
        if let Some(binding) = shortcuts::binding(event, self.modifiers, buffer) {
            if let Some(action) = binding.action(event.repeat) {
                let pass_through = (self.tabs.len() <= 1
                    && matches!(
                        action,
                        Action::SelectTab(_)
                            | Action::SelectLastTab
                            | Action::MoveTabLeft
                            | Action::MoveTabRight
                    ))
                    || (self.tabs[self.active].panes.len() <= 1
                        && matches!(action, Action::FocusPane(_) | Action::ResizePane(_)));
                if pass_through {
                    self.tabs[self.active]
                        .focused_pane_mut()
                        .key(event, &mut self.copy_pending);
                } else {
                    self.run_shortcut(event_loop, action);
                }
            }
        } else {
            self.tabs[self.active]
                .focused_pane_mut()
                .key(event, &mut self.copy_pending);
        }
    }

    fn run_shortcut(&mut self, event_loop: &ActiveEventLoop, action: Action) {
        match action {
            Action::Copy => self.tabs[self.active]
                .focused_pane()
                .copy(&mut self.copy_pending),
            Action::Paste => {
                let pane = self.tabs[self.active].focused_pane_mut();
                if pane.vi.is_none() {
                    pane.paste(ClipboardType::Regular, self.primary_selection.as_ref());
                }
            }
            Action::ToggleVi => self.tabs[self.active].focused_pane_mut().toggle_vi(),
            Action::PreviousPrompt => self.tabs[self.active].focused_pane_mut().jump_prompt(false),
            Action::NextPrompt => self.tabs[self.active].focused_pane_mut().jump_prompt(true),
            Action::CopyLastOutput => self.tabs[self.active]
                .focused_pane()
                .copy_last_output(&mut self.copy_pending),
            Action::ReloadConfig => {
                let _ = self.proxy.send_event(Event::ConfigReload(true));
            }
            Action::OpenDirectory => {
                if !self.directory_open_pending {
                    let pane = self.tabs[self.active].focused_pane();
                    let uri = pane.session.processor().working_directory_uri();
                    let fallback = pane.working_directory();
                    match directory::open(uri.map(str::to_owned), fallback, self.proxy.clone()) {
                        Ok(()) => self.directory_open_pending = true,
                        Err(error) => log::warn!("Failed to start the directory worker: {error}"),
                    }
                }
            }
            Action::Zoom(zoom) => {
                self.font_size_points = match zoom {
                    Zoom::In => (self.font_size_points + 1.0).min(MAX_FONT_SIZE),
                    Zoom::Out => (self.font_size_points - 1.0).max(MIN_FONT_SIZE),
                    Zoom::Reset => self.base_font_size_points,
                };
                self.resize_pending = true;
            }
            Action::ToggleFullscreen => {
                self.window.set_fullscreen(
                    self.window
                        .fullscreen()
                        .is_none()
                        .then_some(Fullscreen::Borderless(None)),
                );
            }
            Action::Scroll(scroll) => {
                let pane = self.tabs[self.active].focused_pane_mut();
                let page = (pane.session.geometry().pty_size().rows as isize - 1).max(1);
                let steps = match scroll {
                    Scroll::Oldest => isize::MAX,
                    Scroll::Live => isize::MIN,
                    Scroll::PageUp => page,
                    Scroll::PageDown => -page,
                };
                pane.scroll_history(steps);
            }
            Action::NewTab => {
                if let Err(error) = self.new_tab() {
                    log::warn!("Could not open a terminal tab: {error}");
                }
            }
            Action::CloseTab => {
                let focused = self.tabs[self.active].focused;
                self.close_pane(event_loop, self.active, focused);
            }
            Action::NextTab => self.select_tab((self.active + 1) % self.tabs.len()),
            Action::PreviousTab => {
                self.select_tab(self.active.checked_sub(1).unwrap_or(self.tabs.len() - 1))
            }
            Action::SelectTab(index) => {
                if index < self.tabs.len() {
                    self.select_tab(index);
                }
            }
            Action::SelectLastTab => self.select_tab(self.tabs.len() - 1),
            Action::MoveTabLeft => self.move_tab(-1),
            Action::MoveTabRight => self.move_tab(1),
            Action::SplitPane(axis) => {
                if let Err(error) = self.split_pane(axis) {
                    log::warn!("Could not split terminal pane: {error}");
                }
            }
            Action::FocusPane(direction) => self.focus_pane_direction(direction),
            Action::ResizePane(direction) => self.resize_pane_direction(direction),
            Action::TogglePaneZoom => self.toggle_pane_zoom(),
        }
    }

    fn split_pane(&mut self, axis: Axis) -> Result<()> {
        let cell = self.painter.metrics().cell;
        let divider = self.divider_thickness();
        let tab = &mut self.tabs[self.active];
        if tab.zoomed.take().is_some() {
            tab.reflow(self.content_area, divider, cell, true);
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
        let cwd = tab.focused_pane().working_directory();
        let new_id = self.allocate_pane_id()?;
        let mut pane = Pane::spawn(
            new_id,
            self.window.clone(),
            self.proxy.clone(),
            grid,
            &self.palette,
            self.scheme,
            cwd,
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

    fn focus_pane(&mut self, target_id: PaneId) {
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

    fn focus_pane_direction(&mut self, direction: SplitDirection) {
        if self.tabs[self.active].panes.len() <= 1 {
            return;
        }
        let cell = self.painter.metrics().cell;
        let divider = self.divider_thickness();
        let tab = &mut self.tabs[self.active];
        if tab.zoomed.take().is_some() {
            tab.reflow(self.content_area, divider, cell, true);
        }
        if let Some(next_id) = SplitTree::focus_neighbor(tab.focused, direction, &tab.layout) {
            self.focus_pane(next_id);
        }
    }

    fn resize_pane_direction(&mut self, direction: SplitDirection) {
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

    fn toggle_pane_zoom(&mut self) {
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

    fn reflow_active_tab(&mut self) {
        let cell = self.painter.metrics().cell;
        let divider = self.divider_thickness();
        self.tabs[self.active].reflow(self.content_area, divider, cell, true);
        self.tabs[self.active].focused_pane_mut().update_ime_area();
    }

    fn move_tab(&mut self, delta: isize) {
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

    fn new_tab(&mut self) -> Result<()> {
        let id = self.allocate_pane_id()?;
        let cwd = self.tabs[self.active].focused_pane().working_directory();
        let size = self.window.inner_size();
        let metrics = self.painter.metrics();
        let bar_height = tab_bar::Layout::height(size, metrics.cell, self.tabs.len() + 1);
        let area = content_rect(size, bar_height).unwrap_or(self.content_area);
        let grid = GridGeometry::fit(area.size, metrics.cell)
            .unwrap_or(self.tabs[self.active].focused_pane().target_grid);
        let mut pane = Pane::spawn(
            id,
            self.window.clone(),
            self.proxy.clone(),
            grid,
            &self.palette,
            self.scheme,
            cwd,
        )?;
        self.readiness
            .arm(id, &mut pane.registration, pane.session.interests())?;
        self.tabs.push(Tab::new(pane, area));
        self.resize_pending = true;
        self.apply_resize();
        self.select_tab(self.tabs.len() - 1);
        Ok(())
    }

    fn select_tab(&mut self, index: usize) {
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

    fn activate_tab(&mut self) {
        let cell = self.painter.metrics().cell;
        let divider = self.divider_thickness();
        self.tabs[self.active].reflow(self.content_area, divider, cell, true);
        self.activate_focused_pane();
        self.painter.switch_session();
        self.window.request_redraw();
    }

    fn activate_focused_pane(&mut self) {
        self.window.set_ime_allowed(false);
        let focused_id = self.tabs[self.active].focused;
        for pane in &mut self.tabs[self.active].panes {
            pane.active = pane.id == focused_id;
            pane.modifiers = self.modifiers;
        }
        let pane = self.tabs[self.active].focused_pane_mut();
        pane.ime_enabled = self.ime_enabled;
        pane.ime_area = None;
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

    fn close_pane(&mut self, event_loop: &ActiveEventLoop, tab_index: usize, pane_id: PaneId) {
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
        if tab.focused == pane_id
            && let Some(next_id) = next_focus.or_else(|| tab.panes.first().map(|pane| pane.id))
        {
            tab.focused = next_id;
        }
        let cell = self.painter.metrics().cell;
        let divider = self.divider_thickness();
        let is_active = tab_index == self.active;
        self.tabs[tab_index].reflow(self.content_area, divider, cell, is_active);
        if is_active {
            self.dragging_divider = None;
            self.activate_focused_pane();
        }
    }

    fn close_tab(&mut self, event_loop: &ActiveEventLoop, index: usize) {
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

    fn update_window_title(&self) {
        let title = &self.tabs[self.active].focused_pane().title;
        self.window
            .set_title(if title.is_empty() { "Lune" } else { title });
    }

    fn apply_system_theme(&mut self, theme: Theme) {
        self.window.set_theme(Some(theme));
        (self.palette, self.scheme) = theme::palette(theme);
        for tab in &mut self.tabs {
            for pane in &mut tab.panes {
                pane.session.set_theme(&self.palette, Some(self.scheme));
            }
        }
        self.painter.invalidate_overlay();
        self.window.request_redraw();
    }

    fn set_notice(&mut self, text: Option<String>) {
        let changed = match text {
            Some(text) => {
                self.notice = Some(Notice {
                    text,
                    expires: Instant::now() + NOTICE_DURATION,
                });
                true
            }
            None => self.notice.take().is_some(),
        };
        if changed {
            self.painter.invalidate_overlay();
            self.window.request_redraw();
        }
    }

    fn apply_config(&mut self, old: &Config, new: &Config, system_theme: Theme) {
        if old.theme != new.theme {
            let selected = new.theme.resolve(system_theme);
            self.window.set_theme(Some(selected));
            (self.palette, self.scheme) = new.theme.palette(selected);
            for tab in &mut self.tabs {
                for pane in &mut tab.panes {
                    pane.session.set_theme(&self.palette, Some(self.scheme));
                }
            }
        }
        if old.background_opacity != new.background_opacity {
            self.painter.set_background_opacity(new.background_opacity);
        }
        let mut layout_changed = false;
        if old.font_size_points != new.font_size_points {
            self.base_font_size_points = new.font_size_points;
            self.font_size_points = new.font_size_points;
            layout_changed = true;
        }
        if old.font_family != new.font_family || old.font_ligatures != new.font_ligatures {
            let physical_font =
                physical_font_size(self.font_size_points, self.window.scale_factor());
            self.painter.set_font(
                new.font_family.as_deref(),
                new.font_ligatures,
                physical_font,
            );
            layout_changed = true;
        }
        self.clipboard_write = new.clipboard_write;
        self.clipboard_read = new.clipboard_read;
        self.notifications_policy = new.notifications;
        if new.notifications == NotificationPolicy::Deny {
            self.notifier = None;
        } else if self.notifier.is_none() {
            match Notifier::start() {
                Ok(notifier) => self.notifier = Some(notifier),
                Err(error) => log::warn!("Could not start desktop notifications: {error}"),
            }
        }
        if layout_changed {
            self.resize_pending = true;
            self.apply_resize();
        } else {
            self.painter.invalidate_overlay();
            self.window.request_redraw();
        }
    }

    fn pty_ready(&mut self, event_loop: &ActiveEventLoop, token: u64) -> Result<()> {
        let (id, sequence) = Readiness::decode(token);
        let location = self.tabs.iter().enumerate().find_map(|(tab_idx, tab)| {
            tab.panes
                .iter()
                .position(|pane| pane.id == id)
                .map(|pane_idx| (tab_idx, pane_idx))
        });
        if let Some((tab_idx, pane_idx)) = location
            && let Some(ready) = self.tabs[tab_idx].panes[pane_idx]
                .registration
                .ready(sequence)
        {
            self.dispatch(event_loop, tab_idx, id, ready)?;
        }
        Ok(())
    }

    fn dispatch(
        &mut self,
        event_loop: &ActiveEventLoop,
        tab_index: usize,
        pane_id: PaneId,
        ready: SessionReady,
    ) -> Result<bool> {
        let is_focused =
            self.focused && tab_index == self.active && self.tabs[tab_index].focused == pane_id;
        let Some(pane) = self.tabs[tab_index].pane_mut(pane_id) else {
            return Ok(false);
        };
        let running = match pane.dispatch(
            ready,
            tab::DispatchContext {
                focused: is_focused,
                copy_pending: &mut self.copy_pending,
                primary: &mut self.primary_selection,
                clipboard_write: self.clipboard_write,
                clipboard_read: self.clipboard_read,
                notifications_policy: self.notifications_policy,
                notifier: self.notifier.as_ref(),
            },
        ) {
            Ok(running) => running,
            Err(error) => {
                self.pane_error(event_loop, tab_index, pane_id, error)?;
                return Ok(false);
            }
        };
        if !running {
            self.close_pane(event_loop, tab_index, pane_id);
        } else if let Some(pane) = self.tabs[tab_index].pane_mut(pane_id) {
            let title_changed = std::mem::take(&mut pane.title_changed);
            let progress_changed = std::mem::take(&mut pane.progress_changed);
            let is_tab_focused_pane = self.tabs[tab_index].focused == pane_id;
            if title_changed && is_tab_focused_pane {
                if tab_index == self.active {
                    self.update_window_title();
                }
                if self.bar.height != 0 {
                    self.reflow_tab_bar();
                }
            }
            if (title_changed && is_tab_focused_pane && self.bar.height != 0)
                || (progress_changed && (tab_index == self.active || self.bar.height != 0))
            {
                self.painter.invalidate_overlay();
                self.window.request_redraw();
            }
            if tab_index == self.active {
                self.update_cursor_icon();
            }
        }
        Ok(running)
    }

    fn pane_error(
        &mut self,
        event_loop: &ActiveEventLoop,
        tab_index: usize,
        pane_id: PaneId,
        error: Box<dyn std::error::Error>,
    ) -> Result<()> {
        if self.tabs.len() == 1 && self.tabs[tab_index].panes.len() == 1 {
            return Err(error);
        }
        log::warn!("Terminal pane failed: {error}");
        self.close_pane(event_loop, tab_index, pane_id);
        Ok(())
    }

    fn wait(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        let now = Instant::now();
        self.apply_resize();
        for pane in &mut self.tabs[self.active].panes {
            pane.tick_selection_scroll(now);
        }
        let mut deadline = self.tabs[self.active]
            .panes
            .iter()
            .filter_map(|pane| {
                pane.selection_drag
                    .as_ref()
                    .and_then(|drag| drag.edge_scroll.as_ref().map(|scroll| scroll.deadline))
            })
            .min();
        let mut immediate = false;
        let mut tab_idx = 0;
        while tab_idx < self.tabs.len() {
            let mut pane_idx = 0;
            while tab_idx < self.tabs.len() && pane_idx < self.tabs[tab_idx].panes.len() {
                let pane_id = self.tabs[tab_idx].panes[pane_idx].id;
                let interests = self.tabs[tab_idx].panes[pane_idx].session.interests();
                if (interests.immediate
                    || interests
                        .presentation_deadline
                        .is_some_and(|deadline| deadline <= now))
                    && !self.dispatch(event_loop, tab_idx, pane_id, SessionReady::default())?
                {
                    if event_loop.exiting() {
                        return Ok(());
                    }
                    continue;
                }
                if tab_idx >= self.tabs.len()
                    || pane_idx >= self.tabs[tab_idx].panes.len()
                    || self.tabs[tab_idx].panes[pane_idx].id != pane_id
                {
                    continue;
                }
                match self.tabs[tab_idx].panes[pane_idx].service(&self.readiness, now) {
                    Ok((work, pane_deadline)) => {
                        immediate |= work;
                        deadline = deadline.into_iter().chain(pane_deadline).min();
                    }
                    Err(error) => {
                        self.pane_error(event_loop, tab_idx, pane_id, error)?;
                        continue;
                    }
                }
                pane_idx += 1;
            }
            tab_idx += 1;
        }
        let focused_pane = self.tabs[self.active].focused_pane_mut();
        let focused_id = focused_pane.id;
        let blink = std::mem::take(&mut focused_pane.restart_blink)
            && self.painter.restart_blink(focused_id, now);
        let notice_expired = self
            .notice
            .as_ref()
            .is_some_and(|notice| now >= notice.expires);
        if notice_expired {
            self.notice = None;
            self.painter.invalidate_overlay();
        }
        if self.painter.tick(now) || blink || notice_expired {
            self.window.request_redraw();
        }
        deadline = deadline
            .into_iter()
            .chain(self.painter.deadline())
            .chain(self.notice.as_ref().map(|notice| notice.expires))
            .min();
        let flow = if immediate {
            ControlFlow::Poll
        } else {
            deadline.map_or(ControlFlow::Wait, ControlFlow::WaitUntil)
        };
        event_loop.set_control_flow(flow);
        Ok(())
    }

    fn apply_resize(&mut self) {
        if !std::mem::take(&mut self.resize_pending) {
            return;
        }
        let size = self.window.inner_size();
        let physical_font = physical_font_size(self.font_size_points, self.window.scale_factor());
        let current = self.painter.metrics();
        let metrics = if current.size != physical_font {
            self.painter.measure(physical_font)
        } else {
            current
        };
        let bar_height = tab_bar::Layout::height(size, metrics.cell, self.tabs.len());
        let Some(content_area) = content_rect(size, bar_height) else {
            return;
        };
        if GridGeometry::fit(content_area.size, metrics.cell).is_none() {
            return;
        }
        self.content_area = content_area;
        self.painter.set_metrics(metrics);
        self.painter.resize(size);
        let divider = self.divider_thickness();
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            tab.reflow(content_area, divider, metrics.cell, index == self.active);
        }
        self.reflow_tab_bar();
        self.tabs[self.active].focused_pane_mut().update_ime_area();
        self.window.request_redraw();
    }

    fn reflow_tab_bar(&mut self) {
        self.bar.reflow(
            self.painter.size(),
            self.painter.metrics().cell,
            self.active,
            self.tabs.len(),
            |index| self.tabs[index].label(),
        );
    }

    fn draw(&mut self) -> Result<()> {
        let active_tab = &mut self.tabs[self.active];
        for layout_pane in &active_tab.layout.panes {
            let Some(pane) = active_tab
                .panes
                .iter_mut()
                .find(|pane| pane.id == layout_pane.id)
            else {
                continue;
            };
            if pane.session.geometry() != pane.target_grid {
                return Ok(());
            }
            if std::mem::take(&mut pane.dirty) {
                self.painter.invalidate_overlay();
            }
        }
        let active_tab = &self.tabs[self.active];
        let zoomed = active_tab.is_zoomed();
        let mut draw_panes = Vec::with_capacity(active_tab.layout.panes.len());
        let mut reverse_video = false;
        for layout_pane in &active_tab.layout.panes {
            let Some(pane) = active_tab.pane(layout_pane.id) else {
                continue;
            };
            let Some(mut frame) = pane.session.presentation_frame() else {
                return Ok(());
            };
            let focused = self.focused && pane.id == active_tab.focused;
            if pane.vi.is_some() || !focused {
                frame.cursor_appearance.visible = false;
            }
            if pane.id == active_tab.focused {
                reverse_video = frame.reverse_video;
            }
            draw_panes.push(paint::PaneDraw {
                id: pane.id,
                rect: layout_pane.rect,
                frame,
                selection: pane.selection.as_ref().map(|selection| &selection.range),
                hovered_link: pane.hovered_link.as_ref(),
                progress: pane.progress,
                preedit: if pane.id == active_tab.focused {
                    &pane.preedit
                } else {
                    ""
                },
                vi: pane.vi.as_ref(),
                focused,
                zoomed: zoomed && pane.id == active_tab.focused,
            });
        }
        let focused_rect = (active_tab.layout.panes.len() > 1)
            .then(|| active_tab.layout.rect_of(active_tab.focused))
            .flatten();
        self.painter.draw(
            &self.window,
            &draw_panes,
            paint::WindowChrome {
                palette: &self.palette,
                reverse_video,
                dividers: &active_tab.layout.dividers,
                focused_rect,
                notice: self.notice.as_ref().map(|notice| notice.text.as_str()),
                tab_bar: tab_bar::View {
                    layout: &self.bar,
                    tabs: &self.tabs,
                    active: self.active,
                    palette: &self.palette,
                },
            },
        )?;
        Ok(())
    }
}

fn content_rect(size: PhysicalSize<u32>, bar_height: u32) -> Option<PixelRect> {
    Some(PixelRect {
        origin: PixelPosition { x: 0, y: 0 },
        size: PixelSize {
            width: NonZeroU32::new(size.width)?,
            height: NonZeroU32::new(size.height.saturating_sub(bar_height))?,
        },
    })
}

fn divider_cursor(axis: Axis) -> CursorIcon {
    match axis {
        Axis::Columns => CursorIcon::ColResize,
        Axis::Rows => CursorIcon::RowResize,
    }
}
