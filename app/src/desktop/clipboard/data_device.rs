//! Core Wayland `wl_data_device_manager` clipboard backend.
//!
//! Runs on a dedicated `calloop` worker thread attached as a guest queue to
//! winit's `wl_display`, so clipboard offers and `wl_data_source.send`
//! transfers work on every Wayland compositor (including compositors that do
//! not advertise `ext_data_control_manager_v1` or `zwlr_data_control_manager_v1`)
//! even while the terminal window is unfocused or idle.

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::io;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use calloop::channel::{self, SyncSender};
use calloop::ping::{Ping, PingSource, make_ping};
use calloop_wayland_source::WaylandSource;
use wayland_client::backend::Backend;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source, wl_keyboard, wl_pointer,
    wl_registry, wl_seat, wl_touch,
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop, event_created_child,
};
use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
use winit::window::Window;

use super::transfer;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_TRANSFERS: usize = 4;

const OFFERED_TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

const PREFERRED_TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];

/// Wrapper allowing a raw `wl_display` pointer to be passed into the worker
/// thread whose lifetime is strictly bounded by `DataDevice::drop`.
struct SendDisplay(*mut c_void);

// SAFETY: `libwayland-client` supports multi-threaded use of a `wl_display`
// when each thread operates on its own `wl_event_queue` via
// `wl_display_prepare_read_queue` / `wl_display_read_events`, and `DataDevice`
// joins the worker thread before `Client.window` (and `wl_display`) is dropped.
unsafe impl Send for SendDisplay {}

pub(in crate::desktop) struct DataDevice {
    commands: SyncSender<Command>,
    has_data_control: bool,
    stop: Ping,
    thread: Option<JoinHandle<()>>,
}

#[derive(Clone)]
pub(in crate::desktop) struct DataDeviceHandle {
    commands: SyncSender<Command>,
    has_data_control: bool,
}

pub(super) enum WriteOutcome {
    Published,
    Fallback,
}

pub(super) enum ReadOutcome {
    Text(String),
    Pipe(OwnedFd),
    Empty,
    Fallback,
}

enum Command {
    Write {
        text: String,
        reply: mpsc::SyncSender<WriteOutcome>,
    },
    Read {
        reply: mpsc::SyncSender<ReadOutcome>,
    },
}

impl DataDevice {
    /// Attaches a `wl_data_device_manager` clipboard worker to `window`'s
    /// Wayland display. Returns `None` if `window` is not on Wayland or the
    /// compositor does not expose `wl_data_device_manager`.
    pub(in crate::desktop) fn attach(window: &Window) -> Option<Self> {
        let display_handle = window.display_handle().ok()?;
        let RawDisplayHandle::Wayland(wayland_display) = display_handle.as_raw() else {
            return None;
        };
        let display = SendDisplay(wayland_display.display.as_ptr());

        let (commands, commands_rx) = channel::sync_channel::<Command>(16);
        let (stop, stop_source) = match make_ping() {
            Ok(ping) => ping,
            Err(error) => {
                log::warn!("Could not create Wayland clipboard worker wakeup: {error}");
                return None;
            }
        };
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<bool, String>>(1);

        let thread = match thread::Builder::new()
            .name("lune-clipboard".into())
            .spawn(move || {
                run_worker(display, commands_rx, stop_source, ready_tx);
            }) {
            Ok(thread) => thread,
            Err(error) => {
                log::warn!("Could not start Wayland data-device clipboard worker: {error}");
                return None;
            }
        };

        match ready_rx.recv_timeout(STARTUP_TIMEOUT) {
            Ok(Ok(has_data_control)) => {
                log::debug!(
                    "Wayland wl_data_device_manager clipboard ready (data_control={has_data_control})"
                );
                Some(Self {
                    commands,
                    has_data_control,
                    stop,
                    thread: Some(thread),
                })
            }
            Ok(Err(error)) => {
                log::debug!("Wayland wl_data_device_manager unavailable: {error}");
                let _ = thread.join();
                None
            }
            Err(_) => {
                log::warn!("Timed out initializing Wayland wl_data_device_manager clipboard");
                stop.ping();
                let _ = thread.join();
                None
            }
        }
    }

