//! Optional `afar_appmenu_v1` Wayland global menu integration backed by a
//! `com.canonical.dbusmenu` object on the session bus.
//!
//! When the compositor does not advertise `afar_appmenu_manager_v1`, nothing is
//! exported on D-Bus, no worker thread is started, and no menu UI is shown.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use futures_lite::future;
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::xdg::shell::client::xdg_toplevel::XdgToplevel;
use winit::event_loop::EventLoopProxy;
use winit::platform::wayland::WindowExtWayland;
use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
use winit::window::Window;
use zbus::zvariant::{OwnedValue, Structure, Value};

use super::Event;
use super::config::{TabPosition, TerminalMode};
use super::shortcuts::{Action, Keymap, Zoom};
use super::split::Axis;

const DBUS_OBJECT_PATH: &str = "/org/lune/MenuBar";
const DBUS_STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const DBUS_MENU_VERSION: u32 = 3;

#[allow(
    dead_code,
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    unused_imports,
    clippy::all
)]
mod protocol {
    use wayland_client;
    use wayland_protocols::xdg::shell::client::*;

    pub mod __interfaces {
        use wayland_protocols::xdg::shell::client::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/afar-appmenu-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocols/afar-appmenu-v1.xml");
}

use protocol::afar_appmenu_manager_v1::AfarAppmenuManagerV1;
use protocol::afar_appmenu_v1::{self, AfarAppmenuV1, Mode as WireMode};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum PresentationMode {
    #[default]
    Local,
    Global,
}

#[derive(Debug, Default)]
struct PresentationTracker {
    latest_serial: u32,
    mode: PresentationMode,
}

impl PresentationTracker {
    fn next_serial(&mut self) -> u32 {
        self.latest_serial = self.latest_serial.wrapping_add(1).max(1);
        self.mode = PresentationMode::Local;
        self.latest_serial
    }

    fn apply_event(&mut self, serial: u32, mode: PresentationMode) -> bool {
        if serial != self.latest_serial {
            return false;
        }
        let changed = self.mode != mode;
        self.mode = mode;
        changed
    }

