mod clipboard;
mod config;
mod cursor;
mod directory;
mod font;
mod input;
mod paint;
mod readiness;
mod selection;
mod shortcuts;
mod text;
mod theme;
mod vi;

use std::collections::VecDeque;
use std::io;
use std::num::NonZeroU32;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lune_terminal::{
    CellPosition, HistoryLimits, MouseButton, MouseEvent, MouseTracking, PixelPosition, PixelSize,
    ScreenBuffer,
};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, KeyEvent, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::platform::wayland::{EventLoopBuilderExtWayland, WindowAttributesExtWayland};
use winit::window::{Fullscreen, Theme, UserAttentionType, Window, WindowId};

use crate::{GridGeometry, MAX_INPUT_BYTES, Session, SessionReady, WaitOutcome};

use clipboard::ClipboardType;
use config::{Config, MAX_FONT_SIZE, MIN_FONT_SIZE};
use font::{Font, FontMetrics, physical_font_size};
use input::Input;
use paint::Painter;
use readiness::Readiness;
use shortcuts::{Action, Scroll, Zoom};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const INPUT_LIMIT: usize = 1024;
const PASTE_TIMEOUT: Duration = Duration::from_secs(2);

enum Event {
    PtyReady(u64),
    PtyError(io::Error),
    CopyReady(std::result::Result<(), String>),
    PasteReady(std::result::Result<String, String>),
    DirectoryOpened(io::Result<()>),
}

pub fn run() -> Result<()> {
    let config = Config::load();
    let event_loop = EventLoop::<Event>::with_user_event()
        .with_wayland()
        .build()?;
    log::debug!("Wayland event loop ready");
    let mut app = App {
        proxy: event_loop.create_proxy(),
        config,
        client: None,
        error: None,
    };
    event_loop.run_app(&mut app)?;
    app.error.map_or(Ok(()), Err)
}

struct App {
    proxy: EventLoopProxy<Event>,
    config: Config,
    client: Option<Client>,
    error: Option<Box<dyn std::error::Error>>,
}

