//! `sauron log` and `sauron thumbnail`: Protect's own event history and event thumbnails, read
//! through the private API with the instant-live service account (the Integration API has no
//! history).

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Write};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use tokio::time::Instant;

use crate::config::Config;
use crate::event_log::{CameraLine, EVENT_TYPES, Time, duration_s, event_kinds};
use crate::keyring;
use crate::private_api::PrivateApi;
use crate::protect::Protect;
use crate::proto::Phase;

/// Events per request, walking forward by start time so a week of history never sits in memory.
/// A short page ends the walk, so this must not exceed any server cap: Protect 7.2 honours larger
/// limits, but a console has been seen returning exactly 200.
const PAGE: usize = 200;
/// How long a missing thumbnail is retried, and how often (see `thumbnail`).
const THUMBNAIL_WAIT: Duration = Duration::from_secs(10);
const THUMBNAIL_RETRY: Duration = Duration::from_millis(500);

/// `sauron log` arguments, not yet interpreted.
pub struct LogArgs<'a> {
    since: &'a str,
    until: Option<&'a str>,
    all: bool,
}

impl<'a> LogArgs<'a> {
    /// `[--since <when>] [--until <when>] [--all]` (also `--since=<when>`); `None` for anything else.
    pub fn parse(mut args: &[&'a str]) -> Option<Self> {
        let mut parsed = Self {
            since: "24h",
            until: None,
            all: false,
        };
        while let [arg, rest @ ..] = args {
            args = rest;
            let (flag, inline) = match arg.split_once('=') {
                Some((flag, value)) => (flag, Some(value)),
                None => (*arg, None),
            };
            let mut value = || match inline {
                Some(value) => Some(value),
                None => {
                    let (value, rest) = args.split_first()?;
                    args = rest;
                    Some(*value)
                }
            };
            match flag {
                "--all" if inline.is_none() => parsed.all = true,
                "--since" => parsed.since = value()?,
                "--until" => parsed.until = Some(value()?),
                _ => return None,
            }
        }
        Some(parsed)
    }
}

/// One event as the private API lists it; everything else it carries is ignored.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryEvent<'a> {
    #[serde(borrow)]
    id: Cow<'a, str>,
    #[serde(rename = "type", borrow)]
    event_type: Cow<'a, str>,
    start: i64,
    end: Option<i64>,
    #[serde(borrow)]
    camera: Option<Cow<'a, str>>,
    smart_detect_types: Option<Vec<String>>,
    #[serde(borrow)]
    metadata: Option<&'a RawValue>,
}

/// A non-camera event (`--all`): access, admin activity, presence, device events, …
#[derive(Serialize)]
struct OtherLine<'a> {
    phase: Phase,
    id: &'a str,
    #[serde(rename = "type")]
    event_type: &'a str,
    camera: Option<&'a str>,
    camera_id: Option<&'a str>,
    start: Time,
    end: Option<Time>,
    duration_s: Option<f64>,
    /// The event's `metadata` as Protect sent it.
    details: Option<&'a RawValue>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// The instant-live service account's password: the private API needs a UniFi OS login.
async fn service_password(config: &Config, client: &Protect, command: &str) -> Result<String> {
    let password = match config.username.as_str() {
        "" => None,
        username => keyring::lookup(client.console_id(), username).await,
    };
    password.with_context(|| {
        format!("sauron {command} reads Protect's history, which needs the instant-live service account: run sauron setup")
    })
}

/// Prints Protect's events from `--since` to `--until` as JSON Lines, oldest first, one page at a
/// time. A closed stdout (`| head`) ends it quietly.
pub async fn log(args: LogArgs<'_>) -> Result<()> {
    let (since, until) = time_range(args.since, args.until, now_ms())?;
    let (config, client, _) = crate::connect().await?;
    let password = service_password(&config, &client, "log").await?;
    let names: HashMap<String, String> = client
        .cameras()
        .await
        .context("cannot list cameras")?
        .into_iter()
        .map(|camera| {
            let name = camera.display_name().to_owned();
            (camera.id, name)
        })
        .collect();
    let mut api = PrivateApi::new(&client, &config.username, password);

    let mut stdout = std::io::stdout().lock();
    let mut out = Vec::with_capacity(64 * 1024);
    let mut pager = Pager::new(since, until);
    loop {
        let body = api.events(pager.cursor, until, PAGE).await?;
        let page: Vec<HistoryEvent> =
            serde_json::from_slice(&body).context("Protect's event list: unexpected response")?;
        out.clear();
        for event in page.iter().filter(|event| pager.is_new(event)) {
            push_line(&mut out, event, &names, args.all)?;
        }
        match stdout.write_all(&out).and_then(|()| stdout.flush()) {
            Err(err) if err.kind() == ErrorKind::BrokenPipe => return Ok(()),
            result => result.context("cannot write to stdout")?,
        }
        if !pager.advance(&page, PAGE) {
            return Ok(());
        }
    }
}