    pub(in crate::desktop) fn handle(&self) -> DataDeviceHandle {
        DataDeviceHandle {
            commands: self.commands.clone(),
            has_data_control: self.has_data_control,
        }
    }
}

impl Drop for DataDevice {
    fn drop(&mut self) {
        // Wake the worker before joining it so the guest backend is dropped
        // before the window releases wl_display.
        self.stop.ping();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl DataDeviceHandle {
    pub(super) fn has_data_control(&self) -> bool {
        self.has_data_control
    }

    pub(super) fn write(&self, text: String, timeout: Duration) -> WriteOutcome {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        if self
            .commands
            .send(Command::Write {
                text,
                reply: reply_tx,
            })
            .is_err()
        {
            return WriteOutcome::Fallback;
        }
        reply_rx
            .recv_timeout(timeout)
            .unwrap_or(WriteOutcome::Fallback)
    }

    pub(super) fn read(&self, deadline: Instant) -> ReadOutcome {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        if self
            .commands
            .send(Command::Read { reply: reply_tx })
            .is_err()
        {
            return ReadOutcome::Fallback;
        }
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            return ReadOutcome::Fallback;
        }
        reply_rx
            .recv_timeout(timeout)
            .unwrap_or(ReadOutcome::Fallback)
    }
}

type OfferMimes = Arc<Mutex<Vec<String>>>;

struct SourceData {
    seat_name: u32,
    text: Arc<String>,
}

struct SeatState {
    seat: wl_seat::WlSeat,
    device: wl_data_device::WlDataDevice,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    touch: Option<wl_touch::WlTouch>,
    focused: bool,
    last_serial: Option<u32>,
    selection_offer: Option<wl_data_offer::WlDataOffer>,
    dnd_offer: Option<wl_data_offer::WlDataOffer>,
    active_source: Option<(wl_data_source::WlDataSource, Arc<String>)>,
}

impl SeatState {
    fn destroy_all(mut self) {
        if let Some(offer) = self.selection_offer.take() {
            offer.destroy();
        }
        if let Some(offer) = self.dnd_offer.take() {
            offer.destroy();
        }
        if let Some((source, _)) = self.active_source.take() {
            source.destroy();
        }
        if let Some(keyboard) = self.keyboard.take()
            && keyboard.version() >= 3
        {
            keyboard.release();
        }
        if let Some(pointer) = self.pointer.take()
            && pointer.version() >= 3
        {
            pointer.release();
        }
        if let Some(touch) = self.touch.take()
            && touch.version() >= 3
        {
            touch.release();
        }
        if self.device.version() >= 2 {
            self.device.release();
        }
        if self.seat.version() >= 5 {
            self.seat.release();
        }
    }
}

struct WorkerState {
    connection: Connection,
    handle: QueueHandle<Self>,
    manager: wl_data_device_manager::WlDataDeviceManager,
    seats: HashMap<u32, SeatState>,
    active_seat: Option<u32>,
    has_data_control: bool,
    pending_commands: VecDeque<Command>,
    transfers: Arc<()>,
}

impl WorkerState {
    fn add_seat(&mut self, registry: &wl_registry::WlRegistry, name: u32, version: u32) {
        let seat = registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(7), &self.handle, name);
        let device = self.manager.get_data_device(&seat, &self.handle, name);
        self.seats.insert(
            name,
            SeatState {
                seat,
                device,
                keyboard: None,
                pointer: None,
                touch: None,
                focused: false,
                last_serial: None,
                selection_offer: None,
                dnd_offer: None,
                active_source: None,
            },
        );
        if self.active_seat.is_none() {
            self.active_seat = Some(name);
        }
    }

    fn remove_seat(&mut self, name: u32) {
        if let Some(seat) = self.seats.remove(&name) {
            seat.destroy_all();
        }
        if self.active_seat == Some(name) {
            self.active_seat = self.seats.keys().next().copied();
        }
    }

    fn cleanup(&mut self) {
        self.pending_commands.clear();
        for (_, seat) in self.seats.drain() {
            seat.destroy_all();
        }
        let _ = self.connection.flush();
    }