    #[cfg(test)]
    fn mode(&self) -> PresentationMode {
        self.mode
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MenuState {
    pub(super) mode: TerminalMode,
    pub(super) tab_position: TabPosition,
    pub(super) sidebar_collapsed: bool,
    pub(super) fullscreen: bool,
    pub(super) vi_mode: bool,
    pub(super) multi_tab: bool,
    pub(super) multi_pane: bool,
    pub(super) pane_zoomed: bool,
    pub(super) broadcast_input: bool,
    pub(super) primary_screen: bool,
    pub(super) keymap_generation: u64,
}

struct WaylandState {
    tracker: PresentationTracker,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(WaylandState: ignore AfarAppmenuManagerV1);

impl Dispatch<AfarAppmenuV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &AfarAppmenuV1,
        event: afar_appmenu_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let afar_appmenu_v1::Event::Presentation { serial, mode } = event
            && let WEnum::Value(wire_mode) = mode
        {
            let mode = match wire_mode {
                WireMode::Local => PresentationMode::Local,
                WireMode::Global => PresentationMode::Global,
            };
            if state.tracker.apply_event(serial, mode) {
                log::debug!("App menu presentation switched to {mode:?} (serial {serial})");
            }
        }
    }
}

pub(super) struct AppMenu {
    connection: Connection,
    queue: EventQueue<WaylandState>,
    state: WaylandState,
    menu: AfarAppmenuV1,
    shared: Arc<Mutex<SharedMenu>>,
    notify_update: Arc<event_listener::Event>,
    stop: Arc<event_listener::Event>,
    last_state: MenuState,
}

impl AppMenu {
    /// Attaches an `afar_appmenu_v1` menu to `window` when the compositor
    /// advertises `afar_appmenu_manager_v1` and a session D-Bus connection is
    /// available. Returns `None` without side effects otherwise.
    pub(super) fn attach(
        window: &Window,
        proxy: EventLoopProxy<Event>,
        initial_state: MenuState,
        keymap: &Keymap,
    ) -> Option<Self> {
        let display_handle = window.display_handle().ok()?;
        let RawDisplayHandle::Wayland(wayland_display) = display_handle.as_raw() else {
            return None;
        };
        let toplevel_ptr = window.xdg_toplevel()?;

        // SAFETY: `window` belongs to the active Wayland event loop and owns a live
        // `wl_display` pointer for the entire lifetime of `Client`. `AppMenu` is dropped
        // before `Client.window` so the guest backend never outlives `wl_display`.
        let backend =
            unsafe { Backend::from_foreign_display(wayland_display.display.as_ptr().cast()) };
        let connection = Connection::from_backend(backend);
        let (globals, queue) = registry_queue_init::<WaylandState>(&connection).ok()?;
        let qh = queue.handle();

        let Ok(manager) = globals.bind::<AfarAppmenuManagerV1, _, _>(&qh, 1..=1, ()) else {
            log::debug!("Compositor does not advertise afar_appmenu_manager_v1");
            return None;
        };

        let model = MenuModel::build(initial_state, keymap);
        let shared = Arc::new(Mutex::new(SharedMenu { revision: 1, model }));
        let notify_update = Arc::new(event_listener::Event::new());
        let stop = Arc::new(event_listener::Event::new());

        // Protocol requirement: the com.canonical.dbusmenu object must already be
        // exported on the session bus before calling afar_appmenu_v1.set_address.
        let bus_name =
            match start_dbus_worker(shared.clone(), proxy, notify_update.clone(), stop.clone()) {
                Some(name) => name,
                None => {
                    manager.destroy();
                    let _ = connection.flush();
                    return None;
                }
            };

        // SAFETY: `toplevel_ptr` was returned by `WindowExtWayland::xdg_toplevel` for the
        // live `winit::window::Window` associated with this `wl_display`.
        let Ok(toplevel_id) =
            (unsafe { ObjectId::from_ptr(XdgToplevel::interface(), toplevel_ptr.as_ptr().cast()) })
        else {
            log::warn!("Could not resolve xdg_toplevel proxy for afar_appmenu_v1");
            stop.notify(1);
            manager.destroy();
            let _ = connection.flush();
            return None;
        };
        let Ok(toplevel) = XdgToplevel::from_id(&connection, toplevel_id) else {
            stop.notify(1);
            manager.destroy();
            let _ = connection.flush();
            return None;
        };

        let menu = manager.get_menu(&toplevel, &qh, ());
        manager.destroy();

        let mut state = WaylandState {
            tracker: PresentationTracker::default(),
        };
        let serial = state.tracker.next_serial();
        menu.set_address(serial, bus_name.clone(), DBUS_OBJECT_PATH.into());
        if let Err(error) = connection.flush() {
            log::warn!("Failed to publish afar_appmenu_v1 address: {error}");
            menu.destroy();
            stop.notify(1);
            return None;
        }

        log::debug!("Published afar_appmenu_v1 at {bus_name} {DBUS_OBJECT_PATH} (serial {serial})");
        Some(Self {
            connection,
            queue,
            state,
            menu,
            shared,
            notify_update,
            stop,
            last_state: initial_state,
        })
    }

    pub(super) fn dispatch_pending(&mut self) {
        let _ = self.queue.dispatch_pending(&mut self.state);
    }