/// `--since` and `--until` (default: `now`) as Unix ms.
fn time_range(since: &str, until: Option<&str>, now: i64) -> Result<(i64, i64)> {
    let since = parse_when(since, now)?;
    let until = match until {
        Some(when) => parse_when(when, now)?,
        None => now,
    };
    if since > until {
        bail!("--since {} is after --until {}", Time(since), Time(until));
    }
    Ok((since, until))
}

/// Walks the history forward a page at a time. Each page starts at the previous page's last start
/// time, since several events can share a millisecond; `printed_at_cursor` holds the ids already
/// printed from that millisecond.
struct Pager {
    cursor: i64,
    until: i64,
    printed_at_cursor: HashSet<String>,
}

impl Pager {
    fn new(since: i64, until: i64) -> Self {
        Self {
            cursor: since,
            until,
            printed_at_cursor: HashSet::new(),
        }
    }

    /// Whether `event` from the current page is still to be printed.
    fn is_new(&self, event: &HistoryEvent) -> bool {
        (self.cursor..=self.until).contains(&event.start)
            && !self.printed_at_cursor.contains(event.id.as_ref())
    }

    /// Moves past `page`, asked for with `limit`; `false` when it was the last page.
    fn advance(&mut self, page: &[HistoryEvent], limit: usize) -> bool {
        if page.len() < limit {
            return false;
        }
        let last = page
            .iter()
            .map(|event| event.start)
            .max()
            .unwrap_or(self.cursor);
        if last > self.cursor {
            self.printed_at_cursor = page
                .iter()
                .filter(|e| e.start == last)
                .map(|e| e.id.clone().into_owned())
                .collect();
            self.cursor = last;
        } else {
            // A whole page from one millisecond: step past it rather than ask for it again.
            self.printed_at_cursor.clear();
            self.cursor += 1;
        }
        true
    }
}

/// Appends the event's JSON line to `out`: the event-log shape for camera events, the looser
/// shape for the rest (only with `--all`).
fn push_line(
    out: &mut Vec<u8>,
    event: &HistoryEvent,
    names: &HashMap<String, String>,
    all: bool,
) -> Result<()> {
    let phase = if event.end.is_some() {
        Phase::End
    } else {
        Phase::Start
    };
    let end = event.end.map(Time);
    let duration = event.end.map(|end| duration_s(event.start, end));
    let camera_id = event.camera.as_deref();
    let camera = camera_id.map(|id| names.get(id).map_or(id, String::as_str));
    if EVENT_TYPES.contains(&event.event_type.as_ref()) {
        let kinds = event_kinds(&event.event_type, event.smart_detect_types.as_deref());
        let line = CameraLine {
            phase,
            id: &event.id,
            camera: camera.unwrap_or_default(),
            camera_id: camera_id.unwrap_or_default(),
            event_type: &event.event_type,
            kinds: &kinds,
            matched: None,
            start: Time(event.start),
            end,
            duration_s: duration,
            forced: false,
        };
        serde_json::to_writer(&mut *out, &line)?;
    } else if all {
        let line = OtherLine {
            phase,
            id: &event.id,
            event_type: &event.event_type,
            camera,
            camera_id,
            start: Time(event.start),
            end,
            duration_s: duration,
            details: event.metadata,
        };
        serde_json::to_writer(&mut *out, &line)?;
    } else {
        return Ok(());
    }
    out.push(b'\n');
    Ok(())
}

/// Unix ms for `30m`, `24h`, `7d` (before `now`), RFC 3339, or local `YYYY-MM-DD[ HH:MM[:SS]]`.
fn parse_when(text: &str, now: i64) -> Result<i64> {
    let text = text.trim();
    let invalid = || {
        anyhow!(
            "invalid time {text:?}: use 30m, 24h, 7d, YYYY-MM-DD, \"YYYY-MM-DD HH:MM\" or RFC 3339"
        )
    };
    let unit_ms = match text.chars().last() {
        Some('m') => Some(60_000),
        Some('h') => Some(3_600_000),
        Some('d') => Some(86_400_000),
        _ => None,
    };
    if let Some(unit_ms) = unit_ms {
        let amount: u32 = text[..text.len() - 1].parse().map_err(|_| invalid())?;
        return Ok(now - i64::from(amount) * unit_ms);
    }
    parse_datetime(text).ok_or_else(invalid)
}