    fn pick_write_seat(&self) -> Option<u32> {
        // Prefer the currently focused seat with a valid input serial.
        if let Some(name) = self.active_seat
            && let Some(seat) = self.seats.get(&name)
            && seat.focused
            && seat.last_serial.is_some()
        {
            return Some(name);
        }
        if let Some((&name, _)) = self
            .seats
            .iter()
            .find(|(_, seat)| seat.focused && seat.last_serial.is_some())
        {
            return Some(name);
        }
        // On compositors without data-control, still try the most recent seat
        // that has observed a serial (for example, right after a global menu click).
        if !self.has_data_control {
            if let Some(name) = self.active_seat
                && self
                    .seats
                    .get(&name)
                    .is_some_and(|seat| seat.last_serial.is_some())
            {
                return Some(name);
            }
            return self
                .seats
                .iter()
                .find(|(_, seat)| seat.last_serial.is_some())
                .map(|(&name, _)| name);
        }
        None
    }

    fn pick_read_seat(&self) -> Option<u32> {
        if let Some(name) = self.active_seat
            && let Some(seat) = self.seats.get(&name)
            && (seat.active_source.is_some() || seat.selection_offer.is_some() || seat.focused)
        {
            return Some(name);
        }
        self.seats
            .iter()
            .find(|(_, seat)| seat.active_source.is_some() || seat.selection_offer.is_some())
            .or_else(|| self.seats.iter().find(|(_, seat)| seat.focused))
            .map(|(&name, _)| name)
            .or(self.active_seat)
    }

    fn process_commands(&mut self) {
        if self.pending_commands.is_empty() {
            return;
        }
        // If libwayland has unread events already queued for our guest event
        // queue (such as the wl_keyboard.key or wl_data_device.selection that
        // triggered this command), let WaylandSource dispatch them first so
        // `last_serial` and `selection_offer` are completely up to date.
        let Some(guard) = self.connection.prepare_read() else {
            return;
        };
        drop(guard);

        while let Some(command) = self.pending_commands.pop_front() {
            match command {
                Command::Write { text, reply } => {
                    let outcome = self.handle_write(text);
                    let _ = reply.send(outcome);
                }
                Command::Read { reply } => {
                    let outcome = self.handle_read();
                    let _ = reply.send(outcome);
                }
            }
        }
        let _ = self.connection.flush();
    }

    fn handle_write(&mut self, text: String) -> WriteOutcome {
        let Some(seat_name) = self.pick_write_seat() else {
            return WriteOutcome::Fallback;
        };
        let Some(seat) = self.seats.get_mut(&seat_name) else {
            return WriteOutcome::Fallback;
        };
        let Some(serial) = seat.last_serial else {
            return WriteOutcome::Fallback;
        };
        let text = Arc::new(text);
        let source = self.manager.create_data_source(
            &self.handle,
            SourceData {
                seat_name,
                text: text.clone(),
            },
        );
        for mime in OFFERED_TEXT_MIMES {
            source.offer((*mime).into());
        }
        seat.device.set_selection(Some(&source), serial);
        seat.active_source = Some((source, text));
        WriteOutcome::Published
    }

    fn handle_read(&mut self) -> ReadOutcome {
        let Some(seat_name) = self.pick_read_seat() else {
            return if self.has_data_control {
                ReadOutcome::Fallback
            } else {
                ReadOutcome::Empty
            };
        };
        let Some(seat) = self.seats.get(&seat_name) else {
            return ReadOutcome::Empty;
        };

        // Self-paste: return owned selection text directly without a pipe roundtrip.
        if let Some((_, text)) = &seat.active_source {
            return ReadOutcome::Text(text.as_ref().clone());
        }

        if let Some(offer) = &seat.selection_offer {
            let offered_mimes = offer
                .data::<OfferMimes>()
                .and_then(|mimes| mimes.lock().ok().map(|guard| guard.clone()))
                .unwrap_or_default();
            let Some(mime) = select_text_mime(&offered_mimes) else {
                return ReadOutcome::Empty;
            };
            let Ok((read_fd, write_fd)) = create_pipe() else {
                return ReadOutcome::Fallback;
            };
            offer.receive(mime.to_owned(), write_fd.as_fd());
            drop(write_fd);
            return ReadOutcome::Pipe(read_fd);
        }

        if self.has_data_control && !seat.focused {
            ReadOutcome::Fallback
        } else {
            ReadOutcome::Empty
        }
    }
}

