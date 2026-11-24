use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lune_terminal::{
    CellPosition, ClipboardQuery, ColorScheme, HistoryLimits, HyperlinkId, MouseButton, MouseEvent,
    MouseTracking, Palette, PixelPosition, PixelSize, Progress, ScreenBuffer,
};
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, KeyEvent, MouseScrollDelta};
use winit::event_loop::EventLoopProxy;
use winit::keyboard::ModifiersState;
use winit::window::{UserAttentionType, Window};

use crate::{GridGeometry, MAX_INPUT_BYTES, PixelRect, Session, SessionReady, WaitOutcome};

use super::clipboard::{self, ClipboardType};
use super::config::{ClipboardPolicy, NotificationPolicy};
use super::directory;
use super::input::{self, Input};
use super::notify::Notifier;
use super::readiness::{Readiness, Registration};
use super::split::{PaneRect, SplitLayout, SplitTree};
use super::url::{self, DetectedUrl, HoveredLink};
use super::{Event, PASTE_TIMEOUT, PaneId, Result, selection, vi};

const INPUT_LIMIT: usize = 1024;

pub(super) struct DispatchContext<'a> {
    pub(super) focused: bool,
    pub(super) copy_pending: &'a mut bool,
    pub(super) primary: &'a mut Option<clipboard::Primary>,
    pub(super) clipboard_write: ClipboardPolicy,
    pub(super) clipboard_read: ClipboardPolicy,
    pub(super) notifications_policy: NotificationPolicy,
    pub(super) notifier: Option<&'a Notifier>,
}

