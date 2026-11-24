use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Instant;

use calloop::channel::{self, SyncSender};
use calloop_wayland_source::WaylandSource;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_callback, wl_registry, wl_seat};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop, event_created_child,
};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1 as device, ext_data_control_manager_v1 as manager,
    ext_data_control_offer_v1 as offer, ext_data_control_source_v1 as source,
};

use super::{PASTE_TIMEOUT, wait_fd};

const MIME_TYPES: &[&str] = &["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"];
const MAX_TRANSFERS: usize = 4;

pub(in crate::desktop) struct Primary {
    commands: SyncSender<()>,
    queue: Arc<Publications>,
}

#[derive(Default)]
struct Publications {
    state: Mutex<Pending>,
    ready: Condvar,
}

#[derive(Default)]
struct Pending {
    latest: u64,
    confirmed: u64,
    next: Option<(u64, String)>,
    closed: bool,
}

pub(super) struct Publication {
    queue: Arc<Publications>,
    id: u64,
}

impl Primary {
    pub(in crate::desktop) fn new() -> io::Result<Self> {
        let (commands, receiver) = channel::sync_channel(1);
        let queue = Arc::new(Publications::default());
        let worker_queue = queue.clone();
        thread::Builder::new()
            .name("lune-primary".into())
            .spawn(move || {
                let result = Connection::connect_to_env()
                    .map_err(|error| error.to_string())
                    .and_then(|connection| {
                        run(connection, receiver, worker_queue.clone())
                            .map_err(|error| error.to_string())
                    });
                {
                    let mut pending = worker_queue.state.lock().unwrap();
                    pending.closed = true;
                    pending.next = None;
                    worker_queue.ready.notify_all();
                }
                if let Err(error) = result {
                    log::warn!("Primary selection worker stopped: {error}");
                }
            })?;
        Ok(Self { commands, queue })
    }

    pub(in crate::desktop) fn publish(&self, text: String) {
        let mut pending = self.queue.state.lock().unwrap();
        if !pending.closed {
            pending.latest += 1;
            pending.next = Some((pending.latest, text));
            let _ = self.commands.try_send(());
        }
    }

    pub(super) fn publication(&self) -> Option<Publication> {
        let pending = self.queue.state.lock().unwrap();
        (!pending.closed && pending.latest != pending.confirmed).then(|| Publication {
            queue: self.queue.clone(),
            id: pending.latest,
        })
    }
}

impl Publication {
    pub(super) fn wait(&self, deadline: Instant) -> bool {
        let pending = self.queue.state.lock().unwrap();
        let (pending, _) = self
            .queue
            .ready
            .wait_timeout_while(
                pending,
                deadline.saturating_duration_since(Instant::now()),
                |pending| !pending.closed && pending.confirmed < self.id,
            )
            .unwrap();
        pending.closed || pending.confirmed >= self.id
    }
}

type Device = device::ExtDataControlDeviceV1;

struct SelectionDevice {
    seat: wl_seat::WlSeat,
    proxy: Device,
    supports_primary: bool,
}

struct Writer {
    queue: Arc<Publications>,
    manager: manager::ExtDataControlManagerV1,
    devices: HashMap<u32, SelectionDevice>,
    handle: QueueHandle<Self>,
    connection: Connection,
    transfers: Arc<()>,
    in_flight: bool,
}

fn run(
    connection: Connection,
    commands: channel::Channel<()>,
    queue: Arc<Publications>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (globals, mut events) = registry_queue_init::<Writer>(&connection)?;
    let handle = events.handle();
    let Ok(manager) = globals.bind(&handle, 1..=1, ()) else {
        return Ok(());
    };
    let mut writer = Writer {
        queue,
        manager,
        devices: HashMap::new(),
        connection: connection.clone(),
        handle,
        transfers: Arc::new(()),
        in_flight: false,
    };
    globals.contents().with_list(|globals_list| {
        for global in globals_list {
            if global.interface == wl_seat::WlSeat::interface().name {
                writer.add_seat(globals.registry(), global.name, global.version);
            }
        }
    });
    events.roundtrip(&mut writer)?;
    if !writer
        .devices
        .values()
        .any(|device| device.supports_primary)
    {
        return Ok(());
    }

    let mut event_loop = calloop::EventLoop::try_new()?;
    let signal = event_loop.get_signal();
    event_loop
        .handle()
        .insert_source(commands, move |event, (), _: &mut Writer| {
            if matches!(event, channel::Event::Closed) {
                signal.stop();
            }
        })?;
    WaylandSource::new(connection, events).insert(event_loop.handle())?;
    event_loop.run(None, &mut writer, Writer::submit)?;
    Ok(())
}