    pub(super) fn sync(&mut self, next_state: MenuState, keymap: &Keymap) {
        if next_state == self.last_state {
            return;
        }
        self.last_state = next_state;
        let model = MenuModel::build(next_state, keymap);
        if let Ok(mut shared) = self.shared.lock() {
            shared.revision = shared.revision.wrapping_add(1).max(1);
            shared.model = model;
        }
        self.notify_update.notify(1);
    }
}

impl Drop for AppMenu {
    fn drop(&mut self) {
        let serial = self.state.tracker.next_serial();
        self.menu.clear(serial);
        self.menu.destroy();
        let _ = self.connection.flush();
        self.stop.notify(1);
    }
}

fn start_dbus_worker(
    shared: Arc<Mutex<SharedMenu>>,
    proxy: EventLoopProxy<Event>,
    notify_update: Arc<event_listener::Event>,
    stop: Arc<event_listener::Event>,
) -> Option<String> {
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<String, String>>(1);
    let spawn = thread::Builder::new()
        .name("lune-appmenu".into())
        .spawn(move || {
            pollster::block_on(run_dbus_worker(
                shared,
                proxy,
                notify_update,
                stop,
                ready_tx,
            ));
        });
    if let Err(error) = spawn {
        log::warn!("Could not start app menu D-Bus worker: {error}");
        return None;
    }
    match ready_rx.recv_timeout(DBUS_STARTUP_TIMEOUT) {
        Ok(Ok(bus_name)) => Some(bus_name),
        Ok(Err(error)) => {
            log::debug!("Session D-Bus unavailable for afar_appmenu_v1: {error}");
            None
        }
        Err(_) => {
            log::warn!("Timed out exporting com.canonical.dbusmenu on session bus");
            None
        }
    }
}

async fn run_dbus_worker(
    shared: Arc<Mutex<SharedMenu>>,
    proxy: EventLoopProxy<Event>,
    notify_update: Arc<event_listener::Event>,
    stop: Arc<event_listener::Event>,
    ready_tx: mpsc::SyncSender<Result<String, String>>,
) {
    let service = DbusMenuService {
        shared: shared.clone(),
        proxy,
    };
    let connection = match zbus::connection::Builder::session()
        .and_then(|builder| builder.serve_at(DBUS_OBJECT_PATH, service))
    {
        Ok(builder) => match builder.build().await {
            Ok(conn) => conn,
            Err(error) => {
                let _ = ready_tx.send(Err(error.to_string()));
                return;
            }
        },
        Err(error) => {
            let _ = ready_tx.send(Err(error.to_string()));
            return;
        }
    };

    let Some(bus_name) = connection
        .unique_name()
        .map(|name| name.as_str().to_owned())
    else {
        let _ = ready_tx.send(Err("session bus connection has no unique name".into()));
        return;
    };

    if ready_tx.send(Ok(bus_name)).is_err() {
        return;
    }

    let Ok(iface_ref) = connection
        .object_server()
        .interface::<_, DbusMenuService>(DBUS_OBJECT_PATH)
        .await
    else {
        return;
    };

    let mut last_emitted_revision = shared.lock().map(|guard| guard.revision).unwrap_or(1);
    let stopped = stop.listen();
    futures_lite::pin!(stopped);

    loop {
        let updated = notify_update.listen();
        let current_revision = shared.lock().map(|guard| guard.revision).unwrap_or(0);
        if current_revision != last_emitted_revision {
            last_emitted_revision = current_revision;
            if let Err(error) =
                DbusMenuService::layout_updated(iface_ref.signal_emitter(), current_revision, 0)
                    .await
            {
                log::debug!("Failed to emit com.canonical.dbusmenu LayoutUpdated: {error}");
                break;
            }
            continue;
        }
        let should_stop = future::race(
            async {
                stopped.as_mut().await;
                true
            },
            async {
                updated.await;
                false
            },
        )
        .await;
        if should_stop {
            break;
        }
    }
}

struct SharedMenu {
    revision: u32,
    model: MenuModel,
}

type LayoutItem = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);
type GroupProperty = (i32, HashMap<String, OwnedValue>);

struct DbusMenuService {
    shared: Arc<Mutex<SharedMenu>>,
    proxy: EventLoopProxy<Event>,
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl DbusMenuService {
    #[zbus(property)]
    async fn version(&self) -> u32 {
        DBUS_MENU_VERSION
    }

    #[zbus(property)]
    async fn text_direction(&self) -> &'static str {
        "ltr"
    }

