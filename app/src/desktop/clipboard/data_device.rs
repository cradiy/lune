//! Core Wayland `wl_data_device_manager` and `zwp_primary_selection_v1`
//! clipboard backend.
//!
//! Runs on a dedicated worker thread attached as a guest queue to winit's
//! `wl_display`, so regular clipboard and primary selection offers and
//! transfers work on every Wayland compositor (including compositors that do
//! not advertise `ext_data_control_manager_v1` or `zwlr_data_control_manager_v1`)
//! even while the terminal window is unfocused or idle.

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use wayland_client::backend::{Backend, WaylandError};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_callback, wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source,
    wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_touch,
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop, event_created_child,
};
use wayland_protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_manager_v1 as primary_manager,
    zwp_primary_selection_device_v1 as primary_device,
    zwp_primary_selection_offer_v1 as primary_offer,
    zwp_primary_selection_source_v1 as primary_source,
};
use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
use winit::window::Window;

use super::{ClipboardType, Primary, transfer};

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
    commands: mpsc::SyncSender<Command>,
    wake: Arc<OwnedFd>,
    has_data_control: bool,
    has_primary_selection: bool,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Clone)]
pub(in crate::desktop) struct DataDeviceHandle {
    commands: mpsc::SyncSender<Command>,
    wake: Arc<OwnedFd>,
    has_data_control: bool,
    has_primary_selection: bool,
}

#[derive(Clone, Copy)]
struct Capabilities {
    has_data_control: bool,
    has_primary_selection: bool,
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
    WriteRegular {
        text: String,
        reply: mpsc::SyncSender<WriteOutcome>,
    },
    WritePrimary {
        text: String,
        fallback: Option<Primary>,
    },
    Read {
        clipboard: ClipboardType,
        reply: mpsc::SyncSender<ReadOutcome>,
    },
}