/// `count` ASCII digits at the start of `text`, and the rest.
fn digits(text: &str, count: usize) -> Option<(i32, &str)> {
    let (head, rest) = text.split_at_checked(count)?;
    if !head.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((head.parse().ok()?, rest))
}

/// `YYYY-MM-DD`, then optionally `[T ]HH:MM[:SS[.fff]]` and a `Z` or `±HH:MM` offset; without an
/// offset the time is local.
fn parse_datetime(text: &str) -> Option<i64> {
    let (year, rest) = digits(text, 4)?;
    let (month, rest) = digits(rest.strip_prefix('-')?, 2)?;
    let (day, mut rest) = digits(rest.strip_prefix('-')?, 2)?;
    let (mut hour, mut minute, mut second, mut millis) = (0, 0, 0, 0);
    if !rest.is_empty() {
        (hour, rest) = digits(rest.strip_prefix(['T', 't', ' '])?, 2)?;
        (minute, rest) = digits(rest.strip_prefix(':')?, 2)?;
        if let Some(seconds) = rest.strip_prefix(':') {
            (second, rest) = digits(seconds, 2)?;
            if let Some(fraction) = rest.strip_prefix(['.', ',']) {
                let count = fraction.bytes().take_while(u8::is_ascii_digit).count();
                if count == 0 {
                    return None;
                }
                let padded = fraction
                    .bytes()
                    .take(count)
                    .chain(std::iter::repeat(b'0'))
                    .take(3);
                millis = padded.fold(0, |ms, digit| ms * 10 + i64::from(digit - b'0'));
                rest = &fraction[count..];
            }
        }
    }
    let offset = match rest {
        "" => None,
        "Z" | "z" => Some(0),
        _ => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let (hours, rest) = digits(&rest[1..], 2)?;
            let (minutes, rest) = digits(rest.strip_prefix(':')?, 2)?;
            if !rest.is_empty() || hours > 23 || minutes > 59 {
                return None;
            }
            Some(sign * i64::from(hours * 3600 + minutes * 60))
        }
    };
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    // SAFETY: a zeroed `tm` is valid; mktime/timegm only read and normalize the one we own.
    let secs = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        tm.tm_year = year - 1900;
        tm.tm_mon = month - 1;
        tm.tm_mday = day;
        tm.tm_hour = hour;
        tm.tm_min = minute;
        tm.tm_sec = second;
        tm.tm_isdst = -1;
        match offset {
            None => libc::mktime(&mut tm),
            Some(_) => libc::timegm(&mut tm),
        }
    };
    if secs == -1 {
        return None;
    }
    Some((secs - offset.unwrap_or(0)) * 1000 + millis)
}