impl App {
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
            match Client::new(event_loop, self.proxy.clone(), &self.config) {
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
        let Some(client) = &mut self.client else {
            return;
        };
        let result = match event {
            Event::PtyReady(token) => match client.readiness.ready(token) {
                Some(ready) => client.dispatch(event_loop, ready),
                None => Ok(()),
            },
            Event::PtyError(error) => Err(error.into()),
            Event::PasteReady(result) => {
                client.paste_worker = false;
                if client.paste_deadline.take().is_some()
                    && let Some(index) = client
                        .inputs
                        .iter()
                        .position(|action| matches!(action, Input::PendingPaste))
                {
                    match result {
                        Ok(text) if !text.is_empty() => client.inputs[index] = Input::Paste(text),
                        Ok(_) => {
                            client.inputs.remove(index);
                        }
                        Err(error) => {
                            log::warn!("Clipboard paste failed: {error}");
                            client.inputs.remove(index);
                        }
                    }
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
    readiness: Readiness,
    session: Session,
    painter: Painter,
    proxy: EventLoopProxy<Event>,
    inputs: VecDeque<Input>,
    copy_pending: bool,
    directory_open_pending: bool,
    primary_selection: Option<clipboard::Primary>,
    paste_worker: bool,
    paste_deadline: Option<Instant>,
    font_size_points: f32,
    base_font_size_points: f32,
    resize_pending: bool,
    modifiers: ModifiersState,
    focused: bool,
    ime_enabled: bool,
    ime_area: Option<(PixelPosition, PixelSize)>,
    preedit: String,
    pointer: Option<PhysicalPosition<f64>>,
    pressed: Option<MouseButton>,
    wheel: f64,
    selection_click: Option<selection::Click>,
    selection_drag: Option<selection::Drag>,
    selection: Option<selection::Selection>,
    vi: Option<vi::Mode>,
    vi_search: Option<vi::Search>,
}

impl Client {
    fn new(
        event_loop: &ActiveEventLoop,
        proxy: EventLoopProxy<Event>,
        config: &Config,
    ) -> Result<Self> {
        let window = Arc::new(
            event_loop.create_window(
                Window::default_attributes()
                    .with_title("Lune")
                    .with_name("lune", "")
                    .with_inner_size(LogicalSize::new(960, 600))
                    .with_min_inner_size(LogicalSize::new(180, 100)),
            )?,
        );
        let scale = window.scale_factor();
        let painter = pollster::block_on(Painter::new(
            window.clone(),
            Font::new(config.font_family.as_deref()),
            physical_font_size(config.font_size_points, scale),
        ))?;
        let geometry = geometry(window.inner_size(), painter.metrics())
            .ok_or_else(|| io::Error::other("The terminal grid does not fit the Wayland window"))?;
        let shell = std::env::var_os("SHELL")
            .filter(|shell| !shell.is_empty())
            .unwrap_or_else(|| "/bin/sh".into());
        let mut command = Command::new(shell);
        command
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("TERM_PROGRAM", "Lune");
        let (palette, scheme) = theme::palette(window.theme().unwrap_or(Theme::Dark));
        let pty_size = geometry.pty_size();
        log::debug!(
            "Starting PTY session with {} columns and {} rows",
            pty_size.columns,
            pty_size.rows
        );
        let mut session = Session::spawn(command, pty_size, palette.clone())?;
        session.set_theme(&palette, Some(scheme));
        session.set_history_limits(HistoryLimits {
            max_rows: 10_000,
            max_bytes: 32 << 20,
        })?;
        let mut readiness = Readiness::new(proxy.clone())?;
        readiness.arm(session.interests())?;
        window.set_ime_allowed(true);
        window.request_redraw();
        log::info!("Wayland terminal ready");
        Ok(Self {
            window,
            readiness,
            session,
            painter,
            proxy,
            inputs: VecDeque::new(),
            copy_pending: false,
            directory_open_pending: false,
            primary_selection: None,
            paste_worker: false,
            paste_deadline: None,
            font_size_points: config.font_size_points,
            base_font_size_points: config.font_size_points,
            resize_pending: false,
            modifiers: ModifiersState::empty(),
            focused: false,
            ime_enabled: false,
            ime_area: None,
            preedit: String::new(),
            pointer: None,
            pressed: None,
            wheel: 0.0,
            selection_click: None,
            selection_drag: None,
            selection: None,
            vi: None,
            vi_search: None,
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
                self.apply_resize()?;
                if !self.resize_pending {
                    self.draw()?;
                }
            }
            WindowEvent::Occluded(true) => {
                self.painter.reset_cursor_motion();
            }
            WindowEvent::Occluded(false) => {
                self.painter.invalidate_overlay();
                self.window.request_redraw();
            }
            WindowEvent::ThemeChanged(theme) => {
                let (palette, scheme) = theme::palette(theme);
                self.session.set_theme(&palette, Some(scheme));
                self.painter.invalidate_overlay();
                self.window.request_redraw();
            }
            WindowEvent::Focused(focused) => {
                self.focused = focused;
                self.push_input(Input::Focus(focused));
                if !focused {
                    if self.painter.reset_cursor_motion() {
                        self.window.request_redraw();
                    }
                    if let Some(button) = self.pressed.take() {
                        self.mouse_event(MouseEvent::Release(button));
                    }
                    self.selection_click = None;
                    self.selection_drag = None;
                    if let Some(vi) = &mut self.vi {
                        vi.cancel_pending();
                    }
                    self.modifiers = ModifiersState::empty();
                    self.set_preedit(String::new());
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } if event.state.is_pressed() => self.key(&event),
            WindowEvent::Ime(Ime::Enabled) => {
                self.ime_enabled = true;
                self.ime_area = None;
                self.update_ime_area();
            }
            WindowEvent::Ime(Ime::Disabled) => {
                self.ime_enabled = false;
                self.set_preedit(String::new());
            }
            WindowEvent::Ime(Ime::Preedit(text, _)) => {
                if self.vi.is_none() && self.preedit.is_empty() && !text.is_empty() {
                    self.follow_output();
                }
                if self.vi.as_ref().is_none_or(|vi| vi.prompt.is_some()) {
                    self.set_preedit(text);
                }
            }
            WindowEvent::Ime(Ime::Commit(mut text)) => {
                self.set_preedit(String::new());
                if let Some(vi) = &mut self.vi {
                    if let Some(prompt) = &mut vi.prompt {
                        prompt.push(&text);
                        self.update_ime_area();
                        self.redraw_vi();
                    }
                    return Ok(());
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
            }
            WindowEvent::CursorMoved { position, .. } => {
                let moved = self.pointer.replace(position) != Some(position);
                if self.selection_drag.is_some() {
                    self.extend_selection();
                    if moved {
                        self.update_selection_scroll(position.y);
                    }
                } else {
                    self.mouse_event(MouseEvent::Motion(self.pressed));
                }
            }
            WindowEvent::CursorLeft { .. }
                if self.pressed.is_none() && self.selection_drag.is_none() =>
            {
                self.pointer = None;
            }
            WindowEvent::MouseInput { state, button, .. } => self.button(state, button),
            WindowEvent::MouseWheel { delta, .. } => self.scroll(delta),
            _ => {}
        }
        Ok(())
    }

    fn dispatch(&mut self, event_loop: &ActiveEventLoop, ready: SessionReady) -> Result<()> {
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
            return Ok(());
        };
        let start = Instant::now();
        for _ in 0..16 {
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
            event_loop.exit();
            return Ok(());
        }
        let notifications = self.session.take_notifications();
        if let Some(title) = notifications.window_title {
            self.window
                .set_title(if title.is_empty() { "Lune" } else { title });
        }
        if notifications.bell && !self.focused {
            self.window
                .request_user_attention(Some(UserAttentionType::Informational));
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
        self.update_ime_area();
        if self.session.presentation_frame().is_some() {
            self.window.request_redraw();
        }
        Ok(())
    }

    fn wait(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        let now = Instant::now();
        if self.paste_deadline.is_some_and(|deadline| deadline <= now) {
            self.paste_deadline = None;
            self.inputs
                .retain(|input| !matches!(input, Input::PendingPaste));
            log::warn!("Clipboard paste timed out");
        }
        let interests = self.session.interests();
        if interests.immediate
            || interests
                .presentation_deadline
                .is_some_and(|deadline| deadline <= now)
        {
            self.dispatch(event_loop, SessionReady::default())?;
        }
        if event_loop.exiting() {
            return Ok(());
        }
        self.apply_resize()?;
        self.tick_selection_scroll(now);
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
        if self.painter.tick(now) {
            self.window.request_redraw();
        }
        let interests = self.session.interests();
        self.readiness.arm(interests)?;
        if interests.immediate {
            event_loop.set_control_flow(ControlFlow::Poll);
        } else {
            let deadline = interests
                .presentation_deadline
                .into_iter()
                .chain(self.painter.deadline())
                .chain(self.paste_deadline)
                .chain(
                    self.selection_drag
                        .as_ref()
                        .and_then(|drag| drag.edge_scroll.as_ref().map(|scroll| scroll.deadline)),
                )
                .min();
            event_loop.set_control_flow(deadline.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
        }
        Ok(())
    }

    fn apply_resize(&mut self) -> Result<()> {
        if !self.resize_pending {
            return Ok(());
        }
        let size = self.window.inner_size();
        if size.width == 0 || size.height == 0 {
            self.resize_pending = false;
            return Ok(());
        }
        let scale = self.window.scale_factor();
        let physical_font = physical_font_size(self.font_size_points, scale);
        let metrics = self.painter.metrics();
        let metrics = if metrics.size == physical_font {
            metrics
        } else {
            self.painter.measure(physical_font)
        };
        let Some(geometry) = geometry(size, metrics) else {
            self.resize_pending = false;
            return Ok(());
        };
        let previous = self.session.geometry().pty_size();
        let next = geometry.pty_size();
        let vi_row = self.vi.as_ref().map(|vi| {
            vi.cursor
                .row
                .saturating_sub(self.session.screen().viewport().range().start)
        });
        match self.session.resize(next) {
            Ok(()) => {
                if next != previous {
                    log::debug!(
                        "PTY resized to {} columns and {} rows",
                        next.columns,
                        next.rows
                    );
                }
                self.resize_pending = false;
                self.painter.set_metrics(metrics);
                self.painter.resize(size);
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
                self.window.request_redraw();
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => self.resize_pending = false,
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    fn draw(&mut self) -> Result<()> {
        if let Some(mut frame) = self.session.presentation_frame() {
            if self.vi.is_some() {
                frame.cursor_appearance.visible = false;
            }
            self.painter.draw(
                &self.window,
                &frame,
                self.selection.as_ref().map(|selection| &selection.range),
                &self.preedit,
                self.vi.as_ref(),
                self.focused,
            )?;
        }
        Ok(())
    }

    fn update_ime_area(&mut self) {
        if !self.ime_enabled {
            return;
        }
        let geometry = self.session.geometry();
        let viewport = self.session.screen().viewport();
        let (row, column) = if let Some(vi) = &self.vi {
            if vi.prompt.is_none() {
                return;
            }
            (viewport.rows() - 1, 0)
        } else if let Some(cursor) = viewport.cursor() {
            (cursor.row, cursor.column)
        } else {
            return;
        };
        let Some(rect) = geometry.row_rect(row, column..column + 1) else {
            return;
        };
        let origin = rect.origin;
        if self.ime_area != Some((origin, rect.size)) {
            self.window.set_ime_cursor_area(
                PhysicalPosition::new(origin.x as i32, origin.y as i32),
                PhysicalSize::new(rect.size.width.get(), rect.size.height.get()),
            );
            self.ime_area = Some((origin, rect.size));
        }
    }

    fn set_preedit(&mut self, mut text: String) {
        text.truncate(text.floor_char_boundary(1024));
        if self.preedit != text {
            self.preedit = text;
            self.painter.invalidate_overlay();
            self.window.request_redraw();
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

    fn key(&mut self, event: &KeyEvent) {
        if let Some(binding) =
            shortcuts::binding(event, self.modifiers, self.session.screen().active_buffer())
        {
            if let Some(action) = binding.action(event.repeat) {
                self.run_shortcut(action);
            }
            return;
        }
        if self.vi.is_some() {
            self.vi_key(event);
            return;
        }
        if let Some(input) = input::key(event, self.modifiers) {
            self.follow_output();
            self.push_input(input);
        }
    }

    fn run_shortcut(&mut self, action: Action) {
        match action {
            Action::Copy => self.copy(),
            Action::Paste => {
                if self.vi.is_none() {
                    self.paste(ClipboardType::Regular);
                }
            }
            Action::ToggleVi => self.toggle_vi(),
            Action::OpenDirectory => {
                if !self.directory_open_pending {
                    let uri = self
                        .session
                        .processor()
                        .working_directory_uri()
                        .map(str::to_owned);
                    match directory::open(uri, self.proxy.clone()) {
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
                let page = (self.session.geometry().pty_size().rows as isize - 1).max(1);
                let steps = match scroll {
                    Scroll::Oldest => isize::MAX,
                    Scroll::Live => isize::MIN,
                    Scroll::PageUp => page,
                    Scroll::PageDown => -page,
                };
                self.scroll_history(steps);
            }
        }
    }

    fn follow_output(&mut self) {
        let was_scrolled = self.session.screen().viewport_offset() != 0;
        self.session.follow_output();
        self.clear_selection();
        if self.painter.restart_blink(Instant::now()) || was_scrolled {
            self.window.request_redraw();
        }
    }

    fn pointer_pixel(&self, clamp: bool) -> Option<PixelPosition> {
        let pointer = self.pointer?;
        let area = self.session.geometry().text_area_pixels()?;
        let (x, y) = (pointer.x, pointer.y);
        if !clamp
            && (x < 0.0
                || y < 0.0
                || x >= f64::from(area.width.get())
                || y >= f64::from(area.height.get()))
        {
            return None;
        }
        Some(PixelPosition {
            x: (x as u32).min(area.width.get() - 1),
            y: (y as u32).min(area.height.get() - 1),
        })
    }

    fn mouse_event(&mut self, event: MouseEvent) {
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

    fn button(&mut self, state: ElementState, button: winit::event::MouseButton) {
        if self.vi.is_some() {
            return;
        }
        let button = match button {
            winit::event::MouseButton::Left => MouseButton::Left,
            winit::event::MouseButton::Right => MouseButton::Right,
            winit::event::MouseButton::Middle => MouseButton::Middle,
            winit::event::MouseButton::Back => MouseButton::Button8,
            winit::event::MouseButton::Forward => MouseButton::Button9,
            _ => return,
        };
        if state.is_pressed() {
            let mouse_reporting = self.mouse_reporting();
            let local = !mouse_reporting || self.modifiers.shift_key();
            if button != MouseButton::Left || !local {
                self.selection_click = None;
            }
            if button == MouseButton::Left && local {
                self.start_selection();
            } else if button == MouseButton::Middle && local {
                if self.pointer_pixel(false).is_some() {
                    self.paste(ClipboardType::Primary);
                }
            } else if mouse_reporting && self.pointer_pixel(false).is_some() {
                self.pressed = Some(button);
                self.mouse_event(MouseEvent::Press(button));
            }
        } else if button == MouseButton::Left && self.selection_drag.is_some() {
            self.extend_selection();
            self.selection_drag = None;
            self.publish_primary_selection();
        } else if self.pressed == Some(button) {
            self.mouse_event(MouseEvent::Release(button));
            self.pressed = None;
        }
    }

    fn scroll(&mut self, delta: MouseScrollDelta) {
        let horizontal = match delta {
            MouseScrollDelta::LineDelta(x, y) => {
                self.wheel += f64::from(y);
                x
            }
            MouseScrollDelta::PixelDelta(position) => {
                self.wheel += position.y / f64::from(self.painter.metrics().cell.height.get());
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

    fn scroll_history(&mut self, steps: isize) {
        if self.session.scroll_history(steps) {
            self.selection_click = None;
            self.selection_drag = None;
            self.scroll_vi_cursor();
            self.window.request_redraw();
        }
    }

    fn update_selection_scroll(&mut self, y: f64) {
        let Some(drag) = &mut self.selection_drag else {
            return;
        };
        let screen = self.session.screen();
        let band = f64::from(self.painter.metrics().cell.height.get()) * 0.4;
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

    fn tick_selection_scroll(&mut self, now: Instant) {
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
        let Some(position) = self.selection_position(false) else {
            if !shift {
                self.clear_selection();
            }
            return;
        };
        let now = Instant::now();
        let next = previous.and_then(|click| click.next(position, now, shift));
        let repeated = next.is_some();
        let click = next.unwrap_or_else(|| selection::Click::new(position, now, shift));
        if shift
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
        }
        let view = self.session.screen().view();
        let range = drag.range(view);
        if self.selection.as_ref().map(|selection| &selection.range) == range.as_ref() {
            return;
        }
        match (&mut self.selection, range) {
            (Some(selection), Some(range)) => selection.set_range(view, range),
            (selection, Some(range)) => {
                *selection = Some(selection::Selection::new(
                    view,
                    range,
                    Some(drag.anchor.clone()),
                ));
            }
            (selection, None) => *selection = None,
        }
        self.painter.invalidate_overlay();
        self.window.request_redraw();
    }

    fn clear_selection(&mut self) {
        self.selection_click = None;
        self.selection_drag = None;
        if self.selection.take().is_some() {
            self.painter.invalidate_overlay();
            self.window.request_redraw();
        }
    }

    fn copy(&mut self) {
        if self.copy_pending {
            return;
        }
        if let Some(text) = self.selected_text() {
            match clipboard::write(text, self.proxy.clone()) {
                Ok(()) => self.copy_pending = true,
                Err(error) => log::warn!("Failed to start the clipboard copy worker: {error}"),
            }
        }
    }

    fn selected_text(&self) -> Option<String> {
        let selection = self.selection.as_ref()?;
        let mut text = self
            .session
            .screen()
            .view()
            .selection_text(selection.range.clone())
            .ok()?
            .to_string();
        if self.vi.as_ref().is_some_and(vi::Mode::linewise) {
            text.push('\n');
        }
        (!text.is_empty()).then_some(text)
    }

    fn publish_primary_selection(&mut self) {
        let Some(text) = self.selected_text() else {
            return;
        };
        if self.primary_selection.is_none() {
            self.primary_selection = match clipboard::Primary::new() {
                Ok(primary) => Some(primary),
                Err(error) => {
                    log::warn!("Failed to start the primary selection worker: {error}");
                    return;
                }
            };
        }
        if let Some(primary) = &self.primary_selection {
            primary.publish(text);
        }
    }

    fn paste(&mut self, clipboard: ClipboardType) {
        if self.paste_worker || self.inputs.len() >= INPUT_LIMIT {
            return;
        }
        match clipboard::read(
            clipboard,
            self.primary_selection.as_ref(),
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

fn geometry(size: PhysicalSize<u32>, metrics: FontMetrics) -> Option<GridGeometry> {
    let area = PixelSize {
        width: NonZeroU32::new(size.width)?,
        height: NonZeroU32::new(size.height)?,
    };
    GridGeometry::fit(area, metrics.cell)
}
