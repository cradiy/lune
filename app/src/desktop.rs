mod bg_image;
mod box_drawing;
pub mod cli;
mod clipboard;
mod config;
mod confirm;
mod contrast;
mod cursor;
mod directory;
mod font;
mod hints;
mod ime;
mod input;
mod ligature;
mod notify;
mod paint;
mod palette;
mod readiness;
mod scrollbar;
mod search;
mod selection;
mod shell_integration;
mod shortcuts;
mod split;
mod svg;
mod tab;
mod tab_bar;
mod text;
mod theme;
mod titlebar;
mod url;
mod vi;
mod wayland_decorations;

use std::ffi::OsString;
use std::io;
use std::num::NonZeroU32;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use lune_terminal::{
    ClipboardQuery, ColorScheme, CursorShape, HistoryLimits, Palette, PixelPosition, PixelSize,
};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{Ime, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::wayland::{EventLoopBuilderExtWayland, WindowAttributesExtWayland};
use winit::window::{CursorIcon, Fullscreen, ImePurpose, ResizeDirection, Theme, Window, WindowId};

use crate::{GridGeometry, PixelRect, SessionReady};

use cli::LaunchOptions;
use clipboard::ClipboardType;
use config::{
    ClipboardPolicy, Config, ConfigSource, MAX_FONT_SIZE, MIN_FONT_SIZE, NotificationPolicy,
    TabPosition, TerminalMode, WindowControlsPolicy,
};
use font::{Font, physical_font_size};
use notify::Notifier;
use paint::{NoticeKind, NoticeView, Painter, PainterOptions};
use palette::{CommandPalette, PaletteAction, PaletteContext};
use readiness::Readiness;
use scrollbar::ScrollbarPolicy;
use shortcuts::{Action, Keymap, Scroll, Zoom};
use split::{Axis, Direction as SplitDirection, Divider, SplitTree};
use tab::{Pane, PaneConfig, Tab};
use titlebar::{SidebarResizeDrag, TabAnimator, TabDrag, TitlebarAction};
use wayland_decorations::{
    CompositorDecorations, WindowButtons, probe_wayland_decorations, resolve_window_buttons,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub(super) type PaneId = u32;

const PASTE_TIMEOUT: Duration = Duration::from_secs(2);
const TOAST_SUCCESS_DURATION: Duration = Duration::from_millis(1800);
const TOAST_WARNING_DURATION: Duration = Duration::from_millis(2500);
const TOAST_ERROR_DURATION: Duration = Duration::from_secs(6);
const SPLIT_RESIZE_STEP: f32 = 0.05;
const MIN_SPLIT_COLUMNS: u16 = 4;
const MIN_SPLIT_ROWS: u16 = 2;

struct Notice {
    kind: NoticeKind,
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
    ButtonLayout(Option<WindowButtons>),
    ConfigReload(bool),
}

pub fn run(mut options: LaunchOptions) -> Result<()> {
    if let Some(path) = options.working_directory.take() {
        let resolved = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()?.join(path)
        };
        if !resolved.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "Working directory does not exist or is not a directory: {}",
                    resolved.display()
                ),
            )
            .into());
        }
        options.working_directory = Some(resolved);
    }
    let config_source = ConfigSource::from_option(options.config.take());
    let loaded = Config::load(&config_source);
    let startup_notice = config::format_errors(&loaded.errors);
    let event_loop = EventLoop::<Event>::with_user_event()
        .with_wayland()
        .build()?;
    log::debug!("Wayland event loop ready");
    let proxy = event_loop.create_proxy();
    let config_watch =
        config_source.watch_target().and_then(
            |(directory, file)| match config::watch::Watch::start(directory, file, proxy.clone()) {
                Ok(watch) => Some(watch),
                Err(error) => {
                    log::warn!("Could not start the configuration watcher: {error}");
                    None
                }
            },
        );
    let mut app = App {
        proxy,
        config_source,
        config: loaded.config,
        launch: Some(options),
        startup_notice,
        client: None,
        error: None,
        system_theme: Theme::Dark,
        portal_buttons: None,
        _theme_watch: None,
        _config_watch: config_watch,
    };
    app.sync_theme_watch();
    event_loop.run_app(&mut app)?;
    app.error.map_or(Ok(()), Err)
}

struct App {
    proxy: EventLoopProxy<Event>,
    config_source: ConfigSource,
    config: Config,
    launch: Option<LaunchOptions>,
    startup_notice: Option<String>,
    client: Option<Client>,
    error: Option<Box<dyn std::error::Error>>,
    system_theme: Theme,
    portal_buttons: Option<WindowButtons>,
    _theme_watch: Option<theme::system::Watch>,
    _config_watch: Option<config::watch::Watch>,
}