/// Saves the event's thumbnail JPEG (default `./<event-id>.jpg`) and prints the path. During an
/// event this is a current frame; when it ends Protect replaces it with the final thumbnail and
/// answers 404 for about a second meanwhile, so a 404 is retried for a while, as uiprotect's
/// `_get_image_with_retry` does (10 s, every 0.5 s).
pub async fn thumbnail(id: &str, out: Option<&str>) -> Result<()> {
    // Events carry their thumbnail id as `e-<id>`; the endpoint wants the bare event id.
    let id = id.strip_prefix("e-").unwrap_or(id);
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        bail!("invalid event id {id:?}");
    }
    let path = out.map_or_else(|| PathBuf::from(format!("./{id}.jpg")), PathBuf::from);
    let (config, client, _) = crate::connect().await?;
    let password = service_password(&config, &client, "thumbnail").await?;
    let mut api = PrivateApi::new(&client, &config.username, password);
    let deadline = Instant::now() + THUMBNAIL_WAIT;
    let jpeg = loop {
        let jpeg = api
            .event_thumbnail(id)
            .await
            .with_context(|| format!("cannot fetch the thumbnail of event {id}"))?;
        match jpeg {
            Some(jpeg) => break jpeg,
            None if Instant::now() + THUMBNAIL_RETRY < deadline => {
                tokio::time::sleep(THUMBNAIL_RETRY).await
            }
            None => bail!("Protect has no thumbnail for event {id}"),
        }
    };
    tokio::fs::write(&path, &jpeg)
        .await
        .with_context(|| format!("cannot write {}", path.display()))?;
    println!("{}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-09-29T19:02:11.482Z, standing in for "now".
    const NOW: i64 = 1_790_708_531_482;

    #[test]
    fn relative_times_count_back_from_now() {
        assert_eq!(parse_when("30m", NOW).unwrap(), NOW - 30 * 60_000);
        assert_eq!(parse_when(" 24h ", NOW).unwrap(), NOW - 24 * 3_600_000);
        assert_eq!(parse_when("7d", NOW).unwrap(), NOW - 7 * 86_400_000);
        assert_eq!(parse_when("0m", NOW).unwrap(), NOW);
    }

    #[test]
    fn rfc3339_times_honour_their_offset() {
        for text in [
            "2026-09-29T14:02:11.482-05:00",
            "2026-09-29T19:02:11.482Z",
            "2026-09-30T00:32:11.4829+05:30",
            "2026-09-29 19:02:11,482z",
        ] {
            assert_eq!(parse_when(text, 0).unwrap(), NOW, "{text}");
        }
        assert_eq!(
            parse_when("2026-09-29T19:02:11.5Z", 0).unwrap(),
            NOW - 482 + 500
        );
        assert_eq!(parse_when("2026-09-29T19:02Z", 0).unwrap(), NOW - 11_482);
    }

    #[test]
    fn local_times_read_as_the_local_clock() {
        // Whatever the local zone: a local date or time reads back as the same wall-clock time.
        let local = |text: &str| Time(parse_when(text, NOW).unwrap()).to_string();
        assert!(
            local("2026-09-29").starts_with("2026-09-29T00:00:00.000"),
            "{}",
            local("2026-09-29")
        );
        assert!(local("2026-09-29 10:30").starts_with("2026-09-29T10:30:00.000"));
        assert!(local("2026-09-29t10:30:15").starts_with("2026-09-29T10:30:15.000"));
        // And the event log's own timestamps parse back exactly.
        assert_eq!(parse_when(&Time(NOW).to_string(), 0).unwrap(), NOW);
    }

    #[test]
    fn malformed_times_are_errors() {
        for text in [
            "",
            "30",
            "m",
            "-5m",
            "1.5h",
            "7w",
            "yesterday",
            "2026-9-29",
            "2026-13-01",
            "2026-09-32",
            "2026-09-29 24:00",
            "2026-09-29 10:60",
            "2026-09-29T10:00+05",
            "2026-09-29T10:00:00.Z",
            "2026-09-29T10:00:00+24:00",
            "2026-09-29T10:00Q",
        ] {
            let err = parse_when(text, NOW)
                .err()
                .unwrap_or_else(|| panic!("{text:?} parsed"));
            assert!(err.to_string().starts_with("invalid time"), "{err}");
        }
    }

    #[test]
    fn range_defaults_until_to_now_and_rejects_since_after_until() {
        assert_eq!(
            time_range("24h", None, NOW).unwrap(),
            (NOW - 86_400_000, NOW)
        );
        assert_eq!(
            time_range("1h", Some("1h"), NOW).unwrap(),
            (NOW - 3_600_000, NOW - 3_600_000)
        );
        let err = time_range("1h", Some("2h"), NOW).unwrap_err().to_string();
        assert!(
            err.starts_with("--since ") && err.contains(" is after --until "),
            "{err}"
        );
        assert!(time_range("1h", Some("soon"), NOW).is_err());
    }

    #[test]
    fn log_arguments() {
        let args = LogArgs::parse(&["--since=7d", "--until", "2026-09-29", "--all"]).unwrap();
        assert_eq!(
            (args.since, args.until, args.all),
            ("7d", Some("2026-09-29"), true)
        );
        let defaults = LogArgs::parse(&[]).unwrap();
        assert_eq!(
            (defaults.since, defaults.until, defaults.all),
            ("24h", None, false)
        );
        for bad in [&["--all=yes"][..], &["--since"], &["--bogus"], &["7d"]] {
            assert!(LogArgs::parse(bad).is_none(), "{bad:?}");
        }
    }

    fn lines(json: &str, all: bool) -> String {
        let events: Vec<HistoryEvent> = serde_json::from_str(json).unwrap();
        let names = HashMap::from([("cam1".to_owned(), "Porch".to_owned())]);
        let mut out = Vec::new();
        for event in &events {
            push_line(&mut out, event, &names, all).unwrap();
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn camera_events_use_the_event_log_shape() {
        let json = format!(
            r#"[{{"id":"e1","type":"smartDetectZone","start":{NOW},"end":{},"camera":"cam1","smartDetectTypes":["person"],"score":80}},
                {{"id":"e2","type":"ring","start":{NOW},"end":null,"camera":"cam9","smartDetectTypes":[]}}]"#,
            NOW + 4_050
        );
        let expected = format!(
            "{}\n{}\n",
            format_args!(
                r#"{{"phase":"end","id":"e1","camera":"Porch","camera_id":"cam1","type":"smartDetectZone","kinds":["person"],"start":"{}","end":"{}","duration_s":4.1}}"#,
                Time(NOW),
                Time(NOW + 4_050)
            ),
            format_args!(
                r#"{{"phase":"start","id":"e2","camera":"cam9","camera_id":"cam9","type":"ring","kinds":["ring"],"start":"{}","end":null,"duration_s":null}}"#,
                Time(NOW)
            ),
        );
        assert_eq!(lines(&json, false), expected);
        assert_eq!(lines(&json, true), expected);
    }

    #[test]
    fn other_events_only_with_all_and_keep_their_metadata() {
        let json = format!(
            r#"[{{"id":"a1","type":"access","start":{NOW},"end":null,"metadata":{{"ip":"192.0.2.44", "n":[1,2]}}}},
                {{"id":"s1","type":"sensorOpened","start":{NOW},"end":{},"camera":"cam1"}}]"#,
            NOW + 1_000
        );
        assert_eq!(lines(&json, false), "");
        let expected = format!(
            "{}\n{}\n",
            format_args!(
                r#"{{"phase":"start","id":"a1","type":"access","camera":null,"camera_id":null,"start":"{}","end":null,"duration_s":null,"details":{{"ip":"192.0.2.44", "n":[1,2]}}}}"#,
                Time(NOW)
            ),
            format_args!(
                r#"{{"phase":"end","id":"s1","type":"sensorOpened","camera":"Porch","camera_id":"cam1","start":"{}","end":"{}","duration_s":1.0,"details":null}}"#,
                Time(NOW),
                Time(NOW + 1_000)
            ),
        );
        assert_eq!(lines(&json, true), expected);
    }

    /// What the console answers: events starting in `[start, end]`, oldest first, at most `limit`.
    fn serve(events: &[(i64, &str)], start: i64, end: i64, limit: usize) -> String {
        let page: Vec<_> = events
            .iter()
            .filter(|(at, _)| (start..=end).contains(at))
            .take(limit)
            .map(|(at, id)| serde_json::json!({ "id": id, "type": "motion", "start": at, "camera": "cam1" }))
            .collect();
        serde_json::to_string(&page).unwrap()
    }

    /// The ids `sauron log` prints walking `events` a page of `limit` at a time.
    fn walk(events: &[(i64, &str)], since: i64, until: i64, limit: usize) -> Vec<String> {
        let mut pager = Pager::new(since, until);
        let mut printed = Vec::new();
        for _ in 0..100 {
            let body = serve(events, pager.cursor, until, limit);
            let page: Vec<HistoryEvent> = serde_json::from_str(&body).unwrap();
            printed.extend(
                page.iter()
                    .filter(|event| pager.is_new(event))
                    .map(|event| event.id.to_string()),
            );
            if !pager.advance(&page, limit) {
                return printed;
            }
        }
        panic!("the walk never ended");
    }

    #[test]
    fn paging_prints_each_event_once_across_shared_milliseconds() {
        let events = [
            (100, "a"),
            (200, "b"),
            (300, "c"),
            (300, "d"),
            (400, "e"),
            (400, "f"),
            (500, "g"),
            (600, "h"),
        ];
        for limit in [1, 2, 3, 4, 8, 9] {
            let printed = walk(&events, 100, 1_000, limit);
            // With a page of one, a millisecond shared by two events is stepped past after the first.
            let expected: &[&str] = if limit == 1 {
                &["a", "b", "c", "e", "g", "h"]
            } else {
                &["a", "b", "c", "d", "e", "f", "g", "h"]
            };
            assert_eq!(printed, expected, "limit {limit}");
        }
    }

    #[test]
    fn paging_keeps_to_the_range_and_steps_past_a_full_millisecond() {
        let events = [
            (100, "a"),
            (200, "b"),
            (300, "c"),
            (300, "d"),
            (300, "e"),
            (400, "f"),
            (500, "g"),
        ];
        assert_eq!(walk(&events, 200, 400, 3), ["b", "c", "d", "e", "f"]);
        // A page entirely from 300 moves the cursor to 301 instead of asking for 300 forever.
        assert_eq!(walk(&events, 300, 1_000, 3), ["c", "d", "e", "f", "g"]);
    }
}
