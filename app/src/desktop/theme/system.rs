use std::collections::HashMap;
use std::io;
use std::thread;
use std::time::Duration;

use futures_lite::{StreamExt, future};
use winit::event_loop::EventLoopProxy;
use winit::window::Theme;
use zbus::zvariant::OwnedValue;

use crate::desktop::Event;
use crate::desktop::wayland_decorations::{WindowButtons, parse_button_layout};

const APPEARANCE: &str = "org.freedesktop.appearance";
const COLOR_SCHEME: &str = "color-scheme";
const WM_PREFERENCES: &str = "org.gnome.desktop.wm.preferences";
const BUTTON_LAYOUT: &str = "button-layout";

pub(in crate::desktop) struct Watch(event_listener::Event);

impl Watch {
    pub(in crate::desktop) fn start(proxy: EventLoopProxy<Event>) -> io::Result<Self> {
        let stop = event_listener::Event::new();
        let stopped = stop.listen();
        thread::Builder::new()
            .name("lune-theme".into())
            .spawn(move || {
                let result = pollster::block_on(future::race(watch_portal(proxy), async {
                    stopped.await;
                    Ok(())
                }));
                if let Err(error) = result {
                    log::debug!("System theme watcher ended: {error}");
                }
            })?;
        Ok(Self(stop))
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.0.notify(1);
    }
}

async fn watch_portal(proxy: EventLoopProxy<Event>) -> zbus::Result<()> {
    let connection = zbus::connection::Builder::session()?
        .method_timeout(Duration::from_secs(2))
        .build()
        .await?;
    let settings = zbus::Proxy::new(
        &connection,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Settings",
    )
    .await?;
    let mut changes = settings.receive_signal("SettingChanged").await?;

    type Settings = HashMap<String, HashMap<String, OwnedValue>>;
    match settings
        .call::<_, _, Settings>("ReadAll", &([APPEARANCE, WM_PREFERENCES].as_slice(),))
        .await
    {
        Ok(values) => {
            let theme = values
                .get(APPEARANCE)
                .and_then(|keys| keys.get(COLOR_SCHEME));
            if proxy
                .send_event(Event::SystemTheme(color_scheme(theme)))
                .is_err()
            {
                return Ok(());
            }
            if let Some(layout) = values
                .get(WM_PREFERENCES)
                .and_then(|keys| keys.get(BUTTON_LAYOUT))
                .and_then(portal_buttons)
                && proxy.send_event(Event::ButtonLayout(Some(layout))).is_err()
            {
                return Ok(());
            }
        }
        Err(error) => log::debug!("Could not read portal settings: {error}"),
    }

    while let Some(signal) = changes.next().await {
        let Ok((namespace, key, value)) =
            signal.body().deserialize::<(String, String, OwnedValue)>()
        else {
            continue;
        };
        if namespace == APPEARANCE && key == COLOR_SCHEME {
            if proxy
                .send_event(Event::SystemTheme(color_scheme(Some(&value))))
                .is_err()
            {
                break;
            }
        } else if namespace == WM_PREFERENCES
            && key == BUTTON_LAYOUT
            && let Some(layout) = portal_buttons(&value)
            && proxy.send_event(Event::ButtonLayout(Some(layout))).is_err()
        {
            break;
        }
    }
    Ok(())
}

fn color_scheme(value: Option<&OwnedValue>) -> Theme {
    match value.and_then(|value| u32::try_from(value).ok()) {
        Some(2) => Theme::Light,
        _ => Theme::Dark,
    }
}

fn portal_buttons(value: &OwnedValue) -> Option<WindowButtons> {
    let raw = <&str>::try_from(value).ok()?;
    Some(parse_button_layout(raw))
}