impl App {
    fn sync_theme_watch(&mut self) {
        if matches!(self.config.theme, theme::Preference::System)
            || self.config.window_controls == WindowControlsPolicy::Auto
        {
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
        let loaded = match Config::load_checked(&self.config_source) {
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
            client.apply_config(&self.config, &loaded.config, self.system_theme, force);
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
            let launch = self.launch.take().unwrap_or_default();
            let custom_config = self.config_source.custom_path().map(PathBuf::from);
            match Client::new(
                event_loop,
                ClientInit {
                    proxy: self.proxy.clone(),
                    config: &self.config,
                    system_theme: self.system_theme,
                    portal_buttons: self.portal_buttons,
                    launch,
                    custom_config,
                    startup_notice: self.startup_notice.take(),
                },
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
        if let Event::ButtonLayout(buttons) = event {
            if self.portal_buttons == buttons {
                return;
            }
            self.portal_buttons = buttons;
            if let Some(client) = &mut self.client {
                client.apply_portal_buttons(buttons);
            }
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
                let confirm_paste = client.confirm_paste;
                let pending_confirm = client
                    .pane_mut(id)
                    .and_then(|pane| pane.finish_paste(result, confirm_paste));
                if let Some(text) = pending_confirm {
                    let message = confirm::format_paste(&text);
                    client.open_confirm(confirm::ConfirmTarget::Paste { pane: id, text }, message);
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
            Event::ButtonLayout(_) | Event::ConfigReload(_) => unreachable!(),
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
    theme_preference: theme::Preference,
    system_theme: Theme,
    config_dir: Option<PathBuf>,
    bar: tab_bar::Layout,
    titlebar: titlebar::Layout,
    titlebar_hovered: Option<TitlebarAction>,
    titlebar_pressed: Option<TitlebarAction>,
    titlebar_last_click: Option<Instant>,
    tab_animator: TabAnimator,
    tab_drag: Option<TabDrag>,
    sidebar_collapsed: bool,
    sidebar_width_cells: Option<f64>,
    sidebar_resize_drag: Option<SidebarResizeDrag>,
    sidebar_divider_last_click: Option<Instant>,
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
    default_title: String,
    custom_app_id: Option<String>,
    custom_config: Option<PathBuf>,
    keymap: Keymap,
    unfocused_split_opacity: f32,
    window_padding: u32,
    history_limits: HistoryLimits,
    cursor_style: CursorShape,
    cursor_blink: bool,
    shell: Option<Vec<OsString>>,
    shell_integration: bool,
    mode: TerminalMode,
    tab_position: TabPosition,
    window_controls: WindowControlsPolicy,
    compositor_decorations: Option<CompositorDecorations>,
    portal_buttons: Option<WindowButtons>,
    scrollbar: ScrollbarPolicy,
    confirm_close: bool,
    confirm_paste: bool,
    copy_on_select: bool,
    confirm: Option<confirm::Dialog>,
    command_palette: Option<CommandPalette>,
}

struct ClientInit<'a> {
    proxy: EventLoopProxy<Event>,
    config: &'a Config,
    system_theme: Theme,
    portal_buttons: Option<WindowButtons>,
    launch: LaunchOptions,
    custom_config: Option<PathBuf>,
    startup_notice: Option<String>,
}

impl Client {
    fn new(event_loop: &ActiveEventLoop, init: ClientInit<'_>) -> Result<Self> {
        let ClientInit {
            proxy,
            config,
            system_theme,
            portal_buttons,
            launch,
            custom_config,
            startup_notice,
        } = init;
        let compositor_decorations = probe_wayland_decorations();
        let selected_theme = config.theme.resolve(system_theme);
        let default_title = launch
            .title
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| "Lune".into());
        let custom_app_id = launch.app_id;
        let app_id = custom_app_id.as_deref().unwrap_or("lune");
        let window = Arc::new(
            event_loop.create_window(
                Window::default_attributes()
                    .with_title(&default_title)
                    .with_name(app_id, "")
                    .with_theme(Some(selected_theme))
                    .with_decorations(config.mode == TerminalMode::Classic)
                    .with_transparent(true)
                    .with_inner_size(LogicalSize::new(960, 600))
                    .with_min_inner_size(LogicalSize::new(180, 100)),
            )?,
        );
        let scale = window.scale_factor();
        let font_config = config.font_config();
        let mut painter = pollster::block_on(Painter::new(
            window.clone(),
            Font::new(&font_config),
            PainterOptions {
                physical_font_size: physical_font_size(config.font_size_points, scale),
                scale_factor: scale,
                background_opacity: config.background_opacity,
                minimum_contrast: config.minimum_contrast,
                cursor_blink: config.cursor_blink,
                cursor_animation: config.cursor_animation,
            },
        ))?;
        let bg_image_error = painter
            .set_background_image(config.background_image_config(), true)
            .err();
        if let Some(error) = &bg_image_error {
            log::warn!("{error}");
        }
        let bar = tab_bar::Layout::default();
        let chrome_cell = painter.chrome_metrics().cell;
        let top_bar_height = titlebar::Layout::height(
            window.inner_size(),
            chrome_cell,
            scale,
            config.mode == TerminalMode::Modern,
        );
        let left_bar_width = titlebar::Layout::compute_sidebar_width(
            window.inner_size(),
            chrome_cell,
            scale,
            config.mode == TerminalMode::Modern,
            config.tab_position,
            false,
            None,
        );
        let padding = physical_padding(config.window_padding, scale);
        let content_area = content_rect(
            window.inner_size(),
            left_bar_width,
            top_bar_height,
            0,
            padding,
            painter.metrics().cell,
        )
        .ok_or_else(|| io::Error::other("The terminal grid does not fit the Wayland window"))?;
        let grid = GridGeometry::fit(content_area.size, painter.metrics().cell)
            .ok_or_else(|| io::Error::other("The terminal grid does not fit the Wayland window"))?;
        let (palette, scheme) = config.theme.palette(selected_theme);
        let history_limits = config.history_limits();
        let mut pane = Pane::spawn(
            1,
            window.clone(),
            proxy.clone(),
            grid,
            PaneConfig {
                palette: &palette,
                scheme,
                history_limits,
                cursor_style: config.cursor_style,
                cursor_blink: config.cursor_blink,
                shell: config.shell.as_deref(),
                shell_integration: config.shell_integration,
            },
            launch.working_directory,
            launch.command.as_deref(),
        )?;
        let readiness = Readiness::new(proxy.clone())?;
        readiness.arm(pane.id, &mut pane.registration, pane.session.interests())?;
        pane.origin = content_area.origin;
        pane.visible = true;
        pane.active = true;
        let tab = Tab::new(pane, content_area);
        window.set_ime_purpose(ImePurpose::Terminal);
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
        let config_dir = custom_config
            .as_deref()
            .and_then(|path| path.parent().map(PathBuf::from))
            .or_else(Config::directory);
        let mut client = Self {
            window,
            painter,
            readiness,
            proxy,
            tabs: vec![tab],
            active: 0,
            next_pane: 2,
            palette,
            scheme,
            theme_preference: config.theme.clone(),
            system_theme,
            config_dir,
            bar,
            titlebar: titlebar::Layout::default(),
            titlebar_hovered: None,
            titlebar_pressed: None,
            titlebar_last_click: None,
            tab_animator: TabAnimator::default(),
            tab_drag: None,
            sidebar_collapsed: false,
            sidebar_width_cells: None,
            sidebar_resize_drag: None,
            sidebar_divider_last_click: None,
            content_area,
            copy_pending: false,
            directory_open_pending: false,
            hyperlink_open_pending: false,
            primary_selection: None,
            clipboard_write: config.clipboard_write,
            clipboard_read: config.clipboard_read,
            notifications_policy: config.notifications,
            notifier,
            notice: startup_notice
                .map(|text| Notice {
                    kind: NoticeKind::Error,
                    text,
                    expires: Instant::now() + TOAST_ERROR_DURATION,
                })
                .or_else(|| {
                    bg_image_error.map(|text| Notice {
                        kind: NoticeKind::Warning,
                        text,
                        expires: Instant::now() + TOAST_WARNING_DURATION,
                    })
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
            default_title,
            custom_app_id,
            custom_config,
            keymap: Keymap::from_overrides(&config.keybinds),
            unfocused_split_opacity: config.unfocused_split_opacity,
            window_padding: config.window_padding,
            history_limits,
            cursor_style: config.cursor_style,
            cursor_blink: config.cursor_blink,
            shell: config.shell.clone(),
            shell_integration: config.shell_integration,
            mode: config.mode,
            tab_position: config.tab_position,
            window_controls: config.window_controls,
            compositor_decorations,
            portal_buttons,
            scrollbar: config.scrollbar,
            confirm_close: config.confirm_close,
            confirm_paste: config.confirm_paste,
            copy_on_select: config.copy_on_select,
            confirm: None,
            command_palette: None,
        };
        client.reflow_tab_bar();
        Ok(client)
    }

    fn pane_config(&self) -> PaneConfig<'_> {
        PaneConfig {
            palette: &self.palette,
            scheme: self.scheme,
            history_limits: self.history_limits,
            cursor_style: self.cursor_style,
            cursor_blink: self.cursor_blink,
            shell: self.shell.as_deref(),
            shell_integration: self.shell_integration,
        }
    }

    fn physical_padding(&self) -> u32 {
        physical_padding(self.window_padding, self.window.scale_factor())
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
            WindowEvent::CloseRequested => {
                if self.confirm.is_none() && self.confirm_window_close() {
                    log::debug!("Terminal window closed");
                    event_loop.exit();
                }
            }
            WindowEvent::Destroyed => {
                log::debug!("Terminal window destroyed");
                event_loop.exit();
            }
            WindowEvent::DroppedFile(path) => {
                if self.confirm.is_some() {
                    return Ok(());
                }
                if let Some(point) = self.pointer
                    && let Some(target_id) = self.tabs[self.active].layout.pane_at(point)
                    && target_id != self.tabs[self.active].focused
                {
                    self.focus_pane(target_id);
                }
                self.tabs[self.active].focused_pane_mut().drop_path(&path);
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
                let pane = self.tabs[self.active].focused_pane_mut();
                pane.focus(focused);
                pane.invalidate_ime_area();
                self.window
                    .set_ime_allowed(focused && self.confirm.is_none() && pane.allows_ime());
                if focused {
                    pane.update_ime_area();
                } else {
                    self.dragging_divider = None;
                    self.titlebar_pressed = None;
                    self.sidebar_resize_drag = None;
                    if self.tab_drag.take().is_some() {
                        self.sync_tab_animator();
                    }
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
                if self.confirm.is_some() {
                    if event.state.is_pressed() {
                        self.handle_confirm_key(event_loop, &event);
                    }
                    return Ok(());
                }
                if self.command_palette.is_some() {
                    if event.state.is_pressed() {
                        self.handle_palette_key(event_loop, &event);
                    }
                    return Ok(());
                }
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
                pane.invalidate_ime_area();
                pane.update_ime_area();
            }
            WindowEvent::Ime(Ime::Disabled) => {
                self.ime_enabled = false;
                let pane = self.tabs[self.active].focused_pane_mut();
                pane.ime_enabled = false;
                pane.invalidate_ime_area();
                pane.clear_preedit();
            }
            WindowEvent::Ime(ime) => {
                if self.confirm.is_some() {
                    return Ok(());
                }
                if let Some(cmd_palette) = &mut self.command_palette {
                    match ime {
                        Ime::Preedit(text, _) => {
                            if cmd_palette.preedit != text {
                                cmd_palette.preedit = text;
                                self.painter.invalidate_overlay();
                                self.window.request_redraw();
                            }
                        }
                        Ime::Commit(text) => {
                            cmd_palette.preedit.clear();
                            if cmd_palette.push_text(&text) {
                                self.sync_palette_theme_preview();
                            }
                            self.painter.invalidate_overlay();
                            self.window.request_redraw();
                        }
                        _ => {}
                    }
                } else {
                    self.tabs[self.active].focused_pane_mut().ime(ime);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer = Some(position);
                if self.confirm.is_some() {
                    let hovered = self
                        .confirm_layout()
                        .and_then(|layout| layout.hit(position));
                    if let Some(dialog) = &mut self.confirm
                        && dialog.hovered != hovered
                    {
                        dialog.hovered = hovered;
                        self.painter.invalidate_overlay();
                        self.window.request_redraw();
                    }
                    self.update_cursor_icon();
                    return Ok(());
                }
                if self.command_palette.is_some() {
                    let hovered = self
                        .palette_layout()
                        .and_then(|(layout, shown)| layout.hit_row(position, shown));
                    if let Some(cmd_palette) = &mut self.command_palette
                        && cmd_palette.hovered != hovered
                    {
                        cmd_palette.hovered = hovered;
                        self.painter.invalidate_overlay();
                        self.window.request_redraw();
                    }
                    self.update_cursor_icon();
                    return Ok(());
                }
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
                if let Some(drag) = self.sidebar_resize_drag {
                    let cell_w = f64::from(self.painter.chrome_metrics().cell.width.get().max(1));
                    let m = self.titlebar.metrics;
                    let next_px = (drag.start_width_px + (position.x - drag.start_x)).clamp(
                        f64::from(m.sidebar_min_width),
                        f64::from(m.sidebar_max_width),
                    );
                    let next_cells = next_px / cell_w;
                    let prev_px = f64::from(self.titlebar.sidebar_width);
                    if (next_px - prev_px).abs() >= 1.0 {
                        self.sidebar_width_cells = Some(next_cells);
                        self.resize_pending = true;
                        self.apply_resize();
                    }
                    self.update_cursor_icon();
                    return Ok(());
                }
                if let Some(mut drag) = self.tab_drag {
                    let dist = ((position.x - drag.start_pos.x).powi(2)
                        + (position.y - drag.start_pos.y).powi(2))
                    .sqrt();
                    if dist >= 4.0 {
                        drag.moved = true;
                    }
                    if drag.moved {
                        if drag.in_sidebar {
                            let raw_y = position.y as f32 - drag.grab_offset;
                            drag.current_coord = self.titlebar.clamp_sidebar_drag_y(raw_y);
                            if let Some(old_index) =
                                self.tabs.iter().position(|tab| tab.id == drag.tab_id)
                            {
                                let new_index = self
                                    .titlebar
                                    .insertion_index_sidebar(drag.current_coord, self.tabs.len());
                                if old_index != new_index {
                                    let tab = self.tabs.remove(old_index);
                                    self.tabs.insert(new_index, tab);
                                    self.active = new_index;
                                    self.reflow_tab_bar_only();
                                }
                            }
                        } else {
                            let raw_x = position.x as f32 - drag.grab_offset;
                            drag.current_coord = self.titlebar.clamp_top_drag_x(raw_x);
                            if let Some(old_index) =
                                self.tabs.iter().position(|tab| tab.id == drag.tab_id)
                            {
                                let new_index = self
                                    .titlebar
                                    .insertion_index_top(drag.current_coord, self.tabs.len());
                                if old_index != new_index {
                                    let tab = self.tabs.remove(old_index);
                                    self.tabs.insert(new_index, tab);
                                    self.active = new_index;
                                    self.reflow_tab_bar_only();
                                }
                            }
                        }
                        self.tab_drag = Some(drag);
                        self.sync_tab_animator();
                        self.painter.invalidate_overlay();
                        self.window.request_redraw();
                    } else {
                        self.tab_drag = Some(drag);
                    }
                    self.update_cursor_icon();
                    return Ok(());
                }
                let hovered_titlebar = self.titlebar.hit(position);
                if self.titlebar_hovered != hovered_titlebar {
                    self.titlebar_hovered = hovered_titlebar;
                    self.painter.invalidate_overlay();
                    self.window.request_redraw();
                }
                let scrollbar = self.scrollbar;
                let tab = &mut self.tabs[self.active];
                let captured = tab
                    .panes
                    .iter()
                    .find(|pane| {
                        pane.selection_drag.is_some()
                            || pane.scrollbar_drag.is_some()
                            || pane.pressed.is_some()
                    })
                    .map(|pane| pane.id);
                if let Some(id) = captured {
                    if let Some(pane) = tab.pane_mut(id) {
                        pane.pointer_moved(position, scrollbar);
                    }
                } else {
                    let hovered_id = (!self.bar.contains(position)
                        && !self.titlebar.contains_any(position))
                    .then(|| tab.layout.pane_at(position))
                    .flatten();
                    for pane in &mut tab.panes {
                        if Some(pane.id) == hovered_id {
                            pane.modifiers = self.modifiers;
                            pane.pointer_moved(position, scrollbar);
                        } else if pane.pointer.is_some() {
                            pane.pointer = None;
                            pane.update_scrollbar_hover(scrollbar);
                            pane.update_hovered_hyperlink();
                        }
                    }
                }
                self.update_cursor_icon();
            }
            WindowEvent::CursorLeft { .. } => {
                self.pointer = None;
                if let Some(dialog) = &mut self.confirm
                    && dialog.hovered.take().is_some()
                {
                    self.painter.invalidate_overlay();
                    self.window.request_redraw();
                }
                if let Some(cmd_palette) = &mut self.command_palette
                    && cmd_palette.hovered.take().is_some()
                {
                    self.painter.invalidate_overlay();
                    self.window.request_redraw();
                }
                if self.titlebar_hovered.take().is_some() {
                    self.painter.invalidate_overlay();
                    self.window.request_redraw();
                }
                let scrollbar = self.scrollbar;
                for pane in &mut self.tabs[self.active].panes {
                    if pane.pressed.is_none()
                        && pane.selection_drag.is_none()
                        && pane.scrollbar_drag.is_none()
                    {
                        pane.pointer = None;
                    }
                    pane.update_scrollbar_hover(scrollbar);
                    pane.update_hovered_hyperlink();
                }
                self.update_cursor_icon();
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if self.confirm.is_some() {
                    if state.is_pressed()
                        && button == winit::event::MouseButton::Left
                        && let Some(point) = self.pointer
                        && let Some(hit) =
                            self.confirm_layout().and_then(|layout| layout.hit(point))
                    {
                        match hit {
                            confirm::Button::Yes => {
                                if let Some(dialog) = self.confirm.take() {
                                    self.execute_confirm(event_loop, dialog.target);
                                }
                            }
                            confirm::Button::No => self.dismiss_confirm(),
                        }
                    }
                    return Ok(());
                }
                if self.command_palette.is_some() {
                    if state.is_pressed()
                        && button == winit::event::MouseButton::Left
                        && let Some(point) = self.pointer
                    {
                        if let Some((layout, shown)) = self.palette_layout() {
                            if let Some(row) = layout.hit_row(point, shown) {
                                let action = self.command_palette.as_mut().and_then(|palette| {
                                    palette.select_visible(row);
                                    palette.entry_at_visible(row).map(|e| e.action.clone())
                                });
                                if let Some(action) = action {
                                    self.execute_palette_action(event_loop, action);
                                }
                            } else if !layout.contains_card(point) {
                                self.close_palette(true);
                            }
                        } else {
                            self.close_palette(true);
                        }
                    }
                    return Ok(());
                }
                if !state.is_pressed()
                    && button == winit::event::MouseButton::Left
                    && self.dragging_divider.take().is_some()
                {
                    self.update_cursor_icon();
                    return Ok(());
                }
                if !state.is_pressed()
                    && button == winit::event::MouseButton::Left
                    && self.sidebar_resize_drag.take().is_some()
                {
                    self.update_cursor_icon();
                    return Ok(());
                }
                if !state.is_pressed()
                    && button == winit::event::MouseButton::Left
                    && let Some(drag) = self.tab_drag.take()
                {
                    if drag.moved {
                        self.sync_tab_animator();
                        self.painter.invalidate_overlay();
                        self.window.request_redraw();
                    }
                    self.update_cursor_icon();
                    return Ok(());
                }
                if !state.is_pressed()
                    && button == winit::event::MouseButton::Left
                    && let Some(pressed) = self.titlebar_pressed.take()
                {
                    if let Some(point) = self.pointer
                        && self.titlebar.hit(point) == Some(pressed)
                    {
                        self.execute_titlebar_action(event_loop, pressed);
                    }
                    self.update_cursor_icon();
                    return Ok(());
                }
                if state.is_pressed()
                    && button == winit::event::MouseButton::Left
                    && let Some(point) = self.pointer
                    && self
                        .titlebar
                        .hit_sidebar_divider(point, self.window.scale_factor())
                {
                    let now = Instant::now();
                    let double_click = self
                        .sidebar_divider_last_click
                        .is_some_and(|prev| now.duration_since(prev) <= Duration::from_millis(350));
                    if double_click {
                        self.sidebar_divider_last_click = None;
                        self.sidebar_width_cells = None;
                        self.resize_pending = true;
                        self.apply_resize();
                    } else {
                        self.sidebar_divider_last_click = Some(now);
                        self.sidebar_resize_drag = Some(SidebarResizeDrag {
                            start_x: point.x,
                            start_width_px: f64::from(self.titlebar.sidebar_width),
                        });
                    }
                    self.update_cursor_icon();
                    return Ok(());
                }
                if state.is_pressed()
                    && button == winit::event::MouseButton::Left
                    && let Some(point) = self.pointer
                    && self.titlebar.hit(point).is_none()
                    && let Some(dir) = self.borderless_resize_direction(point)
                {
                    let _ = self.window.drag_resize_window(dir);
                    return Ok(());
                }
                if state.is_pressed()
                    && let Some(point) = self.pointer
                    && self.titlebar.contains_any(point)
                {
                    let hit = self.titlebar.hit(point);
                    match button {
                        winit::event::MouseButton::Left => match hit {
                            Some(TitlebarAction::SelectTab(index)) => {
                                self.titlebar_last_click = None;
                                if index < self.tabs.len() {
                                    self.select_tab(index);
                                    if self.tabs.len() > 1 {
                                        let in_sidebar = self.titlebar.sidebar_width > 0;
                                        let slot = if in_sidebar {
                                            self.titlebar.sidebar_tab_slot(index)
                                        } else {
                                            self.titlebar.top_tab_slot(index)
                                        };
                                        if let Some(slot) = slot {
                                            let base = if in_sidebar {
                                                slot.rect.origin.y as f32
                                            } else {
                                                slot.rect.origin.x as f32
                                            };
                                            let current = self.tab_animator.coord(slot.id, base);
                                            let pointer_axis = if in_sidebar {
                                                point.y as f32
                                            } else {
                                                point.x as f32
                                            };
                                            self.tab_drag = Some(TabDrag {
                                                tab_id: slot.id,
                                                grab_offset: pointer_axis - current,
                                                start_pos: point,
                                                current_coord: current,
                                                moved: false,
                                                in_sidebar,
                                            });
                                        }
                                    }
                                }
                            }
                            Some(action) => {
                                self.titlebar_last_click = None;
                                self.titlebar_pressed = Some(action);
                            }
                            None if self.titlebar.contains_titlebar(point) => {
                                let now = Instant::now();
                                let double_click = self.titlebar_last_click.is_some_and(|prev| {
                                    now.duration_since(prev) <= Duration::from_millis(350)
                                });
                                if double_click {
                                    self.titlebar_last_click = None;
                                    self.window.set_maximized(!self.window.is_maximized());
                                } else {
                                    self.titlebar_last_click = Some(now);
                                    let _ = self.window.drag_window();
                                }
                            }
                            None => {}
                        },
                        winit::event::MouseButton::Middle => {
                            if let Some(
                                TitlebarAction::SelectTab(index) | TitlebarAction::CloseTab(index),
                            ) = hit
                                && index < self.tabs.len()
                            {
                                self.request_close_tab(event_loop, index);
                            }
                        }
                        _ => {}
                    }
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
                            winit::event::MouseButton::Middle => {
                                self.request_close_tab(event_loop, index);
                            }
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
                    let copied_notice = if let Some(point) = self.pointer
                        && let Some(target_id) = self.tabs[self.active].layout.pane_at(point)
                    {
                        if target_id != self.tabs[self.active].focused {
                            self.focus_pane(target_id);
                        }
                        self.tabs[self.active].pane_mut(target_id).and_then(|pane| {
                            pane.pointer = Some(point);
                            pane.modifiers = self.modifiers;
                            pane.button(
                                state,
                                button,
                                &mut self.primary_selection,
                                &mut self.hyperlink_open_pending,
                                &mut self.copy_pending,
                                self.copy_on_select,
                                self.scrollbar,
                            )
                        })
                    } else {
                        self.tabs[self.active].focused_pane_mut().button(
                            state,
                            button,
                            &mut self.primary_selection,
                            &mut self.hyperlink_open_pending,
                            &mut self.copy_pending,
                            self.copy_on_select,
                            self.scrollbar,
                        )
                    };
                    if let Some(text) = copied_notice {
                        self.show_notice(NoticeKind::Success, text);
                    }
                    self.update_cursor_icon();
                } else {
                    let target_id = self.tabs[self.active]
                        .panes
                        .iter()
                        .find(|pane| {
                            pane.selection_drag.is_some()
                                || pane.scrollbar_drag.is_some()
                                || pane.pressed.is_some()
                        })
                        .map(|pane| pane.id)
                        .unwrap_or(self.tabs[self.active].focused);
                    let copied_notice =
                        self.tabs[self.active].pane_mut(target_id).and_then(|pane| {
                            pane.button(
                                state,
                                button,
                                &mut self.primary_selection,
                                &mut self.hyperlink_open_pending,
                                &mut self.copy_pending,
                                self.copy_on_select,
                                self.scrollbar,
                            )
                        });
                    if let Some(text) = copied_notice {
                        self.show_notice(NoticeKind::Success, text);
                    }
                    self.update_cursor_icon();
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if self.confirm.is_some() {
                    return Ok(());
                }
                if let Some(cmd_palette) = &mut self.command_palette {
                    let y = match delta {
                        winit::event::MouseScrollDelta::LineDelta(_, y) => f64::from(y),
                        winit::event::MouseScrollDelta::PixelDelta(pos) => pos.y / 20.0,
                    };
                    let step = if y > 0.0 {
                        -1
                    } else if y < 0.0 {
                        1
                    } else {
                        0
                    };
                    if step != 0 && cmd_palette.scroll_list(step) {
                        self.painter.invalidate_overlay();
                        self.window.request_redraw();
                    }
                    return Ok(());
                }
                if self
                    .pointer
                    .is_some_and(|point| self.titlebar.contains_sidebar(point))
                {
                    let (x, y) = match delta {
                        winit::event::MouseScrollDelta::LineDelta(x, y) => {
                            (f64::from(x), f64::from(y))
                        }
                        winit::event::MouseScrollDelta::PixelDelta(position) => {
                            (position.x, position.y)
                        }
                    };
                    let direction = if y != 0.0 { y } else { x };
                    if self.tabs.len() > self.titlebar.sidebar_visible_rows.max(1) {
                        let step = if direction > 0.0 {
                            -1
                        } else if direction < 0.0 {
                            1
                        } else {
                            0
                        };
                        if step != 0 && self.titlebar.scroll_sidebar(step, self.tabs.len()) {
                            self.reflow_tab_bar_only();
                            self.sync_tab_animator();
                            self.painter.invalidate_overlay();
                            self.window.request_redraw();
                        }
                    } else if self.tabs.len() > 1 {
                        if direction > 0.0 {
                            self.select_tab(
                                self.active.checked_sub(1).unwrap_or(self.tabs.len() - 1),
                            );
                        } else if direction < 0.0 {
                            self.select_tab((self.active + 1) % self.tabs.len());
                        }
                    }
                } else if self.pointer.is_some_and(|point| {
                    self.bar.contains(point) || self.titlebar.contains_titlebar(point)
                }) {
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
        let next = if let Some(dialog) = &self.confirm {
            if dialog.hovered.is_some() {
                CursorIcon::Pointer
            } else {
                CursorIcon::Default
            }
        } else if let Some(cmd_palette) = &self.command_palette {
            if cmd_palette.hovered.is_some() {
                CursorIcon::Pointer
            } else {
                CursorIcon::Default
            }
        } else if self.sidebar_resize_drag.is_some() {
            CursorIcon::ColResize
        } else if let Some(divider) = self.dragging_divider {
            divider_cursor(divider.axis)
        } else if let Some(point) = self.pointer {
            if self
                .titlebar
                .hit_sidebar_divider(point, self.window.scale_factor())
            {
                CursorIcon::ColResize
            } else if self.titlebar.hit(point).is_some() {
                CursorIcon::Pointer
            } else if let Some(dir) = self.borderless_resize_direction(point) {
                resize_cursor(dir)
            } else if self.bar.contains(point) || self.titlebar.contains_any(point) {
                CursorIcon::Default
            } else {
                let tab = &self.tabs[self.active];
                let busy = tab.panes.iter().any(|pane| {
                    pane.selection_drag.is_some()
                        || pane.scrollbar_drag.is_some()
                        || pane.pressed.is_some()
                });
                if !busy
                    && let Some(divider) = tab.layout.divider_at(point, self.divider_grab_margin())
                {
                    divider_cursor(divider.axis)
                } else if tab
                    .panes
                    .iter()
                    .any(|pane| pane.visible && pane.scrollbar_hovered)
                {
                    CursorIcon::Default
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
        let pane = self.tabs[self.active].focused_pane();
        let buffer = pane.session.screen().active_buffer();
        if pane.hints.is_some() {
            if let Some(binding) = self.keymap.binding(event, self.modifiers, buffer)
                && let Some(Action::Hints) = binding.action(event.repeat)
            {
                self.tabs[self.active].focused_pane_mut().close_hints();
                return;
            }
            let notice = self.tabs[self.active].focused_pane_mut().hints_key(
                event,
                &mut self.copy_pending,
                &mut self.primary_selection,
                &mut self.hyperlink_open_pending,
            );
            if let Some(text) = notice {
                self.show_notice(NoticeKind::Success, text);
            }
            return;
        }
        if pane.search.is_some() {
            if let Some(binding) = self.keymap.binding(event, self.modifiers, buffer)
                && let Some(action) = binding.action(event.repeat)
            {
                match action {
                    Action::Search => {
                        self.tabs[self.active].focused_pane_mut().close_search();
                        return;
                    }
                    Action::Paste => {
                        self.tabs[self.active]
                            .focused_pane_mut()
                            .paste(ClipboardType::Regular, self.primary_selection.as_ref());
                        return;
                    }
                    _ => {}
                }
            }
            self.tabs[self.active].focused_pane_mut().search_key(event);
            return;
        }
        if let Some(binding) = self.keymap.binding(event, self.modifiers, buffer) {
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
                    if let Some(text) = self.tabs[self.active]
                        .focused_pane_mut()
                        .key(event, &mut self.copy_pending)
                    {
                        self.show_notice(NoticeKind::Success, text);
                    }
                } else {
                    self.run_shortcut(event_loop, action);
                }
            }
        } else if let Some(text) = self.tabs[self.active]
            .focused_pane_mut()
            .key(event, &mut self.copy_pending)
        {
            self.show_notice(NoticeKind::Success, text);
        }
    }

    fn run_shortcut(&mut self, event_loop: &ActiveEventLoop, action: Action) {
        match action {
            Action::Copy => {
                if let Some(text) = self.tabs[self.active]
                    .focused_pane()
                    .copy(&mut self.copy_pending)
                {
                    self.show_notice(NoticeKind::Success, text);
                }
            }
            Action::Paste => {
                let pane = self.tabs[self.active].focused_pane_mut();
                if pane.vi.is_none() {
                    pane.paste(ClipboardType::Regular, self.primary_selection.as_ref());
                }
            }
            Action::ToggleVi => self.tabs[self.active].focused_pane_mut().toggle_vi(),
            Action::Search => self.tabs[self.active].focused_pane_mut().toggle_search(),
            Action::Hints => {
                if !self.tabs[self.active].focused_pane_mut().toggle_hints() {
                    self.show_notice(NoticeKind::Warning, "No hints found in view".into());
                }
            }
            Action::CommandPalette => self.open_palette(),
            Action::ToggleMode => self.set_mode(self.mode.toggle()),
            Action::ToggleTabSidebar => self.toggle_tab_sidebar(),
            Action::PreviousPrompt => self.tabs[self.active].focused_pane_mut().jump_prompt(false),
            Action::NextPrompt => self.tabs[self.active].focused_pane_mut().jump_prompt(true),
            Action::CopyLastOutput => {
                if let Some(text) = self.tabs[self.active]
                    .focused_pane()
                    .copy_last_output(&mut self.copy_pending)
                {
                    self.show_notice(NoticeKind::Success, text);
                }
            }
            Action::ReloadConfig => {
                let _ = self.proxy.send_event(Event::ConfigReload(true));
            }
            Action::NewWindow => self.spawn_new_window(),
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
                self.request_close_pane(event_loop, self.active, focused);
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
        let grid = GridGeometry::fit(area.size, metrics.cell)
            .unwrap_or(self.tabs[self.active].focused_pane().target_grid);
        let mut pane = Pane::spawn(
            id,
            self.window.clone(),
            self.proxy.clone(),
            grid,
            self.pane_config(),
            cwd,
            None,
        )?;
        pane.origin = area.origin;
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

    fn spawn_new_window(&self) {
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

    fn update_window_title(&self) {
        let title = &self.tabs[self.active].focused_pane().title;
        self.window.set_title(if title.is_empty() {
            &self.default_title
        } else {
            title
        });
    }

    fn apply_system_theme(&mut self, theme: Theme) {
        self.system_theme = theme;
        self.apply_theme_preference(self.theme_preference.clone());
    }

    fn apply_theme_preference(&mut self, preference: theme::Preference) {
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

    fn set_mode(&mut self, mode: TerminalMode) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.window.set_decorations(mode == TerminalMode::Classic);
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

    fn set_tab_position(&mut self, position: TabPosition) {
        if self.mode != TerminalMode::Modern {
            self.mode = TerminalMode::Modern;
            self.window.set_decorations(false);
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

    fn toggle_tab_sidebar(&mut self) {
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

    fn execute_titlebar_action(&mut self, event_loop: &ActiveEventLoop, action: TitlebarAction) {
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

    fn borderless_resize_direction(&self, pos: PhysicalPosition<f64>) -> Option<ResizeDirection> {
        if self.mode != TerminalMode::Modern
            || self.window.fullscreen().is_some()
            || self.window.is_maximized()
        {
            return None;
        }
        titlebar::edge_resize_direction(pos, self.painter.size(), self.window.scale_factor())
    }

    fn palette_layout(&self) -> Option<(palette::Layout, usize)> {
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

    fn open_palette(&mut self) {
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

    fn close_palette(&mut self, restore_theme: bool) {
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

    fn sync_palette_theme_preview(&mut self) {
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

    fn handle_palette_key(&mut self, event_loop: &ActiveEventLoop, event: &KeyEvent) {
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

    fn execute_palette_action(&mut self, event_loop: &ActiveEventLoop, action: PaletteAction) {
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

    fn confirm_layout(&self) -> Option<confirm::Layout> {
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

    fn open_confirm(&mut self, target: confirm::ConfirmTarget, message: String) {
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

    fn dismiss_confirm(&mut self) {
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

    fn execute_confirm(&mut self, event_loop: &ActiveEventLoop, target: confirm::ConfirmTarget) {
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

    fn handle_confirm_key(&mut self, event_loop: &ActiveEventLoop, event: &KeyEvent) {
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

    fn confirm_window_close(&mut self) -> bool {
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

    fn request_close_pane(
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

    fn request_close_tab(&mut self, event_loop: &ActiveEventLoop, index: usize) {
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

    fn show_notice(&mut self, kind: NoticeKind, text: String) {
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

    fn set_notice(&mut self, text: Option<String>) {
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

    fn apply_config(&mut self, old: &Config, new: &Config, system_theme: Theme, force: bool) {
        self.system_theme = system_theme;
        if old.theme != new.theme {
            self.apply_theme_preference(new.theme.clone());
        }
        let mut layout_changed = false;
        if old.mode != new.mode {
            self.mode = new.mode;
            self.window
                .set_decorations(self.mode == TerminalMode::Classic);
            self.titlebar_hovered = None;
            self.titlebar_pressed = None;
            self.tab_drag = None;
            self.sidebar_resize_drag = None;
            self.tab_animator.clear();
            layout_changed = true;
        }
        if old.tab_position != new.tab_position {
            self.tab_position = new.tab_position;
            self.sidebar_collapsed = false;
            self.tab_drag = None;
            self.sidebar_resize_drag = None;
            self.tab_animator.clear();
            layout_changed = true;
        }
        if old.window_controls != new.window_controls {
            self.window_controls = new.window_controls;
            layout_changed = true;
        }
        if old.scrollbar != new.scrollbar {
            self.scrollbar = new.scrollbar;
            for tab in &mut self.tabs {
                for pane in &mut tab.panes {
                    pane.update_scrollbar_hover(self.scrollbar);
                }
            }
        }
        if old.background_opacity != new.background_opacity {
            self.painter.set_background_opacity(new.background_opacity);
        }
        if old.unfocused_split_opacity != new.unfocused_split_opacity {
            self.unfocused_split_opacity = new.unfocused_split_opacity;
        }
        if old.window_padding != new.window_padding {
            self.window_padding = new.window_padding;
            layout_changed = true;
        }
        if old.scrollback_lines != new.scrollback_lines {
            self.history_limits = new.history_limits();
            for tab in &mut self.tabs {
                for pane in &mut tab.panes {
                    if let Err(error) = pane.session.set_history_limits(self.history_limits) {
                        log::warn!("Could not update scrollback limit: {error}");
                    }
                }
            }
        }
        if old.cursor_style != new.cursor_style || old.cursor_blink != new.cursor_blink {
            self.cursor_style = new.cursor_style;
            self.cursor_blink = new.cursor_blink;
            for tab in &mut self.tabs {
                for pane in &mut tab.panes {
                    pane.session
                        .set_default_cursor_appearance(new.cursor_style, new.cursor_blink);
                }
            }
            self.painter.set_cursor_blink(new.cursor_blink);
        }
        if old.cursor_animation != new.cursor_animation {
            self.painter.set_cursor_animation(new.cursor_animation);
        }
        if old.keybinds != new.keybinds {
            self.keymap = Keymap::from_overrides(&new.keybinds);
        }
        self.shell = new.shell.clone();
        self.shell_integration = new.shell_integration;
        self.confirm_close = new.confirm_close;
        self.confirm_paste = new.confirm_paste;
        self.copy_on_select = new.copy_on_select;
        if !self.confirm_close
            && !matches!(
                self.confirm.as_ref().map(|dialog| &dialog.target),
                Some(confirm::ConfirmTarget::Paste { .. })
            )
        {
            self.dismiss_confirm();
        }
        if old.font_size_points != new.font_size_points {
            self.base_font_size_points = new.font_size_points;
            self.font_size_points = new.font_size_points;
            layout_changed = true;
        }
        let old_font = old.font_config();
        let new_font = new.font_config();
        if old_font != new_font {
            let scale = self.window.scale_factor();
            let physical_font = physical_font_size(self.font_size_points, scale);
            self.painter.set_font(&new_font, physical_font, scale);
            layout_changed = true;
        }
        if old.minimum_contrast != new.minimum_contrast {
            self.painter.set_minimum_contrast(new.minimum_contrast);
        }
        let old_bg_image = old.background_image_config();
        let new_bg_image = new.background_image_config();
        if (old_bg_image != new_bg_image || (force && new_bg_image.path.is_some()))
            && let Err(error) = self.painter.set_background_image(new_bg_image, force)
        {
            log::warn!("{error}");
            if self.notice.is_none() {
                self.show_notice(NoticeKind::Warning, error);
            }
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
        focused_pane.update_ime_area();
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
        let tabs_animating = self.tab_animator.tick(now);
        if tabs_animating {
            self.painter.invalidate_overlay();
        }
        if self.painter.tick(now) || blink || notice_expired || tabs_animating {
            self.window.request_redraw();
        }
        let tab_deadline = self
            .tab_animator
            .moving()
            .then(|| now + Duration::from_millis(8));
        deadline = deadline
            .into_iter()
            .chain(self.painter.deadline())
            .chain(self.notice.as_ref().map(|notice| notice.expires))
            .chain(tab_deadline)
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
        let scale = self.window.scale_factor();
        let physical_font = physical_font_size(self.font_size_points, scale);
        let current = self.painter.metrics();
        let (metrics, chrome_metrics) = if current.size != physical_font {
            let next = self.painter.measure(physical_font, scale);
            let next_chrome = self.painter.measure_chrome(next);
            (next, next_chrome)
        } else {
            (current, self.painter.chrome_metrics())
        };
        let top_bar_height = titlebar::Layout::height(
            size,
            chrome_metrics.cell,
            scale,
            self.mode == TerminalMode::Modern,
        );
        let left_bar_width = titlebar::Layout::compute_sidebar_width(
            size,
            chrome_metrics.cell,
            scale,
            self.mode == TerminalMode::Modern,
            self.tab_position,
            self.sidebar_collapsed,
            self.sidebar_width_cells,
        );
        let bar_height = if self.mode == TerminalMode::Classic {
            tab_bar::Layout::height(size, metrics.cell, self.tabs.len())
        } else {
            0
        };
        let padding = self.physical_padding();
        let Some(content_area) = content_rect(
            size,
            left_bar_width,
            top_bar_height,
            bar_height,
            padding,
            metrics.cell,
        ) else {
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
        let pane = self.tabs[self.active].focused_pane_mut();
        pane.invalidate_ime_area();
        pane.update_ime_area();
        self.window.request_redraw();
    }

    fn resolved_window_buttons(&self) -> WindowButtons {
        if self.window.fullscreen().is_some() {
            return WindowButtons::NONE;
        }
        resolve_window_buttons(
            self.window_controls,
            self.compositor_decorations.as_ref(),
            self.portal_buttons,
        )
    }

    fn apply_portal_buttons(&mut self, buttons: Option<WindowButtons>) {
        let before = self.resolved_window_buttons();
        self.portal_buttons = buttons;
        if self.resolved_window_buttons() != before {
            self.resize_pending = true;
            self.apply_resize();
            self.painter.invalidate_overlay();
            self.window.request_redraw();
        }
    }

    fn reflow_tab_bar_only(&mut self) {
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

    fn sync_tab_animator(&mut self) {
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

    fn reflow_tab_bar(&mut self) {
        self.reflow_tab_bar_only();
        self.sync_tab_animator();
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
        let scale = self.window.scale_factor();
        let active_tab = &self.tabs[self.active];
        let zoomed = active_tab.is_zoomed();
        let multi_pane = active_tab.layout.panes.len() > 1;
        let unfocused_dim = (1.0 - self.unfocused_split_opacity).clamp(0.0, 0.85);
        let mut draw_panes = Vec::with_capacity(active_tab.layout.panes.len());
        let mut reverse_video = false;
        for layout_pane in &active_tab.layout.panes {
            let Some(pane) = active_tab.pane(layout_pane.id) else {
                continue;
            };
            let Some(mut frame) = pane.session.presentation_frame() else {
                return Ok(());
            };
            let focused = self.focused
                && self.confirm.is_none()
                && self.command_palette.is_none()
                && pane.id == active_tab.focused;
            if pane.vi.is_some() || pane.search.is_some() || pane.hints.is_some() || !focused {
                frame.cursor_appearance.visible = false;
            }
            if pane.id == active_tab.focused {
                reverse_video = frame.reverse_video;
            }
            let dim_alpha = if multi_pane && pane.id != active_tab.focused {
                unfocused_dim
            } else {
                0.0
            };
            draw_panes.push(paint::PaneDraw {
                id: pane.id,
                rect: layout_pane.rect,
                frame,
                selection: pane.selection.as_ref(),
                hovered_link: pane.hovered_link.as_ref(),
                progress: pane.progress,
                preedit: if pane.id == active_tab.focused && !pane.preedit.is_empty() {
                    Some(&pane.preedit)
                } else {
                    None
                },
                vi: pane.vi.as_ref(),
                search: pane.search.as_ref(),
                hints: pane.hints.as_ref(),
                focused,
                zoomed: zoomed && pane.id == active_tab.focused,
                scrollbar: pane.scrollbar_view(scale, self.scrollbar),
                dim_alpha,
            });
        }
        let focused_rect = (active_tab.layout.panes.len() > 1)
            .then(|| active_tab.layout.rect_of(active_tab.focused))
            .flatten();
        let palette_shortcut = self
            .keymap
            .shortcut_label(Action::CommandPalette)
            .unwrap_or_default();
        let new_tab_shortcut = self
            .keymap
            .shortcut_label(Action::NewTab)
            .unwrap_or_default();
        self.painter.draw(
            &self.window,
            &draw_panes,
            paint::WindowChrome {
                palette: &self.palette,
                reverse_video,
                dividers: &active_tab.layout.dividers,
                focused_rect,
                notice: self.notice.as_ref().map(|notice| NoticeView {
                    kind: notice.kind,
                    text: notice.text.as_str(),
                }),
                confirm: self.confirm.as_ref(),
                command_palette: self.command_palette.as_ref(),
                scale,
                titlebar: (self.titlebar.height != 0).then(|| titlebar::View {
                    layout: &self.titlebar,
                    animator: &self.tab_animator,
                    dragged_tab: self
                        .tab_drag
                        .filter(|drag| drag.moved)
                        .map(|drag| drag.tab_id),
                    tabs: &self.tabs,
                    active: self.active,
                    palette: &self.palette,
                    hovered: self.titlebar_hovered,
                    maximized: self.window.is_maximized(),
                    background_opacity: self.painter.background_opacity(),
                    palette_shortcut: &palette_shortcut,
                    new_tab_shortcut: &new_tab_shortcut,
                    scale,
                }),
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

fn physical_padding(logical: u32, scale: f64) -> u32 {
    (f64::from(logical) * scale).round().max(0.0) as u32
}

fn content_rect(
    size: PhysicalSize<u32>,
    left_bar_width: u32,
    top_bar_height: u32,
    bottom_bar_height: u32,
    padding: u32,
    cell: PixelSize,
) -> Option<PixelRect> {
    let available_width = size.width.saturating_sub(left_bar_width);
    let available_height = size
        .height
        .saturating_sub(top_bar_height)
        .saturating_sub(bottom_bar_height);
    if padding != 0 {
        let double = padding.saturating_mul(2);
        if let (Some(width), Some(height)) = (
            NonZeroU32::new(available_width.saturating_sub(double)),
            NonZeroU32::new(available_height.saturating_sub(double)),
        ) {
            let padded = PixelRect {
                origin: PixelPosition {
                    x: left_bar_width.saturating_add(padding),
                    y: top_bar_height.saturating_add(padding),
                },
                size: PixelSize { width, height },
            };
            if GridGeometry::fit(padded.size, cell).is_some() {
                return Some(padded);
            }
        }
    }
    Some(PixelRect {
        origin: PixelPosition {
            x: left_bar_width,
            y: top_bar_height,
        },
        size: PixelSize {
            width: NonZeroU32::new(available_width)?,
            height: NonZeroU32::new(available_height)?,
        },
    })
}

fn resize_cursor(direction: ResizeDirection) -> CursorIcon {
    match direction {
        ResizeDirection::North => CursorIcon::NResize,
        ResizeDirection::South => CursorIcon::SResize,
        ResizeDirection::East => CursorIcon::EResize,
        ResizeDirection::West => CursorIcon::WResize,
        ResizeDirection::NorthEast => CursorIcon::NeResize,
        ResizeDirection::NorthWest => CursorIcon::NwResize,
        ResizeDirection::SouthEast => CursorIcon::SeResize,
        ResizeDirection::SouthWest => CursorIcon::SwResize,
    }
}

fn divider_cursor(axis: Axis) -> CursorIcon {
    match axis {
        Axis::Columns => CursorIcon::ColResize,
        Axis::Rows => CursorIcon::RowResize,
    }
}
