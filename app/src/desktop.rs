mod clipboard;
mod font;
mod input;
mod paint;
mod readiness;
mod selection;
mod text;
mod theme;

use std::collections::VecDeque;
use std::io;
use std::num::NonZeroU32;
use std::ops::Range;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lune_terminal::{
    CellPosition, HistoryLimits, Modifiers, MouseButton, MouseEvent, MouseTracking, PixelPosition,
    PixelSize, RowRevision, ScreenBuffer,
};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, KeyEvent, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key as LogicalKey, ModifiersState, NamedKey};
use winit::platform::wayland::{EventLoopBuilderExtWayland, WindowAttributesExtWayland};
use winit::window::{Fullscreen, Theme, UserAttentionType, Window, WindowId};

use crate::{GridGeometry, MAX_INPUT_BYTES, Session, SessionReady, WaitOutcome};

use clipboard::ClipboardType;
use font::{Font, FontMetrics};
use input::Input;
use paint::Painter;
use readiness::Readiness;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const FONT_SIZE: f32 = 15.0;
const PADDING: f64 = 10.0;
const INPUT_LIMIT: usize = 1024;
const PASTE_TIMEOUT: Duration = Duration::from_secs(2);

enum Event {
    PtyReady(u64),
    PtyError(io::Error),
    CopyReady(std::result::Result<(), String>),
    PasteReady(std::result::Result<String, String>),
}

pub fn run() -> Result<()> {
    let event_loop = EventLoop::<Event>::with_user_event()
        .with_wayland()
        .build()?;
    let mut app = App {
        proxy: event_loop.create_proxy(),
        client: None,
        error: None,
    };
    event_loop.run_app(&mut app)?;
    app.error.map_or(Ok(()), Err)
}

