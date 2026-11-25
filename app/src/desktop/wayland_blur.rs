//! Optional native Wayland background blur integration.
//!
//! Prefers the standard `ext_background_effect_v1` staging protocol and falls
//! back to KDE's `org_kde_kwin_blur_manager` when available. When the
//! compositor advertises neither protocol, attachment returns `None` without
//! side effects.

use wayland_client::backend::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_compositor, wl_region, wl_registry, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::ext::background_effect::v1::client::ext_background_effect_manager_v1::{
    self, Capability, ExtBackgroundEffectManagerV1,
};
use wayland_protocols::ext::background_effect::v1::client::ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1;
use wayland_protocols_plasma::blur::client::org_kde_kwin_blur::OrgKdeKwinBlur;
use wayland_protocols_plasma::blur::client::org_kde_kwin_blur_manager::OrgKdeKwinBlurManager;
use winit::dpi::LogicalSize;
use winit::raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
};
use winit::window::Window;

#[derive(Default)]
struct BlurState {
    ext_blur_capable: bool,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for BlurState {
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

impl Dispatch<ExtBackgroundEffectManagerV1, ()> for BlurState {
    fn event(
        state: &mut Self,
        _proxy: &ExtBackgroundEffectManagerV1,
        event: ext_background_effect_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_background_effect_manager_v1::Event::Capabilities { flags } = event {
            state.ext_blur_capable = match flags {
                WEnum::Value(caps) => caps.contains(Capability::Blur),
                WEnum::Unknown(raw) => (raw & Capability::Blur.bits()) != 0,
            };
        }
    }
}

delegate_noop!(BlurState: ignore wl_compositor::WlCompositor);
delegate_noop!(BlurState: ignore wl_region::WlRegion);
delegate_noop!(BlurState: ignore ExtBackgroundEffectSurfaceV1);
delegate_noop!(BlurState: ignore OrgKdeKwinBlurManager);
delegate_noop!(BlurState: ignore OrgKdeKwinBlur);

enum BlurBackend {
    Ext {
        compositor: wl_compositor::WlCompositor,
        effect: ExtBackgroundEffectSurfaceV1,
    },
    Kde {
        manager: OrgKdeKwinBlurManager,
        active: Option<OrgKdeKwinBlur>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AppliedState {
    Disabled,
    Enabled(u32, u32),
}

pub(super) struct BackgroundBlur {
    connection: Connection,
    queue: EventQueue<BlurState>,
    surface: wl_surface::WlSurface,
    backend: BlurBackend,
    applied: Option<AppliedState>,
}

impl BackgroundBlur {
    /// Attaches native Wayland background blur support to `window` when the
    /// compositor advertises `ext_background_effect_v1` (with blur capability)
    /// or `org_kde_kwin_blur_manager`.
    pub(super) fn attach(window: &Window) -> Option<Self> {
        let display_handle = window.display_handle().ok()?;
        let RawDisplayHandle::Wayland(wayland_display) = display_handle.as_raw() else {
            return None;
        };
        let window_handle = window.window_handle().ok()?;
        let RawWindowHandle::Wayland(wayland_window) = window_handle.as_raw() else {
            return None;
        };

        // SAFETY: `window` belongs to the active Wayland event loop and owns a live
        // `wl_display` pointer for the entire lifetime of `Client`. `BackgroundBlur`
        // is declared before `Client.window` so the guest backend drops before `wl_display`.
        let backend =
            unsafe { Backend::from_foreign_display(wayland_display.display.as_ptr().cast()) };
        let connection = Connection::from_backend(backend);
        let (globals, mut queue) = registry_queue_init::<BlurState>(&connection).ok()?;
        let qh = queue.handle();

        // SAFETY: `wayland_window.surface` is the live `wl_surface` owned by `window`
        // on the same `wl_display` wrapped by `connection`.
        let surface_id = unsafe {
            ObjectId::from_ptr(
                wl_surface::WlSurface::interface(),
                wayland_window.surface.as_ptr().cast(),
            )
        }
        .ok()?;
        let surface = wl_surface::WlSurface::from_id(&connection, surface_id).ok()?;

        if let Ok(manager) = globals.bind::<ExtBackgroundEffectManagerV1, _, _>(&qh, 1..=1, ()) {
            let mut state = BlurState::default();
            if queue.roundtrip(&mut state).is_ok()
                && state.ext_blur_capable
                && let Ok(compositor) =
                    globals.bind::<wl_compositor::WlCompositor, _, _>(&qh, 1..=6, ())
            {
                let effect = manager.get_background_effect(&surface, &qh, ());
                manager.destroy();
                let _ = connection.flush();
                log::debug!("Attached Wayland ext_background_effect_v1 blur");
                return Some(Self {
                    connection,
                    queue,
                    surface,
                    backend: BlurBackend::Ext { compositor, effect },
                    applied: None,
                });
            }
            manager.destroy();
            let _ = connection.flush();
        }

        if let Ok(manager) = globals.bind::<OrgKdeKwinBlurManager, _, _>(&qh, 1..=1, ()) {
            log::debug!("Attached Wayland org_kde_kwin_blur_manager blur");
            return Some(Self {
                connection,
                queue,
                surface,
                backend: BlurBackend::Kde {
                    manager,
                    active: None,
                },
                applied: None,
            });
        }

        log::debug!("Compositor does not advertise Wayland background blur");
        None
    }

    /// Updates the native blur region for the surface. Returns `true` if a
    /// protocol change was submitted and a surface commit/redraw is needed.
    pub(super) fn sync(&mut self, enabled: bool, logical_size: LogicalSize<u32>) -> bool {
        let desired = if enabled && logical_size.width > 0 && logical_size.height > 0 {
            AppliedState::Enabled(logical_size.width, logical_size.height)
        } else {
            AppliedState::Disabled
        };

        // For KDE blur (`set_region(None)` blurs the whole surface), size changes
        // do not require re-submitting requests once already enabled.
        if let BlurBackend::Kde { .. } = self.backend
            && matches!(
                (self.applied, desired),
                (Some(AppliedState::Enabled(..)), AppliedState::Enabled(..))
            )
        {
            self.applied = Some(desired);
            return false;
        }
        if self.applied == Some(desired) {
            return false;
        }

        let qh = self.queue.handle();
        match (&mut self.backend, desired) {
            (BlurBackend::Ext { compositor, effect }, AppliedState::Enabled(w, h)) => {
                let region = compositor.create_region(&qh, ());
                let width = i32::try_from(w).unwrap_or(i32::MAX);
                let height = i32::try_from(h).unwrap_or(i32::MAX);
                region.add(0, 0, width, height);
                effect.set_blur_region(Some(&region));
                region.destroy();
            }
            (BlurBackend::Ext { effect, .. }, AppliedState::Disabled) => {
                effect.set_blur_region(None);
            }
            (BlurBackend::Kde { manager, active }, AppliedState::Enabled(..)) => {
                let blur = active.get_or_insert_with(|| manager.create(&self.surface, &qh, ()));
                blur.set_region(None);
                blur.commit();
            }
            (BlurBackend::Kde { manager, active }, AppliedState::Disabled) => {
                manager.unset(&self.surface);
                if let Some(blur) = active.take() {
                    blur.release();
                }
            }
        }

        self.applied = Some(desired);
        let _ = self.connection.flush();
        true
    }
}

impl Drop for BackgroundBlur {
    fn drop(&mut self) {
        match &mut self.backend {
            BlurBackend::Ext { effect, .. } => {
                effect.destroy();
            }
            BlurBackend::Kde { manager, active } => {
                if let Some(blur) = active.take() {
                    manager.unset(&self.surface);
                    blur.release();
                }
            }
        }
        let _ = self.connection.flush();
    }
}
