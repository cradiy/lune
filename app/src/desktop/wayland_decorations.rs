//! Standard Wayland decoration and WM capability probing for inline titlebars.
//!
//! Queries two standard Wayland protocols on an unmapped, invisible probe
//! `xdg_toplevel` (initial empty commit, no buffer attached):
//! - `xdg-decoration-unstable-v1` (`zxdg_decoration_manager_v1` /
//!   `zxdg_toplevel_decoration_v1`): detects whether the compositor requests
//!   server-side / no client-side decorations (`ServerSide`).
//! - `xdg-shell` (`xdg_toplevel.wm_capabilities` v5+ and initial `configure`
//!   states): detects which window controls (`minimize`, `maximize`) the
//!   compositor actually supports and whether new windows open tiled.
//!
//! Also parses `org.gnome.desktop.wm.preferences` `button-layout` values
//! received from `org.freedesktop.portal.Settings`.

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_compositor, wl_registry, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::xdg::decoration::zv1::client::{
    zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

use super::config::WindowControlsPolicy;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct WindowButtons {
    pub(super) minimize: bool,
    pub(super) maximize: bool,
    pub(super) close: bool,
}

impl WindowButtons {
    pub(super) const ALL: Self = Self {
        minimize: true,
        maximize: true,
        close: true,
    };

    pub(super) const NONE: Self = Self {
        minimize: false,
        maximize: false,
        close: false,
    };

    pub(super) fn any(self) -> bool {
        self.minimize || self.maximize || self.close
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DecorationMode {
    ServerSide,
    ClientSide,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct WmCapabilities {
    pub(super) window_menu: bool,
    pub(super) maximize: bool,
    pub(super) fullscreen: bool,
    pub(super) minimize: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct CompositorDecorations {
    pub(super) decoration_mode: Option<DecorationMode>,
    pub(super) wm_capabilities: Option<WmCapabilities>,
    pub(super) initially_tiled: bool,
    pub(super) has_afar_appmenu: bool,
}

/// Resolves which inline titlebar window control buttons should be rendered.
pub(super) fn resolve_window_buttons(
    policy: WindowControlsPolicy,
    compositor: Option<&CompositorDecorations>,
    portal_layout: Option<WindowButtons>,
) -> WindowButtons {
    match policy {
        WindowControlsPolicy::Show => WindowButtons::ALL,
        WindowControlsPolicy::Hide => WindowButtons::NONE,
        WindowControlsPolicy::Auto => {
            if let Some(info) = compositor
                && info.decoration_mode == Some(DecorationMode::ServerSide)
                && (info.initially_tiled
                    || info
                        .wm_capabilities
                        .is_none_or(|caps| !caps.window_menu || !caps.minimize))
            {
                return WindowButtons::NONE;
            }
            let mut buttons = portal_layout.unwrap_or(WindowButtons::ALL);
            if let Some(caps) = compositor.and_then(|info| info.wm_capabilities) {
                buttons.minimize &= caps.minimize;
                buttons.maximize &= caps.maximize;
            }
            buttons
        }
    }
}

/// Parses an XDG/GNOME `button-layout` string such as `":minimize,maximize,close"`,
/// `":close"`, `"close:"`, or `":"` into the enabled window buttons.
pub(super) fn parse_button_layout(raw: &str) -> WindowButtons {
    let mut buttons = WindowButtons::NONE;
    for side in raw.split(':') {
        for token in side.split(',') {
            match token.trim().to_ascii_lowercase().as_str() {
                "minimize" => buttons.minimize = true,
                "maximize" => buttons.maximize = true,
                "close" => buttons.close = true,
                _ => {}
            }
        }
    }
    buttons
}

/// Probes the running Wayland compositor for decoration preference and WM
/// capabilities using an unmapped probe `xdg_toplevel` (never attaches a buffer).
pub(super) fn probe_wayland_decorations() -> Option<CompositorDecorations> {
    let connection = Connection::connect_to_env().ok()?;
    let (globals, mut queue) = registry_queue_init::<ProbeState>(&connection).ok()?;
    let qh = queue.handle();

    let has_afar_appmenu = globals.contents().with_list(|list| {
        list.iter()
            .any(|global| global.interface == "afar_appmenu_manager_v1")
    });
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 1..=6, ()).ok()?;
    let wm_base: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=6, ()).ok()?;
    let decoration_manager: Option<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1> =
        globals.bind(&qh, 1..=1, ()).ok();

    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    let decoration = decoration_manager
        .as_ref()
        .map(|manager| manager.get_toplevel_decoration(&toplevel, &qh, ()));

    // Initial commit without a buffer triggers configure/wm_capabilities without mapping.
    surface.commit();

    let mut state = ProbeState::default();
    state.info.has_afar_appmenu = has_afar_appmenu;
    let _ = queue.roundtrip(&mut state);
    if !state.configured {
        let _ = queue.roundtrip(&mut state);
    }

    if let Some(deco) = decoration {
        deco.destroy();
    }
    toplevel.destroy();
    xdg_surface.destroy();
    surface.destroy();
    if let Some(manager) = decoration_manager {
        manager.destroy();
    }
    wm_base.destroy();
    let _ = connection.flush();

    log::debug!(
        "Wayland decoration probe: mode={:?}, wm_capabilities={:?}, tiled={}",
        state.info.decoration_mode,
        state.info.wm_capabilities,
        state.info.initially_tiled
    );
    Some(state.info)
}

#[derive(Default)]
struct ProbeState {
    info: CompositorDecorations,
    configured: bool,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ProbeState {
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

delegate_noop!(ProbeState: ignore wl_compositor::WlCompositor);
delegate_noop!(ProbeState: ignore wl_surface::WlSurface);
delegate_noop!(ProbeState: ignore zxdg_decoration_manager_v1::ZxdgDecorationManagerV1);

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for ProbeState {
    fn event(
        _state: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for ProbeState {
    fn event(
        state: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            state.configured = true;
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for ProbeState {
    fn event(
        state: &mut Self,
        _toplevel: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { states, .. } => {
                let tiled = states.as_chunks::<4>().0.iter().any(|chunk| {
                    let raw = u32::from_ne_bytes(*chunk);
                    matches!(
                        xdg_toplevel::State::try_from(raw),
                        Ok(xdg_toplevel::State::TiledLeft
                            | xdg_toplevel::State::TiledRight
                            | xdg_toplevel::State::TiledTop
                            | xdg_toplevel::State::TiledBottom)
                    )
                });
                state.info.initially_tiled |= tiled;
            }
            xdg_toplevel::Event::WmCapabilities { capabilities } => {
                let mut caps = WmCapabilities::default();
                for chunk in capabilities.as_chunks::<4>().0 {
                    let raw = u32::from_ne_bytes(*chunk);
                    match xdg_toplevel::WmCapabilities::try_from(raw) {
                        Ok(xdg_toplevel::WmCapabilities::WindowMenu) => caps.window_menu = true,
                        Ok(xdg_toplevel::WmCapabilities::Maximize) => caps.maximize = true,
                        Ok(xdg_toplevel::WmCapabilities::Fullscreen) => caps.fullscreen = true,
                        Ok(xdg_toplevel::WmCapabilities::Minimize) => caps.minimize = true,
                        _ => {}
                    }
                }
                state.info.wm_capabilities = Some(caps);
            }
            _ => {}
        }
    }
}

impl Dispatch<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, ()> for ProbeState {
    fn event(
        state: &mut Self,
        _decoration: &zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
        event: zxdg_toplevel_decoration_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zxdg_toplevel_decoration_v1::Event::Configure { mode } = event {
            state.info.decoration_mode = match mode {
                WEnum::Value(zxdg_toplevel_decoration_v1::Mode::ServerSide) => {
                    Some(DecorationMode::ServerSide)
                }
                WEnum::Value(zxdg_toplevel_decoration_v1::Mode::ClientSide) => {
                    Some(DecorationMode::ClientSide)
                }
                _ => None,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CompositorDecorations, DecorationMode, WindowButtons, WmCapabilities, parse_button_layout,
        resolve_window_buttons,
    };
    use crate::desktop::config::WindowControlsPolicy;

    #[test]
    fn parses_portal_button_layouts() {
        assert_eq!(
            parse_button_layout(":minimize,maximize,close"),
            WindowButtons::ALL
        );
        assert_eq!(
            parse_button_layout("appmenu:close"),
            WindowButtons {
                minimize: false,
                maximize: false,
                close: true,
            }
        );
        assert_eq!(parse_button_layout(":"), WindowButtons::NONE);
        assert_eq!(parse_button_layout(""), WindowButtons::NONE);
    }

    #[test]
    fn auto_policy_respects_wayland_decoration_and_wm_capabilities() {
        // Tiling / prefer-no-csd compositor sending ServerSide hides all buttons.
        let tiling_ssd = CompositorDecorations {
            decoration_mode: Some(DecorationMode::ServerSide),
            wm_capabilities: Some(WmCapabilities {
                maximize: true,
                fullscreen: true,
                minimize: false,
                window_menu: false,
            }),
            initially_tiled: true,
            has_afar_appmenu: false,
        };
        assert_eq!(
            resolve_window_buttons(WindowControlsPolicy::Auto, Some(&tiling_ssd), None),
            WindowButtons::NONE
        );

        // CSD compositor without minimize capability hides only minimize.
        let csd_no_minimize = CompositorDecorations {
            decoration_mode: None,
            wm_capabilities: Some(WmCapabilities {
                maximize: true,
                fullscreen: true,
                minimize: false,
                window_menu: false,
            }),
            initially_tiled: false,
            has_afar_appmenu: false,
        };
        assert_eq!(
            resolve_window_buttons(WindowControlsPolicy::Auto, Some(&csd_no_minimize), None),
            WindowButtons {
                minimize: false,
                maximize: true,
                close: true,
            }
        );

        // Floating desktop with full WM capabilities keeps buttons filtered by portal layout.
        let floating_de = CompositorDecorations {
            decoration_mode: Some(DecorationMode::ServerSide),
            wm_capabilities: Some(WmCapabilities {
                window_menu: true,
                maximize: true,
                fullscreen: true,
                minimize: true,
            }),
            initially_tiled: false,
            has_afar_appmenu: false,
        };
        assert_eq!(
            resolve_window_buttons(
                WindowControlsPolicy::Auto,
                Some(&floating_de),
                Some(parse_button_layout(":close")),
            ),
            WindowButtons {
                minimize: false,
                maximize: false,
                close: true,
            }
        );

        // Explicit Show / Hide overrides ignore compositor hints.
        assert_eq!(
            resolve_window_buttons(WindowControlsPolicy::Show, Some(&tiling_ssd), None),
            WindowButtons::ALL
        );
        assert_eq!(
            resolve_window_buttons(WindowControlsPolicy::Hide, Some(&floating_de), None),
            WindowButtons::NONE
        );
    }
}