    #[zbus(property)]
    async fn status(&self) -> &'static str {
        "normal"
    }

    #[zbus(property)]
    async fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }

    async fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        property_names: Vec<String>,
    ) -> (u32, LayoutItem) {
        let Ok(guard) = self.shared.lock() else {
            return (0, (parent_id, HashMap::new(), Vec::new()));
        };
        let layout = guard
            .model
            .layout(parent_id, recursion_depth, &property_names)
            .unwrap_or_else(|| (parent_id, HashMap::new(), Vec::new()));
        (guard.revision, layout)
    }

    async fn get_group_properties(
        &self,
        ids: Vec<i32>,
        property_names: Vec<String>,
    ) -> Vec<GroupProperty> {
        let Ok(guard) = self.shared.lock() else {
            return Vec::new();
        };
        guard.model.group_properties(&ids, &property_names)
    }

    async fn get_property(&self, id: i32, name: String) -> zbus::fdo::Result<OwnedValue> {
        let guard = self
            .shared
            .lock()
            .map_err(|_| zbus::fdo::Error::Failed("menu state lock poisoned".into()))?;
        guard.model.property(id, &name).ok_or_else(|| {
            zbus::fdo::Error::InvalidArgs(format!("Unknown property {name} on {id}"))
        })
    }

    async fn event(&self, id: i32, event_id: String, _data: OwnedValue, _timestamp: u32) {
        if event_id != "clicked" {
            return;
        }
        let action = self
            .shared
            .lock()
            .ok()
            .and_then(|guard| guard.model.action_for_click(id));
        if let Some(action) = action {
            let _ = self.proxy.send_event(Event::AppMenuAction(action));
        }
    }

    async fn event_group(&self, events: Vec<(i32, String, OwnedValue, u32)>) -> Vec<i32> {
        let mut errors = Vec::new();
        let mut actions = Vec::new();
        if let Ok(guard) = self.shared.lock() {
            for (id, event_id, _data, _timestamp) in events {
                if !guard.model.contains(id) {
                    errors.push(id);
                    continue;
                }
                if event_id == "clicked"
                    && let Some(action) = guard.model.action_for_click(id)
                {
                    actions.push(action);
                }
            }
        }
        for action in actions {
            let _ = self.proxy.send_event(Event::AppMenuAction(action));
        }
        errors
    }

    async fn about_to_show(&self, _id: i32) -> bool {
        false
    }

    async fn about_to_show_group(&self, ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        let errors = self
            .shared
            .lock()
            .map(|guard| {
                ids.into_iter()
                    .filter(|id| !guard.model.contains(*id))
                    .collect()
            })
            .unwrap_or_default();
        (Vec::new(), errors)
    }

    #[zbus(signal)]
    async fn items_properties_updated(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        updated_props: Vec<GroupProperty>,
        removed_props: Vec<(i32, Vec<String>)>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn layout_updated(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        revision: u32,
        parent: i32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn item_activation_requested(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        id: i32,
        timestamp: u32,
    ) -> zbus::Result<()>;
}

#[derive(Clone, Debug)]
struct MenuNode {
    id: i32,
    kind: NodeKind,
    children: Vec<MenuNode>,
}

#[derive(Clone, Debug)]
enum NodeKind {
    Root,
    Submenu {
        label: &'static str,
    },
    Separator,
    Item {
        label: &'static str,
        action: Action,
        enabled: bool,
        checked: Option<bool>,
        shortcut: Option<Vec<String>>,
    },
}

impl MenuNode {
    fn submenu(id: i32, label: &'static str, children: Vec<MenuNode>) -> Self {
        Self {
            id,
            kind: NodeKind::Submenu { label },
            children,
        }
    }

    fn separator(id: i32) -> Self {
        Self {
            id,
            kind: NodeKind::Separator,
            children: Vec::new(),
        }
    }

    fn item(
        id: i32,
        label: &'static str,
        action: Action,
        enabled: bool,
        checked: Option<bool>,
        keymap: &Keymap,
    ) -> Self {
        Self {
            id,
            kind: NodeKind::Item {
                label,
                action,
                enabled,
                checked,
                shortcut: keymap.dbusmenu_shortcut(action),
            },
            children: Vec::new(),
        }
    }

    fn properties(&self, filter: &[String]) -> HashMap<String, OwnedValue> {
        let mut props = HashMap::new();
        let mut insert = |key: &str, value: Value<'_>| {
            if !filter.is_empty() && !filter.iter().any(|wanted| wanted == key) {
                return;
            }
            if let Ok(owned) = OwnedValue::try_from(value) {
                props.insert(key.to_owned(), owned);
            }
        };

        match &self.kind {
            NodeKind::Root => {
                insert("children-display", Value::from("submenu"));
            }
            NodeKind::Submenu { label } => {
                insert("label", Value::from(*label));
                insert("enabled", Value::from(true));
                insert("visible", Value::from(true));
                insert("children-display", Value::from("submenu"));
            }
            NodeKind::Separator => {
                insert("type", Value::from("separator"));
                insert("visible", Value::from(true));
            }
            NodeKind::Item {
                label,
                enabled,
                checked,
                shortcut,
                ..
            } => {
                insert("type", Value::from("standard"));
                insert("label", Value::from(*label));
                insert("enabled", Value::from(*enabled));
                insert("visible", Value::from(true));
                if let Some(is_checked) = checked {
                    insert("toggle-type", Value::from("checkmark"));
                    insert("toggle-state", Value::from(i32::from(*is_checked)));
                }
                if let Some(tokens) = shortcut {
                    let shortcut_list: Vec<Vec<String>> = vec![tokens.clone()];
                    insert("shortcut", Value::from(shortcut_list));
                }
            }
        }
        props
    }

    fn to_layout(&self, depth: i32, filter: &[String]) -> LayoutItem {
        let props = self.properties(filter);
        let children = if depth == 0 {
            Vec::new()
        } else {
            let next_depth = if depth < 0 { -1 } else { depth - 1 };
            self.children
                .iter()
                .filter_map(|child| {
                    let item = child.to_layout(next_depth, filter);
                    OwnedValue::try_from(Value::from(Structure::from(item))).ok()
                })
                .collect()
        };
        (self.id, props, children)
    }

    fn find(&self, id: i32) -> Option<&MenuNode> {
        if self.id == id {
            return Some(self);
        }
        for child in &self.children {
            if let Some(found) = child.find(id) {
                return Some(found);
            }
        }
        None
    }

    fn collect_all<'a>(&'a self, out: &mut Vec<&'a MenuNode>) {
        out.push(self);
        for child in &self.children {
            child.collect_all(out);
        }
    }
}