fn run_worker(
    display: SendDisplay,
    commands_rx: channel::Channel<Command>,
    stop_source: PingSource,
    ready_tx: mpsc::SyncSender<Result<bool, String>>,
) {
    // SAFETY: `display.0` is a live `*mut wl_display` owned by winit's active
    // event loop, and `DataDevice::drop` joins this thread before `Client.window`
    // is dropped.
    let backend = unsafe { Backend::from_foreign_display(display.0.cast()) };
    let connection = Connection::from_backend(backend);

    let (globals, mut queue) = match registry_queue_init::<WorkerState>(&connection) {
        Ok(init) => init,
        Err(error) => {
            let _ = ready_tx.send(Err(error.to_string()));
            return;
        }
    };
    let handle = queue.handle();

    let manager =
        match globals.bind::<wl_data_device_manager::WlDataDeviceManager, _, _>(&handle, 1..=3, ())
        {
            Ok(manager) => manager,
            Err(error) => {
                let _ = ready_tx.send(Err(error.to_string()));
                return;
            }
        };

    let has_data_control = globals.contents().with_list(|list| {
        list.iter().any(|global| {
            global.interface == "ext_data_control_manager_v1"
                || global.interface == "zwlr_data_control_manager_v1"
        })
    });

    let mut state = WorkerState {
        connection: connection.clone(),
        handle,
        manager,
        seats: HashMap::new(),
        active_seat: None,
        has_data_control,
        pending_commands: VecDeque::new(),
        transfers: Arc::new(()),
    };

    globals.contents().with_list(|list| {
        for global in list {
            if global.interface == wl_seat::WlSeat::interface().name {
                state.add_seat(globals.registry(), global.name, global.version);
            }
        }
    });

    // Complete seat capability discovery before winit's main loop starts polling.
    if let Err(error) = queue.roundtrip(&mut state) {
        state.cleanup();
        let _ = ready_tx.send(Err(error.to_string()));
        return;
    }

    let mut event_loop = match calloop::EventLoop::<WorkerState>::try_new() {
        Ok(event_loop) => event_loop,
        Err(error) => {
            state.cleanup();
            let _ = ready_tx.send(Err(error.to_string()));
            return;
        }
    };
    let signal = event_loop.get_signal();
    if let Err(error) =
        event_loop
            .handle()
            .insert_source(stop_source, move |(), (), _: &mut WorkerState| {
                signal.stop();
            })
    {
        state.cleanup();
        let _ = ready_tx.send(Err(error.to_string()));
        return;
    }
    if let Err(error) =
        event_loop
            .handle()
            .insert_source(commands_rx, |event, (), state: &mut WorkerState| {
                if let channel::Event::Msg(command) = event {
                    state.pending_commands.push_back(command);
                }
            })
    {
        state.cleanup();
        let _ = ready_tx.send(Err(error.to_string()));
        return;
    }
    if let Err(error) = WaylandSource::new(connection, queue).insert(event_loop.handle()) {
        state.cleanup();
        let _ = ready_tx.send(Err(error.to_string()));
        return;
    }

    if ready_tx.send(Ok(has_data_control)).is_err() {
        state.cleanup();
        return;
    }

    if let Err(error) = event_loop.run(None, &mut state, WorkerState::process_commands) {
        log::debug!("Wayland data-device clipboard loop ended: {error}");
    }
    state.cleanup();
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WorkerState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == wl_seat::WlSeat::interface().name => {
                state.add_seat(registry, name, version);
            }
            wl_registry::Event::GlobalRemove { name } => state.remove_seat(name),
            _ => {}
        }
    }
}

delegate_noop!(WorkerState: ignore wl_data_device_manager::WlDataDeviceManager);

