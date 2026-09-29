//! Desktop notifications via org.freedesktop.Notifications on the session bus.

use std::collections::{HashMap, VecDeque};
use std::path::Path;

use anyhow::{Context, Result};
use tokio::sync::{Mutex, OnceCell};
use zbus::zvariant::Value;

/// Notification ids remembered for in-place replacement.
const REMEMBERED_IDS: usize = 64;
/// Nerd Font glyph shown by the Omarchy notification daemon.
const GLYPH: &str = "\u{F0208}";

pub struct Notification<'a> {
    pub event_id: &'a str,
    pub camera_id: &'a str,
    pub camera_name: &'a str,
    pub kinds: &'a [String],
    pub start_ms: i64,
    pub image: Option<&'a Path>,
}

pub struct Notifier {
    exe: String,
    connection: OnceCell<zbus::Connection>,
    /// (event id, notification id), newest last; the lock also serializes Notify calls.
    ids: Mutex<VecDeque<(String, u32)>>,
}

impl Notifier {
    pub fn new(exe: String) -> Self {
        Self { exe, connection: OnceCell::new(), ids: Mutex::new(VecDeque::with_capacity(REMEMBERED_IDS)) }
    }

    /// Shows (or updates in place) the notification for an event; returns its id.
    pub async fn notify(&self, n: &Notification<'_>) -> Result<u32> {
        let connection = self
            .connection
            .get_or_try_init(zbus::Connection::session)
            .await
            .context("cannot connect to the session bus")?;
        let mut ids = self.ids.lock().await;
        let replaces = ids.iter().find(|(event, _)| event == n.event_id).map_or(0, |&(_, id)| id);

        let exec_argv = serde_json::to_string(&[self.exe.as_str(), "live", n.camera_id])?;
        let mut hints: HashMap<&str, Value<'_>> = HashMap::with_capacity(4);
        if let Some(image) = n.image.and_then(Path::to_str) {
            hints.insert("image-path", Value::from(image));
        }
        hints.insert("omarchy-glyph", Value::from(GLYPH));
        hints.insert("omarchy-exec-argv", Value::from(exec_argv.as_str()));
        hints.insert("urgency", Value::from(1u8));

        let summary = summary(n.kinds, n.camera_name);
        let body = local_hms(n.start_ms);
        let actions: &[&str] = &[];
        let reply = connection
            .call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "Notify",
                &("Sauron", replaces, "", summary.as_str(), body.as_str(), actions, hints, -1i32),
            )
            .await
            .context("Notify failed")?;
        let id: u32 = reply.body().deserialize().context("unexpected Notify reply")?;

        ids.retain(|(event, _)| event != n.event_id);
        if ids.len() == REMEMBERED_IDS {
            ids.pop_front();
        }
        ids.push_back((n.event_id.to_owned(), id));
        Ok(id)
    }
}

/// "Person, Vehicle · Driveway"
pub fn summary(kinds: &[String], camera: &str) -> String {
    let mut summary = String::with_capacity(64);
    for (i, kind) in kinds.iter().enumerate() {
        if i > 0 {
            summary.push_str(", ");
        }
        push_display_kind(&mut summary, kind);
    }
    summary.push_str(" · ");
    summary.push_str(camera);
    summary
}

fn push_display_kind(out: &mut String, kind: &str) {
    let name = match kind {
        "person" => "Person",
        "vehicle" => "Vehicle",
        "animal" => "Animal",
        "package" => "Package",
        "face" => "Face",
        "licensePlate" => "License plate",
        "audio" => "Sound",
        "ring" => "Doorbell",
        "motion" => "Motion",
        other => {
            let mut chars = other.chars();
            if let Some(first) = chars.next() {
                out.extend(first.to_uppercase());
                out.push_str(chars.as_str());
            }
            return;
        }
    };
    out.push_str(name);
}

/// Local wall-clock `HH:MM:SS` for a Unix-ms timestamp.
fn local_hms(ms: i64) -> String {
    let secs = ms.div_euclid(1000) as libc::time_t;
    // SAFETY: localtime_r only writes into the zeroed `tm` we own; a null return is handled.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&secs, &mut tm).is_null() {
            return String::from("--:--:--");
        }
        tm
    };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}