impl Writer {
    fn add_seat(&mut self, registry: &wl_registry::WlRegistry, name: u32, version: u32) {
        let seat = registry.bind(name, version.min(5), &self.handle, ());
        let proxy = self.manager.get_data_device(&seat, &self.handle, name);
        self.devices.insert(
            name,
            SelectionDevice {
                seat,
                proxy,
                supports_primary: false,
            },
        );
    }

    fn remove_seat(&mut self, name: u32) {
        if let Some(device) = self.devices.remove(&name) {
            device.proxy.destroy();
            if device.seat.version() >= 5 {
                device.seat.release();
            }
        }
    }

    fn submit(&mut self) {
        if self.in_flight || !self.devices.values().any(|device| device.supports_primary) {
            return;
        }
        let Some((id, text)) = self.queue.state.lock().unwrap().next.take() else {
            return;
        };

        let text = Arc::new(text);
        for device in self
            .devices
            .values()
            .filter(|device| device.supports_primary)
        {
            let source = self.manager.create_data_source(&self.handle, text.clone());
            for mime in MIME_TYPES {
                source.offer((*mime).into());
            }
            device.proxy.set_primary_selection(Some(&source));
        }
        self.in_flight = true;
        // A paste uses a separate connection and must wait until the compositor has applied the offer.
        self.connection.display().sync(&self.handle, id);
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Writer {
    fn event(
        writer: &mut Self,
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
                writer.add_seat(registry, name, version);
            }
            wl_registry::Event::GlobalRemove { name } => writer.remove_seat(name),
            _ => {}
        }
    }
}

impl Dispatch<Device, u32> for Writer {
    fn event(
        writer: &mut Self,
        _: &Device,
        event: device::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            device::Event::Selection { id: Some(offer) } => offer.destroy(),
            device::Event::PrimarySelection { id } => {
                if let Some(offer) = id {
                    offer.destroy();
                }
                if let Some(device) = writer.devices.get_mut(name) {
                    device.supports_primary = true;
                }
            }
            device::Event::Finished => writer.remove_seat(*name),
            _ => {}
        }
    }

    event_created_child!(Writer, Device, [
        device::EVT_DATA_OFFER_OPCODE => (offer::ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<source::ExtDataControlSourceV1, Arc<String>> for Writer {
    fn event(
        writer: &mut Self,
        source: &source::ExtDataControlSourceV1,
        event: source::Event,
        text: &Arc<String>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            source::Event::Cancelled => source.destroy(),
            source::Event::Send { mime_type, fd }
                if MIME_TYPES.contains(&mime_type.as_str())
                    && Arc::strong_count(&writer.transfers) <= MAX_TRANSFERS =>
            {
                let text = text.clone();
                let permit = writer.transfers.clone();
                let _ = thread::Builder::new()
                    .name("lune-selection-transfer".into())
                    .spawn(move || {
                        let _permit = permit;
                        let _ = transfer(fd, text.as_bytes());
                    });
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, u64> for Writer {
    fn event(
        writer: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        id: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        {
            let mut pending = writer.queue.state.lock().unwrap();
            pending.confirmed = *id;
            writer.queue.ready.notify_all();
        }
        writer.in_flight = false;
    }
}

delegate_noop!(Writer: ignore wl_seat::WlSeat);
delegate_noop!(Writer: ignore manager::ExtDataControlManagerV1);
delegate_noop!(Writer: ignore offer::ExtDataControlOfferV1);

fn transfer(fd: OwnedFd, mut bytes: &[u8]) -> io::Result<()> {
    let mut pipe = File::from(fd);
    // SAFETY: The pipe owns the descriptor for both fcntl calls.
    unsafe {
        let flags = libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL);
        if flags < 0 || libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let deadline = Instant::now() + PASTE_TIMEOUT;
    while !bytes.is_empty() && wait_fd(pipe.as_fd(), libc::POLLOUT, deadline)? {
        match pipe.write(bytes) {
            Ok(0) => break,
            Ok(count) => bytes = &bytes[count..],
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