#[derive(Clone, Debug)]
struct MenuModel {
    root: MenuNode,
}

impl MenuModel {
    fn build(state: MenuState, keymap: &Keymap) -> Self {
        let close_label = if state.multi_pane {
            "Close Pane"
        } else {
            "Close Tab"
        };
        let sidebar_available =
            state.mode == TerminalMode::Modern && state.tab_position == TabPosition::Left;

        let file = MenuNode::submenu(
            100,
            "_File",
            vec![
                MenuNode::item(101, "New Window", Action::NewWindow, true, None, keymap),
                MenuNode::item(102, "New Tab", Action::NewTab, true, None, keymap),
                MenuNode::separator(103),
                MenuNode::item(
                    104,
                    "Split Right",
                    Action::SplitPane(Axis::Columns),
                    true,
                    None,
                    keymap,
                ),
                MenuNode::item(
                    105,
                    "Split Down",
                    Action::SplitPane(Axis::Rows),
                    true,
                    None,
                    keymap,
                ),
                MenuNode::separator(106),
                MenuNode::item(
                    107,
                    "Open Working Directory",
                    Action::OpenDirectory,
                    true,
                    None,
                    keymap,
                ),
                MenuNode::separator(108),
                MenuNode::item(109, close_label, Action::CloseTab, true, None, keymap),
            ],
        );

        let edit = MenuNode::submenu(
            200,
            "_Edit",
            vec![
                MenuNode::item(201, "Copy", Action::Copy, true, None, keymap),
                MenuNode::item(202, "Paste", Action::Paste, !state.vi_mode, None, keymap),
                MenuNode::separator(203),
                MenuNode::item(204, "Find...", Action::Search, true, None, keymap),
                MenuNode::item(205, "Quick Select Hints", Action::Hints, true, None, keymap),
                MenuNode::item(
                    206,
                    "Vi Mode",
                    Action::ToggleVi,
                    true,
                    Some(state.vi_mode),
                    keymap,
                ),
                MenuNode::separator(207),
                MenuNode::item(
                    208,
                    "Copy Last Command",
                    Action::CopyLastCommand,
                    state.primary_screen,
                    None,
                    keymap,
                ),
                MenuNode::item(
                    209,
                    "Copy Last Output",
                    Action::CopyLastOutput,
                    state.primary_screen,
                    None,
                    keymap,
                ),
                MenuNode::item(
                    210,
                    "Select Last Output",
                    Action::SelectLastOutput,
                    state.primary_screen,
                    None,
                    keymap,
                ),
            ],
        );

        let view = MenuNode::submenu(
            300,
            "_View",
            vec![
                MenuNode::item(301, "Zoom In", Action::Zoom(Zoom::In), true, None, keymap),
                MenuNode::item(302, "Zoom Out", Action::Zoom(Zoom::Out), true, None, keymap),
                MenuNode::item(
                    303,
                    "Reset Zoom",
                    Action::Zoom(Zoom::Reset),
                    true,
                    None,
                    keymap,
                ),
                MenuNode::separator(304),
                MenuNode::item(
                    305,
                    "Modern Mode",
                    Action::ToggleMode,
                    true,
                    Some(state.mode == TerminalMode::Modern),
                    keymap,
                ),
                MenuNode::item(
                    306,
                    "Show Tab Sidebar",
                    Action::ToggleTabSidebar,
                    sidebar_available,
                    Some(sidebar_available && !state.sidebar_collapsed),
                    keymap,
                ),
                MenuNode::item(
                    307,
                    "Zoom Pane",
                    Action::TogglePaneZoom,
                    state.multi_pane || state.pane_zoomed,
                    Some(state.pane_zoomed),
                    keymap,
                ),
                MenuNode::item(
                    311,
                    "Broadcast Input",
                    Action::ToggleBroadcastInput,
                    state.multi_pane,
                    Some(state.broadcast_input),
                    keymap,
                ),
                MenuNode::item(
                    308,
                    "Fullscreen",
                    Action::ToggleFullscreen,
                    true,
                    Some(state.fullscreen),
                    keymap,
                ),
                MenuNode::separator(309),
                MenuNode::item(
                    310,
                    "Command Palette...",
                    Action::CommandPalette,
                    true,
                    None,
                    keymap,
                ),
            ],
        );

        let commands = MenuNode::submenu(
            400,
            "_Commands",
            vec![
                MenuNode::item(
                    401,
                    "Previous Prompt",
                    Action::PreviousPrompt,
                    state.primary_screen,
                    None,
                    keymap,
                ),
                MenuNode::item(
                    402,
                    "Next Prompt",
                    Action::NextPrompt,
                    state.primary_screen,
                    None,
                    keymap,
                ),
                MenuNode::item(
                    403,
                    "Previous Failed Command",
                    Action::PreviousFailedPrompt,
                    state.primary_screen,
                    None,
                    keymap,
                ),
                MenuNode::separator(404),
                MenuNode::item(
                    405,
                    "Re-run Last Command",
                    Action::RerunLastCommand,
                    state.primary_screen,
                    None,
                    keymap,
                ),
                MenuNode::separator(406),
                MenuNode::item(
                    407,
                    "Toggle Block Bookmark",
                    Action::ToggleBlockBookmark,
                    state.primary_screen,
                    None,
                    keymap,
                ),
                MenuNode::item(
                    408,
                    "Previous Bookmark",
                    Action::PreviousBookmark,
                    state.primary_screen,
                    None,
                    keymap,
                ),
                MenuNode::item(
                    409,
                    "Next Bookmark",
                    Action::NextBookmark,
                    state.primary_screen,
                    None,
                    keymap,
                ),
                MenuNode::item(
                    410,
                    "Search Bookmarks...",
                    Action::SearchBookmarks,
                    state.primary_screen,
                    None,
                    keymap,
                ),
            ],
        );

        let window = MenuNode::submenu(
            500,
            "_Window",
            vec![
                MenuNode::item(
                    501,
                    "Previous Tab",
                    Action::PreviousTab,
                    state.multi_tab,
                    None,
                    keymap,
                ),
                MenuNode::item(
                    502,
                    "Next Tab",
                    Action::NextTab,
                    state.multi_tab,
                    None,
                    keymap,
                ),
                MenuNode::item(
                    503,
                    "Move Tab Left",
                    Action::MoveTabLeft,
                    state.multi_tab,
                    None,
                    keymap,
                ),
                MenuNode::item(
                    504,
                    "Move Tab Right",
                    Action::MoveTabRight,
                    state.multi_tab,
                    None,
                    keymap,
                ),
                MenuNode::item(507, "Rename Tab...", Action::RenameTab, true, None, keymap),
                MenuNode::separator(508),
                MenuNode::item(
                    509,
                    "Swap Pane",
                    Action::SwapPane,
                    state.multi_pane,
                    None,
                    keymap,
                ),
                MenuNode::item(
                    510,
                    "Move Pane to New Tab",
                    Action::DetachPane,
                    state.multi_pane,
                    None,
                    keymap,
                ),
                MenuNode::separator(505),
                MenuNode::item(
                    506,
                    "Reload Configuration",
                    Action::ReloadConfig,
                    true,
                    None,
                    keymap,
                ),
            ],
        );

        Self {
            root: MenuNode {
                id: 0,
                kind: NodeKind::Root,
                children: vec![file, edit, view, commands, window],
            },
        }
    }

