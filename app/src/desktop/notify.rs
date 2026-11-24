use std::collections::HashMap;
use std::io;
use std::sync::mpsc::{self, SyncSender};
use std::thread;
use std::time::Duration;

use lune_terminal::{DesktopNotification, NotificationUrgency};
use zbus::zvariant::Value;

const CHANNEL_CAPACITY: usize = 8;

struct Message {
    id: Option<String>,
    title: String,
    body: String,
    urgency: NotificationUrgency,
}

pub(super) struct Notifier {
    sender: SyncSender<Message>,
}

impl Notifier {
    pub(super) fn start() -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Message>(CHANNEL_CAPACITY);
        thread::Builder::new()
            .name("lune-notify".into())
            .spawn(move || {
                pollster::block_on(run(receiver));
            })?;
        Ok(Self { sender })
    }

    pub(super) fn send(&self, notification: DesktopNotification<'_>) {
        let title: String = notification
            .title
            .chars()
            .filter(|ch| !ch.is_control())
            .collect();
        let body: String = notification
            .body
            .chars()
            .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
            .collect();
        if title.is_empty() && body.is_empty() {
            return;
        }
        let message = Message {
            id: notification.id.map(str::to_owned),
            title,
            body,
            urgency: notification.urgency,
        };
        let _ = self.sender.try_send(message);
    }
}

async fn run(receiver: mpsc::Receiver<Message>) {
    let mut proxy: Option<zbus::Proxy<'static>> = None;
    let mut ids: HashMap<String, u32> = HashMap::new();
    while let Ok(message) = receiver.recv() {
        if proxy.is_none() {
            match connect().await {
                Ok(connected) => proxy = Some(connected),
                Err(error) => {
                    log::debug!("Desktop notifications unavailable: {error}");
                    continue;
                }
            }
        }
        let Some(active) = &proxy else {
            continue;
        };
        let replaces_id = message
            .id
            .as_deref()
            .and_then(|key| ids.get(key).copied())
            .unwrap_or(0);
        let summary = if message.title.is_empty() {
            "Lune"
        } else {
            message.title.as_str()
        };
        let urgency: u8 = match message.urgency {
            NotificationUrgency::Low => 0,
            NotificationUrgency::Normal => 1,
            NotificationUrgency::Critical => 2,
        };
        let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
        hints.insert("urgency", Value::U8(urgency));
        let actions: &[&str] = &[];
        match active
            .call::<_, _, u32>(
                "Notify",
                &(
                    "Lune",
                    replaces_id,
                    "lune",
                    summary,
                    message.body.as_str(),
                    actions,
                    hints,
                    -1_i32,
                ),
            )
            .await
        {
            Ok(assigned) => {
                if let Some(key) = message.id {
                    if ids.len() >= 64 && !ids.contains_key(&key) {
                        ids.clear();
                    }
                    ids.insert(key, assigned);
                }
            }
            Err(error) => {
                log::debug!("Desktop notification failed: {error}");
                proxy = None;
            }
        }
    }
}

async fn connect() -> zbus::Result<zbus::Proxy<'static>> {
    let connection = zbus::connection::Builder::session()?
        .method_timeout(Duration::from_secs(2))
        .build()
        .await?;
    zbus::Proxy::new(
        &connection,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )
    .await
}