impl Dispatch<wl_seat::WlSeat, u32> for WorkerState {
    fn event(
        state: &mut Self,
        seat_proxy: &wl_seat::WlSeat,
        event: wl_seat::Event,
        seat_name: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        else {
            return;
        };
        let Some(seat) = state.seats.get_mut(seat_name) else {
            return;
        };

        let has_keyboard = caps.contains(wl_seat::Capability::Keyboard);
        if has_keyboard && seat.keyboard.is_none() {
            seat.keyboard = Some(seat_proxy.get_keyboard(qh, *seat_name));
        } else if !has_keyboard && let Some(keyboard) = seat.keyboard.take() {
            seat.focused = false;
            if keyboard.version() >= 3 {
                keyboard.release();
            }
        }

        let has_pointer = caps.contains(wl_seat::Capability::Pointer);
        if has_pointer && seat.pointer.is_none() {
            seat.pointer = Some(seat_proxy.get_pointer(qh, *seat_name));
        } else if !has_pointer
            && let Some(pointer) = seat.pointer.take()
            && pointer.version() >= 3
        {
            pointer.release();
        }

        let has_touch = caps.contains(wl_seat::Capability::Touch);
        if has_touch && seat.touch.is_none() {
            seat.touch = Some(seat_proxy.get_touch(qh, *seat_name));
        } else if !has_touch
            && let Some(touch) = seat.touch.take()
            && touch.version() >= 3
        {
            touch.release();
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, u32> for WorkerState {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        seat_name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(seat) = state.seats.get_mut(seat_name) else {
            return;
        };
        match event {
            wl_keyboard::Event::Enter { serial, .. } => {
                seat.focused = true;
                seat.last_serial = Some(serial);
                state.active_seat = Some(*seat_name);
            }
            wl_keyboard::Event::Leave { .. } => {
                seat.focused = false;
            }
            wl_keyboard::Event::Key { serial, .. } => {
                seat.last_serial = Some(serial);
                state.active_seat = Some(*seat_name);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, u32> for WorkerState {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        seat_name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_pointer::Event::Button { serial, .. } = event
            && let Some(seat) = state.seats.get_mut(seat_name)
        {
            seat.last_serial = Some(serial);
            state.active_seat = Some(*seat_name);
        }
    }
}

impl Dispatch<wl_touch::WlTouch, u32> for WorkerState {
    fn event(
        state: &mut Self,
        _: &wl_touch::WlTouch,
        event: wl_touch::Event,
        seat_name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_touch::Event::Down { serial, .. } = event
            && let Some(seat) = state.seats.get_mut(seat_name)
        {
            seat.last_serial = Some(serial);
            state.active_seat = Some(*seat_name);
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, u32> for WorkerState {
    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        seat_name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(seat) = state.seats.get_mut(seat_name) else {
            return;
        };
        match event {
            wl_data_device::Event::Selection { id } => {
                if let Some(previous) = seat.selection_offer.replace_if_different(id) {
                    previous.destroy();
                }
            }
            wl_data_device::Event::Enter { id, .. } => {
                if let Some(previous) = seat.dnd_offer.replace_if_different(id) {
                    previous.destroy();
                }
            }
            wl_data_device::Event::Leave => {
                if let Some(previous) = seat.dnd_offer.take() {
                    previous.destroy();
                }
            }
            _ => {}
        }
    }

    event_created_child!(WorkerState, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, OfferMimes::default()),
    ]);
}

trait ReplaceOffer {
    fn replace_if_different(
        &mut self,
        next: Option<wl_data_offer::WlDataOffer>,
    ) -> Option<wl_data_offer::WlDataOffer>;
}

impl ReplaceOffer for Option<wl_data_offer::WlDataOffer> {
    fn replace_if_different(
        &mut self,
        next: Option<wl_data_offer::WlDataOffer>,
    ) -> Option<wl_data_offer::WlDataOffer> {
        if self.as_ref().map(Proxy::id) == next.as_ref().map(Proxy::id) {
            return None;
        }
        std::mem::replace(self, next)
    }
}

impl Dispatch<wl_data_offer::WlDataOffer, OfferMimes> for WorkerState {
    fn event(
        _: &mut Self,
        _: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        mimes: &OfferMimes,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event
            && let Ok(mut list) = mimes.lock()
        {
            list.push(mime_type);
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, SourceData> for WorkerState {
    fn event(
        state: &mut Self,
        source: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        data: &SourceData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Cancelled => {
                if let Some(seat) = state.seats.get_mut(&data.seat_name)
                    && seat
                        .active_source
                        .as_ref()
                        .is_some_and(|(active, _)| active.id() == source.id())
                {
                    seat.active_source = None;
                }
                source.destroy();
            }
            wl_data_source::Event::Send { mime_type, fd }
                if is_offered_text_mime(&mime_type)
                    && Arc::strong_count(&state.transfers) <= MAX_TRANSFERS =>
            {
                let text = data.text.clone();
                let permit = state.transfers.clone();
                let _ = thread::Builder::new()
                    .name("lune-clipboard-send".into())
                    .spawn(move || {
                        let _permit = permit;
                        let _ = transfer(fd, text.as_bytes());
                    });
            }
            _ => {}
        }
    }
}

fn is_offered_text_mime(mime: &str) -> bool {
    OFFERED_TEXT_MIMES.contains(&mime) || is_utf8_plain_text(mime)
}

fn is_utf8_plain_text(mime: &str) -> bool {
    let mut parts = mime.split(';');
    let Some(base) = parts.next() else {
        return false;
    };
    if !base.trim().eq_ignore_ascii_case("text/plain") {
        return false;
    }
    parts.any(|param| {
        param.split_once('=').is_some_and(|(key, value)| {
            key.trim().eq_ignore_ascii_case("charset")
                && value.trim().trim_matches('"').eq_ignore_ascii_case("utf-8")
        })
    })
}

fn select_text_mime(offered: &[String]) -> Option<&str> {
    for preferred in PREFERRED_TEXT_MIMES {
        if let Some(found) = offered.iter().find(|mime| mime.as_str() == *preferred) {
            return Some(found.as_str());
        }
    }
    if let Some(found) = offered.iter().find(|mime| is_utf8_plain_text(mime)) {
        return Some(found.as_str());
    }
    offered
        .iter()
        .find(|mime| mime.trim().eq_ignore_ascii_case("text/plain"))
        .map(String::as_str)
}

fn create_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    // SAFETY: `fds` points to a writable array of two `c_int` descriptors.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `pipe2` succeeded and returned two newly created, uniquely owned
    // file descriptors.
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // SAFETY: Same as above for the write end of the pipe.
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok((read_fd, write_fd))
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use super::*;
    use crate::desktop::clipboard::read_pipe_text;

    #[test]
    fn select_text_mime_prefers_utf8_and_ignores_non_text() {
        let offered = vec![
            "image/png".to_owned(),
            "text/plain".to_owned(),
            "text/plain;charset=utf-8".to_owned(),
        ];
        assert_eq!(select_text_mime(&offered), Some("text/plain;charset=utf-8"));

        let xwayland = vec!["STRING".to_owned(), "UTF8_STRING".to_owned()];
        assert_eq!(select_text_mime(&xwayland), Some("UTF8_STRING"));

        let spaced_utf8 = vec!["text/plain; charset=UTF-8".to_owned()];
        assert_eq!(
            select_text_mime(&spaced_utf8),
            Some("text/plain; charset=UTF-8")
        );

        let image_only = vec!["image/png".to_owned(), "application/pdf".to_owned()];
        assert_eq!(select_text_mime(&image_only), None);
    }

    #[test]
    fn pipe_transfer_and_read_enforce_utf8_control_stripping_and_limit() {
        let (read_fd, write_fd) = create_pipe().unwrap();
        let writer = thread::spawn(move || transfer(write_fd, b"hello\x07\tworld\n"));
        let deadline = Instant::now() + Duration::from_secs(2);
        let text = read_pipe_text(File::from(read_fd), deadline, 64, true).unwrap();
        writer.join().unwrap().unwrap();
        assert_eq!(text, "hello\tworld\n");

        let (read_fd, write_fd) = create_pipe().unwrap();
        let writer = thread::spawn(move || transfer(write_fd, b"0123456789"));
        let err = read_pipe_text(File::from(read_fd), deadline, 4, false).unwrap_err();
        assert!(err.contains("size limit"));
        let _ = writer.join();
    }
}
