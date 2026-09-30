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
use crate::proto::Phase;
use crate::protect::Protect;

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
        let mut parsed = Self { since: "24h", until: None, all: false };
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
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
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
    let now = now_ms();
    let since = parse_when(args.since, now)?;
    let until = match args.until {
        Some(when) => parse_when(when, now)?,
        None => now,
    };
    if since > until {
        bail!("--since {} is after --until {}", Time(since), Time(until));
    }
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
    // Each page starts at the previous page's last start time, since several events can share a
    // millisecond; these are the ids already printed from that millisecond.
    let mut cursor = since;
    let mut printed_at_cursor: HashSet<String> = HashSet::new();
    loop {
        let body = api.events(cursor, until, PAGE).await?;
        let page: Vec<HistoryEvent> =
            serde_json::from_slice(&body).context("Protect's event list: unexpected response")?;
        out.clear();
        for event in &page {
            if (cursor..=until).contains(&event.start) && !printed_at_cursor.contains(event.id.as_ref()) {
                push_line(&mut out, event, &names, args.all)?;
            }
        }
        match stdout.write_all(&out).and_then(|()| stdout.flush()) {
            Err(err) if err.kind() == ErrorKind::BrokenPipe => return Ok(()),
            result => result.context("cannot write to stdout")?,
        }
        if page.len() < PAGE {
            return Ok(());
        }
        let last = page.iter().map(|event| event.start).max().unwrap_or(cursor);
        if last > cursor {
            printed_at_cursor = page.iter().filter(|e| e.start == last).map(|e| e.id.clone().into_owned()).collect();
            cursor = last;
        } else {
            // A whole page from one millisecond: step past it rather than ask for it again.
            printed_at_cursor.clear();
            cursor += 1;
        }
    }
}

/// Appends the event's JSON line to `out`: the event-log shape for camera events, the looser
/// shape for the rest (only with `--all`).
fn push_line(out: &mut Vec<u8>, event: &HistoryEvent, names: &HashMap<String, String>, all: bool) -> Result<()> {
    let phase = if event.end.is_some() { Phase::End } else { Phase::Start };
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
    let invalid = || anyhow!("invalid time {text:?}: use 30m, 24h, 7d, YYYY-MM-DD, \"YYYY-MM-DD HH:MM\" or RFC 3339");
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
                let padded = fraction.bytes().take(count).chain(std::iter::repeat(b'0')).take(3);
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
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
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
        let jpeg =
            api.event_thumbnail(id).await.with_context(|| format!("cannot fetch the thumbnail of event {id}"))?;
        match jpeg {
            Some(jpeg) => break jpeg,
            None if Instant::now() + THUMBNAIL_RETRY < deadline => tokio::time::sleep(THUMBNAIL_RETRY).await,
            None => bail!("Protect has no thumbnail for event {id}"),
        }
    };
    tokio::fs::write(&path, &jpeg).await.with_context(|| format!("cannot write {}", path.display()))?;
    println!("{}", path.display());
    Ok(())
}
