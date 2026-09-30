//! Camera events as JSON Lines: the `event_log` file written by `sauron watch`, and the camera
//! lines of `sauron log`.

use std::collections::HashSet;
use std::fmt;
use std::fs::{DirBuilder, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc;

use serde::{Serialize, Serializer};

use crate::proto::Phase;

/// The camera event types the daemon tracks.
pub const EVENT_TYPES: [&str; 6] =
    ["ring", "motion", "smartDetectZone", "smartDetectLine", "smartDetectLoiterZone", "smartAudioDetect"];

/// Kinds of an event, normalized for filtering and display.
pub fn event_kinds(event_type: &str, smart_detect_types: Option<&[String]>) -> Vec<String> {
    match event_type {
        "ring" => vec!["ring".into()],
        "motion" => vec!["motion".into()],
        "smartAudioDetect" => vec!["audio".into()],
        _ => smart_detect_types.map(<[String]>::to_vec).unwrap_or_default(),
    }
}

/// Local broken-down time for a Unix-ms timestamp.
pub fn local_tm(ms: i64) -> Option<libc::tm> {
    let secs = ms.div_euclid(1000) as libc::time_t;
    // SAFETY: localtime_r only writes into the zeroed `tm` we own; a null return is handled.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        (!libc::localtime_r(&secs, &mut tm).is_null()).then_some(tm)
    }
}

/// A Unix-ms timestamp that serializes as RFC 3339 local time with milliseconds and the UTC
/// offset: `2026-09-29T14:02:11.482-05:00`.
#[derive(Clone, Copy)]
pub struct Time(pub i64);

impl fmt::Display for Time {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(tm) = local_tm(self.0) else { return write!(f, "{}", self.0) };
        let offset = tm.tm_gmtoff / 60;
        let sign = if offset < 0 { '-' } else { '+' };
        write!(
            f,
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}{sign}{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec,
            self.0.rem_euclid(1000),
            offset.abs() / 60,
            offset.abs() % 60,
        )
    }
}

impl Serialize for Time {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Seconds from `start` to `end` (Unix ms), to a tenth of a second.
pub fn duration_s(start: i64, end: i64) -> f64 {
    ((end - start).max(0) as f64 / 100.0).round() / 10.0
}

/// One camera event line; fields serialize in exactly this order.
#[derive(Serialize)]
pub struct CameraLine<'a> {
    pub phase: Phase,
    pub id: &'a str,
    pub camera: &'a str,
    pub camera_id: &'a str,
    #[serde(rename = "type")]
    pub event_type: &'a str,
    pub kinds: &'a [String],
    /// Whether the daemon's filter matched; `sauron log` can't know, so omits it.
    #[serde(rename = "match", skip_serializing_if = "Option::is_none")]
    pub matched: Option<bool>,
    pub start: Time,
    pub end: Option<Time>,
    pub duration_s: Option<f64>,
    /// Ended by the daemon (disconnect or TTL sweep), not by Protect.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub forced: bool,
}

/// Appends lines to the event log from a dedicated thread, in the order they were queued.
pub struct Writer {
    lines: mpsc::Sender<(Arc<Path>, Vec<u8>)>,
}

impl Writer {
    /// Starts the writer thread; `warn` reports each distinct I/O error once per log path.
    pub fn new(warn: impl Fn(String) + Send + 'static) -> std::io::Result<Self> {
        let (lines, rx) = mpsc::channel();
        std::thread::Builder::new().name("event-log".into()).spawn(move || write_lines(&rx, &warn))?;
        Ok(Self { lines })
    }

    /// Queues `line` for `path`.
    pub fn write(&self, path: &Arc<Path>, line: &impl Serialize) {
        let Ok(mut bytes) = serde_json::to_vec(line) else { return };
        bytes.push(b'\n');
        // Only fails once the thread has died, and then there is nobody left to write.
        let _ = self.lines.send((path.clone(), bytes));
    }
}

fn write_lines(rx: &mpsc::Receiver<(Arc<Path>, Vec<u8>)>, warn: &dyn Fn(String)) {
    let mut reported: HashSet<String> = HashSet::new();
    let mut current: Option<Arc<Path>> = None;
    while let Ok((path, line)) = rx.recv() {
        if current.as_ref() != Some(&path) {
            reported.clear();
            current = Some(path.clone());
        }
        if let Err(err) = append(&path, &line) {
            let message = format!("event log {}: {err}", path.display());
            if reported.insert(message.clone()) {
                warn(message);
            }
        }
    }
}

/// Opens, appends one line and closes, so logrotate can move the file at any time: the next line
/// creates a fresh one. A line this short goes out in one `write`, which `O_APPEND` keeps whole.
fn append(path: &Path, line: &[u8]) -> std::io::Result<()> {
    let open = || OpenOptions::new().append(true).create(true).mode(0o600).open(path);
    let mut file = match open() {
        Err(err) if err.kind() == ErrorKind::NotFound => {
            if let Some(dir) = path.parent() {
                DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
            }
            open()?
        }
        file => file?,
    };
    file.write_all(line)
}
