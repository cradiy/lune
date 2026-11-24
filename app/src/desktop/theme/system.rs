use std::collections::HashMap;
use std::io;
use std::thread;
use std::time::Duration;

use futures_lite::{StreamExt, future};
use winit::event_loop::EventLoopProxy;
use winit::window::Theme;
use zbus::zvariant::OwnedValue;

use crate::desktop::Event;

const APPEARANCE: &str = "org.freedesktop.appearance";
const COLOR_SCHEME: &str = "color-scheme";

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
    let mut changes = settings
        .receive_signal_with_args("SettingChanged", &[(0, APPEARANCE), (1, COLOR_SCHEME)])
        .await?;

    type Settings = HashMap<String, HashMap<String, OwnedValue>>;
    match settings
        .call::<_, _, Settings>("ReadAll", &([APPEARANCE].as_slice(),))
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
        }
        Err(error) => log::debug!("Could not read the system theme: {error}"),
    }

    while let Some(signal) = changes.next().await {
        let Ok((_, _, value)) = signal.body().deserialize::<(String, String, OwnedValue)>() else {
            continue;
        };
        if proxy
            .send_event(Event::SystemTheme(color_scheme(Some(&value))))
            .is_err()
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
