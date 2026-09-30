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
pub const EVENT_TYPES: [&str; 6] = [
    "ring",
    "motion",
    "smartDetectZone",
    "smartDetectLine",
    "smartDetectLoiterZone",
    "smartAudioDetect",
];

/// Kinds of an event, normalized for filtering and display.
pub fn event_kinds(event_type: &str, smart_detect_types: Option<&[String]>) -> Vec<String> {
    match event_type {
        "ring" => vec!["ring".into()],
        "motion" => vec!["motion".into()],
        "smartAudioDetect" => vec!["audio".into()],
        _ => smart_detect_types
            .map(<[String]>::to_vec)
            .unwrap_or_default(),
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
        match local_tm(self.0) {
            Some(tm) => write_rfc3339(f, self.0, &tm),
            None => write!(f, "{}", self.0),
        }
    }
}

/// Writes the Unix-ms timestamp `ms` as RFC 3339 from `tm`, its broken-down time in some zone
/// (whose UTC offset `tm_gmtoff` carries).
fn write_rfc3339(f: &mut impl fmt::Write, ms: i64, tm: &libc::tm) -> fmt::Result {
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
        ms.rem_euclid(1000),
        offset.abs() / 60,
        offset.abs() % 60,
    )
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
        std::thread::Builder::new()
            .name("event-log".into())
            .spawn(move || write_lines(&rx, &warn))?;
        Ok(Self { lines })
    }

    /// Queues `line` for `path`.
    pub fn write(&self, path: &Arc<Path>, line: &impl Serialize) {
        let Ok(mut bytes) = serde_json::to_vec(line) else {
            return;
        };
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
    let open = || {
        OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(path)
    };
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

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use super::*;

    /// 2026-09-29T19:02:11.482Z
    const MS: i64 = 1_790_708_531_482;

    /// `ms` formatted as in a zone `offset_s` seconds east of UTC, whatever the local zone is.
    fn at(ms: i64, offset_s: i64) -> String {
        let secs = (ms.div_euclid(1000) + offset_s) as libc::time_t;
        // SAFETY: gmtime_r only writes into the zeroed `tm` we own.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        assert!(!unsafe { libc::gmtime_r(&secs, &mut tm) }.is_null());
        tm.tm_gmtoff = offset_s as _;
        let mut text = String::new();
        write_rfc3339(&mut text, ms, &tm).unwrap();
        text
    }

    #[test]
    fn times_are_rfc3339_with_millis_and_offset() {
        assert_eq!(at(MS, -5 * 3600), "2026-09-29T14:02:11.482-05:00");
        assert_eq!(at(MS, 0), "2026-09-29T19:02:11.482+00:00");
        assert_eq!(at(MS, 5 * 3600 + 1800), "2026-09-30T00:32:11.482+05:30");
        assert_eq!(at(MS, -(9 * 3600 + 1800)), "2026-09-29T09:32:11.482-09:30");
        // 2026-03-01T04:30:00.007Z: back across a month end, millis zero-padded.
        assert_eq!(
            at(1_772_339_400_007, -5 * 3600),
            "2026-02-28T23:30:00.007-05:00"
        );
        // Before the epoch the millis still count forward within the second.
        assert_eq!(at(-1, 0), "1969-12-31T23:59:59.999+00:00");
    }

    #[test]
    fn duration_is_rounded_to_a_tenth_and_never_negative() {
        assert_eq!(duration_s(1_000, 2_049), 1.0);
        assert_eq!(duration_s(1_000, 2_050), 1.1);
        assert_eq!(duration_s(0, 90_000), 90.0);
        assert_eq!(duration_s(5_000, 4_000), 0.0);
    }

    #[test]
    fn kinds_are_normalized_per_event_type() {
        let detected = ["person".to_owned(), "vehicle".to_owned()];
        assert_eq!(event_kinds("ring", Some(&detected)), ["ring"]);
        assert_eq!(event_kinds("motion", Some(&detected)), ["motion"]);
        assert_eq!(
            event_kinds("smartAudioDetect", Some(&["alrmSmoke".to_owned()])),
            ["audio"]
        );
        assert_eq!(event_kinds("smartDetectZone", Some(&detected)), detected);
        assert_eq!(event_kinds("smartDetectLine", None), Vec::<String>::new());
    }

    fn line<'a>(kinds: &'a [String], end: Option<i64>) -> CameraLine<'a> {
        CameraLine {
            phase: if end.is_some() {
                Phase::End
            } else {
                Phase::Start
            },
            id: "66f9a1b2c3d4e5f601234567",
            camera: "Porch",
            camera_id: "cam1",
            event_type: "smartDetectZone",
            kinds,
            matched: None,
            start: Time(MS),
            end: end.map(Time),
            duration_s: end.map(|end| duration_s(MS, end)),
            forced: false,
        }
    }

    #[test]
    fn start_line_has_null_end_and_omits_match_and_forced() {
        let kinds = ["person".to_owned()];
        let json = serde_json::to_string(&line(&kinds, None)).unwrap();
        let expected = format!(
            r#"{{"phase":"start","id":"66f9a1b2c3d4e5f601234567","camera":"Porch","camera_id":"cam1","type":"smartDetectZone","kinds":["person"],"start":"{}","end":null,"duration_s":null}}"#,
            Time(MS)
        );
        assert_eq!(json, expected);
    }

    #[test]
    fn end_line_carries_match_duration_and_forced() {
        let kinds = ["person".to_owned(), "vehicle".to_owned()];
        let end = MS + 12_345;
        let json = serde_json::to_string(&CameraLine {
            matched: Some(false),
            forced: true,
            ..line(&kinds, Some(end))
        });
        let expected = format!(
            r#"{{"phase":"end","id":"66f9a1b2c3d4e5f601234567","camera":"Porch","camera_id":"cam1","type":"smartDetectZone","kinds":["person","vehicle"],"match":false,"start":"{}","end":"{}","duration_s":12.3,"forced":true}}"#,
            Time(MS),
            Time(end)
        );
        assert_eq!(json.unwrap(), expected);
    }

    /// A fresh directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("sauron-test-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Queues `lines` through a `Writer` and runs its thread's loop here until they are written;
    /// returns the warnings.
    fn write_all(lines: &[(&Arc<Path>, &str)]) -> Vec<String> {
        let (tx, rx) = mpsc::channel();
        let writer = Writer { lines: tx };
        for (path, text) in lines {
            writer.write(path, &serde_json::json!({ "line": text }));
        }
        drop(writer);
        let warnings = RefCell::new(Vec::new());
        write_lines(&rx, &|message| warnings.borrow_mut().push(message));
        warnings.into_inner()
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn writer_appends_lines_in_order_to_a_private_file() {
        let tmp = TempDir::new("event-log-append");
        let path: Arc<Path> = Arc::from(tmp.0.join("state/sauron/events.jsonl"));
        assert!(write_all(&[(&path, "a"), (&path, "b")]).is_empty());
        assert!(write_all(&[(&path, "c")]).is_empty());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"line\":\"a\"}\n{\"line\":\"b\"}\n{\"line\":\"c\"}\n"
        );
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }

    #[test]
    fn writer_starts_a_fresh_file_after_rotation() {
        let tmp = TempDir::new("event-log-rotate");
        let path: Arc<Path> = Arc::from(tmp.0.join("events.jsonl"));
        let rotated = tmp.0.join("events.jsonl.1");
        write_all(&[(&path, "old")]);
        std::fs::rename(&path, &rotated).unwrap();
        write_all(&[(&path, "new")]);
        assert_eq!(
            std::fs::read_to_string(&rotated).unwrap(),
            "{\"line\":\"old\"}\n"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"line\":\"new\"}\n"
        );
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn writer_reports_each_error_once_per_log_path() {
        let tmp = TempDir::new("event-log-errors");
        std::fs::write(tmp.0.join("file"), b"").unwrap();
        let broken: Arc<Path> = Arc::from(tmp.0.join("file/events.jsonl"));
        let good: Arc<Path> = Arc::from(tmp.0.join("events.jsonl"));
        let warnings = write_all(&[(&broken, "a"), (&broken, "b"), (&good, "c"), (&broken, "d")]);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings
                .iter()
                .all(|w| w.starts_with(&format!("event log {}: ", broken.display())))
        );
        assert_eq!(
            std::fs::read_to_string(&*good).unwrap(),
            "{\"line\":\"c\"}\n"
        );
    }
}
