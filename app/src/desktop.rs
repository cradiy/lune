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
mod tab;
mod tab_bar;
mod text;
mod theme;
mod vi;

use std::io;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lune_terminal::{ClipboardQuery, ColorScheme, Palette, PixelSize};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{Ime, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::platform::wayland::{EventLoopBuilderExtWayland, WindowAttributesExtWayland};
use winit::window::{Fullscreen, Theme, Window, WindowId};

use crate::{GridGeometry, SessionReady};

use clipboard::ClipboardType;
use config::{ClipboardPolicy, Config, MAX_FONT_SIZE, MIN_FONT_SIZE, NotificationPolicy};
use font::{Font, FontMetrics, physical_font_size};
use notify::Notifier;
use paint::Painter;
use readiness::Readiness;
use shortcuts::{Action, Scroll, Zoom};
use tab::Tab;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

type TabId = u32;

const PASTE_TIMEOUT: Duration = Duration::from_secs(2);
const NOTICE_DURATION: Duration = Duration::from_secs(6);

struct Notice {
    text: String,
    expires: Instant,
}

enum Event {
    PtyReady(u64),
    PtyError(io::Error),
    CopyReady(std::result::Result<(), String>),
    PasteReady(TabId, std::result::Result<String, String>),
    Osc52ReadReady(TabId, ClipboardQuery, std::result::Result<String, String>),
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
                if let Some(tab) = client.tabs.iter_mut().find(|tab| tab.id == id) {
                    tab.finish_paste(result);
                }
                Ok(())
            }
            Event::Osc52ReadReady(id, query, result) => {
                if let Some(tab) = client.tabs.iter_mut().find(|tab| tab.id == id) {
                    tab.finish_osc52_read(query, result);
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
    next_tab: TabId,
    palette: Palette,
    scheme: ColorScheme,
    bar: tab_bar::Layout,
    grid: GridGeometry,
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
        let grid = geometry(window.inner_size(), painter.metrics(), 0)
            .ok_or_else(|| io::Error::other("The terminal grid does not fit the Wayland window"))?;
        let (palette, scheme) = config.theme.palette(selected_theme);
        let mut tab = Tab::spawn(1, window.clone(), proxy.clone(), grid, &palette, scheme)?;
        let readiness = Readiness::new(proxy.clone())?;
        readiness.arm(tab.id, &mut tab.registration, tab.session.interests())?;
        tab.active = true;
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
            next_tab: 2,
            palette,
            scheme,
            bar,
            grid,
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
        })
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
                self.tabs[self.active].focus(focused);
                if !focused {
                    if self.painter.reset_cursor_motion() {
                        self.window.request_redraw();
                    }
                    self.modifiers = ModifiersState::empty();
                    self.tabs[self.active].modifiers = self.modifiers;
                    self.tabs[self.active].update_hovered_hyperlink();
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
                self.tabs[self.active].modifiers = self.modifiers;
                self.tabs[self.active].update_hovered_hyperlink();
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } => {
                if event.state.is_pressed() {
                    self.key(event_loop, &event);
                } else {
                    self.tabs[self.active].key_release(&event);
                }
            }
            WindowEvent::Ime(Ime::Enabled) => {
                self.ime_enabled = true;
                let tab = &mut self.tabs[self.active];
                tab.ime_enabled = true;
                tab.ime_area = None;
                tab.update_ime_area();
            }
            WindowEvent::Ime(Ime::Disabled) => {
                self.ime_enabled = false;
                let tab = &mut self.tabs[self.active];
                tab.ime_enabled = false;
                tab.set_preedit(String::new());
            }
            WindowEvent::Ime(ime) => self.tabs[self.active].ime(ime),
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer = Some(position);
                self.tabs[self.active].pointer_moved(position);
            }
            WindowEvent::CursorLeft { .. } => {
                self.pointer = None;
                let tab = &mut self.tabs[self.active];
                if tab.pressed.is_none() && tab.selection_drag.is_none() {
                    tab.pointer = None;
                }
                tab.update_hovered_hyperlink();
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if state.is_pressed() && self.pointer.is_some_and(|point| self.bar.contains(point))
                {
                    if button == winit::event::MouseButton::Left
                        && let Some(index) = self.bar.hit(self.pointer.unwrap())
                        && index < self.tabs.len()
                    {
                        self.select_tab(index);
                    }
                } else {
                    self.tabs[self.active].button(
                        state,
                        button,
                        &mut self.primary_selection,
                        &mut self.hyperlink_open_pending,
                    );
                }
            }
            WindowEvent::MouseWheel { delta, .. }
                if !self.pointer.is_some_and(|point| self.bar.contains(point)) =>
            {
                self.tabs[self.active].scroll(delta);
            }
            _ => {}
        }
        Ok(())
    }

    fn key(&mut self, event_loop: &ActiveEventLoop, event: &KeyEvent) {
        let buffer = self.tabs[self.active].session.screen().active_buffer();
        if let Some(binding) = shortcuts::binding(event, self.modifiers, buffer) {
            if let Some(action) = binding.action(event.repeat) {
                self.run_shortcut(event_loop, action);
            }
        } else {
            self.tabs[self.active].key(event, &mut self.copy_pending);
        }
    }

    fn run_shortcut(&mut self, event_loop: &ActiveEventLoop, action: Action) {
        let tab = &mut self.tabs[self.active];
        match action {
            Action::Copy => tab.copy(&mut self.copy_pending),
            Action::Paste => {
                if tab.vi.is_none() {
                    tab.paste(ClipboardType::Regular, self.primary_selection.as_ref());
                }
            }
            Action::ToggleVi => tab.toggle_vi(),
            Action::PreviousPrompt => tab.jump_prompt(false),
            Action::NextPrompt => tab.jump_prompt(true),
            Action::CopyLastOutput => tab.copy_last_output(&mut self.copy_pending),
            Action::ReloadConfig => {
                let _ = self.proxy.send_event(Event::ConfigReload(true));
            }
            Action::OpenDirectory => {
                if !self.directory_open_pending {
                    let uri = tab.session.processor().working_directory_uri();
                    match directory::open(uri.map(str::to_owned), self.proxy.clone()) {
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
                let page = (tab.session.geometry().pty_size().rows as isize - 1).max(1);
                let steps = match scroll {
                    Scroll::Oldest => isize::MAX,
                    Scroll::Live => isize::MIN,
                    Scroll::PageUp => page,
                    Scroll::PageDown => -page,
                };
                tab.scroll_history(steps);
            }
            Action::NewTab => {
                if let Err(error) = self.new_tab() {
                    log::warn!("Could not open a terminal tab: {error}");
                }
            }
            Action::CloseTab => self.close_tab(event_loop, self.active),
            Action::NextTab => self.select_tab((self.active + 1) % self.tabs.len()),
            Action::PreviousTab => {
                self.select_tab(self.active.checked_sub(1).unwrap_or(self.tabs.len() - 1))
            }
        }
    }

    fn new_tab(&mut self) -> Result<()> {
        let id = self.next_tab;
        self.next_tab = id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Tab IDs exhausted"))?;
        let size = self.window.inner_size();
        let metrics = self.painter.metrics();
        let bar_height = tab_bar::Layout::height(size, metrics.cell, self.tabs.len() + 1);
        let grid = geometry(size, metrics, bar_height).unwrap_or(self.grid);
        let mut tab = Tab::spawn(
            id,
            self.window.clone(),
            self.proxy.clone(),
            grid,
            &self.palette,
            self.scheme,
        )?;
        self.readiness
            .arm(id, &mut tab.registration, tab.session.interests())?;
        self.tabs.push(tab);
        self.resize_pending = true;
        self.apply_resize();
        self.select_tab(self.tabs.len() - 1);
        Ok(())
    }

    fn select_tab(&mut self, index: usize) {
        if index == self.active {
            return;
        }
        let old = &mut self.tabs[self.active];
        old.active = false;
        old.focus(false);
        old.pointer = None;
        self.active = index;
        self.activate_tab();
    }

    fn activate_tab(&mut self) {
        self.window.set_ime_allowed(false);
        let tab = &mut self.tabs[self.active];
        tab.active = true;
        tab.modifiers = self.modifiers;
        tab.ime_enabled = self.ime_enabled;
        tab.ime_area = None;
        tab.pointer = self.pointer;
        tab.set_ime_allowed();
        tab.update_ime_area();
        tab.update_hovered_hyperlink();
        if self.focused {
            tab.focus(true);
        }
        self.update_window_title();
        self.reflow_tab_bar();
        self.painter.switch_session();
        self.window.request_redraw();
    }

    fn close_tab(&mut self, event_loop: &ActiveEventLoop, index: usize) {
        if self.tabs.len() == 1 {
            event_loop.exit();
            return;
        }
        self.tabs.remove(index);
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
        let title = &self.tabs[self.active].title;
        self.window
            .set_title(if title.is_empty() { "Lune" } else { title });
    }

    fn apply_system_theme(&mut self, theme: Theme) {
        self.window.set_theme(Some(theme));
        (self.palette, self.scheme) = theme::palette(theme);
        for tab in &mut self.tabs {
            tab.session.set_theme(&self.palette, Some(self.scheme));
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
                tab.session.set_theme(&self.palette, Some(self.scheme));
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
        if let Some(index) = self.tabs.iter().position(|tab| tab.id == id)
            && let Some(ready) = self.tabs[index].registration.ready(sequence)
        {
            self.dispatch(event_loop, index, ready)?;
        }
        Ok(())
    }

    fn dispatch(
        &mut self,
        event_loop: &ActiveEventLoop,
        index: usize,
        ready: SessionReady,
    ) -> Result<bool> {
        let running = match self.tabs[index].dispatch(
            ready,
            tab::DispatchContext {
                focused: self.focused && index == self.active,
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
                self.tab_error(event_loop, index, error)?;
                return Ok(false);
            }
        };
        if !running {
            self.close_tab(event_loop, index);
        } else {
            let title_changed = std::mem::take(&mut self.tabs[index].title_changed);
            let progress_changed = std::mem::take(&mut self.tabs[index].progress_changed);
            if title_changed {
                if index == self.active {
                    self.update_window_title();
                }
                if self.bar.height != 0 {
                    self.reflow_tab_bar();
                }
            }
            if (title_changed && self.bar.height != 0)
                || (progress_changed && (index == self.active || self.bar.height != 0))
            {
                self.painter.invalidate_overlay();
                self.window.request_redraw();
            }
        }
        Ok(running)
    }

    fn tab_error(
        &mut self,
        event_loop: &ActiveEventLoop,
        index: usize,
        error: Box<dyn std::error::Error>,
    ) -> Result<()> {
        if self.tabs.len() == 1 {
            return Err(error);
        }
        log::warn!("Terminal tab failed: {error}");
        self.close_tab(event_loop, index);
        Ok(())
    }

    fn wait(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        let now = Instant::now();
        self.apply_resize();
        self.tabs[self.active].tick_selection_scroll(now);
        let mut deadline = self.tabs[self.active]
            .selection_drag
            .as_ref()
            .and_then(|drag| drag.edge_scroll.as_ref().map(|scroll| scroll.deadline));
        let mut immediate = false;
        let mut index = 0;
        while index < self.tabs.len() {
            let interests = self.tabs[index].session.interests();
            if (interests.immediate
                || interests
                    .presentation_deadline
                    .is_some_and(|deadline| deadline <= now))
                && !self.dispatch(event_loop, index, SessionReady::default())?
            {
                if event_loop.exiting() {
                    return Ok(());
                }
                continue;
            }
            match self.tabs[index].service(&self.readiness, self.grid, now) {
                Ok((work, tab_deadline)) => {
                    immediate |= work;
                    deadline = deadline.into_iter().chain(tab_deadline).min();
                }
                Err(error) => {
                    self.tab_error(event_loop, index, error)?;
                    continue;
                }
            }
            index += 1;
        }
        let blink = std::mem::take(&mut self.tabs[self.active].restart_blink)
            && self.painter.restart_blink(now);
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
        let Some(grid) = geometry(size, metrics, bar_height) else {
            return;
        };
        self.grid = grid;
        self.painter.set_metrics(metrics);
        self.painter.resize(size);
        self.reflow_tab_bar();
        self.tabs[self.active].update_ime_area();
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
        let tab = &mut self.tabs[self.active];
        if tab.session.geometry() != self.grid {
            return Ok(());
        }
        if std::mem::take(&mut tab.dirty) {
            self.painter.invalidate_overlay();
        }
        let tab = &self.tabs[self.active];
        if let Some(mut frame) = tab.session.presentation_frame() {
            if tab.vi.is_some() {
                frame.cursor_appearance.visible = false;
            }
            self.painter.draw(
                &self.window,
                &frame,
                paint::Overlay {
                    selection: tab.selection.as_ref().map(|selection| &selection.range),
                    hovered_hyperlink: tab.hovered_hyperlink,
                    progress: tab.progress,
                    progress_phase: 0.0,
                    background_opacity: 1.0,
                    notice: self.notice.as_ref().map(|notice| notice.text.as_str()),
                    preedit: &tab.preedit,
                    vi: tab.vi.as_ref(),
                    focused: self.focused,
                    tab_bar: tab_bar::View {
                        layout: &self.bar,
                        tabs: &self.tabs,
                        active: self.active,
                        palette: &self.palette,
                    },
                },
            )?;
        }
        Ok(())
    }
}

fn geometry(
    size: PhysicalSize<u32>,
    metrics: FontMetrics,
    bar_height: u32,
) -> Option<GridGeometry> {
    let area = PixelSize {
        width: NonZeroU32::new(size.width)?,
        height: NonZeroU32::new(size.height.saturating_sub(bar_height))?,
    };
    GridGeometry::fit(area, metrics.cell)
}