struct App {
    proxy: EventLoopProxy<Event>,
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
            match Client::new(event_loop, self.proxy.clone()) {
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
                            eprintln!("Lune clipboard: {error}");
                            client.inputs.remove(index);
                        }
                    }
                }
                Ok(())
            }
            Event::CopyReady(result) => {
                client.copy_pending = false;
                if let Err(error) = result {
                    eprintln!("Lune clipboard: {error}");
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
    paste_worker: bool,
    paste_deadline: Option<Instant>,
    font_size: f32,
    padding: u32,
    resize_pending: bool,
    modifiers: ModifiersState,
    focused: bool,
    ime_enabled: bool,
    ime_area: Option<(PixelPosition, PixelSize)>,
    preedit: String,
    pointer: Option<PhysicalPosition<f64>>,
    pressed: Option<MouseButton>,
    wheel: f64,
    selection_anchor: Option<CellPosition>,
    selection: Option<Selection>,
}

struct Selection {
    range: Range<CellPosition>,
    rows: Vec<RowRevision>,
}

impl Client {
    fn new(event_loop: &ActiveEventLoop, proxy: EventLoopProxy<Event>) -> Result<Self> {
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
            Font::new(),
            FONT_SIZE * scale as f32,
        ))?;
        let padding = padding(scale);
        let geometry = geometry(window.inner_size(), painter.metrics(), padding)
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
        let mut session = Session::spawn(command, geometry.pty_size(), palette.clone())?;
        session.set_theme(&palette, Some(scheme));
        session.set_history_limits(HistoryLimits {
            max_rows: 10_000,
            max_bytes: 32 << 20,
        })?;
        let mut readiness = Readiness::new(proxy.clone())?;
        readiness.arm(session.interests())?;
        window.set_ime_allowed(true);
        window.request_redraw();
        Ok(Self {
            window,
            readiness,
            session,
            painter,
            proxy,
            inputs: VecDeque::new(),
            copy_pending: false,
            paste_worker: false,
            paste_deadline: None,
            font_size: FONT_SIZE,
            padding,
            resize_pending: false,
            modifiers: ModifiersState::empty(),
            focused: false,
            ime_enabled: false,
            ime_area: None,
            preedit: String::new(),
            pointer: None,
            pressed: None,
            wheel: 0.0,
            selection_anchor: None,
            selection: None,
        })
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, event: WindowEvent) -> Result<()> {
        match event {
            WindowEvent::CloseRequested | WindowEvent::Destroyed => event_loop.exit(),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                self.resize_pending = true
            }
            WindowEvent::RedrawRequested => {
                self.apply_resize()?;
                if !self.resize_pending {
                    self.draw()?;
                }
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
                    if let Some(button) = self.pressed.take() {
                        self.mouse_event(MouseEvent::Release(button));
                    }
                    self.selection_anchor = None;
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
                if self.preedit.is_empty() && !text.is_empty() {
                    self.follow_output();
                }
                self.set_preedit(text);
            }
            WindowEvent::Ime(Ime::Commit(mut text)) => {
                self.set_preedit(String::new());
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
                self.pointer = Some(position);
                if self.selection_anchor.is_some() {
                    self.extend_selection();
                } else {
                    self.mouse_event(MouseEvent::Motion(self.pressed));
                }
            }
            WindowEvent::CursorLeft { .. }
                if self.pressed.is_none() && self.selection_anchor.is_none() =>
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
        if self.selection.as_ref().is_some_and(|selection| {
            let viewport = self.session.screen().viewport();
            (selection.range.start.row..=selection.range.end.row)
                .map(|row| viewport.row(row).map(|row| row.revision()))
                .ne(selection.rows.iter().copied().map(Some))
        }) {
            self.clear_selection();
        }
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
            eprintln!("Lune clipboard: Paste timed out");
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
        let physical_font = (self.font_size * scale as f32).clamp(8.0, 128.0);
        let metrics = self.painter.metrics();
        let metrics = if metrics.size == physical_font {
            metrics
        } else {
            self.painter.measure(physical_font)
        };
        let padding = padding(scale);
        let Some(geometry) = geometry(size, metrics, padding) else {
            self.resize_pending = false;
            return Ok(());
        };
        match self.session.resize(geometry.pty_size()) {
            Ok(()) => {
                self.resize_pending = false;
                self.padding = padding;
                self.painter.set_metrics(metrics);
                self.painter.resize(size);
                self.clear_selection();
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
        if let Some(frame) = self.session.presentation_frame() {
            self.painter.draw(
                &self.window,
                &frame,
                self.padding,
                self.selection.as_ref().map(|selection| &selection.range),
                &self.preedit,
            )?;
        }
        Ok(())
    }

    fn update_ime_area(&mut self) {
        if !self.ime_enabled {
            return;
        }
        let geometry = self.session.geometry();
        let Some(cursor) = self.session.screen().viewport().cursor() else {
            return;
        };
        let Some(rect) = geometry.row_rect(cursor.row, cursor.column..cursor.column + 1) else {
            return;
        };
        let origin = PixelPosition {
            x: rect.origin.x + self.padding,
            y: rect.origin.y + self.padding,
        };
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

    fn modifiers(&self) -> Modifiers {
        Modifiers {
            shift: self.modifiers.shift_key(),
            alt: self.modifiers.alt_key(),
            control: self.modifiers.control_key(),
        }
    }

    fn key(&mut self, event: &KeyEvent) {
        if self.modifiers.super_key() {
            return;
        }
        let modifiers = self.modifiers();
        match (
            &event.logical_key,
            modifiers.control,
            modifiers.shift,
            modifiers.alt,
        ) {
            (LogicalKey::Character(key), true, true, false) if key.eq_ignore_ascii_case("c") => {
                if !event.repeat {
                    self.copy();
                }
                return;
            }
            (LogicalKey::Character(key), true, true, false) if key.eq_ignore_ascii_case("v") => {
                if !event.repeat {
                    self.paste(ClipboardType::Regular);
                }
                return;
            }
            (LogicalKey::Character(key), true, _, false)
                if matches!(key.as_str(), "=" | "+" | "-" | "0") =>
            {
                self.font_size = match key.as_str() {
                    "-" => (self.font_size - 1.0).max(8.0),
                    "0" => FONT_SIZE,
                    _ => (self.font_size + 1.0).min(32.0),
                };
                self.resize_pending = true;
                return;
            }
            (LogicalKey::Named(NamedKey::F11), false, false, false) => {
                if !event.repeat {
                    self.window.set_fullscreen(
                        self.window
                            .fullscreen()
                            .is_none()
                            .then_some(Fullscreen::Borderless(None)),
                    );
                }
                return;
            }
            (LogicalKey::Named(NamedKey::PageUp | NamedKey::PageDown), false, true, false) => {
                let page = (self.session.geometry().pty_size().rows as isize - 1).max(1);
                let up = event.logical_key == LogicalKey::Named(NamedKey::PageUp);
                if self.session.scroll_history(if up { page } else { -page }) {
                    self.clear_selection();
                    self.window.request_redraw();
                }
                return;
            }
            _ => {}
        }
        if let Some(input) = input::key(event, modifiers) {
            self.follow_output();
            self.push_input(input);
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
        let padding = f64::from(self.padding);
        let (x, y) = (pointer.x - padding, pointer.y - padding);
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
            self.push_input(Input::Mouse(event, pixel, self.modifiers()));
        }
    }

    fn mouse_reporting(&self) -> bool {
        self.session.screen().viewport_offset() == 0
            && self.session.processor().input_modes().mouse_tracking != MouseTracking::Disabled
    }

    fn button(&mut self, state: ElementState, button: winit::event::MouseButton) {
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
            if button == MouseButton::Left && local {
                self.clear_selection();
                self.selection_anchor = self
                    .pointer_pixel(false)
                    .and_then(|pixel| self.session.geometry().cell_at(pixel));
                self.extend_selection();
            } else if button == MouseButton::Middle && local {
                if self.pointer_pixel(false).is_some() {
                    self.paste(ClipboardType::Primary);
                }
            } else if mouse_reporting && self.pointer_pixel(false).is_some() {
                self.pressed = Some(button);
                self.mouse_event(MouseEvent::Press(button));
            }
        } else if button == MouseButton::Left && self.selection_anchor.is_some() {
            self.extend_selection();
            self.selection_anchor = None;
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
        if self.session.screen().active_buffer() == ScreenBuffer::Primary
            && (!self.mouse_reporting() || self.modifiers.shift_key())
        {
            if self.session.scroll_history(steps * 3) {
                self.clear_selection();
                self.window.request_redraw();
            }
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

    fn extend_selection(&mut self) {
        let Some(anchor) = self.selection_anchor else {
            return;
        };
        let Some(focus) = self
            .pointer_pixel(true)
            .and_then(|pixel| self.session.geometry().cell_at(pixel))
        else {
            return;
        };
        let viewport = self.session.screen().viewport();
        let range = selection::range(viewport, anchor, focus);
        if self.selection.as_ref().map(|selection| &selection.range) == range.as_ref() {
            return;
        }
        self.selection = range.map(|range| Selection {
            rows: (range.start.row..=range.end.row)
                .filter_map(|row| viewport.row(row).map(|row| row.revision()))
                .collect(),
            range,
        });
        self.painter.invalidate_overlay();
        self.window.request_redraw();
    }

    fn clear_selection(&mut self) {
        self.selection_anchor = None;
        if self.selection.take().is_some() {
            self.painter.invalidate_overlay();
            self.window.request_redraw();
        }
    }

    fn copy(&mut self) {
        if self.copy_pending {
            return;
        }
        if let Some(selection) = &self.selection
            && let Ok(text) = self
                .session
                .screen()
                .viewport()
                .selection_text(selection.range.clone())
        {
            match clipboard::write(text.to_string(), self.proxy.clone()) {
                Ok(()) => self.copy_pending = true,
                Err(error) => eprintln!("Lune clipboard: {error}"),
            }
        }
    }

    fn paste(&mut self, clipboard: ClipboardType) {
        if self.paste_worker || self.inputs.len() >= INPUT_LIMIT {
            return;
        }
        match clipboard::read(clipboard, self.proxy.clone()) {
            Ok(()) => {
                self.follow_output();
                self.push_input(Input::PendingPaste);
                self.paste_worker = true;
                self.paste_deadline = Some(Instant::now() + PASTE_TIMEOUT);
            }
            Err(error) => eprintln!("Lune clipboard: {error}"),
        }
    }
}

fn padding(scale: f64) -> u32 {
    (PADDING * scale).round() as u32
}

fn geometry(size: PhysicalSize<u32>, metrics: FontMetrics, padding: u32) -> Option<GridGeometry> {
    let area = PixelSize {
        width: NonZeroU32::new(size.width.checked_sub(2 * padding)?)?,
        height: NonZeroU32::new(size.height.checked_sub(2 * padding)?)?,
    };
    GridGeometry::fit(area, metrics.cell)
}
