mod app_menu;
pub mod bench {
    use std::ops::Range;

    use lune_terminal::{ScreenBuffer, ScreenView};

    use super::{blocks, search};

    pub fn search_scrollback(
        view: ScreenView<'_>,
        columns: usize,
        viewport: Range<usize>,
        query: &str,
        case_sensitive: bool,
        regex_mode: bool,
    ) -> usize {
        let saved = search::SavedSearch {
            query: query.to_owned(),
            direction: super::vi::Direction::Forward,
            case_sensitive,
            regex_mode,
        };
        let bar =
            search::SearchBar::open(Some(saved), ScreenBuffer::Primary, view, columns, viewport);
        bar.matches.len()
    }

    pub fn discover_viewport_blocks(view: ScreenView<'_>, visible_rows: Range<usize>) -> usize {
        blocks::blocks_in_viewport(view, visible_rows).len()
    }
}
mod bg_image;
mod blocks;
mod box_drawing;
pub mod cli;
mod client_modals;
mod client_workspace;
mod clipboard;
mod config;
mod confirm;
mod context_menu;
mod contrast;
mod cursor;
mod directory;
mod font;
mod hints;
mod ime;
mod input;
pub mod ipc;
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
mod wayland_blur;
mod wayland_decorations;

use std::ffi::OsString;
use std::io;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lune_terminal::{
    ClipboardQuery, ColorScheme, CursorShape, HistoryLimits, Palette, PixelPosition, PixelSize,
};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{Ime, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::platform::wayland::{EventLoopBuilderExtWayland, WindowAttributesExtWayland};
use winit::window::{CursorIcon, Fullscreen, ImePurpose, ResizeDirection, Theme, Window, WindowId};

use crate::{GridGeometry, PixelRect, SessionReady};

use cli::LaunchOptions;
use clipboard::ClipboardType;
use config::{
    ClipboardPolicy, Config, ConfigSource, MAX_FONT_SIZE, MIN_FONT_SIZE, NotificationPolicy,
    TabPosition, TerminalMode, WindowControlsPolicy,
};
use context_menu::ContextMenu;
use font::{Font, physical_font_size};
use notify::Notifier;
use paint::{NoticeKind, NoticeView, Painter, PainterOptions};
use palette::CommandPalette;
use readiness::Readiness;
use scrollbar::ScrollbarPolicy;
use shortcuts::{Action, Keymap, Scroll, Zoom};
use split::{Axis, Divider};
use tab::{Pane, PaneConfig, Tab};
use titlebar::{SidebarResizeDrag, TabAnimator, TabDrag, TitlebarAction};
use wayland_decorations::{CompositorDecorations, WindowButtons, probe_wayland_decorations};

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
    ConfigSave,
    IpcCommand(ipc::IpcCommand),
    AppMenuAction(Action),
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

    fn save_config(&mut self) {
        let Some(client) = &mut self.client else {
            return;
        };
        let settings = client.saved_settings();
        match self.config_source.save(&settings) {
            Ok((path, changed)) => {
                client.base_font_size_points = client.font_size_points;
                if let Ok(loaded) = Config::load_checked(&self.config_source) {
                    self.config = loaded.config;
                    self.sync_theme_watch();
                }
                let notice = if changed == 0 {
                    "Configuration already up to date".to_owned()
                } else {
                    let name = path
                        .file_name()
                        .and_then(|file| file.to_str())
                        .unwrap_or("lune.conf");
                    format!("Saved settings to {name}")
                };
                if let Some(client) = &mut self.client {
                    client.show_notice(NoticeKind::Success, notice);
                }
                log::info!("Saved configuration to {}", path.display());
            }
            Err(error) => {
                log::warn!("Could not save configuration: {error}");
                if let Some(client) = &mut self.client {
                    client
                        .show_notice(NoticeKind::Error, format!("Could not save config: {error}"));
                }
            }
        }
    }

    fn handle_ipc_command(&mut self, command: ipc::IpcCommand) {
        match command {
            ipc::IpcCommand::ReloadConfig => self.reload_config(true),
            ipc::IpcCommand::SaveConfig => self.save_config(),
            ipc::IpcCommand::NewTab {
                working_directory,
                title,
                command,
            } => {
                if let Some(client) = &mut self.client {
                    let cwd = working_directory.filter(|dir| dir.is_dir()).or_else(|| {
                        client.tabs[client.active]
                            .focused_pane()
                            .working_directory()
                    });
                    if let Err(error) = client.new_tab_with_options(cwd, title, command.as_deref())
                    {
                        log::warn!("IPC new-tab failed: {error}");
                    }
                }
            }
            ipc::IpcCommand::Split {
                axis,
                working_directory,
            } => {
                if let Some(client) = &mut self.client {
                    let cwd = working_directory.filter(|dir| dir.is_dir()).or_else(|| {
                        client.tabs[client.active]
                            .focused_pane()
                            .working_directory()
                    });
                    if let Err(error) = client.split_pane_with_cwd(axis, cwd) {
                        log::warn!("IPC split failed: {error}");
                    }
                }
            }
            ipc::IpcCommand::SetTheme(name) => {
                if let Some(client) = &mut self.client {
                    let dir = client
                        .config_dir
                        .as_deref()
                        .unwrap_or(std::path::Path::new(""));
                    let mut errors = Vec::new();
                    match theme::Preference::load(dir, &name, &mut errors) {
                        Ok(pref) => {
                            client.apply_theme_preference(pref);
                            client.show_notice(NoticeKind::Success, format!("Theme: {name}"));
                        }
                        Err(error) => {
                            client.show_notice(NoticeKind::Warning, error.into());
                        }
                    }
                }
            }
        }
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
        if let Event::ConfigSave = event {
            self.save_config();
            return;
        }
        if let Event::IpcCommand(command) = event {
            self.handle_ipc_command(command);
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
                let broadcast_result = result.as_ref().ok().cloned();
                let broadcast = client.tabs[client.active].broadcast_input
                    && client.tabs[client.active].panes.len() > 1
                    && client.tabs[client.active].focused == id;
                let pending_confirm = client
                    .pane_mut(id)
                    .and_then(|pane| pane.finish_paste(result, confirm_paste));
                if let Some(text) = pending_confirm {
                    let message = confirm::format_paste(&text);
                    client.open_confirm(confirm::ConfirmTarget::Paste { pane: id, text }, message);
                } else if broadcast
                    && let Some(text) = broadcast_result
                    && !text.is_empty()
                {
                    for pane in &mut client.tabs[client.active].panes {
                        if pane.id != id
                            && pane.vi.is_none()
                            && pane.search.is_none()
                            && pane.hints.is_none()
                        {
                            pane.commit_paste(text.clone());
                        }
                    }
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
            Event::AppMenuAction(action) => {
                client.run_shortcut(event_loop, action);
                Ok(())
            }
            Event::ButtonLayout(_)
            | Event::ConfigReload(_)
            | Event::ConfigSave
            | Event::IpcCommand(_) => unreachable!(),
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
    // Declared before `window` so guest Wayland queues (`AppMenu`, `BackgroundBlur`,
    // and `DataDevice`) clean up and stop before `window`'s `wl_display` and
    // `xdg_toplevel` are destroyed.
    app_menu: Option<app_menu::AppMenu>,
    blur: Option<wayland_blur::BackgroundBlur>,
    clipboard: Option<clipboard::DataDevice>,
    ipc: Option<ipc::IpcServer>,
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
    keymap_generation: u64,
    unfocused_split_opacity: f32,
    window_padding: u32,
    history_limits: HistoryLimits,
    cursor_style: CursorShape,
    cursor_blink: bool,
    shell: Option<Vec<OsString>>,
    shell_integration: bool,
    term: String,
    term_program: String,
    env: Vec<(String, String)>,
    mode: TerminalMode,
    tab_position: TabPosition,
    window_decorations: bool,
    background_blur: bool,
    window_controls: WindowControlsPolicy,
    command_blocks: bool,
    compositor_decorations: Option<CompositorDecorations>,
    portal_buttons: Option<WindowButtons>,
    scrollbar: ScrollbarPolicy,
    confirm_close: bool,
    confirm_paste: bool,
    copy_on_select: bool,
    confirm: Option<confirm::Dialog>,
    command_palette: Option<CommandPalette>,
    context_menu: Option<ContextMenu>,
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
                    .with_decorations(
                        (config.mode == TerminalMode::Classic && config.window_decorations)
                            || compositor_decorations
                                .as_ref()
                                .is_some_and(CompositorDecorations::uses_frameless_ssd),
                    )
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
        let initial_gutter = if config.mode == TerminalMode::Modern && config.command_blocks {
            (14.0 * scale.max(1.0)).round() as u32
        } else {
            0
        };
        let cell = painter.metrics().cell;
        let initial_inset = initial_gutter.min(
            content_area
                .size
                .width
                .get()
                .saturating_sub(cell.width.get()),
        );
        let initial_text_size = PixelSize {
            width: NonZeroU32::new(content_area.size.width.get().saturating_sub(initial_inset))
                .unwrap_or(cell.width),
            height: content_area.size.height,
        };
        let grid = GridGeometry::fit(initial_text_size, cell)
            .ok_or_else(|| io::Error::other("The terminal grid does not fit the Wayland window"))?;
        let blur = wayland_blur::BackgroundBlur::attach(&window);
        let clipboard = clipboard::DataDevice::attach(&window);
        let ipc = ipc::IpcServer::start(proxy.clone());
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
                term: &config.term,
                term_program: &config.term_program,
                env: &config.env,
                clipboard: clipboard.as_ref().map(clipboard::DataDevice::handle),
                ipc_socket: ipc.as_ref().map(ipc::IpcServer::socket_path),
            },
            launch.working_directory,
            launch.command.as_deref(),
        )?;
        let readiness = Readiness::new(proxy.clone())?;
        readiness.arm(pane.id, &mut pane.registration, pane.session.interests())?;
        pane.origin = PixelPosition {
            x: content_area.origin.x.saturating_add(initial_inset),
            y: content_area.origin.y,
        };
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
            app_menu: None,
            blur,
            clipboard,
            ipc,
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
            keymap_generation: 0,
            unfocused_split_opacity: config.unfocused_split_opacity,
            window_padding: config.window_padding,
            history_limits,
            cursor_style: config.cursor_style,
            cursor_blink: config.cursor_blink,
            shell: config.shell.clone(),
            shell_integration: config.shell_integration,
            term: config.term.clone(),
            term_program: config.term_program.clone(),
            env: config.env.clone(),
            mode: config.mode,
            tab_position: config.tab_position,
            window_decorations: config.window_decorations,
            background_blur: config.background_blur,
            window_controls: config.window_controls,
            command_blocks: config.command_blocks,
            compositor_decorations,
            portal_buttons,
            scrollbar: config.scrollbar,
            confirm_close: config.confirm_close,
            confirm_paste: config.confirm_paste,
            copy_on_select: config.copy_on_select,
            confirm: None,
            command_palette: None,
            context_menu: None,
        };
        client.reflow_tab_bar();
        client.sync_background_blur();
        if client
            .compositor_decorations
            .as_ref()
            .is_none_or(|decorations| decorations.has_afar_appmenu)
        {
            let menu_state = client.app_menu_state();
            client.app_menu = app_menu::AppMenu::attach(
                &client.window,
                client.proxy.clone(),
                menu_state,
                &client.keymap,
            );
        }
        Ok(client)
    }

    fn app_menu_state(&self) -> app_menu::MenuState {
        let tab = &self.tabs[self.active];
        let pane = tab.focused_pane();
        app_menu::MenuState {
            mode: self.mode,
            tab_position: self.tab_position,
            window_decorations: self.window_decorations,
            background_blur: self.background_blur,
            sidebar_collapsed: self.sidebar_collapsed,
            fullscreen: self.window.fullscreen().is_some(),
            vi_mode: pane.vi.is_some(),
            multi_tab: self.tabs.len() > 1,
            multi_pane: tab.panes.len() > 1,
            pane_zoomed: tab.zoomed.is_some(),
            broadcast_input: tab.broadcast_input,
            primary_screen: pane.session.screen().active_buffer()
                == lune_terminal::ScreenBuffer::Primary,
            keymap_generation: self.keymap_generation,
        }
    }

    fn sync_app_menu(&mut self) {
        if self.app_menu.is_none() {
            return;
        }
        let state = self.app_menu_state();
        if let Some(app_menu) = &mut self.app_menu {
            app_menu.dispatch_pending();
            app_menu.sync(state, &self.keymap);
        }
    }

    pub(super) fn clipboard_handle(&self) -> Option<clipboard::DataDeviceHandle> {
        self.clipboard.as_ref().map(clipboard::DataDevice::handle)
    }

    fn saved_settings(&self) -> config::SavedSettings {
        config::SavedSettings {
            mode: self.mode,
            tab_position: self.tab_position,
            window_decorations: self.window_decorations,
            background_blur: self.background_blur,
            window_controls: self.window_controls,
            command_blocks: self.command_blocks,
            scrollbar: self.scrollbar,
            copy_on_select: self.copy_on_select,
            font_size_points: self.font_size_points,
            theme_name: self.theme_preference.config_name().to_owned(),
        }
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
            term: &self.term,
            term_program: &self.term_program,
            env: &self.env,
            clipboard: self.clipboard_handle(),
            ipc_socket: self.ipc.as_ref().map(ipc::IpcServer::socket_path),
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
                self.close_context_menu();
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
                    self.context_menu = None;
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
                if self.context_menu.is_some() {
                    if event.state.is_pressed() {
                        self.handle_context_menu_key(event_loop, &event);
                    }
                    return Ok(());
                }
                if event.state.is_pressed() {
                    self.key(event_loop, &event);
                } else if self.tabs[self.active].broadcast_input
                    && self.tabs[self.active].panes.len() > 1
                {
                    for pane in &mut self.tabs[self.active].panes {
                        pane.key_release(&event);
                    }
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
                } else if self.tabs[self.active].broadcast_input
                    && self.tabs[self.active].panes.len() > 1
                    && let Ime::Commit(text) = ime
                {
                    let focused_id = self.tabs[self.active].focused;
                    for pane in &mut self.tabs[self.active].panes {
                        if pane.id == focused_id
                            || (pane.vi.is_none() && pane.search.is_none() && pane.hints.is_none())
                        {
                            pane.ime(Ime::Commit(text.clone()));
                        }
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
                if self.context_menu.is_some() {
                    let hovered = self
                        .context_menu_layout()
                        .and_then(|layout| layout.hit(position));
                    if let Some(menu) = &mut self.context_menu
                        && menu.hovered != hovered
                    {
                        menu.hovered = hovered;
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
                let blocks_enabled = self.command_blocks_enabled();
                let ui_cell = self.painter.chrome_metrics().cell;
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
                        pane.update_hovered_block(false, ui_cell, scrollbar);
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
                            pane.update_hovered_block(blocks_enabled, ui_cell, scrollbar);
                        } else if pane.pointer.is_some() || pane.hovered_block.is_some() {
                            pane.pointer = None;
                            pane.update_scrollbar_hover(scrollbar);
                            pane.update_hovered_hyperlink();
                            pane.update_hovered_block(false, ui_cell, scrollbar);
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
                let ui_cell = self.painter.chrome_metrics().cell;
                for pane in &mut self.tabs[self.active].panes {
                    if pane.pressed.is_none()
                        && pane.selection_drag.is_none()
                        && pane.scrollbar_drag.is_none()
                    {
                        pane.pointer = None;
                    }
                    pane.update_scrollbar_hover(scrollbar);
                    pane.update_hovered_hyperlink();
                    pane.update_hovered_block(false, ui_cell, scrollbar);
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
                if self.context_menu.is_some() {
                    if state.is_pressed() {
                        if button == winit::event::MouseButton::Left
                            && let Some(point) = self.pointer
                            && let Some(layout) = self.context_menu_layout()
                            && let Some(index) = layout.hit(point)
                        {
                            let action = self
                                .context_menu
                                .as_ref()
                                .and_then(|menu| menu.action_at(index));
                            self.close_context_menu();
                            if let Some(action) = action {
                                self.execute_context_menu_action(event_loop, action);
                            }
                            self.update_cursor_icon();
                            return Ok(());
                        }
                        let right_outside = button == winit::event::MouseButton::Right
                            && self.pointer.is_some_and(|point| {
                                self.context_menu_layout()
                                    .is_none_or(|layout| !layout.contains_card(point))
                            });
                        self.close_context_menu();
                        self.update_cursor_icon();
                        if !right_outside {
                            return Ok(());
                        }
                    } else {
                        return Ok(());
                    }
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
                        winit::event::MouseButton::Right => {
                            if let Some(
                                TitlebarAction::SelectTab(index) | TitlebarAction::CloseTab(index),
                            ) = hit
                                && index < self.tabs.len()
                            {
                                self.open_tab_context_menu(point, index);
                            } else {
                                self.open_chrome_context_menu(point);
                            }
                        }
                        _ => {}
                    }
                    self.update_cursor_icon();
                    return Ok(());
                }
                if state.is_pressed() && self.pointer.is_some_and(|point| self.bar.contains(point))
                {
                    let point = self.pointer.unwrap();
                    if let Some(index) = self.bar.hit(point)
                        && index < self.tabs.len()
                    {
                        match button {
                            winit::event::MouseButton::Left => self.select_tab(index),
                            winit::event::MouseButton::Middle => {
                                self.request_close_tab(event_loop, index);
                            }
                            winit::event::MouseButton::Right => {
                                self.open_tab_context_menu(point, index);
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
                    if button == winit::event::MouseButton::Right
                        && let Some(point) = self.pointer
                        && let Some(target_id) = self.tabs[self.active].layout.pane_at(point)
                        && self.tabs[self.active].pane(target_id).is_some_and(|pane| {
                            pane.allows_context_menu(self.modifiers.shift_key())
                        })
                    {
                        if target_id != self.tabs[self.active].focused {
                            self.focus_pane(target_id);
                        }
                        self.open_pane_context_menu(point, target_id);
                        self.update_cursor_icon();
                        return Ok(());
                    }
                    if button == winit::event::MouseButton::Left
                        && self.command_blocks_enabled()
                        && let Some(point) = self.pointer
                        && let Some(target_id) = self.tabs[self.active].layout.pane_at(point)
                        && self.tabs[self.active].pane(target_id).is_some_and(|pane| {
                            pane.hovered_block
                                .is_some_and(|hovered| hovered.hovered_action.is_some())
                        })
                    {
                        if target_id != self.tabs[self.active].focused {
                            self.focus_pane(target_id);
                        }
                        if let Some(notice) =
                            self.tabs[self.active].pane_mut(target_id).and_then(|pane| {
                                pane.trigger_hovered_block_action(&mut self.copy_pending)
                            })
                        {
                            self.show_notice(NoticeKind::Success, notice);
                        }
                        self.painter.invalidate_overlay();
                        self.window.request_redraw();
                        self.update_cursor_icon();
                        return Ok(());
                    }
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
                    let blocks_enabled = self.command_blocks_enabled();
                    let ui_cell = self.painter.chrome_metrics().cell;
                    let scrollbar = self.scrollbar;
                    if let Some(pane) = self.tabs[self.active].pane_mut(target_id) {
                        pane.scroll(delta);
                        pane.update_hovered_block(blocks_enabled, ui_cell, scrollbar);
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
        } else if let Some(menu) = &self.context_menu {
            if menu.hovered.is_some() {
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
                } else if tab.panes.iter().any(|pane| {
                    pane.visible
                        && (pane.hovered_link.is_some()
                            || pane
                                .hovered_block
                                .is_some_and(|block| block.hovered_action.is_some()))
                }) {
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
                    self.dispatch_pane_key(event);
                } else {
                    self.run_shortcut(event_loop, action);
                }
            }
        } else {
            self.dispatch_pane_key(event);
        }
    }

    fn dispatch_pane_key(&mut self, event: &KeyEvent) {
        let broadcast = self.tabs[self.active].broadcast_input
            && self.tabs[self.active].panes.len() > 1
            && self.tabs[self.active].focused_pane().vi.is_none();
        if broadcast {
            let focused_id = self.tabs[self.active].focused;
            let mut notice = None;
            for pane in &mut self.tabs[self.active].panes {
                let target = pane.id == focused_id
                    || (pane.vi.is_none() && pane.search.is_none() && pane.hints.is_none());
                if target && let Some(text) = pane.key(event, &mut self.copy_pending) {
                    notice = Some(text);
                }
            }
            if let Some(text) = notice {
                self.show_notice(NoticeKind::Success, text);
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
            Action::ToggleWindowDecorations => self.toggle_window_decorations(),
            Action::ToggleBackgroundBlur => self.toggle_background_blur(),
            Action::PreviousPrompt => self.tabs[self.active].focused_pane_mut().jump_prompt(false),
            Action::NextPrompt => self.tabs[self.active].focused_pane_mut().jump_prompt(true),
            Action::PreviousFailedPrompt => {
                if !self.tabs[self.active]
                    .focused_pane_mut()
                    .jump_previous_failed_prompt()
                {
                    self.show_notice(NoticeKind::Warning, "No failed command found".into());
                }
            }
            Action::CopyLastOutput => {
                if let Some(text) = self.tabs[self.active]
                    .focused_pane()
                    .copy_last_output(&mut self.copy_pending)
                {
                    self.show_notice(NoticeKind::Success, text);
                } else {
                    self.show_notice(NoticeKind::Warning, "No command output found".into());
                }
            }
            Action::CopyLastCommand => {
                if let Some(text) = self.tabs[self.active]
                    .focused_pane()
                    .copy_last_command(&mut self.copy_pending)
                {
                    self.show_notice(NoticeKind::Success, text);
                } else {
                    self.show_notice(NoticeKind::Warning, "No command found".into());
                }
            }
            Action::SelectLastOutput => {
                if let Some(text) = self.tabs[self.active]
                    .focused_pane_mut()
                    .select_last_output()
                {
                    self.show_notice(NoticeKind::Success, text);
                } else {
                    self.show_notice(NoticeKind::Warning, "No command output found".into());
                }
            }
            Action::ToggleBlockBookmark => {
                if let Some(text) = self.tabs[self.active]
                    .focused_pane_mut()
                    .toggle_active_block_bookmark()
                {
                    self.show_notice(NoticeKind::Success, text);
                } else {
                    self.show_notice(NoticeKind::Warning, "No command block found".into());
                }
            }
            Action::PreviousBookmark => {
                if !self.tabs[self.active]
                    .focused_pane_mut()
                    .jump_bookmark(false)
                {
                    self.show_notice(NoticeKind::Warning, "No earlier bookmark found".into());
                }
            }
            Action::NextBookmark => {
                if !self.tabs[self.active]
                    .focused_pane_mut()
                    .jump_bookmark(true)
                {
                    self.show_notice(NoticeKind::Warning, "No later bookmark found".into());
                }
            }
            Action::SearchBookmarks => {
                let entries = {
                    let screen = self.tabs[self.active].focused_pane().session.screen();
                    palette::build_bookmarked_command_entries(screen.view())
                };
                if entries.is_empty() {
                    self.show_notice(NoticeKind::Warning, "No bookmarked commands in pane".into());
                } else {
                    self.open_palette();
                    if let Some(cmd_palette) = &mut self.command_palette {
                        cmd_palette.push_submenu("Bookmarks", entries, 0);
                    }
                    self.painter.invalidate_overlay();
                    self.window.request_redraw();
                }
            }
            Action::RerunLastCommand => {
                if let Some(text) = self.tabs[self.active]
                    .focused_pane_mut()
                    .rerun_last_command()
                {
                    self.show_notice(NoticeKind::Success, text);
                } else {
                    self.show_notice(NoticeKind::Warning, "No command found".into());
                }
            }
            Action::ReloadConfig => {
                let _ = self.proxy.send_event(Event::ConfigReload(true));
            }
            Action::SaveConfig => {
                let _ = self.proxy.send_event(Event::ConfigSave);
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
            Action::RenameTab => self.open_rename_tab(self.active),
            Action::SplitPane(axis) => {
                if let Err(error) = self.split_pane(axis) {
                    log::warn!("Could not split terminal pane: {error}");
                }
            }
            Action::FocusPane(direction) => self.focus_pane_direction(direction),
            Action::ResizePane(direction) => self.resize_pane_direction(direction),
            Action::TogglePaneZoom => self.toggle_pane_zoom(),
            Action::SwapPane => self.swap_pane_next(),
            Action::DetachPane => self.detach_pane_to_tab(),
            Action::ToggleBroadcastInput => self.toggle_broadcast_input(),
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
                .set_decorations(self.native_decorations_enabled());
            self.titlebar_hovered = None;
            self.titlebar_pressed = None;
            self.tab_drag = None;
            self.sidebar_resize_drag = None;
            self.tab_animator.clear();
            layout_changed = true;
        }
        if old.window_decorations != new.window_decorations {
            self.window_decorations = new.window_decorations;
            self.window
                .set_decorations(self.native_decorations_enabled());
            self.update_cursor_icon();
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
        if old.command_blocks != new.command_blocks {
            self.command_blocks = new.command_blocks;
            if !self.command_blocks {
                for tab in &mut self.tabs {
                    for pane in &mut tab.panes {
                        pane.hovered_block = None;
                    }
                }
            }
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
        if old.background_blur != new.background_blur {
            self.background_blur = new.background_blur;
        }
        if old.background_blur != new.background_blur
            || old.background_opacity != new.background_opacity
        {
            self.sync_background_blur();
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
            self.keymap_generation = self.keymap_generation.wrapping_add(1);
        }
        self.shell = new.shell.clone();
        self.shell_integration = new.shell_integration;
        self.term = new.term.clone();
        self.term_program = new.term_program.clone();
        self.env = new.env.clone();
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
        self.sync_app_menu();
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
        self.sync_background_blur();
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
        let gutter = self.block_gutter();
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            tab.reflow(
                content_area,
                divider,
                metrics.cell,
                gutter,
                index == self.active,
            );
        }
        self.reflow_tab_bar();
        let pane = self.tabs[self.active].focused_pane_mut();
        pane.invalidate_ime_area();
        pane.update_ime_area();
        self.window.request_redraw();
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
                text_origin: pane.origin,
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
                command_blocks: self.command_blocks_enabled()
                    && pane.session.screen().active_buffer()
                        == lune_terminal::ScreenBuffer::Primary,
                hovered_block: pane.hovered_block,
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
                context_menu: self.context_menu.as_ref(),
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