    fn contains(&self, id: i32) -> bool {
        self.root.find(id).is_some()
    }

    fn layout(&self, parent_id: i32, depth: i32, filter: &[String]) -> Option<LayoutItem> {
        self.root
            .find(parent_id)
            .map(|node| node.to_layout(depth, filter))
    }

    fn group_properties(&self, ids: &[i32], filter: &[String]) -> Vec<GroupProperty> {
        if ids.is_empty() {
            let mut nodes = Vec::new();
            self.root.collect_all(&mut nodes);
            return nodes
                .into_iter()
                .map(|node| (node.id, node.properties(filter)))
                .collect();
        }
        ids.iter()
            .filter_map(|id| {
                self.root
                    .find(*id)
                    .map(|node| (node.id, node.properties(filter)))
            })
            .collect()
    }

    fn property(&self, id: i32, name: &str) -> Option<OwnedValue> {
        let node = self.root.find(id)?;
        let filter = [name.to_owned()];
        node.properties(&filter).remove(name)
    }

    fn action_for_click(&self, id: i32) -> Option<Action> {
        match self.root.find(id)?.kind {
            NodeKind::Item {
                action,
                enabled: true,
                ..
            } => Some(action),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Type;

    fn sample_state() -> MenuState {
        MenuState {
            mode: TerminalMode::Modern,
            tab_position: TabPosition::Left,
            sidebar_collapsed: false,
            fullscreen: false,
            vi_mode: false,
            multi_tab: true,
            multi_pane: false,
            pane_zoomed: false,
            broadcast_input: false,
            primary_screen: true,
            keymap_generation: 0,
        }
    }

    #[test]
    fn presentation_tracker_applies_latest_serial_only() {
        let mut tracker = PresentationTracker::default();
        assert_eq!(tracker.mode(), PresentationMode::Local);

        let s1 = tracker.next_serial();
        assert!(tracker.apply_event(s1, PresentationMode::Global));
        assert_eq!(tracker.mode(), PresentationMode::Global);

        let s2 = tracker.next_serial();
        assert_eq!(tracker.mode(), PresentationMode::Local);
        // Stale event from s1 must be ignored.
        assert!(!tracker.apply_event(s1, PresentationMode::Global));
        assert_eq!(tracker.mode(), PresentationMode::Local);

        assert!(tracker.apply_event(s2, PresentationMode::Global));
        assert_eq!(tracker.mode(), PresentationMode::Global);
        assert!(tracker.apply_event(s2, PresentationMode::Local));
        assert_eq!(tracker.mode(), PresentationMode::Local);
    }

    #[test]
    fn dbusmenu_layout_matches_wire_signature_depth_and_filters() {
        assert_eq!(LayoutItem::SIGNATURE.to_string(), "(ia{sv}av)");

        let keymap = Keymap::from_overrides(&[]);
        let model = MenuModel::build(sample_state(), &keymap);

        // Depth 0 returns root properties with no children.
        let (root_id, root_props, root_children) = model.layout(0, 0, &[]).unwrap();
        assert_eq!(root_id, 0);
        assert_eq!(
            root_props
                .get("children-display")
                .and_then(|v| <&str>::try_from(v).ok()),
            Some("submenu")
        );
        assert!(root_children.is_empty());

        // Depth 1 returns top-level submenus without expanding grandchildren.
        let (_, _, top_menus) = model.layout(0, 1, &["label".into()]).unwrap();
        assert_eq!(top_menus.len(), 5);
        let first_menu = Structure::try_from(top_menus[0].clone()).unwrap();
        let fields = first_menu.fields();
        assert_eq!(i32::try_from(&fields[0]).unwrap(), 100);
        let props = HashMap::<String, OwnedValue>::try_from(fields[1].clone()).unwrap();
        assert_eq!(props.len(), 1);
        assert_eq!(
            props.get("label").and_then(|v| <&str>::try_from(v).ok()),
            Some("_File")
        );
        let grandchildren = Vec::<OwnedValue>::try_from(fields[2].clone()).unwrap();
        assert!(grandchildren.is_empty());

        // Unlimited depth expands all menu items and preserves shortcuts.
        let (file_id, _, file_children) = model.layout(100, -1, &[]).unwrap();
        assert_eq!(file_id, 100);
        assert!(!file_children.is_empty());
        let shortcut = model.property(101, "shortcut").unwrap();
        let tokens = Vec::<Vec<String>>::try_from(shortcut).unwrap();
        assert_eq!(
            tokens,
            vec![vec![
                "Control".to_owned(),
                "Shift".to_owned(),
                "n".to_owned()
            ]]
        );
    }

    #[test]
    fn dbusmenu_reflects_dynamic_state_and_click_gating() {
        let keymap = Keymap::from_overrides(&[]);
        let mut state = sample_state();
        state.vi_mode = true;
        state.multi_tab = false;
        state.primary_screen = false;

        let model = MenuModel::build(state, &keymap);
        // Paste is disabled while in Vi mode; clicking it must not dispatch.
        assert_eq!(
            model
                .property(202, "enabled")
                .and_then(|v| bool::try_from(v).ok()),
            Some(false)
        );
        assert_eq!(model.action_for_click(202), None);

        // Vi Mode checkmark toggle reflects active state.
        assert_eq!(
            model
                .property(206, "toggle-state")
                .and_then(|v| i32::try_from(v).ok()),
            Some(1)
        );
        assert_eq!(model.action_for_click(206), Some(Action::ToggleVi));

        // Tab navigation disabled when only one tab is open.
        assert_eq!(model.action_for_click(502), None);
        // Primary-screen-only command actions disabled on alternate screen.
        assert_eq!(model.action_for_click(401), None);
    }
}