pub(super) struct Tab {
    pub(super) tree: SplitTree,
    pub(super) panes: Vec<Pane>,
    pub(super) focused: PaneId,
    pub(super) zoomed: Option<PaneId>,
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
            tree: SplitTree::new(id),
            panes: vec![pane],
            focused: id,
            zoomed: None,
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
        self.focused_pane().label()
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
        tab_active: bool,
    ) {
        let zoomed = self.zoomed.filter(|_| self.panes.len() > 1);
        self.layout = self.tree.layout(area, divider, zoomed);
        for pane in &mut self.panes {
            if let Some(rect) = self.layout.rect_of(pane.id) {
                pane.origin = rect.origin;
                if let Some(grid) = GridGeometry::fit(rect.size, cell) {
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
    pub(super) ime_area: Option<(PixelPosition, PixelSize)>,
    pub(super) preedit: String,
    pub(super) pointer: Option<PhysicalPosition<f64>>,
    pub(super) pressed: Option<MouseButton>,
    wheel: f64,
    selection_click: Option<selection::Click>,
    pub(super) selection_drag: Option<selection::Drag>,
    pub(super) selection: Option<selection::Selection>,
    pub(super) hovered_link: Option<HoveredLink>,
    hyperlink_click: Option<HyperlinkId>,
    pub(super) vi: Option<vi::Mode>,
    pub(super) vi_search: Option<vi::Search>,
}

impl Pane {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn spawn(
        id: PaneId,
        window: Arc<Window>,
        proxy: EventLoopProxy<Event>,
        grid: GridGeometry,
        palette: &Palette,
        scheme: ColorScheme,
        cwd: Option<PathBuf>,
        program: Option<&[OsString]>,
    ) -> Result<Self> {
        let mut command = match program {
            Some([executable, args @ ..]) => {
                let mut command = Command::new(executable);
                command.args(args);
                command
            }
            _ => {
                let shell = std::env::var_os("SHELL")
                    .filter(|shell| !shell.is_empty())
                    .unwrap_or_else(|| "/bin/sh".into());
                Command::new(shell)
            }
        };
        if let Some(dir) = cwd {
            command.current_dir(dir);
        }
        command
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("TERM_PROGRAM", "Lune");
        let pty_size = grid.pty_size();
        log::debug!(
            "Starting PTY session with {} columns and {} rows",
            pty_size.columns,
            pty_size.rows
        );
        let mut session = Session::spawn(command, pty_size, palette.clone())?;
        session.set_theme(palette, Some(scheme));
        session.set_history_limits(HistoryLimits {
            max_rows: 10_000,
            max_bytes: 32 << 20,
        })?;
        Ok(Self {
            id,
            origin: PixelPosition { x: 0, y: 0 },
            target_grid: grid,
            session,
            registration: Registration::default(),
            window,
            proxy,
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
            ime_area: None,
            preedit: String::new(),
            pointer: None,
            pressed: None,
            wheel: 0.0,
            selection_click: None,
            selection_drag: None,
            selection: None,
            hovered_link: None,
            hyperlink_click: None,
            vi: None,
            vi_search: None,
        })
    }

    pub(super) fn working_directory(&self) -> Option<PathBuf> {
        directory::working_directory(
            self.session.processor().working_directory_uri(),
            self.session.foreground_process_group(),
            self.session.child_id(),
        )
    }

    pub(super) fn invalidate_overlay(&mut self) {
        self.dirty = true;
        if self.visible {
            self.window.request_redraw();
        }
    }

    pub(super) fn set_ime_allowed(&self) {
        if self.active {
            self.window
                .set_ime_allowed(self.vi.as_ref().is_none_or(|vi| vi.prompt.is_some()));
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
            self.hyperlink_click = None;
            if let Some(vi) = &mut self.vi {
                vi.cancel_pending();
            }
            self.set_preedit(String::new());
        }
    }

    pub(super) fn pointer_moved(&mut self, position: PhysicalPosition<f64>) {
        let moved = self.pointer.replace(position) != Some(position);
        if self.selection_drag.is_some() {
            self.extend_selection();
            if moved {
                self.update_selection_scroll(position.y);
            }
        } else {
            self.mouse_event(MouseEvent::Motion(self.pressed));
        }
        self.update_hovered_hyperlink();
    }

    pub(super) fn update_hovered_hyperlink(&mut self) {
        let ctrl_only = self.modifiers.control_key()
            && !self.modifiers.alt_key()
            && !self.modifiers.super_key();
        let active_hover = self.visible
            && self.vi.is_none()
            && self.selection_drag.is_none()
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
            Ime::Preedit(text, _) => {
                if self.vi.is_none() && self.preedit.is_empty() && !text.is_empty() {
                    self.follow_output();
                }
                if self.vi.as_ref().is_none_or(|vi| vi.prompt.is_some()) {
                    self.set_preedit(text);
                }
            }
            Ime::Commit(mut text) => {
                self.set_preedit(String::new());
                if let Some(vi) = &mut self.vi {
                    if let Some(prompt) = &mut vi.prompt {
                        prompt.push(&text);
                        self.update_ime_area();
                        self.redraw_vi();
                    }
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
            if write.targets.clipboard && !*ctx.copy_pending {
                match clipboard::write(text.clone(), self.proxy.clone()) {
                    Ok(()) => *ctx.copy_pending = true,
                    Err(error) => {
                        log::warn!("Failed to start the OSC 52 copy worker: {error}");
                    }
                }
            }
            if write.targets.primary {
                publish_primary_text(text, ctx.primary);
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
        let origin = PixelPosition {
            x: self.origin.x + rect.origin.x,
            y: self.origin.y + rect.origin.y,
        };
        if self.ime_area != Some((origin, rect.size)) {
            self.window.set_ime_cursor_area(
                PhysicalPosition::new(origin.x as i32, origin.y as i32),
                PhysicalSize::new(rect.size.width.get(), rect.size.height.get()),
            );
            self.ime_area = Some((origin, rect.size));
        }
    }

    pub(super) fn set_preedit(&mut self, mut text: String) {
        text.truncate(text.floor_char_boundary(1024));
        if self.preedit != text {
            self.preedit = text;
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

    pub(super) fn finish_paste(&mut self, result: std::result::Result<String, String>) {
        self.paste_worker = false;
        if self.paste_deadline.take().is_some()
            && let Some(index) = self
                .inputs
                .iter()
                .position(|input| matches!(input, Input::PendingPaste))
        {
            match result {
                Ok(text) if !text.is_empty() => self.inputs[index] = Input::Paste(text),
                Ok(_) => {
                    self.inputs.remove(index);
                }
                Err(error) => {
                    log::warn!("Clipboard paste failed: {error}");
                    self.inputs.remove(index);
                }
            }
        }
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

    pub(super) fn key(&mut self, event: &KeyEvent, copy_pending: &mut bool) {
        if self.vi.is_some() {
            self.vi_key(event, copy_pending);
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
        }
    }

    pub(super) fn key_release(&mut self, event: &KeyEvent) {
        if self.vi.is_none()
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

    pub(super) fn follow_output(&mut self) {
        let was_scrolled = self.session.screen().viewport_offset() != 0;
        self.session.follow_output();
        self.clear_selection();
        self.restart_blink = true;
        if self.visible && was_scrolled {
            self.window.request_redraw();
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

    pub(super) fn button(
        &mut self,
        state: ElementState,
        button: winit::event::MouseButton,
        primary: &mut Option<clipboard::Primary>,
        hyperlink_open_pending: &mut bool,
    ) {
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
            let ctrl_only = self.modifiers.control_key()
                && !self.modifiers.alt_key()
                && !self.modifiers.super_key();
            if button == MouseButton::Left && ctrl_only {
                if let Some(link_id) = self.hyperlink_at_pointer() {
                    self.open_hyperlink(link_id, hyperlink_open_pending);
                    return;
                }
                if let Some(detected) = self.detected_url_at_pointer() {
                    self.open_uri(detected.uri, hyperlink_open_pending);
                    return;
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
        } else if button == MouseButton::Left && self.selection_drag.is_some() {
            self.extend_selection();
            let moved = self
                .selection_drag
                .as_ref()
                .is_some_and(selection::Drag::moved);
            self.selection_drag = None;
            let clicked_link = self.hyperlink_click.take();
            if !moved
                && self.selection.is_none()
                && let Some(link_id) = clicked_link
                && self.hyperlink_at_pointer() == Some(link_id)
            {
                self.open_hyperlink(link_id, hyperlink_open_pending);
            } else {
                self.publish_primary_selection(primary);
            }
            self.update_hovered_hyperlink();
        } else if self.pressed == Some(button) {
            self.mouse_event(MouseEvent::Release(button));
            self.pressed = None;
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
            self.selection_click = None;
            self.selection_drag = None;
            self.hyperlink_click = None;
            self.scroll_vi_cursor();
            self.update_hovered_hyperlink();
            self.window.request_redraw();
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

    pub(super) fn copy_last_output(&self, copy_pending: &mut bool) {
        if *copy_pending {
            return;
        }
        let view = self.session.screen().view();
        let Some(range) = view.last_command_output() else {
            return;
        };
        let Ok(text) = view.selection_text(range) else {
            return;
        };
        let text = text.to_string();
        if text.is_empty() {
            return;
        }
        match clipboard::write(text, self.proxy.clone()) {
            Ok(()) => *copy_pending = true,
            Err(error) => log::warn!("Failed to start the clipboard copy worker: {error}"),
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
            self.hyperlink_click = None;
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
        self.invalidate_overlay();
    }

    pub(super) fn clear_selection(&mut self) {
        self.selection_click = None;
        self.selection_drag = None;
        if self.selection.take().is_some() {
            self.invalidate_overlay();
        }
    }

    pub(super) fn copy(&self, copy_pending: &mut bool) {
        if *copy_pending {
            return;
        }
        if let Some(text) = self.selected_text() {
            match clipboard::write(text, self.proxy.clone()) {
                Ok(()) => *copy_pending = true,
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

    fn publish_primary_selection(&self, primary: &mut Option<clipboard::Primary>) {
        if let Some(text) = self.selected_text() {
            publish_primary_text(text, primary);
        }
    }

    pub(super) fn paste(&mut self, clipboard: ClipboardType, primary: Option<&clipboard::Primary>) {
        if self.paste_worker || self.inputs.len() >= INPUT_LIMIT {
            return;
        }
        match clipboard::read(clipboard, primary, self.id, self.proxy.clone()) {
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

fn publish_primary_text(text: String, primary: &mut Option<clipboard::Primary>) {
    if primary.is_none() {
        *primary = match clipboard::Primary::new() {
            Ok(primary) => Some(primary),
            Err(error) => {
                log::warn!("Failed to start the primary selection worker: {error}");
                return;
            }
        };
    }
    if let Some(primary) = primary {
        primary.publish(text);
    }
}