impl DataDevice {
    /// Attaches a `wl_data_device_manager` (and optional `zwp_primary_selection_v1`)
    /// clipboard worker to `window`'s Wayland display. Returns `None` if `window`
    /// is not on Wayland or the compositor does not expose `wl_data_device_manager`.
    pub(in crate::desktop) fn attach(window: &Window) -> Option<Self> {
        let display_handle = window.display_handle().ok()?;
        let RawDisplayHandle::Wayland(wayland_display) = display_handle.as_raw() else {
            return None;
        };
        let display = SendDisplay(wayland_display.display.as_ptr());

        let (wake_read, wake_write) = match create_nonblocking_pipe() {
            Ok(pipe) => pipe,
            Err(error) => {
                log::warn!("Could not create Wayland clipboard worker wakeup: {error}");
                return None;
            }
        };
        let wake = Arc::new(wake_write);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let (commands, commands_rx) = mpsc::sync_channel::<Command>(32);
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<Capabilities, String>>(1);

        let thread = match thread::Builder::new()
            .name("lune-clipboard".into())
            .spawn(move || {
                run_worker(display, commands_rx, wake_read, worker_stop, ready_tx);
            }) {
            Ok(thread) => thread,
            Err(error) => {
                log::warn!("Could not start Wayland data-device clipboard worker: {error}");
                return None;
            }
        };

        match ready_rx.recv_timeout(STARTUP_TIMEOUT) {
            Ok(Ok(caps)) => {
                log::debug!(
                    "Wayland wl_data_device_manager clipboard ready (data_control={}, primary_selection={})",
                    caps.has_data_control,
                    caps.has_primary_selection
                );
                Some(Self {
                    commands,
                    wake,
                    has_data_control: caps.has_data_control,
                    has_primary_selection: caps.has_primary_selection,
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
                stop.store(true, Ordering::Release);
                signal_wake(&wake);
                let _ = thread.join();
                None
            }
        }
    }

    /// Wakes the worker thread so it dispatches any guest queue events that
    /// winit read off the shared Wayland display socket.
    pub(in crate::desktop) fn wake(&self) {
        signal_wake(&self.wake);
    }

    pub(in crate::desktop) fn handle(&self) -> DataDeviceHandle {
        DataDeviceHandle {
            commands: self.commands.clone(),
            wake: self.wake.clone(),
            has_data_control: self.has_data_control,
            has_primary_selection: self.has_primary_selection,
        }
    }
}

impl Drop for DataDevice {
    fn drop(&mut self) {
        // Wake the worker before joining it so the guest backend is dropped
        // before the window releases wl_display.
        self.stop.store(true, Ordering::Release);
        signal_wake(&self.wake);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl DataDeviceHandle {
    pub(in crate::desktop) fn has_data_control(&self) -> bool {
        self.has_data_control
    }

    pub(in crate::desktop) fn has_primary_selection(&self) -> bool {
        self.has_primary_selection
    }

    pub(in crate::desktop) fn publish_primary(
        &self,
        text: String,
        fallback: Option<Primary>,
    ) -> bool {
        if !self.has_primary_selection {
            return false;
        }
        if self
            .commands
            .try_send(Command::WritePrimary { text, fallback })
            .is_ok()
        {
            signal_wake(&self.wake);
            true
        } else {
            false
        }
    }

    pub(super) fn write(&self, text: String, timeout: Duration) -> WriteOutcome {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        if self
            .commands
            .send(Command::WriteRegular {
                text,
                reply: reply_tx,
            })
            .is_err()
        {
            return WriteOutcome::Fallback;
        }
        signal_wake(&self.wake);
        reply_rx
            .recv_timeout(timeout)
            .unwrap_or(WriteOutcome::Fallback)
    }

    pub(super) fn read(&self, clipboard: ClipboardType, deadline: Instant) -> ReadOutcome {
        if clipboard == ClipboardType::Primary && !self.has_primary_selection {
            return ReadOutcome::Fallback;
        }
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        if self
            .commands
            .send(Command::Read {
                clipboard,
                reply: reply_tx,
            })
            .is_err()
        {
            return ReadOutcome::Fallback;
        }
        signal_wake(&self.wake);
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
    write_id: u64,
    text: Arc<String>,
}

struct WriteSync(u64);

enum PendingWrite {
    Regular(mpsc::SyncSender<WriteOutcome>),
    Primary {
        text: Arc<String>,
        fallback: Option<Primary>,
    },
}

struct SeatState {
    seat: wl_seat::WlSeat,
    device: wl_data_device::WlDataDevice,
    primary_device: Option<primary_device::ZwpPrimarySelectionDeviceV1>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    touch: Option<wl_touch::WlTouch>,
    focused: bool,
    last_serial: Option<u32>,
    selection_offer: Option<wl_data_offer::WlDataOffer>,
    dnd_offer: Option<wl_data_offer::WlDataOffer>,
    active_source: Option<(wl_data_source::WlDataSource, Arc<String>, u64)>,
    primary_offer: Option<primary_offer::ZwpPrimarySelectionOfferV1>,
    active_primary_source: Option<(
        primary_source::ZwpPrimarySelectionSourceV1,
        Arc<String>,
        u64,
    )>,
}

impl SeatState {
    fn destroy_all(mut self) {
        if let Some(offer) = self.selection_offer.take() {
            offer.destroy();
        }
        if let Some(offer) = self.dnd_offer.take() {
            offer.destroy();
        }
        if let Some((source, _, _)) = self.active_source.take() {
            source.destroy();
        }
        if let Some(offer) = self.primary_offer.take() {
            offer.destroy();
        }
        if let Some((source, _, _)) = self.active_primary_source.take() {
            source.destroy();
        }
        if let Some(primary) = self.primary_device.take() {
            primary.destroy();
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
    primary_manager: Option<primary_manager::ZwpPrimarySelectionDeviceManagerV1>,
    seats: HashMap<u32, SeatState>,
    active_seat: Option<u32>,
    has_data_control: bool,
    pending_commands: VecDeque<Command>,
    pending_writes: HashMap<u64, PendingWrite>,
    next_write_id: u64,
    transfers: Arc<()>,
}

impl WorkerState {
    fn add_seat(&mut self, registry: &wl_registry::WlRegistry, name: u32, version: u32) {
        let seat = registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(7), &self.handle, name);
        let device = self.manager.get_data_device(&seat, &self.handle, name);
        let primary_device = self
            .primary_manager
            .as_ref()
            .map(|manager| manager.get_device(&seat, &self.handle, name));
        self.seats.insert(
            name,
            SeatState {
                seat,
                device,
                primary_device,
                keyboard: None,
                pointer: None,
                touch: None,
                focused: false,
                last_serial: None,
                selection_offer: None,
                dnd_offer: None,
                active_source: None,
                primary_offer: None,
                active_primary_source: None,
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
        for (_, pending) in self.pending_writes.drain() {
            if let PendingWrite::Regular(reply) = pending {
                let _ = reply.send(WriteOutcome::Fallback);
            }
        }
        for (_, seat) in self.seats.drain() {
            seat.destroy_all();
        }
        if let Some(manager) = self.primary_manager.take() {
            manager.destroy();
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

    fn pick_read_seat(&self, clipboard: ClipboardType) -> Option<u32> {
        let has_data = |seat: &SeatState| match clipboard {
            ClipboardType::Regular => {
                seat.active_source.is_some() || seat.selection_offer.is_some()
            }
            ClipboardType::Primary => {
                seat.active_primary_source.is_some() || seat.primary_offer.is_some()
            }
        };
        if let Some(name) = self.active_seat
            && let Some(seat) = self.seats.get(&name)
            && (has_data(seat) || seat.focused)
        {
            return Some(name);
        }
        self.seats
            .iter()
            .find(|(_, seat)| has_data(seat))
            .or_else(|| self.seats.iter().find(|(_, seat)| seat.focused))
            .map(|(&name, _)| name)
            .or(self.active_seat)
    }

    fn allocate_write_id(&mut self) -> u64 {
        let id = self.next_write_id;
        self.next_write_id = self.next_write_id.wrapping_add(1);
        id
    }

    fn supersede_pending_write(&mut self, write_id: u64) {
        if let Some(PendingWrite::Regular(reply)) = self.pending_writes.remove(&write_id) {
            let _ = reply.send(WriteOutcome::Published);
        }
    }

    fn process_commands(&mut self) {
        while let Some(command) = self.pending_commands.pop_front() {
            match command {
                Command::WriteRegular { text, reply } => {
                    self.handle_write_regular(text, reply);
                }
                Command::WritePrimary { text, fallback } => {
                    self.handle_write_primary(text, fallback);
                }
                Command::Read { clipboard, reply } => {
                    let outcome = self.handle_read(clipboard);
                    let _ = reply.send(outcome);
                }
            }
        }
    }

    fn handle_write_regular(&mut self, text: String, reply: mpsc::SyncSender<WriteOutcome>) {
        let Some(seat_name) = self.pick_write_seat() else {
            let _ = reply.send(WriteOutcome::Fallback);
            return;
        };
        let Some(serial) = self.seats.get(&seat_name).and_then(|seat| seat.last_serial) else {
            let _ = reply.send(WriteOutcome::Fallback);
            return;
        };
        let prev_write_id = self
            .seats
            .get(&seat_name)
            .and_then(|seat| seat.active_source.as_ref().map(|(_, _, id)| *id));
        if let Some(prev_id) = prev_write_id {
            self.supersede_pending_write(prev_id);
        }
        let write_id = self.allocate_write_id();
        let text = Arc::new(text);
        let source = self.manager.create_data_source(
            &self.handle,
            SourceData {
                seat_name,
                write_id,
                text: text.clone(),
            },
        );
        for mime in OFFERED_TEXT_MIMES {
            source.offer((*mime).into());
        }
        let Some(seat) = self.seats.get_mut(&seat_name) else {
            source.destroy();
            let _ = reply.send(WriteOutcome::Fallback);
            return;
        };
        seat.device.set_selection(Some(&source), serial);
        seat.active_source = Some((source, text, write_id));
        self.pending_writes
            .insert(write_id, PendingWrite::Regular(reply));
        // Confirm compositor acceptance: any immediate Cancelled from a rejected
        // serial arrives on this queue before wl_display.sync completes.
        self.connection
            .display()
            .sync(&self.handle, WriteSync(write_id));
    }

    fn handle_write_primary(&mut self, text: String, fallback: Option<Primary>) {
        let Some(seat_name) = self.pick_write_seat() else {
            if let Some(primary) = fallback {
                primary.publish(text);
            }
            return;
        };
        let has_primary = self.primary_manager.is_some()
            && self
                .seats
                .get(&seat_name)
                .is_some_and(|seat| seat.primary_device.is_some());
        let serial = self.seats.get(&seat_name).and_then(|seat| seat.last_serial);
        let (true, Some(serial)) = (has_primary, serial) else {
            if let Some(primary) = fallback {
                primary.publish(text);
            }
            return;
        };
        let prev_write_id = self
            .seats
            .get(&seat_name)
            .and_then(|seat| seat.active_primary_source.as_ref().map(|(_, _, id)| *id));
        if let Some(prev_id) = prev_write_id {
            self.supersede_pending_write(prev_id);
        }
        let write_id = self.allocate_write_id();
        let text = Arc::new(text);
        let Some(manager) = self.primary_manager.as_ref() else {
            if let Some(primary) = fallback {
                primary.publish(text.as_ref().clone());
            }
            return;
        };
        let source = manager.create_source(
            &self.handle,
            SourceData {
                seat_name,
                write_id,
                text: text.clone(),
            },
        );
        for mime in OFFERED_TEXT_MIMES {
            source.offer((*mime).into());
        }
        let Some(device) = self
            .seats
            .get(&seat_name)
            .and_then(|seat| seat.primary_device.as_ref())
        else {
            source.destroy();
            if let Some(primary) = fallback {
                primary.publish(text.as_ref().clone());
            }
            return;
        };
        device.set_selection(Some(&source), serial);
        if let Some(seat) = self.seats.get_mut(&seat_name) {
            seat.active_primary_source = Some((source, text.clone(), write_id));
        }
        if fallback.is_some() {
            self.pending_writes
                .insert(write_id, PendingWrite::Primary { text, fallback });
            self.connection
                .display()
                .sync(&self.handle, WriteSync(write_id));
        }
    }

    fn handle_read(&mut self, clipboard: ClipboardType) -> ReadOutcome {
        let Some(seat_name) = self.pick_read_seat(clipboard) else {
            return if self.has_data_control {
                ReadOutcome::Fallback
            } else {
                ReadOutcome::Empty
            };
        };
        let Some(seat) = self.seats.get(&seat_name) else {
            return ReadOutcome::Empty;
        };

        match clipboard {
            ClipboardType::Regular => {
                // Self-paste: return owned selection text directly without a pipe roundtrip.
                if let Some((_, text, _)) = &seat.active_source {
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
            }
            ClipboardType::Primary => {
                if self.primary_manager.is_none() {
                    return ReadOutcome::Fallback;
                }
                if let Some((_, text, _)) = &seat.active_primary_source {
                    return ReadOutcome::Text(text.as_ref().clone());
                }
                if let Some(offer) = &seat.primary_offer {
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
            }
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
    commands_rx: mpsc::Receiver<Command>,
    wake_read: OwnedFd,
    stop: Arc<AtomicBool>,
    ready_tx: mpsc::SyncSender<Result<Capabilities, String>>,
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

    let primary_manager = globals
        .bind::<primary_manager::ZwpPrimarySelectionDeviceManagerV1, _, _>(&handle, 1..=1, ())
        .ok();
    let has_primary_selection = primary_manager.is_some();

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
        primary_manager,
        seats: HashMap::new(),
        active_seat: None,
        has_data_control,
        pending_commands: VecDeque::new(),
        pending_writes: HashMap::new(),
        next_write_id: 1,
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

    if ready_tx
        .send(Ok(Capabilities {
            has_data_control,
            has_primary_selection,
        }))
        .is_err()
    {
        state.cleanup();
        return;
    }

    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        // Dispatch all events already queued in our guest EventQueue (including
        // input/selection/source events that winit read off the shared socket)
        // before running clipboard commands so focus and serials are current.
        if let Err(error) = queue.dispatch_pending(&mut state) {
            log::debug!("Wayland data-device clipboard dispatch ended: {error}");
            break;
        }
        while let Ok(command) = commands_rx.try_recv() {
            state.pending_commands.push_back(command);
        }
        state.process_commands();

        if let Err(error) = connection.flush() {
            log::debug!("Wayland data-device clipboard flush ended: {error}");
            break;
        }
        if stop.load(Ordering::Acquire) {
            break;
        }

        // Prepare to read our guest queue (`wl_display_prepare_read_queue`). If
        // `None`, more events arrived in our guest queue concurrently (e.g. from
        // winit's main thread), so loop immediately to dispatch them.
        let Some(read_guard) = queue.prepare_read() else {
            continue;
        };

        let wl_fd = read_guard.connection_fd();
        let mut fds = [
            libc::pollfd {
                fd: wl_fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake_read.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `fds` holds two valid `pollfd` entries for live descriptors.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if ready < 0 {
            drop(read_guard);
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            log::debug!("Wayland data-device clipboard poll ended: {error}");
            break;
        }
        if fds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            drop(read_guard);
            break;
        }
        if fds[0].revents & libc::POLLIN != 0 {
            match read_guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => {
                    log::debug!("Wayland data-device clipboard read ended: {error}");
                    break;
                }
            }
        } else {
            drop(read_guard);
        }
        if fds[1].revents & libc::POLLIN != 0 {
            drain_wake(&wake_read);
        }
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
delegate_noop!(WorkerState: ignore primary_manager::ZwpPrimarySelectionDeviceManagerV1);

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
            wl_keyboard::Event::Key {
                serial,
                state: key_state,
                ..
            } if is_key_activation(key_state) => {
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
        if let wl_pointer::Event::Button {
            serial,
            state: button_state,
            ..
        } = event
            && is_button_activation(button_state)
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

trait ReplaceOffer<T> {
    fn replace_if_different(&mut self, next: Option<T>) -> Option<T>;
}

impl<T: Proxy> ReplaceOffer<T> for Option<T> {
    fn replace_if_different(&mut self, next: Option<T>) -> Option<T> {
        if self.as_ref().map(Proxy::id) == next.as_ref().map(Proxy::id) {
            return None;
        }
        std::mem::replace(self, next)
    }
}

impl Dispatch<primary_device::ZwpPrimarySelectionDeviceV1, u32> for WorkerState {
    fn event(
        state: &mut Self,
        _: &primary_device::ZwpPrimarySelectionDeviceV1,
        event: primary_device::Event,
        seat_name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(seat) = state.seats.get_mut(seat_name) else {
            return;
        };
        if let primary_device::Event::Selection { id } = event
            && let Some(previous) = seat.primary_offer.replace_if_different(id)
        {
            previous.destroy();
        }
    }

    event_created_child!(WorkerState, primary_device::ZwpPrimarySelectionDeviceV1, [
        primary_device::EVT_DATA_OFFER_OPCODE => (primary_offer::ZwpPrimarySelectionOfferV1, OfferMimes::default()),
    ]);
}

impl Dispatch<primary_offer::ZwpPrimarySelectionOfferV1, OfferMimes> for WorkerState {
    fn event(
        _: &mut Self,
        _: &primary_offer::ZwpPrimarySelectionOfferV1,
        event: primary_offer::Event,
        mimes: &OfferMimes,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let primary_offer::Event::Offer { mime_type } = event
            && let Ok(mut list) = mimes.lock()
        {
            list.push(mime_type);
        }
    }
}

impl Dispatch<primary_source::ZwpPrimarySelectionSourceV1, SourceData> for WorkerState {
    fn event(
        state: &mut Self,
        source: &primary_source::ZwpPrimarySelectionSourceV1,
        event: primary_source::Event,
        data: &SourceData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            primary_source::Event::Cancelled => {
                if let Some(seat) = state.seats.get_mut(&data.seat_name)
                    && seat
                        .active_primary_source
                        .as_ref()
                        .is_some_and(|(active, _, _)| active.id() == source.id())
                {
                    seat.active_primary_source = None;
                }
                if let Some(PendingWrite::Primary {
                    text,
                    fallback: Some(primary),
                }) = state.pending_writes.remove(&data.write_id)
                {
                    primary.publish(text.as_ref().clone());
                }
                source.destroy();
            }
            primary_source::Event::Send { mime_type, fd }
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
                        .is_some_and(|(active, _, _)| active.id() == source.id())
                {
                    seat.active_source = None;
                }
                if let Some(PendingWrite::Regular(reply)) =
                    state.pending_writes.remove(&data.write_id)
                {
                    let _ = reply.send(WriteOutcome::Fallback);
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

impl Dispatch<wl_callback::WlCallback, WriteSync> for WorkerState {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        sync: &WriteSync,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let Some(PendingWrite::Regular(reply)) = state.pending_writes.remove(&sync.0) {
            let _ = reply.send(WriteOutcome::Published);
        }
    }
}

fn is_key_activation(state: WEnum<wl_keyboard::KeyState>) -> bool {
    matches!(state, WEnum::Value(wl_keyboard::KeyState::Pressed))
}

fn is_button_activation(state: WEnum<wl_pointer::ButtonState>) -> bool {
    matches!(state, WEnum::Value(wl_pointer::ButtonState::Pressed))
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

fn create_nonblocking_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    // SAFETY: `fds` points to a writable array of two `c_int` descriptors.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `pipe2` succeeded and returned two newly created, uniquely owned
    // file descriptors.
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // SAFETY: Same as above for the write end of the pipe.
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok((read_fd, write_fd))
}

fn signal_wake(write_fd: &OwnedFd) {
    let byte = 1u8;
    // SAFETY: `write_fd` is a live non-blocking pipe descriptor; EAGAIN/EPIPE are ignored.
    let _ = unsafe { libc::write(write_fd.as_raw_fd(), (&raw const byte).cast(), 1) };
}

fn drain_wake(read_fd: &OwnedFd) {
    let mut buf = [0u8; 64];
    loop {
        // SAFETY: `read_fd` is a live non-blocking pipe descriptor and `buf` is writable.
        let count = unsafe { libc::read(read_fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if count <= 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use super::*;
    use crate::desktop::clipboard::read_pipe_text;

    #[test]
    fn only_press_events_count_as_activation_serials() {
        assert!(is_key_activation(WEnum::Value(
            wl_keyboard::KeyState::Pressed
        )));
        assert!(!is_key_activation(WEnum::Value(
            wl_keyboard::KeyState::Released
        )));
        assert!(!is_key_activation(WEnum::Value(
            wl_keyboard::KeyState::Repeated
        )));
        assert!(is_button_activation(WEnum::Value(
            wl_pointer::ButtonState::Pressed
        )));
        assert!(!is_button_activation(WEnum::Value(
            wl_pointer::ButtonState::Released
        )));
    }

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
